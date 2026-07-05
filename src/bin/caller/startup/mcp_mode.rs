//! The MCP execution shape: run_mcp_mode is main()'s `if flags.mcp`
//! arm — Model Context Protocol on stdio, architecturally a peer of
//! the TUI (same EventBus, same UserAction contract).

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_mcp_mode(
    flags: &CliFlags,
    project: &Project,
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
) -> Result<(), CallerError> {
        // MCP mode — speaks Model Context Protocol on stdio.
        // This is architecturally a peer of the TUI: same EventBus, same UserAction contract.
        let bus = EventBus::new();
        let event_rx = bus.subscribe();
        let human_question_path = event::shared_question_path(log_dir.join("human_question"));
        let _human_monitor =
            event::spawn_human_question_monitor(bus.clone(), human_question_path.clone());
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 1000);
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if use_web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                web_port,
                bus.clone(),
            ))
        } else {
            None
        };
        let mcp_control_tx = if flags.control_socket {
            let (_control_handle, control_tx) = control::spawn_control_server(bus.clone());
            slog(&session_log, |l| {
                l.info(&format!(
                    "Control socket: {}",
                    control::socket_path().display()
                ))
            });
            Some(control_tx)
        } else {
            None
        };

        // Outbound event broadcast channel — shared by control socket, web gateway,
        // and the outbound broadcaster.  If control socket is active, reuse its
        // channel; otherwise create a standalone one when web or broadcaster needs it.
        let outbound_tx = if let Some(ref tx) = mcp_control_tx {
            tx.clone()
        } else if use_web {
            let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
            tx
        } else {
            // No control socket, no web — create a channel anyway so the
            // outbound broadcaster can still run (receivers just drop events).
            let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
            tx
        };

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents on the
        // shared broadcast channel.
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe_session_log(), session_log.clone());

        let _fission_lifecycle_watcher = start_fission_lifecycle(&bus, &session_log);

        // File watcher: observes project directory for changes, emits FileChanged events.
        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            if !file_watcher::root_is_snapshot_worthy(&project.root) {
                // Fallback roots (no .git / intendant.toml — e.g. a service's
                // $HOME WorkingDirectory) must never be baseline-scanned: it
                // blocks boot for minutes and shadow-copies the whole tree.
                eprintln!(
                    "[file_watcher] rewind snapshots off: {} is not a project root (no .git or \
                 intendant.toml) — start intendant inside a project to enable rewind",
                    project.root.display()
                );
                (None, None, None)
            } else {
                let snapshot_dir = log_dir.join("file_snapshots");
                match file_watcher::FileWatcher::new(
                    project.root.clone(),
                    snapshot_dir,
                    bus.clone(),
                ) {
                    Ok(watcher) => {
                        let (fw, wh, rh) = watcher.start_shared();
                        (Some(fw), Some(wh), Some(rh))
                    }
                    Err(e) => {
                        eprintln!("[file_watcher] Failed to start: {}", e);
                        (None, None, None)
                    }
                }
            };

        // Web gateway (WebSocket)
        let _web_handle = if use_web {
            let broadcast_tx = outbound_tx.clone();
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            eprintln!("Transcription init failed: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
            let mut config = web_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
                project.config.transcription.enabled,
                project.config.webrtc.to_ice_config(),
                project.config.webrtc.federation_allow_h264,
            );
            config.peer_access_requests = project.config.server.peer_access_requests.clone();
            config.connect = project.config.connect.clone().effective_with_env();
            config.presence_enabled = runtime_presence_enabled;
            config.external_agent = initial_agent_backend
                .as_ref()
                .map(|backend| backend.as_short_str().to_string());
            let snapshot_dir = log_dir.join("file_snapshots");
            let shared_session =
                Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
                    daemon_session_id: session_log_id(&session_log),
                    query_ctx: None,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                    session_registry: Some(session_registry.clone()),
                    snapshot_dir: Some(snapshot_dir.clone()),
                    project_root_for_changes: Some(project.root.clone()),
                    runtime_settings: web_gateway::RuntimeSettingsState {
                        external_agent: Some(shared_external_agent.clone()),
                        presence_enabled: Some(runtime_presence_enabled),
                    },
                    file_watcher: shared_file_watcher.clone(),
                }));
            let mut mcp_http_state = mcp::McpAppState::new(
                "none".into(),
                "none".into(),
                autonomy.clone(),
                log_dir.clone(),
            );
            mcp_http_state.codex_managed_context =
                project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
            mcp_http_state.configured_codex_managed_context = mcp_http_state.codex_managed_context;
            mcp_http_state.frame_registry = Some(frame_registry.clone());
            mcp_http_state.session_registry = Some(session_registry.clone());
            mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
            let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new_http(
                Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
                bus.clone(),
            )));
            let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(flags, project);
            let handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                broadcast_tx,
                config,
                shared_session,
                transcriber,
                None, // MCP mode: no WebTui
                None, // No task_tx in MCP mode
                Some(project.root.clone()),
                mcp_http_server,
                Some(peer_registry),
                advertise_urls,
                project.config.server.auth.bearer_token.clone(),
                build_local_advertised_auth(
                    &project.config.server.auth,
                    &access::backend::select_backend().cert_dir(),
                )?,
                web_tls_client_cert_required,
                web_tls_acceptor.clone(),
            );
            slog(&session_log, |l| {
                l.info(&web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip))
            });
            eprintln!(
                "{}",
                web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip)
            );
            Some(handle)
        } else {
            None
        };

        let mut mcp_app_state = mcp::McpAppState::new(
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
        mcp_app_state.external_agent = initial_agent_backend.clone();
        mcp_app_state.codex_managed_context =
            project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
        mcp_app_state.configured_codex_managed_context = mcp_app_state.codex_managed_context;
        mcp_app_state.context_window = provider.as_ref().map(|p| p.context_window()).unwrap_or(0);
        mcp_app_state.hard_context_window = provider.as_ref().map(|p| p.context_window());
        mcp_app_state.session_id = session_log
            .lock()
            .map(|l| l.session_id().to_string())
            .unwrap_or_default();
        mcp_app_state.task_description = task.clone().unwrap_or_default();
        mcp_app_state.frame_registry = Some(frame_registry.clone());
        mcp_app_state.session_registry = Some(session_registry.clone());
        mcp_app_state.screenshot_dir = Some(log_dir.join("screenshots"));
        let mcp_state = std::sync::Arc::new(tokio::sync::RwLock::new(mcp_app_state));

        // Build a launcher closure that can spawn the agent loop on demand.
        // This captures the provider factory parameters (not the provider itself,
        // since providers are not Clone) so each start_task creates a fresh provider.
        let project_root = project.root.clone();
        let autonomy_for_launcher = autonomy.clone();
        let session_log_for_launcher = session_log.clone();
        let log_dir_for_launcher = log_dir.clone();
        let mcp_state_for_launcher = mcp_state.clone();
        let session_registry_for_launcher = session_registry.clone();
        #[allow(clippy::async_yields_async)]
        let launcher: mcp::TaskLauncher = Box::new(move |task_str: String, bus: EventBus| {
            let project_root = project_root.clone();
            let autonomy = autonomy_for_launcher.clone();
            let session_log = session_log_for_launcher.clone();
            let _parent_log_dir = log_dir_for_launcher.clone();
            let mcp_state = mcp_state_for_launcher.clone();
            let session_registry = session_registry_for_launcher.clone();
            Box::pin(async move {
                // Each MCP task gets a fresh session directory so conversations
                // don't bleed between tasks (reasoning items, tool calls, etc.).
                let task_log_dir = session_log::SessionLog::resolve_path(None);
                match session_log::SessionLog::open(task_log_dir.clone()) {
                    Ok(mut l) => {
                        l.write_meta(Some(&project_root), Some(&task_str));
                        l.info(&format!("MCP sub-task session: {}", l.session_id()));
                        // Replace the shared session log with the fresh one
                        if let Ok(mut guard) = session_log.lock() {
                            *guard = l;
                        }
                        // Notify MCP state of the new session dir so askHuman
                        // response files are written to the correct location.
                        bus.send(AppEvent::SessionDirChanged {
                            path: task_log_dir.clone(),
                        });
                    }
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to create task session: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                }
                let log_dir = task_log_dir;

                // Create a fresh provider for this task
                let provider = match provider::select_provider() {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to create provider: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                };
                let project = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to load project: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                };
                // Consume the mode override set by start_task. Orchestration
                // (sub-agent spawning) needs the daemon's session supervisor;
                // this standalone MCP task path runs sessions directly, so an
                // orchestrate request degrades to a direct session.
                let orchestrate_override = {
                    let mut s = mcp_state.write().await;
                    s.next_task_orchestrate.take()
                };
                if orchestrate_override == Some(true) {
                    bus.send(AppEvent::LoopError(
                        "orchestrate=true requires the web daemon's session supervisor; \
                         running the task as a direct session"
                            .to_string(),
                    ));
                }

                // Create follow-up channel for multi-round support
                let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
                {
                    let mut s = mcp_state.write().await;
                    s.follow_up_tx = Some(follow_up_tx);
                }

                let approval_registry = mcp_state.read().await.approval_registry.clone();
                let bus_clone = bus.clone();
                let task_for_summary = task_str.clone();
                let session_log_summary = session_log.clone();
                let mcp_state_cleanup = mcp_state.clone();
                // Resolve external agent backend: MCP shared state > config default
                let agent_backend = resolve_agent_backend_from_config(
                    mcp_state.read().await.external_agent.clone(),
                    &project,
                );

                tokio::spawn(async move {
                    let result = if let Some(backend) = agent_backend {
                        run_external_agent_mode(
                            backend,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy,
                            session_log,
                            log_dir,
                            follow_up_rx,
                            None,
                            approval_registry,
                            event::ContextInjectionQueue::default(),
                            false,
                            web_port_for_agent,
                            UserAttachments::default(),
                            None,
                            None,
                            None,
                            None,
                            false,
                            None,
                        )
                        .await
                    } else {
                        run_direct_mode(
                            provider,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy,
                            session_log,
                            log_dir,
                            None,
                            follow_up_rx,
                            None, // no JSON approval in MCP mode
                            approval_registry,
                            event::ContextInjectionQueue::default(),
                            Some(session_registry),
                            false, // not headless — MCP has interactive approval
                            UserAttachments::default(),
                            NativeSessionConfig::direct(),
                        )
                        .await
                    };

                    match result {
                        Ok(stats) => {
                            slog(&session_log_summary, |l| {
                                l.write_summary_with_rounds(
                                    &task_for_summary,
                                    "completed",
                                    stats.turns,
                                    Some(stats.rounds),
                                )
                            });
                            // Note: TaskComplete is already emitted by run_agent_loop
                            // when it breaks (done signal, no JSON, etc.)
                        }
                        Err(e) => {
                            slog(&session_log_summary, |l| {
                                l.write_summary(&task_for_summary, &format!("error: {}", e), 0)
                            });
                            bus_clone.send(AppEvent::LoopError(e.to_string()));
                        }
                    }

                    // Clean up follow-up sender so MCP knows no task is active
                    {
                        let mut s = mcp_state_cleanup.write().await;
                        s.follow_up_tx = None;
                    }
                })
            })
        });

        // Store the launcher in MCP state
        {
            let mut s = mcp_state.write().await;
            s.launcher = Some(std::sync::Arc::new(launcher));
        }

        // If a task was provided on the CLI, start it immediately
        if let Some(initial_task) = task {
            let handle = {
                let s = mcp_state.read().await;
                let launcher = s.launcher.as_ref().unwrap().clone();
                drop(s);
                (launcher)(initial_task, bus.clone()).await
            };
            let mut s = mcp_state.write().await;
            s.phase = types::Phase::Thinking;
            s.task_handle = Some(handle);
        }

        // Run the MCP server on stdio (blocks until client disconnects or quit)
        if let Err(e) = mcp::run_mcp_server(
            mcp_state,
            bus,
            event_rx,
            Some(human_question_path),
            mcp_control_tx,
        )
        .await
        {
            slog(&session_log, |l| {
                l.info(&format!("MCP server ended: {}", e))
            });
        }
        if flags.control_socket {
            control::cleanup();
        }

    Ok(())
}
