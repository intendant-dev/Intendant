//! The gateway listener: TLS accept + failure rate-limiting, dead-listener
//! rebind, and `spawn_web_gateway` — the accept loop and per-connection
//! demux/upgrade head. Each connection's tail work lives in named tasks:
//! websocket sessions run [`ws_inbound_task`] / [`ws_outbound_task`]
//! (`ws_session.rs`) and plain HTTP requests run [`serve_http_request`]
//! (`http_dispatch.rs`), each handed a per-connection context struct built
//! here at the spawn/call site.

use super::*;

pub(crate) const TLS_FAILURE_LOG_INTERVAL_SECS: u64 = 30;

#[derive(Debug)]
pub(crate) struct TlsFailureLogEntry {
    last_logged: std::time::Instant,
    suppressed: u64,
}

pub(crate) type TlsFailureLogState = Arc<Mutex<HashMap<String, TlsFailureLogEntry>>>;

pub(crate) fn log_tls_failure_rate_limited(
    state: &TlsFailureLogState,
    peer: &str,
    kind: &str,
    detail: &str,
) {
    let now = std::time::Instant::now();
    let key = format!("{kind}|{peer}|{detail}");
    let mut map = state.lock().unwrap_or_else(|e| e.into_inner());
    match map.get_mut(&key) {
        Some(entry)
            if now.duration_since(entry.last_logged)
                < std::time::Duration::from_secs(TLS_FAILURE_LOG_INTERVAL_SECS) =>
        {
            entry.suppressed = entry.suppressed.saturating_add(1);
        }
        Some(entry) => {
            let suppressed = entry.suppressed;
            entry.last_logged = now;
            entry.suppressed = 0;
            drop(map);
            if suppressed > 0 {
                eprintln!(
                    "[web_gateway] {kind} from {peer}: {detail} \
                     (suppressed {suppressed} repeats in the last {TLS_FAILURE_LOG_INTERVAL_SECS}s)"
                );
            } else {
                eprintln!("[web_gateway] {kind} from {peer}: {detail}");
            }
        }
        None => {
            map.insert(
                key,
                TlsFailureLogEntry {
                    last_logged: now,
                    suppressed: 0,
                },
            );
            drop(map);
            eprintln!("[web_gateway] {kind} from {peer}: {detail}");
        }
    }
}

/// Apply a dashboard-control authority request only while its display is
/// present in the live registry. The active-session and registry read guards
/// stay held through the synchronous authority mutation, so a concurrent
/// session replacement or display removal cannot clear the display and then
/// lose a race to a stale grant. `try_read` keeps this synchronous bridge
/// non-blocking; contention rejects the request closed and the user can retry.
fn apply_dashboard_grant_for_existing_display(
    shared_session: &SharedActiveSession,
    display_id: u32,
    session_id: &str,
    authority: &Arc<DisplayInputAuthority>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let Ok(session) = shared_session.try_read() else {
        return false;
    };
    let Some(session_registry) = session.session_registry.as_ref() else {
        return false;
    };
    let Ok(registry) = session_registry.try_read() else {
        return false;
    };
    if registry.get_any(display_id).is_none() {
        return false;
    }
    apply_grant_input_authority_dashboard_control(
        display_id,
        session_id.to_string(),
        authority,
        authority_change_tx,
    );
    true
}

#[cfg(test)]
mod authority_grant_validation_tests {
    use super::*;

    #[test]
    fn nonexistent_dashboard_authority_requests_do_not_grow_global_maps() {
        let session_registry = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let shared_session = ActiveSessionState::empty();
        shared_session
            .try_write()
            .expect("fresh active session is uncontended")
            .session_registry = Some(session_registry);
        let authority = Arc::new(DisplayInputAuthority::default());
        let (change_tx, mut change_rx) = broadcast::channel(8);

        for display_id in 1..=2_048 {
            assert!(!apply_dashboard_grant_for_existing_display(
                &shared_session,
                display_id,
                "dashboard-session",
                &authority,
                &change_tx,
            ));
        }

        assert_eq!(authority.tracked_entry_counts(), (0, 0));
        assert!(
            change_rx.try_recv().is_err(),
            "rejected requests must not publish authority changes"
        );
    }
}

// Exact fork baselines are a synchronous `/api/sessions` refinement. The scanner
// below parses compact Codex token lines without materializing full JSON values.
// Exact fork baselines come from scanning the parent's log (results
// persist per file-state in the codex-parent-baseline namespace, so each
// scan happens once). The per-file cap covers the largest observed
// rollouts; parents past the per-build budget pick up their baseline on a
// later list pass.

pub(crate) use intendant_core::net::{
    rebind_dead_tcp_listener, should_continue_after_accept_error, FATAL_ACCEPT_REBIND_THRESHOLD,
};

#[cfg(not(test))]
fn default_access_cert_dir() -> std::path::PathBuf {
    crate::access::backend::select_backend().cert_dir()
}

