//! The external-agent execution shape: run_external_agent_mode
//! supervises a third-party coding harness (Codex, Claude Code, Kimi Code, Pi) as the
//! session's backend, draining its events into the app event stream.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

/// Surface a visible notice when an outbound message carries image
/// attachments the backend cannot deliver natively (the
/// `send_message_with_images` default forwards text only). The stored-path
/// prelude (`[attachment stored: …]` — see
/// `external_agent::format_attachments_prelude`) still names every stored
/// attachment in the delivered text, so the agent can read the file with
/// its own tools — the pixels just don't arrive inline. An image with no
/// stored file (in-memory only) gets no path line either, so that corner
/// stays an honest "will not reach the agent" warning.
fn warn_undeliverable_images(
    session_log: &SharedSessionLog,
    agent_name: &str,
    supports_images: bool,
    attachments: &[external_agent::AgentAttachment],
) {
    if supports_images {
        return;
    }
    let stored_path = |a: &&external_agent::AgentAttachment| match a {
        external_agent::AgentAttachment::Image(img) => Some(img.local_path.is_some()),
        external_agent::AgentAttachment::File(_) => None,
    };
    let referenced = attachments
        .iter()
        .filter(|a| stored_path(a) == Some(true))
        .count();
    let vanishing = attachments
        .iter()
        .filter(|a| stored_path(a) == Some(false))
        .count();
    if referenced > 0 {
        slog(session_log, |l| {
            l.warn(&format!(
                "{agent_name} backend does not take inline image input; {referenced} image attachment(s) delivered as stored-file path reference(s) only"
            ));
        });
    }
    if vanishing > 0 {
        slog(session_log, |l| {
            l.warn(&format!(
                "{agent_name} backend does not take inline image input; {vanishing} image attachment(s) have no stored file and will not reach the agent"
            ));
        });
    }
}

/// Translate an idle backend cwd announcement without opening an observed
/// turn. Kept as one shared predicate for the persistent-presence and
/// standalone external loops: an unscoped announcement belongs to the primary
/// conversation, while a scoped side/sub-agent announcement must not retarget
/// the primary session's git locus.
pub(crate) fn idle_external_cwd_event(
    event_thread_id: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
    cwd: String,
) -> Option<AppEvent> {
    (!cwd.trim().is_empty()
        && scoped_event_targets_config(event_thread_id, session_id, alias_session_id))
    .then(|| AppEvent::SessionCwdAnnounced {
        session_id: session_id.clone(),
        cwd,
    })
}

/// [`idle_external_cwd_event`]'s twin for a backend's idle VCS notice
/// (commit/push/merge/rebase): same primary-conversation scoping, same
/// no-turn semantics — the hint only freshens the git chip and Changes
/// tab.
pub(crate) fn idle_external_vcs_event(
    event_thread_id: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
    kind: String,
    cwd: Option<String>,
) -> Option<AppEvent> {
    scoped_event_targets_config(event_thread_id, session_id, alias_session_id).then(|| {
        AppEvent::SessionVcsActivity {
            session_id: session_id.clone(),
            kind,
            cwd,
        }
    })
}

/// [`idle_external_cwd_event`]'s twin for an idle-lane PR-publication
/// notice: same scoping, same no-turn semantics; URL validation is the
/// same daemon-side gate the in-turn drain applies.
pub(crate) fn idle_external_pr_published_event(
    event_thread_id: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
    provider: String,
    url: String,
    repo: String,
    identifier: String,
) -> Option<AppEvent> {
    let concrete = session_id.clone()?;
    (!identifier.is_empty()
        && scoped_event_targets_config(event_thread_id, session_id, alias_session_id))
    .then(|| AppEvent::SessionPrPublished {
        session_id: concrete,
        pr: crate::types::SessionPublishedPr {
            provider,
            repo,
            number: identifier,
            url: crate::thread_actions::validated_pr_url(&url),
        },
    })
}

/// Assemble the terminal-goodbye conclude facts at one of the loop's two
/// safe points (idle entry after a round; the idle reload lane). Every
/// input is the loop's own live state except two disk/store reads: the
/// durable bg-park marker (the backend's own activity claims) and the
/// agenda attestation lookup (freshened, so an attest applied through a
/// co-homed daemon counts). No published agenda handle means no
/// attestation and therefore never a conclude — foreground shapes keep
/// today's semantics.
#[allow(clippy::too_many_arguments)]
fn assemble_seat_conclude_facts(
    round_ran_in_this_wrapper: bool,
    parked_follow_ups: &std::collections::VecDeque<FollowUpMessage>,
    follow_up_rx: &FollowUpReceiver,
    context_injection: &event::ContextInjectionQueue,
    live_session_id: &Option<String>,
    alias_session_id: Option<&str>,
    limit_park_armed: bool,
    open_side_threads: usize,
    pending_native_wakeup: bool,
    log_dir: &std::path::Path,
    candidate_session_ids: &[&str],
) -> SeatConcludeFacts {
    let bg_park = crate::session_log::read_session_meta(log_dir).and_then(|meta| meta.bg_park);
    let occurrence_attested = crate::agenda::published_agenda_handle()
        .map(|handle| handle.session_occurrence_attested(candidate_session_ids))
        .unwrap_or(false);
    SeatConcludeFacts {
        round_ran_in_this_wrapper,
        parked_follow_ups: parked_follow_ups.len(),
        channel_follow_ups: follow_up_rx.len(),
        queued_steers: has_queued_steers_for_session(
            context_injection,
            live_session_id.as_deref(),
            alias_session_id,
        ),
        limit_park_armed,
        open_side_threads,
        pending_native_wakeup,
        live_bg_task_park: bg_park_is_live(bg_park.as_ref()),
        occurrence_attested,
    }
}

/// The seat-conclude terminal emissions, shared by both conclude sites:
/// the honest line (session log + dashboard), then the typed
/// `TaskComplete` success terminal — the registry flips the row to
/// `done` on it (dropping it from the drain wait set) and the agenda
/// scheduler journals the occurrence's transport outcome; the caller
/// breaks the loop and `finish_session` removes the row.
fn emit_seat_conclude(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    live_session_id: &Option<String>,
    line: &str,
) {
    slog(session_log, |l| l.info(line));
    bus.send(AppEvent::LogEntry {
        session_id: live_session_id.clone(),
        level: "info".to_string(),
        source: "Intendant".to_string(),
        content: line.to_string(),
        turn: None,
    });
    bus.send(AppEvent::TaskComplete {
        session_id: live_session_id.clone(),
        reason: SEAT_CONCLUDED_TASK_REASON.to_string(),
        summary: None,
        outcome: crate::event::TaskOutcome::Completed,
    });
}

/// In-place backend respawn for reload-credentials (the dashboard's
/// "Reload credentials" chip after a Claude sign-in): cancel a live
/// rate-limit park PRESERVING its pending re-send (unlike an interrupt,
/// which drops it — the reload wants the work to continue on the fresh
/// account), shut the old process down, and re-create the agent
/// resume-attached to the same backend session id so the new process
/// reads the fresh credential store. Queued `parked_follow_ups` stay in
/// the loop untouched and flush through the normal preamble after the
/// respawn. Returns `None` when the respawn failed — the old process is
/// already gone, so the caller exits the loop honestly; on success,
/// returns the descriptions of parked background tasks the restart
/// killed (they were the old process's children —
/// [`mark_parked_tasks_died_with_restart`] flipped their records and
/// published the attention state before the shutdown), so the caller's
/// continuation can carry the re-run offer. The same call re-arms a
/// pending native scheduled wakeup wrapper-side
/// ([`take_over_native_wakeup_at_respawn`]) — the harness timer dies
/// with the old process, and the resumed CLI restores none.
#[allow(clippy::too_many_arguments)]
async fn apply_backend_credentials_reload(
    backend: &external_agent::AgentBackend,
    project: &Project,
    web_port: Option<u16>,
    intendant_session_id: &Option<String>,
    session_agent_config: &session_config::SessionAgentConfig,
    stats: &LoopStats,
    limit_park: &mut Option<LimitParkState>,
    limit_park_streak: &mut u32,
    error_park_streak: &mut u32,
    parked_follow_ups: &mut std::collections::VecDeque<FollowUpMessage>,
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    drain_config: &mut DrainConfig<'_>,
) -> Option<Vec<String>> {
    let session_log = drain_config.session_log;
    // Hoisted out of `drain_config` so the closures below don't hold a
    // borrow across the in-place respawn, which needs `&mut drain_config`.
    let bus = drain_config.bus;
    let reload_session_id = drain_config.session_id.clone();
    let announce = |line: &str| {
        slog(session_log, |l| l.info(line));
        bus.send(AppEvent::LogEntry {
            session_id: reload_session_id.clone(),
            level: "info".to_string(),
            source: "Intendant".to_string(),
            content: line.to_string(),
            turn: None,
        });
    };
    // Typed twin of the announce lines: the supervisor stamps the served
    // per-session reload lifecycle from these, so the Vault card's chips
    // track the daemon's actual progress instead of client-side memory.
    let progress = |progress: crate::event::CredentialReloadProgress| {
        bus.send(AppEvent::BackendCredentialsReloadProgress {
            session_id: reload_session_id.clone(),
            progress,
        });
    };
    if let Some(park) = limit_park.take() {
        *limit_park_streak = 0;
        *error_park_streak = 0;
        slog(session_log, |l| l.set_limit_park(None));
        let noun = park.kind.noun();
        if let Some(pending) = park.pending {
            // Front of the queue: the parked re-send delivers first, then
            // everything queued while parked, oldest first.
            parked_follow_ups.push_front(pending);
        }
        announce(&format!(
            "{noun} cancelled for the credential reload — parked messages deliver after the respawn",
        ));
    }
    // The restart kills the old process's background children before the
    // fresh process exists: flip any parked-on tasks to died-with-restart
    // NOW, with this class's name, so the park never outlives its wake.
    let died_task_descs = mark_parked_tasks_died_with_restart(
        drain_config.bus,
        session_log,
        &drain_config.session_id,
        stats.announced_native_session_id.as_deref(),
        CREDENTIAL_RELOAD_RESTART_CAUSE,
    );
    announce(&format!(
        "Reloading credentials: restarting {} resume-attached to its backend session",
        backend
    ));
    progress(crate::event::CredentialReloadProgress::Respawning);
    match respawn_external_backend_in_place(
        backend,
        project,
        web_port,
        intendant_session_id,
        session_agent_config,
        stats,
        agent,
        event_rx,
        drain_config,
    )
    .await
    {
        Ok(()) => {
            announce(
                "Credential reload complete — the backend restarted on the fresh credential store",
            );
            progress(crate::event::CredentialReloadProgress::Done);
            Some(died_task_descs)
        }
        Err(e) => {
            let line = format!(
                "Credential reload failed: could not respawn {}: {e}",
                backend
            );
            slog(session_log, |l| l.error(&line));
            progress(crate::event::CredentialReloadProgress::Failed {
                error: format!("could not respawn {backend}: {e}"),
            });
            drain_config.bus.send(AppEvent::LoopError(line));
            None
        }
    }
}

/// In-place backend respawn shared by the credential-reload lane and the
/// park-wake lane: shut the old process down (a no-op when it already
/// exited), re-create the agent resume-attached to the same backend
/// session id, and swap the loop's live handles (agent, event stream,
/// thread id). The caller owns the surrounding announcements and any
/// park/queue bookkeeping — and must re-open its event-channel gate,
/// since the swapped-in receiver is live again. `Err` carries the create
/// failure; the old process is already gone then, so callers exit their
/// lane honestly.
#[allow(clippy::too_many_arguments)]
async fn respawn_external_backend_in_place(
    backend: &external_agent::AgentBackend,
    project: &Project,
    web_port: Option<u16>,
    intendant_session_id: &Option<String>,
    session_agent_config: &session_config::SessionAgentConfig,
    stats: &LoopStats,
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    drain_config: &mut DrainConfig<'_>,
) -> Result<(), CallerError> {
    let session_log = drain_config.session_log;
    let resume_id = stats
        .announced_native_session_id
        .clone()
        .or_else(|| drain_config.backend_thread_id.clone());
    if let Err(e) = agent.shutdown().await {
        slog(session_log, |l| {
            l.warn(&format!("Backend shutdown before respawn: {e}"))
        });
    }
    let (new_agent, new_thread, new_event_rx) = create_external_agent(
        backend,
        project,
        session_log,
        web_port,
        resume_id,
        intendant_session_id.clone(),
        session_agent_config.codex_service_tier.clone(),
        session_agent_config.codex_home.clone(),
    )
    .await?;
    *agent = new_agent;
    *event_rx = new_event_rx;
    drain_config.backend_thread_id = Some(new_thread.thread_id.clone());
    Ok(())
}

/// The named cause stamped on background tasks the credential-reload
/// respawn kills (see [`mark_parked_tasks_died_with_restart`]).
pub(crate) const CREDENTIAL_RELOAD_RESTART_CAUSE: &str = "the credential-reload restart";

/// The wake message the supervising loop delivers when a re-armed native
/// scheduled wakeup fires ([`crate::native_wakeup`]): honest about the
/// mechanism — the harness's own `ScheduleWakeup` timer died with the
/// named backend restart and Intendant re-armed it — and carrying the
/// model's own wake prompt, so the loop the model was pacing continues
/// as if the harness had fired. Pure; the clock is injected for tests.
pub(crate) fn native_wakeup_delivery_message(
    record: &crate::native_wakeup::NativeWakeupRecord,
    now_epoch: u64,
) -> FollowUpMessage {
    let cause = record
        .rearmed_cause
        .as_deref()
        .unwrap_or("a backend restart");
    let reason = record
        .reason
        .as_deref()
        .map(|r| format!(" Your stated reason for the delay: {r}."))
        .unwrap_or_default();
    let prompt = if record.prompt.trim().is_empty() {
        "(the arm carried no prompt)".to_string()
    } else {
        record.prompt.clone()
    };
    FollowUpMessage::text(format!(
        "⏰ Scheduled wakeup ({}). Your ScheduleWakeup timer did not survive {cause}, so \
         Intendant re-armed it and is delivering the wake itself — re-arm your next wakeup \
         as usual if you still want the cadence.{reason} Original wakeup prompt:\n{prompt}",
        crate::native_wakeup::due_phrase(record.fire_at_epoch, now_epoch),
    ))
}

/// Deadline instant for the idle select's native-wakeup arm: the due
/// time as a tokio instant (an already-due record reads as "now" and the
/// arm fires immediately). `now` when nothing pends — the arm is
/// guard-disabled then and the value merely type-checks the disabled
/// branch, the limit-park arm's exact pattern.
fn native_wakeup_deadline(
    pending: &Option<crate::native_wakeup::NativeWakeupRecord>,
) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    match pending {
        Some(record) => {
            let now_epoch = crate::session_activity::epoch_seconds();
            now + std::time::Duration::from_secs(record.fire_at_epoch.saturating_sub(now_epoch))
        }
        None => now,
    }
}

/// The follow-up text synthesized when a credential reload's own
/// interrupt cut a live turn. Resume-attach keeps the conversation
/// context, so a nudge — never a re-send of the original prompt, which
/// would double-execute — is the safe shape (the same self-continuation
/// pattern as the managed-context density interrupt).
pub(crate) const RELOAD_MIDTURN_CONTINUATION_TEXT: &str =
    "A credential reload interrupted the previous turn mid-stream — continue where you left off.";

/// Synthesized continuation after a credential-reload respawn:
/// `Some(..)` only when the reload's own interrupt cut a live turn (the
/// turn drain returned `Interrupted` with
/// [`RELOAD_CREDENTIALS_INTERRUPT_REASON`]). The interrupted turn's
/// driving message was already consumed mid-delivery, so nothing else
/// re-drives the work after the respawn — the session would idle on the
/// fresh account with the task half-done. Idle and between-turn reloads
/// pass `false` (idle in, idle out), as does a turn that completed
/// normally despite the request, or one whose interrupt belonged to the
/// user (their stop wins). The message carries no steer/follow-up id, so
/// id-keyed cancel matching can never drop it. Rides the shared
/// [`midturn_continuation`] seam with the rate-limit park's resume nudge
/// ([`limit_park_pending`]) — the started-turn decision lives once.
fn synthesized_reload_continuation(reload_interrupted_turn: bool) -> Option<FollowUpMessage> {
    midturn_continuation(RELOAD_MIDTURN_CONTINUATION_TEXT, reload_interrupted_turn)
}

/// Surface every message still owed to a lane its terminal killed, with
/// the named reason: accepted mid-turn steers retire through the shared
/// cancel seam (otherwise their "awaiting the model's next activity"
/// claim outlives the model — the 2026-07-31 zombie-turn specimen, where
/// an owner follow-up sat against a dead backend indefinitely), and
/// queued follow-ups report FAILED instead of vanishing with the loop.
/// Surfacing only — nothing here re-arms delivery: the park lanes own
/// re-delivery for their own classes, and the safeguards class must
/// never re-arm at all (re-delivery into a flagged context is the
/// re-flag loop). Returns (retired steers, failed follow-ups) for the
/// caller's log row.
pub(crate) fn surface_undelivered_input_at_terminal(
    bus: &EventBus,
    session_id: &Option<String>,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    parked_follow_ups: &mut std::collections::VecDeque<FollowUpMessage>,
    detail: &str,
) -> (usize, usize) {
    let retired_steers = cancel_pending_runtime_steers_for_session(
        bus,
        pending_runtime_steers,
        None,
        None,
        None,
        detail,
    );
    let mut failed_follow_ups = 0usize;
    while let Some(parked) = parked_follow_ups.pop_front() {
        failed_follow_ups += 1;
        emit_follow_up_status(
            bus,
            session_id.as_deref(),
            &parked.follow_up_id,
            Some(&parked.text),
            "failed",
            Some(detail),
        );
    }
    (retired_steers, failed_follow_ups)
}

