//! The headless execution shape: run_headless_mode is main()'s final
//! else arm — no TUI, optional web gateway, task from flags/env, with
//! the daemon-loop fallthrough when the gateway stays up.

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
        // Headless mode always has a task (enforced above).
        let task = task.unwrap();

        // Headless mode: no WebTui or terminal TUI is active.
        let bus = EventBus::new();
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

        // Outbound broadcast channel — shared by web gateway and JSON stdout subscriber
        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents
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

        // Web gateway in headless mode
        let headless_shared_session: Option<web_gateway::SharedActiveSession> = if use_web {
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
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(flags, &project);
            let _web_handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                outbound_tx.clone(),
                config,
                shared_session.clone(),
                transcriber,
                None, // Headless mode: no WebTui
                None, // No task_tx in headless mode
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
            eprintln!(
                "{}",
                web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip)
            );
            Some(shared_session)
        } else {
            None
        };

        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };

        // Create follow-up channel. In JSON mode, spawn a stdin reader to enable
        // follow-up via stdin lines and JSON commands (approve, deny, input, etc.).
        // Otherwise, drop the sender immediately so recv() returns None → single-round.
        let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
        let json_approval_slot = if flags.json_output {
            Some(new_json_approval_slot())
        } else {
            None
        };
        if flags.json_output {
            // JSON mode: read follow-up lines and control commands from stdin
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
                                        let _ =
                                            tx.send(event::ApprovalResponse::Answer { answers });
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
        } else {
            drop(follow_up_tx); // single-round: recv() returns None immediately
        }

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
        let shared_codex_config: control_plane::SharedCodexConfig = {
            let cfg = &project.config.agent.codex;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: cfg.command.clone(),
                    managed_command: cfg.managed_command.clone(),
                    sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
                    approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
                    model: cfg.model.clone(),
                    reasoning_effort: project::normalize_reasoning_effort(
                        cfg.reasoning_effort.as_deref(),
                    ),
                    service_tier: project::normalize_codex_service_tier(
                        cfg.service_tier.as_deref(),
                    ),
                    web_search: cfg.web_search,
                    network_access: cfg.network_access,
                    writable_roots: cfg.writable_roots.clone(),
                    managed_context: project::normalize_codex_managed_context(&cfg.managed_context),
                    context_archive: project::normalize_codex_context_archive(&cfg.context_archive),
                },
            ))
        };
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

        let result = if let Some(backend) = agent_backend {
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
                event::ApprovalRegistry::default(),
                event::ContextInjectionQueue::default(),
                true, // headless mode
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
            let provider = provider.ok_or_else(|| {
                CallerError::Config("Headless mode requires an API key".to_string())
            })?;
            // Orchestration (sub-agent spawning) requires the daemon's
            // session supervisor; headless non-daemon tasks run as direct
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
                event::ApprovalRegistry::default(),
                event::ContextInjectionQueue::default(),
                Some(session_registry.clone()),
                true, // headless mode
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
                project_root: project_root.clone(),
                autonomy: autonomy_for_daemon.clone(),
                shared_external_agent: shared_external_agent.clone(),
                shared_codex_config: shared_codex_config.clone(),
                shared_claude_config: shared_claude_config.clone(),
                frame_registry: frame_registry.clone(),
                session_registry: Some(session_registry.clone()),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: headless_shared_session.clone(),
            })
            .await;
        } else {
            result?;
        }

    Ok(())
}