/// Unit-test callers historically reached the production wrapper and thereby
/// read the runner's live access store. Keep those transport tests isolated by
/// giving every gateway its own process-lifetime temp store. Tests that need a
/// populated store use `spawn_web_gateway_from_cert_dir` explicitly.
#[cfg(test)]
fn default_access_cert_dir() -> std::path::PathBuf {
    static TEST_STORES: std::sync::OnceLock<std::sync::Mutex<Vec<tempfile::TempDir>>> =
        std::sync::OnceLock::new();
    let store = tempfile::tempdir().expect("create isolated gateway access store");
    let path = store.path().to_path_buf();
    TEST_STORES
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(store);
    path
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub fn spawn_web_gateway(
    listener: TcpListener,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: WebGatewayConfig,
    shared_session: SharedActiveSession,
    transcriber: Option<Arc<dyn crate::transcription::Transcriber>>,
    task_tx: Option<tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>>,
    project_root: Option<std::path::PathBuf>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    peer_registry: Option<crate::peer::PeerRegistry>,
    advertise_urls: Vec<String>,
    inbound_bearer_token: Option<String>,
    local_card_auth: crate::peer::AuthRequirements,
    tls_client_cert_required: bool,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
) -> tokio::task::JoinHandle<()> {
    spawn_web_gateway_from_cert_dir(
        listener,
        bus,
        broadcast_tx,
        config,
        shared_session,
        transcriber,
        task_tx,
        project_root,
        mcp_server,
        peer_registry,
        advertise_urls,
        inbound_bearer_token,
        local_card_auth,
        tls_client_cert_required,
        tls_acceptor,
        default_access_cert_dir(),
    )
}

/// Spawn a gateway against an explicit access-certificate store.
///
/// Production resolves the installed platform store in [`spawn_web_gateway`].
/// Keeping the path explicit below that transport edge makes request IAM and
/// peer-identity resolution testable without reading the runner's real home.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_web_gateway_from_cert_dir(
    listener: TcpListener,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: WebGatewayConfig,
    shared_session: SharedActiveSession,
    transcriber: Option<Arc<dyn crate::transcription::Transcriber>>,
    task_tx: Option<tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>>,
    project_root: Option<std::path::PathBuf>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    peer_registry: Option<crate::peer::PeerRegistry>,
    advertise_urls: Vec<String>,
    // Inbound bearer token enforcement. When `Some`, federation REST
    // endpoints (/api/peers*, /api/coordinator/*, /api/sessions)
    // require `Authorization: Bearer <token>` matching the configured
    // value; missing or wrong token returns 401. When `None`, no
    // application-layer auth is enforced — the operator's expected to
    // rely on transport security (mTLS proxy, tailnet, loopback).
    // Sourced from `[server.auth] bearer_token` in intendant.toml.
    //
    // /.well-known/agent-card.json, /config, the dashboard HTML, and static
    // assets are intentionally exempt. /ws is enforced separately below:
    // daemons use Authorization, browsers use a ?token= query parameter, and
    // every browser Origin must be the daemon's own or the signed app scheme.
    inbound_bearer_token: Option<String>,
    // What to advertise in the local Agent Card's `auth` field —
    // tells connecting peers what wire-layer (transport) and
    // application-layer (bearer) auth they need to satisfy.
    // Built by `crate::main::build_local_advertised_auth` from
    // `[server.auth] advertised_transport` (`"none"` /
    // `"mutual-tls"` / `"pin-self-cert"`) and
    // `[server.auth] bearer_token`. The `pin-self-cert` path reads
    // the daemon's own `server.crt` from the access cert dir and
    // pre-fills the fingerprint so operators don't have to compute
    // it manually.
    //
    // Test call sites pass `AuthRequirements::none()` since they
    // don't exercise the advertise path; production call sites in
    // main.rs build the requirements from the project config.
    local_card_auth: crate::peer::AuthRequirements,
    // When true, the TLS layer may complete without a client certificate so
    // authority-free shell/discovery bytes and public access-request doors
    // remain reachable. Protected HTTP and every WS path still require a
    // rustls-verified client certificate (or authenticated peer identity).
    tls_client_cert_required: bool,
    // Native TLS for the dashboard. `Some(acceptor)` (built in main.rs
    // from `[server.tls]` / `--tls`) makes the per-connection demux wrap
    // any connection whose first bytes are a TLS ClientHello, serving the
    // dashboard over HTTPS/WSS. `None` (the default) preserves the
    // current plain-HTTP behavior. Either way the raw ICE-TCP (STUN-
    // framed) demux branch is unaffected — TLS is distinguished by its
    // `0x16` handshake-record first byte, which is disjoint from both the
    // STUN length-prefix and HTTP method bytes.
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    access_cert_dir: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());
    let peer_access_request_config = config.peer_access_requests.clone();
    // Cache the most recent worktree inventory scan. Scanning can walk
    // large worktree directories for disk-size accounting, so the
    // dashboard explicitly triggers refreshes instead of doing it on
    // every GET. Shared by HTTP and the dashboard WebRTC control tunnel.
    let worktree_inventory_cache: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Build the local Agent Card from live runtime state so
    // `/.well-known/agent-card.json` can serve it. The transport URLs
    // come from [`resolve_advertise_urls`], which uses operator
    // overrides verbatim when provided and otherwise falls back to a
    // single auto-detected URL derived from the listener's bind
    // address. Multiple URLs let one daemon advertise itself reachable
    // via several paths (LAN IP, Tailscale, host port-forward, etc.)
    // — the connecting peer probes them in order.
    //
    // TLS state drives the advertised scheme: when a TLS acceptor is
    // present the dashboard is HTTPS/WSS-only (strict demux below), so
    // auto-detected URLs must be `wss://`, not `ws://`, or peers handed a
    // `ws://` URL would be refused.
    let advertise_urls = resolve_advertise_urls(
        listener.local_addr().ok(),
        &advertise_urls,
        tls_acceptor.is_some(),
    );
    let agent_card = build_local_agent_card(advertise_urls, local_card_auth);
    let mut agent_card_value =
        serde_json::to_value(&agent_card).unwrap_or_else(|_| serde_json::json!({}));
    // Phase 7: the signed card names the rendezvous this daemon actually
    // polls, so browsers learn the signaling base from the daemon record
    // instead of assuming the default hosted instance.
    if config.connect.enabled {
        if let Some(base) = config
            .connect
            .rendezvous_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            agent_card_value["rendezvous_base"] =
                serde_json::Value::String(base.trim_end_matches('/').to_string());
            if let Some(id) = config
                .connect
                .daemon_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                agent_card_value["connect_daemon_id"] = serde_json::Value::String(id.to_string());
            }
        }
    }
    let agent_card_json =
        serde_json::to_string(&agent_card_value).unwrap_or_else(|_| "{}".to_string());
    let agent_card_value_for_targets = agent_card_value.clone();
    let bootstrap_caches = crate::dashboard_control::DashboardBootstrapCaches::default();

    // Warm the session list in the background so the first dashboard
    // request doesn't pay the initial scan. The persistent per-session
    // index makes this mostly stat calls + one parallel index sweep; the
    // results land in the ordinary response caches.
    //
    // Not under test: unit tests spawn hundreds of gateways, and the warm
    // scan walks the REAL ~/.intendant of whoever runs the tests — on a
    // dev box with a long session history that is a stat storm per test
    // (the dominant cost of the whole suite before this gate) and a
    // hermeticity leak. Tests that exercise session lists inject their
    // own home via list_sessions_from_home.
    #[cfg(not(test))]
    tokio::task::spawn_blocking(|| {
        preload_session_index();
        // 600 matches the dashboard's default recent-list request size;
        // the unlimited list feeds the Stats tab's usage view. Sequential
        // so the second scan reuses everything the first one warmed.
        let _ = cached_list_sessions_with_limit(SESSION_LIST_STREAM_QUICK_LIMIT);
        let _ = cached_list_sessions();
    });

    // Pre-build ICE config for WebRTC display sessions from the gateway config.
    let ice_config = crate::display::IceConfig {
        ice_servers: config.ice_servers.clone(),
    };

    // Shared ICE-TCP peer registry + advertised TCP port.
    //
    // We multiplex ICE-TCP onto the HTTP listener port: the per-connection
    // accept handler (later in this function) peeks every accepted TCP
    // connection's first bytes to tell HTTP vs. WebSocket vs. STUN-framed
    // traffic apart. STUN traffic is read through one RFC 4571 frame and
    // handed to this registry, which demuxes to the matching peer by the
    // STUN USERNAME's local-ufrag half. The advertised TCP candidate port
    // is the HTTP port itself, so ICE-TCP flows through the exact same
    // tunnel/port-forward that already carries the dashboard — users
    // don't configure anything extra beyond what the dashboard already
    // requires.
    let http_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let tcp_peer_registry = crate::display::webrtc::TcpPeerRegistry::new();
    let tcp_advertised_port: Option<u16> = if http_port != 0 {
        Some(http_port)
    } else {
        None
    };
    let peer_file_transfer_registry =
        Arc::new(crate::peer_file_transfer::PeerFileTransferRegistry::new(
            ice_config.clone(),
            bus.clone(),
            Arc::clone(&tcp_peer_registry),
        ));

    // Slice 3b: TCP relay registry for primary-as-media-relay. When
    // a federated WebRTC `Answer` flows from a peer back to the
    // browser, the translator (below) extracts the peer's ICE ufrag
    // from the SDP, resolves the peer's outbound TCP address, and
    // registers the mapping here. The accept loop (below) then
    // dispatches incoming STUN-framed TCP connections whose ufrag
    // matches an entry to the relay byte-forwarding path instead of
    // the local WebRtcPeer path — the primary opens a fresh TCP
    // connection to the peer and shuttles bytes between browser and
    // peer until either side closes. Browser ICE treats this as a
    // TCP candidate alongside the peer's direct candidate; direct
    // wins on reachable topologies, relay covers the browser-can-
    // only-reach-primary case (e.g. hypervisor-isolated VMs).
    let tcp_relay_registry = crate::display::webrtc::TcpRelayRegistry::new();

    // Primary's relay TCP URL, used to inject a relay candidate into
    // forwarded `Answer` SDPs. Derived from the agent card's first
    // IntendantWs transport — that's the URL the primary advertises
    // to peers, which on most deployments is also what browsers use
    // to reach the primary. Stored as a string so DNS resolution
    // happens lazily at per-Answer rewrite time rather than once at
    // startup (hostnames may not resolve at boot for Tailscale /
    // mDNS / etc).
    let relay_advertise_url: Option<String> = agent_card.transports.iter().find_map(|t| match t {
        crate::peer::TransportSpec::IntendantWs { url } => Some(url.clone()),
        _ => None,
    });

    // Inject content-hash version into WASM/JS URLs for cache-busting.
    let v = asset_version().to_string();
    let session_provider = config.provider.clone();
    let session_model = config.model.clone();
    let voice_debug = Arc::new(Mutex::new(VoiceDebugState::default()));
    let active_presence: Arc<Mutex<Option<ActivePresence>>> = Arc::new(Mutex::new(None));
    // Per-display input authority (phase 5).  Entry absence = unclaimed
    // (any connection can input — pre-phase-5 default); entry presence =
    // exclusive ownership by that one `connection_id`.
    //
    // Synchronous `StdRwLock` (5a.1): the WebRTC data-channel input
    // handler in `display/mod.rs::handle_offer_pool_mode` is an
    // `Arc<dyn Fn(InputEvent) + Send + Sync>` invoked from rtc's sync
    // receive context, and reads this map through the per-peer
    // `input_authorized` closure each time an event arrives.  Tokio's
    // RwLock can't be read from sync code without `block_on`; std's
    // can.  The map is small, write-rare (grant/release/WS-close only),
    // read-frequent on the hot input path; std::sync::RwLock is the
    // correct lock here.
    let display_input_authority = Arc::new(DisplayInputAuthority::default());

    // Tab presence (Access pane): live connections on both event lanes,
    // with voice/input ownership joined from the two handles above at
    // query time. The /ws lane registers below at accept; the control
    // tunnel registers inside DashboardControlRegistry's answer/close.
    let dashboard_tabs =
        DashboardTabsRegistry::new(active_presence.clone(), display_input_authority.clone());

    // Phase 5a.1 authority transition channel.  Each per-connection
    // outbound task subscribes; emit sites are the Request/Release
    // ControlMsg handlers, transport cleanup, and the synchronous display
    // registry lifecycle observer installed before the accept loop starts.
    let (authority_change_tx, _authority_change_rx0) =
        broadcast::channel::<DisplayInputAuthorityChange>(AUTHORITY_CHANGE_CAPACITY);

    let (dashboard_authority_change_tx, _dashboard_authority_change_rx0) =
        broadcast::channel::<u32>(AUTHORITY_CHANGE_CAPACITY);
    {
        let mut authority_change_rx = authority_change_tx.subscribe();
        let dashboard_authority_change_tx = dashboard_authority_change_tx.clone();
        let authority_shared_session = shared_session.clone();
        let observer_authority = Arc::clone(&display_input_authority);
        tokio::spawn(async move {
            let mut last_holders: HashMap<u32, DisplayInputHolder> = HashMap::new();
            loop {
                match authority_change_rx.recv().await {
                    Ok(change) => {
                        // Mutation and broadcast are intentionally separated so
                        // no channel send happens under the hot holder lock.
                        // A concurrent newer mutation can therefore broadcast
                        // first; never regress derived state to its stale event.
                        if !change.is_current(&observer_authority) {
                            continue;
                        }
                        let identity_changed =
                            match (last_holders.get(&change.display_id), change.holder.as_ref()) {
                                (Some(previous), Some(current)) => !previous.same_identity(current),
                                (None, None) => false,
                                _ => true,
                            };
                        match change.holder.as_ref() {
                            Some(holder) => {
                                last_holders.insert(change.display_id, holder.clone());
                            }
                            None => {
                                last_holders.remove(&change.display_id);
                            }
                        }
                        // Every holder transition advances the display input
                        // epoch and clears its backlog, even when no native
                        // edge is held yet. Otherwise a queued A event could
                        // survive a fast A -> B -> A sequence and become true
                        // under A's identity-only guard again.
                        if identity_changed {
                            let session_registry = authority_shared_session
                                .read()
                                .await
                                .session_registry
                                .clone();
                            if let Some(session_registry) = session_registry {
                                if let Some(session) =
                                    session_registry.read().await.get_any(change.display_id)
                                {
                                    session.reset_browser_input_before_authority_revision(
                                        change.revision,
                                        "display input authority changed",
                                    );
                                }
                            }
                        }
                        let _ = dashboard_authority_change_tx.send(change.display_id);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        last_holders.clear();
                        let session_registry = authority_shared_session
                            .read()
                            .await
                            .session_registry
                            .clone();
                        if let Some(session_registry) = session_registry {
                            let sessions = {
                                let registry = session_registry.read().await;
                                registry
                                    .all_display_ids()
                                    .into_iter()
                                    .filter_map(|display_id| {
                                        registry
                                            .get_any(display_id)
                                            .map(|session| (display_id, session))
                                    })
                                    .collect::<Vec<_>>()
                            };
                            for (display_id, session) in sessions {
                                let revision = observer_authority
                                    .revision(display_id)
                                    .load(Ordering::SeqCst);
                                session.reset_browser_input_before_authority_revision(
                                    revision,
                                    "display input authority updates were lost",
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    let dashboard_display_authority = {
        let snapshot_authority = Arc::clone(&display_input_authority);
        let state_authority = Arc::clone(&display_input_authority);
        let request_authority = Arc::clone(&display_input_authority);
        let request_change_tx = authority_change_tx.clone();
        let request_shared_session = shared_session.clone();
        let release_authority = Arc::clone(&display_input_authority);
        let release_change_tx = authority_change_tx.clone();
        let input_authority = Arc::clone(&display_input_authority);
        let input_revision_authority = Arc::clone(&display_input_authority);
        let cleanup_authority = Arc::clone(&display_input_authority);
        let cleanup_change_tx = authority_change_tx.clone();
        let subscribe_tx = dashboard_authority_change_tx.clone();
        crate::dashboard_control::DashboardDisplayAuthorityBridge::new(
            move |session_id, display_ids| {
                dashboard_control_authority_snapshot_frames(
                    session_id,
                    display_ids,
                    &snapshot_authority,
                )
            },
            move |session_id, display_id| {
                Some(dashboard_control_authority_state_frame(
                    session_id,
                    display_id,
                    &state_authority,
                ))
            },
            move |session_id, display_id| {
                if !apply_dashboard_grant_for_existing_display(
                    &request_shared_session,
                    display_id,
                    session_id,
                    &request_authority,
                    &request_change_tx,
                ) {
                    return Vec::new();
                }
                vec![dashboard_control_authority_state_frame(
                    session_id,
                    display_id,
                    &request_authority,
                )]
            },
            move |session_id, display_id| {
                apply_release_input_authority_dashboard_control(
                    display_id,
                    session_id,
                    &release_authority,
                    &release_change_tx,
                );
                vec![dashboard_control_authority_state_frame(
                    session_id,
                    display_id,
                    &release_authority,
                )]
            },
            move |session_id, display_id| {
                dashboard_control_input_authorized(session_id, display_id, &input_authority)
            },
            move |display_id| input_revision_authority.revision(display_id),
            move |session_id| {
                apply_dashboard_control_close_input_authority(
                    session_id,
                    &cleanup_authority,
                    &cleanup_change_tx,
                );
            },
            move || subscribe_tx.subscribe(),
        )
    };

    // Process-wide registry of standalone shell PTY sessions, keyed by
    // (host_id, terminal_id). Lives as long as the web gateway task and
    // is cloned into each per-connection handler so reconnects reattach
    // to existing shells. Keyed on host_id even though there's only one
    // host today so multi-host phase 1 can add siblings without refactor.
    let terminal_registry: Arc<crate::terminal::TerminalRegistry> = Arc::new(
        crate::terminal::TerminalRegistry::new(project_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })),
    );

    let dashboard_presence = {
        let connect_active_presence = active_presence.clone();
        let connect_voice_debug = voice_debug.clone();
        let connect_shared_session = shared_session.clone();
        let connect_bus = bus.clone();
        let connect_provider = config.provider.clone();
        let connect_model = config.model.clone();

        let disconnect_active_presence = active_presence.clone();
        let disconnect_voice_debug = voice_debug.clone();
        let disconnect_shared_session = shared_session.clone();
        let disconnect_bus = bus.clone();

        let make_active_presence = active_presence.clone();
        let make_voice_debug = voice_debug.clone();
        let make_shared_session = shared_session.clone();
        let make_bus = bus.clone();
        let make_provider = config.provider.clone();
        let make_model = config.model.clone();

        let cleanup_active_presence = active_presence.clone();
        let cleanup_voice_debug = voice_debug.clone();
        let cleanup_shared_session = shared_session.clone();
        let cleanup_bus = bus.clone();

        let log_voice_debug = voice_debug.clone();

        crate::dashboard_control::DashboardPresenceBridge::new(
            move |request| {
                Box::pin(dashboard_control_presence_connect(
                    request,
                    connect_active_presence.clone(),
                    connect_voice_debug.clone(),
                    connect_shared_session.clone(),
                    connect_bus.clone(),
                    connect_provider.clone(),
                    connect_model.clone(),
                ))
            },
            move |request| {
                Box::pin(dashboard_control_presence_disconnect(
                    request,
                    disconnect_active_presence.clone(),
                    disconnect_voice_debug.clone(),
                    disconnect_shared_session.clone(),
                    disconnect_bus.clone(),
                ))
            },
            move |request| {
                Box::pin(dashboard_control_presence_make_active(
                    request,
                    make_active_presence.clone(),
                    make_voice_debug.clone(),
                    make_shared_session.clone(),
                    make_bus.clone(),
                    make_provider.clone(),
                    make_model.clone(),
                ))
            },
            move |session_id| {
                Box::pin(dashboard_control_presence_cleanup(
                    session_id,
                    cleanup_active_presence.clone(),
                    cleanup_voice_debug.clone(),
                    cleanup_shared_session.clone(),
                    cleanup_bus.clone(),
                ))
            },
            move |text| {
                let mut vd = log_voice_debug.lock().unwrap_or_else(|e| e.into_inner());
                vd.voice_log_count += 1;
                vd.last_voice_log = text;
            },
        )
    };

    let dashboard_control = Arc::new(crate::dashboard_control::DashboardControlRegistry::new(
        config.clone(),
        broadcast_tx.clone(),
        bus.clone(),
        peer_registry.clone(),
        mcp_server.clone(),
        shared_session.clone(),
        project_root.clone(),
        worktree_inventory_cache.clone(),
        terminal_registry.clone(),
        task_tx.clone(),
        agent_card_value,
        bootstrap_caches.clone(),
        Some(dashboard_display_authority),
        Some(dashboard_presence),
        ice_config.clone(),
        Arc::clone(&tcp_peer_registry),
        dashboard_tabs.clone(),
    ));
    crate::connect_rendezvous::spawn_connect_rendezvous_client(
        config.connect.clone(),
        dashboard_control.clone(),
        tcp_advertised_port,
    );
    // Pending-request attention nudges: watch approvals/questions on the bus
    // and ping the Connect rendezvous when they age with no dashboard around.
    crate::attention_nudge::spawn_attention_nudge_monitor(bus.clone());
    // An owner surface (dashboards on this gateway) now exists in this
    // process: display requests may block on the popup instead of failing
    // closed with the headless no-approver refusal.
    crate::display_requests::mark_approver_surface_available();
    // Fleet certificates: restore any stored certificate into the live
    // SNI resolver and keep it renewed (fleet_cert.rs).
    crate::fleet_cert::refresh_installed_state_in(&access_cert_dir);
    crate::fleet_cert::spawn_renewal_loop();
    // Hosted-bundle code transparency: when Connect is enabled,
    // periodically verify what the rendezvous serves against its public
    // transparency log (hosted_verify.rs — advisory and fail-open, the
    // CT tripwire's sibling).
    crate::hosted_verify::spawn_hosted_bundle_monitor();

    // F-1.3b3 federated authority subscribers. Federated counterpart
    // to local 5c's per-WS subscriber loop: federated browsers don't
    // share the local 5c WS path, so the gateway needs an explicit
    // registry of `(federation_connection_id, session_id, display_id)`
    // → `WebRtcPeer` to fan personalized state out to. Owned here at
    // gateway scope so cleanup edges (federated `Close`, federation
    // WS close) can locate entries by either single-identity or
    // bulk-by-connection key. See the F-1.3b3 helpers above.
    let federated_authority_subscribers: FederatedAuthoritySubscribers =
        Arc::new(StdRwLock::new(HashMap::new()));

    // Cache the latest usage_update JSON so late-connecting browsers get it
    // without sending ControlMsg (which would pollute the event log).
    let last_usage_json = bootstrap_caches.last_usage_json.clone();
    // Cache the latest live_usage_update JSON for late-connecting browsers.
    let last_live_usage_json = bootstrap_caches.last_live_usage_json.clone();
    // Cache the latest status event (has autonomy, session_id, task).
    let last_status_json = bootstrap_caches.last_status_json.clone();
    // Cache standalone autonomy changes so reconnecting dashboards do not
    // fall back to the stale autonomy value in the latest status event.
    let last_autonomy_json = bootstrap_caches.last_autonomy_json.clone();
    // Cache the latest external_agent_changed event so a refreshed
    // browser learns the current value without having to re-fetch
    // settings. Without this the dashboard dropdown snaps back to
    // "None (internal agent)" on every page refresh even though the
    // daemon still has the value in memory.
    let last_external_agent_json = bootstrap_caches.last_external_agent_json.clone();
    // Cache all currently externally-attached sessions so refreshed browsers
    // can rehydrate every open Activity window with the same compact
    // transcript shown in the Sessions tab. This must be a set, not "last
    // attached", because multiple Codex/Claude/Gemini session windows may be
    // open at once.
    let attached_external_sessions = bootstrap_caches.attached_external_sessions.clone();
    // Cache the latest user_display_granted event. The authoritative
    // state lives in AutonomyState.user_display_granted on the server,
    // but the dashboard only learns about it via the broadcast; without
    // this cache a refreshed browser shows "off" regardless of whether
    // the user has actually granted access. Cleared on user_display_revoked
    // so a stale grant doesn't get replayed after the user revokes.
    let last_user_display_json = bootstrap_caches.last_user_display_json.clone();
    // Cache the latest change-detected per-session state (session_vitals /
    // session_goal). Those emit only on change — an idle session never
    // repeats them — so late joiners (browser refresh on an idle daemon, a
    // peer transport attaching) would otherwise never see state that last
    // changed before they connected. Pruned on session_ended.
    let session_state_lines = bootstrap_caches.session_state_lines.clone();
    // Cache display_ready JSON per display_id for late-connecting browsers.
    // Using a HashMap so multiple concurrent display sessions are all replayed.
    let display_ready_cache: Arc<Mutex<HashMap<u32, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // Bootstrap-cache maintenance rides the TYPED bus (4096-slot), not the
    // 256-slot serialized channel it used to sniff with `line.contains`
    // probes: a lag on the old channel silently poisoned every future
    // tab's bootstrap (ghost approvals, ghost sessions). The typed
    // maintainer matches AppEvent variants, serializes only the relevant
    // ones through the same canonical converter the broadcaster uses, and
    // on Lagged clears the affected sections so the next bootstrap omits
    // stale lines instead of replaying ghosts (see
    // `BootstrapCacheMaintainer::clear_on_gap` for the per-section
    // ground-truth story). Authority is tied synchronously to the session
    // registry; this maintainer only clears it as fail-closed gap recovery.
    spawn_bootstrap_cache_maintainer(
        &bus,
        BootstrapCacheMaintainer {
            caches: bootstrap_caches.clone(),
            display_ready_cache: display_ready_cache.clone(),
            display_input_authority: Arc::clone(&display_input_authority),
            authority_change_tx: authority_change_tx.clone(),
        },
    );

    // Peer registry → dashboard push translator.
    //
    // When the registry is wired (the daemon was started with
    // federation enabled), subscribe to its [`RegistryEvent`] stream
    // and translate each event into the matching wire-format
    // [`OutboundEvent`] variant, broadcast over the same channel as
    // every other dashboard event. The browser's existing primary
    // WebSocket pipeline picks them up and updates peer rows in-place
    // without polling `GET /api/peers`.
    //
    // Lagged events are skipped on purpose: the dashboard's recovery
    // path is to re-fetch `/api/peers`, which always returns ground
    // truth. Closed receiver = registry was dropped, exit cleanly.
    if let Some(reg) = peer_registry.as_ref() {
        let mut reg_rx = reg.subscribe();
        let push_tx = broadcast_tx.clone();
        let reg_for_task = reg.clone();
        let relay_registry_for_task = Arc::clone(&tcp_relay_registry);
        let relay_url_for_task = relay_advertise_url.clone();
        let bus_for_task = bus.clone();
        tokio::spawn(async move {
            loop {
                match reg_rx.recv().await {
                    Ok(event) => {
                        let outbound = match event {
                            crate::peer::RegistryEvent::PeerAdded(snap) => {
                                crate::types::OutboundEvent::PeerAdded { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerRemoved(id) => {
                                crate::types::OutboundEvent::PeerRemoved {
                                    id: id.as_str().to_string(),
                                }
                            }
                            crate::peer::RegistryEvent::PeerStateChanged(snap) => {
                                crate::types::OutboundEvent::PeerStateChanged { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerEventForwarded { peer, event } => {
                                // Slice 3b: when a federated Answer
                                // comes back toward the browser, rewrite
                                // the SDP to inject a TCP candidate
                                // pointing at the primary's own relay
                                // address, and register the peer's ufrag
                                // in the relay registry so incoming
                                // browser TCP connections with that
                                // ufrag get forwarded to the peer. Other
                                // event variants pass through verbatim.
                                let rewritten_event = maybe_rewrite_federated_answer(
                                    &peer,
                                    *event,
                                    &reg_for_task,
                                    &relay_registry_for_task,
                                    relay_url_for_task.as_deref(),
                                    &bus_for_task,
                                )
                                .await;
                                crate::types::OutboundEvent::PeerEventForwarded {
                                    peer_id: peer.as_str().to_string(),
                                    payload: rewritten_event,
                                }
                            }
                        };
                        crate::control::broadcast_event(&push_tx, &outbound);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let app_html = Arc::new(rewrite_app_html_asset_urls(APP_HTML.to_string(), &v));
    // INTENDANT_APP_HTML_PATH (dev override): read once at spawn; when
    // set, every dashboard request re-reads that path instead of serving
    // the embedded `app_html` above.
    let app_html_override: Option<Arc<std::path::Path>> = app_html_override_path().map(Arc::from);
    if let Some(path) = &app_html_override {
        eprintln!(
            "[web_gateway] INTENDANT_APP_HTML_PATH: serving the dashboard from {} \
             (re-read per request; the embedded copy is ignored)",
            path.display()
        );
        if let Err(err) = std::fs::metadata(path) {
            eprintln!(
                "[web_gateway] WARNING: INTENDANT_APP_HTML_PATH is not readable right now: {err}"
            );
        }
    }
    // Lazily computed (ETag token, gzipped body) for the rewritten
    // app.html — once per gateway spawn, on the first page load. The
    // rewritten HTML is gateway-scoped (unlike the `include_*!` constants
    // behind `embedded_static_asset`), so its cache lives here.
    let app_html_cache: Arc<OnceLock<(String, Vec<u8>)>> = Arc::new(OnceLock::new());
    let tls_failure_log_state: TlsFailureLogState = Arc::new(Mutex::new(HashMap::new()));
    let lifecycle_shared_session = shared_session.clone();
    let lifecycle_authority = Arc::clone(&display_input_authority);
    let lifecycle_authority_change_tx = authority_change_tx.clone();

    tokio::spawn(async move {
        // Install the authority invalidator before accepting any browser.
        // SessionRegistry invokes it synchronously under its write guard and
        // before insert/remove publication, closing the display-ID reuse race
        // that an asynchronous DisplayReady/CaptureLost subscriber cannot.
        if let Some(session_registry) = lifecycle_shared_session
            .read()
            .await
            .session_registry
            .clone()
        {
            session_registry
                .write()
                .await
                .set_lifecycle_observer(Some(Arc::new(move |display_id| {
                    let (_, revision) = lifecycle_authority.clear_display(display_id);
                    let _ = lifecycle_authority_change_tx.send(DisplayInputAuthorityChange {
                        display_id,
                        holder: None,
                        revision,
                    });
                })));
        }

        let mut listener = listener;
        let bind_addr = listener.local_addr().ok();
        let port = bind_addr.map(|a| a.port()).unwrap_or(0);

        if let Some(p) = tcp_advertised_port {
            eprintln!("[web_gateway] ICE-TCP candidates advertise port {p}");
        }

        let mut fatal_accept_streak: u32 = 0;
        loop {
            let (stream, peer_addr) = match listener.accept().await {
                Ok(conn) => {
                    fatal_accept_streak = 0;
                    conn
                }
                Err(e) => {
                    if should_continue_after_accept_error(&e) {
                        eprintln!("[web_gateway] accept failed on port {port}: {e} (continuing)");
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                    // "Fatal" classifications (EINVAL class) have been
                    // observed spuriously on macOS: accept() failed while
                    // the socket remained LISTEN at the kernel (backlog
                    // still completing handshakes), correlated with
                    // external-agent spawns. Give the same socket a short
                    // streak of retries before declaring it dead.
                    fatal_accept_streak += 1;
                    if fatal_accept_streak < FATAL_ACCEPT_REBIND_THRESHOLD {
                        eprintln!(
                            "[web_gateway] accept failed on port {port}: {e} (retry {fatal_accept_streak}/{FATAL_ACCEPT_REBIND_THRESHOLD} before rebind)"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                    fatal_accept_streak = 0;
                    // Persistent: the listening socket really is dead.
                    // Exiting here would leave the daemon half-alive
                    // (established WebSockets keep the UI looking healthy
                    // while every new request fails) — rebind the original
                    // address instead, backing off up to 30s.
                    let Some(addr) = bind_addr else {
                        eprintln!(
                            "[web_gateway] accept failed on port {port}: {e} (bind address unknown; listener task exiting)"
                        );
                        break;
                    };
                    eprintln!(
                        "[web_gateway] accept failed on port {port}: {e} (rebinding listener)"
                    );
                    // Drop the dead listener before rebinding: it still owns
                    // the port at the kernel level, and SO_REUSEADDR only
                    // bypasses TIME_WAIT — a live (even unusable) LISTEN
                    // socket makes every rebind fail with EADDRINUSE, so
                    // holding it across the loop wedged recovery forever.
                    // Its backlog also keeps completing handshakes for
                    // requests nothing will ever read.
                    drop(listener);
                    let mut delay = std::time::Duration::from_millis(250);
                    listener = loop {
                        tokio::time::sleep(delay).await;
                        match rebind_dead_tcp_listener(addr) {
                            Ok(fresh) => {
                                eprintln!("[web_gateway] listener rebound on port {port}");
                                break fresh;
                            }
                            Err(err) => {
                                delay = (delay * 2).min(std::time::Duration::from_secs(30));
                                eprintln!(
                                    "[web_gateway] listener rebind on port {port} failed: {err} (retrying in {:.1}s)",
                                    delay.as_secs_f32()
                                );
                            }
                        }
                    };
                    continue;
                }
            };

            let bus = bus.clone();
            let broadcast_tx = broadcast_tx.clone();
            let config_json = config_json.clone();
            let agent_card_json = agent_card_json.clone();
            let agent_card_value_for_targets = agent_card_value_for_targets.clone();
            let peer_access_request_config = peer_access_request_config.clone();
            let peer_registry = peer_registry.clone();
            let dashboard_control = Arc::clone(&dashboard_control);
            let peer_file_transfer_registry = Arc::clone(&peer_file_transfer_registry);
            let ice_config = ice_config.clone();
            let tcp_peer_registry = Arc::clone(&tcp_peer_registry);
            let tcp_relay_registry = Arc::clone(&tcp_relay_registry);
            let tcp_advertised_port = tcp_advertised_port;
            let shared_session = shared_session.clone();
            let voice_debug = voice_debug.clone();
            let session_provider = session_provider.clone();
            let session_model = session_model.clone();
            let app_html = app_html.clone();
            let app_html_cache = app_html_cache.clone();
            let app_html_override = app_html_override.clone();
            let transcriber = transcriber.clone();
            let active_presence = active_presence.clone();
            let display_input_authority = display_input_authority.clone();
            let dashboard_tabs = dashboard_tabs.clone();
            let authority_change_tx = authority_change_tx.clone();
            let federated_authority_subscribers = federated_authority_subscribers.clone();
            let last_usage_json = last_usage_json.clone();
            let last_live_usage_json = last_live_usage_json.clone();
            let last_status_json = last_status_json.clone();
            let last_autonomy_json = last_autonomy_json.clone();
            let last_external_agent_json = last_external_agent_json.clone();
            let attached_external_sessions = attached_external_sessions.clone();
            let last_user_display_json = last_user_display_json.clone();
            let session_state_lines = session_state_lines.clone();
            let display_ready_cache = display_ready_cache.clone();
            let bootstrap_caches = bootstrap_caches.clone();
            let task_tx = task_tx.clone();
            let project_root = project_root.clone();
            let mcp_server = mcp_server.clone();
            let terminal_registry = terminal_registry.clone();
            let inbound_bearer_token = inbound_bearer_token.clone();
            let worktree_inventory_cache = worktree_inventory_cache.clone();
            let access_cert_dir = access_cert_dir.clone();
            let tls_client_cert_required = tls_client_cert_required;
            let source_hint = peer_addr.ip().to_string();
            let tls_failure_log_state = Arc::clone(&tls_failure_log_state);
            // `TlsAcceptor` wraps an `Arc<ServerConfig>`, so cloning is cheap
            // (one Arc bump). `None` when TLS is disabled.
            let tls_acceptor = tls_acceptor.clone();

            tokio::spawn(async move {
                // Snapshot session state at connection time
                let session_snap = shared_session.read().await;
                let daemon_session_id = session_snap.daemon_session_id.clone();
                let query_ctx = session_snap.query_ctx.clone();
                let frame_registry = session_snap.frame_registry.clone();
                let session_log = session_snap.session_log.clone();
                let recording_registry = session_snap.recording_registry.clone();
                let session_registry = session_snap.session_registry.clone();
                let snapshot_dir = session_snap.snapshot_dir.clone();
                let project_root_for_changes = session_snap.project_root_for_changes.clone();
                let file_watcher = session_snap.file_watcher.clone();
                let runtime_settings = session_snap.runtime_settings.clone();
                drop(session_snap);
                // Peek at the first bytes to detect (in order):
                //  1. ICE-TCP STUN-framed traffic (RFC 4571 length prefix
                //     followed by a STUN message whose magic cookie
                //     0x2112A442 sits at payload offset 4 = peek offset 6).
                //     First byte (length MSB) is 0x00 for STUN-sized frames.
                //  2. TLS ClientHello (handshake record: first byte 0x16,
                //     then version major 0x03) — only when a TLS acceptor
                //     is configured. Wrapped in the rustls acceptor; the
                //     decrypted stream then flows through the WS/HTTP paths
                //     below exactly as a plain connection would.
                //  3. WebSocket upgrade (HTTP header containing
                //     "Upgrade: websocket")
                //  4. Plain HTTP (everything else)
                //
                // Cases 3 and 4 are cleartext. When a TLS acceptor is
                // configured the dashboard is HTTPS/WSS-only, so such
                // cleartext connections are refused (see the strict-TLS
                // rejection below) — only the TLS-wrapped path serves them.
                // Case 1 (raw ICE-TCP for the WebRTC media tunnel) stays
                // cleartext regardless: it returns above before that check.
                //
                // The three first-byte classes are mutually exclusive:
                // STUN length-prefix MSB 0x00, TLS handshake 0x16, HTTP
                // method ASCII letters (>= 0x41). So one peeked byte
                // disambiguates raw ICE-TCP from TLS from cleartext HTTP.
                //
                // `peek()` does not consume the data, so the ICE-TCP, TLS,
                // WebSocket, and HTTP branches all still get the full
                // first segment. The ICE-TCP branch reads (and consumes)
                // the first RFC 4571 frame and hands the rest to the
                // WebRTC peer reader; the TLS branch lets the handshake
                // consume the peeked ClientHello and re-reads the
                // decrypted request head before dispatching.
                let mut buf = [0u8; 2048];
                let mut raw_stream = stream;
                let peeked = match raw_stream.peek(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };

                // ICE-TCP detection: look for a STUN binding request
                // wrapped in an RFC 4571 2-byte BE length prefix. STUN
                // binding request type is 0x0001 (first payload byte < 2),
                // magic cookie is 0x2112A442 at payload offset 4, which
                // lives at peek offset 6..10 once we account for the
                // length prefix. A valid HTTP request never starts with
                // these bytes (method chars are ASCII >= 0x41).
                let looks_like_stun_tcp =
                    peeked >= 22 && buf[2] < 2 && buf[6..10] == [0x21, 0x12, 0xA4, 0x42];
                if looks_like_stun_tcp {
                    // Consume the first RFC 4571 frame from the stream
                    // (peek leaves it in the kernel buffer; we have to
                    // read it through to hand a clean stream to the peer
                    // reader task).
                    let first_frame =
                        match crate::display::webrtc::read_rfc4571_frame_pub(&mut raw_stream).await
                        {
                            Ok(f) => f,
                            Err(e) => {
                                eprintln!("[web_gateway] ICE-TCP first-frame read failed: {e}");
                                return;
                            }
                        };
                    let remote_addr = match raw_stream.peer_addr() {
                        Ok(a) => a,
                        Err(_) => return,
                    };

                    // Slice 3b dispatch: parse the frame's ufrag once,
                    // then check the local `TcpPeerRegistry` first (for
                    // local WebRtcPeers the daemon owns) and fall
                    // through to the `TcpRelayRegistry` (federated
                    // peers the primary relays to). Unknown ufrag =
                    // close with a diagnostic log.
                    //
                    // Local first keeps the existing behavior
                    // unchanged for non-federated topologies;
                    // relay-as-fallback adds the federation relay
                    // path without touching the local fast path.
                    match crate::display::webrtc::parse_first_frame_ufrag(&first_frame) {
                        Some(ufrag) if tcp_peer_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) = tcp_peer_registry
                                .route_accepted(raw_stream, first_frame, remote_addr)
                                .await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP local routing for {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) if tcp_relay_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) = tcp_relay_registry
                                .route_accepted(raw_stream, first_frame)
                                .await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP relay routing for ufrag={ufrag} from {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: no route for ufrag {ufrag:?} from {remote_addr} \
                                 (neither local peer nor registered relay)"
                            );
                        }
                        None => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: first frame from {remote_addr} isn't a \
                                 STUN binding request with a parseable USERNAME"
                            );
                        }
                    }
                    return;
                }

                // Connection is not raw ICE-TCP. It is one of: TLS
                // (HTTPS/WSS), plain WebSocket, or plain HTTP. Convert the
                // raw `TcpStream` into a unified, boxed `DemuxStream` that
                // the WS/HTTP handling below operates through. The plain
                // path boxes the TcpStream verbatim (the peeked bytes stay
                // in the kernel buffer, unconsumed). The TLS path runs the
                // rustls handshake — which consumes the peeked ClientHello
                // — then re-reads the decrypted request head so the rest of
                // the handler sees cleartext HTTP.
                let is_tls = tls_acceptor.is_some()
                    && crate::web_tls::looks_like_tls_client_hello(&buf[..peeked]);

                let cleartext_header_text = if is_tls {
                    String::new()
                } else {
                    String::from_utf8_lossy(&buf[..peeked]).to_string()
                };
                let allow_loopback_cleartext_mcp =
                    is_loopback_cleartext_mcp_request(peer_addr, is_tls, &cleartext_header_text);

                // Strict TLS: when a TLS acceptor is configured the dashboard
                // is HTTPS/WSS-only. A connection that reaches this point is
                // neither raw ICE-TCP (handled and returned above — that path
                // stays cleartext for the WebRTC media tunnel and must keep
                // working) nor a TLS ClientHello, so it's a cleartext HTTP or
                // WebSocket client dialing the secure port in the clear.
                // Opportunistic TLS — quietly serving such a client over plain
                // HTTP — would undercut the project's "no unencrypted traffic"
                // guarantee, so we refuse it. The one exception is the local
                // loopback `/mcp` endpoint used by managed child CLIs: those
                // clients cannot present the dashboard mTLS certificate, and
                // their transport never leaves the host. Browser-originated
                // requests do not qualify for that exception.
                if tls_acceptor.is_some() && !is_tls && !allow_loopback_cleartext_mcp {
                    use tokio::io::AsyncWriteExt;
                    log_tls_failure_rate_limited(
                        &tls_failure_log_state,
                        &source_hint,
                        "strict TLS cleartext reject",
                        "dashboard is HTTPS/WSS-only; use https:// or wss://",
                    );
                    let body = "This endpoint requires TLS. Use https:// (or wss://) instead of \
                                http:// / ws://.\n";
                    let response =
                        HttpResponse::with_content("426 Upgrade Required", "text/plain", body)
                            .header("Upgrade", "TLS/1.2")
                            .header("Connection", "close")
                            .into_string();
                    let _ = raw_stream.write_all(response.as_bytes()).await;
                    let _ = raw_stream.shutdown().await;
                    return;
                }

                let buf_owned: Vec<u8>;
                let mut stream: DemuxStream;
                let tls_fleet_origin: bool;
                let tls_client_cert_present: bool;
                let tls_client_cert_fingerprint: Option<String>;
                if is_tls {
                    let acceptor = tls_acceptor
                        .as_ref()
                        .expect("is_tls implies acceptor present")
                        .clone();
                    let mut tls_stream = match acceptor.accept(raw_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            log_tls_failure_rate_limited(
                                &tls_failure_log_state,
                                &source_hint,
                                "TLS handshake failed",
                                &e.to_string(),
                            );
                            return;
                        }
                    };
                    // Capture certificate-selection provenance at the TLS
                    // boundary. The public fleet/WebPKI name is a discovery
                    // endpoint, never an authority anchor; HTTP Host alone is
                    // mutable and cannot establish the stronger direct-mTLS
                    // ceremony.
                    tls_fleet_origin =
                        crate::web_tls::is_fleet_server_name(tls_stream.get_ref().1.server_name());
                    let peer_certs = tls_stream
                        .get_ref()
                        .1
                        .peer_certificates()
                        .map(|certs| certs.to_vec())
                        .unwrap_or_default();
                    tls_client_cert_present = !peer_certs.is_empty();
                    tls_client_cert_fingerprint = peer_certs
                        .first()
                        .map(|cert| crate::peer::access_policy::fingerprint_der(cert.as_ref()));
                    // Read the first segment of the *decrypted* request so
                    // we can route on the real HTTP request line/headers.
                    // This is the TLS analogue of the plain-path peek.
                    use tokio::io::AsyncReadExt;
                    let mut decrypted = vec![0u8; 8192];
                    let read_n = match tls_stream.read(&mut decrypted).await {
                        Ok(0) => return, // client closed right after handshake
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("[web_gateway] TLS first decrypted read failed: {e}");
                            return;
                        }
                    };
                    decrypted.truncate(read_n);
                    buf_owned = decrypted.clone();
                    // Replay the decrypted request head in front of the TLS
                    // stream so the WS upgrade / HTTP body reads downstream
                    // see the request from byte zero.
                    stream = DemuxStream::new(Box::pin(crate::web_tls::PrefixedStream::new(
                        decrypted, tls_stream,
                    )));
                } else {
                    // Plain HTTP/WS: the peeked bytes are still in the
                    // kernel buffer. Box the raw stream with an empty
                    // replay prefix — a zero-overhead pass-through that
                    // reads the request straight from the socket.
                    buf_owned = buf[..peeked].to_vec();
                    tls_fleet_origin = false;
                    tls_client_cert_present = false;
                    tls_client_cert_fingerprint = None;
                    stream = DemuxStream::new(Box::pin(crate::web_tls::PrefixedStream::new(
                        Vec::new(),
                        raw_stream,
                    )));
                }
                // ── HTTP/1.1 keep-alive request loop ─────────────────
                // One accepted connection serves a sequence of requests:
                // each iteration processes the request head captured in
                // `head_segment` (request 1: the demux peek / first TLS
                // read; request N+1: read back below after the previous
                // response PARKED the stream instead of closing it).
                // Identity, IAM context, and the origin gates are
                // re-evaluated PER REQUEST inside `serve_http_request` —
                // only transport facts (TLS-ness, the client-cert
                // fingerprint, the peer address) are connection-scoped.
                // The loop ends when a write edge closes instead of
                // parking (the keep_alive module holds the decision
                // table), when the idle timeout / request budget runs
                // out, or when a WebSocket upgrade hands the whole
                // connection off below.
                let parked = DemuxStream::new_parked_slot();
                stream.arm_keep_alive(&parked);
                let mut served: u32 = 0;
                let mut head_segment: Vec<u8> = buf_owned;
                let (mut stream, header_text, peer_connection_identity, browser_host_ip) = loop {
                    served += 1;
                    let n = head_segment.len();
                    let header_text = String::from_utf8_lossy(&head_segment).to_string();
                    let peer_connection_identity =
                        match resolve_peer_connection_identity_from_cert_dir(
                            &access_cert_dir,
                            &header_text,
                            tls_client_cert_fingerprint.as_deref(),
                        ) {
                            Ok(identity) => identity,
                            Err((status, body)) => {
                                use tokio::io::AsyncWriteExt;
                                let reason = match status {
                                    401 => "Unauthorized",
                                    403 => "Forbidden",
                                    _ => "Error",
                                };
                                let response = HttpResponse::with_content(
                                    format!("{} {}", status, reason),
                                    "application/json",
                                    body,
                                )
                                .header("Cache-Control", "no-cache")
                                .header("Connection", "close")
                                .into_string();
                                let _ = stream.write_all(response.as_bytes()).await;
                                finalize_http_stream(&mut stream).await;
                                return;
                            }
                        };
                    let is_websocket = header_text
                        .lines()
                        .any(|l| l.to_lowercase().contains("upgrade: websocket"));

                    // Parse the `Host:` header to learn what address the
                    // browser thinks reaches us. We use this later as the IP
                    // for ICE-TCP host candidates: Firefox refuses to pair
                    // remote loopback candidates, so we need a non-loopback
                    // address the browser can actually connect to. The only
                    // one we know for sure the browser can reach is whatever
                    // it just used to reach us for HTTP — which is exactly
                    // what the Host header contains. If the user accessed
                    // via a hostname (`localhost`, `myserver.local`) rather
                    // than a literal IP, we get None here and skip the TCP
                    // candidate entirely; those users can still use UDP if
                    // their topology allows it.
                    let browser_host_ip: Option<std::net::IpAddr> =
                        extract_host_header_ip(&header_text);

                    if is_websocket {
                        // Upgrades never loop: the connection stops being
                        // HTTP. Hand the WS path everything it needs —
                        // whether this was request 1 or a kept-alive
                        // follow-up, the stream delivers the upgrade head
                        // from byte zero (kernel buffer / PrefixedStream /
                        // replay).
                        break (
                            stream,
                            header_text,
                            peer_connection_identity,
                            browser_host_ip,
                        );
                    }

                    // Request leg of the keep-alive verdict; the body and
                    // response legs are dispatch's and the write edges'.
                    stream.begin_request(
                        served < KEEP_ALIVE_MAX_REQUESTS
                            && request_allows_keep_alive(&header_text)
                            && segment_is_single_request(&head_segment),
                    );
                    let http_ctx = HttpRequestCtx {
                        access_cert_dir: access_cert_dir.clone(),
                        bus: bus.clone(),
                        config_json: config_json.clone(),
                        session_provider: session_provider.clone(),
                        session_model: session_model.clone(),
                        agent_card_json: agent_card_json.clone(),
                        agent_card_value_for_targets: agent_card_value_for_targets.clone(),
                        app_html: app_html.clone(),
                        app_html_override: app_html_override.clone(),
                        app_html_cache: app_html_cache.clone(),
                        worktree_inventory_cache: worktree_inventory_cache.clone(),
                        mcp_server: mcp_server.clone(),
                        peer_registry: peer_registry.clone(),
                        project_root: project_root.clone(),
                        inbound_bearer_token: inbound_bearer_token.clone(),
                        tls_client_cert_required,
                        peer_access_request_config: peer_access_request_config.clone(),
                        active_presence: active_presence.clone(),
                        voice_debug: voice_debug.clone(),
                        dashboard_control: Arc::clone(&dashboard_control),
                        daemon_session_id: daemon_session_id.clone(),
                        query_ctx: query_ctx.clone(),
                        frame_registry: frame_registry.clone(),
                        session_log: session_log.clone(),
                        recording_registry: recording_registry.clone(),
                        session_registry: session_registry.clone(),
                        snapshot_dir: snapshot_dir.clone(),
                        project_root_for_changes: project_root_for_changes.clone(),
                        runtime_settings: runtime_settings.clone(),
                        file_watcher: file_watcher.clone(),
                    };
                    serve_http_request(
                        http_ctx,
                        stream,
                        n,
                        &header_text,
                        peer_addr,
                        source_hint.clone(),
                        is_tls,
                        tls_fleet_origin,
                        tls_client_cert_present,
                        tls_client_cert_fingerprint.clone(),
                        peer_connection_identity,
                    )
                    .await;

                    // The write edge either parked the stream for reuse
                    // or closed it (finalize); no parked stream means the
                    // connection is done.
                    let Some(back) = parked.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                        return;
                    };
                    stream = back;
                    let Some(next) = read_next_request_head(
                        &mut stream,
                        std::time::Duration::from_secs(KEEP_ALIVE_IDLE_SECS),
                    )
                    .await
                    else {
                        // Idle timeout, clean client close, or an
                        // unparseable follow-up: flush + shutdown ends the
                        // connection (the finalize contract's home is
                        // connection end now — parked responses were
                        // already flushed per response).
                        finalize_http_stream(&mut stream).await;
                        return;
                    };
                    // Serve the captured segment back to readers so
                    // dispatch (or a WS upgrade) sees the request from
                    // byte zero, exactly as request 1 arrives.
                    stream.push_replay(&next);
                    head_segment = next;
                };

                // ── WebSocket upgrade path — request 1 or any kept-alive
                //    follow-up whose head asked to upgrade. ──
                {
                    if tls_fleet_origin || request_names_known_fleet_origin(&header_text) {
                        use tokio::io::AsyncWriteExt;
                        let response = json_error(
                            "403 Forbidden",
                            "the public fleet-name endpoint is discovery-only; use loopback or the independently fingerprint-verified direct mTLS address for control",
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                    // Browsers attach their page Origin to WebSocket
                    // handshakes, but WebSocket's same-origin protection is
                    // entirely server-enforced. Reject foreign pages before
                    // transport identity is converted into a dashboard grant:
                    // otherwise a hosted page can make the browser present an
                    // already-enrolled mTLS certificate and inherit that
                    // direct principal's daemon-local IAM grant. Native
                    // and daemon clients may omit Origin. The local wrapper's
                    // custom scheme is an origin allowance, not authentication
                    // or a shipped signed-native remote anchor; transport IAM
                    // checks below still apply.
                    if let Some(origin) = extract_origin_header(&header_text)
                        .filter(|origin| !is_own_or_app_origin(origin, is_tls, &header_text))
                    {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": "cross-origin caller is not allowed on this WebSocket",
                            "origin": origin,
                        })
                        .to_string();
                        let response =
                            HttpResponse::with_content("403 Forbidden", "application/json", body)
                                .header("Cache-Control", "no-cache")
                                .header("Vary", "Origin")
                                .header("Connection", "close")
                                .into_string();
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                    let remote_client_auth_missing = remote_dashboard_client_auth_missing(
                        peer_addr,
                        &header_text,
                        tls_client_cert_fingerprint.as_deref(),
                        peer_connection_identity.as_ref(),
                    );
                    if (tls_client_cert_required && !tls_client_cert_present)
                        || remote_client_auth_missing
                    {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": if remote_client_auth_missing {
                                "verified client certificate or authenticated peer identity required for remote dashboard access"
                            } else {
                                "mTLS client certificate required"
                            }
                        })
                        .to_string();
                        let response = HttpResponse::with_content(
                            "401 Unauthorized",
                            "application/json",
                            body,
                        )
                        .header("Cache-Control", "no-cache")
                        .header("Connection", "close")
                        .into_string();
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                    // Bearer enforcement on /ws — dual-mode (Authorization
                    // header from daemons, ?token= query param from
                    // browsers). Reject with a plain HTTP 401 *before*
                    // the WebSocket handshake so the rejected client
                    // never sees a successful upgrade.
                    if let Err((status, body)) =
                        verify_bearer_for_ws(&header_text, inbound_bearer_token.as_deref())
                    {
                        use tokio::io::AsyncWriteExt;
                        let reason = match status {
                            401 => "Unauthorized",
                            _ => "Error",
                        };
                        let response = HttpResponse::with_content(
                            format!("{} {}", status, reason),
                            "application/json",
                            body,
                        )
                        .header("WWW-Authenticate", "Bearer")
                        .header("Connection", "close")
                        .into_string();
                        let _ = stream.write_all(response.as_bytes()).await;
                        // Flush + cleanly shut down before the task returns and
                        // drops the stream. On the TLS path rustls buffers the
                        // ciphertext for this 401 inside the session; dropping
                        // without flushing discards it and the rejected client
                        // sees an *empty* response instead of the 401 (audit
                        // F2). A no-op pass-through on plain TCP.
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                    let dashboard_control_grant_for_ws = match dashboard_control_grant_for_client(
                        &access_cert_dir,
                        peer_connection_identity.as_ref(),
                        tls_client_cert_fingerprint.as_deref(),
                        tls_client_cert_present,
                    ) {
                        Ok(grant) => grant,
                        Err(message) => {
                            use tokio::io::AsyncWriteExt;
                            let response = json_error("500 Internal Server Error", message);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    };
                    if !dashboard_control_grant_for_ws.allows_unfiltered_websocket_stream() {
                        use tokio::io::AsyncWriteExt;
                        let response = json_error(
                            "403 Forbidden",
                            "mTLS client lacks the complete observer read set required by the legacy WebSocket stream",
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                    let peer_identity_for_ws = peer_connection_identity.clone();
                    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };

                    let (ws_tx, ws_rx) = ws_stream.split();
                    let outbound_rx = broadcast_tx.subscribe();

                    // Per-connection identity for active/passive tracking
                    let connection_id = uuid::Uuid::new_v4().to_string();

                    // Tab presence: register this event-lane connection.
                    // The tab id is client-declared (`?tab=` beside the
                    // token — browsers can't set headers on WebSocket
                    // opens); ws_inbound_task unregisters on close.
                    dashboard_tabs.register(
                        &connection_id,
                        DashboardTabConnection {
                            lane: DashboardTabLane::LegacyWs,
                            kind: dashboard_control_grant_for_ws.connection_kind(),
                            label: dashboard_control_grant_for_ws.label().to_string(),
                            tab_id: extract_query_param(
                                header_text.lines().next().unwrap_or(""),
                                "tab",
                            ),
                            remote: browser_host_ip.map(|ip| ip.to_string()),
                            user_agent: extract_header_value(&header_text, "user-agent"),
                            connected_at_unix_ms: now_unix_ms(),
                        },
                    );

                    // Direct response channel: tool_response and state_snapshot
                    // messages for this specific connection (not broadcast).
                    let (direct_tx, direct_rx) = mpsc::unbounded_channel::<String>();

                    // Subscribe before reading the authority bootstrap. Any
                    // transition racing the snapshot is then either represented
                    // by the snapshot or retained for the outbound task. Waiting
                    // until after log replay left a long missed-update window.
                    let authority_change_rx = authority_change_tx.subscribe();

                    // Ordering barrier for the outbound task: live broadcast
                    // events stay gated until every bootstrap frame queued on
                    // `direct_tx` below has drained to the socket, so a live
                    // event can never interleave into (or precede) the
                    // bootstrap sequence. Fired after the last bootstrap
                    // enqueue; `ws_outbound_task`'s biased select drains
                    // direct frames first, then opens the broadcast lane.
                    let (bootstrap_flushed_tx, bootstrap_flushed_rx) =
                        tokio::sync::oneshot::channel::<()>();

                    // Send bootstrap state snapshot on connect (with connection_id).
                    // Include config (provider/model) since AgentStateSnapshot
                    // doesn't carry those. The top-level `session_id` is the
                    // stable daemon/process session, not the active worker log.
                    let state = query_ctx
                        .as_ref()
                        .map(|ctx| {
                            ctx.agent_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                        })
                        .unwrap_or_default();
                    let bootstrap_session_id = daemon_session_id
                        .clone()
                        .or_else(|| {
                            query_ctx
                                .as_ref()
                                .and_then(|ctx| replay_session_id_from_dir(&ctx.log_dir))
                        })
                        .or_else(|| session_log.as_ref().and_then(session_log_id));
                    if query_ctx.is_some() || bootstrap_session_id.is_some() {
                        let config: serde_json::Value =
                            serde_json::from_str(&config_json).unwrap_or_default();
                        let bootstrap = serde_json::json!({
                            "t": "state_snapshot",
                            "state": state,
                            "connection_id": connection_id,
                            "config": config,
                            "session_id": bootstrap_session_id.unwrap_or_default(),
                        });
                        let _ = direct_tx.send(bootstrap.to_string());
                    }

                    // Send cached usage data so late-connecting browsers
                    // populate the Usage tab without sending ControlMsg.
                    if let Ok(guard) = last_usage_json.lock() {
                        if let Some(ref usage_json) = *guard {
                            let _ = direct_tx.send(usage_json.clone());
                        }
                    }

                    // Send cached live usage data.
                    if let Ok(guard) = last_live_usage_json.lock() {
                        if let Some(ref live_json) = *guard {
                            let _ = direct_tx.send(live_json.clone());
                        }
                    }

                    // Send cached status (autonomy, session_id, task).
                    if let Ok(guard) = last_status_json.lock() {
                        if let Some(ref status_json) = *guard {
                            let _ = direct_tx.send(status_json.clone());
                        }
                    }

                    // Re-send the latest change-detected per-session state
                    // (session_started / session_vitals / session_goal).
                    // These fire on change only, so without this a late
                    // joiner — a refreshed browser on an idle daemon, or a
                    // peer transport attaching — would never see state that
                    // last changed before this connection existed. Each
                    // session's `session_started` goes FIRST (windows must
                    // exist before state lands on them) and is stamped
                    // `replayed: true` so the frontend rebuilds the window
                    // without live-start side effects (thinking phase,
                    // focus steal, current-task clobber).
                    let session_state_replay: Vec<String> = session_state_lines
                        .lock()
                        .map(|guard| {
                            guard
                                .values()
                                .flat_map(|kinds| {
                                    let started = kinds.get("session_started").map(|line| {
                                        match serde_json::from_str::<serde_json::Value>(line) {
                                            Ok(mut parsed) => {
                                                parsed["replayed"] = serde_json::json!(true);
                                                parsed.to_string()
                                            }
                                            Err(_) => line.clone(),
                                        }
                                    });
                                    started.into_iter().chain(
                                        kinds
                                            .iter()
                                            .filter(|(kind, _)| **kind != "session_started")
                                            .map(|(_, line)| line.clone()),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    for line in session_state_replay {
                        let _ = direct_tx.send(line);
                    }

                    // Send cached autonomy after cached status so it wins
                    // when the latest status event is older than the user's
                    // most recent autonomy switch.
                    if let Ok(guard) = last_autonomy_json.lock() {
                        if let Some(ref autonomy_json) = *guard {
                            let _ = direct_tx.send(autonomy_json.clone());
                        }
                    }

                    // Send cached external_agent_changed so the dropdown
                    // and status badge reflect the current value on a
                    // fresh browser connection.
                    if let Ok(guard) = last_external_agent_json.lock() {
                        if let Some(ref ea_json) = *guard {
                            let _ = direct_tx.send(ea_json.clone());
                        }
                    }

                    // Send cached user_display_granted so the "your display"
                    // status bar toggle reflects the current grant state on
                    // a refreshed browser. Cache is cleared on revoke so
                    // a revoked state simply results in nothing being sent
                    // (the dashboard's HTML default is "off").
                    if let Ok(guard) = last_user_display_json.lock() {
                        if let Some(ref ud_json) = *guard {
                            let _ = direct_tx.send(ud_json.clone());
                        }
                    }

                    // Re-raise still-pending display requests so a
                    // late-connecting dashboard (the exact browser the
                    // attention nudge just summoned) shows the popup.
                    // Requests are short-lived; expired entries are
                    // filtered by the snapshot.
                    for pending in crate::display_requests::registry()
                        .pending_snapshot(crate::display_requests::now_unix_ms())
                    {
                        let line = serde_json::to_string(
                            &crate::types::OutboundEvent::DisplayRequestRaised {
                                session_id: Some(pending.session_key.clone())
                                    .filter(|s| s != "main"),
                                id: pending.id,
                                access: pending.access.as_str().to_string(),
                                reason: pending.reason.clone(),
                                expires_unix_ms: pending.expires_unix_ms,
                            },
                        );
                        if let Ok(line) = line {
                            let _ = direct_tx.send(line);
                        }
                    }

                    let browser_workspaces = crate::browser_workspace::list_workspaces().await;
                    let browser_snapshot = serde_json::json!({
                        "t": "browser_workspace_snapshot",
                        "workspaces": browser_workspaces,
                    });
                    let _ = direct_tx.send(browser_snapshot.to_string());

                    // Replay display_ready for every active display session so
                    // late-connecting browsers (including refreshes) recreate
                    // their DisplaySlots and initiate WebRTC.  Prefer the
                    // live session registry over the broadcast cache — it is
                    // authoritative and handles multiple concurrent displays.
                    //
                    // Phase 5a.1: alongside each display_ready, send a
                    // personalized `display_input_authority_state` so the
                    // browser starts at the authoritative state instead of
                    // `unknown`.  Without this snapshot the chip would only
                    // resolve on the next authority transition, which may
                    // be never if no one ever takes control.
                    //
                    // Frame ordering: `display_ready` goes out now (so the
                    // slot exists before any log replay happens); the
                    // per-display `display_input_authority_state` frame is
                    // deferred until *after* `log_replay` below. **#59**:
                    // browser-side `addDisplaySlot` is now idempotent for
                    // an existing live slot, so a replayed historical
                    // `display_ready` no longer destroys the bootstrap
                    // slot. The deferral here is therefore defense-in-
                    // depth against message ordering and late-replay
                    // state — for example a grant→revoke→grant cycle in
                    // session.jsonl whose intermediate `user_display_revoked`
                    // does tear the bootstrap slot down, after which the
                    // replayed re-grant `display_ready` creates a fresh
                    // slot that needs the authority frame to land on it
                    // rather than on the destroyed predecessor. Sending
                    // the authority frame after replay guarantees it lands
                    // on the *final* slot in every replay shape.
                    let bootstrap_authority_snapshots: Vec<(u32, &'static str)> =
                        if let Some(ref sr) = session_registry {
                            let reg = sr.read().await;
                            // Dashboards are the user surface: replay
                            // private user views too (they exist FOR this
                            // surface), tagged so the tile renders its
                            // "private view" chip.
                            let active_ids: Vec<u32> = reg.all_display_ids();
                            // Snapshot resolutions + auth states under the
                            // std lock, then drop the guard before any
                            // direct_tx.send calls.
                            let resolutions: Vec<(u32, u32, u32, bool)> = active_ids
                                .iter()
                                .filter_map(|&did| {
                                    reg.get_any(did).map(|session| {
                                        let (w, h) = session.resolution();
                                        (did, w, h, session.agent_visible())
                                    })
                                })
                                .collect();
                            let auth_snapshots = {
                                let auth = display_input_authority
                                    .read()
                                    .unwrap_or_else(|e| e.into_inner());
                                compute_bootstrap_authority_snapshots(
                                    resolutions.iter().map(|(did, _, _, _)| *did),
                                    &auth,
                                    &connection_id,
                                )
                            };
                            // Send the display_ready frames now; defer the
                            // authority frames until after log_replay.
                            for (did, w, h, agent_visible) in resolutions {
                                let ready = serde_json::json!({
                                    "event": "display_ready",
                                    "display_id": did,
                                    "width": w,
                                    "height": h,
                                    "agent_visible": agent_visible,
                                });
                                let _ = direct_tx.send(ready.to_string());
                            }
                            auth_snapshots
                        } else {
                            // Fallback: use the broadcast-derived cache when
                            // no session registry is available (shouldn't
                            // happen in practice, but keeps the old
                            // behaviour as safety net).  No authority frame
                            // to send in this branch — the cache only holds
                            // display_ready JSON, no holder state.
                            if let Ok(guard) = display_ready_cache.lock() {
                                for display_json in guard.values() {
                                    let _ = direct_tx.send(display_json.clone());
                                }
                            }
                            Vec::new()
                        };

                    // Replay session log so late-connecting browsers see
                    // historical events (not just real-time from now on).
                    // Each JSONL entry is converted to an OutboundEvent via
                    // session_log_entry_to_app_event → app_event_to_outbound
                    // so replay drives the same rendering path as live.
                    let replay_log_dir =
                        query_ctx
                            .as_ref()
                            .map(|ctx| ctx.log_dir.clone())
                            .or_else(|| {
                                session_log.as_ref().and_then(|sl| {
                                    sl.lock().ok().map(|log| log.dir().to_path_buf())
                                })
                            });
                    let mut replayed_external_session_ids: HashSet<String> = HashSet::new();
                    if let Some(log_dir) = replay_log_dir.clone() {
                        // Cache-fast after the first connect, but still file IO on a
                        // miss — keep it off the reactor (single-flight in the cache
                        // coalesces concurrent connects).
                        let replay_result = tokio::task::spawn_blocking(move || {
                            session_log_replay_payload_from_dir_with_limit(
                                &log_dir,
                                Some(WEBSOCKET_BOOTSTRAP_REPLAY_ENTRY_LIMIT),
                            )
                        })
                        .await
                        .ok()
                        .flatten();
                        if let Some((replay, external_session_id)) = replay_result {
                            if let Some(external_session_id) = external_session_id {
                                replayed_external_session_ids.insert(external_session_id);
                            }
                            let _ = direct_tx.send(replay);
                        }
                    }

                    let mut active_external_sessions: Vec<(String, String)> =
                        attached_external_sessions
                            .lock()
                            .ok()
                            .map(|guard| {
                                guard
                                    .iter()
                                    .map(|(session_id, source)| {
                                        (session_id.clone(), source.clone())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                    active_external_sessions.sort_by(|a, b| a.0.cmp(&b.0));
                    for (session_id, source) in active_external_sessions {
                        // Wrapper-covered sessions replay from the wrapper
                        // log ONLY. The old mtime tiebreak re-sent the whole
                        // external transcript whenever the backend's file
                        // was momentarily newer — which is the steady state
                        // (Claude Code flushes after the drain logs), so
                        // every connect triple-rendered supervised sessions
                        // (wrapper replay + external replay + hydration).
                        // Pre-attach history for resumed sessions still
                        // arrives via the window-hydration fetch, which
                        // merges instead of re-replaying. This matches the
                        // dashboard-control bootstrap lane, which never had
                        // the mtime override.
                        if replayed_external_session_ids.contains(&session_id) {
                            continue;
                        }
                        if let Some(replay) =
                            external_session_activity_replay_for_websocket(&source, &session_id)
                        {
                            let _ = direct_tx.send(replay);
                        }
                    }

                    // Phase 5a.1: now that log_replay has finished
                    // recreating display slots from historical events,
                    // send the personalized `display_input_authority_state`
                    // for each currently-active display.  Sending these
                    // before log_replay would land the chip on a slot that
                    // log_replay then destroys (see the slot lifecycle
                    // bookkeeping in `addDisplaySlot` / `removeDisplaySlot`
                    // on the browser side).
                    for (did, state) in bootstrap_authority_snapshots {
                        let auth_msg = serde_json::json!({
                            "t": "display_input_authority_state",
                            "display_id": did,
                            "state": state,
                        });
                        let _ = direct_tx.send(auth_msg.to_string());
                    }

                    // Bootstrap fully queued — open the outbound task's
                    // broadcast lane once these frames have drained.
                    let _ = bootstrap_flushed_tx.send(());

                    // Inbound: WebSocket → EventBus
                    // Handles message types:
                    //   {"t":"presence_connect",...}     → AppEvent::PresenceConnected
                    //   {"t":"presence_disconnect"}      → AppEvent::PresenceDisconnected
                    //   {"t":"voice_log",...}             → AppEvent::VoiceLog
                    //   {"t":"presence_checkpoint",...}   → AppEvent::PresenceCheckpointReceived
                    //   {"t":"voice_diagnostic",...}      → AppEvent::VoiceDiagnostic
                    //   {"t":"tool_request", "id":"...", "tool":"...", "args":{}} → tool_response
                    //   {"action":"status", ...}         → AppEvent::ControlCommand
                    // Assign a unique peer ID for WebRTC signaling
                    let peer_id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);
                    let ws_session_cancel = tokio_util::sync::CancellationToken::new();

                    let inbound_ctx = WsInboundCtx {
                        bus: bus.clone(),
                        query_ctx: query_ctx.clone(),
                        direct_tx: direct_tx.clone(),
                        voice_debug: voice_debug.clone(),
                        live_provider: session_provider.clone(),
                        live_model: session_model.clone(),
                        transcriber: transcriber.clone(),
                        active_presence: active_presence.clone(),
                        display_input_authority: display_input_authority.clone(),
                        authority_change_tx: authority_change_tx.clone(),
                        federated_authority_subscribers: federated_authority_subscribers.clone(),
                        connection_id: connection_id.clone(),
                        dashboard_tabs: dashboard_tabs.clone(),
                        frame_registry: frame_registry.clone(),
                        recording_registry: recording_registry.clone(),
                        session_log: session_log.clone(),
                        session_registry: session_registry.clone(),
                        task_tx: task_tx.clone(),
                        terminal_registry: terminal_registry.clone(),
                        dashboard_control: Arc::clone(&dashboard_control),
                        dashboard_control_grant: dashboard_control_grant_for_ws.clone(),
                        peer_file_transfer_registry: Arc::clone(&peer_file_transfer_registry),
                        peer_identity: peer_identity_for_ws.clone(),
                        browser_host_ip,
                        ice_config: ice_config.clone(),
                        tcp_advertised_port,
                        tcp_peer_registry: Arc::clone(&tcp_peer_registry),
                        session_cancel: ws_session_cancel.clone(),
                    };
                    let inbound = tokio::spawn(ws_inbound_task(inbound_ctx, ws_rx, peer_id));

                    // Attention-nudge presence: a live `/ws` client is the
                    // "somebody is watching" signal that suppresses pushes.
                    crate::attention_nudge::dashboard_connected();

                    // Outbound: broadcast + direct responses → WebSocket
                    let outbound = tokio::spawn(ws_outbound_task(
                        outbound_rx,
                        direct_rx,
                        ws_tx,
                        authority_change_rx,
                        connection_id.clone(),
                        display_input_authority.clone(),
                        session_registry.clone(),
                        bootstrap_caches.clone(),
                        bootstrap_flushed_rx,
                        dashboard_control_grant_for_ws.clone(),
                        ws_session_cancel,
                    ));

                    let _ = tokio::join!(inbound, outbound);
                    crate::attention_nudge::dashboard_disconnected();
                }
            });
        }
    })
}

// ---------------------------------------------------------------------------
// Bootstrap-cache maintenance (typed bus)
// ---------------------------------------------------------------------------

/// Serialize an AppEvent to the exact wire line browsers receive — the same
/// `app_event_to_outbound` + serde path the outbound broadcaster uses, so
/// cached lines are byte-identical to the broadcast copies they stand in for.
fn bootstrap_wire_line(event: &crate::event::AppEvent) -> Option<String> {
    crate::event::app_event_to_outbound(event)
        .and_then(|outbound| serde_json::to_string(&outbound).ok())
}

/// State fed by [`spawn_bootstrap_cache_maintainer`]: the shared bootstrap
/// caches every new `/ws` connection (and the tunnel's
/// `api_dashboard_bootstrap`) replays, the per-display `display_ready`
/// fallback cache, plus authority state used only for fail-closed recovery
/// when the typed event stream itself has a gap.
pub(crate) struct BootstrapCacheMaintainer {
    pub(crate) caches: crate::dashboard_control::DashboardBootstrapCaches,
    pub(crate) display_ready_cache: Arc<Mutex<HashMap<u32, String>>>,
    pub(crate) display_input_authority: Arc<DisplayInputAuthority>,
    pub(crate) authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
}

impl BootstrapCacheMaintainer {
    fn set_latest(cache: &Arc<Mutex<Option<String>>>, event: &crate::event::AppEvent) {
        if let Some(line) = bootstrap_wire_line(event) {
            if let Ok(mut guard) = cache.lock() {
                *guard = Some(line);
            }
        }
    }

    /// Latest change-detected per-session state (`session_started` /
    /// `session_vitals` / `session_goal` / pending `approval_required` /
    /// `user_question`), replayed to late joiners. Bounded at 256 sessions
    /// against a runaway session-id source: `session_ended` is the normal
    /// prune; overflow evicts the lexicographically first key, which is
    /// arbitrary but keeps it finite.
    fn cache_session_state_line(
        &self,
        kind: &'static str,
        session_id: &str,
        event: &crate::event::AppEvent,
    ) {
        let Some(line) = bootstrap_wire_line(event) else {
            return;
        };
        if let Ok(mut guard) = self.caches.session_state_lines.lock() {
            guard
                .entry(session_id.to_string())
                .or_default()
                .insert(kind, line);
            if guard.len() > 256 {
                if let Some(first) = guard.keys().next().cloned() {
                    guard.remove(&first);
                }
            }
        }
    }

    /// Fold one typed event into the bootstrap caches. Only the variants a
    /// bootstrap replays are serialized; the high-volume stream events
    /// (deltas, context snapshots) fall through untouched.
    pub(crate) fn apply(&self, event: &crate::event::AppEvent) {
        use crate::event::AppEvent as E;
        match event {
            E::DisplayReady { display_id, .. } => {
                if let Some(line) = bootstrap_wire_line(event) {
                    if let Ok(mut guard) = self.display_ready_cache.lock() {
                        guard.insert(*display_id, line);
                    }
                }
            }
            // Evict display_ready on revoke / capture loss; a revoke also
            // clears the cached grant so a refreshed browser after a revoke
            // doesn't re-enable the badge.
            E::UserDisplayRevoked { display_id, .. } => {
                if let Ok(mut guard) = self.display_ready_cache.lock() {
                    guard.remove(display_id);
                }
                if let Ok(mut guard) = self.caches.last_user_display_json.lock() {
                    *guard = None;
                }
            }
            E::DisplayCaptureLost { display_id, .. } => {
                if let Ok(mut guard) = self.display_ready_cache.lock() {
                    guard.remove(display_id);
                }
            }
            E::UserDisplayGranted { .. } => {
                Self::set_latest(&self.caches.last_user_display_json, event)
            }
            E::UsageSnapshot { .. } => Self::set_latest(&self.caches.last_usage_json, event),
            E::LiveUsageUpdate { .. } => Self::set_latest(&self.caches.last_live_usage_json, event),
            E::StatusUpdate { .. } => Self::set_latest(&self.caches.last_status_json, event),
            E::AutonomyChanged { .. } => Self::set_latest(&self.caches.last_autonomy_json, event),
            E::ExternalAgentChanged { .. } => {
                Self::set_latest(&self.caches.last_external_agent_json, event)
            }
            E::SessionAttached { .. } | E::SessionIdentity { .. } => {
                if let Some(line) = bootstrap_wire_line(event) {
                    if let Ok(mut guard) = self.caches.attached_external_sessions.lock() {
                        update_external_attached_sessions_from_wire(&mut guard, &line);
                    }
                }
            }
            // A live session's birth announcement: replayed to late joiners
            // so their Activity grid rebuilds windows for work that predates
            // the connection (session_started routinely falls off the
            // tail-limited log replay).
            E::SessionStarted { session_id, .. } => {
                self.cache_session_state_line("session_started", session_id, event)
            }
            E::SessionVitals { session_id, .. } => {
                self.cache_session_state_line("session_vitals", session_id, event)
            }
            E::SessionGoal { session_id, .. } => {
                self.cache_session_state_line("session_goal", session_id, event)
            }
            // Pending approvals/questions: the daemon-side registry survives
            // a page reload but the panel state does not — replay the ask so
            // a reconnecting operator can still answer. Cleared on
            // ApprovalResolved below. Session-less asks were never cached by
            // the wire sniffer (no `session_id` key on the line) — same here.
            E::ApprovalRequired {
                session_id: Some(session_id),
                ..
            } => self.cache_session_state_line("approval_required", session_id, event),
            E::UserQuestionRequired {
                session_id: Some(session_id),
                ..
            } => self.cache_session_state_line("user_question", session_id, event),
            E::ApprovalResolved {
                session_id: Some(session_id),
                id,
                ..
            } => {
                if let Ok(mut guard) = self.caches.session_state_lines.lock() {
                    if let Some(kinds) = guard.get_mut(session_id) {
                        for kind in ["approval_required", "user_question"] {
                            let matches = kinds
                                .get(kind)
                                .and_then(|cached| {
                                    serde_json::from_str::<serde_json::Value>(cached).ok()
                                })
                                .is_some_and(|cached| cached["id"].as_u64() == Some(*id));
                            if matches {
                                kinds.remove(kind);
                            }
                        }
                    }
                }
            }
            E::SessionEnded { session_id, .. } => {
                if let Some(line) = bootstrap_wire_line(event) {
                    if let Ok(mut guard) = self.caches.attached_external_sessions.lock() {
                        update_external_attached_sessions_from_wire(&mut guard, &line);
                    }
                }
                if let Ok(mut guard) = self.caches.session_state_lines.lock() {
                    guard.remove(session_id);
                }
            }
            _ => {}
        }
    }

    /// The maintainer lagged the typed bus: any number of grants, revokes,
    /// approvals, or session ends were missed, so every cached line may now
    /// be a ghost. Clear the affected sections — the next bootstrap omits
    /// stale lines instead of replaying them, and each cache refills on its
    /// next live event:
    /// - `display_ready_cache` is only the bootstrap's NO-registry fallback;
    ///   whenever a session registry exists the bootstrap already reads that
    ///   ground truth directly, so clearing here never blanks a live path.
    /// - the "latest line" caches (usage/status/autonomy/agent/grant) have
    ///   no in-scope authority to rebuild from; empty means the dashboard
    ///   falls back to its defaults instead of trusting a stale value.
    /// - pending approvals/questions clear rather than replay as ghosts;
    ///   the daemon-side approval registry still holds the real pending set
    ///   and live `approval_required` re-emissions repopulate the panel.
    pub(crate) fn clear_on_gap(&self) {
        for (display_id, revision) in self.display_input_authority.clear_all() {
            let _ = self.authority_change_tx.send(DisplayInputAuthorityChange {
                display_id,
                holder: None,
                revision,
            });
        }
        if let Ok(mut guard) = self.display_ready_cache.lock() {
            guard.clear();
        }
        for cache in [
            &self.caches.last_usage_json,
            &self.caches.last_live_usage_json,
            &self.caches.last_status_json,
            &self.caches.last_autonomy_json,
            &self.caches.last_external_agent_json,
            &self.caches.last_user_display_json,
        ] {
            if let Ok(mut guard) = cache.lock() {
                *guard = None;
            }
        }
        if let Ok(mut guard) = self.caches.attached_external_sessions.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = self.caches.session_state_lines.lock() {
            guard.clear();
        }
    }
}

/// Drive a [`BootstrapCacheMaintainer`] from the typed event bus.
fn spawn_bootstrap_cache_maintainer(
    bus: &EventBus,
    maintainer: BootstrapCacheMaintainer,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => maintainer.apply(&event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    eprintln!(
                        "[web_gateway] bootstrap-cache maintainer lagged the event bus \
                         ({skipped} events skipped); clearing caches so bootstraps omit \
                         stale state instead of replaying ghosts"
                    );
                    maintainer.clear_on_gap();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod bootstrap_cache_tests {
    use super::*;
    use crate::event::AppEvent;

    fn maintainer() -> (
        BootstrapCacheMaintainer,
        broadcast::Receiver<DisplayInputAuthorityChange>,
    ) {
        let (authority_change_tx, authority_rx) = broadcast::channel(8);
        (
            BootstrapCacheMaintainer {
                caches: crate::dashboard_control::DashboardBootstrapCaches::default(),
                display_ready_cache: Arc::new(Mutex::new(HashMap::new())),
                display_input_authority: Arc::new(DisplayInputAuthority::default()),
                authority_change_tx,
            },
            authority_rx,
        )
    }

    fn status_event(session_id: &str, phase: &str) -> AppEvent {
        AppEvent::StatusUpdate {
            turn: 1,
            phase: phase.to_string(),
            autonomy: "medium".to_string(),
            session_id: session_id.to_string(),
            task: "task".to_string(),
        }
    }

    #[test]
    fn typed_events_fill_the_caches_with_wire_lines() {
        let (m, _authority_rx) = maintainer();
        let display_cache = m.display_ready_cache.clone();

        m.apply(&status_event("s-1", "running"));
        let status_line = m
            .caches
            .last_status_json
            .lock()
            .unwrap()
            .clone()
            .expect("status cached");
        let parsed: serde_json::Value = serde_json::from_str(&status_line).unwrap();
        assert_eq!(parsed["event"], "status", "cached line is the wire shape");
        assert_eq!(parsed["session_id"], "s-1");

        m.apply(&AppEvent::AutonomyChanged {
            autonomy: "High".to_string(),
        });
        assert!(m
            .caches
            .last_autonomy_json
            .lock()
            .unwrap()
            .as_deref()
            .unwrap()
            .contains("\"autonomy_changed\""));

        // display_ready caches per display. The synchronous session-registry
        // lifecycle observer owns authority invalidation and UI notification.
        m.apply(&AppEvent::DisplayReady {
            display_id: 3,
            width: 1280,
            height: 720,
            agent_visible: true,
        });
        assert!(display_cache.lock().unwrap().contains_key(&3));

        // Grant cached; revoke clears it AND evicts the display entry.
        m.apply(&AppEvent::UserDisplayGranted {
            display_id: 3,
            agent_visible: true,
        });
        assert!(m.caches.last_user_display_json.lock().unwrap().is_some());
        m.apply(&AppEvent::UserDisplayRevoked {
            display_id: 3,
            note: None,
        });
        assert!(m.caches.last_user_display_json.lock().unwrap().is_none());
        assert!(!display_cache.lock().unwrap().contains_key(&3));
    }

    #[test]
    fn pending_ask_lines_track_their_resolution_by_id() {
        let (m, _rx) = maintainer();
        m.apply(&AppEvent::ApprovalRequired {
            session_id: Some("s-1".to_string()),
            id: 41,
            command_preview: "rm -rf scratch".to_string(),
            category: crate::autonomy::ActionCategory::CommandExec,
        });
        assert!(
            m.caches.session_state_lines.lock().unwrap()["s-1"].contains_key("approval_required")
        );

        // A different id resolving must NOT clear the pending ask.
        m.apply(&AppEvent::ApprovalResolved {
            session_id: Some("s-1".to_string()),
            id: 40,
            action: "approved".to_string(),
        });
        assert!(
            m.caches.session_state_lines.lock().unwrap()["s-1"].contains_key("approval_required")
        );

        // The matching id clears it — no ghost approval on the next
        // bootstrap.
        m.apply(&AppEvent::ApprovalResolved {
            session_id: Some("s-1".to_string()),
            id: 41,
            action: "approved".to_string(),
        });
        assert!(
            !m.caches.session_state_lines.lock().unwrap()["s-1"].contains_key("approval_required")
        );
    }

    #[test]
    fn session_end_prunes_session_state() {
        let (m, _rx) = maintainer();
        m.apply(&AppEvent::SessionStarted {
            session_id: "s-2".to_string(),
            task: Some("t".to_string()),
        });
        assert!(m
            .caches
            .session_state_lines
            .lock()
            .unwrap()
            .contains_key("s-2"));
        m.apply(&AppEvent::SessionEnded {
            session_id: "s-2".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        assert!(!m
            .caches
            .session_state_lines
            .lock()
            .unwrap()
            .contains_key("s-2"));
    }

    /// Lag policy: clear every affected section so a bootstrap after the
    /// gap omits possibly-stale lines instead of replaying ghosts.
    #[test]
    fn clear_on_gap_empties_every_cache_section() {
        let (m, mut authority_rx) = maintainer();
        let display_cache = m.display_ready_cache.clone();
        let revision = m.display_input_authority.revision(1);
        m.display_input_authority.write().unwrap().insert(
            1,
            DisplayInputHolder::LocalWs {
                connection_id: "stale-holder".to_string(),
                direct_tx: mpsc::unbounded_channel().0,
            },
        );
        m.apply(&status_event("s-1", "running"));
        m.apply(&AppEvent::DisplayReady {
            display_id: 1,
            width: 640,
            height: 480,
            agent_visible: true,
        });
        m.apply(&AppEvent::SessionStarted {
            session_id: "s-1".to_string(),
            task: None,
        });
        m.apply(&AppEvent::UserDisplayGranted {
            display_id: 1,
            agent_visible: true,
        });

        m.clear_on_gap();

        assert!(m.caches.last_status_json.lock().unwrap().is_none());
        assert!(m.caches.last_user_display_json.lock().unwrap().is_none());
        assert!(m.caches.session_state_lines.lock().unwrap().is_empty());
        assert!(m
            .caches
            .attached_external_sessions
            .lock()
            .unwrap()
            .is_empty());
        assert!(display_cache.lock().unwrap().is_empty());
        assert!(m.display_input_authority.read().unwrap().is_empty());
        assert_eq!(revision.load(Ordering::SeqCst), 1);
        let cleared = authority_rx.try_recv().expect("gap publishes unclaimed");
        assert_eq!(cleared.display_id, 1);
        assert!(cleared.holder.is_none());
    }

    /// End to end: intents flooding past the broadcast ring can lag the
    /// old string maintainer; the typed one keeps serving fresh state and
    /// resync lines mirror the bootstrap subset.
    #[test]
    fn resync_lines_replay_cached_state_with_session_started_stamped() {
        let (m, _rx) = maintainer();
        m.apply(&AppEvent::SessionStarted {
            session_id: "s-3".to_string(),
            task: Some("hello".to_string()),
        });
        m.apply(&status_event("s-3", "running"));
        let lines = crate::web_gateway::ws_session::bootstrap_cache_resync_lines(&m.caches);
        let started = lines
            .iter()
            .find(|line| line.contains("\"session_started\""))
            .expect("session_started replayed");
        let parsed: serde_json::Value = serde_json::from_str(started).unwrap();
        assert_eq!(
            parsed["replayed"], true,
            "resync must stamp session_started like the bootstrap replay"
        );
        assert!(lines
            .iter()
            .any(|line| line.contains("\"event\":\"status\"")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TEST_ENV_LOCK;
    use crate::types::OutboundEvent;
    use crate::web_gateway::tests::{next_ws_json_matching, EnvVarGuard};
    use tokio::io::AsyncWriteExt;

    async fn next_ws_json_type<S>(ws_rx: &mut S, ty: &str) -> serde_json::Value
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        next_ws_json_matching(ws_rx, |json| json["t"] == ty).await
    }

    #[tokio::test]
    async fn test_spawn_web_gateway_lifecycle() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );

        // Give it a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_websocket_echo() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        // Bind to port 0 for a random free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a Status control message
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Verify the EventBus receives the ControlCommand
        // (may be preceded by a PresenceLog debug event from the diagnostic logging)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");

            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status { .. })) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        handle.abort();
    }

    #[tokio::test]
    async fn test_broadcast_to_websocket() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx.clone(),
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx, mut ws_rx) = ws.split();

        // Give the subscription a moment to register
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Broadcast an event
        let event = OutboundEvent::Status {
            turn: 1,
            phase: "thinking".to_string(),
            autonomy: "medium".to_string(),
            session_id: "test-session".to_string(),
            task: "test task".to_string(),
            external_agent: None,
        };
        crate::control::broadcast_event(&broadcast_tx, &event);

        // Verify the WebSocket client receives it. Other bootstrap snapshots may
        // be sent first.
        let json = next_ws_json_matching(&mut ws_rx, |json| json["event"] == "status").await;
        assert_eq!(json["turn"], 1);

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_html() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Plain HTTP GET
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        // Read with timeout
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("<!DOCTYPE html>"));

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_config() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
            ..Default::default()
        };
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // GET /config
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("application/json"));
        assert!(response_str.contains("\"provider\":\"openai\""));

        handle.abort();
    }

    /// `/config` is scoped to voice/runtime config only after the
    /// AgentCard split. Identity fields (host_label, version, git_sha)
    /// moved to /.well-known/agent-card.json. This test enforces the
    /// boundary so a future code change can't reintroduce drift
    /// between the two by sneaking identity fields back into
    /// WebGatewayConfig.
    #[tokio::test]
    async fn test_config_endpoint_has_no_identity_fields() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));

        // Extract the JSON body (after the header terminator).
        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("body is JSON");
        let obj = parsed.as_object().expect("body is an object");

        assert!(
            obj.contains_key("provider"),
            "should still have runtime fields"
        );
        assert!(obj.contains_key("model"));
        assert!(
            !obj.contains_key("host_label"),
            "host_label must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("version"),
            "version must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("git_sha"),
            "git_sha must live on the agent card, not /config: {obj:?}"
        );

        handle.abort();
    }

    /// `/.well-known/agent-card.json` reflects live daemon state and
    /// deserializes into an [`crate::peer::AgentCard`] with the
    /// expected shape. This is the server-side guardrail the user
    /// asked for — if someone breaks the assembly in
    /// `build_local_agent_card`, the endpoint round-trip fails here
    /// before anyone hits it in the browser.
    #[tokio::test]
    async fn test_agent_card_endpoint_reflects_live_state() {
        use crate::peer::{AgentCard, Capability, TransportAuth, TransportSpec};

        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("200 OK"),
            "agent card endpoint should return 200: {response_str}"
        );
        assert!(response_str.contains("application/json"));

        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let card: AgentCard = serde_json::from_str(body).expect("body deserializes as AgentCard");

        // Identity fields must be populated from live state.
        assert_eq!(
            card.id.kind(),
            Some(crate::peer::id::PeerKind::Intendant),
            "local daemon must identify as Intendant kind: id = {:?}",
            card.id
        );
        assert!(
            card.id.as_str().starts_with("intendant:"),
            "PeerId must have intendant prefix: {}",
            card.id.as_str()
        );
        assert!(
            !card.label.is_empty(),
            "label must be resolved from access::resolve_host_label"
        );
        assert_eq!(
            card.version,
            env!("CARGO_PKG_VERSION"),
            "version must come from CARGO_PKG_VERSION"
        );
        assert_eq!(
            card.git_sha.as_deref(),
            Some(env!("INTENDANT_GIT_SHA")),
            "git_sha must come from INTENDANT_GIT_SHA"
        );

        // Transports must advertise at least the native Intendant WS
        // transport, with a URL that points back at this listener.
        assert_eq!(card.transports.len(), 1, "expected one transport");
        let expected_url_prefix = format!("ws://127.0.0.1:{port}");
        match &card.transports[0] {
            TransportSpec::IntendantWs { url } => {
                assert!(
                    url.starts_with(&expected_url_prefix) && url.ends_with("/ws"),
                    "transport URL {url} should start with {expected_url_prefix} and end with /ws"
                );
            }
            other => panic!("expected IntendantWs transport, got {other:?}"),
        }

        // Phase 1 conservative capability set.
        assert!(
            card.capabilities.contains(&Capability::ComputerUse),
            "card should advertise ComputerUse capability: {:?}",
            card.capabilities
        );
        assert!(
            card.capabilities.contains(&Capability::Knowledge),
            "card should advertise Knowledge capability: {:?}",
            card.capabilities
        );

        // Auth defaults to None in phase 1 (trust the network layer).
        assert!(
            matches!(card.auth.transport, TransportAuth::None) && card.auth.application.is_none(),
            "expected AuthRequirements::none() in phase 1, got {:?}",
            card.auth
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_presence_connect_disconnect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect (new protocol)
        ws.send(Message::Text(
            r#"{"t":"presence_connect","server_session_id":"sess-1","last_event_seq":5}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        match event {
            AppEvent::PresenceConnected {
                server_session_id,
                last_event_seq,
                ..
            } => {
                assert_eq!(server_session_id.as_deref(), Some("sess-1"));
                assert_eq!(last_event_seq, 5);
            }
            _ => panic!("expected PresenceConnected, got {:?}", event),
        }

        // Send presence_disconnect
        ws.send(Message::Text(r#"{"t":"presence_disconnect"}"#.into()))
            .await
            .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    #[tokio::test]
    async fn test_voice_log_forwarding() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        ws.send(Message::Text(
            r#"{"t":"voice_log","text":"hello","seq":3,"tool_context":"check_status"}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        match event {
            AppEvent::VoiceLog {
                text,
                seq,
                tool_context,
            } => {
                assert_eq!(text, "hello");
                assert_eq!(seq, 3);
                assert_eq!(tool_context.as_deref(), Some("check_status"));
            }
            _ => panic!("expected VoiceLog"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_check_status() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Create a query context with a known agent state
        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "thinking".to_string(),
            turn: 3,
            budget_pct: 0.15,
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx_split, mut ws_rx) = ws.split();

        // First message should be the bootstrap state_snapshot
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["state"]["phase"], "thinking");
            assert_eq!(json["state"]["turn"], 3);
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_bootstrap_state_snapshot_uses_daemon_session_without_active_session() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.daemon_session_id = Some("daemon-session".to_string());
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (_ws, mut ws_rx) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap()
            .0
            .split();

        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["session_id"], "daemon-session");
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_bootstrap_state_snapshot_prefers_daemon_over_active_session_log() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let active_log = Arc::new(Mutex::new(
            crate::session_log::SessionLog::open(dir.path().join("active-worker")).unwrap(),
        ));

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            {
                let mut state = ss.write().await;
                state.daemon_session_id = Some("daemon-session".to_string());
                state.session_log = Some(active_log);
            }
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (_ws, mut ws_rx) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap()
            .0
            .split();

        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["session_id"], "daemon-session");
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_response_roundtrip() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "running_agent".to_string(),
            turn: 5,
            budget_pct: 0.42,
            last_command_preview: "cargo test".to_string(),
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a check_status tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_1","tool":"check_status","args":{}}"#.into(),
        ))
        .await
        .unwrap();

        let json = next_ws_json_type(&mut ws, "tool_response").await;
        assert_eq!(json["id"], "req_1");
        let result = json["result"].as_str().unwrap();
        assert!(result.contains("running_agent"), "result: {}", result);
        assert!(result.contains("Turn: 5"), "result: {}", result);

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_action_dispatches_control() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot::default()));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send an approve_action tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_2","tool":"approve_action","args":{"id":42}}"#.into(),
        ))
        .await
        .unwrap();

        // Should emit a ControlCommand(Approve) on the EventBus
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..10 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
            if let AppEvent::ControlCommand(ControlMsg::Approve { id, .. }) = event {
                assert_eq!(id, 42);
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Approve)");

        // Should also get a tool_response back. Other bootstrap snapshots may
        // be sent first.
        let json = next_ws_json_type(&mut ws, "tool_response").await;
        assert_eq!(json["id"], "req_2");
        assert!(json["result"].as_str().unwrap().contains("Approved"));

        handle.abort();
    }

    /// When a WebSocket client that sent `presence_connect` drops without
    /// sending `presence_disconnect`, the server should auto-emit
    /// `PresenceDisconnected` to resume server-side presence.
    #[tokio::test]
    async fn test_ws_drop_auto_sends_presence_disconnected() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the WebSocket WITHOUT sending presence_disconnect
        ws.close(None).await.unwrap();

        // Server should auto-send PresenceDisconnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for auto PresenceDisconnected")
            .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    /// When a client that never sent `presence_connect` drops, no
    /// `PresenceDisconnected` should be emitted.
    #[tokio::test]
    async fn test_ws_drop_no_auto_disconnect_without_presence() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a control action (routes through EventBus regardless of active state)
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Drain events until we see the Status control event
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status { .. })) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        // Drop the WebSocket
        ws.close(None).await.unwrap();

        // Should NOT receive PresenceDisconnected — only a timeout
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(result.is_err(), "expected timeout, got {:?}", result);

        handle.abort();
    }

    /// POST /session returns 502 when no API key is configured.
    #[tokio::test]
    async fn test_post_session_no_api_key() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _gemini_key = EnvVarGuard::unset("GEMINI_API_KEY");
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // POST /session without any API key env var set
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("502 Bad Gateway"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("not set on server"),
            "response: {}",
            response_str
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_audio_processor_js() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(
                b"GET /audio-processor.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("200 OK"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("application/javascript"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("AudioCaptureProcessor"),
            "response: {}",
            response_str
        );

        handle.abort();
    }

    /// First browser to send presence_connect should become active.
    #[tokio::test]
    async fn test_first_browser_becomes_active() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should get PresenceConnected on the bus (active browser emits it)
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive a presence_welcome with is_active: true via direct
        // channel. Other bootstrap snapshots may be sent first.
        let (_ws_tx_split, mut ws_rx) = ws.split();
        let json = next_ws_json_type(&mut ws_rx, "presence_welcome").await;
        assert_eq!(json["is_active"], true);

        handle.abort();
    }

    /// Second browser to send presence_connect should be passive (no PresenceConnected emitted).
    #[tokio::test]
    async fn test_second_browser_is_passive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects — becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Drain PresenceConnected from first browser
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Second browser connects — should be passive
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should NOT receive PresenceConnected on bus (passive)
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "passive browser should not emit PresenceConnected"
        );

        // Second browser should receive welcome with is_active: false
        // Drain bootstrap state_snapshot first
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(
                                json["is_active"], false,
                                "second browser should be passive"
                            );
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_welcome,
            "second browser should receive presence_welcome"
        );

        handle.abort();
    }

    /// When second browser sends make_active, the first should receive force_disconnect_voice.
    #[tokio::test]
    async fn test_make_active_handover() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // Browser 1 connects and becomes active
        let (ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws1_tx, mut ws1_rx) = ws1.split();
        ws1_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain ws1's bootstrap + welcome messages
        for _ in 0..3 {
            let _ =
                tokio::time::timeout(tokio::time::Duration::from_millis(300), ws1_rx.next()).await;
        }

        // Browser 2 connects (passive — no presence_connect yet, just make_active)
        let (ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws2_tx, mut ws2_rx) = ws2.split();

        // Drain ws2's bootstrap state_snapshot
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(300), ws2_rx.next()).await;

        // Browser 2 sends make_active
        ws2_tx
            .send(Message::Text(r#"{"t":"make_active"}"#.into()))
            .await
            .unwrap();

        // Browser 1 should receive force_disconnect_voice
        let mut found_force_disconnect = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws1_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "force_disconnect_voice" {
                            assert_eq!(json["reason"], "handover");
                            found_force_disconnect = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_force_disconnect,
            "browser 1 should receive force_disconnect_voice"
        );

        // Browser 2 should receive active_granted
        let mut found_active_granted = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "active_granted" {
                            assert_eq!(json["is_active"], true);
                            found_active_granted = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_active_granted,
            "browser 2 should receive active_granted"
        );

        // EventBus should have received a new PresenceConnected for browser 2
        let mut found_connected = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(AppEvent::PresenceConnected { .. })) => {
                    found_connected = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(found_connected, "make_active should emit PresenceConnected");

        handle.abort();
    }

    /// When the active browser drops, the next browser to connect should get active.
    #[tokio::test]
    async fn test_active_drop_clears_slot() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects and becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the active browser
        ws1.close(None).await.unwrap();

        // Should get PresenceDisconnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceDisconnected));

        // Give server a moment to process the drop
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Second browser connects — should now become active
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should get PresenceConnected (new active)
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive welcome with is_active: true
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(json["is_active"], true);
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_welcome,
            "new browser should be active after old one dropped"
        );

        handle.abort();
    }

    /// An already-active browser re-sending presence_connect (e.g. after voice reconnect)
    /// should receive is_active: true and NOT emit a duplicate PresenceConnected.
    #[tokio::test]
    async fn test_active_browser_resend_presence_connect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws_tx, mut ws_rx) = ws.split();

        // First presence_connect — becomes active
        ws_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Drain PresenceConnected from first connect
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain welcome + bootstrap messages
        for _ in 0..5 {
            let _ =
                tokio::time::timeout(tokio::time::Duration::from_millis(200), ws_rx.next()).await;
        }

        // Re-send presence_connect (simulates voice reconnect after handover)
        ws_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Should receive welcome with is_active: true (still active)
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(
                                json["is_active"], true,
                                "already-active browser should still be active on re-connect"
                            );
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_welcome, "should receive presence_welcome");

        // Should NOT get a duplicate PresenceConnected on the bus
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "should not emit duplicate PresenceConnected for already-active browser"
        );

        handle.abort();
    }

    // ── HTTP/1.1 keep-alive (the per-connection request loop) ──

    /// Spawn a bare test gateway (no session state, no TLS) and return
    /// its port + task handle — the common preamble of the keep-alive
    /// tests below.
    async fn spawn_keep_alive_test_gateway() -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Read exactly ONE HTTP response off a kept-alive connection: the
    /// head through `\r\n\r\n`, then exactly `Content-Length` body
    /// bytes. Asserts the server sent nothing beyond the response — on
    /// a parked connection stray bytes would be protocol corruption.
    async fn read_one_http_response(stream: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut bytes: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), stream.read(&mut chunk))
                    .await
                    .expect("response head timed out")
                    .expect("response head read failed");
            assert!(n > 0, "connection closed mid-head");
            bytes.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
        let content_length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map(|value| value.trim().parse().expect("Content-Length parses"))
            .unwrap_or(0);
        while bytes.len() < head_end + content_length {
            let n =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), stream.read(&mut chunk))
                    .await
                    .expect("response body timed out")
                    .expect("response body read failed");
            assert!(n > 0, "connection closed mid-body");
            bytes.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(
            bytes.len(),
            head_end + content_length,
            "server sent bytes beyond the framed response"
        );
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn test_http_keep_alive_reuses_connection() {
        use tokio::io::AsyncWriteExt;
        let (port, handle) = spawn_keep_alive_test_gateway().await;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        // Request 1: HTTP/1.1 defaults to keep-alive; the framed JSON
        // response advertises it and the connection stays open.
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let resp1 = read_one_http_response(&mut stream).await;
        assert!(resp1.starts_with("HTTP/1.1 200 OK\r\n"), "{resp1}");
        assert!(resp1.contains("Connection: keep-alive\r\n"), "{resp1}");
        assert!(
            resp1.contains(&format!("Keep-Alive: timeout={KEEP_ALIVE_IDLE_SECS}\r\n")),
            "{resp1}"
        );

        // Request 2 rides the SAME connection through the route-table
        // funnel (write_api_response): the rewritten header tail must
        // advertise keep-alive there too.
        stream
            .write_all(b"GET /api/project-root HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let resp2 = read_one_http_response(&mut stream).await;
        assert!(resp2.starts_with("HTTP/1.1 200 OK\r\n"), "{resp2}");
        assert!(resp2.contains("Connection: keep-alive\r\n"), "{resp2}");
        assert!(!resp2.contains("Connection: close"), "{resp2}");

        // Request 3: a static-asset chain arm, same connection still.
        stream
            .write_all(b"GET /audio-processor.js HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let resp3 = read_one_http_response(&mut stream).await;
        assert!(resp3.starts_with("HTTP/1.1 200 OK\r\n"), "{resp3}");
        assert!(resp3.contains("Connection: keep-alive\r\n"), "{resp3}");

        // Request 4 says close: the server honors it and the connection
        // ends cleanly right after the response.
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let resp4 = read_one_http_response(&mut stream).await;
        assert!(resp4.starts_with("HTTP/1.1 200 OK\r\n"), "{resp4}");
        assert!(resp4.contains("Connection: close\r\n"), "{resp4}");
        assert!(!resp4.contains("Connection: keep-alive"), "{resp4}");
        let mut rest = Vec::new();
        let n = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut rest),
        )
        .await
        .expect("EOF after Connection: close")
        .expect("clean EOF read");
        assert_eq!(n, 0, "no bytes may follow a Connection: close response");

        handle.abort();
    }

    #[tokio::test]
    async fn test_http10_request_closes_by_default() {
        use tokio::io::AsyncWriteExt;
        let (port, handle) = spawn_keep_alive_test_gateway().await;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        stream
            .write_all(b"GET /config HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await
        .expect("HTTP/1.0 response must end in EOF promptly")
        .unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("Connection: close\r\n"), "{response}");
        assert!(!response.contains("Connection: keep-alive"), "{response}");
        handle.abort();
    }

    #[tokio::test]
    async fn test_ws_upgrade_on_kept_alive_connection() {
        use tokio::io::AsyncWriteExt;
        let (port, handle) = spawn_keep_alive_test_gateway().await;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();

        // Plain request first; the connection parks for reuse.
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let resp = read_one_http_response(&mut stream).await;
        assert!(resp.contains("Connection: keep-alive\r\n"), "{resp}");

        // A WebSocket upgrade arrives as the SECOND request on the same
        // connection: the loop replays the captured head to the upgrade
        // handshake and hands the connection off — it must never keep
        // looping past an upgrade.
        let (ws, upgrade_response) =
            tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/ws"), stream)
                .await
                .expect("WS upgrade on a kept-alive connection");
        assert_eq!(
            upgrade_response.status(),
            tokio_tungstenite::tungstenite::http::StatusCode::SWITCHING_PROTOCOLS
        );
        drop(ws);
        handle.abort();
    }

    #[tokio::test]
    async fn test_http_keep_alive_request_budget_closes_at_cap() {
        use tokio::io::AsyncWriteExt;
        let (port, handle) = spawn_keep_alive_test_gateway().await;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        for i in 1..=KEEP_ALIVE_MAX_REQUESTS {
            stream
                .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let resp = read_one_http_response(&mut stream).await;
            if i < KEEP_ALIVE_MAX_REQUESTS {
                assert!(
                    resp.contains("Connection: keep-alive\r\n"),
                    "request {i}: {resp}"
                );
            } else {
                // The budget-exhausting request answers close…
                assert!(
                    resp.contains("Connection: close\r\n"),
                    "request {i}: {resp}"
                );
            }
        }
        // …and the connection really ends.
        let mut rest = Vec::new();
        let n = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut rest),
        )
        .await
        .expect("EOF after the request budget is spent")
        .expect("clean EOF read");
        assert_eq!(n, 0);
        handle.abort();
    }
}
