//! The daemon execution shape: run_daemon is main()'s
//! web_daemon_requested branch (wiring + session supervisor), and
//! run_daemon_loop/DaemonConfig is the fallback daemon loop the
//! headless web-gateway path falls through to after its task ends.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

/// Configuration for `run_daemon_loop`.
pub(crate) struct DaemonConfig {
    pub(crate) bus: EventBus,
    /// `None` = projectless daemon: no default session project; every
    /// CreateSession must carry an explicit `project_root` override.
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) autonomy: SharedAutonomy,
    pub(crate) shared_external_agent:
        Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    pub(crate) shared_codex_config: control_plane::SharedCodexConfig,
    pub(crate) shared_claude_config: control_plane::SharedClaudeConfig,
    pub(crate) frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    pub(crate) session_registry: Option<display::SharedSessionRegistry>,
    pub(crate) peer_registry: Option<peer::PeerRegistry>,
    pub(crate) web_port: Option<u16>,
    pub(crate) flags_direct: bool,
    /// Optional shared session state for headless mode (cleared between tasks).
    pub(crate) shared_session: Option<web_gateway::SharedActiveSession>,
    /// Git-vitals target registry handed to the supervisor (see
    /// `SessionSupervisorConfig::git_vitals_targets`).
    pub(crate) git_vitals_targets: Option<session_vitals::GitVitalsTargets>,
}

/// Daemon loop the headless web-gateway path falls through to after its task ends.
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
        peer_registry: config.peer_registry,
        web_port: config.web_port,
        flags_direct: config.flags_direct,
        shared_session: config.shared_session,
        provider_factory: None,
        logs_home_override: None,
        git_vitals_targets: config.git_vitals_targets,
    })
    .run()
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_daemon(
    flags: &CliFlags,
    project: &Project,
    // The daemon's default project root, `None` when the launch directory
    // has no project marker (projectless — see main's
    // `daemon_project_root`). `project` still supplies config defaults;
    // its `root` field is only the launch cwd and must not be treated as
    // a project here.
    project_root: Option<PathBuf>,
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
    provider_identity: crate::usage_rail::ProviderIdentity,
) -> Result<(), CallerError> {
    // Retention guard for the daemon-global fallback store (projectless
    // staged uploads / transfer jobs): prune entries idle past the
    // retention window so the store cannot grow unbounded across restarts.
    tokio::task::spawn_blocking(global_store::prune_at_daemon_startup);
    let bus = EventBus::new();
    // No tick timer here: `AppEvent::Tick` is consumed only by the stdio
    // MCP event listener (stuck-phase warnings), which daemon mode never
    // runs — the daemon's `/mcp` gateway surface observes state through
    // `spawn_http_observation_listener`, which ignores Tick. A 1 Hz tick
    // would only wake every bus subscriber for nothing.
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
    let _debug_handler = startup::wiring::spawn_debug_handler(&bus, project, web_port, true);

    let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);
    let _outbound_broadcaster =
        event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());
    let _log_sinks = startup::wiring::spawn_log_sinks(&bus, &session_log);

    let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
        startup::wiring::start_project_file_watcher(project_root.as_deref(), &log_dir, &bus);

    let (transcriber, transcriber_err) =
        startup::wiring::build_transcriber(&project.config.transcription);
    if let Some(err) = transcriber_err {
        eprintln!("{}", err);
    }
    let gateway = startup::wiring::spawn_mode_web_gateway(
        flags,
        project,
        project_root.clone(),
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
    )?;
    let shared_session = gateway.shared_session.clone();
    eprintln!("{}", gateway.log_line);

    let shared_codex_config = shared_codex_config_from_project(project);
    let shared_claude_config = shared_claude_config_from_project(project);
    let settings_root = project_root
        .clone()
        .unwrap_or_else(project::daemon_settings_config_root);
    let _control_plane_handle = control_plane::spawn(control_plane::ControlPlaneState {
        autonomy: autonomy.clone(),
        external_agent: shared_external_agent.clone(),
        codex_config: shared_codex_config.clone(),
        claude_config: shared_claude_config.clone(),
        bus: bus.clone(),
        project_root: Some(settings_root),
    });

    // Session vitals: cache/limits are usage-driven and cover every
    // session on any backend, so the producer always runs. The git segment
    // probes the live target registry: seeded with the daemon's primary
    // session when a project root exists, and fed per-session by the
    // supervisor at launch — dashboard-spawned sessions get their dirty /
    // merge-parity / unpushed rows even on a projectless daemon.
    let vitals_git_seed = match (session_log_id(&session_log), project_root.clone()) {
        (Some(session_id), Some(root)) => vec![(session_id, root)],
        _ => Vec::new(),
    };
    let (vitals_git_targets, _vitals_producer) =
        session_vitals::spawn_session_vitals_producer(bus.clone(), vitals_git_seed);
    // Native usage rail: derive per-session UsageSnapshots from
    // ModelResponse events (dashboard meter + cache/limits vitals).
    // Covers supervisor-spawned native children too.
    let _usage_rail = crate::usage_rail::spawn_native_usage_rail(bus.clone(), provider_identity);

    let startup_bus = bus.clone();
    let supervisor_handle =
        session_supervisor::SessionSupervisor::new(session_supervisor::SessionSupervisorConfig {
            bus,
            project_root,
            autonomy,
            shared_external_agent,
            shared_codex_config,
            shared_claude_config,
            frame_registry,
            session_registry: Some(session_registry.clone()),
            peer_registry: Some(gateway.peer_registry.clone()),
            web_port: web_port_for_agent,
            flags_direct: flags.direct,
            shared_session: Some(shared_session),
            provider_factory: None,
            logs_home_override: None,
            git_vitals_targets: Some(vitals_git_targets.clone()),
        })
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
            relationship_kind: None,
            auto_attach: false,
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
