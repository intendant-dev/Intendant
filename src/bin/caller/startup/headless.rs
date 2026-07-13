//! The foreground execution shape: run_headless_mode is main()'s final
//! else arm — no terminal UI; the web gateway (on by default) serves the
//! dashboard for the primary session, with presence mediation when
//! enabled and the daemon-loop fallthrough when the gateway stays up.
//! `--json` swaps the dashboard for JSONL stdio; `--no-web --json` is
//! the scripted headless shape, `--no-web` alone runs a single round.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_headless_mode(
    flags: &CliFlags,
    mut project: Project,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    task: Option<String>,
    provider: Option<Box<dyn provider::ChatProvider>>,
    use_web: bool,
    web_port: u16,
    web_bind_ip: Option<std::net::IpAddr>,
    web_port_for_agent: Option<u16>,
    mut web_listener: Option<tokio::net::TcpListener>,
    web_tls_client_cert_required: bool,
    web_tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    runtime_presence_enabled: bool,
    initial_agent_backend: Option<external_agent::AgentBackend>,
    shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_registry: display::SharedSessionRegistry,
    recording_registry: Arc<tokio::sync::RwLock<recording::RecordingRegistry>>,
    startup_external_resume_session: Option<String>,
) -> Result<(), CallerError> {
    // This mode always has a task (enforced by task resolution in main).
    let task = task.unwrap();

    let bus = EventBus::new();
    let _session_listeners = startup::wiring::spawn_session_listeners(
        &bus,
        &recording_registry,
        &session_registry,
        &frame_registry,
    );
    // askHuman file → bus events, so the dashboard (and MCP-over-HTTP)
    // can surface agent questions for the foreground session.
    let _human_monitor = event::spawn_human_question_monitor(
        bus.clone(),
        event::shared_question_path(log_dir.join("human_question")),
    );
    start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
    let _debug_handler = startup::wiring::spawn_debug_handler(&bus, &project, web_port, use_web);

    // Outbound broadcast channel — shared by the web gateway, the JSON
    // stdout subscriber, and (when enabled) the control socket, which
    // brings its own broadcast channel that everything else reuses.
    let mut _control_handle: Option<tokio::task::JoinHandle<()>> = None;
    let outbound_tx = if flags.control_socket {
        let (handle, control_tx) = control::spawn_control_server(bus.clone());
        _control_handle = Some(handle);
        eprintln!("Control socket: {}", control::socket_path().display());
        control_tx
    } else {
        let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
        tx
    };

    // Wire outbound broadcaster: converts AppEvents to OutboundEvents
    let _outbound_broadcaster =
        event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());

    let _log_sinks = startup::wiring::spawn_log_sinks(&bus, &session_log);

    let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
        startup::wiring::start_project_file_watcher(Some(&project.root), &log_dir, &bus);

    // JSON stdout subscriber: prints OutboundEvents as JSONL to stdout
    if flags.json_output {
        let mut json_rx = outbound_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match json_rx.recv().await {
                    Ok(line) => {
                        println!("{}", line);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Foreground-session state shared with the dashboard. The TUI App
    // used to own these; they are plain shared objects.
    let approval_registry = event::ApprovalRegistry::default();
    let context_injection = event::ContextInjectionQueue::default();
    let agent_state = Arc::new(std::sync::Mutex::new(
        presence::AgentStateSnapshot::default(),
    ));
    let presence_session = Arc::new(Mutex::new(presence::PresenceSession::new(
        session_log_id(&session_log).unwrap_or_default(),
    )));

    // Presence rides only under the web gateway: its narration, chat
    // mediation, and voice handoff all surface in the dashboard.
    // Terminal/JSON runs dispatch directly, as headless always did.
    // Note: --direct only forces single-agent mode for the worker; it
    // does NOT disable presence. Use --no-presence to disable presence.
    let use_presence = use_web && !flags.no_presence && project.config.presence.enabled;

    // Shared paused ref-count: incremented by PresenceConnected, decremented
    // by PresenceDisconnected. Server-side presence is paused when count > 0
    // (any browser has active voice).
    let presence_paused = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // If presence is active, create the user ↔ presence channels and spawn
    // the bus → presence pump: agent-state snapshot updates, the pause
    // ref-count, event filtering + presence-session recording, and
    // forwarding — the job `App::forward_to_presence` did when the TUI
    // owned the bus subscription.
    let (presence_user_rx, presence_event_rx_for_task, presence_tx_for_dispatch) = if use_presence {
        let (presence_tx, presence_user_rx) = tokio::sync::mpsc::channel::<String>(4);
        let (presence_event_tx, presence_event_rx) =
            tokio::sync::mpsc::channel::<presence::PresenceEvent>(64);
        let agent_state = agent_state.clone();
        let presence_paused = presence_paused.clone();
        let presence_session = presence_session.clone();
        let mut pump_rx = bus.subscribe();
        tokio::spawn(async move {
            let mut last_presence_phase = String::new();
            loop {
                let event = match pump_rx.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                // Update the agent state snapshot (sees ALL events)
                presence::update_agent_state(&event, &agent_state);
                match &event {
                    AppEvent::PresenceConnected { .. } => {
                        presence_paused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    AppEvent::PresenceDisconnected => {
                        let _ = presence_paused.fetch_update(
                            std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,
                            |v| Some(v.saturating_sub(1)),
                        );
                    }
                    _ => {}
                }
                // Filter and forward push-worthy events to presence
                if let Some(pe) = presence::filter_event(&event, &mut last_presence_phase) {
                    // Record into the presence session event window
                    // (for browser replay)
                    if let Ok(mut session) = presence_session.lock() {
                        session.record_event(pe.clone());
                    }
                    let _ = presence_event_tx.try_send(pe);
                }
            }
        });
        (
            Some(presence_user_rx),
            Some(presence_event_rx),
            Some(presence_tx),
        )
    } else {
        (None, None, None)
    };

    // Task dispatch channel: browser tool calls / dashboard StartTask →
    // presence task loop. Only created when presence is enabled, because
    // the channel is consumed by `run_with_presence`. In non-presence
    // mode, leaving `task_tx` as None makes the dispatcher route to
    // `follow_up_tx` instead, which is consumed by
    // `run_external_agent_mode` / `run_direct_mode`.
    let (task_tx, task_rx) = if use_presence {
        let (tx, rx) = tokio::sync::mpsc::channel::<presence::TaskEnvelope>(4);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Follow-up channel. Under the web gateway the dispatcher owns a
    // sender for the whole run (dashboard follow-ups continue this
    // session); in JSON mode the stdin reader owns one; a plain
    // terminal run holds none, so recv() returns None → single round.
    let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(4);
    let json_approval_slot = if flags.json_output {
        Some(new_json_approval_slot())
    } else {
        None
    };
    if flags.json_output {
        // JSON mode: read follow-up lines and control commands from stdin
        let follow_up_tx = follow_up_tx.clone();
        let approval_slot = json_approval_slot.clone().unwrap();
        let log_dir_for_stdin = log_dir.clone();
        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let reader = tokio::io::BufReader::new(stdin);
            use tokio::io::AsyncBufReadExt;
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                // Try to parse as a JSON control command
                if line.starts_with('{') {
                    if let Ok(msg) = serde_json::from_str::<event::ControlMsg>(&line) {
                        match msg {
                            event::ControlMsg::Approve { .. } => {
                                let mut guard = approval_slot.lock().unwrap();
                                if let Some((_id, tx)) = guard.take() {
                                    let _ = tx.send(event::ApprovalResponse::Approve);
                                }
                            }
                            event::ControlMsg::Deny { .. } => {
                                let mut guard = approval_slot.lock().unwrap();
                                if let Some((_id, tx)) = guard.take() {
                                    let _ = tx.send(event::ApprovalResponse::Deny);
                                }
                            }
                            event::ControlMsg::Skip { .. } => {
                                let mut guard = approval_slot.lock().unwrap();
                                if let Some((_id, tx)) = guard.take() {
                                    let _ = tx.send(event::ApprovalResponse::Skip);
                                }
                            }
                            event::ControlMsg::ApproveAll { .. } => {
                                let mut guard = approval_slot.lock().unwrap();
                                if let Some((_id, tx)) = guard.take() {
                                    let _ = tx.send(event::ApprovalResponse::ApproveAll);
                                }
                            }
                            event::ControlMsg::AnswerQuestion { answers, .. } => {
                                // Structured question prompts share the
                                // approval slot in JSON mode.
                                let mut guard = approval_slot.lock().unwrap();
                                if let Some((_id, tx)) = guard.take() {
                                    let _ = tx.send(event::ApprovalResponse::Answer { answers });
                                }
                            }
                            event::ControlMsg::Input { text } => {
                                // Write human_response file for askHuman IPC.
                                // The agent polls for this file; a swallowed
                                // failure leaves it waiting forever.
                                let resp_path = log_dir_for_stdin.join("human_response");
                                if let Err(e) = std::fs::write(&resp_path, text.as_bytes()) {
                                    eprintln!(
                                        "Failed to write askHuman response {}: {}",
                                        resp_path.display(),
                                        e
                                    );
                                }
                            }
                            event::ControlMsg::FollowUp {
                                text, direct: _, ..
                            } => {
                                // This stdin handler only exists in
                                // the headless `--json` path where
                                // there's no presence layer, so the
                                // direct bit is implicitly always on.
                                if follow_up_tx
                                    .send(FollowUpMessage::text(text))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            _ => {
                                // Unknown command — ignore
                            }
                        }
                        continue;
                    }
                }
                // Plain text → follow-up message
                if follow_up_tx
                    .send(FollowUpMessage::text(line))
                    .await
                    .is_err()
                {
                    break; // receiver dropped
                }
            }
        });
    }

    if use_web {
        // Backend task dispatcher: listens on the bus for
        // ControlCommand(StartTask | FollowUp) from the dashboard and
        // routes to the presence layer or the follow-up channel.
        //
        // `follow_up_tx` only when something will read the other end:
        // `follow_up_rx` is consumed by `run_external_agent_mode` /
        // `run_direct_mode` — the `!use_presence` branches below.
        // `run_with_presence` never reads it, so handing the dispatcher a
        // sender in the presence shape made `try_send` "succeed" into a
        // never-drained channel and emit phantom `SteerQueued` receipts
        // (and silently ate task/follow-up fallbacks) for messages nothing
        // would ever deliver. With `None`, those fallbacks reach the
        // explicit warn+drop instead.
        task_dispatch::Dispatcher {
            presence_tx: presence_tx_for_dispatch,
            task_tx: task_tx.clone(),
            follow_up_tx: (!use_presence).then(|| follow_up_tx.clone()),
            primary_session_id: session_log_id(&session_log),
        }
        .spawn(bus.clone());

        // askHuman replies from the dashboard: write the human_response
        // file the agent polls for (the TUI used to do this on
        // ControlCommand(Input)).
        let log_dir_for_input = log_dir.clone();
        let mut input_rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match input_rx.recv().await {
                    Ok(AppEvent::ControlCommand(event::ControlMsg::Input { text })) => {
                        let resp_path = log_dir_for_input.join("human_response");
                        if let Err(e) = std::fs::write(&resp_path, text.as_bytes()) {
                            eprintln!(
                                "Failed to write askHuman response {}: {}",
                                resp_path.display(),
                                e
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Approval / question resolutions from the dashboard and the
        // control socket: resolve the foreground session's pending
        // oneshot in the approval registry (the TUI's ControlCommand
        // arms used to do this; the session supervisor covers only
        // daemon-managed sessions, so without this a foreground
        // approval_required blocks its turn forever). Matches by id
        // only, exactly like the TUI-era resolver. The waiter that
        // receives the response emits ApprovalResolved itself.
        let approval_registry_for_controls = approval_registry.clone();
        let mut approvals_rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                let ctrl = match approvals_rx.recv().await {
                    Ok(AppEvent::ControlCommand(ctrl)) => ctrl,
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let (id, response) = match ctrl {
                    event::ControlMsg::Approve { id, .. } => (id, event::ApprovalResponse::Approve),
                    event::ControlMsg::ApproveAll { id, .. } => {
                        (id, event::ApprovalResponse::ApproveAll)
                    }
                    event::ControlMsg::Deny { id, .. } => (id, event::ApprovalResponse::Deny),
                    event::ControlMsg::Skip { id, .. } => (id, event::ApprovalResponse::Skip),
                    event::ControlMsg::AnswerQuestion { id, answers, .. } => {
                        (id, event::ApprovalResponse::Answer { answers })
                    }
                    _ => continue,
                };
                let responder = approval_registry_for_controls
                    .lock()
                    .ok()
                    .and_then(|mut registry| registry.remove(&id));
                if let Some(tx) = responder {
                    let _ = tx.send(response);
                }
            }
        });
    }
    // Only the dispatcher / stdin reader hold senders now (if any).
    drop(follow_up_tx);

    // Web gateway with the foreground session's query context: the
    // dashboard's annotation Send button needs the context_injection
    // queue regardless of whether the presence layer is enabled, so
    // injections reach the agent loop in --no-presence mode too. When
    // presence is disabled, agent_state is a fresh empty snapshot (no
    // live updates), but context_injection is still wired through.
    let mut headless_peer_registry: Option<peer::PeerRegistry> = None;
    let headless_shared_session: Option<web_gateway::SharedActiveSession> = if use_web {
        let (transcriber, transcriber_err) =
            startup::wiring::build_transcriber(&project.config.transcription);
        if let Some(err) = transcriber_err {
            eprintln!("{}", err);
        }
        let query_ctx = Some(web_gateway::WebQueryCtx {
            agent_state: agent_state.clone(),
            project_root: project.root.clone(),
            log_dir: log_dir.clone(),
            knowledge_path: project.memory_path(),
            presence_session: Some(presence_session.clone()),
            context_injection: Some(context_injection.clone()),
        });
        let gateway = startup::wiring::spawn_mode_web_gateway(
            flags,
            &project,
            Some(project.root.clone()),
            &autonomy,
            &log_dir,
            &session_log,
            &bus,
            &mut web_listener,
            web_tls_client_cert_required,
            &web_tls_acceptor,
            web_port,
            web_bind_ip,
            runtime_presence_enabled,
            &initial_agent_backend,
            &shared_external_agent,
            &frame_registry,
            &session_registry,
            &recording_registry,
            &shared_file_watcher,
            transcriber,
            outbound_tx.clone(),
            query_ctx,
            Some(session_log.clone()),
        )?;
        eprintln!("{}", gateway.log_line);
        headless_peer_registry = Some(gateway.peer_registry.clone());
        Some(gateway.shared_session)
    } else {
        None
    };

    let mcp_mgr = if !project.config.mcp_servers.is_empty() {
        Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
    } else {
        None
    };

    let session_id = log_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    bus.send(AppEvent::SessionStarted {
        session_id: session_id.clone(),
        task: Some(task.clone()),
    });

    // Save for daemon loop (project and autonomy are moved into the agent loop)
    let project_root = project.root.clone();
    let autonomy_for_daemon = autonomy.clone();

    // External agent backend resolved at startup; the shared runtime handle
    // above is kept in sync by ControlPlane SetExternalAgent messages.
    let agent_backend = initial_agent_backend.clone();
    let shared_codex_config = shared_codex_config_from_project(&project);
    let shared_claude_config = shared_claude_config_from_project(&project);
    let _control_plane_handle = control_plane::spawn(control_plane::ControlPlaneState {
        autonomy: autonomy.clone(),
        external_agent: shared_external_agent.clone(),
        codex_config: shared_codex_config.clone(),
        claude_config: shared_claude_config.clone(),
        bus: bus.clone(),
        project_root: Some(project.root.clone()),
    });

    // Session vitals: cache/limits are usage-driven and always produced;
    // the git segment probes the live target registry (primary session
    // seed + supervisor-registered sessions).
    let vitals = if use_web {
        let seed = session_log_id(&session_log)
            .map(|session_id| vec![(session_id, project.root.clone())])
            .unwrap_or_default();
        Some(session_vitals::spawn_session_vitals_producer(
            bus.clone(),
            seed,
        ))
    } else {
        None
    };
    let vitals_git_targets = vitals.as_ref().map(|(targets, _)| targets.clone());

    // Dashboard-driven CreateSession / ResumeSession while the foreground
    // session runs: parallel sessions are owned by the session supervisor
    // (the dispatcher deliberately ignores them).
    let _resume_listener_handle = if use_web {
        Some(
            session_supervisor::SessionSupervisor::new(
                session_supervisor::SessionSupervisorConfig {
                    bus: bus.clone(),
                    project_root: Some(project.root.clone()),
                    autonomy: autonomy.clone(),
                    shared_external_agent: shared_external_agent.clone(),
                    shared_codex_config: shared_codex_config.clone(),
                    shared_claude_config: shared_claude_config.clone(),
                    frame_registry: frame_registry.clone(),
                    session_registry: Some(session_registry.clone()),
                    peer_registry: headless_peer_registry.clone(),
                    web_port: web_port_for_agent,
                    flags_direct: flags.direct,
                    shared_session: headless_shared_session.clone(),
                    provider_factory: None,
                    logs_home_override: None,
                    git_vitals_targets: vitals_git_targets.clone(),
                },
            )
            .spawn_resume_listener(),
        )
    } else {
        None
    };

    // Native usage rail: derive per-session UsageSnapshots from
    // ModelResponse events (dashboard meter + cache/limits vitals).
    let _usage_rail = if use_web {
        Some(crate::usage_rail::spawn_native_usage_rail(
            bus.clone(),
            crate::usage_rail::ProviderIdentity::from_provider(provider.as_deref()),
        ))
    } else {
        None
    };

    // Rehydrate the resumed session's persisted per-session agent config
    // (managed context, sandbox, approval policy, agent command, …) for a
    // startup `--resume`/`--continue`, mirroring the daemon resume path in
    // `SessionSupervisor::resume_session`. Applied after the shared runtime
    // configs were seeded above so per-session overrides stay per-session.
    let startup_external_resume_overrides = agent_backend.as_ref().and_then(|backend| {
        apply_startup_external_resume_config(
            backend,
            &mut project,
            session_log_id(&session_log).as_deref(),
            startup_external_resume_session.as_deref(),
        )
    });

    let result = if use_presence {
        // Presence mode: the presence layer mediates between user and
        // agent. Channels are Some when use_presence is true (see above).
        let presence_user_rx = presence_user_rx.unwrap();
        let presence_event_rx = presence_event_rx_for_task.unwrap();
        let task_tx = task_tx.expect("task_tx created in presence mode");
        let task_rx = task_rx.expect("task_rx created in presence mode");
        let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<String>(8);

        // Forward presence responses to the dashboard as log entries +
        // reset phase.
        let bus_for_responses = bus.clone();
        tokio::spawn(async move {
            while let Some(response) = response_rx.recv().await {
                if !response.is_empty() {
                    if response.starts_with("Presence error:")
                        || response.starts_with("Presence provider timed out")
                    {
                        bus_for_responses.send(AppEvent::LoopError(response));
                    } else {
                        // Log presence response as a visible PresenceLog entry
                        bus_for_responses.send(AppEvent::PresenceLog {
                            message: format!("[presence] {}", response),
                            level: None,
                            turn: None,
                        });
                        // Switch to follow-up mode after presence responds
                        bus_for_responses.send(AppEvent::PresenceReady);
                    }
                }
            }
        });

        run_with_presence(
            Some(task.clone()),
            project,
            bus.clone(),
            autonomy,
            session_log.clone(),
            log_dir,
            presence_user_rx,
            response_tx,
            presence_event_rx,
            agent_state.clone(),
            flags.direct,
            presence_paused.clone(),
            task_tx,
            task_rx,
            approval_registry.clone(),
            frame_registry.clone(),
            context_injection.clone(),
            session_registry.clone(),
            headless_peer_registry.clone(),
            agent_backend,
            shared_external_agent.clone(),
            shared_codex_config.clone(),
            shared_claude_config.clone(),
            if use_web { Some(web_port) } else { None },
            startup_external_resume_session.clone(),
            startup_external_resume_overrides,
        )
        .await
    } else if let Some(backend) = agent_backend {
        run_external_agent_mode(
            backend,
            task.clone(),
            project,
            bus.clone(),
            autonomy,
            session_log.clone(),
            log_dir,
            follow_up_rx,
            json_approval_slot,
            approval_registry.clone(),
            context_injection.clone(),
            !use_web, // under the gateway, approvals surface in the dashboard
            web_port_for_agent,
            UserAttachments::default(),
            startup_external_resume_session.clone(),
            startup_external_resume_overrides
                .as_ref()
                .and_then(|config| config.codex_service_tier.clone()),
            startup_external_resume_overrides
                .as_ref()
                .and_then(|config| config.codex_home.clone()),
            None,
            false,
            None,
        )
        .await
    } else {
        let provider = provider
            .ok_or_else(|| CallerError::Config("Headless mode requires an API key".to_string()))?;
        // Orchestration (sub-agent spawning) requires the daemon's
        // session supervisor; foreground non-daemon tasks run as direct
        // sessions.
        run_direct_mode(
            provider,
            task.clone(),
            project,
            bus.clone(),
            autonomy,
            session_log.clone(),
            log_dir,
            mcp_mgr,
            follow_up_rx,
            json_approval_slot,
            approval_registry.clone(),
            context_injection.clone(),
            Some(session_registry.clone()),
            headless_peer_registry.clone(),
            !use_web, // under the gateway, approvals surface in the dashboard
            UserAttachments::default(),
            NativeSessionConfig::direct(),
        )
        .await
    };

    let reason = match &result {
        Ok(stats) => {
            let outcome = stats.terminal_outcome.as_deref().unwrap_or("completed");
            slog(&session_log, |l| {
                l.write_summary_with_rounds(&task, outcome, stats.turns, Some(stats.rounds))
            });
            outcome.to_string()
        }
        Err(e) => {
            slog(&session_log, |l| {
                l.write_summary(&task, &format!("error: {}", e), 0)
            });
            format!("error: {}", e)
        }
    };

    bus.send(AppEvent::SessionEnded {
        session_id,
        reason: reason.clone(),
        error_kind: result
            .as_ref()
            .err()
            .and_then(|e| e.session_end_kind())
            .map(str::to_string),
    });

    if use_web {
        // Daemon mode: keep web gateway alive, listen for new tasks from web UI.
        if let Some(ref shared_session) = headless_shared_session {
            // Clear session-specific state so new connections see "no active session"
            {
                let mut ss = shared_session.write().await;
                ss.query_ctx = None;
                ss.session_log = None;
                // Keep frame_registry and recording_registry alive
            }
        }
        eprintln!(
            "Session ended ({}). Web gateway running on port {}. Waiting for new tasks...",
            reason, web_port
        );

        run_daemon_loop(DaemonConfig {
            bus: bus.clone(),
            project_root: Some(project_root.clone()),
            autonomy: autonomy_for_daemon.clone(),
            shared_external_agent: shared_external_agent.clone(),
            shared_codex_config: shared_codex_config.clone(),
            shared_claude_config: shared_claude_config.clone(),
            frame_registry: frame_registry.clone(),
            session_registry: Some(session_registry.clone()),
            peer_registry: headless_peer_registry.clone(),
            web_port: web_port_for_agent,
            flags_direct: flags.direct,
            shared_session: headless_shared_session.clone(),
            git_vitals_targets: vitals_git_targets.clone(),
        })
        .await;
    } else {
        result?;
    }

    control::cleanup();

    Ok(())
}
