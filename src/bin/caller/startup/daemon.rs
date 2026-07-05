//! The daemon execution shape: run_daemon is main()'s
//! web_daemon_requested branch (wiring + session supervisor), and
//! run_daemon_loop/DaemonConfig is the fallback daemon loop shared by
//! the TUI post-exit path and the headless web-gateway path.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

/// Configuration for `run_daemon_loop`.
pub(crate) struct DaemonConfig {
    pub(crate) bus: EventBus,
    pub(crate) project_root: PathBuf,
    pub(crate) autonomy: SharedAutonomy,
    pub(crate) shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    pub(crate) shared_codex_config: control_plane::SharedCodexConfig,
    pub(crate) shared_claude_config: control_plane::SharedClaudeConfig,
    pub(crate) frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    pub(crate) session_registry: Option<display::SharedSessionRegistry>,
    pub(crate) web_port: Option<u16>,
    pub(crate) flags_direct: bool,
    /// Optional shared session state for headless mode (cleared between tasks).
    pub(crate) shared_session: Option<web_gateway::SharedActiveSession>,
}

/// Daemon loop shared by the TUI post-exit path and the headless web-gateway path.
///
/// Waits for `StartTask` and `SetExternalAgent` control messages from the web
/// UI, spawning agent tasks in the background. Exits when the bus closes.
///
/// Ctrl+C is handled by the global signal handler installed in `main`, which
/// writes `mark_interrupted` to the session meta and calls `exit(130)` — we
/// deliberately do not also listen for it here because racing two handlers
/// risked the loop `break`ing before the meta update ran.
pub(crate) async fn run_daemon_loop(config: DaemonConfig) {
    session_supervisor::SessionSupervisor::new(session_supervisor::SessionSupervisorConfig {
        bus: config.bus,
        project_root: config.project_root,
        autonomy: config.autonomy,
        shared_external_agent: config.shared_external_agent,
        shared_codex_config: config.shared_codex_config,
        shared_claude_config: config.shared_claude_config,
        frame_registry: config.frame_registry,
        session_registry: config.session_registry,
        web_port: config.web_port,
        flags_direct: config.flags_direct,
        shared_session: config.shared_session,
        provider_factory: None,
    })
    .run()
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_daemon(
    flags: &CliFlags,
    project: &Project,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
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
    daemon_startup_resume_dir: Option<PathBuf>,
) -> Result<(), CallerError> {
        let bus = EventBus::new();
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
        // Windows: auto-register the existing desktop as an active display so
        // the dashboard streams it on connect (mirrors the macOS end state of
        // a live session sitting in the registry). macOS/Linux compile this
        // out and keep activating only on an explicit grant.
        #[cfg(target_os = "windows")]
        auto_activate_windows_user_display(
            &bus,
            &session_registry,
            Some(frame_registry.clone()),
            &autonomy,
        )
        .await;
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = Some(debug::spawn_debug_screen_handler(
            bus.subscribe(),
            project.config.recording.clone(),
            web_port,
            bus.clone(),
        ));

        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe_session_log(), session_log.clone());
        let _fission_lifecycle_watcher = start_fission_lifecycle(&bus, &session_log);

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
        let mut web_config = web_gateway::build_config(
            project.config.presence.live_provider.as_deref(),
            project.config.presence.live_model.as_deref(),
            project.config.transcription.enabled,
            project.config.webrtc.to_ice_config(),
            project.config.webrtc.federation_allow_h264,
        );
        web_config.peer_access_requests = project.config.server.peer_access_requests.clone();
        web_config.connect = project.config.connect.clone().effective_with_env();
        web_config.presence_enabled = runtime_presence_enabled;
        web_config.external_agent = initial_agent_backend
            .as_ref()
            .map(|backend| backend.as_short_str().to_string());
        let shared_session = Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
            daemon_session_id: session_log_id(&session_log),
            query_ctx: None,
            frame_registry: Some(frame_registry.clone()),
            session_log: None,
            recording_registry: Some(recording_registry.clone()),
            session_registry: Some(session_registry.clone()),
            snapshot_dir: Some(log_dir.join("file_snapshots")),
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
        let _web_handle = web_gateway::spawn_web_gateway(
            web_listener
                .take()
                .expect("web listener must exist when use_web"),
            bus.clone(),
            outbound_tx.clone(),
            web_config,
            shared_session.clone(),
            transcriber,
            None,
            None,
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
        let shared_claude_config = shared_claude_config_from_project(project);
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

        let startup_bus = bus.clone();
        let supervisor_handle = session_supervisor::SessionSupervisor::new(
            session_supervisor::SessionSupervisorConfig {
                bus,
                project_root: project.root.clone(),
                autonomy,
                shared_external_agent,
                shared_codex_config,
                shared_claude_config,
                frame_registry,
                session_registry: Some(session_registry.clone()),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: Some(shared_session),
                provider_factory: None,
            },
        )
        .spawn();
        // --continue/--resume under the daemon: the supervisor (subscribed
        // above, before this send) resumes the target session — attach only,
        // no task; follow-ups come from the dashboard/TUI like any session.
        if let Some(resume_dir) = daemon_startup_resume_dir {
            let session_id = resume_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let source = crate::session_config::read_log_dir_config(&resume_dir)
                .and_then(|config| config.source)
                .unwrap_or_else(|| "intendant".to_string());
            eprintln!("Resuming session {session_id} ({source}) in the daemon");
            startup_bus.send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                source,
                session_id,
                resume_id: None,
                project_root: None,
                task: None,
                direct: None,
                attachments: Vec::new(),
                fork: false,
                agent_command: None,
                codex_sandbox: None,
                codex_approval_policy: None,
                codex_managed_context: None,
                codex_context_archive: None,
            }));
        }
        let _ = supervisor_handle.await;
        Ok(())
}
