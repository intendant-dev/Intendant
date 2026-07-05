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
        let _session_listeners = startup::wiring::spawn_session_listeners(
            &bus,
            &recording_registry,
            &session_registry,
            &frame_registry,
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler =
            startup::wiring::spawn_debug_handler(&bus, &project, web_port, use_web);

        // Outbound broadcast channel — shared by web gateway and JSON stdout subscriber
        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());

        let _log_sinks = startup::wiring::spawn_log_sinks(&bus, &session_log);

        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            startup::wiring::start_project_file_watcher(&project.root, &log_dir, &bus);

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
            let (transcriber, transcriber_err) =
                startup::wiring::build_transcriber(&project.config.transcription);
            if let Some(err) = transcriber_err {
                eprintln!("{}", err);
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
                outbound_tx.clone(),
                None, // query_ctx: headless has no dashboard agent-state queries
                Some(session_log.clone()),
                None, // web_tui_tx: headless has no WebTui
            )?;
            eprintln!("{}", gateway.log_line);
            Some(gateway.shared_session)
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
