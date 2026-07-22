//! The daemon execution shape: `run_daemon` is main()'s
//! `web_daemon_requested` branch (wiring + session supervisor). Foreground
//! web sessions promote their already-subscribed supervisor in place when
//! they fall through to daemon service.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

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
    // Install every shipped builtin skill independently for Agent Skills
    // consumers and Claude Code (marker-owned, no-op when unchanged).
    crate::skill_install::install_global_skills_at_startup();

    // Retention guard for the daemon-global fallback store (projectless
    // staged uploads / transfer jobs): prune entries idle past the
    // retention window so the store cannot grow unbounded across restarts.
    tokio::task::spawn_blocking(global_store::prune_at_daemon_startup);
    // Coordination-space liveness GC (§9 rule-8 liveness amendment):
    // declarations a day past heartbeat, messages past TTL, orphaned
    // atomic-write temps. Never touches checkpoint documents.
    tokio::task::spawn_blocking(crate::coordination::gc::sweep_at_daemon_startup);
    // One-time external-wrapper-index repair (v1 -> v2): recompute each
    // (source, backend session) group's active wrapper from log-dir
    // activity, undoing the inversions written while the session-catalog
    // list scan shared the activating `upsert` (see
    // `external_wrapper_index::migrate_index`). No-op once migrated.
    tokio::task::spawn_blocking(crate::external_wrapper_index::migrate_at_daemon_startup);
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
    let shared_kimi_config = shared_kimi_config_from_project(project);
    let settings_root = project_root
        .clone()
        .unwrap_or_else(project::daemon_settings_config_root);
    let _control_plane_handle = control_plane::spawn(control_plane::ControlPlaneState {
        autonomy: autonomy.clone(),
        external_agent: shared_external_agent.clone(),
        codex_config: shared_codex_config.clone(),
        claude_config: shared_claude_config.clone(),
        kimi_config: shared_kimi_config.clone(),
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
    let (vitals_git_targets, _vitals_producer) = session_vitals::spawn_session_vitals_producer(
        bus.clone(),
        vitals_git_seed,
        Some(crate::session_vitals_restore::account_limit_store_path()),
    );
    // Publish the live registry for read-side lanes: the Changes tab's
    // working-tree list resolves a session's checkout through the SAME
    // effective target the dirty chip probes (activity locus included),
    // so the chip and the tab can never state different checkouts.
    session_vitals::publish_git_vitals_targets(&vitals_git_targets);
    // Restored sessions: a restart empties the target registry, so idle
    // session windows lose their git/health chips until the next resume.
    // One bounded walk (newest-first, insert-if-absent — see
    // restore_session_vitals_at_boot) re-registers the store's non-ended
    // sessions AND hydrates their vitals from disk (recorded launch
    // config + backend transcript tails), off the startup path; the
    // hub's emissions and the first probe tick re-fill each session's
    // chips, and the bootstrap caches carry them to later-connecting
    // dashboards.
    {
        let registry = vitals_git_targets.clone();
        let restore_bus = bus.clone();
        tokio::task::spawn_blocking(move || {
            let (restored, hydrated) =
                crate::session_vitals_restore::restore_session_vitals_at_boot(
                    &crate::platform::home_dir(),
                    &registry,
                    &restore_bus,
                );
            if restored > 0 || hydrated > 0 {
                eprintln!(
                    "Session vitals: git targets restored for {restored} session(s), vitals hydrated for {hydrated}"
                );
            }
        });
    }
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
            shared_kimi_config,
            frame_registry,
            session_registry: Some(session_registry.clone()),
            peer_registry: Some(gateway.peer_registry.clone()),
            web_port: web_port_for_agent,
            flags_direct: flags.direct,
            shared_session: Some(shared_session),
            provider_factory: None,
            logs_home_override: None,
            git_vitals_targets: Some(vitals_git_targets.clone()),
            hosted_control_cert_dir: Some(crate::startup::installed_access_cert_dir()),
            launch_gate_for_tests: None,
            agenda: gateway.agenda.clone(),
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