/// Named reason for input a terminal leaves undelivered (first line,
/// bounded — the full cause is the terminal's own log row).
fn undelivered_detail_for_terminal(reason: &str) -> String {
    let first_line = reason.lines().next().unwrap_or("").trim();
    format!(
        "undelivered — the session ended: {}",
        truncate_string_copy(first_line, 120)
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_external_agent_mode(
    backend: external_agent::AgentBackend,
    task: String,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mut follow_up_rx: FollowUpReceiver,
    json_approval: Option<JsonApprovalSlot>,
    approval_registry: event::ApprovalRegistry,
    context_injection: event::ContextInjectionQueue,
    headless: bool,
    web_port: Option<u16>,
    attachments: UserAttachments,
    resume_session: Option<String>,
    codex_service_tier: Option<String>,
    codex_home: Option<String>,
    control_session_id: Option<String>,
    emit_session_started_after_identity: bool,
    ready_for_thread_actions: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<LoopStats, CallerError> {
    // Effective root stamped on every RoundComplete this mode emits, so
    // the file watcher's round listener can route rounds by root.
    let round_session_root: Option<std::path::PathBuf> = Some(project.root.clone());
    slog(&session_log, |l| {
        l.info(&format!("Mode: external agent ({})", backend));
    });
    if headless {
        println!("External agent: {}", backend);
        if task.trim().is_empty() {
            println!("Attached session; waiting for input");
        } else {
            println!("Task: {}", task);
        }
        println!("---");
    }

    // Construct, initialize, and start a thread for the external agent
    let resumed_external_session = resume_session.clone();
    // Supervisor-spawned sessions (launch.rs) hold this loop's follow-up
    // sender in their ManagedSession entry; the supervisor dropping it is
    // the authoritative "session was stopped/removed" signal. The
    // foreground/MCP shapes pass None and manage the sender lifecycle
    // differently (it may be dropped while a one-shot task legitimately
    // runs), so the queued-turn stop guard below only applies when
    // supervised.
    let supervised_by_session_supervisor = control_session_id.is_some();
    let persist_model_responses_inline = control_session_id.is_some();
    let intendant_session_id = control_session_id.or_else(|| session_log_id(&session_log));
    let effective_codex_home = if backend == external_agent::AgentBackend::Codex {
        codex_home
            .as_deref()
            .and_then(|home| crate::session_config::normalize_codex_home(Some(home)))
            .or_else(crate::session_config::effective_codex_home)
    } else {
        None
    };
    let effective_codex_service_tier = if backend == external_agent::AgentBackend::Codex {
        codex_service_tier.clone().or_else(|| {
            project::normalize_codex_service_tier(
                project.config.agent.codex.service_tier.as_deref(),
            )
        })
    } else {
        None
    };
    if backend == external_agent::AgentBackend::Codex {
        emit_codex_session_capabilities_for_project(
            &bus,
            intendant_session_id.as_deref(),
            &project,
            effective_codex_service_tier.as_deref(),
        );
    } else if backend == external_agent::AgentBackend::ClaudeCode {
        emit_claude_code_session_capabilities(&bus, intendant_session_id.as_deref());
    } else if backend == external_agent::AgentBackend::Kimi {
        emit_kimi_session_capabilities(&bus, intendant_session_id.as_deref());
    } else if backend == external_agent::AgentBackend::Pi {
        emit_pi_session_capabilities(&bus, intendant_session_id.as_deref());
    }
    // Use one control receiver across idle waits and active turn drains.
    // A second parked receiver would retain mid-turn controls and replay them
    // as new idle follow-ups after the turn completes. Subscribed BEFORE the
    // backend spawn below: creating the process (and loading a large resume)
    // can take seconds, and the supervisor routes Stop/Interrupt at this
    // session from the moment it registered the launch — a receiver created
    // only after the spawn silently dropped anything sent in that window
    // (verified live 2026-07-15: a stop during the attach window left the
    // backend running the task it was meant to abort). Events emitted while
    // the backend starts are buffered here and consumed at the first
    // idle/drain poll.
    let mut external_control_rx = bus.subscribe();
    // Stop additionally rides the lossless intent lane. The shared broadcast
    // receiver also carries high-volume model/context traffic and can lag
    // exactly when a foreground session is asked to stop; the originating
    // ControlCommand remains ordered and lossless here.
    let mut external_intent_rx = bus.subscribe_intents();
    let mut external_intent_open = true;
    let (mut agent, thread, mut event_rx) = match create_external_agent(
        &backend,
        &project,
        &session_log,
        web_port,
        resume_session,
        intendant_session_id.clone(),
        effective_codex_service_tier,
        effective_codex_home.clone(),
    )
    .await
    {
        Ok(started) => started,
        Err(e) => {
            if emit_session_started_after_identity {
                if let Some(session_id) = intendant_session_id.clone() {
                    bus.send(AppEvent::SessionStarted {
                        session_id,
                        task: if task.trim().is_empty() {
                            None
                        } else {
                            Some(task.clone())
                        },
                    });
                }
            }
            return Err(e);
        }
    };
    let codex_managed_context_enabled =
        backend == external_agent::AgentBackend::Codex && agent.supports_item_anchor_rewind();
    let backend_session_id = thread.thread_id.clone();
    let mut session_agent_config = session_config::from_project(&backend, &project);
    if backend == external_agent::AgentBackend::Codex {
        session_agent_config.codex_service_tier = agent.service_tier().map(str::to_string);
        session_agent_config.codex_home = effective_codex_home;
    }
    // The spawner (session supervisor) may already have persisted
    // per-session facts to this log dir — fork lineage (`forked_from`),
    // per-session overrides — before launching this loop. Project defaults
    // must never clobber them.
    if let Some(existing) = session_config::read_log_dir_config(&log_dir) {
        session_agent_config.merge_missing_from(existing);
    }
    if let Err(e) = session_config::write_log_dir_config(&log_dir, &session_agent_config) {
        slog(&session_log, |l| {
            l.debug(&format!("Persist session launch config failed: {e}"))
        });
    }
    if backend.thread_id_is_canonical(&backend_session_id) {
        if let Err(e) = session_config::write_external_overlay(
            &platform::home_dir(),
            backend.as_short_str(),
            &backend_session_id,
            &session_agent_config,
        ) {
            slog(&session_log, |l| {
                l.debug(&format!("Persist external launch config failed: {e}"))
            });
        }
    }
    let mut live_session_id = if backend.thread_id_is_canonical(&backend_session_id) {
        Some(backend_session_id.clone())
    } else {
        intendant_session_id.clone()
    };
    // Placeholder thread ids (see thread_id_is_canonical) are withheld from
    // the identity stream: the real backend id is announced later via
    // AgentEvent::NativeSessionId and recording the placeholder would point
    // frontends' status routing at a never-materialized window.
    if backend.thread_id_is_canonical(&backend_session_id) {
        emit_external_session_identity(
            &bus,
            intendant_session_id
                .clone()
                .or_else(|| session_log_id(&session_log)),
            backend.as_short_str(),
            &backend_session_id,
        );
    }
    if backend == external_agent::AgentBackend::Codex {
        let service_tier = agent.service_tier().map(str::to_string);
        emit_codex_session_capabilities_for_project(
            &bus,
            intendant_session_id.as_deref(),
            &project,
            service_tier.as_deref(),
        );
        if live_session_id != intendant_session_id {
            emit_codex_session_capabilities_for_project(
                &bus,
                live_session_id.as_deref(),
                &project,
                service_tier.as_deref(),
            );
        }
    } else if backend == external_agent::AgentBackend::ClaudeCode {
        emit_claude_code_session_capabilities(&bus, intendant_session_id.as_deref());
        if live_session_id != intendant_session_id {
            emit_claude_code_session_capabilities(&bus, live_session_id.as_deref());
        }
    } else if backend == external_agent::AgentBackend::Kimi {
        emit_kimi_session_capabilities(&bus, intendant_session_id.as_deref());
        if live_session_id != intendant_session_id {
            emit_kimi_session_capabilities(&bus, live_session_id.as_deref());
        }
    } else if backend == external_agent::AgentBackend::Pi {
        emit_pi_session_capabilities(&bus, intendant_session_id.as_deref());
        if live_session_id != intendant_session_id {
            emit_pi_session_capabilities(&bus, live_session_id.as_deref());
        }
    }
    if emit_session_started_after_identity {
        if let Some(session_id) = live_session_id.clone() {
            bus.send(AppEvent::SessionStarted {
                session_id,
                task: if task.trim().is_empty() {
                    None
                } else {
                    Some(task.clone())
                },
            });
        }
    }

    // Event loop
    //
    // Resumed threads seed their user-turn state from the backend's own
    // history: the transcript's prompt ordinal is the turn authority (the
    // catalog's hydration lane and the reload annotator both count the
    // WHOLE resumed transcript), so a live lane restarting at turn 1
    // would double-render every prompt under disagreeing badges and
    // reject edits of transcript-numbered turns.
    let mut user_turn_revisions = match (
        &backend,
        resumed_external_session.as_deref(),
        backend_session_id.as_str(),
    ) {
        (external_agent::AgentBackend::Codex, Some(_), session_id) => {
            codex_user_turn_state_from_history(&platform::home_dir(), session_id)
                .unwrap_or_default()
        }
        // A canonical id here is a plain resume (the id stays stable
        // across respawns), so the thread's own transcript seeds it.
        (external_agent::AgentBackend::ClaudeCode, Some(_), session_id)
            if backend.thread_id_is_canonical(session_id) =>
        {
            claude_user_turn_state_from_history(&platform::home_dir(), session_id)
                .unwrap_or_default()
        }
        (external_agent::AgentBackend::Kimi, Some(_), session_id)
            if backend.thread_id_is_canonical(session_id) =>
        {
            crate::web_gateway::session_catalog::kimi_history::kimi_user_turn_state_from_history(
                &platform::home_dir(),
                session_id,
            )
            .unwrap_or_default()
        }
        // A `--fork-session` resume starts on the placeholder id — the
        // forked child announces its own id only mid-turn, after the
        // first prompt is numbered — so spawn is the one race-free seed
        // point, and the fork source's transcript is the copied span the
        // child continues (empty-ledger rationale on the helper).
        (external_agent::AgentBackend::ClaudeCode, Some(fork_source), _) => {
            claude_user_turn_state_from_fork_source(&platform::home_dir(), fork_source)
                .unwrap_or_default()
        }
        _ => UserTurnRevisionState::default(),
    };
    let mut round = user_turn_revisions.active_count() as usize;
    // Seat-conclude guard (terminal-goodbye shape): true once a round ran
    // in THIS wrapper process. `round` itself starts at the resumed
    // transcript's turn count, so it cannot distinguish "the seat said
    // goodbye here" from "a fresh wrapper resume-attached to an old
    // transcript" — and a freshly resumed wrapper must never
    // insta-conclude on a stale attestation before the owner can type.
    let mut round_ran_in_this_wrapper = false;
    let mut stats = LoopStats::default();
    if backend == external_agent::AgentBackend::Codex {
        stats.codex_subagent_parent_threads = codex_subagent_parent_threads_from_log(&log_dir);
        for child_id in stats.codex_subagent_parent_threads.keys().cloned() {
            stats.codex_subagent_rounds.entry(child_id).or_insert(0);
        }
    }
    let mut diff_tracker = ExternalDiffDeltaTracker::default();
    let mut pending_runtime_steers: std::collections::VecDeque<PendingRuntimeSteer> =
        std::collections::VecDeque::new();
    let mut handled_steer_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cancelled_follow_ups: HashSet<String> = HashSet::new();
    let mut open_side_threads: HashMap<String, String> = HashMap::new();
    let mut side_rounds: HashMap<String, usize> = HashMap::new();
    let mut side_turn_revisions: HashMap<String, UserTurnRevisionState> = HashMap::new();
    let mut pending_managed_context_replays: std::collections::VecDeque<FollowUpMessage> =
        std::collections::VecDeque::new();
    // Rate-limit park (park-until-reset): armed when a turn ends
    // limit-rejected. While parked, the idle wait holds new input in
    // `parked_follow_ups` instead of burning it against the rejected
    // backend; the park timer re-sends the pending message at the reset.
    // The streak counts consecutive limit-rejections (backoff input when
    // the wire carries no reset time) and clears on any completed turn.
    let mut limit_park: Option<LimitParkState> = None;
    let mut limit_park_streak: u32 = 0;
    // Whether the backend event channel still has a live sender. A park
    // with owed work now holds through the backend's death (the
    // park-then-die reconciliation), and a dead process's channel closes
    // moments later — an ungated `recv()` on the closed channel would
    // spin the idle select. Closed-while-parked flips this gate off; the
    // respawn lanes (credential reload, park wake) flip it back on when
    // they swap in a fresh receiver.
    let mut event_channel_open = true;
    // The event-bus-closed exit is the daemon tearing down, not this
    // session's story ending: the exit backstop below must then leave the
    // durable limit-park marker in place as the boot auto-readopt trace
    // (exactly like a hard daemon death that never reaches the backstop),
    // instead of cancelling the park like a deliberate session end.
    let mut daemon_teardown_exit = false;
    // Consecutive transient-service-condition round deaths (the error
    // park's recovery-attempt counter): +1 each death, reset by a
    // completed turn or an explicit intervention (interrupt, reload) —
    // past the bounded widening schedule the session suspends visibly
    // instead of parking again.
    let mut error_park_streak: u32 = 0;
    let mut parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
        std::collections::VecDeque::new();
    let mut managed_context_recovery_kickstarts_without_rewind = 0u8;
    let mut managed_context_density_block_handoffs_without_relief = 0u8;
    let mut managed_context_surgical_recoveries = 0u8;
    // Task statement for surgical-recovery primers (the supervisor cannot
    // summarize the pruned span; it restates the task instead).
    let surgical_task_statement = (!task.trim().is_empty()).then(|| task.clone());
    let mut next_turn = if task.trim().is_empty() {
        None
    } else {
        Some(FollowUpMessage::with_attachments(task, attachments))
    };

    // Coordination-bus declaration (Track C §1.5): the supervisor
    // declares on the backend's behalf (`backend:` set) for the whole
    // supervised span; the drain's event ticks heartbeat it, and the
    // guard's Drop removes it on any orderly exit (crash-abandoned
    // copies age out by TTL). Advisory: bus trouble logs and the
    // session runs undeclared. Identity note: the declaration keeps the
    // spawn-time session id even if the primary address later rotates
    // (fork/native-id upgrades) — the `session:` field is a correlation
    // hint, not routing state.
    let coordination_declaration = live_session_id
        .clone()
        .or_else(|| session_log_id(&session_log))
        .and_then(|session_id| {
            let (space_dir, space_key) = coordination::paths::resolve_space_dir(
                coordination::paths::env_override().as_deref(),
                &crate::platform::intendant_home(),
                &project.root,
            );
            match coordination::lifecycle::SessionDeclarationGuard::declare(
                coordination::lifecycle::DeclareParams {
                    space_dir: &space_dir,
                    space_key: &space_key,
                    session_id: &session_id,
                    backend: backend.as_short_str(),
                    project_root: &project.root,
                    branch: crate::worktree::current_branch(&project.root),
                    intent: surgical_task_statement.as_deref().unwrap_or(""),
                },
                coordination::now_ms(),
            ) {
                Ok(guard) => Some(guard),
                Err(e) => {
                    slog(&session_log, |l| {
                        l.debug(&format!("Coordination declaration skipped: {e}"))
                    });
                    None
                }
            }
        });

    // Reload-credentials handshake: the drain raises this when the request
    // arrives mid-turn (after interrupting the backend); the loop applies
    // the in-place respawn at the next safe point. Idle requests apply
    // immediately in the idle listener below.
    let backend_credentials_reload = std::sync::atomic::AtomicBool::new(false);
    // Companion marker: the reload's own interrupt cut a live primary
    // turn (the turn drain returned `Interrupted` with the reload
    // reason), so the safe-point respawn must front-queue a synthesized
    // continuation to re-drive the dropped work. Never set on the idle
    // lane — idle in, idle out.
    let mut reload_interrupted_turn = false;
    let mut drain_config = DrainConfig {
        bus: &bus,
        web_port,
        session_id: live_session_id.clone(),
        alias_session_id: if intendant_session_id != live_session_id {
            intendant_session_id.clone()
        } else {
            None
        },
        backend_thread_id: Some(backend_session_id.clone()),
        autonomy: autonomy.clone(),
        session_log: &session_log,
        project_root: &project.root,
        log_dir: &log_dir,
        approval_registry: &approval_registry,
        json_approval: json_approval.as_ref(),
        agent_source: Some(backend.to_string()),
        suppress_agent_started: false,
        persist_model_responses_inline,
        headless,
        context_injection: &context_injection,
        reload_credentials: Some(&backend_credentials_reload),
        coordination_declaration: coordination_declaration.as_ref(),
    };
    let mut codex_thread_action_dedupe = CodexThreadActionDedupe::default();
    if let Some(ready_tx) = ready_for_thread_actions {
        let _ = ready_tx.send(());
    }

    'outer: loop {
        // A supervised session signals stop/removal by dropping the
        // follow-up sender held in its ManagedSession entry. The idle wait
        // below already exits on that closure, but a queued `next_turn`
        // (the resume/start task, a managed-context replay) bypasses the
        // idle wait entirely — a StopSession that won the startup race
        // against this loop then ran the very turn it was meant to abort.
        // Together with the pre-spawn control subscription above this
        // closes the attach-window stop completely: a stop before the
        // subscription implies the sender is already gone (caught here); a
        // stop after it is buffered on the control receiver (caught by the
        // idle select / turn drain).
        if supervised_by_session_supervisor && next_turn.is_some() && follow_up_rx.is_closed() {
            slog(&session_log, |l| {
                l.info("Session was stopped before its queued turn started; exiting")
            });
            stats.terminal_outcome = Some("stopped before the queued turn started".to_string());
            break 'outer;
        }
        // A reload-credentials request raised mid-turn applies here, at
        // the loop's safe point: the drain has already interrupted the
        // backend and returned; the respawn precedes any queued turn so
        // the next delivery runs on the fresh credential store.
        if backend_credentials_reload.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let Some(reload_died_task_descs) = apply_backend_credentials_reload(
                &backend,
                &project,
                web_port,
                &intendant_session_id,
                &session_agent_config,
                &stats,
                &mut limit_park,
                &mut limit_park_streak,
                &mut error_park_streak,
                &mut parked_follow_ups,
                &mut agent,
                &mut event_rx,
                &mut drain_config,
            )
            .await
            else {
                stats.terminal_outcome =
                    Some("credential reload could not respawn the backend".to_string());
                break 'outer;
            };
            event_channel_open = true;
            // When the reload's own interrupt cut the live turn, the
            // turn's driving message was consumed mid-delivery — nothing
            // re-drives it after the respawn, so the session would idle
            // on the fresh account with the work half-done. Mirror the
            // rate-limit park's pending preservation: front-queue a
            // synthesized continuation so the flush below re-drives the
            // interrupted work ahead of anything queued behind it. When
            // the restart also killed parked background tasks, the
            // continuation carries the re-run OFFER (and only an already
            // owed continuation does — a between-rounds park mints no
            // nudge; its surfaces are the attention state and the session
            // card's one-tap re-run).
            if let Some(mut continuation) =
                synthesized_reload_continuation(std::mem::take(&mut reload_interrupted_turn))
            {
                if let Some(addendum) = died_tasks_nudge_addendum(
                    &reload_died_task_descs,
                    CREDENTIAL_RELOAD_RESTART_CAUSE,
                ) {
                    continuation.text.push_str(&addendum);
                }
                parked_follow_ups.push_front(continuation);
                let line = "Credential reload interrupted the previous turn — queued a continuation so the work resumes on the fresh account";
                slog(&session_log, |l| l.info(line));
                bus.send(AppEvent::LogEntry {
                    session_id: drain_config.session_id.clone(),
                    level: "info".to_string(),
                    source: "Intendant".to_string(),
                    content: line.to_string(),
                    turn: None,
                });
            }
        }
        // Seat-conclude runs once per idle entry (the first pass of the
        // idle wait below), never per bus event: the terminal-goodbye
        // shape is decided when the round that just ended left the loop
        // idle, and later events either end the session themselves or
        // queue work that breaks the shape's emptiness conjuncts.
        let mut seat_conclude_checked = false;
        let followup = match next_turn.take() {
            Some(turn) => turn,
            None => loop {
                if limit_park.is_none() {
                    // Flush messages queued during a rate-limit park,
                    // oldest first, honoring cancels recorded while they
                    // waited. (Steer-id dedup happened when each message
                    // entered the queue — the queue is not a second
                    // delivery path.)
                    let (flushed, skipped) =
                        next_parked_follow_up(&mut parked_follow_ups, &mut cancelled_follow_ups);
                    if skipped > 0 {
                        slog(&session_log, |l| {
                            l.info(&format!("Skipped {skipped} cancelled queued follow-up(s)"))
                        });
                    }
                    if let Some(queued) = flushed {
                        break queued;
                    }
                }
                // While parked, a queued steer must not trigger an empty
                // flush turn into the rejected backend — it merges into
                // the pending re-send at resume instead.
                if limit_park.is_none()
                    && has_queued_steers_for_session(
                        &context_injection,
                        live_session_id.as_deref(),
                        drain_config.alias_session_id.as_deref(),
                    )
                {
                    break FollowUpMessage::text(String::new());
                }
                // Pending native scheduled wakeup ([`crate::native_wakeup`]),
                // peeked per iteration — every drained event re-enters this
                // loop, so a fresh arm/replace/stop from the backend is
                // re-read before the next wait. The deadline arm below
                // retires a harness-owned record whose due time passes with
                // the backend alive (the harness owns that fire) and
                // delivers a wrapper-owned one.
                let pending_native_wakeup = stats
                    .announced_native_session_id
                    .as_deref()
                    .and_then(crate::native_wakeup::pending_for);
                // Terminal-goodbye conclude (respawn-after-close card
                // 01KZ0PRYE7…): at the first idle pass after a round, a
                // seat that honestly finished — occurrence attested,
                // nothing pending anywhere, no live background tasks —
                // ends its own row instead of idling as a drain-holding
                // husk nothing can wake. A pending native scheduled
                // wakeup is owed work (the peek above): concluding over
                // it would kill the very fire the wakeup lane preserves
                // across respawns, so it blocks the shape.
                if !seat_conclude_checked {
                    seat_conclude_checked = true;
                    let mut conclude_ids: Vec<&str> = Vec::new();
                    for id in [
                        intendant_session_id.as_deref(),
                        live_session_id.as_deref(),
                        drain_config.session_id.as_deref(),
                        drain_config.alias_session_id.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if !conclude_ids.contains(&id) {
                            conclude_ids.push(id);
                        }
                    }
                    let facts = assemble_seat_conclude_facts(
                        round_ran_in_this_wrapper,
                        &parked_follow_ups,
                        &follow_up_rx,
                        &context_injection,
                        &live_session_id,
                        drain_config.alias_session_id.as_deref(),
                        limit_park.is_some(),
                        open_side_threads.len(),
                        pending_native_wakeup.is_some(),
                        &log_dir,
                        &conclude_ids,
                    );
                    if facts.concluded() {
                        emit_seat_conclude(
                            &bus,
                            &session_log,
                            &live_session_id,
                            SEAT_CONCLUDED_GOODBYE_LINE,
                        );
                        stats.terminal_outcome = Some("completed".to_string());
                        break 'outer;
                    }
                }
                tokio::select! {
                    maybe_intent = external_intent_rx.recv(), if external_intent_open => {
                        match maybe_intent {
                            Some(event) => {
                                if let Some(reason) = external_stop_reason_from_control_intent(
                                    &event,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    stats.announced_native_session_id.as_deref(),
                                ) {
                                    slog(&session_log, |l| {
                                        l.info(&format!(
                                            "Stop requested on lossless intent lane while idle: {reason}"
                                        ))
                                    });
                                    stats.terminal_outcome = Some(reason);
                                    break 'outer;
                                }
                            }
                            None => external_intent_open = false,
                        }
                        continue;
                    }
                    maybe_followup = follow_up_rx.recv() => {
                        match maybe_followup {
                            Some(followup) => {
                                if follow_up_message_was_cancelled(
                                    &mut cancelled_follow_ups,
                                    &followup,
                                ) {
                                    slog(&session_log, |l| {
                                        l.info("Skipped cancelled queued follow-up")
                                    });
                                    continue;
                                }
                                if let Some(id) = followup.steer_id.as_deref() {
                                    if steer_id_has_been_handled(&handled_steer_ids, id) {
                                        slog(&session_log, |l| {
                                            l.debug(&format!(
                                                "Ignoring duplicate queued steer {} already consumed by another delivery path",
                                                id
                                            ))
                                        });
                                        continue;
                                    }
                                    mark_steer_id_handled(&mut handled_steer_ids, id);
                                }
                                break followup;
                            }
                            None => {
                                slog(&session_log, |l| {
                                    l.info("Follow-up channel closed, exiting")
                                });
                                stats.terminal_outcome =
                                    Some("follow-up channel closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                    // Park timer (both kinds ride this one slot): at the
                    // limit's reset or the recovery schedule's next step,
                    // re-send what the park held. Messages queued
                    // meanwhile flush right after via the parked-flush
                    // preamble above.
                    _ = tokio::time::sleep_until(
                        limit_park
                            .as_ref()
                            .map(|park| park.resume_at)
                            .unwrap_or_else(tokio::time::Instant::now)
                    ), if limit_park.is_some() => {
                        let park = limit_park.take().expect("branch guarded by is_some");
                        slog(&session_log, |l| l.set_limit_park(None));
                        let noun = park.kind.noun();
                        match park.pending {
                            Some(pending)
                                if !follow_up_message_was_cancelled(
                                    &mut cancelled_follow_ups,
                                    &pending,
                                ) =>
                            {
                                // A park can hold through the backend's own
                                // death (the park-then-die reconciliation):
                                // a wake that owes a delivery must respawn
                                // a confirmed-dead backend resume-attached
                                // FIRST, or the re-send would fail into the
                                // dead process's stdin. The probe is
                                // per-backend honest — only a confirmed
                                // exit (or no process at all) answers true,
                                // so a live backend is never replaced;
                                // backends without the probe (everything
                                // but Claude Code today) keep the direct
                                // send, whose failure surfaces loudly
                                // through the delivery lane.
                                if agent.next_round_reads_fresh_credentials() {
                                    match respawn_external_backend_in_place(
                                        &backend,
                                        &project,
                                        web_port,
                                        &intendant_session_id,
                                        &session_agent_config,
                                        &stats,
                                        &mut agent,
                                        &mut event_rx,
                                        &mut drain_config,
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            event_channel_open = true;
                                            let line = format!(
                                                "{noun} elapsed with the backend gone — respawned {} resume-attached to deliver the parked message",
                                                backend
                                            );
                                            slog(&session_log, |l| l.info(&line));
                                            bus.send(AppEvent::LogEntry {
                                                session_id: live_session_id.clone(),
                                                level: "info".to_string(),
                                                source: "Intendant".to_string(),
                                                content: line,
                                                turn: None,
                                            });
                                        }
                                        Err(e) => {
                                            let line = format!(
                                                "{noun} elapsed, but the dead backend could not be respawned: {e}"
                                            );
                                            slog(&session_log, |l| l.error(&line));
                                            emit_follow_up_status(
                                                &bus,
                                                live_session_id.as_deref(),
                                                &pending.follow_up_id,
                                                Some(&pending.text),
                                                "failed",
                                                Some("the backend could not be respawned at the park's wake"),
                                            );
                                            bus.send(AppEvent::LoopError(line.clone()));
                                            stats.terminal_outcome = Some(line);
                                            break 'outer;
                                        }
                                    }
                                }
                                let line = format!(
                                    "{noun} elapsed — re-sending the parked message"
                                );
                                slog(&session_log, |l| l.info(&line));
                                bus.send(AppEvent::LogEntry {
                                    session_id: live_session_id.clone(),
                                    level: "info".to_string(),
                                    source: "Intendant".to_string(),
                                    content: line,
                                    turn: None,
                                });
                                break pending;
                            }
                            Some(_) => {
                                slog(&session_log, |l| {
                                    l.info(&format!(
                                        "{noun} elapsed — the parked message was cancelled; awaiting input",
                                    ))
                                });
                            }
                            None => {
                                slog(&session_log, |l| {
                                    l.info(&format!("{noun} elapsed — awaiting input"))
                                });
                            }
                        }
                        // A pending-less wake runs no turn, so nothing
                        // re-announces identity until the next message —
                        // after an account switch the idle session would
                        // wear the superseded era's limit chips
                        // indefinitely. When no live backend process holds
                        // an older credential read, re-announce now: the
                        // vitals hub re-keys membership into the CURRENT
                        // era (the same announce the next process start
                        // would make) without burning a turn.
                        if agent.next_round_reads_fresh_credentials() {
                            slog(&session_log, |l| {
                                l.debug(
                                    "Reset wake refreshed account-era membership without a turn (no live backend process)",
                                )
                            });
                            emit_external_session_identity(
                                &bus,
                                intendant_session_id
                                    .clone()
                                    .or_else(|| session_log_id(&session_log)),
                                backend.as_short_str(),
                                live_session_id.as_deref().unwrap_or_default(),
                            );
                        }
                    }
                    // Native-wakeup deadline (see the peek above). A
                    // harness-owned record whose due time passes with the
                    // backend process alive is retired — the harness owns
                    // that fire, and a record outliving its moment would
                    // make a later respawn re-deliver a wake the model
                    // already got. A wrapper-owned record delivers: the
                    // wake respawns a confirmed-dead backend first (the
                    // park-wake pattern above) and queues behind a live
                    // limit park instead of burning against a rejected
                    // backend.
                    _ = tokio::time::sleep_until(
                        native_wakeup_deadline(&pending_native_wakeup)
                    ), if pending_native_wakeup.is_some() => {
                        let record = pending_native_wakeup.expect("branch guarded by is_some");
                        let session_key = stats
                            .announced_native_session_id
                            .clone()
                            .unwrap_or_default();
                        let now_epoch = crate::session_activity::epoch_seconds();
                        if record.rearmed_cause.is_none() {
                            if !agent.next_round_reads_fresh_credentials() {
                                // Alive through the deadline: the harness
                                // owns this fire (its wake turn, if any,
                                // arrives as ordinary backend activity).
                                crate::native_wakeup::consume(&session_key);
                                slog(&session_log, |l| {
                                    l.debug(
                                        "Native scheduled wakeup deadline passed with the backend process alive — the harness owns that fire; the wrapper record is retired",
                                    )
                                });
                                slog(&session_log, |l| l.set_native_wakeup(None));
                                continue;
                            }
                            // Confirmed dead with the record still
                            // harness-owned: this death reached no respawn
                            // seam (the narrow idle-death window) — take
                            // the timer over now and deliver below.
                            take_over_native_wakeup_at_respawn(
                                &bus,
                                &session_log,
                                &live_session_id,
                                &session_key,
                                "the backend exit",
                                now_epoch,
                            );
                        }
                        // Re-take from the registry: the takeover above
                        // (or an earlier seam's) rewrote the record with
                        // its wrapper-owned cause — the peeked copy is
                        // stale.
                        let Some(record) = crate::native_wakeup::consume(&session_key) else {
                            continue;
                        };
                        if limit_park.is_some() {
                            let line = format!(
                                "⏰ The re-armed native scheduled wakeup fired during a park ({}) — queued; it delivers at the park's wake",
                                crate::native_wakeup::due_phrase(record.fire_at_epoch, now_epoch),
                            );
                            slog(&session_log, |l| l.info(&line));
                            bus.send(AppEvent::LogEntry {
                                session_id: live_session_id.clone(),
                                level: "info".to_string(),
                                source: "Intendant".to_string(),
                                content: line,
                                turn: None,
                            });
                            parked_follow_ups
                                .push_back(native_wakeup_delivery_message(&record, now_epoch));
                            slog(&session_log, |l| l.set_native_wakeup(None));
                            continue;
                        }
                        if agent.next_round_reads_fresh_credentials() {
                            match respawn_external_backend_in_place(
                                &backend,
                                &project,
                                web_port,
                                &intendant_session_id,
                                &session_agent_config,
                                &stats,
                                &mut agent,
                                &mut event_rx,
                                &mut drain_config,
                            )
                            .await
                            {
                                Ok(()) => {
                                    event_channel_open = true;
                                    let line = format!(
                                        "The re-armed native wakeup elapsed with the backend gone — respawned {} resume-attached to deliver the wake",
                                        backend
                                    );
                                    slog(&session_log, |l| l.info(&line));
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "info".to_string(),
                                        source: "Intendant".to_string(),
                                        content: line,
                                        turn: None,
                                    });
                                }
                                Err(e) => {
                                    let line = format!(
                                        "The re-armed native wakeup elapsed, but the dead backend could not be respawned: {e}"
                                    );
                                    slog(&session_log, |l| l.error(&line));
                                    let mut died = record.to_meta();
                                    died.died_cause =
                                        Some("the failed respawn at the wake".to_string());
                                    died.died_at_epoch = Some(now_epoch);
                                    slog(&session_log, |l| {
                                        l.set_native_wakeup(Some(died))
                                    });
                                    bus.send(AppEvent::LoopError(line.clone()));
                                    stats.terminal_outcome = Some(line);
                                    break 'outer;
                                }
                            }
                        }
                        let line = format!(
                            "⏰ Delivering the re-armed native scheduled wakeup ({})",
                            crate::native_wakeup::due_phrase(record.fire_at_epoch, now_epoch),
                        );
                        slog(&session_log, |l| l.info(&line));
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "info".to_string(),
                            source: "Intendant".to_string(),
                            content: line,
                            turn: None,
                        });
                        slog(&session_log, |l| l.set_native_wakeup(None));
                        break native_wakeup_delivery_message(&record, now_epoch);
                    }
                    maybe_event = event_rx.recv(), if event_channel_open => {
                        match maybe_event {
                            Some(event) => {
                                let (event_thread_id, event_turn_id, event) = event.into_scope();
                                if let Some(child_thread_id) =
                                    scoped_event_codex_subagent_thread_id(&event_thread_id, &stats)
                                {
                                    if should_route_idle_external_child_interaction(
                                        &backend, &event,
                                    ) {
                                        // Subscribe before checking the supervisor sender:
                                        // a Stop before the subscription closes that sender,
                                        // while a Stop after it is buffered here. The
                                        // interaction wait can therefore never strand the
                                        // backend after the supervisor removes this session.
                                        let mut idle_interaction_lifecycle_rx =
                                            bus.subscribe_intents();
                                        if supervised_by_session_supervisor
                                            && follow_up_rx.is_closed()
                                        {
                                            let reason =
                                                "session stopped while awaiting child interaction"
                                                    .to_string();
                                            slog(&session_log, |l| l.info(&reason));
                                            stats.terminal_outcome = Some(reason);
                                            break 'outer;
                                        }
                                        let outcome = handle_idle_external_child_interaction(
                                            agent.as_mut(),
                                            &drain_config,
                                            &mut stats,
                                            child_thread_id,
                                            event,
                                            &mut idle_interaction_lifecycle_rx,
                                        )
                                        .await;
                                        match outcome {
                                            IdleExternalChildInteractionOutcome::Resolved => {
                                                continue;
                                            }
                                            IdleExternalChildInteractionOutcome::StopRequested {
                                                reason,
                                            } => {
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "Stop requested during idle child interaction: {reason}"
                                                    ))
                                                });
                                                stats.terminal_outcome = Some(reason);
                                                break 'outer;
                                            }
                                        }
                                    }
                                    handle_idle_codex_subagent_event(
                                        &drain_config,
                                        &mut stats,
                                        child_thread_id,
                                        event,
                                    );
                                    continue;
                                }
                                match event {
                                    external_agent::AgentEvent::NativeSessionId { session_id } => {
                                        persist_native_backend_session_id(
                                            &drain_config,
                                            &session_id,
                                        );
                                        if backend.thread_id_is_canonical(&session_id) {
                                            rotate_external_identity(
                                                &session_id,
                                                &mut live_session_id,
                                                &mut drain_config,
                                            );
                                        }
                                    }
                                    external_agent::AgentEvent::GoalUpdated { goal } => {
                                        emit_external_session_goal(
                                            &drain_config,
                                            event_thread_id,
                                            Some(goal),
                                        );
                                    }
                                    external_agent::AgentEvent::GoalCleared => {
                                        emit_external_session_goal(
                                            &drain_config,
                                            event_thread_id,
                                            None,
                                        );
                                    }
                                    external_agent::AgentEvent::Terminated { reason, exit_code } => {
                                        if let Some(park) = limit_park
                                            .as_ref()
                                            .filter(|park| park.pending.is_some())
                                        {
                                            // Park-then-die reconciliation:
                                            // the backend announcing its own
                                            // death right after a rejection
                                            // armed the park (the specimen's
                                            // process-exit shape) must not
                                            // end the session that promised
                                            // to resume the owed work — the
                                            // park stands; the wake respawns
                                            // before delivering. The
                                            // channel's subsequent close is
                                            // held by the gated recv arm.
                                            let line = park_holds_through_terminal_line(
                                                park.kind,
                                                "the backend process terminated while idle",
                                                &reason,
                                            );
                                            slog(&session_log, |l| l.warn(&line));
                                            bus.send(AppEvent::LogEntry {
                                                session_id: drain_config.session_id.clone(),
                                                level: "warn".to_string(),
                                                source: "Intendant".to_string(),
                                                content: line,
                                                turn: None,
                                            });
                                        } else {
                                            let message = format!(
                                                "{} terminated while idle: {} (exit code: {:?})",
                                                agent.name(),
                                                reason,
                                                exit_code
                                            );
                                            slog(&session_log, |l| l.warn(&message));
                                            bus.send(AppEvent::LoopError(message));
                                            stats.terminal_outcome = Some(reason);
                                            break 'outer;
                                        }
                                    }
                                    // Ambient diagnostics are not evidence of a
                                    // backend-initiated turn. Recording them inline and
                                    // staying idle matters: entering the observe drain on
                                    // one of these deadlocks the session — with no real
                                    // turn running the drain never sees a terminal event,
                                    // so queued follow-ups are never picked up again
                                    // (codex emits stderr `Log` lines right after a
                                    // resume attach, e.g. failing MCP-server logins).
                                    // Only turn-implying events (messages, reasoning,
                                    // tools, plan/diff updates, turn completion) may fall
                                    // through to the observe drain below.
                                    external_agent::AgentEvent::Log { level, message } => {
                                        slog(&session_log, |l| match level.as_str() {
                                            "warn" => l.warn(&message),
                                            "error" => l.error(&message),
                                            _ => l.info(&message),
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: drain_config.session_id.clone(),
                                            level,
                                            source: drain_config
                                                .agent_source
                                                .clone()
                                                .unwrap_or_else(|| "worker".to_string()),
                                            content: message,
                                            turn: None,
                                        });
                                    }
                                    external_agent::AgentEvent::Usage { usage } => {
                                        bus.send(AppEvent::UsageSnapshot {
                                            session_id: drain_config.session_id.clone(),
                                            main: usage.into_model_snapshot(),
                                            presence: None,
                                        });
                                    }
                                    external_agent::AgentEvent::RateLimitWindows { windows } => {
                                        bus.send(AppEvent::SessionRateLimits {
                                            session_id: drain_config.session_id.clone(),
                                            windows,
                                        });
                                    }
                                    // Ambient bookkeeping like Usage/Log:
                                    // forward to the vitals hub, never into
                                    // the observe drain (an idle activity
                                    // snapshot implies no turn and must not
                                    // open a spontaneous round).
                                    external_agent::AgentEvent::ActivityUpdate { activity } => {
                                        stamp_bg_park_marker_from_activity(
                                            &session_log,
                                            &activity,
                                        );
                                        bus.send(AppEvent::SessionActivity {
                                            session_id: drain_config.session_id.clone(),
                                            activity,
                                        });
                                    }
                                    external_agent::AgentEvent::ConfigFacts { facts } => {
                                        bus.send(AppEvent::SessionConfigFacts {
                                            session_id: drain_config.session_id.clone(),
                                            facts,
                                        });
                                    }
                                    // Ambient like ActivityUpdate: the
                                    // adapter's pending-wakeup statement
                                    // mirrors into the durable marker and
                                    // must never open an observe round.
                                    external_agent::AgentEvent::NativeWakeupMarker {
                                        marker,
                                    } => {
                                        slog(&session_log, |l| {
                                            l.set_native_wakeup(marker)
                                        });
                                    }
                                    external_agent::AgentEvent::CwdAnnounced { cwd } => {
                                        // Working-directory announcements are
                                        // ambient session metadata, not proof
                                        // that a backend turn started. Kimi
                                        // emits one while an empty-task
                                        // resumed/forked wrapper attaches; if
                                        // it falls into the observe drain
                                        // below, that drain waits forever for
                                        // a turn completion that can never
                                        // arrive and the wrapper stops
                                        // accepting follow-ups.
                                        if let Some(event) = idle_external_cwd_event(
                                            &event_thread_id,
                                            &live_session_id,
                                            &drain_config.alias_session_id,
                                            cwd,
                                        ) {
                                            bus.send(event);
                                        }
                                    }
                                    external_agent::AgentEvent::VcsActivity { kind, cwd } => {
                                        // Ambient like CwdAnnounced: an
                                        // idle-lane VCS notice freshens
                                        // the git chip without implying a
                                        // turn.
                                        if let Some(event) = idle_external_vcs_event(
                                            &event_thread_id,
                                            &live_session_id,
                                            &drain_config.alias_session_id,
                                            kind,
                                            cwd,
                                        ) {
                                            bus.send(event);
                                        }
                                    }
                                    external_agent::AgentEvent::CodeChangePublished {
                                        provider,
                                        url,
                                        repo,
                                        identifier,
                                    } => {
                                        if let Some(event) = idle_external_pr_published_event(
                                            &event_thread_id,
                                            &live_session_id,
                                            &drain_config.alias_session_id,
                                            provider,
                                            url,
                                            repo,
                                            identifier,
                                        ) {
                                            bus.send(event);
                                        }
                                    }
                                    external_agent::AgentEvent::BackendError {
                                        message,
                                        code,
                                        details,
                                        will_retry,
                                        ..
                                    } => {
                                        let mut content = if let Some(code) = code.as_deref() {
                                            format!(
                                                "{} backend error while idle ({code}): {message}",
                                                agent.name()
                                            )
                                        } else {
                                            format!(
                                                "{} backend error while idle: {message}",
                                                agent.name()
                                            )
                                        };
                                        if let Some(details) =
                                            details.as_deref().filter(|s| !s.trim().is_empty())
                                        {
                                            content.push('\n');
                                            content.push_str(details.trim());
                                        }
                                        slog(&session_log, |l| {
                                            if will_retry {
                                                l.warn(&content)
                                            } else {
                                                l.error(&content)
                                            }
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: drain_config.session_id.clone(),
                                            level: if will_retry { "warn" } else { "error" }
                                                .to_string(),
                                            source: external_agent_log_source(
                                                drain_config.agent_source.as_deref(),
                                            ),
                                            content,
                                            turn: None,
                                        });
                                    }
                                    other => {
                                        let event_targets_primary = scoped_event_targets_config(
                                            &event_thread_id,
                                            &live_session_id,
                                            &drain_config.alias_session_id,
                                        );
                                        let event_targets_side = event_thread_id
                                            .as_deref()
                                            .is_some_and(|id| open_side_threads.contains_key(id));
                                        if !event_targets_primary && !event_targets_side {
                                            continue;
                                        }

                                        let prefetched_event = external_agent::AgentEvent::scoped(
                                            event_thread_id.clone(),
                                            event_turn_id,
                                            other,
                                        );
                                        let observed_session_id =
                                            event_thread_id.clone().or_else(|| live_session_id.clone());
                                        let mut prefetched_events =
                                            std::collections::VecDeque::new();
                                        prefetched_events.push_back(prefetched_event);
                                        let mut side_session_state = ExternalSideSessionState {
                                            open_side_threads: &mut open_side_threads,
                                            side_rounds: &mut side_rounds,
                                            side_turn_revisions: &mut side_turn_revisions,
                                        };
                                        round += 1;
                                        round_ran_in_this_wrapper = true;
                                        stats.turns = 0;
                                        emit_external_turn_status(
                                            &bus,
                                            &autonomy,
                                            observed_session_id.as_deref(),
                                            round,
                                            "running",
                                            format!(
                                                "{} backend turn {} observed while idle",
                                                agent.name(),
                                                round
                                            ),
                                        )
                                        .await;
                                        let drain_outcome =
                                            drain_external_agent_events_with_prefetched(
                                                &mut agent,
                                                &mut event_rx,
                                                &mut external_control_rx,
                                                &drain_config,
                                                &mut stats,
                                                &mut diff_tracker,
                                                &mut pending_runtime_steers,
                                                &mut handled_steer_ids,
                                                &mut cancelled_follow_ups,
                                                &mut codex_thread_action_dedupe,
                                                &mut prefetched_events,
                                                Some(&mut side_session_state),
                                                Some(&mut user_turn_revisions),
                                                false,
                                                false,
                                                false,
                                            )
                                            .await;
                                        if let Some(native) =
                                            stats.announced_native_session_id.take()
                                        {
                                            if backend.thread_id_is_canonical(&native) {
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External session address upgraded to native id {}",
                                                        short_external_session_id(&native)
                                                    ))
                                                });
                                                rotate_external_identity(
                                                    &native,
                                                    &mut live_session_id,
                                                    &mut drain_config,
                                                );
                                            }
                                        }
                                        match drain_outcome {
                                            DrainOutcome::TurnFailed {
                                                reason,
                                                turns_in_round,
                                            } => {
                                                if let Some(park) = limit_park
                                                    .as_ref()
                                                    .filter(|park| park.pending.is_some())
                                                {
                                                    // Park-then-die
                                                    // reconciliation
                                                    // (2026-08-01 specimen
                                                    // e883a2db): this failed
                                                    // spontaneous round is
                                                    // usually the dying
                                                    // backend's death rattle
                                                    // from the very
                                                    // rejection that armed
                                                    // the park seconds
                                                    // earlier — breaking
                                                    // here destroyed the
                                                    // in-memory wake while
                                                    // the durable meta kept
                                                    // advertising owed work.
                                                    // The armed park with
                                                    // pending outranks the
                                                    // terminal: stay
                                                    // resident, roll the
                                                    // round back like the
                                                    // limit arm, restore the
                                                    // waiting status the
                                                    // round's "running"
                                                    // claim overwrote.
                                                    let line =
                                                        park_holds_through_terminal_line(
                                                            park.kind,
                                                            "the observed round failed before any turn completed",
                                                            &reason,
                                                        );
                                                    slog(&session_log, |l| l.warn(&line));
                                                    bus.send(AppEvent::LogEntry {
                                                        session_id: live_session_id.clone(),
                                                        level: "warn".to_string(),
                                                        source: "Intendant".to_string(),
                                                        content: line,
                                                        turn: None,
                                                    });
                                                    emit_external_turn_status(
                                                        &bus,
                                                        &autonomy,
                                                        live_session_id.as_deref(),
                                                        round,
                                                        park.kind.waiting_turn_status(),
                                                        park.kind
                                                            .waiting_turn_detail(agent.name()),
                                                    )
                                                    .await;
                                                    round = round.saturating_sub(1);
                                                } else {
                                                    // A backend-started round
                                                    // observed from idle died on
                                                    // a fatal error before any
                                                    // turn ran: fail honestly,
                                                    // like the primary loop.
                                                    stats.rounds = round;
                                                    slog(&session_log, |l| {
                                                        l.error(&format!(
                                                            "External agent round failed before any turn completed while observed from idle: {reason}"
                                                        ))
                                                    });
                                                    record_external_round_inline(
                                                        &session_log,
                                                        persist_model_responses_inline,
                                                        round,
                                                        turns_in_round,
                                                    );
                                                    bus.send(AppEvent::RoundComplete {
                                                        session_id: live_session_id.clone(),
                                                        round,
                                                        turns_in_round,
                                                        native_message_count: None,
                                                        project_root: round_session_root.clone(),
                                                    });
                                                    bus.send(AppEvent::TaskComplete {
                                                        session_id: live_session_id.clone(),
                                                        reason: reason.clone(),
                                                        summary: None,
                                                        outcome: crate::event::TaskOutcome::Failed,
                                                    });
                                                    stats.terminal_outcome = Some(reason);
                                                    break 'outer;
                                                }
                                            }
                                            DrainOutcome::TurnCompleted {
                                                message,
                                                turns_in_round,
                                            } => {
                                                stats.rounds = round;
                                                record_external_done_and_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    live_session_id.as_deref(),
                                                    message.as_deref(),
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: live_session_id.clone(),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                    project_root: round_session_root.clone(),
                                                });
                                            }
                                            DrainOutcome::LimitRejected {
                                                resets_at_epoch,
                                                message: _,
                                                turn_had_started,
                                            } => {
                                                // A backend-started round
                                                // ended limit-rejected:
                                                // hand the round number
                                                // back and arm a REAL park.
                                                // This arm used to log
                                                // "parked" while arming
                                                // nothing (2026-07-29,
                                                // sessions 379864df/
                                                // a43b7f32): the reset
                                                // never woke them, the
                                                // credential reload's
                                                // park-cancel found nothing
                                                // to resume, and their
                                                // interrupted work was
                                                // silently lost. No driving
                                                // message exists to re-send
                                                // — the pending is the
                                                // resume nudge when the
                                                // backend had started the
                                                // turn — and both the reset
                                                // timer and the reload's
                                                // cancel-and-preserve now
                                                // resume this lane through
                                                // the same paths as the
                                                // follow-up lane's parks.
                                                round = round.saturating_sub(1);
                                                limit_park_streak =
                                                    limit_park_streak.saturating_add(1);
                                                // Confirmed-exit gated:
                                                // a limit that killed
                                                // the process killed its
                                                // background children;
                                                // the resume nudge then
                                                // carries the re-run
                                                // offer.
                                                let died_addendum =
                                                    mark_died_tasks_at_park_arm(
                                                        &mut agent,
                                                        &bus,
                                                        &session_log,
                                                        &live_session_id,
                                                        stats
                                                            .announced_native_session_id
                                                            .as_deref(),
                                                        RATE_LIMIT_RESTART_CAUSE,
                                                        turn_had_started,
                                                    );
                                                let (mut park, mut park_line) =
                                                    backend_started_limit_park(
                                                        resets_at_epoch,
                                                        tokio::time::Instant::now(),
                                                        crate::session_activity::epoch_seconds(),
                                                        limit_park_streak,
                                                        limit_park_jitter_secs(),
                                                        turn_had_started,
                                                    );
                                                // A rejection landing while
                                                // already parked (the death
                                                // rattle held by the
                                                // reconciliation above)
                                                // re-arms with a fresh wake
                                                // clock but must not
                                                // clobber the owed pending
                                                // — restate the line with
                                                // the truthful pending-ness.
                                                if inherit_owed_pending(
                                                    limit_park.take(),
                                                    &mut park,
                                                ) {
                                                    park_line = limit_park_log_line(
                                                        resets_at_epoch,
                                                        crate::session_activity::epoch_seconds(),
                                                        true,
                                                    );
                                                }
                                                if let (Some(pending), Some(addendum)) =
                                                    (park.pending.as_mut(), died_addendum)
                                                {
                                                    pending.text.push_str(&addendum);
                                                }
                                                let has_pending = park.pending.is_some();
                                                slog(&session_log, |l| l.warn(&park_line));
                                                bus.send(AppEvent::LogEntry {
                                                    session_id: live_session_id.clone(),
                                                    level: "warn".to_string(),
                                                    source: "Intendant".to_string(),
                                                    content: park_line,
                                                    turn: None,
                                                });
                                                emit_external_turn_status(
                                                    &bus,
                                                    &autonomy,
                                                    live_session_id.as_deref(),
                                                    round.saturating_add(1),
                                                    park.kind.waiting_turn_status(),
                                                    park.kind.waiting_turn_detail(agent.name()),
                                                )
                                                .await;
                                                limit_park = Some(park);
                                                // Durable marker: mirrors
                                                // the follow-up arm, so a
                                                // daemon death mid-park
                                                // leaves the boot
                                                // auto-readopt trace.
                                                slog(&session_log, |l| {
                                                    l.set_limit_park(Some(
                                                        crate::session_log::SessionLimitParkMeta {
                                                            resets_at_epoch,
                                                            has_pending,
                                                        },
                                                    ))
                                                });
                                            }
                                            DrainOutcome::SafeguardsFlagged {
                                                reason,
                                                turns_in_round,
                                            } => {
                                                // A backend-started round
                                                // ended on the provider's
                                                // safeguards flag: the
                                                // honest terminal, never
                                                // a park — mechanical
                                                // retry re-flags forever.
                                                stats.rounds = round;
                                                let entry =
                                                    crate::safeguards_recast::RecastRef {
                                                        session_id: live_session_id
                                                            .clone()
                                                            .unwrap_or_default(),
                                                        source: agent.name().to_string(),
                                                        reason: reason.clone(),
                                                        disposition:
                                                            crate::safeguards_recast::RecastDisposition::SessionEnded,
                                                    };
                                                let line =
                                                    crate::safeguards_recast::safeguards_flag_line(
                                                        &reason,
                                                    );
                                                slog(&session_log, |l| l.error(&line));
                                                slog(&session_log, |l| {
                                                    l.set_safeguards_flag(entry.meta(
                                                        crate::session_activity::epoch_seconds(),
                                                    ))
                                                });
                                                let (retired_steers, failed_follow_ups) =
                                                    surface_undelivered_input_at_terminal(
                                                        &bus,
                                                        &live_session_id,
                                                        &mut pending_runtime_steers,
                                                        &mut parked_follow_ups,
                                                        crate::safeguards_recast::SAFEGUARDS_UNDELIVERED_DETAIL,
                                                    );
                                                if retired_steers + failed_follow_ups > 0 {
                                                    slog(&session_log, |l| {
                                                        l.info(&format!(
                                                            "Surfaced {} owed message(s) as undelivered — the flagged session never redelivers them",
                                                            retired_steers + failed_follow_ups
                                                        ))
                                                    });
                                                }
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                    project_root: round_session_root.clone(),
                                                });
                                                bus.send(AppEvent::TaskComplete {
                                                    session_id: live_session_id.clone(),
                                                    reason: reason.clone(),
                                                    summary: None,
                                                    outcome: crate::event::TaskOutcome::Failed,
                                                });
                                                crate::safeguards_recast::report_safeguards_flag(
                                                    &bus,
                                                    crate::agenda::published_agenda_handle()
                                                        .as_deref(),
                                                    &entry,
                                                );
                                                stats.terminal_outcome = Some(reason);
                                                break 'outer;
                                            }
                                            DrainOutcome::TransientRoundDeath {
                                                reason,
                                                turns_in_round,
                                                turn_had_started,
                                            } => {
                                                // A backend-started round
                                                // died on a temporary
                                                // service condition: arm
                                                // the error park (no
                                                // driving message from
                                                // this side — the pending
                                                // is the resume nudge
                                                // exactly when the turn
                                                // had started), or
                                                // suspend visibly once
                                                // the widening schedule
                                                // is exhausted.
                                                error_park_streak =
                                                    error_park_streak.saturating_add(1);
                                                if error_park_attempts_exhausted(error_park_streak)
                                                {
                                                    stats.rounds = round;
                                                    let line = error_park_exhausted_line(
                                                        &reason,
                                                        error_park_streak.saturating_sub(1),
                                                    );
                                                    slog(&session_log, |l| l.error(&line));
                                                    record_external_round_inline(
                                                        &session_log,
                                                        persist_model_responses_inline,
                                                        round,
                                                        turns_in_round,
                                                    );
                                                    bus.send(AppEvent::RoundComplete {
                                                        session_id: live_session_id.clone(),
                                                        round,
                                                        turns_in_round,
                                                        native_message_count: None,
                                                        project_root: round_session_root.clone(),
                                                    });
                                                    bus.send(AppEvent::TaskComplete {
                                                        session_id: live_session_id.clone(),
                                                        reason: line.clone(),
                                                        summary: None,
                                                        outcome: crate::event::TaskOutcome::Failed,
                                                    });
                                                    stats.terminal_outcome = Some(line);
                                                    break 'outer;
                                                }
                                                round = round.saturating_sub(1);
                                                // A spontaneous round's
                                                // death that took the
                                                // process took its
                                                // background children
                                                // too (confirmed-exit
                                                // gated); the resume
                                                // nudge carries the
                                                // re-run offer.
                                                let died_addendum =
                                                    mark_died_tasks_at_park_arm(
                                                        &mut agent,
                                                        &bus,
                                                        &session_log,
                                                        &live_session_id,
                                                        stats
                                                            .announced_native_session_id
                                                            .as_deref(),
                                                        SERVICE_RECOVERY_RESTART_CAUSE,
                                                        turn_had_started,
                                                    );
                                                let error_park_jitter =
                                                    error_park_jitter_secs();
                                                let (mut park, mut park_line) =
                                                    transient_round_death_error_park(
                                                        &reason,
                                                        tokio::time::Instant::now(),
                                                        error_park_streak,
                                                        error_park_jitter,
                                                        turn_had_started,
                                                        None,
                                                    );
                                                // A round death landing
                                                // while already parked
                                                // re-arms on the recovery
                                                // schedule but must not
                                                // clobber the owed pending
                                                // — restate the line with
                                                // the truthful pending-ness.
                                                if inherit_owed_pending(
                                                    limit_park.take(),
                                                    &mut park,
                                                ) {
                                                    park_line = error_park_log_line(
                                                        &reason,
                                                        error_park_streak,
                                                        error_park_delay(
                                                            error_park_streak,
                                                            error_park_jitter,
                                                        ),
                                                        true,
                                                    );
                                                }
                                                if let (Some(pending), Some(addendum)) =
                                                    (park.pending.as_mut(), died_addendum)
                                                {
                                                    pending.text.push_str(&addendum);
                                                }
                                                let has_pending = park.pending.is_some();
                                                slog(&session_log, |l| l.warn(&park_line));
                                                bus.send(AppEvent::LogEntry {
                                                    session_id: live_session_id.clone(),
                                                    level: "warn".to_string(),
                                                    source: "Intendant".to_string(),
                                                    content: park_line,
                                                    turn: None,
                                                });
                                                emit_external_turn_status(
                                                    &bus,
                                                    &autonomy,
                                                    live_session_id.as_deref(),
                                                    round.saturating_add(1),
                                                    park.kind.waiting_turn_status(),
                                                    park.kind.waiting_turn_detail(agent.name()),
                                                )
                                                .await;
                                                limit_park = Some(park);
                                                // Durable marker: like the
                                                // limit arms, so a daemon
                                                // death mid-park leaves
                                                // the boot auto-readopt
                                                // trace (no reset clock —
                                                // the schedule is ours).
                                                slog(&session_log, |l| {
                                                    l.set_limit_park(Some(
                                                        crate::session_log::SessionLimitParkMeta {
                                                            resets_at_epoch: None,
                                                            has_pending,
                                                        },
                                                    ))
                                                });
                                            }
                                            DrainOutcome::ContextRewindRequested {
                                                request,
                                                message,
                                                turns_in_round,
                                                ..
                                            } => {
                                                stats.rounds = round;
                                                record_external_done_and_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    live_session_id.as_deref(),
                                                    message.as_deref(),
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: live_session_id.clone(),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                    project_root: round_session_root.clone(),
                                                });
                                                emit_context_rewind_failure(
                                                    &request,
                                                    "context rewind was requested during a backend-started turn observed from idle; the turn was recorded, but the rewind was not applied automatically".to_string(),
                                                    &drain_config,
                                                );
                                            }
                                            DrainOutcome::RecoveryRequired {
                                                message,
                                                recovery_hint,
                                                turns_in_round,
                                            } => {
                                                stats.rounds = round;
                                                let message = recovery_required_message(
                                                    &message,
                                                    recovery_hint.as_deref(),
                                                );
                                                slog(&session_log, |l| l.warn(&message));
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                    project_root: round_session_root.clone(),
                                                });
                                                bus.send(AppEvent::LoopError(message));
                                                stats.terminal_outcome =
                                                    Some("recovery required".to_string());
                                                break 'outer;
                                            }
                                            DrainOutcome::Interrupted { reason } => {
                                                stats.rounds = round;
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External agent interrupted while observed from idle: {}",
                                                        reason
                                                    ))
                                                });
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    stats.turns,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round: stats.turns,
                                                    native_message_count: None,
                                                    project_root: round_session_root.clone(),
                                                });
                                            }
                                            DrainOutcome::Terminated { reason, exit_code } => {
                                                if let Some(park) = limit_park
                                                    .as_ref()
                                                    .filter(|park| park.pending.is_some())
                                                {
                                                    // Park-then-die: the
                                                    // process dying while a
                                                    // park owes work is the
                                                    // expected shape of a
                                                    // limit that killed its
                                                    // backend — the park
                                                    // outranks the terminal
                                                    // and the wake respawns
                                                    // before delivering.
                                                    let line =
                                                        park_holds_through_terminal_line(
                                                            park.kind,
                                                            "the backend process terminated",
                                                            &reason,
                                                        );
                                                    slog(&session_log, |l| l.warn(&line));
                                                    bus.send(AppEvent::LogEntry {
                                                        session_id: live_session_id.clone(),
                                                        level: "warn".to_string(),
                                                        source: "Intendant".to_string(),
                                                        content: line,
                                                        turn: None,
                                                    });
                                                    emit_external_turn_status(
                                                        &bus,
                                                        &autonomy,
                                                        live_session_id.as_deref(),
                                                        round,
                                                        park.kind.waiting_turn_status(),
                                                        park.kind
                                                            .waiting_turn_detail(agent.name()),
                                                    )
                                                    .await;
                                                    round = round.saturating_sub(1);
                                                } else {
                                                    stats.rounds = round;
                                                    slog(&session_log, |l| {
                                                        l.info(&format!(
                                                            "External agent terminated while observed from idle: {} (exit code: {:?})",
                                                            reason,
                                                            exit_code
                                                        ))
                                                    });
                                                    bus.send(AppEvent::TaskComplete {
                                                        session_id: live_session_id.clone(),
                                                        reason: reason.clone(),
                                                        summary: stats.last_response.clone(),
                                                        outcome: crate::event::TaskOutcome::Failed,
                                                    });
                                                    stats.terminal_outcome = Some(reason);
                                                    break 'outer;
                                                }
                                            }
                                            DrainOutcome::ChannelClosed => {
                                                if let Some(park) = limit_park
                                                    .as_ref()
                                                    .filter(|park| park.pending.is_some())
                                                {
                                                    // Park-then-die: gate
                                                    // the closed channel and
                                                    // stay resident (see the
                                                    // idle recv arm's twin).
                                                    event_channel_open = false;
                                                    let line =
                                                        park_holds_through_terminal_line(
                                                            park.kind,
                                                            "the backend event channel closed",
                                                            "",
                                                        );
                                                    slog(&session_log, |l| l.warn(&line));
                                                    bus.send(AppEvent::LogEntry {
                                                        session_id: live_session_id.clone(),
                                                        level: "warn".to_string(),
                                                        source: "Intendant".to_string(),
                                                        content: line,
                                                        turn: None,
                                                    });
                                                    emit_external_turn_status(
                                                        &bus,
                                                        &autonomy,
                                                        live_session_id.as_deref(),
                                                        round,
                                                        park.kind.waiting_turn_status(),
                                                        park.kind
                                                            .waiting_turn_detail(agent.name()),
                                                    )
                                                    .await;
                                                    round = round.saturating_sub(1);
                                                } else {
                                                    slog(&session_log, |l| {
                                                        l.info(
                                                            "External agent event channel closed while observed from idle",
                                                        )
                                                    });
                                                    stats.terminal_outcome = Some(
                                                        "external agent event channel closed".to_string(),
                                                    );
                                                    break 'outer;
                                                }
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                            None => {
                                if let Some(park) =
                                    limit_park.as_ref().filter(|park| park.pending.is_some())
                                {
                                    // Park-then-die reconciliation: the
                                    // dead backend's channel closing is
                                    // the death rattle's quiet sibling —
                                    // the armed park with owed work
                                    // outranks it. Gate the closed
                                    // channel (an ungated recv() would
                                    // spin this select) and stay
                                    // resident; the wake respawns the
                                    // backend before delivering.
                                    event_channel_open = false;
                                    let line = park_holds_through_terminal_line(
                                        park.kind,
                                        "the backend event channel closed",
                                        "",
                                    );
                                    slog(&session_log, |l| l.warn(&line));
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content: line,
                                        turn: None,
                                    });
                                } else {
                                    slog(&session_log, |l| {
                                        l.info("External agent event channel closed, exiting")
                                    });
                                    stats.terminal_outcome =
                                        Some("external agent event channel closed".to_string());
                                    break 'outer;
                                }
                            }
                        }
                    }
                    bus_event = external_control_rx.recv() => {
                        // No native-id normalization here: every drain exit
                        // `take()`s `stats.announced_native_session_id` (and
                        // rotates a canonical id into `live_session_id`), so
                        // by the time this idle select runs the announced id
                        // is always `None` — post-upgrade targets already
                        // match via the rotated `live_session_id`/alias.
                        match bus_event {
                            Ok(AppEvent::SessionStopRequested { session_id, reason })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                slog(&session_log, |l| {
                                    l.info(&format!("Stop requested while idle: {}", reason))
                                });
                                stats.terminal_outcome = Some(reason);
                                break 'outer;
                            }
                            Ok(AppEvent::SteerCancelRequested {
                                session_id,
                                id,
                                reason,
                            }) => {
                                let Some((target_session_id, _target_kind)) =
                                    resolve_external_steer_target_session(
                                        &session_id,
                                        &live_session_id,
                                        &drain_config.alias_session_id,
                                        Some(&open_side_threads),
                                    )
                                else {
                                    continue;
                                };
                                let cancelled_queue = cancel_queued_steers_for_session(
                                    &context_injection,
                                    &bus,
                                    target_session_id.as_deref(),
                                    if target_session_id == live_session_id {
                                        drain_config.alias_session_id.as_deref()
                                    } else {
                                        None
                                    },
                                    id.as_deref(),
                                    &reason,
                                );
                                let cancelled_pending = cancel_pending_runtime_steers_for_session(
                                    &bus,
                                    &mut pending_runtime_steers,
                                    target_session_id.as_deref(),
                                    if target_session_id == live_session_id {
                                        drain_config.alias_session_id.as_deref()
                                    } else {
                                        None
                                    },
                                    id.as_deref(),
                                    &reason,
                                );
                                if cancelled_queue + cancelled_pending == 0 {
                                    // Nothing left to cancel: the steer
                                    // already delivered or converted to a
                                    // follow-up — never fabricate
                                    // `SteerCancelled` (the turn drain's
                                    // handler documents why).
                                    emit_steer_cancel_failed_for_unmatched(
                                        &bus,
                                        target_session_id.or_else(|| live_session_id.clone()),
                                        id,
                                        STEER_CANCEL_UNMATCHED_EXTERNAL_REASON,
                                    );
                                }
                                continue;
                            }
                            Ok(AppEvent::FollowUpCancelRequested {
                                session_id,
                                id,
                                reason,
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                let status_session =
                                    session_id.as_deref().or(live_session_id.as_deref());
                                record_cancelled_follow_up_id(
                                    &mut cancelled_follow_ups,
                                    &bus,
                                    status_session,
                                    id,
                                    &reason,
                                );
                                continue;
                            }
                            Ok(AppEvent::CoordinationRadar {
                                session_id,
                                state: crate::types::CoordinationRadarState::Raised,
                                ..
                            }) if event_targets_session_or_alias(
                                &Some(session_id.clone()),
                                &live_session_id,
                                &drain_config.alias_session_id,
                            ) => {
                                // Coordination radar, external ALERT lane
                                // (§2.8): an alert raised while this
                                // session parks queues the schema line as
                                // the targeted-ContextInjection fallback —
                                // merged into the NEXT turn's prompt, never
                                // an immediate turn of its own. The ledger
                                // inside dedups sets and holds the 10-min
                                // cooldown.
                                queue_external_coordination_alert_steers(
                                    &context_injection,
                                    live_session_id.as_deref(),
                                    &session_log,
                                );
                                continue;
                            }
                            Ok(AppEvent::SteerRequested {
                                session_id,
                                text,
                                id,
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                if steer_id_has_been_handled(&handled_steer_ids, &id) {
                                    slog(&session_log, |l| {
                                        l.debug(&format!(
                                            "Ignoring duplicate steer {} already consumed by another delivery path",
                                            id
                                        ))
                                    });
                                    continue;
                                }
                                mark_steer_id_handled(&mut handled_steer_ids, &id);
                                if maybe_handle_codex_fast_slash_steer(
                                    &mut agent,
                                    &text,
                                    session_id.clone(),
                                    id.clone(),
                                    &drain_config,
                                )
                                .await
                                {
                                    continue;
                                }
                                break FollowUpMessage::steer(
                                    text,
                                    UserAttachments::default(),
                                    id,
                                )
                                .for_target(session_id);
                            }
                            Ok(AppEvent::ExternalFollowUpRequested {
                                session_id,
                                text,
                                attachments,
                                follow_up_id,
                            }) if event_targets_external_session_or_side(
                                &Some(session_id.clone()),
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                let followup = FollowUpMessage::with_attachments(text, attachments)
                                    .for_target(Some(session_id))
                                    .with_follow_up_id(follow_up_id);
                                if follow_up_message_was_cancelled(
                                    &mut cancelled_follow_ups,
                                    &followup,
                                ) {
                                    slog(&session_log, |l| {
                                        l.info("Skipped cancelled queued follow-up")
                                    });
                                    continue;
                                }
                                break followup;
                            }
                            Ok(AppEvent::CodexThreadActionRequested {
                                request_id,
                                session_id,
                                action,
                                params,
                                ..
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                if !codex_thread_action_dedupe.mark_seen(&request_id) {
                                    continue;
                                }
                                if let Some(request) =
                                    external_context_rewind_request_from_action(
                                        &action,
                                        &params,
                                        session_id.clone(),
                                    )
                                {
                                    let request = match request {
                                        Ok(request) => request,
                                        Err(message) => {
                                            bus.send(AppEvent::CodexThreadActionResult {
                                                session_id: session_id.clone().or_else(|| live_session_id.clone()),
                                                action,
                                                success: false,
                                                message,
                                                record_id: None,
                                            });
                                            continue;
                                        }
                                    };
                                    if session_id
                                        .as_deref()
                                        .is_some_and(|id| open_side_threads.contains_key(id))
                                    {
                                        emit_context_rewind_failure(
                                            &request,
                                            "context rewind is not supported for side conversations".to_string(),
                                            &drain_config,
                                        );
                                        continue;
                                    }
                                    match apply_external_context_rewind(
                                        &mut agent,
                                        &thread.thread_id,
                                        &request,
                                        &drain_config,
                                    )
                                    .await
                                    {
                                        Ok(Some(followup)) => {
                                            break followup;
                                        }
                                        Ok(None) => {
                                            continue;
                                        }
                                        Err(message) => {
                                            emit_context_rewind_failure(
                                                &request,
                                                message,
                                                &drain_config,
                                            );
                                            continue;
                                        }
                                    }
                                }
                                if let Some(side_thread_id) = session_id
                                    .as_deref()
                                    .filter(|id| open_side_threads.contains_key(*id))
                                    .map(str::to_string)
                                {
                                    if action == "undo" {
                                        handle_side_undo_thread_action(
                                            &mut agent,
                                            &mut side_rounds,
                                            &mut side_turn_revisions,
                                            &side_thread_id,
                                            params,
                                            &drain_config,
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                                if action == "undo" {
                                    handle_parent_undo_thread_action(
                                        &mut agent,
                                        &mut round,
                                        &mut user_turn_revisions,
                                        params,
                                        &drain_config,
                                    )
                                    .await;
                                    continue;
                                }
                                // An out-of-band /compact while a rate-limit
                                // park is armed would fire into the very
                                // limit the park waits out — defer it with
                                // the calm reset line instead (twin arm in
                                // run_modes' ThreadAction chain).
                                if let Some(deferral) = compact_deferred_by_limit_park(
                                    &action,
                                    &limit_park,
                                    tokio::time::Instant::now(),
                                    crate::session_activity::epoch_seconds(),
                                ) {
                                    slog(&session_log, |l| l.info(&deferral));
                                    bus.send(AppEvent::CodexThreadActionResult {
                                        session_id: session_id
                                            .clone()
                                            .or_else(|| live_session_id.clone()),
                                        action,
                                        success: false,
                                        message: deferral,
                                        record_id: None,
                                    });
                                    continue;
                                }
                                let effect = handle_external_thread_action(
                                    &mut agent,
                                    action,
                                    params,
                                    session_id,
                                    &drain_config,
                                )
                                .await;
                                if let ExternalThreadActionEffect::SideTurnStarted {
                                    parent_thread_id,
                                    child_thread_id,
                                    prompt,
                                } = effect
                                {
                                    open_side_threads.insert(
                                        child_thread_id.clone(),
                                        parent_thread_id.clone(),
                                    );
                                    side_rounds.entry(child_thread_id.clone()).or_insert(1);
                                    side_turn_revisions
                                        .entry(child_thread_id.clone())
                                        .or_insert_with(|| {
                                            let mut state = UserTurnRevisionState::default();
                                            state.record_next_turn();
                                            state
                                        });
                                    emit_side_session_started(
                                        &drain_config,
                                        &parent_thread_id,
                                        &child_thread_id,
                                        prompt.as_deref(),
                                    );
                                    drain_external_child_turn(
                                        &mut agent,
                                        &mut event_rx,
                                        &mut external_control_rx,
                                        &drain_config,
                                        &mut stats,
                                        &mut diff_tracker,
                                        &mut pending_runtime_steers,
                                        &mut handled_steer_ids,
                                        &mut cancelled_follow_ups,
                                        &mut codex_thread_action_dedupe,
                                        child_thread_id,
                                        "side",
                                    )
                                    .await;
                                } else if let ExternalThreadActionEffect::SideTurnClosed {
                                    child_thread_id,
                                } = effect
                                {
                                    open_side_threads.remove(&child_thread_id);
                                    side_rounds.remove(&child_thread_id);
                                    side_turn_revisions.remove(&child_thread_id);
                                    emit_side_session_closed(&bus, child_thread_id);
                                }
                            }
                            Ok(AppEvent::InterruptRequested { session_id })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                // Ignore idle interrupts; this shared receiver
                                // consumed the event, so the next task will not
                                // inherit a stale Stop request. A live
                                // rate-limit park is the exception: the
                                // interrupt cancels the timer and drops the
                                // pending re-send (messages queued during the
                                // park stay queued and flush normally).
                                if let Some(park) = limit_park.take() {
                                    limit_park_streak = 0;
                                    error_park_streak = 0;
                                    slog(&session_log, |l| l.set_limit_park(None));
                                    let noun = park.kind.noun();
                                    let line = if park.pending.is_some() {
                                        format!("{noun} cancelled by interrupt — dropped the pending re-send")
                                    } else {
                                        format!("{noun} cancelled by interrupt")
                                    };
                                    slog(&session_log, |l| l.info(&line));
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "info".to_string(),
                                        source: "Intendant".to_string(),
                                        content: line,
                                        turn: None,
                                    });
                                }
                            }
                            Ok(AppEvent::ReloadBackendCredentials { session_id })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                // Idle reload: apply the in-place respawn
                                // right away (the park cancel inside
                                // preserves the pending re-send; queued
                                // messages flush through the preamble).
                                // No continuation is owed from idle, so
                                // tasks the restart killed surface through
                                // the attention state the respawn already
                                // published — never a minted nudge.
                                backend_credentials_reload
                                    .store(false, std::sync::atomic::Ordering::SeqCst);
                                // Respawn-after-close guard (card
                                // 01KZ0PRYE7…): a reload landing at a
                                // concluded seat must not resurrect it
                                // into a fresh idle row — the specimen
                                // fan-out minted a drain-holding husk
                                // this way. End the row instead, stated
                                // honestly. A parked seat with an owed
                                // re-send, queued work, live background
                                // tasks, or no attestation fails the
                                // shape and reloads normally.
                                {
                                    let mut conclude_ids: Vec<&str> = Vec::new();
                                    for id in [
                                        intendant_session_id.as_deref(),
                                        live_session_id.as_deref(),
                                        drain_config.session_id.as_deref(),
                                        drain_config.alias_session_id.as_deref(),
                                    ]
                                    .into_iter()
                                    .flatten()
                                    {
                                        if !conclude_ids.contains(&id) {
                                            conclude_ids.push(id);
                                        }
                                    }
                                    let facts = assemble_seat_conclude_facts(
                                        round_ran_in_this_wrapper,
                                        &parked_follow_ups,
                                        &follow_up_rx,
                                        &context_injection,
                                        &live_session_id,
                                        drain_config.alias_session_id.as_deref(),
                                        limit_park.is_some(),
                                        open_side_threads.len(),
                                        stats
                                            .announced_native_session_id
                                            .as_deref()
                                            .and_then(crate::native_wakeup::pending_for)
                                            .is_some(),
                                        &log_dir,
                                        &conclude_ids,
                                    );
                                    if facts.concluded() {
                                        emit_seat_conclude(
                                            &bus,
                                            &session_log,
                                            &live_session_id,
                                            SEAT_CONCLUDED_RELOAD_SKIP_LINE,
                                        );
                                        stats.terminal_outcome =
                                            Some("completed".to_string());
                                        break 'outer;
                                    }
                                }
                                if apply_backend_credentials_reload(
                                    &backend,
                                    &project,
                                    web_port,
                                    &intendant_session_id,
                                    &session_agent_config,
                                    &stats,
                                    &mut limit_park,
                                    &mut limit_park_streak,
                                    &mut error_park_streak,
                                    &mut parked_follow_ups,
                                    &mut agent,
                                    &mut event_rx,
                                    &mut drain_config,
                                )
                                .await
                                .is_none()
                                {
                                    stats.terminal_outcome = Some(
                                        "credential reload could not respawn the backend"
                                            .to_string(),
                                    );
                                    break 'outer;
                                }
                                event_channel_open = true;
                            }
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                slog(&session_log, |l| l.info("Event bus closed, exiting"));
                                daemon_teardown_exit = true;
                                stats.terminal_outcome = Some("event bus closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                }
            },
        };
        if follow_up_message_was_cancelled(&mut cancelled_follow_ups, &followup) {
            slog(&session_log, |l| {
                l.info("Skipped cancelled queued follow-up")
            });
            continue;
        }
        if let Some(park) = limit_park.as_ref() {
            // Armed park (either kind): never burn input against the
            // unavailable backend (a delivered message would be consumed
            // with zero work). Queue it and return to the idle wait; the
            // flush preamble delivers it after the pending re-send.
            slog(&session_log, |l| l.info(park.kind.queued_log()));
            bus.send(AppEvent::LogEntry {
                session_id: live_session_id.clone(),
                level: "info".to_string(),
                source: "Intendant".to_string(),
                content: park.kind.queued_log().to_string(),
                turn: None,
            });
            emit_follow_up_status(
                &bus,
                live_session_id.as_deref(),
                &followup.follow_up_id,
                Some(&followup.text),
                "queued",
                Some(park.kind.queued_status_detail()),
            );
            parked_follow_ups.push_back(followup);
            continue;
        }
        let active_followup_for_rewind_replay = followup.clone();
        let turn_text = followup.text;
        let attachments = followup.attachments;
        let steer_id = followup.steer_id;
        let follow_up_id = followup.follow_up_id;
        let edit_user_turn_index = followup.edit_user_turn_index;
        let edit_user_turn_revision = followup.edit_user_turn_revision;
        let edit_original_text = followup.edit_original_text;
        let claude_inplace_rewind_targets = followup.claude_inplace_rewind_targets;
        let unresolved_attachment_ids = followup.unresolved_attachment_ids;
        let target_session_id = followup.target_session_id.clone();
        let managed_context_recovery_kickstart = followup.managed_context_recovery_kickstart;
        let managed_context_density_handoff = followup.managed_context_density_handoff;
        let managed_context_density_handoff_completed =
            followup.managed_context_density_handoff_completed;

        if let Some(side_thread_id) = target_session_id
            .as_deref()
            .filter(|id| open_side_threads.contains_key(*id))
            .map(str::to_string)
        {
            let mut replacement_for_user_turn_index = None;
            if let Some(user_turn_index) = edit_user_turn_index {
                if !agent.supports_user_message_rewind() {
                    let message = format!("{} does not support user-message rewind", agent.name());
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                let current_side_round = *side_rounds.entry(side_thread_id.clone()).or_insert(1);
                let revisions = side_turn_revisions
                    .entry(side_thread_id.clone())
                    .or_default();
                revisions.seed_active_turns_to(current_side_round as u32);
                if let Err(message) =
                    revisions.validate_expected_revision(user_turn_index, edit_user_turn_revision)
                {
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                match rollback_side_thread_from_turn(
                    &mut agent,
                    &mut side_rounds,
                    &mut side_turn_revisions,
                    &side_thread_id,
                    user_turn_index,
                    &drain_config,
                )
                .await
                {
                    Ok(turns_to_drop) => {
                        replacement_for_user_turn_index = Some(user_turn_index);
                        let message = format!(
                            "Edited side user turn {}; rolled back {} turn{}",
                            user_turn_index,
                            turns_to_drop,
                            if turns_to_drop == 1 { "" } else { "s" }
                        );
                        slog(&session_log, |l| l.info(&message));
                    }
                    Err(message) => {
                        slog(&session_log, |l| l.warn(&message));
                        bus.send(AppEvent::LoopError(message));
                        continue;
                    }
                }
            }

            let side_round = side_rounds.entry(side_thread_id.clone()).or_insert(0);
            *side_round += 1;
            // Prompt ordinal from the revision state (side rounds track it
            // 1:1 today, but the ordinal is the emitted authority — see
            // the primary emit site).
            let (side_user_turn_index, user_turn_revision) = side_turn_revisions
                .entry(side_thread_id.clone())
                .or_default()
                .record_next_turn();
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&side_thread_id),
                Some(side_user_turn_index),
                Some(user_turn_revision),
                replacement_for_user_turn_index,
                &attachments.refs,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&side_thread_id),
                None,
            )
            .unwrap_or_else(|| turn_text.clone());
            let side_thread = external_agent::AgentThread {
                thread_id: side_thread_id.clone(),
            };
            emit_external_turn_status(
                &bus,
                &autonomy,
                Some(&side_thread_id),
                *side_round,
                "thinking",
                format!("{} side turn in progress", agent.name()),
            )
            .await;
            let send_result = if attachments.is_empty() {
                agent.send_message(&side_thread, &merged).await
            } else {
                warn_undeliverable_images(
                    &session_log,
                    agent.name(),
                    agent.supports_image_input(),
                    &attachments.items,
                );
                agent
                    .send_message_with_attachments(&side_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                emit_follow_up_status(
                    &bus,
                    Some(&side_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send side follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send side follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&side_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(side_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            let parent_thread_id = open_side_threads.get(&side_thread_id).cloned();
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut external_control_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                &mut handled_steer_ids,
                &mut cancelled_follow_ups,
                &mut codex_thread_action_dedupe,
                side_thread_id,
                "side",
            )
            .await;
            if let Some(parent_thread_id) = parent_thread_id {
                if let Err(e) = agent.activate_thread(&parent_thread_id).await {
                    let message = format!("Failed to restore Codex parent thread: {}", e);
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                }
            }
            continue;
        }

        if let Some(subagent_thread_id) = target_session_id
            .as_deref()
            .filter(|id| stats.codex_subagent_parent_threads.contains_key(*id))
            .map(str::to_string)
        {
            if edit_user_turn_index.is_some() {
                let message = format!(
                    "User-message rewind is not supported for Codex subagent session {}",
                    subagent_thread_id.chars().take(8).collect::<String>()
                );
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
                continue;
            }

            let subagent_round = stats
                .codex_subagent_rounds
                .entry(subagent_thread_id.clone())
                .or_insert(0);
            *subagent_round += 1;
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&subagent_thread_id),
                Some(*subagent_round as u32),
                None,
                None,
                &attachments.refs,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&subagent_thread_id),
                None,
            )
            .unwrap_or_else(|| turn_text.clone());
            let subagent_thread = external_agent::AgentThread {
                thread_id: subagent_thread_id.clone(),
            };
            let parent_thread_id = stats
                .codex_subagent_parent_threads
                .get(&subagent_thread_id)
                .cloned()
                .unwrap_or_else(|| thread.thread_id.clone());
            emit_external_turn_status(
                &bus,
                &autonomy,
                Some(&subagent_thread_id),
                *subagent_round,
                "thinking",
                format!("{} subagent turn in progress", agent.name()),
            )
            .await;
            let send_result = if attachments.is_empty() {
                agent.send_message(&subagent_thread, &merged).await
            } else {
                warn_undeliverable_images(
                    &session_log,
                    agent.name(),
                    agent.supports_image_input(),
                    &attachments.items,
                );
                agent
                    .send_message_with_attachments(&subagent_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                let _ = agent.activate_thread(&parent_thread_id).await;
                emit_follow_up_status(
                    &bus,
                    Some(&subagent_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send subagent follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send subagent follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&subagent_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(subagent_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut external_control_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                &mut handled_steer_ids,
                &mut cancelled_follow_ups,
                &mut codex_thread_action_dedupe,
                subagent_thread_id,
                "subagent",
            )
            .await;
            if let Err(e) = agent.activate_thread(&parent_thread_id).await {
                let message = format!("Failed to restore Codex parent thread: {}", e);
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
            }
            continue;
        }

        let managed_context_rewind_only_preflight_enabled =
            managed_context_preflight_rewind_only_gate_enabled(
                codex_managed_context_enabled,
                managed_context_recovery_kickstart,
                managed_context_density_handoff,
            );
        if managed_context_rewind_only_preflight_enabled {
            match refresh_external_context_usage_snapshot_for_preflight(&mut agent, &drain_config)
                .await
            {
                Ok(Some(snapshot)) => {
                    if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot) {
                        let drop_original = managed_context_drop_original_for_recovery(
                            &turn_text,
                            !attachments.is_empty(),
                            steer_id.is_some(),
                            edit_user_turn_index.is_some(),
                        );
                        let held_user_input = !drop_original;
                        if held_user_input {
                            pending_managed_context_replays.push_back(FollowUpMessage {
                                text: turn_text.clone(),
                                attachments: attachments.clone(),
                                steer_id: steer_id.clone(),
                                follow_up_id: follow_up_id.clone(),
                                edit_user_turn_index,
                                edit_user_turn_revision,
                                edit_original_text: edit_original_text.clone(),
                                unresolved_attachment_ids: unresolved_attachment_ids.clone(),
                                target_session_id: target_session_id.clone(),
                                // Codex managed-context lane: never a
                                // claude in-place edit.
                                claude_inplace_rewind_targets: Vec::new(),
                                managed_context_recovery_kickstart: false,
                                managed_context_density_handoff: false,
                                managed_context_density_handoff_completed: false,
                            });
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                None,
                                "queued",
                                Some(
                                    "managed context is above the rewind-only threshold; recovering before sending this follow-up",
                                ),
                            );
                        } else {
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                Some(&turn_text),
                                "queued",
                                Some(
                                    "managed context is above the rewind-only threshold; treating this as a recovery kickstart",
                                ),
                            );
                        }

                        let recovery_text =
                            managed_context_recovery_kickstart_text(pressure, held_user_input);
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Holding Codex follow-up during managed-context {} pressure ({}/{} tokens); sending recovery kickstart",
                                pressure.status,
                                pressure.used_tokens,
                                pressure.rewind_only_limit
                            ))
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "info".to_string(),
                            source: "Intendant".to_string(),
                            content: format!(
                                "Managed context is in rewind-only pressure ({}/{} tokens); {}.",
                                pressure.used_tokens,
                                pressure.rewind_only_limit,
                                if held_user_input {
                                    "holding the user follow-up until recovery succeeds"
                                } else {
                                    "using the request as a recovery kickstart"
                                }
                            ),
                            turn: None,
                        });
                        let mut recovery_followup = FollowUpMessage::text(recovery_text)
                            .managed_context_recovery_kickstart();
                        if !held_user_input {
                            recovery_followup =
                                recovery_followup.with_follow_up_id(follow_up_id.clone());
                        }
                        next_turn = Some(recovery_followup);
                        continue 'outer;
                    } else if managed_context_preflight_density_gate_enabled(
                        managed_context_rewind_only_preflight_enabled,
                        managed_context_density_handoff_completed,
                    ) {
                        if let Some(pressure) = managed_context_density_pressure(&snapshot) {
                            pending_managed_context_replays.push_back(FollowUpMessage {
                                text: turn_text.clone(),
                                attachments: attachments.clone(),
                                steer_id: steer_id.clone(),
                                follow_up_id: follow_up_id.clone(),
                                edit_user_turn_index,
                                edit_user_turn_revision,
                                edit_original_text: edit_original_text.clone(),
                                unresolved_attachment_ids: unresolved_attachment_ids.clone(),
                                target_session_id: target_session_id.clone(),
                                // Codex managed-context lane: never a
                                // claude in-place edit.
                                claude_inplace_rewind_targets: Vec::new(),
                                managed_context_recovery_kickstart: false,
                                managed_context_density_handoff: false,
                                managed_context_density_handoff_completed: false,
                            });
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                None,
                                "queued",
                                Some(
                                    "managed context is above the recommended density threshold; sending density handoff before broad follow-up",
                                ),
                            );
                            let handoff_text = managed_context_density_handoff_text(pressure);
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "Holding Codex follow-up during managed-context density watch ({}/{} tokens, threshold {}); sending density handoff",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                ))
                            });
                            bus.send(AppEvent::LogEntry {
                                session_id: live_session_id.clone(),
                                level: "info".to_string(),
                                source: "Intendant".to_string(),
                                content: format!(
                                    "Managed context is above the recommended density threshold ({}/{} tokens, threshold {}). Sending a density handoff before broad follow-up work. Normal tools remain allowed below rewind-only pressure.",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                ),
                                turn: None,
                            });
                            next_turn = Some(
                                FollowUpMessage::text(handoff_text)
                                    .managed_context_density_handoff(),
                            );
                            continue 'outer;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    slog(&session_log, |l| {
                        l.debug(&format!(
                            "Could not read Codex context snapshot before follow-up gate: {}",
                            e
                        ))
                    });
                }
            }
        }

        let mut replacement_for_user_turn_index = None;
        // Claude Code in-place edit: transcript-addressed (the supervisor
        // resolved the uuid walk-back list), so it never consults the
        // index-addressed `supports_user_message_rewind` protocol below.
        // Refusals hand the edit BACK to the supervisor's ladder via the
        // `rewind-unavailable` status — this arm must never service a
        // refused edit itself, or two rungs would both run it.
        let claude_inplace_edit = edit_user_turn_index.is_some()
            && backend == external_agent::AgentBackend::ClaudeCode
            && !claude_inplace_rewind_targets.is_empty();
        if claude_inplace_edit {
            let user_turn_index = edit_user_turn_index.unwrap_or_default();
            bus.send(AppEvent::UserMessageEditStatus {
                session_id: live_session_id.clone(),
                user_turn_index,
                status: "running".to_string(),
                message: "rewinding this session in place — the edited prompt continues here"
                    .to_string(),
            });
            let mut rewind_refusal: Option<String> = None;
            for target_uuid in &claude_inplace_rewind_targets {
                match agent
                    .rewind_conversation_to_message(target_uuid, true)
                    .await
                {
                    Ok(outcome) if outcome.rewound => {}
                    Ok(outcome) => {
                        rewind_refusal = Some(outcome.detail.unwrap_or_else(|| {
                            "the CLI refused the rewind without a reason".to_string()
                        }));
                        break;
                    }
                    Err(err) => {
                        rewind_refusal = Some(err.to_string());
                        break;
                    }
                }
            }
            if let Some(reason) = rewind_refusal {
                let message = format!("in-place rewind unavailable: {reason}");
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "rewind-unavailable".to_string(),
                    message,
                });
                continue;
            }
            let turns_removed = claude_inplace_rewind_targets.len() as u32;
            // Supervision-run turn bookkeeping is best-effort here: the
            // transcript is truth after a rewind, and resumes re-seed
            // from it; an out-of-range index only means this run's
            // counter drift stays cosmetic.
            if user_turn_index >= 1 && user_turn_index <= user_turn_revisions.active_count() {
                user_turn_revisions.rewind_from_turn(user_turn_index);
                round = user_turn_index.saturating_sub(1) as usize;
            }
            replacement_for_user_turn_index = Some(user_turn_index);
            let message = format!(
                "Rewound {} conversation turn{} in place; the edited prompt continues in this session",
                turns_removed,
                if turns_removed == 1 { "" } else { "s" }
            );
            slog(&session_log, |l| l.info(&message));
            bus.send(AppEvent::UserMessageRewind {
                session_id: live_session_id.clone(),
                user_turn_index,
                turns_removed,
            });
            bus.send(AppEvent::UserMessageEditStatus {
                session_id: live_session_id.clone(),
                user_turn_index,
                status: "ok".to_string(),
                message,
            });
        } else if let Some(user_turn_index) = edit_user_turn_index {
            bus.send(AppEvent::UserMessageEditStatus {
                session_id: live_session_id.clone(),
                user_turn_index,
                status: "running".to_string(),
                message: format!("applying edit to user turn {}", user_turn_index),
            });
            if !agent.supports_user_message_rewind() {
                let message = format!("{} does not support user-message rewind", agent.name());
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    agent.name(),
                    message,
                );
                continue;
            }
            if user_turn_index == 0 {
                let message = format!(
                    "Cannot edit user turn 0 in {} session {}",
                    backend,
                    live_session_id
                        .as_deref()
                        .map(|sid| sid.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "unknown".to_string())
                );
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            let active_edit_revision_ok = user_turn_index <= user_turn_revisions.active_count()
                && user_turn_revisions
                    .validate_expected_revision(user_turn_index, edit_user_turn_revision)
                    .is_ok();
            let mut archived_edit_branch_not_found = false;
            if !active_edit_revision_ok && codex_managed_context_enabled {
                match fork_managed_context_edit_branch(
                    &mut agent,
                    &thread.thread_id,
                    user_turn_index,
                    edit_original_text.as_deref(),
                    turn_text.clone(),
                    unresolved_attachment_ids.clone(),
                    &drain_config,
                )
                .await
                {
                    Ok(Some(message)) => {
                        slog(&session_log, |l| l.info(&message));
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: live_session_id.clone(),
                            action: "managed-edit-branch".to_string(),
                            success: true,
                            message: message.clone(),
                            record_id: None,
                        });
                        emit_follow_up_status(
                            &bus,
                            live_session_id.as_deref(),
                            &follow_up_id,
                            Some(&turn_text),
                            "queued",
                            Some("created managed edit branch from archived context"),
                        );
                        bus.send(AppEvent::UserMessageEditStatus {
                            session_id: live_session_id.clone(),
                            user_turn_index,
                            status: "ok".to_string(),
                            message,
                        });
                        continue 'outer;
                    }
                    Ok(None) => {
                        archived_edit_branch_not_found = true;
                    }
                    Err(message) => {
                        bus.send(AppEvent::UserMessageEditStatus {
                            session_id: live_session_id.clone(),
                            user_turn_index,
                            status: "failed".to_string(),
                            message: message.clone(),
                        });
                        emit_external_session_loop_error(
                            &bus,
                            &session_log,
                            live_session_id.as_deref(),
                            &backend.to_string(),
                            message,
                        );
                        continue;
                    }
                }
            }
            if user_turn_index > user_turn_revisions.active_count() {
                let message = format!(
                    "Cannot edit user turn {} in {} session {}; current user turn count is {}",
                    user_turn_index,
                    backend,
                    live_session_id
                        .as_deref()
                        .map(|sid| sid.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "unknown".to_string()),
                    user_turn_revisions.active_count()
                );
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            if let Err(message) = user_turn_revisions
                .validate_expected_revision(user_turn_index, edit_user_turn_revision)
            {
                let message = if archived_edit_branch_not_found {
                    format!(
                        "{message}. No matching managed-context archive was found for the clicked message text; the selected turn is no longer active and cannot be safely edited from this attach wrapper."
                    )
                } else {
                    message
                };
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            // Rollback depth is counted in USER turns from the prompt
            // ordinal state — `round` can exceed it (spontaneous backend
            // rounds), and a round-based count over-rolls the backend.
            let turns_to_drop = user_turn_revisions.active_count() - user_turn_index + 1;
            let mut rollback_result = agent.rollback_turns(turns_to_drop).await;
            if let Err(err) = rollback_result.as_ref() {
                if backend == external_agent::AgentBackend::Codex
                    && external_rollback_turn_in_progress(err)
                {
                    let message = format!(
                        "Codex still has a turn in progress; pausing autonomous goal work and waiting before editing user turn {}",
                        user_turn_index
                    );
                    slog(&session_log, |l| l.info(&message));
                    bus.send(AppEvent::LogEntry {
                        session_id: live_session_id.clone(),
                        level: "info".to_string(),
                        source: "Codex".to_string(),
                        content: message,
                        turn: None,
                    });
                    match agent.pause_autonomous_goal(&thread.thread_id).await {
                        Ok(result) => {
                            if let Some(goal) = result.goal {
                                emit_external_session_goal(
                                    &drain_config,
                                    live_session_id.clone(),
                                    Some(goal),
                                );
                            } else if result.goal_absent {
                                emit_external_session_goal(
                                    &drain_config,
                                    live_session_id.clone(),
                                    None,
                                );
                            }
                        }
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not pause Codex goal before edit rollback retry: {}",
                                    e
                                ))
                            });
                        }
                    }

                    let mut side_session_state = ExternalSideSessionState {
                        open_side_threads: &mut open_side_threads,
                        side_rounds: &mut side_rounds,
                        side_turn_revisions: &mut side_turn_revisions,
                    };
                    let drain_outcome = drain_external_agent_events(
                        &mut agent,
                        &mut event_rx,
                        &mut external_control_rx,
                        &drain_config,
                        &mut stats,
                        &mut diff_tracker,
                        &mut pending_runtime_steers,
                        &mut handled_steer_ids,
                        &mut cancelled_follow_ups,
                        &mut codex_thread_action_dedupe,
                        Some(&mut side_session_state),
                        Some(&mut user_turn_revisions),
                        false,
                        false,
                        false,
                    )
                    .await;
                    // A native id announced mid-turn (Claude Code's first
                    // turn) becomes the loop's primary address before the
                    // outcome is reported, so follow-up controls targeting
                    // the upgraded id match this conversation.
                    if let Some(native) = stats.announced_native_session_id.take() {
                        if backend.thread_id_is_canonical(&native) {
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "External session address upgraded to native id {}",
                                    short_external_session_id(&native)
                                ))
                            });
                            rotate_external_identity(
                                &native,
                                &mut live_session_id,
                                &mut drain_config,
                            );
                        }
                    }
                    match drain_outcome {
                        DrainOutcome::TurnCompleted {
                            message,
                            turns_in_round,
                        } => {
                            stats.rounds = round;
                            record_external_done_and_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                live_session_id.as_deref(),
                                message.as_deref(),
                                round,
                                turns_in_round,
                            );
                            bus.send(AppEvent::DoneSignal {
                                session_id: live_session_id.clone(),
                                message: message.clone(),
                            });
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round,
                                native_message_count: None,
                                project_root: round_session_root.clone(),
                            });
                        }
                        DrainOutcome::LimitRejected {
                            resets_at_epoch, ..
                        } => {
                            // The in-flight turn ended rejected at the
                            // provider limit; no round to record. The turn
                            // is over, so the user's edit rollback below
                            // proceeds like any completed turn — and
                            // deliberately WITHOUT a park: the edit rewinds
                            // past the rejected turn, so a resume nudge
                            // would re-drive work the user just superseded.
                            // The edited message delivers next and parks
                            // properly through the primary arm if the limit
                            // still holds. The line must not claim "parked"
                            // (a parked claim with nothing armed is the
                            // silent-loss bug class).
                            let line = format!(
                                "Rate-limited — the in-flight turn was rejected; {}; proceeding with the edit rollback",
                                external_agent::limit_reset_phrase(
                                    resets_at_epoch,
                                    crate::session_activity::epoch_seconds(),
                                ),
                            );
                            slog(&session_log, |l| l.warn(&line));
                            bus.send(AppEvent::LogEntry {
                                session_id: live_session_id.clone(),
                                level: "warn".to_string(),
                                source: "Intendant".to_string(),
                                content: line,
                                turn: None,
                            });
                        }
                        DrainOutcome::SafeguardsFlagged { reason, .. } => {
                            // The provider's safeguards flagged the
                            // in-flight turn the user's edit already
                            // superseded. Proceed with the rollback: the
                            // edit rewrites the offending tail (a recast
                            // by the owner's own hand), and if the
                            // retained context still flags, the edited
                            // message's own round ends through the
                            // primary safeguards terminal. No park, no
                            // retry, never a model switch.
                            let line = format!(
                                "Provider safeguards flagged the in-flight turn ({}); proceeding with the edit rollback — the edit replaces the flagged content",
                                reason
                            );
                            slog(&session_log, |l| l.warn(&line));
                            bus.send(AppEvent::LogEntry {
                                session_id: live_session_id.clone(),
                                level: "warn".to_string(),
                                source: "Intendant".to_string(),
                                content: line,
                                turn: None,
                            });
                        }
                        DrainOutcome::TransientRoundDeath { reason, .. } => {
                            // The in-flight turn died on a temporary
                            // service condition mid-edit. Like the limit
                            // arm above: no park — the edit rewinds past
                            // the dead turn, so a parked continuation
                            // would re-drive work the user just
                            // superseded. The edited message delivers
                            // next and parks through the primary arm if
                            // the condition still holds.
                            let line = format!(
                                "Temporary service condition ended the in-flight turn ({}); proceeding with the edit rollback",
                                reason
                            );
                            slog(&session_log, |l| l.warn(&line));
                            bus.send(AppEvent::LogEntry {
                                session_id: live_session_id.clone(),
                                level: "warn".to_string(),
                                source: "Intendant".to_string(),
                                content: line,
                                turn: None,
                            });
                        }
                        DrainOutcome::ContextRewindRequested {
                            request,
                            message,
                            turns_in_round,
                            ..
                        } => {
                            stats.rounds = round;
                            record_external_done_and_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                live_session_id.as_deref(),
                                message.as_deref(),
                                round,
                                turns_in_round,
                            );
                            bus.send(AppEvent::DoneSignal {
                                session_id: live_session_id.clone(),
                                message: message.clone(),
                            });
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round,
                                native_message_count: None,
                                project_root: round_session_root.clone(),
                            });
                            emit_context_rewind_failure(
                                &request,
                                "user edit superseded the pending context rewind".to_string(),
                                &drain_config,
                            );
                        }
                        DrainOutcome::Interrupted { reason } => {
                            stats.rounds = round;
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "External agent interrupted before edit rollback: {}",
                                    reason
                                ))
                            });
                            record_external_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                round,
                                stats.turns,
                            );
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round: stats.turns,
                                native_message_count: None,
                                project_root: round_session_root.clone(),
                            });
                        }
                        DrainOutcome::TurnFailed { reason, .. } => {
                            // The in-flight turn died on a fatal error
                            // before running anything; the edit cannot be
                            // applied to a failed round — report and skip
                            // the rollback, like recovery/termination.
                            let message = format!(
                                "{} turn failed before edit rollback: {}",
                                agent.name(),
                                reason
                            );
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                        DrainOutcome::RecoveryRequired {
                            message,
                            recovery_hint,
                            ..
                        } => {
                            let message =
                                recovery_required_message(&message, recovery_hint.as_deref());
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                        DrainOutcome::Terminated { reason, exit_code } => {
                            let message = format!(
                                "{} terminated before edit rollback: {} (exit code: {:?})",
                                agent.name(),
                                reason,
                                exit_code
                            );
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                        DrainOutcome::ChannelClosed => {
                            let message =
                                "External agent event channel closed before edit rollback"
                                    .to_string();
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                    }
                    rollback_result = agent.rollback_turns(turns_to_drop).await;
                }
            }
            match rollback_result {
                Ok(()) => {
                    user_turn_revisions.rewind_from_turn(user_turn_index);
                    round = user_turn_index.saturating_sub(1) as usize;
                    replacement_for_user_turn_index = Some(user_turn_index);
                    let message = format!(
                        "Edited user turn {}; rolled back {} turn{}",
                        user_turn_index,
                        turns_to_drop,
                        if turns_to_drop == 1 { "" } else { "s" }
                    );
                    slog(&session_log, |l| l.info(&message));
                    bus.send(AppEvent::UserMessageRewind {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        turns_removed: turns_to_drop,
                    });
                    bus.send(AppEvent::UserMessageEditStatus {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        status: "ok".to_string(),
                        message,
                    });
                }
                Err(e) => {
                    let message = format!(
                        "Cannot edit user turn {} in {} session: {}",
                        user_turn_index, backend, e
                    );
                    bus.send(AppEvent::UserMessageEditStatus {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        status: "failed".to_string(),
                        message: message.clone(),
                    });
                    emit_external_session_loop_error(
                        &bus,
                        &session_log,
                        live_session_id.as_deref(),
                        &backend.to_string(),
                        message,
                    );
                    continue;
                }
            }
        }

        round += 1;
        round_ran_in_this_wrapper = true;
        // The emitted turn index is the PROMPT ORDINAL from the revision
        // state — never `round`, which also counts spontaneous backend
        // rounds (and restarts on resume for backends without a seed), so
        // it drifts off the transcript lane's positional numbering.
        // `round` stays the round counter for status lines/RoundComplete.
        // After an edit rollback (`rewind_from_turn` above) this yields
        // the edited index with a bumped revision, exactly like the old
        // round-aligned bookkeeping.
        let (user_turn_index, user_turn_revision) = user_turn_revisions.record_next_turn();
        stats.turns = 0;
        let attachment_count = attachments.len();
        let merged = drain_steer_queue_as_followup(
            &context_injection,
            &turn_text,
            &bus,
            live_session_id.as_deref(),
            drain_config.alias_session_id.as_deref(),
        )
        .unwrap_or_else(|| turn_text.clone());
        let user_log_text = if turn_text.trim().is_empty() {
            &merged
        } else {
            &turn_text
        };
        emit_user_message_log(
            &bus,
            &session_log,
            live_session_id.as_deref(),
            Some(user_turn_index),
            Some(user_turn_revision),
            replacement_for_user_turn_index,
            &attachments.refs,
            user_log_text,
        );
        slog(&session_log, |l| {
            if round == 1 {
                l.info(&format!(
                    "Initial task sent to external agent{}",
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" with {} attachment(s)", attachment_count)
                    }
                ));
            } else {
                l.info(&format!(
                    "Follow-up round {}: {}{}",
                    round,
                    merged,
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" ({} attachment(s))", attachment_count)
                    }
                ));
            }
        });
        diff_tracker.seed_from_session_log(&project.root, &log_dir);
        emit_external_turn_status(
            &bus,
            &autonomy,
            live_session_id.as_deref(),
            round,
            "thinking",
            external_turn_status_task(agent.name(), round, user_log_text),
        )
        .await;
        let send_result = if attachments.is_empty() {
            agent.send_message(&thread, &merged).await
        } else {
            warn_undeliverable_images(
                &session_log,
                agent.name(),
                agent.supports_image_input(),
                &attachments.items,
            );
            agent
                .send_message_with_attachments(&thread, &merged, &attachments.items)
                .await
        };
        if let Err(e) = send_result {
            emit_follow_up_status(
                &bus,
                live_session_id.as_deref(),
                &follow_up_id,
                Some(&turn_text),
                "failed",
                Some("failed to send follow-up"),
            );
            if round == 1 {
                return Err(e);
            }
            bus.send(AppEvent::LoopError(format!(
                "Failed to send follow-up: {}",
                e
            )));
            stats.terminal_outcome = Some(format!("failed to send follow-up: {}", e));
            break;
        }
        emit_follow_up_status(
            &bus,
            live_session_id.as_deref(),
            &follow_up_id,
            Some(&turn_text),
            "delivered",
            None,
        );
        if let Some(id) = follow_up_id.as_deref() {
            // Pairs with the supervisor's "FollowUp … queued" daemon-log
            // line; queued without delivered means the queue stopped
            // draining.
            slog(&session_log, |l| {
                l.debug(&format!("Follow-up {} delivered to {}", id, agent.name()))
            });
        }
        if let Some(id) = steer_id {
            bus.send(AppEvent::SteerDelivered {
                session_id: live_session_id.clone(),
                id,
                mid_turn: false,
            });
        }

        let mut side_session_state = ExternalSideSessionState {
            open_side_threads: &mut open_side_threads,
            side_rounds: &mut side_rounds,
            side_turn_revisions: &mut side_turn_revisions,
        };
        let drain_outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut external_control_rx,
            &drain_config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut codex_thread_action_dedupe,
            Some(&mut side_session_state),
            Some(&mut user_turn_revisions),
            managed_context_recovery_kickstart,
            managed_context_density_handoff,
            managed_context_density_handoff_completed,
        )
        .await;
        // A native id announced mid-turn (Claude Code's first turn) becomes
        // the loop's primary address before the outcome is reported, so
        // targeted controls sent under the upgraded id keep matching.
        if let Some(native) = stats.announced_native_session_id.take() {
            if backend.thread_id_is_canonical(&native) {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "External session address upgraded to native id {}",
                        short_external_session_id(&native)
                    ))
                });
                rotate_external_identity(&native, &mut live_session_id, &mut drain_config);
            }
        }
        match drain_outcome {
            DrainOutcome::SafeguardsFlagged {
                reason,
                turns_in_round,
            } => {
                // The provider's safeguards flagged the conversation:
                // terminal for these bytes whatever the round count — no
                // park (mechanical retry re-flags forever), no model
                // fallback, no completion (a DoneSignal journaled the
                // 2026-07-31 specimen 69c8535e COMPLETED at 95 turns and
                // the death was invisible). The honest terminal is FAILED
                // plus the safeguards attention surfaces; the remedy is
                // the owner's fresh-session recast.
                stats.rounds = round;
                let entry = crate::safeguards_recast::RecastRef {
                    session_id: live_session_id.clone().unwrap_or_default(),
                    source: agent.name().to_string(),
                    reason: reason.clone(),
                    disposition: crate::safeguards_recast::RecastDisposition::SessionEnded,
                };
                let line = crate::safeguards_recast::safeguards_flag_line(&reason);
                slog(&session_log, |l| l.error(&line));
                // Durable first: the boot pass and the session catalog
                // key on the meta marker, so it must survive whatever
                // happens after this point.
                slog(&session_log, |l| {
                    l.set_safeguards_flag(entry.meta(crate::session_activity::epoch_seconds()))
                });
                let (retired_steers, failed_follow_ups) = surface_undelivered_input_at_terminal(
                    &bus,
                    &live_session_id,
                    &mut pending_runtime_steers,
                    &mut parked_follow_ups,
                    crate::safeguards_recast::SAFEGUARDS_UNDELIVERED_DETAIL,
                );
                if retired_steers + failed_follow_ups > 0 {
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "Surfaced {} owed message(s) as undelivered — the flagged session never redelivers them",
                            retired_steers + failed_follow_ups
                        ))
                    });
                }
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                    project_root: round_session_root.clone(),
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: reason.clone(),
                    // The cause is the story; the last response IS the
                    // provider's flag banner, so echoing it as a summary
                    // would double it.
                    summary: None,
                    outcome: crate::event::TaskOutcome::Failed,
                });
                crate::safeguards_recast::report_safeguards_flag(
                    &bus,
                    crate::agenda::published_agenda_handle().as_deref(),
                    &entry,
                );
                stats.terminal_outcome = Some(reason);
                break;
            }
            DrainOutcome::TurnFailed {
                reason,
                turns_in_round,
            } => {
                // The launch-refusal class: a fatal backend error ended the
                // round before any turn ran (an invalid model pin, an auth
                // refusal at spawn). The honest terminal is FAILED — the
                // typed outcome is what the agenda scheduler journals
                // (streaks, suspension, owner visibility); a DoneSignal
                // here journaled a fable-5 refusal COMPLETED on 2026-07-26
                // (occurrence 21fe746a). Follow-ups would re-fire into the
                // same refusal (the pin rides the launch config), so the
                // supervised span ends like a termination.
                stats.rounds = round;
                slog(&session_log, |l| {
                    l.error(&format!(
                        "External agent round failed before any turn completed: {reason}"
                    ))
                });
                // Queued input would re-fire into the same refusal —
                // surface it with the named reason instead of dropping
                // it silently with the loop.
                surface_undelivered_input_at_terminal(
                    &bus,
                    &live_session_id,
                    &mut pending_runtime_steers,
                    &mut parked_follow_ups,
                    &undelivered_detail_for_terminal(&reason),
                );
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                    project_root: round_session_root.clone(),
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: reason.clone(),
                    // Never the "agent's last words": for a round that ran
                    // nothing, the cause is the whole story.
                    summary: None,
                    outcome: crate::event::TaskOutcome::Failed,
                });
                stats.terminal_outcome = Some(reason);
                break;
            }
            DrainOutcome::TurnCompleted {
                message,
                turns_in_round,
            } => {
                stats.rounds = round;
                // A completed turn proves the provider is serving again.
                limit_park_streak = 0;
                error_park_streak = 0;
                if codex_managed_context_enabled {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                managed_context_recovery_kickstarts_without_rewind =
                                    managed_context_recovery_kickstarts_without_rewind
                                        .saturating_add(1);
                                if managed_context_recovery_kickstarts_without_rewind
                                    < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                {
                                    let held_user_input =
                                        !pending_managed_context_replays.is_empty();
                                    let recovery_text = managed_context_recovery_kickstart_text(
                                        pressure,
                                        held_user_input,
                                    );
                                    let turn_kind = if managed_context_recovery_kickstart {
                                        "recovery kickstart"
                                    } else {
                                        "managed Codex turn"
                                    };
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Managed-context {turn_kind} completed without a context rewind while pressure remains {}/{} tokens; retrying recovery",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ))
                                    });
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content: format!(
                                            "Managed-context {turn_kind} did not reduce context below the rewind-only threshold; context still reports {}/{} tokens. Retrying recovery before sending any normal follow-up.",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ),
                                        turn: None,
                                    });
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                        project_root: round_session_root.clone(),
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(recovery_text)
                                            .managed_context_recovery_kickstart(),
                                    );
                                    continue 'outer;
                                } else {
                                    // Model-driven recovery exhausted its retry
                                    // budget (the fork's recovery turn hit its
                                    // step limit each time without rewinding).
                                    // Backstop: supervisor-forced surgical
                                    // rewind instead of session death.
                                    let mut surgical_failure = None;
                                    if managed_context_surgical_recovery_available(
                                        managed_context_surgical_recoveries,
                                    ) {
                                        match attempt_supervisor_surgical_context_rewind(
                                            &mut agent,
                                            &thread.thread_id,
                                            &drain_config,
                                            surgical_task_statement.as_deref(),
                                            &mut pending_managed_context_replays,
                                        )
                                        .await
                                        {
                                            Ok(continuation) => {
                                                managed_context_surgical_recoveries =
                                                    managed_context_surgical_recoveries
                                                        .saturating_add(1);
                                                managed_context_recovery_kickstarts_without_rewind =
                                                    0;
                                                let content = format!(
                                                    "Managed-context recovery exhausted {} kickstarts without a rewind at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                    MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                                    pressure.used_tokens,
                                                    pressure.rewind_only_limit,
                                                    managed_context_surgical_recoveries,
                                                    MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                                );
                                                slog(&session_log, |l| l.warn(&content));
                                                bus.send(AppEvent::LogEntry {
                                                    session_id: live_session_id.clone(),
                                                    level: "warn".to_string(),
                                                    source: "Intendant".to_string(),
                                                    content,
                                                    turn: None,
                                                });
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                    project_root: round_session_root.clone(),
                                                });
                                                next_turn = Some(continuation);
                                                continue 'outer;
                                            }
                                            Err(e) => surgical_failure = Some(e),
                                        }
                                    }
                                    let mut message = format!(
                                        "Managed-context recovery completed without rewind_context while context remains above the rewind-only threshold ({}/{} tokens); refusing to send normal follow-ups.",
                                        pressure.used_tokens,
                                        pressure.rewind_only_limit
                                    );
                                    match surgical_failure {
                                        Some(failure) => {
                                            message.push_str(&format!(
                                                " Supervisor surgical rewind also failed: {failure}"
                                            ));
                                        }
                                        None => {
                                            message.push_str(&format!(
                                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                                            ));
                                        }
                                    }
                                    slog(&session_log, |l| l.warn(&message));
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                        project_root: round_session_root.clone(),
                                    });
                                    bus.send(AppEvent::LoopError(message));
                                    stats.terminal_outcome = Some(
                                        "managed Codex context pressure unresolved".to_string(),
                                    );
                                    break;
                                }
                            } else {
                                managed_context_recovery_kickstarts_without_rewind = 0;
                                managed_context_density_block_handoffs_without_relief = 0;
                                if managed_context_recovery_without_rewind_blocks_held_replay(
                                    managed_context_recovery_kickstart,
                                    &pending_managed_context_replays,
                                ) {
                                    let message = "Managed-context recovery turn completed without rewind_context; refusing to replay held normal follow-up until a successful rewind lowers context pressure.".to_string();
                                    slog(&session_log, |l| l.warn(&message));
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                        project_root: round_session_root.clone(),
                                    });
                                    bus.send(AppEvent::LoopError(message));
                                    stats.terminal_outcome =
                                        Some("managed Codex recovery did not rewind".to_string());
                                    break;
                                }
                                if let Some(mut replay) =
                                    pending_managed_context_replays.pop_front()
                                {
                                    if managed_context_density_handoff {
                                        slog(&session_log, |l| {
                                            l.info(
                                                "Managed-context density handoff completed without a context rewind; replaying held follow-up",
                                            )
                                        });
                                        replay = replay.after_managed_context_density_handoff();
                                    } else {
                                        slog(&session_log, |l| {
                                            l.warn(
                                                "Managed-context pressure cleared without a context rewind; replaying held follow-up",
                                            )
                                        });
                                    }
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                        project_root: round_session_root.clone(),
                                    });
                                    next_turn = Some(replay);
                                    continue 'outer;
                                }
                                if managed_context_post_turn_density_handoff_enabled(
                                    managed_context_recovery_kickstart,
                                    managed_context_density_handoff,
                                    managed_context_density_handoff_completed,
                                ) {
                                    if let Some(pressure) =
                                        managed_context_density_pressure(&snapshot)
                                    {
                                        let handoff_text =
                                            managed_context_density_handoff_text(pressure);
                                        slog(&session_log, |l| {
                                            l.info(&format!(
                                                "Managed Codex completed at density-watch pressure ({}/{} tokens); sending one-shot context handoff before waiting for follow-up",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit
                                            ))
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: live_session_id.clone(),
                                            level: "info".to_string(),
                                            source: "Intendant".to_string(),
                                            content: format!(
                                                "Managed context is above the recommended density threshold ({}/{} tokens, threshold {}). Sending a one-shot context handoff before waiting for follow-up.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                pressure.recommended_rewind_limit
                                            ),
                                            turn: None,
                                        });
                                        record_external_round_inline(
                                            &session_log,
                                            persist_model_responses_inline,
                                            round,
                                            turns_in_round,
                                        );
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: live_session_id.clone(),
                                            round,
                                            turns_in_round,
                                            native_message_count: None,
                                            project_root: round_session_root.clone(),
                                        });
                                        next_turn = Some(
                                            FollowUpMessage::text(handoff_text)
                                                .managed_context_density_handoff(),
                                        );
                                        continue 'outer;
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            if managed_context_recovery_kickstart
                                || !pending_managed_context_replays.is_empty()
                            {
                                let message = "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read; refusing to send normal follow-ups.".to_string();
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    turns_in_round,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round,
                                    native_message_count: None,
                                    project_root: round_session_root.clone(),
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unreadable".to_string());
                                break;
                            }
                        }
                        Err(e) => {
                            if managed_context_recovery_kickstart
                                || !pending_managed_context_replays.is_empty()
                            {
                                let message = format!(
                                    "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read: {}; refusing to send normal follow-ups.",
                                    e
                                );
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    turns_in_round,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round,
                                    native_message_count: None,
                                    project_root: round_session_root.clone(),
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unreadable".to_string());
                                break;
                            } else {
                                slog(&session_log, |l| {
                                    l.debug(&format!(
                                        "Could not re-read Codex context pressure after managed turn: {}",
                                        e
                                    ))
                                });
                            }
                        }
                    }
                }

                record_external_done_and_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    live_session_id.as_deref(),
                    message.as_deref(),
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::DoneSignal {
                    session_id: live_session_id.clone(),
                    message: message.clone(),
                });
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                    project_root: round_session_root.clone(),
                });
            }
            DrainOutcome::LimitRejected {
                resets_at_epoch,
                message: _,
                turn_had_started,
            } => {
                // Hand the round number back so the retried round reuses
                // it, count no round (no DoneSignal / RoundComplete —
                // those were the burn), and park instead of re-firing.
                // The incident class burned follow-up rounds at decaying
                // intervals until the budget silently exhausted, with the
                // reset time on the wire the whole time.
                round = round.saturating_sub(1);
                limit_park_streak = limit_park_streak.saturating_add(1);
                let now_epoch = crate::session_activity::epoch_seconds();
                let delay = limit_park_delay(
                    resets_at_epoch,
                    now_epoch,
                    limit_park_streak,
                    limit_park_jitter_secs(),
                );
                let park_line = limit_park_log_line(resets_at_epoch, now_epoch, true);
                slog(&session_log, |l| l.warn(&park_line));
                bus.send(AppEvent::LogEntry {
                    session_id: live_session_id.clone(),
                    level: "warn".to_string(),
                    source: "Intendant".to_string(),
                    content: park_line,
                    turn: None,
                });
                emit_external_turn_status(
                    &bus,
                    &autonomy,
                    live_session_id.as_deref(),
                    round.saturating_add(1),
                    "waiting-rate-limit",
                    format!(
                        "{} rate-limited; parked until the limit resets",
                        agent.name()
                    ),
                )
                .await;
                // Delivery-aware pending (`limit_park_pending`): when the
                // rejection arrived before the backend did anything, park
                // exactly what it rejected — the merged text (queued
                // steers were already consumed into it) with the original
                // attachments. When the turn had already started, the
                // backend holds that message; park a resume nudge instead
                // of doubling it.
                if turn_had_started {
                    let line = format!(
                        "The rejected turn had already started at {} — parking a resume nudge instead of re-sending the message",
                        agent.name()
                    );
                    slog(&session_log, |l| l.info(&line));
                    bus.send(AppEvent::LogEntry {
                        session_id: live_session_id.clone(),
                        level: "info".to_string(),
                        source: "Intendant".to_string(),
                        content: line,
                        turn: None,
                    });
                }
                // A limit-killed backend process took its background
                // children with it (confirmed-exit gated: a mere
                // rejection against a live process marks nothing) — the
                // resume nudge then carries the re-run offer.
                let died_addendum = mark_died_tasks_at_park_arm(
                    &mut agent,
                    &bus,
                    &session_log,
                    &live_session_id,
                    stats.announced_native_session_id.as_deref(),
                    RATE_LIMIT_RESTART_CAUSE,
                    turn_had_started,
                );
                let mut pending = active_followup_for_rewind_replay.clone();
                pending.text = merged.clone();
                let mut parked_pending = limit_park_pending(pending, turn_had_started);
                if let Some(addendum) = died_addendum {
                    parked_pending.text.push_str(&addendum);
                }
                limit_park = Some(LimitParkState {
                    resume_at: tokio::time::Instant::now() + delay,
                    pending: Some(parked_pending),
                    kind: ParkKind::ProviderLimit,
                });
                // Durable park marker: the in-memory park dies with the
                // daemon, and the boot auto-readopt pass needs to know a
                // dead boot's wrapper still owed its parked re-send.
                slog(&session_log, |l| {
                    l.set_limit_park(Some(crate::session_log::SessionLimitParkMeta {
                        resets_at_epoch,
                        has_pending: true,
                    }))
                });
            }
            DrainOutcome::TransientRoundDeath {
                reason,
                turns_in_round,
                turn_had_started,
            } => {
                // The round this side drove died on a temporary service
                // condition (the 2026-07-29 specimen shape: an API-500
                // round-death that rode a DoneSignal and stranded the
                // commission fake-idle). Hand the round number back,
                // count nothing, and arm the error park with the
                // delivery-aware pending — the driving message verbatim
                // when the backend never started the turn, the resume
                // nudge when it did. Past the widening schedule the
                // session ends FAILED so the outage surfaces (agenda
                // occurrences journal `failed` and count on the
                // suspension streak) instead of waiting unattended.
                error_park_streak = error_park_streak.saturating_add(1);
                if error_park_attempts_exhausted(error_park_streak) {
                    stats.rounds = round;
                    let line =
                        error_park_exhausted_line(&reason, error_park_streak.saturating_sub(1));
                    slog(&session_log, |l| l.error(&line));
                    record_external_round_inline(
                        &session_log,
                        persist_model_responses_inline,
                        round,
                        turns_in_round,
                    );
                    bus.send(AppEvent::RoundComplete {
                        session_id: live_session_id.clone(),
                        round,
                        turns_in_round,
                        native_message_count: None,
                        project_root: round_session_root.clone(),
                    });
                    bus.send(AppEvent::TaskComplete {
                        session_id: live_session_id.clone(),
                        reason: line.clone(),
                        summary: None,
                        outcome: crate::event::TaskOutcome::Failed,
                    });
                    stats.terminal_outcome = Some(line);
                    break;
                }
                round = round.saturating_sub(1);
                // A round death that took the backend process also took
                // its background children (confirmed-exit gated — an
                // API-500 against a live process marks nothing); the
                // resume nudge then carries the re-run offer.
                let died_addendum = mark_died_tasks_at_park_arm(
                    &mut agent,
                    &bus,
                    &session_log,
                    &live_session_id,
                    stats.announced_native_session_id.as_deref(),
                    SERVICE_RECOVERY_RESTART_CAUSE,
                    turn_had_started,
                );
                let mut pending = active_followup_for_rewind_replay.clone();
                pending.text = merged.clone();
                let (mut park, park_line) = transient_round_death_error_park(
                    &reason,
                    tokio::time::Instant::now(),
                    error_park_streak,
                    error_park_jitter_secs(),
                    turn_had_started,
                    Some(pending),
                );
                if let (Some(pending), Some(addendum)) = (park.pending.as_mut(), died_addendum) {
                    pending.text.push_str(&addendum);
                }
                let has_pending = park.pending.is_some();
                slog(&session_log, |l| l.warn(&park_line));
                bus.send(AppEvent::LogEntry {
                    session_id: live_session_id.clone(),
                    level: "warn".to_string(),
                    source: "Intendant".to_string(),
                    content: park_line,
                    turn: None,
                });
                emit_external_turn_status(
                    &bus,
                    &autonomy,
                    live_session_id.as_deref(),
                    round.saturating_add(1),
                    "waiting-service-recovery",
                    format!(
                        "{} waiting out a temporary service condition; parked for recovery",
                        agent.name()
                    ),
                )
                .await;
                limit_park = Some(park);
                // Durable park marker, like the limit arm — the boot
                // auto-readopt pass must see a dead boot's wrapper still
                // owed its parked continuation (no reset clock: the
                // widening schedule is the wrapper's own).
                slog(&session_log, |l| {
                    l.set_limit_park(Some(crate::session_log::SessionLimitParkMeta {
                        resets_at_epoch: None,
                        has_pending,
                    }))
                });
            }
            DrainOutcome::ContextRewindRequested {
                request,
                message,
                turns_in_round,
                turn_stop_status,
            } => {
                managed_context_recovery_kickstarts_without_rewind = 0;
                managed_context_density_block_handoffs_without_relief = 0;
                stats.rounds = round;
                match apply_external_context_rewind(
                    &mut agent,
                    &thread.thread_id,
                    &request,
                    &drain_config,
                )
                .await
                {
                    Ok(automatic_resume) => {
                        if let Some(mut continuation) = managed_context_rewind_continuation(
                            &mut pending_managed_context_replays,
                            &active_followup_for_rewind_replay,
                            automatic_resume,
                            &turn_stop_status,
                        ) {
                            if managed_context_density_handoff {
                                continuation = continuation.after_managed_context_density_handoff();
                            }
                            slog(&session_log, |l| {
                                l.info(
                                    "Managed-context rewind succeeded; continuing queued follow-up",
                                )
                            });
                            next_turn = Some(continuation);
                            continue 'outer;
                        }
                        record_external_done_and_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            live_session_id.as_deref(),
                            message.as_deref(),
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::DoneSignal {
                            session_id: live_session_id.clone(),
                            message: message.clone(),
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                            project_root: round_session_root.clone(),
                        });
                    }
                    Err(message) => {
                        emit_context_rewind_failure(&request, message, &drain_config);
                        record_external_done_and_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            live_session_id.as_deref(),
                            None,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::DoneSignal {
                            session_id: live_session_id.clone(),
                            message: None,
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                            project_root: round_session_root.clone(),
                        });
                    }
                }
            }
            DrainOutcome::RecoveryRequired {
                message,
                recovery_hint,
                turns_in_round,
            } => {
                stats.rounds = round;
                if codex_managed_context_enabled {
                    managed_context_recovery_kickstarts_without_rewind =
                        managed_context_recovery_kickstarts_without_rewind.saturating_add(1);
                    if managed_context_recovery_kickstarts_without_rewind
                        < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                    {
                        let pressure = match refresh_external_context_usage_snapshot(
                            &mut agent,
                            &drain_config,
                        )
                        .await
                        {
                            Ok(Some(snapshot)) => managed_context_recovery_pressure(&snapshot),
                            Ok(None) => None,
                            Err(e) => {
                                slog(&session_log, |l| {
                                    l.debug(&format!(
                                        "Could not read Codex context snapshot after recovery-required outcome: {}",
                                        e
                                    ))
                                });
                                None
                            }
                        };
                        let recovery_text = pressure
                            .map(|pressure| {
                                managed_context_recovery_kickstart_text(pressure, false)
                            })
                            .unwrap_or_else(|| {
                                managed_context_backend_recovery_kickstart_text(
                                    &message,
                                    recovery_hint.as_deref(),
                                )
                            });
                        slog(&session_log, |l| {
                            l.warn("Managed Codex reported recovery required; sending managed-context recovery kickstart instead of ending the session")
                        });
                        record_external_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                            project_root: round_session_root.clone(),
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "warn".to_string(),
                            source: "Intendant".to_string(),
                            content: "Managed Codex reported recovery required; sending a managed-context rewind kickstart instead of ending the session.".to_string(),
                            turn: None,
                        });
                        next_turn = Some(
                            FollowUpMessage::text(recovery_text)
                                .managed_context_recovery_kickstart(),
                        );
                        continue 'outer;
                    } else {
                        // Backstop: the model kept reporting recovery required
                        // without rewinding (step-limit exhaustion ends those
                        // turns); perform a surgical rewind before giving up.
                        let mut surgical_failure = None;
                        if managed_context_surgical_recovery_available(
                            managed_context_surgical_recoveries,
                        ) {
                            match attempt_supervisor_surgical_context_rewind(
                                &mut agent,
                                &thread.thread_id,
                                &drain_config,
                                surgical_task_statement.as_deref(),
                                &mut pending_managed_context_replays,
                            )
                            .await
                            {
                                Ok(continuation) => {
                                    managed_context_surgical_recoveries =
                                        managed_context_surgical_recoveries.saturating_add(1);
                                    managed_context_recovery_kickstarts_without_rewind = 0;
                                    let content = format!(
                                        "Managed Codex kept reporting backend recovery required after {} kickstarts without a rewind; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                        MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                        managed_context_surgical_recoveries,
                                        MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                    );
                                    slog(&session_log, |l| l.warn(&content));
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content,
                                        turn: None,
                                    });
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                        project_root: round_session_root.clone(),
                                    });
                                    next_turn = Some(continuation);
                                    continue 'outer;
                                }
                                Err(e) => surgical_failure = Some(e),
                            }
                        }
                        let mut failure = format!(
                            "Managed Codex still reports backend recovery required after {} recovery kickstarts without another successful rewind; refusing to mark the session complete.",
                            managed_context_recovery_kickstarts_without_rewind
                        );
                        match surgical_failure {
                            Some(surgical) => failure.push_str(&format!(
                                " Supervisor surgical rewind also failed: {surgical}"
                            )),
                            None => failure.push_str(&format!(
                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                            )),
                        }
                        slog(&session_log, |l| l.warn(&failure));
                        record_external_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                            project_root: round_session_root.clone(),
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "error".to_string(),
                            source: "Intendant".to_string(),
                            content: failure.clone(),
                            turn: None,
                        });
                        bus.send(AppEvent::LoopError(failure));
                        stats.terminal_outcome =
                            Some("managed Codex recovery required".to_string());
                        break;
                    }
                }
                slog(&session_log, |l| {
                    l.warn(&recovery_required_message(
                        &message,
                        recovery_hint.as_deref(),
                    ))
                });
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                    project_root: round_session_root.clone(),
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: "recovery required".to_string(),
                    summary: recovery_hint.or(Some(message)),
                    outcome: crate::event::TaskOutcome::Failed,
                });
                stats.terminal_outcome = Some("recovery required".to_string());
                break;
            }
            DrainOutcome::Interrupted { reason } => {
                // Emit RoundComplete so the dashboard updates and log the
                // interrupt. For a *user-requested* interrupt the round ends
                // here and the loop waits for the next follow-up. When the
                // managed-context density tool gate generated the interrupt,
                // there may be no user at all (headless `--task-file` runs),
                // so the supervisor must continue the loop itself with the
                // density maintenance handoff (managed.md: density gating
                // inserts a maintenance handoff) or a recovery kickstart if
                // pressure escalated past the rewind-only threshold. A
                // credential-reload interrupt arms the safe-point respawn's
                // synthesized continuation instead — the reload wants the
                // interrupted work to continue on the fresh account.
                stats.rounds = round;
                if reason == RELOAD_CREDENTIALS_INTERRUPT_REASON {
                    reload_interrupted_turn = true;
                }
                slog(&session_log, |l| {
                    l.info(&format!("External agent interrupted: {}", reason))
                });
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    stats.turns,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round: stats.turns,
                    native_message_count: None,
                    project_root: round_session_root.clone(),
                });
                if codex_managed_context_enabled
                    && reason == MANAGED_CONTEXT_DENSITY_BLOCK_INTERRUPT_REASON
                {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                managed_context_recovery_kickstarts_without_rewind =
                                    managed_context_recovery_kickstarts_without_rewind
                                        .saturating_add(1);
                                if managed_context_recovery_kickstarts_without_rewind
                                    < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                {
                                    let held_user_input =
                                        !pending_managed_context_replays.is_empty();
                                    let recovery_text = managed_context_recovery_kickstart_text(
                                        pressure,
                                        held_user_input,
                                    );
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Managed-context density tool gate interrupted the turn while pressure escalated to rewind-only ({}/{} tokens); sending recovery kickstart",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ))
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(recovery_text)
                                            .managed_context_recovery_kickstart(),
                                    );
                                    continue 'outer;
                                }
                                // Backstop: surgical rewind before giving up
                                // (same exhaustion as the TurnCompleted arm,
                                // reached via the density-gate interrupt).
                                let mut surgical_failure = None;
                                if managed_context_surgical_recovery_available(
                                    managed_context_surgical_recoveries,
                                ) {
                                    match attempt_supervisor_surgical_context_rewind(
                                        &mut agent,
                                        &thread.thread_id,
                                        &drain_config,
                                        surgical_task_statement.as_deref(),
                                        &mut pending_managed_context_replays,
                                    )
                                    .await
                                    {
                                        Ok(continuation) => {
                                            managed_context_surgical_recoveries =
                                                managed_context_surgical_recoveries
                                                    .saturating_add(1);
                                            managed_context_recovery_kickstarts_without_rewind = 0;
                                            let content = format!(
                                                "Managed-context recovery exhausted its kickstart budget at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                managed_context_surgical_recoveries,
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                            );
                                            slog(&session_log, |l| l.warn(&content));
                                            bus.send(AppEvent::LogEntry {
                                                session_id: live_session_id.clone(),
                                                level: "warn".to_string(),
                                                source: "Intendant".to_string(),
                                                content,
                                                turn: None,
                                            });
                                            next_turn = Some(continuation);
                                            continue 'outer;
                                        }
                                        Err(e) => surgical_failure = Some(e),
                                    }
                                }
                                let mut message = format!(
                                    "Managed-context density tool gate kept interrupting while context stayed above the rewind-only threshold ({}/{} tokens); refusing to continue without a rewind.",
                                    pressure.used_tokens, pressure.rewind_only_limit
                                );
                                match surgical_failure {
                                    Some(failure) => message.push_str(&format!(
                                        " Supervisor surgical rewind also failed: {failure}"
                                    )),
                                    None => message.push_str(&format!(
                                        " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                        MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                                    )),
                                }
                                slog(&session_log, |l| l.warn(&message));
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unresolved".to_string());
                                break;
                            }
                            if let Some(pressure) = managed_context_density_pressure(&snapshot) {
                                managed_context_density_block_handoffs_without_relief =
                                    managed_context_density_block_handoffs_without_relief
                                        .saturating_add(1);
                                if managed_context_density_block_handoffs_without_relief
                                    < MANAGED_CONTEXT_DENSITY_BLOCK_MAX_HANDOFFS_WITHOUT_RELIEF
                                {
                                    let handoff_text =
                                        managed_context_density_handoff_text(pressure);
                                    slog(&session_log, |l| {
                                        l.info(&format!(
                                            "Managed-context density tool gate interrupted the turn ({}/{} tokens, threshold {}); sending density maintenance handoff",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit,
                                            pressure.recommended_rewind_limit
                                        ))
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(handoff_text)
                                            .managed_context_density_handoff(),
                                    );
                                    continue 'outer;
                                }
                                let message = format!(
                                    "Managed-context density maintenance did not converge after {} handoffs ({}/{} tokens, threshold {}); refusing to ping-pong until the task timeout.",
                                    managed_context_density_block_handoffs_without_relief,
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                );
                                slog(&session_log, |l| l.warn(&message));
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome = Some(
                                    "managed Codex density maintenance unresolved".to_string(),
                                );
                                break;
                            }
                            // Pressure dropped below the density threshold
                            // between the block and this re-read (a fresher
                            // backend report landed); the steer is stale —
                            // resume the interrupted task.
                            managed_context_density_block_handoffs_without_relief = 0;
                            slog(&session_log, |l| {
                                l.info(
                                    "Managed-context density tool gate interrupted the turn, but a fresher backend report is below the density threshold; resuming the task",
                                )
                            });
                            next_turn = Some(FollowUpMessage::text(
                                "The previous turn was interrupted by a managed-context density gate, but the latest backend report now shows context pressure below the recommended density threshold, so that steer is stale. Continue the task from where it was interrupted."
                                    .to_string(),
                            ));
                            continue 'outer;
                        }
                        Ok(None) => {
                            slog(&session_log, |l| {
                                l.warn(
                                    "Managed-context density tool gate interrupted the turn, but no backend context report is available; waiting for a follow-up",
                                )
                            });
                        }
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Managed-context density tool gate interrupted the turn, but context pressure could not be re-read: {}; waiting for a follow-up",
                                    e
                                ))
                            });
                        }
                    }
                }
            }
            DrainOutcome::Terminated { reason, exit_code } => {
                stats.rounds = round;
                let user_requested_stop =
                    matches!(reason.as_str(), "stopped by user" | "restarting session")
                        || reason == crate::session_supervisor::CLAUDE_EDIT_INPLACE_STOP_REASON
                        || reason == crate::session_supervisor::CLAUDE_EDIT_SUPERSEDED_STOP_REASON;
                if codex_managed_context_enabled && !user_requested_stop {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                let message = format!(
                                    "Managed Codex terminated as {reason} while backend-reported pressure remains {}/{} tokens; refusing to mark the session complete.",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit
                                );
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    stats.turns,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round: stats.turns,
                                    native_message_count: None,
                                    project_root: round_session_root.clone(),
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome = Some(
                                    "managed Codex terminated under context pressure".to_string(),
                                );
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not re-read Codex context pressure after managed termination: {}",
                                    e
                                ))
                            });
                        }
                    }
                }
                slog(&session_log, |l| {
                    l.info(&format!(
                        "External agent terminated: {} (exit code: {:?})",
                        reason, exit_code
                    ));
                });
                // Input owed to the dead lane surfaces with the named
                // reason instead of waiting forever on a model that no
                // longer exists (the zombie-turn class).
                surface_undelivered_input_at_terminal(
                    &bus,
                    &live_session_id,
                    &mut pending_runtime_steers,
                    &mut parked_follow_ups,
                    &undelivered_detail_for_terminal(&reason),
                );
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: reason.clone(),
                    summary: stats.last_response.clone(),
                    outcome: crate::event::TaskOutcome::Failed,
                });
                stats.terminal_outcome = Some(reason);
                break;
            }
            DrainOutcome::ChannelClosed => {
                slog(&session_log, |l| {
                    l.info("External agent event channel closed")
                });
                surface_undelivered_input_at_terminal(
                    &bus,
                    &live_session_id,
                    &mut pending_runtime_steers,
                    &mut parked_follow_ups,
                    &undelivered_detail_for_terminal("external agent event channel closed"),
                );
                stats.terminal_outcome = Some("external agent event channel closed".to_string());
                break;
            }
        }
    }

    // Park-then-die reconciliation, the exit side: the loop is ending
    // while a park is still ARMED (a deliberate stop, session removal,
    // safeguards terminal, recovery-required, …). Consume the park
    // honestly — surface the owed message as undelivered and clear the
    // durable marker so the boot sweep never resurrects a deliberately
    // ended session — and say so in the terminal outcome, so the summary
    // never reads clean over stranded work. The one exception is the
    // daemon-teardown exit (event bus closed): the daemon is dying, not
    // this session's story — the marker survives as the boot
    // auto-readopt trace, exactly like a hard daemon death that never
    // reaches this line at all.
    if let Some(park) = limit_park.take() {
        let noun = park.kind.noun();
        let detail = stats
            .terminal_outcome
            .clone()
            .unwrap_or_else(|| "the session ended".to_string());
        let had_pending = park.pending.is_some();
        if daemon_teardown_exit {
            // The marker (and its owed pending) survives for the boot
            // readopt pass; the meta write stays untouched and session_end
            // refuses the clean completion over it.
            slog(&session_log, |l| {
                l.warn(&format!(
                    "{noun} still armed at the daemon-teardown exit — the durable marker \
                     survives for the boot readopt pass"
                ))
            });
            if had_pending {
                stats.terminal_outcome = Some(format!(
                    "{detail}; a {} with pending work was still armed — its durable \
                     marker survives for the boot readopt pass",
                    noun.to_lowercase()
                ));
            }
        } else {
            if let Some(pending) = park.pending {
                emit_follow_up_status(
                    &bus,
                    live_session_id.as_deref(),
                    &pending.follow_up_id,
                    Some(&pending.text),
                    "failed",
                    Some(&format!(
                        "{noun} was still armed when the session ended ({detail})"
                    )),
                );
                stats.terminal_outcome = Some(format!(
                    "{detail}; a {} with pending work was still armed at session end \
                     (its owed message was surfaced as undelivered)",
                    noun.to_lowercase()
                ));
            }
            slog(&session_log, |l| l.set_limit_park(None));
            slog(&session_log, |l| {
                l.info(&format!("{noun} cancelled — {detail}"))
            });
        }
    }

    // The wakeup twin of the park backstop above: the loop is ending with
    // the session's native scheduled wakeup still on record — the
    // in-memory registry entry is about to lose the only lane that could
    // deliver it. Flip the durable marker to its died form (the honest
    // lost-timer statement: nothing will deliver it, and re-arming is the
    // resumed session's decision) — except at the daemon-teardown exit,
    // where the marker survives untouched as the boot pass's trace,
    // exactly like the limit park's.
    if let Some(record) = stats
        .announced_native_session_id
        .as_deref()
        .and_then(crate::native_wakeup::consume)
    {
        let now_epoch = crate::session_activity::epoch_seconds();
        let due = crate::native_wakeup::due_phrase(record.fire_at_epoch, now_epoch);
        let detail = stats
            .terminal_outcome
            .clone()
            .unwrap_or_else(|| "the session ended".to_string());
        if daemon_teardown_exit {
            slog(&session_log, |l| {
                l.warn(&format!(
                    "A native scheduled wakeup ({due}) is still pending at the \
                     daemon-teardown exit — the durable marker survives for the boot pass"
                ))
            });
        } else {
            slog(&session_log, |l| {
                l.warn(&format!(
                    "⚠ The session's native scheduled wakeup ({due}) was lost with the \
                     session end ({detail}) — nothing will deliver it"
                ))
            });
            let mut died = record.to_meta();
            died.died_cause = Some("the session end".to_string());
            died.died_at_epoch = Some(now_epoch);
            slog(&session_log, |l| l.set_native_wakeup(Some(died)));
        }
    }

    // The supervised span is over: reconcile sub-agent children that never
    // reported a terminal (their processes die with the backend). The
    // reader's own EOF sweep cannot cover this — `shutdown()` below aborts
    // the reader task before it can see EOF — and a child resumed by the
    // next wrapper re-arms through its own stream.
    sweep_stranded_external_subagents(&drain_config, &mut stats);

    if let Err(e) = agent.shutdown().await {
        slog(&session_log, |l| {
            l.warn(&format!("Agent shutdown error: {}", e))
        });
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delivered wake is honest about its mechanism and carries the
    /// model's own prompt — plus the re-arm reminder, since the harness
    /// timer the model thinks it holds is gone. Empty prompts are said,
    /// not omitted; an unflipped record names the generic restart.
    #[test]
    fn native_wakeup_delivery_message_is_honest_and_carries_the_prompt() {
        let record = crate::native_wakeup::NativeWakeupRecord {
            armed_at_epoch: 100,
            fire_at_epoch: 880,
            prompt: "<<autonomous-loop-dynamic>>".into(),
            reason: Some("watching the queue".into()),
            tool_use_id: "toolu_wk".into(),
            rearmed_cause: Some(CREDENTIAL_RELOAD_RESTART_CAUSE.into()),
        };
        let message = native_wakeup_delivery_message(&record, 900);
        assert!(
            message.text.contains("Scheduled wakeup"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains(CREDENTIAL_RELOAD_RESTART_CAUSE),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("<<autonomous-loop-dynamic>>"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("watching the queue"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("re-arm your next wakeup"),
            "{}",
            message.text
        );
        assert!(
            message.steer_id.is_none() && message.follow_up_id.is_none(),
            "no id-keyed cancel matching can drop the wake"
        );

        let bare = crate::native_wakeup::NativeWakeupRecord {
            prompt: String::new(),
            reason: None,
            rearmed_cause: None,
            ..record
        };
        let message = native_wakeup_delivery_message(&bare, 900);
        assert!(
            message.text.contains("(the arm carried no prompt)"),
            "{}",
            message.text
        );
        assert!(
            message.text.contains("a backend restart"),
            "{}",
            message.text
        );
    }

    /// The deadline arm's instant: a future due time sleeps toward it, a
    /// past one fires immediately, and the disabled branch type-checks
    /// on "now" (the limit-park arm's exact pattern).
    #[test]
    fn native_wakeup_deadline_maps_due_times_to_instants() {
        let now_epoch = crate::session_activity::epoch_seconds();
        let record = |fire_at: u64| {
            Some(crate::native_wakeup::NativeWakeupRecord {
                armed_at_epoch: now_epoch,
                fire_at_epoch: fire_at,
                prompt: "x".into(),
                reason: None,
                tool_use_id: "t".into(),
                rearmed_cause: None,
            })
        };
        let now = tokio::time::Instant::now();
        let future = native_wakeup_deadline(&record(now_epoch + 600));
        let remaining = future.duration_since(now);
        assert!(
            remaining >= std::time::Duration::from_secs(590)
                && remaining <= std::time::Duration::from_secs(610),
            "future due time sleeps toward it ({remaining:?})"
        );
        assert!(
            native_wakeup_deadline(&record(now_epoch.saturating_sub(600))).duration_since(now)
                < std::time::Duration::from_secs(2),
            "past due time fires immediately"
        );
        assert!(
            native_wakeup_deadline(&None).duration_since(now) < std::time::Duration::from_secs(2),
            "disabled branch reads as now"
        );
    }

    #[test]
    fn idle_cwd_announcement_is_housekeeping_for_the_primary_session() {
        let session_id = Some("session-main".to_string());
        let alias_session_id = Some("wrapper-main".to_string());

        assert!(matches!(
            idle_external_cwd_event(
                &None,
                &session_id,
                &alias_session_id,
                "/repo".to_string(),
            ),
            Some(AppEvent::SessionCwdAnnounced {
                session_id: Some(id),
                cwd,
            }) if id == "session-main" && cwd == "/repo"
        ));
        assert!(idle_external_cwd_event(
            &Some("wrapper-main".to_string()),
            &session_id,
            &alias_session_id,
            "/repo".to_string(),
        )
        .is_some());
        assert!(idle_external_cwd_event(
            &Some("side-thread".to_string()),
            &session_id,
            &alias_session_id,
            "/other".to_string(),
        )
        .is_none());
        assert!(
            idle_external_cwd_event(&None, &session_id, &alias_session_id, "  ".to_string(),)
                .is_none()
        );
    }

    #[test]
    fn midturn_reload_pushes_continuation() {
        // The drain interrupted a live turn with the reload's own reason:
        // the safe-point respawn synthesizes a continuation and fronts
        // the queue with it, ahead of everything queued behind the
        // dropped turn.
        let continuation = synthesized_reload_continuation(true)
            .expect("a reload that cut a live turn synthesizes a continuation");
        assert_eq!(continuation.text, RELOAD_MIDTURN_CONTINUATION_TEXT);
        assert!(continuation.follow_up_id.is_none());
        assert!(continuation.steer_id.is_none());

        let mut parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
            std::collections::VecDeque::new();
        parked_follow_ups.push_back(FollowUpMessage::text("queued while running".to_string()));
        parked_follow_ups.push_front(continuation);
        assert_eq!(parked_follow_ups.len(), 2);
        assert_eq!(
            parked_follow_ups.front().map(|m| m.text.as_str()),
            Some(RELOAD_MIDTURN_CONTINUATION_TEXT)
        );

        // The discriminator is the exact reason the drain mints when the
        // reload itself cut the turn (external_events.rs) — the string
        // live-log greps key on.
        assert_eq!(RELOAD_CREDENTIALS_INTERRUPT_REASON, "reloading credentials");
    }

    #[test]
    fn reload_mirrors_park_pending_preservation() {
        // Both reload lanes preserve interrupted work the same way: the
        // rate-limit park's pending re-send and the mid-turn interrupt's
        // synthesized continuation each ride the FRONT of
        // `parked_follow_ups` and deliver through the same flush, ahead
        // of messages queued behind them.
        let mut cancelled = HashSet::new();

        // Park lane: `apply_backend_credentials_reload` push_fronts
        // `park.pending` when cancelling the park.
        let mut parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
            std::collections::VecDeque::new();
        parked_follow_ups.push_back(FollowUpMessage::text("queued during park".to_string()));
        let pending = FollowUpMessage::text("pending re-send".to_string())
            .with_follow_up_id(Some("f-pending".to_string()));
        parked_follow_ups.push_front(pending);
        let (first, skipped) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(skipped, 0);
        assert_eq!(first.map(|m| m.text), Some("pending re-send".to_string()));

        // Mid-turn reload lane: the synthesized continuation takes the
        // same front-of-queue slot through the same flush.
        let mut parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
            std::collections::VecDeque::new();
        parked_follow_ups.push_back(FollowUpMessage::text("queued mid-turn".to_string()));
        parked_follow_ups.push_front(
            synthesized_reload_continuation(true).expect("mid-turn interrupt continuation"),
        );
        let (first, skipped) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(skipped, 0);
        assert_eq!(
            first.map(|m| m.text),
            Some(RELOAD_MIDTURN_CONTINUATION_TEXT.to_string())
        );
        assert_eq!(
            parked_follow_ups.front().map(|m| m.text.as_str()),
            Some("queued mid-turn")
        );
    }

    #[test]
    fn idle_sessions_get_no_synthesized_message() {
        // Idle and between-turn reloads never arm the marker (the idle
        // listener applies the respawn directly; no drain returned
        // `Interrupted` with the reload reason): idle in, idle out.
        assert!(synthesized_reload_continuation(false).is_none());

        // Adjacent interrupt reasons must not collide with the
        // discriminator: a user's stop keeps stop semantics even when a
        // reload rides the same drain, and the density gate keeps its
        // own self-continuation lane.
        for reason in [
            "user requested",
            "user requested (backend reported no turn end)",
            MANAGED_CONTEXT_DENSITY_BLOCK_INTERRUPT_REASON,
        ] {
            assert_ne!(reason, RELOAD_CREDENTIALS_INTERRUPT_REASON);
        }
    }

    #[test]
    fn continuation_rides_existing_flush() {
        // The continuation is delivered by the loop's existing
        // post-respawn flush (`next_parked_follow_up`), first out, and —
        // carrying no steer/follow-up id — it can never be dropped by
        // id-keyed cancel matching, even with unrelated cancels pending.
        let mut parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
            std::collections::VecDeque::new();
        parked_follow_ups.push_back(
            FollowUpMessage::text("user message".to_string())
                .with_follow_up_id(Some("f-2".to_string())),
        );
        parked_follow_ups.push_front(
            synthesized_reload_continuation(true).expect("mid-turn interrupt continuation"),
        );

        let mut cancelled: HashSet<String> = HashSet::from(["f-9".to_string()]);
        assert!(!follow_up_message_was_cancelled(
            &mut cancelled,
            parked_follow_ups
                .front()
                .expect("front is the continuation"),
        ));

        let (first, skipped) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(skipped, 0);
        assert_eq!(
            first.map(|m| m.text),
            Some(RELOAD_MIDTURN_CONTINUATION_TEXT.to_string())
        );

        let (second, skipped) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(skipped, 0);
        assert_eq!(second.map(|m| m.text), Some("user message".to_string()));
        assert!(parked_follow_ups.is_empty());

        // A cancel that names the queued user message still lands while
        // the continuation flushes first: rebuild the queue and cancel
        // "f-2" — the flush skips it and delivers the continuation.
        let mut parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
            std::collections::VecDeque::new();
        parked_follow_ups.push_back(
            FollowUpMessage::text("user message".to_string())
                .with_follow_up_id(Some("f-2".to_string())),
        );
        parked_follow_ups.push_front(
            synthesized_reload_continuation(true).expect("mid-turn interrupt continuation"),
        );
        let mut cancelled: HashSet<String> = HashSet::from(["f-2".to_string()]);
        let (first, skipped) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(skipped, 0);
        assert_eq!(
            first.map(|m| m.text),
            Some(RELOAD_MIDTURN_CONTINUATION_TEXT.to_string())
        );
        let (second, skipped) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(skipped, 1);
        assert!(second.is_none());
    }
}
