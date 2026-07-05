//! The interactive (terminal TUI) execution shape:
//! run_interactive_mode is main()'s `else if use_tui` arm — ratatui
//! frontend over the same EventBus, with the daemon-loop fallthrough
//! when the TUI exits under an active web gateway.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_interactive_mode(
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
        // TUI mode — task may be None (user provides it via follow-up input)

        // TUI mode
        let bus = EventBus::new();
        let event_rx = bus.subscribe();

        // Spawn background tasks.
        // In web mode, key events come from WebSocket, not the terminal.
        let _crossterm_handle = if !use_web {
            Some(tui::event::spawn_crossterm_reader(bus.clone()))
        } else {
            None
        };
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 100);
        let _human_monitor = event::spawn_human_question_monitor(
            bus.clone(),
            event::shared_question_path(log_dir.join("human_question")),
        );
        let _session_listeners = startup::wiring::spawn_session_listeners(
            &bus,
            &recording_registry,
            &session_registry,
            &frame_registry,
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler =
            startup::wiring::spawn_debug_handler(&bus, &project, web_port, use_web);

        // TUI is created later — just before run() — so that web mode
        // (--web) can use WebTui instead of the real terminal backend.

        // Create app state
        let mut app = tui::app::App::new(
            provider
                .as_ref()
                .map(|p| p.name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            provider
                .as_ref()
                .map(|p| p.model().to_string())
                .unwrap_or_else(|| "none".to_string()),
            autonomy.clone(),
            log_dir.clone(),
        );
        app.context_window = provider.as_ref().map(|p| p.context_window()).unwrap_or(0);
        app.session_id = session_log
            .lock()
            .map(|l| l.session_id().to_string())
            .unwrap_or_default();
        app.task_description = task.clone().unwrap_or_default();
        app.project_root = Some(project.root.clone());
        app.knowledge_path = Some(project.memory_path());
        app.skills = skills::discover_skills(Some(&project.root));
        if flags.verbose {
            app.pending_verbosity = Some(types::Verbosity::Debug);
        }
        if flags.control_socket {
            let (_control_handle, control_tx) = control::spawn_control_server(bus.clone());
            app.set_control_socket(control_tx);
            app.log(
                types::LogLevel::Info,
                format!("Control socket: {}", control::socket_path().display()),
            );
        }

        // Per-connection WebTui command channel (only for web mode).
        let (web_tui_tx, web_tui_rx) = if use_web {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<tui::web::WebTuiCommand>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Web gateway broadcast channel — shares with control socket if both enabled.
        // The actual web gateway spawn is deferred until after presence setup so we
        // can pass the WebQueryCtx (agent state, project root, etc.) for tool requests.
        let web_broadcast_tx = if use_web {
            let tx = if let Some(ref tx) = app.control_tx {
                tx.clone()
            } else {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            };
            Some(tx)
        } else {
            None
        };

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents on the
        // shared broadcast channel (control socket / web gateway).
        let _outbound_broadcaster = app
            .control_tx
            .as_ref()
            .map(|tx| event::spawn_outbound_broadcaster(bus.subscribe(), tx.clone()));

        let _log_sinks = startup::wiring::spawn_log_sinks(&bus, &session_log);

        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            startup::wiring::start_project_file_watcher(&project.root, &log_dir, &bus);

        if let Some(ref t) = task {
            app.log(types::LogLevel::Info, format!("Task: {}", t));
        }

        // Determine if presence layer should be active.
        // Note: --direct only forces single-agent mode for the worker; it does
        // NOT disable presence.  Use --no-presence to disable presence.
        let use_presence = !flags.no_presence && project.config.presence.enabled;

        // Create follow-up channel for multi-round support.
        // When there is no initial task, the follow-up channel also delivers
        // the very first task from the input panel. Owned by the task
        // dispatcher (spawned below), not the TUI — the TUI emits
        // ControlCommand on the bus, the dispatcher routes.
        let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(4);

        // If no task was provided, start in follow-up mode so the user sees
        // the input panel immediately.
        if task.is_none() {
            app.current_phase = types::Phase::WaitingFollowUp;
            app.mode = tui::app::AppMode::FollowUp;
            let mut textarea = ratatui_textarea::TextArea::default();
            textarea.set_cursor_line_style(ratatui::style::Style::default());
            app.follow_up_textarea = Some(textarea);
            app.log(
                types::LogLevel::Info,
                "Ready. Enter a task to get started.".to_string(),
            );
        }

        // If presence is active, create channels for user ↔ presence communication
        // and the shared agent state snapshot. The presence_tx sender is owned by
        // the task dispatcher (spawned below), which routes non-direct user text
        // through the presence LLM.
        let (
            presence_user_rx,
            presence_event_rx_for_task,
            presence_agent_state,
            presence_tx_for_dispatch,
        ) = if use_presence {
            let (presence_tx, presence_user_rx) = tokio::sync::mpsc::channel::<String>(4);

            // Create presence event channel: TUI forwards filtered events here
            let (presence_event_tx, presence_event_rx) =
                tokio::sync::mpsc::channel::<presence::PresenceEvent>(64);
            app.set_presence_event_sender(presence_event_tx);

            // Shared agent state: updated by TUI (via forward_to_presence), read by presence tools
            let agent_state = Arc::new(std::sync::Mutex::new(
                presence::AgentStateSnapshot::default(),
            ));
            app.set_presence_agent_state(agent_state.clone());

            app.log_sourced(
                types::LogLevel::Info,
                "Presence layer active".to_string(),
                tui::app::LogSource::Presence,
                None,
            );
            // If there's an initial task, set the phase to Thinking immediately
            // so the TUI doesn't sit at "Idle" during the presence API call.
            if task.is_some() {
                app.current_phase = types::Phase::Thinking;
            }
            (
                Some(presence_user_rx),
                Some(presence_event_rx),
                Some(agent_state),
                Some(presence_tx),
            )
        } else {
            (None, None, None, None)
        };

        // Create the shared PresenceSession for event replay and checkpoints
        let presence_session = {
            let sid = session_log
                .lock()
                .map(|l| l.session_id().to_string())
                .unwrap_or_default();
            Arc::new(Mutex::new(presence::PresenceSession::new(sid)))
        };
        app.presence_session = Some(presence_session.clone());
        app.session_log = Some(session_log.clone());

        // Task dispatch channel: browser tool calls / dashboard StartTask →
        // presence task loop (CU-first routing). Only created when presence
        // is enabled, because the channel is consumed by `run_with_presence`.
        // The sender is owned by the dispatcher (spawned below) and by the
        // presence layer (its own `submit_task` tool). In non-presence mode,
        // leaving `task_tx` as None makes the dispatcher route to
        // `follow_up_tx` instead, which is consumed by
        // `run_external_agent_mode` / `run_direct_mode`.
        let (task_tx, task_rx) = if use_presence {
            let (tx, rx) = tokio::sync::mpsc::channel::<presence::TaskEnvelope>(4);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Spawn the backend task dispatcher. It listens on the bus for
        // ControlCommand(StartTask | FollowUp) and routes to the appropriate
        // channel. Replaces the routing logic that used to live in the TUI.
        let _dispatcher_handle = task_dispatch::Dispatcher {
            presence_tx: presence_tx_for_dispatch,
            task_tx: task_tx.clone(),
            follow_up_tx: Some(follow_up_tx.clone()),
            primary_session_id: session_log
                .lock()
                .map(|log| log.session_id().to_string())
                .ok(),
        }
        .spawn(bus.clone());

        // Deferred web gateway spawn — now we have the agent state for tool queries.
        // Note: WebQueryCtx is built UNCONDITIONALLY (not gated on presence).
        // The web dashboard's annotation Send button needs the context_injection
        // queue regardless of whether the presence layer is enabled, so that
        // injections still reach the agent loop in --no-presence mode.
        // When presence is disabled, agent_state is a fresh empty snapshot
        // (no live updates), but context_injection is still wired through.
        let mut web_shared_session_for_supervisor: Option<web_gateway::SharedActiveSession> = None;
        let _web_handle = if let Some(broadcast_tx) = web_broadcast_tx {
            let query_ctx_agent_state = presence_agent_state.clone().unwrap_or_else(|| {
                Arc::new(std::sync::Mutex::new(
                    presence::AgentStateSnapshot::default(),
                ))
            });
            let query_ctx = Some(web_gateway::WebQueryCtx {
                agent_state: query_ctx_agent_state,
                project_root: project.root.clone(),
                log_dir: log_dir.clone(),
                knowledge_path: project.memory_path(),
                presence_session: Some(presence_session.clone()),
                context_injection: Some(app.context_injection.clone()),
            });
            let (transcriber, transcriber_err) =
                startup::wiring::build_transcriber(&project.config.transcription);
            if let Some(err) = transcriber_err {
                app.log(types::LogLevel::Warn, err);
            }
            let gateway = startup::wiring::spawn_mode_web_gateway(
                flags,
                &project,
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
                broadcast_tx,
                query_ctx,
                Some(session_log.clone()),
                web_tui_tx.clone(),
            )?;
            web_shared_session_for_supervisor = Some(gateway.shared_session.clone());
            app.log(types::LogLevel::Info, gateway.log_line.clone());
            Some(gateway)
        } else {
            None
        };

        // Save for daemon loop (project is moved into the agent loop closure)
        let project_root = project.root.clone();
        // Clone frame_registry for event handlers (original may be moved into spawns)
        let frame_registry_for_events = frame_registry.clone();

        // Spawn the agent loop in a background task
        let bus_clone = bus.clone();
        let autonomy_clone = autonomy.clone();
        let session_log_clone = session_log.clone();
        let session_log_summary = session_log.clone();
        let log_dir_clone = log_dir.clone();
        let approval_registry_clone = app.approval_registry.clone();
        let context_injection_clone = app.context_injection.clone();
        let session_registry_clone = session_registry.clone();
        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };
        let force_direct = flags.direct;
        // External agent backend resolved at startup; the shared runtime handle
        // above is kept in sync by ControlPlane SetExternalAgent messages.
        let agent_backend = initial_agent_backend.clone();
        // Live Codex config — seeded from TOML, updated by SetCodex* ControlMsgs.
        // The daemon loop reads this at the start of each task so a Control-tab
        // toggle takes effect on the next task without needing a restart.
        let shared_codex_config = shared_codex_config_from_project(&project);
        let shared_claude_config = shared_claude_config_from_project(&project);
        let _control_plane_handle = control_plane::spawn(
            bus.subscribe(),
            control_plane::ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: shared_external_agent.clone(),
                codex_config: shared_codex_config.clone(),
                claude_config: shared_claude_config.clone(),
                bus: bus.clone(),
                project_root: Some(project.root.clone()),
            },
        );
        let _resume_listener_handle = if use_web {
            Some(
                session_supervisor::SessionSupervisor::new(
                    session_supervisor::SessionSupervisorConfig {
                        bus: bus.clone(),
                        project_root: project.root.clone(),
                        autonomy: autonomy.clone(),
                        shared_external_agent: shared_external_agent.clone(),
                        shared_codex_config: shared_codex_config.clone(),
                        shared_claude_config: shared_claude_config.clone(),
                        frame_registry: frame_registry.clone(),
                        session_registry: Some(session_registry.clone()),
                        web_port: web_port_for_agent,
                        flags_direct: flags.direct,
                        shared_session: web_shared_session_for_supervisor.clone(),
                        provider_factory: None,
                    },
                )
                .spawn_resume_listener(),
            )
        } else {
            None
        };
        // A startup `--resume`/`--continue` of an external session must run
        // with that session's persisted per-session agent config (managed
        // context, sandbox, approval policy, agent command, …), not the
        // global defaults — same rehydration the daemon resume path does in
        // `SessionSupervisor::resume_session`. Applied after the shared
        // runtime configs were seeded above so per-session overrides don't
        // leak into the dashboard's global Codex config.
        let startup_external_resume_overrides = agent_backend.as_ref().and_then(|backend| {
            apply_startup_external_resume_config(
                backend,
                &mut project,
                session_log_id(&session_log).as_deref(),
                startup_external_resume_session.as_deref(),
            )
        });

        let mut loop_handle = if use_presence {
            // Presence mode: the presence layer mediates between user and agent
            let presence_user_rx = presence_user_rx.unwrap();
            let presence_event_rx = presence_event_rx_for_task.unwrap();
            let agent_state = presence_agent_state.unwrap();
            // task_tx/task_rx are Some when use_presence is true (see above).
            let task_tx = task_tx.expect("task_tx created in presence mode");
            let task_rx = task_rx.expect("task_rx created in presence mode");
            let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<String>(8);

            // Shared paused ref-count: incremented by PresenceConnected, decremented by PresenceDisconnected.
            // Server-side presence is paused when count > 0 (any browser has active voice).
            let presence_paused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            app.set_presence_paused_flag(presence_paused.clone());

            // Forward presence responses to TUI as log entries + reset phase
            let bus_for_responses = bus_clone.clone();
            let _response_forwarder = tokio::spawn(async move {
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

            // Vitals chips for the primary session: git state of the
            // project root (statusline port).
            let _vitals_producer = if let Some(session_id) = session_log_id(&session_log) {
                Some(session_vitals::spawn_session_vitals_producer(
                    bus.clone(),
                    vec![(session_id, project.root.clone())],
                ))
            } else {
                None
            };

            let agent_backend_for_presence = agent_backend.clone();
            let shared_external_agent_for_presence = shared_external_agent.clone();
            let shared_codex_config_for_presence = shared_codex_config.clone();
            let shared_claude_config_for_presence = shared_claude_config.clone();
            let session_registry_for_presence = session_registry.clone();
            tokio::spawn(async move {
                let result = run_with_presence(
                    task,
                    project,
                    bus_clone.clone(),
                    autonomy_clone,
                    session_log_clone,
                    log_dir_clone,
                    presence_user_rx,
                    response_tx,
                    presence_event_rx,
                    agent_state,
                    force_direct,
                    presence_paused,
                    task_tx,
                    task_rx,
                    approval_registry_clone,
                    frame_registry.clone(),
                    context_injection_clone,
                    session_registry_for_presence,
                    agent_backend_for_presence,
                    shared_external_agent_for_presence,
                    shared_codex_config_for_presence,
                    shared_claude_config_for_presence,
                    if use_web { Some(web_port) } else { None },
                    startup_external_resume_session.clone(),
                    startup_external_resume_overrides,
                )
                .await;

                match result {
                    Ok(stats) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary_with_rounds(
                                "(presence)",
                                "completed",
                                stats.turns,
                                Some(stats.rounds),
                            )
                        });
                    }
                    Err(e) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary("(presence)", &format!("error: {}", e), 0)
                        });
                        bus_clone.send(AppEvent::LoopError(e.to_string()));
                    }
                }
            })
        } else {
            // Standard mode: direct agent loop.
            // When task is None, wait for the first follow-up message to
            // use as the task. This lets the TUI start idle.
            tokio::spawn(async move {
                let (task_str, follow_up_rx) = if let Some(t) = task {
                    (t, follow_up_rx)
                } else {
                    // Wait for the first message from the follow-up panel
                    match follow_up_rx.recv().await {
                        Some(first_task) => {
                            slog(&session_log_clone, |l| {
                                l.info(&format!("Task (from input): {}", first_task.text))
                            });
                            bus_clone.send(AppEvent::TurnStarted {
                                session_id: session_log_id(&session_log_clone),
                                turn: 0,
                                budget_pct: 0.0,
                                remaining: 0,
                            });
                            (first_task.text, follow_up_rx)
                        }
                        None => return, // channel closed before a task arrived
                    }
                };

                let result = if let Some(backend) = agent_backend {
                    run_external_agent_mode(
                        backend,
                        task_str,
                        project,
                        bus_clone.clone(),
                        autonomy_clone,
                        session_log_clone,
                        log_dir_clone,
                        follow_up_rx,
                        None,
                        approval_registry_clone,
                        context_injection_clone.clone(),
                        false, // not headless — TUI handles approval
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
                    // Re-select provider at task start (may have been None at startup)
                    let provider = match provider.or_else(|| provider::select_provider().ok()) {
                        Some(p) => p,
                        None => {
                            bus_clone.send(AppEvent::LoopError(
                                "No API key configured. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY.".to_string()
                            ));
                            return;
                        }
                    };

                    // Orchestration (sub-agent spawning) requires the
                    // daemon's session supervisor; TUI-mode tasks run as
                    // direct sessions.
                    run_direct_mode(
                        provider,
                        task_str,
                        project,
                        bus_clone.clone(),
                        autonomy_clone,
                        session_log_clone,
                        log_dir_clone,
                        mcp_mgr,
                        follow_up_rx,
                        None, // no JSON approval in TUI mode
                        approval_registry_clone,
                        context_injection_clone,
                        Some(session_registry_clone),
                        false, // not headless — TUI handles approval
                        UserAttachments::default(),
                        NativeSessionConfig::direct(),
                    )
                    .await
                };

                match result {
                    Ok(stats) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary_with_rounds(
                                "(tui)",
                                "completed",
                                stats.turns,
                                Some(stats.rounds),
                            )
                        });
                    }
                    Err(e) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary("(tui)", &format!("error: {}", e), 0)
                        });
                        bus_clone.send(AppEvent::LoopError(e.to_string()));
                    }
                }
            })
        };

        // Run the TUI event loop (blocks until quit).
        // In web mode, render to a buffer and stream to xterm.js.
        // In terminal mode, render directly to stdout.
        if use_web {
            let broadcast_tx = app.control_tx.clone().unwrap_or_else(|| {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            });
            eprintln!(
                "{}",
                web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip)
            );
            let mut web_tui = tui::web::WebTui::new(120, 40, broadcast_tx)
                .map_err(|e| CallerError::Tui(format!("Failed to initialize Web TUI: {}", e)))?;
            let cmd_rx = web_tui_rx.expect("web_tui_rx must exist in web mode");
            let _ = web_tui.run(&mut app, event_rx, cmd_rx, bus.clone()).await;
        } else {
            let mut terminal = tui::Tui::new()
                .map_err(|e| CallerError::Tui(format!("Failed to initialize TUI: {}", e)))?;
            let _ = terminal.run(&mut app, event_rx, bus.clone()).await;
        }

        // Drop the App (and its follow_up_tx) so the round loop's recv()
        // returns None and exits gracefully, allowing write_summary to run.
        let session_id = app.session_id.clone();
        drop(app);

        // Give the agent task a moment to finish writing the session summary.
        // If it doesn't finish in time (e.g. stuck on an API call), abort it.
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut loop_handle).await {
            Ok(_) => {}                    // task finished naturally
            Err(_) => loop_handle.abort(), // timed out — force stop
        }

        if use_web && !session_id.is_empty() {
            bus.send(AppEvent::SessionEnded {
                session_id,
                reason: "completed".to_string(),
            });
            // Daemon mode: keep web gateway alive after TUI quits.
            // Fall through to a headless daemon loop (TUI is not re-created).
            eprintln!(
                "TUI exited. Web gateway still running on port {}. Waiting for new tasks...",
                web_port
            );
            run_daemon_loop(DaemonConfig {
                bus: bus.clone(),
                project_root: project_root.clone(),
                autonomy: autonomy.clone(),
                shared_external_agent: shared_external_agent.clone(),
                shared_codex_config: shared_codex_config.clone(),
                shared_claude_config: shared_claude_config.clone(),
                frame_registry: frame_registry_for_events.clone(),
                session_registry: Some(session_registry.clone()),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: None,
            })
            .await;
        }

        control::cleanup();

    Ok(())
}
