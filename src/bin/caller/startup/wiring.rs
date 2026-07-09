//! Per-mode startup wiring, deduplicated. The four mode branches
//! (daemon, mcp_mode, interactive, headless) used to carry
//! near-identical copies of the session listeners, the debug handler,
//! the log sinks, the transcriber construction, the project file
//! watcher, and the ~85-line web-gateway assembly — the audit's 13.1
//! P1 (live-audio transcription bypass) survived in exactly this kind
//! of diverged fifth copy. Each block is built once here; the genuine
//! mode differences ride as parameters:
//!
//! - `query_ctx`: only the foreground (headless) mode has a WebQueryCtx
//!   (agent state for dashboard tool queries).
//! - `active_session_log`: None for the daemon — supervised child
//!   sessions register their own logs; the single-session modes pass
//!   theirs.
//! - the gateway log line is *returned*, not printed — MCP mirrors it
//!   to the session log (stdout is reserved for JSON-RPC), the other
//!   modes print to stderr.
//! - transcriber init errors are returned for the same reason.
//!
//! Deliberately NOT unified (real mode differences): the tick timer
//! (daemon/MCP tick at 1000ms, headless has none), the human-question
//! monitor, outbound-channel sourcing (control-socket reuse), the
//! Windows desktop auto-activation in the daemon, and MCP mode's
//! absence of a control plane.

use crate::*;

/// Handles for the session listeners every mode spawns first:
/// the recording listener, the user-display grant/revoke listener, and
/// the session-list cache invalidator.
/// tokio tasks detach on drop; the struct mirrors the original
/// keep-until-scope-end bindings.
pub(crate) struct SessionListeners {
    pub(crate) _recording_listener: tokio::task::JoinHandle<()>,
    pub(crate) _user_display_listener: tokio::task::JoinHandle<()>,
    pub(crate) _session_list_cache_invalidator: tokio::task::JoinHandle<()>,
}

