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
        let _session_listeners = startup::wiring::spawn_session_listeners(
            &bus,
            &recording_registry,
            &session_registry,
            &frame_registry,
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
        let _debug_handler =
            startup::wiring::spawn_debug_handler(&bus, project, web_port, true);

        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());
        let _log_sinks = startup::wiring::spawn_log_sinks(&bus, &session_log);

        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            startup::wiring::start_project_file_watcher(&project.root, &log_dir, &bus);

        let (transcriber, transcriber_err) =
            startup::wiring::build_transcriber(&project.config.transcription);
        if let Some(err) = transcriber_err {
            eprintln!("{}", err);
        }
        let gateway = startup::wiring::spawn_mode_web_gateway(
            flags,
            project,
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
            None, // query_ctx: the daemon serves supervised sessions, not one live agent
            None, // active_session_log: supervised children register their own logs
            None, // web_tui_tx: no WebTui under the daemon
        )?;
        let shared_session = gateway.shared_session.clone();
        eprintln!("{}", gateway.log_line);

        let shared_codex_config = shared_codex_config_from_project(project);
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

        // Vitals chips for the daemon's primary session: git state of the
        // project root (statusline port).
        let _vitals_producer = if let Some(session_id) = session_log_id(&session_log) {
            Some(session_vitals::spawn_session_vitals_producer(
                bus.clone(),
                vec![(session_id, project.root.clone())],
            ))
        } else {
            None
        };

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