pub(crate) fn spawn_session_listeners(
    bus: &EventBus,
    recording_registry: &Arc<tokio::sync::RwLock<recording::RecordingRegistry>>,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> SessionListeners {
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
    let _session_list_cache_invalidator = spawn_session_list_cache_invalidator(bus.subscribe());
    SessionListeners {
        _recording_listener,
        _user_display_listener,
        _session_list_cache_invalidator,
    }
}

/// Session-list response caches go stale the moment session membership
/// changes; the daemon emits those changes on the bus, so listen and drop
/// the cached bodies instead of serving ghosts for up to the SWR window
/// (bit the dashboard's own post-create refresh and every parameterless
/// /api/sessions caller).
fn spawn_session_list_cache_invalidator(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(
                    AppEvent::SessionStarted { .. }
                    | AppEvent::SessionEnded { .. }
                    | AppEvent::SessionAttached { .. }
                    | AppEvent::SessionIdentity { .. }
                    | AppEvent::SessionRelationship { .. },
                ) => {
                    crate::web_gateway::invalidate_session_list_response_caches();
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Missed events ⇒ the list may have changed unseen.
                    crate::web_gateway::invalidate_session_list_response_caches();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// The debug-screen handler, spawned only when the web gateway is up
/// (its screenshots are served over the dashboard).
pub(crate) fn spawn_debug_handler(
    bus: &EventBus,
    project: &Project,
    web_port: u16,
    use_web: bool,
) -> Option<tokio::task::JoinHandle<()>> {
    if use_web {
        Some(debug::spawn_debug_screen_handler(
            bus.subscribe(),
            project.config.recording.clone(),
            web_port,
            bus.clone(),
        ))
    } else {
        None
    }
}

/// The two log sinks every mode wires after its outbound broadcaster:
/// the session-log writer (persists bus events that aren't logged
/// inline) and the fission-lifecycle watcher.
pub(crate) struct LogSinks {
    pub(crate) _session_log_writer: tokio::task::JoinHandle<()>,
    pub(crate) _fission_lifecycle_watcher: tokio::task::JoinHandle<()>,
}

pub(crate) fn spawn_log_sinks(bus: &EventBus, session_log: &SharedSessionLog) -> LogSinks {
    LogSinks {
        _session_log_writer: event::spawn_session_log_writer(
            bus.subscribe_session_log(),
            session_log.clone(),
        ),
        _fission_lifecycle_watcher: start_fission_lifecycle(bus, session_log),
    }
}

/// Whisper transcriber from config. Init failure is returned, not
/// printed: the interactive mode logs it through the TUI app (an
/// eprintln! would draw over the alternate screen), the others send
/// it to stderr.
pub(crate) fn build_transcriber(
    cfg: &transcription::TranscriptionConfig,
) -> (
    Option<std::sync::Arc<dyn transcription::Transcriber>>,
    Option<String>,
) {
    if cfg.enabled {
        match transcription::WhisperTranscriber::new(cfg) {
            Ok(t) => (Some(std::sync::Arc::new(t)), None),
            Err(e) => (None, Some(format!("Transcription init failed: {}", e))),
        }
    } else {
        (None, None)
    }
}

/// Project file watcher for rewind snapshots. A projectless daemon
/// (`None`) watches nothing at all, and fallback roots (no .git /
/// intendant.toml — e.g. a service's $HOME WorkingDirectory) must
/// never be baseline-scanned: it blocks boot for minutes and
/// shadow-copies the whole tree.
pub(crate) fn start_project_file_watcher(
    project_root: Option<&Path>,
    log_dir: &Path,
    bus: &EventBus,
) -> (
    Option<file_watcher::SharedFileWatcher>,
    Option<tokio::task::JoinHandle<()>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    let Some(project_root) = project_root else {
        eprintln!(
            "[file_watcher] rewind snapshots off: the daemon has no project — nothing to watch"
        );
        return (None, None, None);
    };
    if !file_watcher::root_is_snapshot_worthy(project_root) {
        eprintln!(
            "[file_watcher] rewind snapshots off: {} is not a project root (no .git or \
             intendant.toml) — start intendant inside a project to enable rewind",
            project_root.display()
        );
        (None, None, None)
    } else {
        let snapshot_dir = log_dir.join("file_snapshots");
        match file_watcher::FileWatcher::new(project_root.to_path_buf(), snapshot_dir, bus.clone())
        {
            Ok(watcher) => {
                let (fw, wh, rh) = watcher.start_shared();
                (Some(fw), Some(wh), Some(rh))
            }
            Err(e) => {
                eprintln!("[file_watcher] Failed to start: {}", e);
                (None, None, None)
            }
        }
    }
}

/// Everything `spawn_mode_web_gateway` returns: the gateway task, the
/// shared active-session state (the daemon hands it to the session
/// supervisor; headless clears it between tasks), and the "listening
/// on" line for the caller to log through its own sink.
pub(crate) struct GatewaySpawn {
    pub(crate) _handle: tokio::task::JoinHandle<()>,
    pub(crate) shared_session: web_gateway::SharedActiveSession,
    pub(crate) log_line: String,
    /// The federated peer registry the gateway serves. Mode runners
    /// thread it into session launches so the native `peer` tool and
    /// the MCP peer tools act on the same registry.
    pub(crate) peer_registry: crate::peer::PeerRegistry,
}

/// Build and spawn the web gateway the way every mode does: gateway
/// config from project config, the ActiveSessionState, the HTTP MCP
/// server, the peer registry + advertise URLs, then the spawn itself.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_mode_web_gateway(
    flags: &CliFlags,
    project: &Project,
    // The mode's project root as served to gateway routes (project-root
    // endpoint, Changes tab, worktree scan, settings persistence). `None`
    // = projectless daemon: routes that need a durable store (staged
    // uploads, transfer jobs) fall back to the daemon-global store
    // (`global_store::StoreScope`); the rest answer honestly ("no
    // project root"). Non-daemon modes pass `Some(project.root)`.
    project_root: Option<PathBuf>,
    autonomy: &SharedAutonomy,
    log_dir: &Path,
    session_log: &SharedSessionLog,
    bus: &EventBus,
    web_listener: &mut Option<tokio::net::TcpListener>,
    web_tls_client_cert_required: bool,
    web_tls_acceptor: &Option<tokio_rustls::TlsAcceptor>,
    web_port: u16,
    web_bind_ip: Option<std::net::IpAddr>,
    runtime_presence_enabled: bool,
    initial_agent_backend: &Option<external_agent::AgentBackend>,
    shared_external_agent: &Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    frame_registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_registry: &display::SharedSessionRegistry,
    recording_registry: &Arc<tokio::sync::RwLock<recording::RecordingRegistry>>,
    shared_file_watcher: &Option<file_watcher::SharedFileWatcher>,
    transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>>,
    broadcast_tx: tokio::sync::broadcast::Sender<String>,
    query_ctx: Option<web_gateway::WebQueryCtx>,
    active_session_log: Option<SharedSessionLog>,
) -> Result<GatewaySpawn, CallerError> {
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
    let shared_session = Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
        daemon_session_id: session_log_id(session_log),
        query_ctx,
        frame_registry: Some(frame_registry.clone()),
        session_log: active_session_log,
        recording_registry: Some(recording_registry.clone()),
        session_registry: Some(session_registry.clone()),
        snapshot_dir: Some(log_dir.join("file_snapshots")),
        project_root_for_changes: project_root.clone(),
        runtime_settings: web_gateway::RuntimeSettingsState {
            external_agent: Some(shared_external_agent.clone()),
            presence_enabled: Some(runtime_presence_enabled),
        },
        file_watcher: shared_file_watcher.clone(),
    }));
    let peer_registry = build_and_hydrate_peer_registry(log_dir, &project.config.peers);
    let mut mcp_http_state = mcp::McpAppState::new(
        "none".into(),
        "none".into(),
        autonomy.clone(),
        log_dir.to_path_buf(),
    );
    mcp_http_state.codex_managed_context =
        project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
    mcp_http_state.configured_codex_managed_context = mcp_http_state.codex_managed_context;
    mcp_http_state.frame_registry = Some(frame_registry.clone());
    mcp_http_state.session_registry = Some(session_registry.clone());
    mcp_http_state.peer_registry = Some(peer_registry.clone());
    mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
    let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new_http(
        Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
        bus.clone(),
    )));
    let advertise_urls = resolve_advertise_urls_from_flags_and_config(flags, project);
    let handle = web_gateway::spawn_web_gateway(
        web_listener
            .take()
            .expect("web listener must exist when use_web"),
        bus.clone(),
        broadcast_tx,
        config,
        shared_session.clone(),
        transcriber,
        None, // task_tx: browser SubmitTask routes via the EventBus → dispatcher path
        project_root,
        mcp_http_server,
        Some(peer_registry.clone()),
        advertise_urls,
        project.config.server.auth.bearer_token.clone(),
        build_local_advertised_auth(
            &project.config.server.auth,
            &access::backend::select_backend().cert_dir(),
        )?,
        web_tls_client_cert_required,
        web_tls_acceptor.clone(),
    );
    Ok(GatewaySpawn {
        _handle: handle,
        shared_session,
        log_line: dashboard_log_line(web_tls_acceptor, web_port, web_bind_ip),
        peer_registry,
    })
}
