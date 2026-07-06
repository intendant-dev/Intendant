//! The gateway listener: TLS accept + failure rate-limiting, dead-listener
//! rebind, and `spawn_web_gateway` — the accept loop with the per-connection
//! HTTP demux/dispatch and websocket inbound/outbound tasks nested inside.
//! (`spawn_web_gateway` is a single ~5k-line function; pure moves cannot
//! split a function body — extracting its nested tasks is a separate,
//! behavior-aware refactor.)

use super::*;

pub(crate) const TLS_FAILURE_LOG_INTERVAL_SECS: u64 = 30;

#[derive(Debug)]
pub(crate) struct TlsFailureLogEntry {
    last_logged: std::time::Instant,
    suppressed: u64,
}

pub(crate) type TlsFailureLogState = Arc<Mutex<HashMap<String, TlsFailureLogEntry>>>;

pub(crate) fn log_tls_failure_rate_limited(state: &TlsFailureLogState, peer: &str, kind: &str, detail: &str) {
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

// Exact fork baselines are a synchronous `/api/sessions` refinement. The scanner
// below parses compact Codex token lines without materializing full JSON values.
// Exact fork baselines come from scanning the parent's log (results
// persist per file-state in the codex-parent-baseline namespace, so each
// scan happens once). The per-file cap covers the largest observed
// rollouts; parents past the per-build budget pick up their baseline on a
// later list pass.

/// Consecutive "fatal-class" accept failures tolerated on the same socket
/// before it is dropped and rebound. EINVAL has been observed twice on
/// macOS (2026-07-04, both times within ~1s of an external-agent spawn)
/// on a listener that remained LISTEN at the kernel afterwards — treating
/// the first one as fatal is what actually broke the dashboard. A short
/// streak (~2s) absorbs the spurious case; a genuinely dead socket fails
/// every retry and reaches the rebind path.
pub(crate) const FATAL_ACCEPT_REBIND_THRESHOLD: u32 = 8;

pub(crate) fn should_continue_after_accept_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::TimedOut => return true,
        std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::NotFound
        | std::io::ErrorKind::PermissionDenied => return false,
        _ => {}
    }

    match error.raw_os_error() {
        // The listener file descriptor/socket is invalid or no longer a
        // listening socket (EBADF/EINVAL/ENOTSOCK). Retrying accept() on it
        // would spin forever — the caller rebinds a fresh listener instead.
        Some(9 | 22 | 38) => false,
        // Process/system descriptor pressure and socket buffer pressure are
        // recoverable after current connections close. Keep the gateway alive
        // so the dashboard recovers instead of becoming half-alive.
        Some(23 | 24 | 55) => true,
        // Unknown accept errors are safer to treat as per-connection failures:
        // losing one inbound connection is better than dropping the dashboard
        // listener while existing WebSocket tasks make the UI look alive.
        _ => true,
    }
}

/// Rebind a TCP listener on its original address after the previous
/// socket became unusable — seen in the wild on macOS as `accept()`
/// returning EINVAL a minute into an app-spawned daemon's life, which
/// used to kill the listener task and leave the dashboard half-alive
/// (established WebSockets kept flowing while every new connection —
/// session details, files, uploads, Station assets — failed). Mirrors
/// `bind_dual_stack_or_v4`: dual-stack for the IPv6 wildcard,
/// `SO_REUSEADDR` so lingering TIME_WAIT sockets don't block the port.
/// Shared by the dashboard gateway and the enrollment cert server.
pub(crate) fn rebind_dead_tcp_listener(
    addr: std::net::SocketAddr,
) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() && addr.ip().is_unspecified() {
        let _ = socket.set_only_v6(false);
    }
    let _ = socket.set_reuse_address(true);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
}


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
    // Inbound bearer token enforcement. When `Some`, federation REST
    // endpoints (/api/peers*, /api/coordinator/*, /api/sessions)
    // require `Authorization: Bearer <token>` matching the configured
    // value; missing or wrong token returns 401. When `None`, no
    // application-layer auth is enforced — the operator's expected to
    // rely on transport security (mTLS proxy, tailnet, loopback).
    // Sourced from `[server.auth] bearer_token` in intendant.toml.
    //
    // /ws, /.well-known/agent-card.json, /config, the dashboard HTML,
    // and static assets are intentionally exempt in this slice — /ws
    // enforcement requires a parallel dashboard auth flow (browser
    // can't easily set headers on `WebSocket` opens) which lands in
    // slice 2d.
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
    // public peer-access requests can reach the doorbell endpoint, but every
    // other HTTP/WS path is rejected unless rustls verified a client cert.
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
    let display_input_authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>> =
        Arc::new(StdRwLock::new(HashMap::new()));

    // Phase 5a.1 authority transition channel.  Each per-connection
    // outbound task subscribes; emit sites are the Request/Release
    // ControlMsg handlers, the WS-close cleanup, and the DisplayReady
    // listener that fires `holder: None` for freshly
    // created display sessions so already-connected browsers move
    // from `unknown` to `unclaimed`.
    let (authority_change_tx, _authority_change_rx0) =
        broadcast::channel::<DisplayInputAuthorityChange>(AUTHORITY_CHANGE_CAPACITY);

    let (dashboard_authority_change_tx, _dashboard_authority_change_rx0) =
        broadcast::channel::<u32>(AUTHORITY_CHANGE_CAPACITY);
    {
        let mut authority_change_rx = authority_change_tx.subscribe();
        let dashboard_authority_change_tx = dashboard_authority_change_tx.clone();
        tokio::spawn(async move {
            loop {
                match authority_change_rx.recv().await {
                    Ok(change) => {
                        let _ = dashboard_authority_change_tx.send(change.display_id);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    let dashboard_display_authority = {
        let snapshot_authority = Arc::clone(&display_input_authority);
        let state_authority = Arc::clone(&display_input_authority);
        let request_authority = Arc::clone(&display_input_authority);
        let request_change_tx = authority_change_tx.clone();
        let release_authority = Arc::clone(&display_input_authority);
        let release_change_tx = authority_change_tx.clone();
        let input_authority = Arc::clone(&display_input_authority);
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
                apply_grant_input_authority_dashboard_control(
                    display_id,
                    session_id.to_string(),
                    &request_authority,
                    &request_change_tx,
                );
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
    ));
    let _connect_rendezvous_handle = crate::connect_rendezvous::spawn_connect_rendezvous_client(
        config.connect.clone(),
        dashboard_control.clone(),
    );

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

    // Spawn a listener that fires an "unclaimed" authority change for
    // every newly-created display session so already-connected browsers'
    // chips flip from `unknown` to `unclaimed` without waiting for the
    // first Request/Release.  Subscribes to the broadcast_tx event
    // stream (already serialized JSON) and pattern-matches on
    // `display_ready` rather than the typed AppEvent — same source the
    // existing `display_ready_cache` task uses, keeps the dependency
    // surface small.
    {
        let authority_change_tx = authority_change_tx.clone();
        let mut display_events_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match display_events_rx.recv().await {
                    Ok(line) => {
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                let _ = authority_change_tx.send(DisplayInputAuthorityChange {
                                    display_id: did,
                                    holder: None,
                                });
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

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
    {
        let usage_cache = last_usage_json.clone();
        let live_usage_cache = last_live_usage_json.clone();
        let status_cache = last_status_json.clone();
        let autonomy_cache = last_autonomy_json.clone();
        let external_agent_cache = last_external_agent_json.clone();
        let session_attached_cache = attached_external_sessions.clone();
        let user_display_cache = last_user_display_json.clone();
        let session_state_cache = session_state_lines.clone();
        let display_cache = display_ready_cache.clone();
        let mut usage_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match usage_rx.recv().await {
                    Ok(line) => {
                        // Cache display_ready events per display_id for
                        // late-connecting browsers.
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.insert(did, line.clone());
                                }
                            }
                        }
                        // Evict display_ready cache when display is revoked.
                        if line.contains("\"event\":\"user_display_revoked\"")
                            || line.contains("\"event\":\"display_capture_lost\"")
                        {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.remove(&did);
                                }
                            }
                        }
                        // Cache user_display_granted for replay on reconnect.
                        // Clear the cache on user_display_revoked so a refreshed
                        // browser after a revoke doesn't re-enable the badge.
                        if line.contains("\"event\":\"user_display_granted\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"user_display_revoked\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = None;
                            }
                        }
                        if line.contains("\"event\":\"usage_update\"")
                            || line.contains("\"event\":\"usage\"")
                        {
                            if let Ok(mut guard) = usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"live_usage_update\"") {
                            if let Ok(mut guard) = live_usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"status\"") {
                            if let Ok(mut guard) = status_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"autonomy_changed\"") {
                            if let Ok(mut guard) = autonomy_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"external_agent_changed\"") {
                            if let Ok(mut guard) = external_agent_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"session_attached\"")
                            || line.contains("\"event\":\"session_identity\"")
                        {
                            if let Ok(mut guard) = session_attached_cache.lock() {
                                update_external_attached_sessions_from_wire(&mut guard, &line);
                            }
                        }
                        if line.contains("\"event\":\"session_vitals\"")
                            || line.contains("\"event\":\"session_goal\"")
                        {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let kind = match parsed["event"].as_str() {
                                    Some("session_vitals") => Some("session_vitals"),
                                    Some("session_goal") => Some("session_goal"),
                                    _ => None,
                                };
                                if let (Some(kind), Some(sid)) =
                                    (kind, parsed["session_id"].as_str())
                                {
                                    if let Ok(mut guard) = session_state_cache.lock() {
                                        guard
                                            .entry(sid.to_string())
                                            .or_default()
                                            .insert(kind, line.clone());
                                        // Bound the cache against a runaway
                                        // session-id source. session_ended is
                                        // the normal prune; this evicts the
                                        // lexicographically first key, which
                                        // is arbitrary but keeps it finite.
                                        if guard.len() > 256 {
                                            if let Some(first) = guard.keys().next().cloned() {
                                                guard.remove(&first);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if line.contains("\"event\":\"session_ended\"") {
                            if let Ok(mut guard) = session_attached_cache.lock() {
                                update_external_attached_sessions_from_wire(&mut guard, &line);
                            }
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                if let Some(sid) = parsed["session_id"].as_str() {
                                    if let Ok(mut guard) = session_state_cache.lock() {
                                        guard.remove(sid);
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

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
    let app_html_override: Option<Arc<std::path::Path>> =
        app_html_override_path().map(Arc::from);
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

    tokio::spawn(async move {
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
            let task_tx = task_tx.clone();
            let project_root = project_root.clone();
            let mcp_server = mcp_server.clone();
            let terminal_registry = terminal_registry.clone();
            let inbound_bearer_token = inbound_bearer_token.clone();
            let worktree_inventory_cache = worktree_inventory_cache.clone();
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
                    let response = HttpResponse::with_content("426 Upgrade Required", "text/plain", body)
                        .header("Upgrade", "TLS/1.2")
                        .header("Connection", "close")
                        .into_string();
                    let _ = raw_stream.write_all(response.as_bytes()).await;
                    let _ = raw_stream.shutdown().await;
                    return;
                }

                let buf_owned: Vec<u8>;
                let n: usize;
                let mut stream: DemuxStream;
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
                    n = read_n;
                    buf_owned = decrypted.clone();
                    // Replay the decrypted request head in front of the TLS
                    // stream so the WS upgrade / HTTP body reads downstream
                    // see the request from byte zero.
                    stream = Box::pin(crate::web_tls::PrefixedStream::new(decrypted, tls_stream));
                } else {
                    // Plain HTTP/WS: the peeked bytes are still in the
                    // kernel buffer. Box the raw stream with an empty
                    // replay prefix — a zero-overhead pass-through that
                    // reads the request straight from the socket.
                    n = peeked;
                    buf_owned = buf[..peeked].to_vec();
                    tls_client_cert_present = false;
                    tls_client_cert_fingerprint = None;
                    stream = Box::pin(crate::web_tls::PrefixedStream::new(Vec::new(), raw_stream));
                }
                // Downstream code reads `buf[..n]`; point `buf` at the
                // (decrypted, for TLS) request head we just captured.
                let buf = buf_owned.as_slice();

                let header_text = String::from_utf8_lossy(&buf[..n]);
                let request_line = header_text.lines().next().unwrap_or("");
                let peer_connection_identity = match resolve_peer_connection_identity(
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
                        let response = HttpResponse::with_content(format!("{} {}", status, reason), "application/json", body)
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
                    if tls_client_cert_required && !tls_client_cert_present {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": "mTLS client certificate required"
                        })
                        .to_string();
                        let response = HttpResponse::with_content("401 Unauthorized", "application/json", body)
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
                        let response = HttpResponse::with_content(format!("{} {}", status, reason), "application/json", body)
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
                    let cert_dir = crate::access::backend::select_backend().cert_dir();
                    let dashboard_control_grant_for_ws = match dashboard_control_grant_for_client(
                        &cert_dir,
                        peer_connection_identity.as_ref(),
                        tls_client_cert_fingerprint.as_deref(),
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
                    let peer_identity_for_ws = peer_connection_identity.clone();
                    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };

                    let (mut ws_tx, mut ws_rx) = ws_stream.split();
                    let mut outbound_rx = broadcast_tx.subscribe();

                    // Per-connection identity for active/passive tracking
                    let connection_id = uuid::Uuid::new_v4().to_string();

                    // Direct response channel: tool_response and state_snapshot
                    // messages for this specific connection (not broadcast).
                    let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<String>();

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
                    // (session_vitals / session_goal). These fire on change
                    // only, so without this a late joiner — a refreshed
                    // browser on an idle daemon, or a peer transport
                    // attaching — would never see state that last changed
                    // before this connection existed.
                    let session_state_replay: Vec<String> = session_state_lines
                        .lock()
                        .map(|guard| {
                            guard
                                .values()
                                .flat_map(|kinds| kinds.values().cloned())
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
                            let active_ids: Vec<u32> = reg.display_ids();
                            // Snapshot resolutions + auth states under the
                            // std lock, then drop the guard before any
                            // direct_tx.send calls.
                            let resolutions: Vec<(u32, u32, u32)> = active_ids
                                .iter()
                                .filter_map(|&did| {
                                    reg.get(did).map(|session| {
                                        let (w, h) = session.resolution();
                                        (did, w, h)
                                    })
                                })
                                .collect();
                            let auth_snapshots = {
                                let auth = display_input_authority
                                    .read()
                                    .unwrap_or_else(|e| e.into_inner());
                                compute_bootstrap_authority_snapshots(
                                    resolutions.iter().map(|(did, _, _)| *did),
                                    &auth,
                                    &connection_id,
                                )
                            };
                            // Send the display_ready frames now; defer the
                            // authority frames until after log_replay.
                            for (did, w, h) in resolutions {
                                let ready = serde_json::json!({
                                    "event": "display_ready",
                                    "display_id": did,
                                    "width": w,
                                    "height": h,
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
                    if let Some(ref log_dir) = replay_log_dir {
                        if let Some((replay, external_session_id)) =
                            session_log_replay_payload_from_dir_with_limit(
                                log_dir,
                                Some(WEBSOCKET_BOOTSTRAP_REPLAY_ENTRY_LIMIT),
                            )
                        {
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
                    let home_dir = crate::platform::home_dir();
                    for (session_id, source) in active_external_sessions {
                        let wrapper_replay_is_current = replayed_external_session_ids
                            .contains(&session_id)
                            && replay_log_dir.as_ref().is_some_and(|log_dir| {
                                !external_session_newer_than_wrapper(
                                    &home_dir,
                                    log_dir,
                                    &source,
                                    &session_id,
                                )
                            });
                        if wrapper_replay_is_current {
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

                    let bus_inbound = bus.clone();
                    let query_ctx_inbound = query_ctx.clone();
                    let direct_tx_inbound = direct_tx.clone();
                    let voice_debug_inbound = voice_debug.clone();
                    let live_provider = session_provider.clone();
                    let live_model = session_model.clone();
                    let transcriber_inbound = transcriber.clone();
                    let active_presence_inbound = active_presence.clone();
                    let display_input_authority_inbound = display_input_authority.clone();
                    let authority_change_tx_inbound = authority_change_tx.clone();
                    let federated_authority_subscribers_inbound =
                        federated_authority_subscribers.clone();
                    let connection_id_inbound = connection_id.clone();
                    let frame_registry_inbound = frame_registry.clone();
                    let recording_registry_inbound = recording_registry.clone();
                    let session_log_inbound = session_log.clone();
                    let session_registry_inbound = session_registry.clone();
                    let task_tx_inbound = task_tx.clone();
                    let terminal_registry_inbound = terminal_registry.clone();
                    let dashboard_control_inbound = Arc::clone(&dashboard_control);
                    let dashboard_control_grant_inbound = dashboard_control_grant_for_ws.clone();
                    let peer_file_transfer_registry_inbound =
                        Arc::clone(&peer_file_transfer_registry);
                    let peer_identity_inbound = peer_identity_for_ws.clone();
                    let inbound = tokio::spawn(async move {
                        // Track whether this connection has an active presence model,
                        // so we can auto-send PresenceDisconnected if the WebSocket drops
                        // without a clean presence_disconnect message (e.g. tab close
                        // before beforeunload fires, network failure).
                        let mut is_presence_connected = false;
                        // Whether this connection is the active voice owner
                        let mut is_active = false;

                        // Per-connection clip accumulators for batched clip_frame messages
                        struct ClipAccumulator {
                            stream: String,
                            note: String,
                            inject: bool,
                            in_secs: f64,
                            out_secs: f64,
                            fps: u32,
                            #[allow(dead_code)]
                            expected: usize,
                            frames: Vec<(String, String)>, // (frame_id, base64_data)
                        }
                        let mut clip_accumulators: std::collections::HashMap<
                            String,
                            ClipAccumulator,
                        > = std::collections::HashMap::new();

                        // Display IDs this peer has WebRTC connections to,
                        // used for cleanup when the WebSocket disconnects.
                        let mut peer_display_ids: Vec<u32> = Vec::new();
                        let mut dashboard_control_session_ids: Vec<String> = Vec::new();

                        // Frame types already denied+logged once on this
                        // connection — dedupes the warn log only; the denial
                        // frame itself is sent for every rejected frame.
                        let mut ws_denied_logged: std::collections::HashSet<String> =
                            std::collections::HashSet::new();

                        // Shell-session lane for this connection: root sees
                        // every session, scoped principals see owned/shared.
                        let ws_terminal_actor = dashboard_control_grant_inbound.terminal_actor();

                        // Per-connection audio transcription buffer.
                        // PCM16 bytes are accumulated and drained every ~3s.
                        let mut audio_buf: Vec<u8> = Vec::new();
                        let mut audio_seq: u64 = 0;
                        // Input sample rate (known from config, default 16kHz)
                        let audio_sample_rate: u32 = 16000;

                        while let Some(Ok(msg)) = ws_rx.next().await {
                            if let Message::Text(text) = msg {
                                let trimmed = text.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                // Try to parse as JSON for type-tagged messages
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
                                {
                                    // Per-frame IAM enforcement on the direct
                                    // /ws path — the same frame→operation
                                    // table the dashboard-control tunnel
                                    // enforces, so a scoped grant means the
                                    // same thing on every transport.
                                    if deny_ws_frame_if_unauthorized(
                                        &dashboard_control_grant_inbound,
                                        &json,
                                        &direct_tx_inbound,
                                        &bus_inbound,
                                        &mut ws_denied_logged,
                                    ) {
                                        continue;
                                    }
                                    match json.get("t").and_then(|v| v.as_str()) {
                                        Some("presence_connect") => {
                                            is_presence_connected = true;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = true;
                                            let server_session_id = json
                                                .get("server_session_id")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            let last_event_seq = json
                                                .get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);
                                            // Use provider/model from the browser if sent,
                                            // fall back to config defaults.
                                            let msg_provider = json
                                                .get("provider")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_provider.clone());
                                            let msg_model = json
                                                .get("model")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_model.clone());

                                            // Determine if this connection becomes active or passive.
                                            // Browsers can request always-passive mode (observer/follow-along).
                                            let force_passive = json
                                                .get("passive")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(false);
                                            let becomes_active = if force_passive {
                                                false
                                            } else {
                                                let slot = active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                // Empty slot → first connect wins.
                                                // Slot occupied by THIS connection → already active
                                                // (happens when active browser reconnects voice after handover).
                                                slot.is_none()
                                                    || slot
                                                        .as_ref()
                                                        .map(|a| {
                                                            a.connection_id == connection_id_inbound
                                                        })
                                                        .unwrap_or(false)
                                            };

                                            let was_already_active = is_active;
                                            if becomes_active {
                                                // First-connect wins (or re-confirm already-active)
                                                *active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner()) =
                                                    Some(ActivePresence {
                                                        connection_id: connection_id_inbound
                                                            .clone(),
                                                        direct_tx: direct_tx_inbound.clone(),
                                                    });
                                                is_active = true;
                                            }

                                            // Send welcome with replay window if presence session is available
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                // Build conversation context from recent voice transcripts
                                                let conversation_ctx =
                                                    presence::build_conversation_context(
                                                        &ctx.log_dir,
                                                        20,
                                                    );

                                                if let Some(ref ps) = ctx.presence_session {
                                                    let mut session = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    if becomes_active {
                                                        session.set_connected(true);
                                                    }
                                                    let state = ctx
                                                        .agent_state
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .clone();
                                                    let welcome = session
                                                        .build_welcome(last_event_seq, &state);
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "session_id": welcome.session_id,
                                                        "state": welcome.state,
                                                        "events": welcome.events,
                                                        "last_checkpoint_summary": welcome.last_checkpoint_summary,
                                                        "current_seq": welcome.current_seq,
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound
                                                        .send(welcome_msg.to_string());
                                                } else {
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound
                                                        .send(welcome_msg.to_string());
                                                }
                                            } else {
                                                // No presence session — still send a minimal welcome with is_active
                                                let welcome_msg = serde_json::json!({
                                                    "t": "presence_welcome",
                                                    "is_active": becomes_active,
                                                });
                                                let _ =
                                                    direct_tx_inbound.send(welcome_msg.to_string());
                                            }

                                            // Only emit PresenceConnected for the active browser
                                            // (passive browsers don't pause server-side presence).
                                            // Skip if already active (e.g. voice reconnect after make_active
                                            // handover — PresenceConnected was already emitted by make_active).
                                            if becomes_active && !was_already_active {
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_connected(
                                                            Some(&msg_provider),
                                                            Some(&msg_model),
                                                        );
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceConnected {
                                                    server_session_id,
                                                    last_event_seq,
                                                    live_provider: Some(msg_provider),
                                                    live_model: Some(msg_model),
                                                });
                                            }
                                        }
                                        Some("presence_disconnect") => {
                                            is_presence_connected = false;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = false;
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    ps.lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .set_connected(false);
                                                }
                                            }
                                            // Only emit PresenceDisconnected if this was the active browser
                                            if is_active {
                                                // Clear the active slot
                                                let mut slot = active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                if slot
                                                    .as_ref()
                                                    .map(|a| {
                                                        a.connection_id == connection_id_inbound
                                                    })
                                                    .unwrap_or(false)
                                                {
                                                    *slot = None;
                                                }
                                                is_active = false;
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_disconnected();
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceDisconnected);
                                            }
                                        }
                                        Some("make_active") => {
                                            // Request to become the active voice owner
                                            let mut slot = active_presence_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            let previous_active = slot
                                                .as_ref()
                                                .map(|active| active.connection_id.clone());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_received_gateway",
                                                        &format!(
                                                            "request from connection={} previous_active={}",
                                                            connection_id_inbound,
                                                            previous_active.as_deref().unwrap_or("none"),
                                                        ),
                                                    );
                                                }
                                            }

                                            // Tell old active to disconnect voice
                                            if let Some(ref old) = *slot {
                                                if old.connection_id != connection_id_inbound {
                                                    let force_msg = serde_json::json!({
                                                        "t": "force_disconnect_voice",
                                                        "reason": "handover",
                                                    });
                                                    let _ =
                                                        old.direct_tx.send(force_msg.to_string());
                                                    if let Some(ref sl) = session_log_inbound {
                                                        if let Ok(mut l) = sl.lock() {
                                                            l.voice_diagnostic(
                                                                "make_active_force_disconnect_gateway",
                                                                &format!(
                                                                    "old_active={} new_active={}",
                                                                    old.connection_id, connection_id_inbound,
                                                                ),
                                                            );
                                                        }
                                                    }
                                                } else if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.voice_diagnostic(
                                                            "make_active_noop_gateway",
                                                            &format!(
                                                                "request from already-active connection={}",
                                                                connection_id_inbound,
                                                            ),
                                                        );
                                                    }
                                                }
                                            }

                                            // Install this connection as new active
                                            *slot = Some(ActivePresence {
                                                connection_id: connection_id_inbound.clone(),
                                                direct_tx: direct_tx_inbound.clone(),
                                            });
                                            drop(slot);

                                            is_active = true;
                                            is_presence_connected = true;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = true;

                                            // Build handover context from latest checkpoint
                                            let handover_context = query_ctx_inbound
                                                .as_ref()
                                                .and_then(|ctx| ctx.presence_session.as_ref())
                                                .and_then(|ps| {
                                                    let session = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    session.last_checkpoint_summary()
                                                })
                                                .unwrap_or_default();

                                            // Build conversation context from recent voice transcripts
                                            let conversation_ctx =
                                                query_ctx_inbound.as_ref().and_then(|ctx| {
                                                    presence::build_conversation_context(
                                                        &ctx.log_dir,
                                                        20,
                                                    )
                                                });
                                            let has_handover_context = !handover_context.is_empty();
                                            let has_conversation_context = conversation_ctx
                                                .as_deref()
                                                .map(|s| !s.is_empty())
                                                .unwrap_or(false);

                                            // Send active_granted to this connection
                                            let granted_msg = serde_json::json!({
                                                "t": "active_granted",
                                                "is_active": true,
                                                "handover_context": handover_context,
                                                "conversation_context": conversation_ctx,
                                            });
                                            let _ = direct_tx_inbound.send(granted_msg.to_string());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_granted_gateway",
                                                        &format!(
                                                            "connection={} handover_context={} conversation_context={}",
                                                            connection_id_inbound,
                                                            if has_handover_context { "yes" } else { "no" },
                                                            if has_conversation_context { "yes" } else { "no" },
                                                        ),
                                                    );
                                                }
                                            }

                                            // Emit PresenceConnected for the new active browser
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_connected(
                                                        Some(&live_provider),
                                                        Some(&live_model),
                                                    );
                                                }
                                            }
                                            bus_inbound.send(AppEvent::PresenceConnected {
                                                server_session_id: None,
                                                last_event_seq: 0,
                                                live_provider: Some(live_provider.clone()),
                                                live_model: Some(live_model.clone()),
                                            });
                                        }
                                        Some("voice_log") => {
                                            let text =
                                                json["text"].as_str().unwrap_or("").to_string();
                                            let seq = json["seq"].as_u64().unwrap_or(0);
                                            let tool_context = json
                                                .get("tool_context")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            {
                                                let mut vd = voice_debug_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                vd.voice_log_count += 1;
                                                vd.last_voice_log = text.clone();
                                            }
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_log(
                                                        &text,
                                                        seq,
                                                        tool_context.as_deref(),
                                                    );
                                                }
                                            }
                                            bus_inbound.send(AppEvent::VoiceLog {
                                                text,
                                                seq,
                                                tool_context,
                                            });
                                        }
                                        Some("live_usage_update") => {
                                            bus_inbound.send(AppEvent::LiveUsageUpdate {
                                                provider: json["provider"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                model: json["model"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                input_tokens: json["input_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_tokens: json["output_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_tokens: json["cached_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                total_tokens: json["total_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                thinking_tokens: json["thinking_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_text_tokens: json["input_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_audio_tokens: json["input_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_image_tokens: json["input_image_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_text_tokens: json["cached_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_audio_tokens: json["cached_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_image_tokens: json["cached_image_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_text_tokens: json["output_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_audio_tokens: json["output_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                            });
                                        }
                                        Some("presence_checkpoint") => {
                                            let summary =
                                                json["summary"].as_str().unwrap_or("").to_string();
                                            let last_event_seq = json
                                                .get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);

                                            // Record checkpoint and send ack
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    let checkpoint =
                                                        presence_core::PresenceCheckpoint {
                                                            summary: summary.clone(),
                                                            last_event_seq,
                                                        };
                                                    let ack = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .record_checkpoint(checkpoint);
                                                    let ack_msg = serde_json::json!({
                                                        "t": "presence_checkpoint_ack",
                                                        "seq": ack.seq,
                                                    });
                                                    let _ =
                                                        direct_tx_inbound.send(ack_msg.to_string());
                                                }
                                            }

                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_checkpoint(&summary, last_event_seq);
                                                }
                                            }
                                            bus_inbound.send(
                                                AppEvent::PresenceCheckpointReceived {
                                                    summary,
                                                    last_event_seq,
                                                },
                                            );
                                        }
                                        Some("voice_diagnostic") => {
                                            let kind = json["kind"]
                                                .as_str()
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let detail =
                                                json["detail"].as_str().unwrap_or("").to_string();
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(&kind, &detail);
                                                }
                                            }
                                            bus_inbound
                                                .send(AppEvent::VoiceDiagnostic { kind, detail });
                                        }
                                        Some("user_audio") => {
                                            // Browser sends base64-encoded PCM16 audio for server-side transcription.
                                            if let Some(ref transcriber) = transcriber_inbound {
                                                if let Some(data_b64) = json["data"].as_str() {
                                                    use base64::Engine;
                                                    if let Ok(pcm_bytes) =
                                                        base64::engine::general_purpose::STANDARD
                                                            .decode(data_b64)
                                                    {
                                                        audio_buf.extend_from_slice(&pcm_bytes);
                                                        // Drain at ~3s of audio (16kHz * 2 bytes/sample * 1 channel * 3s = 96000)
                                                        let threshold =
                                                            (audio_sample_rate as usize) * 2 * 3;
                                                        if audio_buf.len() >= threshold {
                                                            // Skip silent buffers — compute RMS energy of PCM16 samples.
                                                            // Whisper hallucinates on silence (outputs "you", ".", etc).
                                                            let rms = {
                                                                let samples = audio_buf
                                                                    .chunks_exact(2)
                                                                    .map(|c| {
                                                                        i16::from_le_bytes([
                                                                            c[0], c[1],
                                                                        ])
                                                                            as f64
                                                                    });
                                                                let sum_sq: f64 =
                                                                    samples.map(|s| s * s).sum();
                                                                let n = audio_buf.len() / 2;
                                                                if n > 0 {
                                                                    (sum_sq / n as f64).sqrt()
                                                                } else {
                                                                    0.0
                                                                }
                                                            };
                                                            if rms < 1000.0 {
                                                                // Below speech threshold — skip transcription.
                                                                // Whisper hallucinates aggressively on low-energy
                                                                // audio ("Thank you", "Bye bye", etc).
                                                                audio_buf.clear();
                                                                continue;
                                                            }
                                                            let wav =
                                                                crate::transcription::encode_wav(
                                                                    &audio_buf,
                                                                    audio_sample_rate,
                                                                    1,
                                                                );
                                                            audio_buf.clear();
                                                            audio_seq += 1;
                                                            let seq = audio_seq;
                                                            let t = transcriber.clone();
                                                            let bus_tx = bus_inbound.clone();
                                                            let session_log_tx =
                                                                session_log_inbound.clone();
                                                            tokio::spawn(async move {
                                                                match t.transcribe(&wav).await {
                                                                    Ok(segment) => {
                                                                        let text = segment
                                                                            .text
                                                                            .trim()
                                                                            .to_string();
                                                                        if !text.is_empty() {
                                                                            if let Some(ref sl) =
                                                                                session_log_tx
                                                                            {
                                                                                if let Ok(mut l) =
                                                                                    sl.lock()
                                                                                {
                                                                                    l.user_transcript(&text, seq);
                                                                                }
                                                                            }
                                                                            bus_tx.send(AppEvent::UserTranscript { text, seq });
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        eprintln!("transcription failed: {}", e);
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("video_frame") => {
                                            // Browser sends a video frame for HQ archival in the frame registry.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("cam0")
                                                .to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    // Register in frame registry
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: true,
                                                            live_resolution: Some(
                                                                "768x768".to_string(),
                                                            ),
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) =
                                                            reg.register(meta, &jpeg_bytes)
                                                        {
                                                            eprintln!(
                                                                "frame registry write failed: {}",
                                                                e
                                                            );
                                                        }
                                                    }
                                                    // Feed into recording pipeline (auto-starts on first frame)
                                                    if let Some(ref rec_reg) =
                                                        recording_registry_inbound
                                                    {
                                                        let mut rreg = rec_reg.write().await;
                                                        if rreg.is_enabled() {
                                                            if !rreg.is_recording(&stream)
                                                                && crate::recording::is_ffmpeg_available() {
                                                                    if let Err(e) = rreg.start_stream(&stream).await {
                                                                        eprintln!("camera recording start failed: {}", e);
                                                                    } else {
                                                                        bus_inbound.send(AppEvent::RecordingStarted {
                                                                            stream_name: stream.clone(),
                                                                        });
                                                                    }
                                                                }
                                                            let _ = rreg
                                                                .feed_frame(&stream, &jpeg_bytes)
                                                                .await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("annotation_attach") => {
                                            // User clicked "Attach" on an annotation/frame: register
                                            // the JPEG in the frame registry but DO NOT inject into
                                            // the agent context. The browser tracks this frame ID as
                                            // a pending attachment and submits it with the next task.
                                            //
                                            // Works regardless of presence/agent state — attachments
                                            // are independent of any running task.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("annotation")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    let mut saved_path = String::new();
                                                    let mut registered = false;
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: if note.is_empty() {
                                                                None
                                                            } else {
                                                                Some(note.clone())
                                                            },
                                                        };
                                                        let mut reg = registry.write().await;
                                                        match reg.register(meta, &jpeg_bytes) {
                                                            Ok(path) => {
                                                                saved_path = path.display().to_string();
                                                                registered = true;
                                                            }
                                                            Err(e) => eprintln!("annotation_attach frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    let _ = direct_tx_inbound.send(
                                                        serde_json::json!({
                                                            "t": "annotation_attached",
                                                            "frame_id": frame_id,
                                                            "stream": stream,
                                                            "path": saved_path,
                                                            "note": note,
                                                            "ok": registered,
                                                        })
                                                        .to_string(),
                                                    );
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} attached (pending)",
                                                            frame_id
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                        Some("annotation_submit") => {
                                            // User drew annotations on a frame and submitted it with a note.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("annotation")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    // Register in frame registry
                                                    let mut saved_path = String::new();
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: if note.is_empty() {
                                                                None
                                                            } else {
                                                                Some(note.clone())
                                                            },
                                                        };
                                                        let mut reg = registry.write().await;
                                                        match reg.register(meta, &jpeg_bytes) {
                                                            Ok(path) => saved_path = path.display().to_string(),
                                                            Err(e) => eprintln!("annotation frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    // Optionally inject into agent conversation
                                                    let mut injected_to_queue = false;
                                                    if inject {
                                                        if let Some(ref ctx) = query_ctx_inbound {
                                                            if let Some(ref ciq) =
                                                                ctx.context_injection
                                                            {
                                                                if let Ok(mut q) = ciq.lock() {
                                                                    let label = if note.is_empty() {
                                                                        "[User Annotation] User highlighted something on the screen.".to_string()
                                                                    } else {
                                                                        format!(
                                                                            "[User Annotation] {}",
                                                                            note
                                                                        )
                                                                    };
                                                                    q.push(crate::event::ContextInjection {
                                                                        text: label,
                                                                        images: vec![crate::conversation::ImageData {
                                                                            media_type: "image/jpeg".to_string(),
                                                                            data: data_b64.to_string(),
                                                                        }],
                                                                        source: crate::event::InjectionSource::User,
                                                                        target_session_id: None,
                                                                        steer_id: None,
                                                                    });
                                                                    injected_to_queue = true;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    // Send path back to browser. Report whether the injection
                                                    // actually landed in the queue (not just whether the user
                                                    // pressed Send), so the UI doesn't lie when no presence is
                                                    // running.
                                                    let _ = direct_tx_inbound.send(
                                                        serde_json::json!({
                                                            "t": "annotation_saved",
                                                            "frame_id": frame_id,
                                                            "path": saved_path,
                                                            "injected": injected_to_queue,
                                                        })
                                                        .to_string(),
                                                    );
                                                    let status_label = if inject {
                                                        if injected_to_queue {
                                                            " (sent to agent)"
                                                        } else {
                                                            " (saved — no agent connected)"
                                                        }
                                                    } else {
                                                        ""
                                                    };
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} on {}{}",
                                                            frame_id, stream, status_label
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                        Some("clip_start") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("recording")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            let in_secs = json["in_secs"].as_f64().unwrap_or(0.0);
                                            let out_secs = json["out_secs"].as_f64().unwrap_or(0.0);
                                            let fps = json["fps"].as_u64().unwrap_or(2) as u32;
                                            let total =
                                                json["total_frames"].as_u64().unwrap_or(0) as usize;
                                            clip_accumulators.insert(
                                                clip_id.clone(),
                                                ClipAccumulator {
                                                    stream,
                                                    note,
                                                    inject,
                                                    in_secs,
                                                    out_secs,
                                                    fps,
                                                    expected: total,
                                                    frames: Vec::with_capacity(total),
                                                },
                                            );
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[clip] started {} ({} frames, {}fps)",
                                                    clip_id, total, fps
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });
                                        }
                                        Some("clip_frame") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                // Register frame in frame registry
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: format!("clip:{}", clip_id),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) =
                                                            reg.register(meta, &jpeg_bytes)
                                                        {
                                                            eprintln!("clip frame registry write failed: {}", e);
                                                        }
                                                    }
                                                }
                                                // Accumulate for context injection
                                                if let Some(acc) =
                                                    clip_accumulators.get_mut(&clip_id)
                                                {
                                                    acc.frames
                                                        .push((frame_id, data_b64.to_string()));
                                                }
                                            }
                                        }
                                        Some("clip_end") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let mut injected = false;

                                            if let Some(acc) = clip_accumulators.remove(&clip_id) {
                                                let frames_registered = acc.frames.len();
                                                if acc.inject {
                                                    if let Some(ref ctx) = query_ctx_inbound {
                                                        if let Some(ref ciq) = ctx.context_injection
                                                        {
                                                            if let Ok(mut q) = ciq.lock() {
                                                                let label = if acc.note.is_empty() {
                                                                    format!(
                                                                        "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps)",
                                                                        acc.stream,
                                                                        acc.in_secs,
                                                                        acc.out_secs,
                                                                        frames_registered, acc.fps,
                                                                    )
                                                                } else {
                                                                    format!(
                                                                        "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps). {}",
                                                                        acc.stream,
                                                                        acc.in_secs,
                                                                        acc.out_secs,
                                                                        frames_registered, acc.fps, acc.note,
                                                                    )
                                                                };
                                                                let images: Vec<crate::conversation::ImageData> = acc.frames.iter().map(|(_, b64)| {
                                                                    crate::conversation::ImageData {
                                                                        media_type: "image/jpeg".to_string(),
                                                                        data: b64.clone(),
                                                                    }
                                                                }).collect();
                                                                q.push(crate::event::ContextInjection {
                                                                    text: label,
                                                                    images,
                                                                    source: crate::event::InjectionSource::User,
                                                                    target_session_id: None,
                                                                    steer_id: None,
                                                                });
                                                                injected = true;
                                                            }
                                                        }
                                                    }
                                                }

                                                let _ = direct_tx_inbound.send(
                                                    serde_json::json!({
                                                        "t": "clip_saved",
                                                        "clip_id": clip_id,
                                                        "frames_registered": frames_registered,
                                                        "injected": injected,
                                                    })
                                                    .to_string(),
                                                );

                                                bus_inbound.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "[clip] {} — {} frames{}",
                                                        clip_id,
                                                        frames_registered,
                                                        if injected {
                                                            " (sent to agent)"
                                                        } else {
                                                            " (saved)"
                                                        }
                                                    ),
                                                    level: Some(LogLevel::Info),
                                                    turn: None,
                                                });
                                            }
                                        }
                                        Some("tool_request") => {
                                            let req_id =
                                                json["id"].as_str().unwrap_or("").to_string();
                                            let tool =
                                                json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned().unwrap_or(
                                                serde_json::Value::Object(Default::default()),
                                            );

                                            // Log the incoming tool request at Debug level
                                            let args_preview = {
                                                let s = serde_json::to_string(&args)
                                                    .unwrap_or_default();
                                                preview_text(&s, 200)
                                            };
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[tool_request] {}({})",
                                                    tool, args_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            // Dispatch through presence-core (single canonical layer)
                                            let state = query_ctx_inbound
                                                .as_ref()
                                                .map(|ctx| {
                                                    ctx.agent_state
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .clone()
                                                })
                                                .unwrap_or_default();
                                            let action =
                                                presence::dispatch_tool_call(&tool, &args, &state);

                                            // SubmitTask: send directly to task_tx (bypasses TUI)
                                            let query_result =
                                                if let presence::PresenceAction::SubmitTask(
                                                    envelope,
                                                ) = action
                                                {
                                                    let msg = format!(
                                                        "Task submitted: {}",
                                                        envelope.task
                                                    );
                                                    if let Some(ref tx) = task_tx_inbound {
                                                        let _ = tx.send(envelope).await;
                                                    } else {
                                                        // Fallback: dispatch via EventBus if no task_tx
                                                        let ctrl_action =
                                                            presence::PresenceAction::SubmitTask(
                                                                envelope,
                                                            );
                                                        if let Some((ctrl, _)) =
                                                            presence::action_to_control_msg(
                                                                &ctrl_action,
                                                            )
                                                        {
                                                            bus_inbound.send(
                                                                AppEvent::ControlCommand(ctrl),
                                                            );
                                                        }
                                                    }
                                                    presence::ToolQueryResult::text(msg)
                                                } else if let Some((ctrl, msg)) =
                                                    presence::action_to_control_msg(&action)
                                                {
                                                    // Other action tools: dispatch via EventBus
                                                    bus_inbound
                                                        .send(AppEvent::ControlCommand(ctrl));
                                                    presence::ToolQueryResult::text(msg)
                                                } else {
                                                    match action {
                                                        presence::PresenceAction::TextResult(
                                                            text,
                                                        ) => presence::ToolQueryResult::text(text),
                                                        presence::PresenceAction::NeedsIO {
                                                            tool_name,
                                                            args: io_args,
                                                        } => {
                                                            if let Some(ref ctx) = query_ctx_inbound
                                                            {
                                                                if let Some(result) =
                                                                    presence::handle_tool_query(
                                                                        &ctx.agent_state,
                                                                        &ctx.project_root,
                                                                        &ctx.log_dir,
                                                                        &ctx.knowledge_path,
                                                                        &tool_name,
                                                                        &io_args,
                                                                        frame_registry_inbound
                                                                            .as_ref(),
                                                                        ctx.context_injection
                                                                            .as_ref(),
                                                                    )
                                                                    .await
                                                                {
                                                                    result
                                                                } else {
                                                                    presence::ToolQueryResult::text(
                                                                        format!(
                                                                            "Unknown tool: {}",
                                                                            tool
                                                                        ),
                                                                    )
                                                                }
                                                            } else {
                                                                presence::ToolQueryResult::text("Presence query context not available".to_string())
                                                            }
                                                        }
                                                        _ => unreachable!(),
                                                    }
                                                };

                                            // Log the tool response at Debug level
                                            let result_preview =
                                                preview_text(&query_result.text, 200);
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[tool_response] {} → {}",
                                                    tool, result_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "tool_response",
                                                "id": req_id,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> =
                                                    query_result
                                                        .images
                                                        .iter()
                                                        .map(|img| {
                                                            serde_json::json!({
                                                                "mime_type": img.media_type,
                                                                "data": img.data,
                                                            })
                                                        })
                                                        .collect();
                                                response["images"] =
                                                    serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("async_query") => {
                                            // Async query from browser — same dispatch as tool_request
                                            // but result goes back as async_query_result (injected into
                                            // voice session as text, not as a tool response).
                                            let req_id =
                                                json["id"].as_str().unwrap_or("").to_string();
                                            let tool =
                                                json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned().unwrap_or(
                                                serde_json::Value::Object(Default::default()),
                                            );

                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[async_query] {}", tool),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let query_result = if let Some(ref ctx) =
                                                query_ctx_inbound
                                            {
                                                if let Some(result) = presence::handle_tool_query(
                                                    &ctx.agent_state,
                                                    &ctx.project_root,
                                                    &ctx.log_dir,
                                                    &ctx.knowledge_path,
                                                    &tool,
                                                    &args,
                                                    frame_registry_inbound.as_ref(),
                                                    ctx.context_injection.as_ref(),
                                                )
                                                .await
                                                {
                                                    result
                                                } else {
                                                    presence::ToolQueryResult::text(format!(
                                                        "Unknown query tool: {}",
                                                        tool
                                                    ))
                                                }
                                            } else {
                                                presence::ToolQueryResult::text(
                                                    "Presence query context not available"
                                                        .to_string(),
                                                )
                                            };

                                            let result_preview =
                                                preview_text(&query_result.text, 200);
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[async_query_result] {} → {}",
                                                    tool, result_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "async_query_result",
                                                "id": req_id,
                                                "tool": tool,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> =
                                                    query_result
                                                        .images
                                                        .iter()
                                                        .map(|img| {
                                                            serde_json::json!({
                                                                "mime_type": img.media_type,
                                                                "data": img.data,
                                                            })
                                                        })
                                                        .collect();
                                                response["images"] =
                                                    serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("display_offer") => {
                                            // WebRTC SDP offer from browser for a display session
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let sdp =
                                                json["sdp"].as_str().unwrap_or("").to_string();

                                            // Clone the Arc<DisplaySession> out of the read
                                            // lock before calling handle_offer. Holding the
                                            // guard across the await chokes any writer
                                            // (notably deactivate_user_display's
                                            // registry.write()) for as long as this block
                                            // runs. The Arc keeps the session alive
                                            // independently of the lock.
                                            let session: Option<
                                                Arc<crate::display::DisplaySession>,
                                            > = match session_registry_inbound.as_ref() {
                                                Some(sr) => sr.read().await.get(display_id),
                                                None => None,
                                            };
                                            if let Some(session) = session {
                                                let (ice_tx, mut ice_rx) = mpsc::channel::<(
                                                    crate::display::PeerId,
                                                    String,
                                                )>(
                                                    64
                                                );
                                                // Combine the Host-header IP with the
                                                // port we want to advertise (HTTP port
                                                // for Phase 3 multiplex, or standalone
                                                // Phase 2 port) to form the single TCP
                                                // candidate the peer will emit. None
                                                // if either piece is missing (typically
                                                // because the browser connected via
                                                // hostname).
                                                let tcp_advertised_addr: Option<
                                                    std::net::SocketAddr,
                                                > = match (browser_host_ip, tcp_advertised_port) {
                                                    (Some(ip), Some(port)) => {
                                                        Some(std::net::SocketAddr::new(ip, port))
                                                    }
                                                    _ => None,
                                                };
                                                // Phase 5a.1 input authority gate.  The closure
                                                // returns true when this connection is the
                                                // authority holder OR when the display has no
                                                // holder (unclaimed = pre-phase-5 default).
                                                // `display/mod.rs` only sees this boolean; it
                                                // never learns about DisplayInputHolder, the
                                                // map, or connection IDs.  See
                                                // [`build_local_ws_input_authorizer`] for the
                                                // closure semantics + tests.
                                                let input_authorized =
                                                    build_local_ws_input_authorizer(
                                                        display_id,
                                                        connection_id_inbound.clone(),
                                                        Arc::clone(
                                                            &display_input_authority_inbound,
                                                        ),
                                                    );
                                                // F-1.3b2 transport plumbing: local DisplaySlot's
                                                // browser doesn't create the
                                                // `display_input_authority` data channel
                                                // (5a/5c uses the WS path), so the handler is
                                                // never invoked here. The no-op keeps the
                                                // transport-layer signature uniform across
                                                // both peer kinds; the real federated handler
                                                // is wired by the federated path's caller in
                                                // a later slice.
                                                let authority_handler =
                                                    crate::display::webrtc::noop_authority_handler(
                                                    );
                                                match session
                                                    .handle_offer(
                                                        peer_id,
                                                        &sdp,
                                                        &ice_config,
                                                        Some(Arc::clone(&tcp_peer_registry)),
                                                        tcp_advertised_addr,
                                                        ice_tx,
                                                        input_authorized,
                                                        authority_handler,
                                                    )
                                                    .await
                                                {
                                                    Ok(answer_sdp) => {
                                                        peer_display_ids.push(display_id);
                                                        let answer = serde_json::json!({
                                                            "t": "display_answer",
                                                            "display_id": display_id,
                                                            "sdp": answer_sdp,
                                                        });
                                                        let _ = direct_tx_inbound
                                                            .send(answer.to_string());

                                                        // Forward server ICE candidates to browser
                                                        let ice_direct_tx =
                                                            direct_tx_inbound.clone();
                                                        tokio::spawn(async move {
                                                            while let Some((_pid, candidate_json)) =
                                                                ice_rx.recv().await
                                                            {
                                                                let msg = serde_json::json!({
                                                                    "t": "display_ice",
                                                                    "display_id": display_id,
                                                                    "candidate": serde_json::from_str::<serde_json::Value>(&candidate_json).unwrap_or_default(),
                                                                });
                                                                if ice_direct_tx
                                                                    .send(msg.to_string())
                                                                    .is_err()
                                                                {
                                                                    break;
                                                                }
                                                            }
                                                        });
                                                    }
                                                    Err(e) => {
                                                        eprintln!("[web_gateway] WebRTC offer failed for display {}: {}", display_id, e);
                                                    }
                                                }
                                            }
                                        }
                                        Some("display_ice") => {
                                            // Trickle ICE candidate from browser. Spawn the
                                            // handling off the ws reader loop because
                                            // `add_ice_candidate` resolves mDNS hostnames
                                            // (browsers obfuscate host candidates as
                                            // `<uuid>.local`). On hosts without an mDNS
                                            // responder — every headless VM without Avahi,
                                            // which is the common deployment — each lookup
                                            // blocks on the system resolver's full timeout
                                            // (5-20s). With multiple candidates and ICE
                                            // retries, that piles 20-30s of blocking inside
                                            // this reader, stalling every other ws frame
                                            // behind it including grant/revoke — the root
                                            // cause of the "second ON takes 20+s" bug.
                                            //
                                            // Spawning decouples candidate processing from
                                            // frame intake. Failed lookups still log the
                                            // same "mdns resolve failed" diagnostic; losing
                                            // a candidate is survivable (ICE has others),
                                            // whereas blocking the reader is not.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let candidate = json["candidate"].to_string();
                                            let sr_clone = session_registry_inbound.clone();
                                            let pid = peer_id;
                                            tokio::spawn(async move {
                                                // Clone the session Arc out of the read
                                                // lock first. The previous spread-across-
                                                // `if let` form held the guard across
                                                // add_ice_candidate's mDNS resolution,
                                                // which on hosts without Avahi blocks for
                                                // 5-20s per candidate — starving any
                                                // concurrent writer (notably
                                                // deactivate_user_display's
                                                // registry.write()). Dropping the guard
                                                // first lets deactivate proceed
                                                // immediately; the session Arc keeps the
                                                // target alive while mDNS resolves.
                                                let session: Option<
                                                    Arc<crate::display::DisplaySession>,
                                                > = match sr_clone.as_ref() {
                                                    Some(sr) => sr.read().await.get(display_id),
                                                    None => None,
                                                };
                                                if let Some(session) = session {
                                                    if let Err(e) = session
                                                        .add_ice_candidate(pid, &candidate)
                                                        .await
                                                    {
                                                        eprintln!("[web_gateway] ICE candidate failed for display {}: {}", display_id, e);
                                                    }
                                                }
                                            });
                                        }
                                        Some("dashboard_control_offer") => {
                                            let sdp =
                                                json["sdp"].as_str().unwrap_or("").to_string();
                                            let client_nonce = json["client_nonce"]
                                                .as_str()
                                                .map(str::trim)
                                                .filter(|nonce| !nonce.is_empty())
                                                .map(str::to_string);
                                            if sdp.is_empty() {
                                                let msg = serde_json::json!({
                                                    "t": "dashboard_control_error",
                                                    "error": "missing sdp",
                                                });
                                                let _ = direct_tx_inbound.send(msg.to_string());
                                                continue;
                                            }
                                            match dashboard_control_inbound
                                                .answer_offer_with_grant(
                                                    sdp,
                                                    None,
                                                    client_nonce,
                                                    dashboard_control_grant_inbound.clone(),
                                                )
                                                .await
                                            {
                                                Ok(answer) => {
                                                    dashboard_control_session_ids
                                                        .push(answer.session_id.clone());
                                                    let msg = serde_json::json!({
                                                        "t": "dashboard_control_answer",
                                                        "session_id": answer.session_id,
                                                        "sdp": answer.sdp,
                                                        "binding": answer.binding,
                                                    });
                                                    let _ = direct_tx_inbound.send(msg.to_string());
                                                }
                                                Err(e) => {
                                                    let msg = serde_json::json!({
                                                        "t": "dashboard_control_error",
                                                        "error": e,
                                                    });
                                                    let _ = direct_tx_inbound.send(msg.to_string());
                                                }
                                            }
                                        }
                                        Some("dashboard_control_ice") => {
                                            let session_id = json["session_id"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string();
                                            let candidate = json
                                                .get("candidate")
                                                .cloned()
                                                .unwrap_or_else(|| serde_json::json!({}));
                                            if session_id.is_empty() {
                                                continue;
                                            }
                                            let registry = Arc::clone(&dashboard_control_inbound);
                                            tokio::spawn(async move {
                                                if let Err(e) = registry
                                                    .add_ice_candidate(&session_id, &candidate)
                                                    .await
                                                {
                                                    eprintln!(
                                                        "[dashboard/control] add ICE failed: {e}"
                                                    );
                                                }
                                            });
                                        }
                                        Some("dashboard_control_close") => {
                                            let session_id = json["session_id"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string();
                                            if !session_id.is_empty() {
                                                dashboard_control_inbound.close(&session_id).await;
                                                dashboard_control_session_ids
                                                    .retain(|s| s != &session_id);
                                            }
                                        }
                                        Some("terminal_open") => {
                                            // {"t":"terminal_open","host_id":"local","terminal_id":"shell-0","cols":80,"rows":24}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey {
                                                host_id: host_id.clone(),
                                                terminal_id: terminal_id.clone(),
                                            };

                                            // Attach needs only the terminal.view
                                            // floor already enforced; creating a
                                            // shell needs shell.spawn, decided at
                                            // frame time so expiry mid-connection
                                            // is honored. A grant-level fs scope
                                            // makes the new shell a sandboxed one.
                                            let spawn_policy = crate::terminal::ShellSpawnPolicy {
                                                may_spawn: dashboard_control_grant_inbound
                                                    .access_decision(
                                                        crate::peer::access_policy::PeerOperation::ShellSpawn,
                                                    )
                                                    .allowed,
                                                shared: json["shared"]
                                                    .as_bool()
                                                    .unwrap_or(false),
                                                scope: dashboard_control_grant_inbound
                                                    .filesystem()
                                                    .cloned(),
                                            };
                                            match terminal_registry_inbound
                                                .open_or_attach(
                                                    key.clone(),
                                                    cols,
                                                    rows,
                                                    &ws_terminal_actor,
                                                    spawn_policy,
                                                )
                                                .await
                                            {
                                                Ok((session, _created)) => {
                                                    // Spawn a forwarder task that drains the session's
                                                    // per-listener channel and sends base64-encoded
                                                    // output to this WS connection.
                                                    let (tx, mut rx) =
                                                        tokio::sync::mpsc::unbounded_channel();
                                                    session.attach(tx);

                                                    let forwarder_tx = direct_tx_inbound.clone();
                                                    let fwd_host = host_id.clone();
                                                    let fwd_term = terminal_id.clone();
                                                    tokio::spawn(async move {
                                                        use base64::Engine as _;
                                                        while let Some(event) = rx.recv().await {
                                                            let msg = match event {
                                                                crate::terminal::TerminalEvent::Output(bytes) => {
                                                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                                                    serde_json::json!({
                                                                        "t": "terminal_output",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "data": b64,
                                                                    })
                                                                }
                                                                crate::terminal::TerminalEvent::Exited { status } => {
                                                                    serde_json::json!({
                                                                        "t": "terminal_exited",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "status": status,
                                                                    })
                                                                }
                                                            };
                                                            if forwarder_tx
                                                                .send(msg.to_string())
                                                                .is_err()
                                                            {
                                                                break;
                                                            }
                                                        }
                                                    });

                                                    let ack = serde_json::json!({
                                                        "t": "terminal_opened",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                        "shared": session.shared(),
                                                        "can_share": session
                                                            .managed_by(&ws_terminal_actor),
                                                    });
                                                    let _ = direct_tx_inbound.send(ack.to_string());
                                                }
                                                Err(e) => {
                                                    let err = serde_json::json!({
                                                        "t": "terminal_error",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                        "error": e.to_string(),
                                                    });
                                                    let _ = direct_tx_inbound.send(err.to_string());
                                                }
                                            }
                                        }
                                        Some("terminal_input") => {
                                            // {"t":"terminal_input","host_id":"local","terminal_id":"shell-0","data":"<base64>"}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let data_b64 = json["data"].as_str().unwrap_or("");
                                            use base64::Engine as _;
                                            if let Ok(data) =
                                                base64::engine::general_purpose::STANDARD
                                                    .decode(data_b64)
                                            {
                                                let key = crate::terminal::TerminalKey {
                                                    host_id,
                                                    terminal_id,
                                                };
                                                if let Some(session) = terminal_registry_inbound
                                                    .get_visible(&key, &ws_terminal_actor)
                                                    .await
                                                {
                                                    session.write_input(&data);
                                                }
                                            }
                                        }
                                        Some("terminal_resize") => {
                                            // {"t":"terminal_resize","host_id":"local","terminal_id":"shell-0","cols":N,"rows":N}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey {
                                                host_id,
                                                terminal_id,
                                            };
                                            if let Some(session) = terminal_registry_inbound
                                                .get_visible(&key, &ws_terminal_actor)
                                                .await
                                            {
                                                session.resize(cols, rows);
                                            }
                                        }
                                        Some("terminal_close") => {
                                            // {"t":"terminal_close","host_id":"local","terminal_id":"shell-0"}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let key = crate::terminal::TerminalKey {
                                                host_id,
                                                terminal_id,
                                            };
                                            terminal_registry_inbound
                                                .close_visible(&key, &ws_terminal_actor)
                                                .await;
                                        }
                                        Some("terminal_share") => {
                                            // {"t":"terminal_share","host_id":"local","terminal_id":"shell-0","shared":true}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let shared = json["shared"].as_bool().unwrap_or(true);
                                            let key = crate::terminal::TerminalKey {
                                                host_id: host_id.clone(),
                                                terminal_id: terminal_id.clone(),
                                            };
                                            let msg = match terminal_registry_inbound
                                                .set_shared(&key, &ws_terminal_actor, shared)
                                                .await
                                            {
                                                Some(state) => serde_json::json!({
                                                    "t": "terminal_shared",
                                                    "host_id": host_id,
                                                    "terminal_id": terminal_id,
                                                    "shared": state,
                                                }),
                                                None => serde_json::json!({
                                                    "t": "terminal_error",
                                                    "host_id": host_id,
                                                    "terminal_id": terminal_id,
                                                    "error": "not allowed: only the session owner or root can change sharing",
                                                }),
                                            };
                                            let _ = direct_tx_inbound.send(msg.to_string());
                                        }
                                        Some("display_input") => {
                                            // Input event (keyboard/mouse) for a display session.
                                            // Drop the registry read lock before the inject
                                            // (which runs xdotool/cliclick subprocesses) so a
                                            // concurrent deactivate can take the write lock
                                            // without waiting on subprocess exits.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;

                                            // Phase 5 authority gate: if someone has claimed
                                            // input authority for this display, only that
                                            // connection's input flows through. Unclaimed
                                            // (no entry in the map) = pre-phase-5 default,
                                            // every connection can input. See the
                                            // `DisplayInputHolder` doc for the full
                                            // contract.
                                            let allowed = {
                                                let authority = display_input_authority_inbound
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                match authority.get(&display_id) {
                                                    Some(entry) => entry
                                                        .matches_local_ws(&connection_id_inbound),
                                                    None => true,
                                                }
                                            };
                                            if !allowed {
                                                // Silent drop — matches the "force_disconnect_voice"
                                                // convention where demoted connections don't get
                                                // per-message denial feedback; the browser already
                                                // knows it's passive from the authority_revoked
                                                // notification it received when it was demoted.
                                                continue;
                                            }

                                            if let Some(evt) = json.get("event") {
                                                if let Ok(input_event) = serde_json::from_value::<
                                                    crate::display::InputEvent,
                                                >(
                                                    evt.clone()
                                                ) {
                                                    let session: Option<
                                                        Arc<crate::display::DisplaySession>,
                                                    > = match session_registry_inbound.as_ref() {
                                                        Some(sr) => sr.read().await.get(display_id),
                                                        None => None,
                                                    };
                                                    if let Some(session) = session {
                                                        if let Err(e) =
                                                            session.inject_input(input_event).await
                                                        {
                                                            eprintln!("[web_gateway] display input injection failed: {}", e);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("set_diagnostics_visual_marker") => {
                                            // **Phase 0 visual-freshness diagnostic toggle**
                                            // (task #83). Inline rather than going through
                                            // the ControlMsg dispatch path because the
                                            // effect is a single atomic store on the
                                            // matching DisplaySession — no shared autonomy
                                            // state, no event-bus side effects, no listener
                                            // chain to wait on. Symmetric with the
                                            // `display_input` arm above for the same reason
                                            // (direct session access, no bus round-trip).
                                            //
                                            // No authority gate: diagnostics is operator-
                                            // initiated and the marker affects every viewer
                                            // of this display when on (it's stamped pre-
                                            // encoder, lands in every encoded layer). An
                                            // operator running a smoke run sets it, all
                                            // viewers see the marker until they unset it.
                                            // No covert-stamp scenario worth gating against.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let enabled =
                                                json["enabled"].as_bool().unwrap_or(false);
                                            match session_registry_inbound.as_ref() {
                                                Some(sr) => {
                                                    let applied = sr
                                                        .write()
                                                        .await
                                                        .set_diagnostics_visual_marker(
                                                            display_id, enabled,
                                                        );
                                                    eprintln!(
                                                        "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                                        display_id,
                                                        enabled,
                                                        if applied { "" } else { " (pending)" },
                                                    );
                                                }
                                                None => {
                                                    eprintln!(
                                                        "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                                        display_id, enabled,
                                                    );
                                                }
                                            }
                                        }
                                        _ => {
                                            // Fall through to ControlMsg parsing.
                                            // WebRtcSignal needs special handling because
                                            // it requires session_registry / direct_tx
                                            // access for the response leg; everything else
                                            // gets re-broadcast as AppEvent::ControlCommand
                                            // for the agent loop / TUI / MCP consumers.
                                            match serde_json::from_value::<ControlMsg>(json) {
                                                Ok(ctrl)
                                                    if !peer_identity_allows_ws_control(
                                                        peer_identity_inbound.as_ref(),
                                                        &ctrl,
                                                        &bus_inbound,
                                                    ) => {}
                                                Ok(ctrl)
                                                    if !ws_grant_allows_control(
                                                        &dashboard_control_grant_inbound,
                                                        peer_identity_inbound.as_ref(),
                                                        &ctrl,
                                                        &bus_inbound,
                                                    ) => {}
                                                Ok(ControlMsg::WebRtcSignal {
                                                    display_id,
                                                    session_id,
                                                    signal,
                                                }) => {
                                                    let federated_display_input_allowed =
                                                                peer_identity_allows_operation(
                                                                    peer_identity_inbound.as_ref(),
                                                                    crate::peer::access_policy::PeerOperation::DisplayInput,
                                                                    "peer-webrtc-display",
                                                                );
                                                    handle_federated_webrtc_signal(
                                                                display_id,
                                                                session_id,
                                                                signal,
                                                        session_registry_inbound.as_ref(),
                                                        &ice_config,
                                                        Arc::clone(&tcp_peer_registry),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                        // F-1.3b3 federated authority context.
                                                        // `connection_id_inbound` is this WS's
                                                        // id, which doubles as the federation
                                                        // transport's `federation_connection_id`
                                                        // when this connection is acting as a
                                                        // federation transport.
                                                        connection_id_inbound.clone(),
                                                        Arc::clone(&display_input_authority_inbound),
                                                        authority_change_tx_inbound.clone(),
                                                        Arc::clone(&federated_authority_subscribers_inbound),
                                                        federated_display_input_allowed,
                                                    )
                                                    .await;
                                                }
                                                Ok(ControlMsg::PeerFileTransferSignal {
                                                    session_id,
                                                    signal,
                                                }) => {
                                                    handle_peer_file_transfer_signal(
                                                        session_id,
                                                        signal,
                                                        Arc::clone(
                                                            &peer_file_transfer_registry_inbound,
                                                        ),
                                                        peer_identity_inbound.clone(),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                    )
                                                    .await;
                                                }
                                                Ok(ControlMsg::PeerDashboardControlSignal {
                                                    session_id,
                                                    signal,
                                                }) => {
                                                    handle_peer_dashboard_control_signal(
                                                        session_id,
                                                        signal,
                                                        Arc::clone(&dashboard_control_inbound),
                                                        peer_identity_inbound.clone(),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                    )
                                                    .await;
                                                }
                                                Ok(ControlMsg::RequestDisplayInputAuthority {
                                                    display_id,
                                                }) => {
                                                    // Phase 5a.1: handler body lives in
                                                    // `apply_grant_input_authority` so the
                                                    // authority-change emission is unit-testable
                                                    // without standing up a WS lifecycle.  This
                                                    // arm keeps the bus log + the per-connection
                                                    // confirm message at the call site to avoid
                                                    // baking logging dependencies into the helper.
                                                    apply_grant_input_authority(
                                                        display_id,
                                                        connection_id_inbound.clone(),
                                                        direct_tx_inbound.clone(),
                                                        &display_input_authority_inbound,
                                                        &authority_change_tx_inbound,
                                                    );
                                                    // Confirm to the new holder (kept here so the
                                                    // helper has no dependency on the call site's
                                                    // direct_tx — and so the failure-to-send case
                                                    // doesn't bubble past the gate).
                                                    let granted = serde_json::json!({
                                                        "t": "display_input_authority_granted",
                                                        "display_id": display_id,
                                                    })
                                                    .to_string();
                                                    let _ = direct_tx_inbound.send(granted);
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] display_input_authority granted display={} holder={}",
                                                            display_id, connection_id_inbound,
                                                        ),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                }
                                                Ok(ControlMsg::ReleaseDisplayInputAuthority {
                                                    display_id,
                                                }) => {
                                                    let removed = apply_release_input_authority(
                                                        display_id,
                                                        connection_id_inbound.as_str(),
                                                        &display_input_authority_inbound,
                                                        &authority_change_tx_inbound,
                                                    );
                                                    if removed {
                                                        bus_inbound.send(AppEvent::PresenceLog {
                                                            message: format!(
                                                                "[ws] display_input_authority released display={} holder={}",
                                                                display_id, connection_id_inbound,
                                                            ),
                                                            level: Some(LogLevel::Debug),
                                                            turn: None,
                                                        });
                                                    }
                                                }
                                                Ok(ControlMsg::SetDiagnosticsVisualMarker {
                                                    display_id,
                                                    enabled,
                                                }) => {
                                                    // Accept the documented ControlMsg wire form
                                                    // (`{"action":"set_diagnostics_visual_marker", ...}`)
                                                    // in addition to the low-level `t` form
                                                    // handled above. The smoke script uses
                                                    // ControlMsg JSON so the toggle must be
                                                    // applied here instead of falling through to
                                                    // the generic bus path, where this variant is
                                                    // intentionally a no-op for TUI/MCP parity.
                                                    let display_id = display_id.unwrap_or(0);
                                                    match session_registry_inbound.as_ref() {
                                                        Some(sr) => {
                                                            let applied = sr
                                                                .write()
                                                                .await
                                                                .set_diagnostics_visual_marker(
                                                                    display_id, enabled,
                                                                );
                                                            eprintln!(
                                                                "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                                                display_id,
                                                                enabled,
                                                                if applied { "" } else { " (pending)" },
                                                            );
                                                        }
                                                        None => {
                                                            eprintln!(
                                                                "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                                                display_id, enabled,
                                                            );
                                                        }
                                                    }
                                                }
                                                Ok(ctrl @ ControlMsg::ResumeSession { .. }) => {
                                                    let ControlMsg::ResumeSession {
                                                        source,
                                                        session_id,
                                                        resume_id,
                                                        task,
                                                        ..
                                                    } = &ctrl
                                                    else {
                                                        unreachable!();
                                                    };
                                                    let source = source.clone();
                                                    let session_id = session_id.clone();
                                                    let resume_id = resume_id.clone();
                                                    let task = task.clone();
                                                    let direct_tx_resume =
                                                        direct_tx_inbound.clone();
                                                    let bus_resume = bus_inbound.clone();
                                                    tokio::spawn(async move {
                                                        let replay = tokio::task::spawn_blocking(
                                                            move || {
                                                                resume_session_activity_replay(
                                                                    &source,
                                                                    &session_id,
                                                                    resume_id.as_deref(),
                                                                    task.as_deref(),
                                                                    EXTERNAL_ACTIVITY_REPLAY_LIMIT,
                                                                )
                                                            },
                                                        )
                                                        .await
                                                        .ok()
                                                        .flatten();
                                                        if let Some(replay) = replay {
                                                            let _ = direct_tx_resume.send(replay);
                                                        }
                                                        bus_resume.send(AppEvent::PresenceLog {
                                                            message: format!(
                                                                "[ws] ControlMsg: {:?}",
                                                                ctrl
                                                            ),
                                                            level: Some(LogLevel::Debug),
                                                            turn: None,
                                                        });
                                                        bus_resume
                                                            .send(AppEvent::ControlCommand(ctrl));
                                                    });
                                                }
                                                Ok(ctrl) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] ControlMsg: {:?}",
                                                            match &ctrl {
                                                                ControlMsg::StartTask {
                                                                    task,
                                                                    ..
                                                                } => format!(
                                                                    "StartTask({})",
                                                                    preview_text(task, 60)
                                                                ),
                                                                other => format!("{:?}", other),
                                                            }
                                                        ),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                    bus_inbound
                                                        .send(AppEvent::ControlCommand(ctrl));
                                                }
                                                Err(e) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] ControlMsg parse failed: {}",
                                                            e
                                                        ),
                                                        level: Some(LogLevel::Warn),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // WebSocket closed — clean up active slot and auto-resume
                        // server presence if this was the active browser (covers tab
                        // close without beforeunload, network drops, etc.)
                        if is_active {
                            let mut slot = active_presence_inbound
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if slot
                                .as_ref()
                                .map(|a| a.connection_id == connection_id_inbound)
                                .unwrap_or(false)
                            {
                                *slot = None;
                            }
                        }
                        // Also release any display input authority this
                        // connection held (phase 5).  Without this, a
                        // dangling entry would block other browsers from
                        // claiming the display until someone explicitly
                        // sent RequestDisplayInputAuthority to force-take
                        // it — the `retain` below is the normal-drop
                        // cleanup that keeps the map consistent with
                        // live connections.
                        //
                        // Phase 5a.1: helper handles map mutation + per-
                        // display None-holder change emit so other
                        // browsers don't stay stuck on `other` after the
                        // holder's WS drops.  See
                        // `apply_ws_close_input_authority` for the
                        // semantics + tests.
                        apply_ws_close_input_authority(
                            connection_id_inbound.as_str(),
                            &display_input_authority_inbound,
                            &authority_change_tx_inbound,
                        );
                        // F-1.3b3: federation-transport WS-close
                        // cleanup. Two disjoint registry entries can
                        // belong to one connection_id — `LocalWs` from
                        // direct-browser use or `FederatedWebRtc` from
                        // federation-transport use — so both apply_*
                        // helpers fire from the same WS-close hook.
                        // The single WS in practice acts in only one
                        // role at a time, so the second helper is a
                        // no-op in the typical case; the cost of
                        // running both is the bookkeeping above.
                        //
                        // Order: unregister subscribers first (stops
                        // new fanout sends) → release authority (so
                        // observers see `unclaimed`) → close
                        // WebRtcPeers (so the data channels stop
                        // accepting incoming `display_input_authority_request`
                        // frames under the now-defunct federation
                        // identity). Without the peer-teardown step,
                        // the authority handler closure on each
                        // surviving peer would keep mutating the
                        // registry under an identity whose WS is
                        // gone — the structural bug F-1.3b3 fix #2
                        // closes.
                        let released_federated_subs =
                            unregister_all_federated_subscribers_for_connection(
                                connection_id_inbound.as_str(),
                                &federated_authority_subscribers_inbound,
                            );
                        apply_federated_ws_close_input_authority(
                            connection_id_inbound.as_str(),
                            &display_input_authority_inbound,
                            &authority_change_tx_inbound,
                        );
                        close_federated_peers_for_sessions(
                            &released_federated_subs,
                            session_registry_inbound.as_ref(),
                        )
                        .await;
                        if is_presence_connected && is_active {
                            bus_inbound.send(AppEvent::PresenceDisconnected);
                        }
                        // Remove this peer from display sessions it connected to
                        if !peer_display_ids.is_empty() {
                            if let Some(ref sr) = session_registry_inbound {
                                let reg = sr.read().await;
                                for did in &peer_display_ids {
                                    if let Some(session) = reg.get(*did) {
                                        session.remove_peer(peer_id).await;
                                    }
                                }
                            }
                        }
                        for session_id in dashboard_control_session_ids {
                            dashboard_control_inbound.close(&session_id).await;
                        }
                    });

                    // Phase 5a.1 outbound personalization plumbing.  The
                    // authority change channel carries the holder's
                    // server-internal connection_id; this connection's
                    // outbound task converts each incoming change into a
                    // personalized `display_input_authority_state` wire
                    // message.  Connection IDs never leave the daemon —
                    // only the resolved `you|other|unclaimed` state does.
                    let mut authority_change_rx = authority_change_tx.subscribe();
                    let connection_id_outbound = connection_id.clone();
                    let display_input_authority_outbound = display_input_authority.clone();
                    let session_registry_outbound = session_registry.clone();

                    // Outbound: broadcast + direct responses → WebSocket
                    let outbound = tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                msg = outbound_rx.recv() => {
                                    match msg {
                                        Ok(line) => {
                                            if ws_tx
                                                .send(Message::Text(line.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break,
                                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                    }
                                }
                                msg = direct_rx.recv() => {
                                    match msg {
                                        Some(line) => {
                                            if ws_tx
                                                .send(Message::Text(line.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        None => break,
                                    }
                                }
                                msg = authority_change_rx.recv() => {
                                    match msg {
                                        Ok(change) => {
                                            // Personalize: never ship the holder's identity.
                                            let state = match &change.holder {
                                                Some(h) if h.matches_local_ws(&connection_id_outbound) => "you",
                                                Some(_) => "other",
                                                None => "unclaimed",
                                            };
                                            let frame = serde_json::json!({
                                                "t": "display_input_authority_state",
                                                "display_id": change.display_id,
                                                "state": state,
                                            }).to_string();
                                            if ws_tx
                                                .send(Message::Text(frame.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break,
                                        Err(broadcast::error::RecvError::Lagged(_)) => {
                                            // Phase 5a.1: a lagged subscriber missed at least one
                                            // authority transition.  Send a fresh personalized
                                            // snapshot for every currently-active display so the
                                            // browser's chip cannot be left stuck on stale state.
                                            // Snapshot is computed under the std lock (held briefly,
                                            // released before any send) plus the session registry's
                                            // tokio lock for the active-display list — order
                                            // matters: take the std lock LAST and drop it before
                                            // awaiting the send to avoid awaiting under a sync guard.
                                                            let display_ids: Vec<u32> = match session_registry_outbound.as_ref() {
                                                Some(sr) => sr.read().await.display_ids(),
                                                None => Vec::new(),
                                            };
                                            let snapshots: Vec<(u32, &'static str)> = {
                                                let auth = display_input_authority_outbound
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                display_ids.into_iter().map(|did| {
                                                    let state = match auth.get(&did) {
                                                        Some(entry) if entry.matches_local_ws(&connection_id_outbound) => "you",
                                                        Some(_) => "other",
                                                        None => "unclaimed",
                                                    };
                                                    (did, state)
                                                }).collect()
                                            };
                                            let mut send_failed = false;
                                            for (did, state) in snapshots {
                                                let frame = serde_json::json!({
                                                    "t": "display_input_authority_state",
                                                    "display_id": did,
                                                    "state": state,
                                                }).to_string();
                                                if ws_tx
                                                    .send(Message::Text(frame.into()))
                                                    .await
                                                    .is_err()
                                                {
                                                    send_failed = true;
                                                    break;
                                                }
                                            }
                                            if send_failed { break; }
                                        }
                                    }
                                }
                            }
                        }
                    });

                    let _ = tokio::join!(inbound, outbound);
                } else {
                    // Plain HTTP: consume the peeked request bytes, then send response.
                    let mut discard = vec![0u8; n];
                    use tokio::io::AsyncReadExt;
                    let _ = stream.read_exact(&mut discard).await;

                    // Parse the request target once: the static-asset arms
                    // below match on the *exact* path (query stripped), so
                    // an API request that merely mentions an asset path in
                    // a query parameter can no longer be shadowed by them.
                    let (req_method, req_path, req_query) = parse_request_target(request_line);

                    // CORS preflight: respond to OPTIONS with permissive headers.
                    // Needed when the page is served from a custom scheme (intendant://)
                    // in the macOS app bundle — API fetches become cross-origin.
                    //
                    // Exception: the fleet Access APIs never get the wildcard.
                    // Their preflight only succeeds for allowlisted fleet
                    // origins (and the write-side gate below enforces the same
                    // list on the actual requests, so a non-preflighted
                    // cross-site POST is refused too).
                    if req_method == "OPTIONS" {
                        use tokio::io::AsyncWriteExt;
                        let opt_path = req_path;
                        // Table-declared paths take their posture and method
                        // union from the same route declarations that drive
                        // dispatch and IAM — a preflight can no longer be
                        // looser (or tighter) than its endpoint. Undeclared
                        // paths keep the legacy prefix rules: /api/* look-
                        // alikes stay own-origin-scoped, everything else gets
                        // the permissive wildcard (needed when the page is
                        // served from the macOS app's intendant:// scheme).
                        let table_posture = crate::gateway_routes::preflight_posture(opt_path);
                        let table_methods =
                            crate::gateway_routes::allowed_methods_for_path(opt_path);
                        let own_origin_scoped = match table_posture {
                            Some(crate::gateway_routes::CorsPosture::OwnOrigin) => true,
                            Some(_) => false,
                            None => {
                                (opt_path.starts_with("/api/")
                                    && !is_fleet_cors_access_path(opt_path)
                                    && !is_public_peer_access_request_path(request_line))
                                    || opt_path == "/mcp"
                            }
                        };
                        let fleet_scoped = matches!(
                            table_posture,
                            Some(crate::gateway_routes::CorsPosture::FleetAllowlist)
                        ) || (table_posture.is_none()
                            && is_fleet_cors_access_path(opt_path));
                        let response = if own_origin_scoped {
                            // Own-origin APIs (and /mcp) are same-origin (or
                            // app-scheme) only; a cross-origin preflight gets
                            // no ACAO and the browser stops there.
                            let methods = table_methods
                                .as_deref()
                                .unwrap_or("GET, POST, DELETE, OPTIONS");
                            let allowed = extract_origin_header(&header_text).filter(|origin| {
                                is_own_or_app_origin(origin, is_tls, &header_text)
                            });
                            match allowed {
                                Some(origin) => HttpResponse::new("204 No Content")
                                    .header("Access-Control-Allow-Origin", origin)
                                    .header("Access-Control-Allow-Methods", methods)
                                    .header(
                                        "Access-Control-Allow-Headers",
                                        "Content-Type, Authorization",
                                    )
                                    .header("Access-Control-Max-Age", "86400")
                                    .header("Vary", "Origin")
                                    .header("Connection", "close")
                                    .into_string(),
                                None => HttpResponse::new("204 No Content")
                                    .header("Vary", "Origin")
                                    .header("Connection", "close")
                                    .into_string(),
                            }
                        } else if fleet_scoped {
                            let methods =
                                table_methods.as_deref().unwrap_or("GET, POST, OPTIONS");
                            let cert_dir = crate::access::backend::select_backend().cert_dir();
                            let allowed = extract_origin_header(&header_text).filter(|origin| {
                                fleet_access_origin_allowed(
                                    origin,
                                    is_tls,
                                    &header_text,
                                    peer_registry.as_ref(),
                                    &cert_dir,
                                )
                            });
                            match allowed {
                                Some(origin) => HttpResponse::new("204 No Content")
                                    .header("Access-Control-Allow-Origin", origin)
                                    .header("Access-Control-Allow-Methods", methods)
                                    .header(
                                        "Access-Control-Allow-Headers",
                                        "Content-Type, Authorization",
                                    )
                                    .header("Access-Control-Max-Age", "86400")
                                    .header("Vary", "Origin")
                                    .header("Connection", "close")
                                    .into_string(),
                                None => HttpResponse::new("204 No Content")
                                    .header("Vary", "Origin")
                                    .header("Connection", "close")
                                    .into_string(),
                            }
                        } else {
                            let methods = table_methods
                                .as_deref()
                                .unwrap_or("GET, POST, DELETE, OPTIONS");
                            HttpResponse::new("204 No Content")
                                .header("Access-Control-Allow-Origin", "*")
                                .header("Access-Control-Allow-Methods", methods)
                                .header(
                                    "Access-Control-Allow-Headers",
                                    "Content-Type, Authorization",
                                )
                                .header("Access-Control-Max-Age", "86400")
                                .header("Connection", "close")
                                .into_string()
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }

                    if tls_client_cert_required
                        && !tls_client_cert_present
                        && !is_loopback_cleartext_mcp_request(peer_addr, is_tls, &header_text)
                        && !is_public_peer_access_request_path(request_line)
                        && !is_public_org_grant_path(request_line)
                        && !is_public_connect_bootstrap_path(request_line)
                    {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": "mTLS client certificate required"
                        })
                        .to_string();
                        let response = HttpResponse::with_content("401 Unauthorized", "application/json", body)
                            .header("Cache-Control", "no-cache")
                            .header("Connection", "close")
                            .into_string();
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }

                    let cert_dir = crate::access::backend::select_backend().cert_dir();
                    let http_access_context = match http_access_context(
                        &cert_dir,
                        peer_connection_identity.as_ref(),
                        tls_client_cert_fingerprint.as_deref(),
                        tls_client_cert_present,
                        is_tls,
                    ) {
                        Ok(context) => context,
                        Err(message) => {
                            use tokio::io::AsyncWriteExt;
                            let response = json_error("500 Internal Server Error", message);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    };

                    if let Some((op, kind)) = peer_filesystem_query_request(req_method, req_path) {
                        let path = query_param(request_line, "path").unwrap_or_default();
                        if let Err(message) = authorize_http_filesystem_access(
                            &http_access_context,
                            peer_connection_identity.as_ref(),
                            op,
                            kind,
                            &path,
                            &bus,
                        ) {
                            use tokio::io::AsyncWriteExt;
                            let response = json_error("403 Forbidden", message);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    // Federation auth enforcement. Applied before any
                    // federation API branch in the dispatch chain
                    // below; non-federation paths (WASM, frames,
                    // dashboard HTML, /config, /.well-known, /ws,
                    // /static/*) sail through unauthenticated. See
                    // `is_federation_path` for the exact set and the
                    // `inbound_bearer_token` docs on `spawn_web_gateway`
                    // for the design rationale.
                    if is_federation_path(request_line) {
                        if let Some(op) = crate::peer::access_policy::federation_http_operation(
                            req_method, req_path,
                        ) {
                            let decision = http_access_context.decision(op);
                            if !decision.allowed {
                                use tokio::io::AsyncWriteExt;
                                let body = serde_json::json!({
                                    "error": "principal does not allow this operation",
                                    "principal": http_access_context.principal.as_value(),
                                    "permission": decision.permission,
                                    "reason": decision.reason,
                                })
                                .to_string();
                                let response = HttpResponse::with_content("403 Forbidden", "application/json", body)
                                    .header("Cache-Control", "no-cache")
                                    .header("Connection", "close")
                                    .into_string();
                                let _ = stream.write_all(response.as_bytes()).await;
                                finalize_http_stream(&mut stream).await;
                                return;
                            }
                        }
                        if let Err((status, body)) =
                            verify_bearer_token(&header_text, inbound_bearer_token.as_deref())
                        {
                            use tokio::io::AsyncWriteExt;
                            let reason = match status {
                                401 => "Unauthorized",
                                _ => "Error",
                            };
                            let response = HttpResponse::with_content(format!("{} {}", status, reason), "application/json", body)
                                .header("Cache-Control", "no-cache")
                                .header("WWW-Authenticate", "Bearer")
                                .header("Connection", "close")
                                .into_string();
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    if let Some(op) = dashboard_http_operation(req_method, req_path) {
                        let decision = http_access_context.decision(op);
                        if !decision.allowed {
                            use tokio::io::AsyncWriteExt;
                            let response =
                                http_access_forbidden_response(&http_access_context, decision);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    // API origin gate + CORS echo. A browser sends an Origin
                    // header on every cross-origin request (and on
                    // same-origin POSTs); the browser-attached mTLS
                    // certificate must not let an arbitrary website drive or
                    // read these APIs cross-site. Policy:
                    //   - no Origin header (same-origin GETs, curl, native
                    //     code, the macOS app's URLSession proxy): untouched;
                    //   - own origin or the intendant:// app scheme: allowed;
                    //   - fleet-allowlisted origins: allowed on the six fleet
                    //     Access APIs, which also echo the origin so the
                    //     anchor page can read the responses;
                    //   - anything else on any /api/ path: 403, except the
                    //     public doorbell, which is designed to be knocked on.
                    let request_origin = extract_origin_header(&header_text);
                    let mut fleet_cors_origin: Option<String> = None;
                    if let Some(origin) = request_origin.as_deref().filter(|_| {
                        req_path.starts_with("/api/")
                            && !is_public_peer_access_request_path(request_line)
                            && !is_public_org_grant_path(request_line)
                    }) {
                        let own = is_own_or_app_origin(origin, is_tls, &header_text);
                        let fleet_allowed = !own
                            && is_fleet_cors_access_path(req_path)
                            && fleet_access_origin_allowed(
                                origin,
                                is_tls,
                                &header_text,
                                peer_registry.as_ref(),
                                &cert_dir,
                            );
                        if fleet_allowed {
                            fleet_cors_origin = Some(origin.to_string());
                        } else if !own {
                            use tokio::io::AsyncWriteExt;
                            let body = serde_json::json!({
                                "error": "cross-origin caller is not allowed on this API",
                                "origin": origin,
                            })
                            .to_string();
                            let response = json_response("403 Forbidden", body);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    if let Some((route, _route_captures)) =
                        crate::gateway_routes::match_route(req_method, req_path)
                    {
                        // Table-dispatched routes: every /api/* and /mcp route is
                        // declared once in gateway_routes::ROUTES (which the IAM
                        // gate above already consulted through
                        // dashboard_http_operation). The if/else chain below serves
                        // only the non-API surface (connect bootstrap, recordings,
                        // frames, debug, config, static assets, SPA fallback) — a
                        // route is served by the table or the chain, never both.
                        // Handler bodies moved here verbatim from their chain arms;
                        // never add an API route to the chain, declare it in the
                        // table instead.
                        use crate::gateway_routes::{BodyPolicy, RouteHandlerId};
                        // Dispatch owns request-body consumption: the body is
                        // read (and capped) here per the route's declared
                        // BodyPolicy, so a handler can no longer forget its
                        // cap — the old readers' unbounded Content-Length
                        // allocation is retired from the API surface. None
                        // and Streaming routes get an empty string; uploads
                        // and the doorbell keep driving the stream
                        // themselves.
                        let route_body = match route.body {
                            BodyPolicy::None | BodyPolicy::Streaming => String::new(),
                            BodyPolicy::Default | BodyPolicy::Capped(_) => {
                                let cap = match route.body {
                                    BodyPolicy::Capped(cap) => cap,
                                    _ => crate::gateway_routes::DEFAULT_BODY_CAP_BYTES,
                                };
                                match read_request_body_capped(&mut stream, &header_text, cap)
                                    .await
                                {
                                    Ok(body) => body,
                                    Err((status, body)) => {
                                        use tokio::io::AsyncWriteExt;
                                        let base = HttpResponse::json(
                                            status_reason(status),
                                            body,
                                        );
                                        let response =
                                            match crate::gateway_routes::preflight_posture(
                                                req_path,
                                            ) {
                                                Some(
                                                    crate::gateway_routes::CorsPosture::Public,
                                                ) => base.public_cors(),
                                                Some(
                                                    crate::gateway_routes::CorsPosture::FleetAllowlist,
                                                ) => base.fleet_cors(fleet_cors_origin.as_deref()),
                                                _ => base,
                                            };
                                        let _ =
                                            stream.write_all(&response.into_bytes()).await;
                                        finalize_http_stream(&mut stream).await;
                                        return;
                                    }
                                }
                            }
                        };
                        match route.handler {
                            RouteHandlerId::FsWrite => {
                                return handle_fs_write(
                                    stream,
                                    route_body,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionCurrentChanges => {
                                return handle_session_current_changes(
                                    stream,
                                    request_line,
                                    project_root_for_changes,
                                    snapshot_dir,
                                )
                                .await;
                            }
                            RouteHandlerId::WorktreesInspect => {
                                return handle_worktrees_inspect(stream, route_body).await;
                            }
                            RouteHandlerId::WorktreesRemove => {
                                return handle_worktrees_remove(
                                    stream,
                                    route_body,
                                    worktree_inventory_cache,
                                )
                                .await;
                            }
                            RouteHandlerId::WorktreesScan => {
                                return handle_worktrees_scan(
                                    stream,
                                    project_root,
                                    worktree_inventory_cache,
                                )
                                .await;
                            }
                            RouteHandlerId::WorktreesList => {
                                return handle_worktrees_list(
                                    stream,
                                    worktree_inventory_cache,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionsList => {
                                return handle_sessions_list(stream, request_line).await;
                            }
                            RouteHandlerId::FsStat => {
                                return handle_fs_stat(stream, request_line).await;
                            }
                            RouteHandlerId::FsList => {
                                return handle_fs_list(stream, request_line).await;
                            }
                            RouteHandlerId::FsRead => {
                                return handle_fs_read(stream, &header_text, request_line).await;
                            }
                            RouteHandlerId::FsMkdir => {
                                return handle_fs_mkdir(
                                    stream,
                                    route_body,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::FsRename => {
                                return handle_fs_rename(
                                    stream,
                                    route_body,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::FsDelete => {
                                return handle_fs_delete(
                                    stream,
                                    route_body,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentHistory => {
                                return handle_current_history(stream, file_watcher).await;
                            }
                            RouteHandlerId::CurrentRollback => {
                                return handle_current_rollback(
                                    stream,
                                    route_body,
                                    bus,
                                    query_ctx,
                                    file_watcher,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentRedo => {
                                return handle_current_redo(stream, query_ctx, file_watcher)
                                .await;
                            }
                            RouteHandlerId::CurrentPrune => {
                                return handle_current_prune(stream, file_watcher).await;
                            }
                            RouteHandlerId::CurrentAgentOutput => {
                                return handle_current_agent_output(
                                    stream,
                                    route_body,
                                    query_ctx,
                                    session_log,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentUploadsPost => {
                                return handle_current_uploads_post(
                                    stream,
                                    &header_text,
                                    request_line,
                                    discard,
                                    bus,
                                    project_root_for_changes,
                                    session_log,
                                    daemon_session_id,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentUploadsGet => {
                                return handle_current_uploads_get(
                                    stream,
                                    request_line,
                                    project_root_for_changes,
                                    session_log,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentUploadDelete => {
                                return handle_current_upload_delete(
                                    stream,
                                    request_line,
                                    bus,
                                    project_root_for_changes,
                                    session_log,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionDelete => {
                                return handle_session_delete(stream, request_line).await;
                            }
                            RouteHandlerId::SessionAgentOutput => {
                                return handle_session_agent_output(
                                    stream,
                                    route_body,
                                    request_line,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionSubRouter => {
                                return handle_session_sub_router(
                                    stream,
                                    request_line,
                                    session_log,
                                    query_ctx,
                                )
                                .await;
                            }
                            RouteHandlerId::McAnchors => {
                                return handle_mc_anchors(stream, request_line, session_log).await;
                            }
                            RouteHandlerId::McRecords => {
                                return handle_mc_records(stream, request_line, session_log).await;
                            }
                            RouteHandlerId::McFission => {
                                return handle_mc_fission(stream, request_line, session_log).await;
                            }
                            RouteHandlerId::SessionsStream => {
                                return handle_sessions_stream(stream, request_line).await;
                            }
                            RouteHandlerId::SessionsSearch => {
                                return handle_sessions_search(stream, request_line).await;
                            }
                            RouteHandlerId::ProjectRoot => {
                                return handle_project_root(stream, project_root).await;
                            }
                            RouteHandlerId::SettingsPost => {
                                return handle_settings_post(
                                    stream,
                                    route_body,
                                    bus,
                                    project_root,
                                )
                                .await;
                            }
                            RouteHandlerId::SettingsGet => {
                                return handle_settings_get(
                                    stream,
                                    project_root,
                                    runtime_settings,
                                )
                                .await;
                            }
                            RouteHandlerId::ApiKeysPost => {
                                return handle_api_keys_post(stream, route_body).await;
                            }
                            RouteHandlerId::ApiKeyStatus => {
                                return handle_api_key_status(stream).await;
                            }
                            RouteHandlerId::ExternalAgents => {
                                return handle_external_agents(stream, project_root).await;
                            }
                            RouteHandlerId::DiagnosticsVisualFreshness => {
                                return handle_diagnostics_visual_freshness(
                                    stream,
                                    route_body,
                                    request_line,
                                )
                                .await;
                            }
                            RouteHandlerId::Displays => {
                                return handle_displays(stream, session_registry).await;
                            }
                            RouteHandlerId::Doorbell => {
                                return handle_doorbell(
                                    stream,
                                    &header_text,
                                    request_line,
                                    req_method,
                                    peer_access_request_config,
                                    source_hint,
                                    is_tls,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessOrgGrantPresent => {
                                return handle_access_org_grant_present(
                                    stream,
                                    route_body,
                                    req_method,
                                    agent_card_value_for_targets,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessOrgRevocations => {
                                return handle_access_org_revocations(stream, req_path).await;
                            }
                            RouteHandlerId::AccessOrgApplyRenew => {
                                return handle_access_org_apply_renew(
                                    stream,
                                    route_body,
                                    req_method,
                                    req_path,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessIamGrants => {
                                return handle_access_iam_grants(
                                    stream,
                                    route_body,
                                    req_method,
                                    req_path,
                                    http_access_context,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessOrgManage => {
                                return handle_access_org_manage(
                                    stream,
                                    route_body,
                                    req_method,
                                    req_path,
                                    http_access_context,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessEnrollmentDecide => {
                                return handle_access_enrollment_decide(
                                    stream,
                                    route_body,
                                    req_method,
                                    http_access_context,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessEnrollmentRequests => {
                                return handle_access_enrollment_requests(
                                    stream,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessIamState => {
                                return handle_access_iam_state(stream, fleet_cors_origin).await;
                            }
                            RouteHandlerId::AccessOverview => {
                                return handle_access_overview(
                                    stream,
                                    http_access_context,
                                    fleet_cors_origin,
                                    peer_registry,
                                    agent_card_value_for_targets,
                                )
                                .await;
                            }
                            RouteHandlerId::DashboardTargets => {
                                return handle_dashboard_targets(
                                    stream,
                                    peer_registry,
                                    agent_card_value_for_targets,
                                )
                                .await;
                            }
                            RouteHandlerId::PeersSubRouter => {
                                return handle_peers_sub_router(
                                    stream,
                                    route_body,
                                    request_line,
                                    req_method,
                                    bus,
                                    project_root,
                                    peer_registry,
                                )
                                .await;
                            }
                            RouteHandlerId::CoordinatorRoute => {
                                return handle_coordinator_route(
                                    stream,
                                    route_body,
                                    req_method,
                                    peer_registry,
                                )
                                .await;
                            }
                            RouteHandlerId::McpPost => {
                                return handle_mcp_post(
                                    stream,
                                    route_body,
                                    &header_text,
                                    request_line,
                                    peer_connection_identity,
                                    mcp_server,
                                    is_tls,
                                    tls_client_cert_present,
                                    tls_client_cert_fingerprint,
                                    peer_addr,
                                )
                                .await;
                            }
                            RouteHandlerId::McpStream => {
                                return handle_mcp_stream(stream, &header_text, is_tls).await;
                            }
                        }
                    } else if let Some(allow) =
                        crate::gateway_routes::allowed_methods_for_path(req_path)
                    {
                        // Declared API path, undeclared method: uniform 405
                        // with an Allow header derived from the route table.
                        // Before the method tightening these requests either
                        // reached a method-blind read handler or fell all the
                        // way through to the SPA-shell fallback. CORS posture
                        // mirrors the path's declared posture so cross-origin
                        // fleet/public callers can read the error.
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": "method not allowed",
                            "allow": allow,
                        })
                        .to_string();
                        let base = HttpResponse::json("405 Method Not Allowed", body)
                            .header("Allow", &allow);
                        let response = match crate::gateway_routes::preflight_posture(req_path) {
                            Some(crate::gateway_routes::CorsPosture::Public) => base.public_cors(),
                            Some(crate::gateway_routes::CorsPosture::FleetAllowlist) => {
                                base.fleet_cors(fleet_cors_origin.as_deref())
                            }
                            _ => base,
                        };
                        let _ = stream.write_all(&response.into_bytes()).await;
                    } else if req_method == "GET" && req_path == "/connect/bootstrap" {
                        use tokio::io::AsyncWriteExt;
                        let body = connect_bootstrap_html();
                        let response = html_response("200 OK", body);
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "GET" && req_path == "/connect/status" {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "ok": true,
                            "transport": "webrtc-dashboard-control",
                            "signaling": "connect-bootstrap-local",
                            "mtls_required_for_dashboard": tls_client_cert_required,
                        });
                        let _ = stream
                            .write_all(with_public_cors(json_ok(body)).as_bytes())
                            .await;
                    } else if req_method == "POST" && req_path == "/connect/dashboard/offer" {
                        use tokio::io::AsyncWriteExt;
                        let body_text = match read_request_body_capped(
                            &mut stream,
                            &header_text,
                            CONNECT_SIGNALING_BODY_CAP_BYTES,
                        )
                        .await
                        {
                            Ok(body) => body,
                            Err((status, body)) => {
                                let response =
                                    HttpResponse::json(status_reason(status), body).public_cors();
                                let _ = stream.write_all(&response.into_bytes()).await;
                                finalize_http_stream(&mut stream).await;
                                return;
                            }
                        };
                        let response = with_public_cors(
                            connect_dashboard_offer_response(
                                &dashboard_control,
                                &body_text,
                                &agent_card_value_for_targets,
                            )
                            .await,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "POST" && req_path == "/connect/dashboard/ice" {
                        use tokio::io::AsyncWriteExt;
                        let body_text = match read_request_body_capped(
                            &mut stream,
                            &header_text,
                            CONNECT_SIGNALING_BODY_CAP_BYTES,
                        )
                        .await
                        {
                            Ok(body) => body,
                            Err((status, body)) => {
                                let response =
                                    HttpResponse::json(status_reason(status), body).public_cors();
                                let _ = stream.write_all(&response.into_bytes()).await;
                                finalize_http_stream(&mut stream).await;
                                return;
                            }
                        };
                        let response = with_public_cors(
                            connect_dashboard_ice_response(&dashboard_control, &body_text).await,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "POST" && req_path == "/connect/dashboard/close" {
                        use tokio::io::AsyncWriteExt;
                        let body_text = match read_request_body_capped(
                            &mut stream,
                            &header_text,
                            CONNECT_SIGNALING_BODY_CAP_BYTES,
                        )
                        .await
                        {
                            Ok(body) => body,
                            Err((status, body)) => {
                                let response =
                                    HttpResponse::json(status_reason(status), body).public_cors();
                                let _ = stream.write_all(&response.into_bytes()).await;
                                finalize_http_stream(&mut stream).await;
                                return;
                            }
                        };
                        let response = with_public_cors(
                            connect_dashboard_close_response(&dashboard_control, &body_text).await,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    // Route WASM binaries (need async write_all for large payloads)
                    } else if let Some(asset) = static_asset_arm(
                        req_method,
                        req_path,
                        &[
                            "/wasm-web/presence_web_bg.wasm",
                            "/wasm-station/station_web_bg.wasm",
                        ],
                    ) {
                        let response = build_static_asset_response(
                            req_method,
                            &header_text,
                            req_query,
                            asset_version(),
                            asset.view(),
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else if let Some(asset) = static_asset_arm(
                        req_method,
                        req_path,
                        &[
                            "/icon-128.png",
                            "/favicon.ico",
                            "/icon-512.png",
                            "/icon-512-maskable.png",
                            "/apple-touch-icon.png",
                            "/manifest.webmanifest",
                        ],
                    ) {
                        let response = build_static_asset_response(
                            req_method,
                            &header_text,
                            req_query,
                            asset_version(),
                            asset.view(),
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else if req_path.starts_with("/frames/") {
                        // Serve HQ frame images from the frame registry.
                        // URL format: /frames/<frame_id> (not /api/session/*/frames/*)
                        use tokio::io::AsyncWriteExt;
                        let frame_id = request_line
                            .split("/frames/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let data = if let Some(ref reg) = frame_registry {
                            let reg = reg.read().await;
                            reg.read_hq(frame_id).ok()
                        } else {
                            None
                        };
                        if let Some(jpeg_data) = data {
                            let header = HttpResponse::new("200 OK")
                                .header("Content-Type", "image/jpeg")
                                .header("Content-Length", jpeg_data.len().to_string())
                                .header("Cache-Control", "public, max-age=31536000, immutable")
                                .header("Connection", "close")
                                .into_string();
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&jpeg_data).await;
                        } else {
                            let body = "Frame not found";
                            let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                                .header("Connection", "close")
                                .into_string();
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if req_method == "POST" && req_path == "/session" {
                        let result = mint_session_token(&session_provider, &session_model).await;
                        let (status, body) = match result {
                            Ok(json) => ("200 OK", json),
                            Err(msg) => (
                                "502 Bad Gateway",
                                serde_json::json!({"error": msg}).to_string(),
                            ),
                        };
                        let response = HttpResponse::with_content(status, "application/json", body)
                            .header("Connection", "close")
                            .into_string();
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_path.starts_with("/recordings/") {
                        // Serve recording data: segment files and metadata.
                        use tokio::io::AsyncWriteExt;
                        let path_part = request_line
                            .split("/recordings/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let parts: Vec<&str> = path_part.split('/').collect();

                        if let Some(ref rec_reg) = recording_registry {
                            let reg = rec_reg.read().await;

                            if parts.len() == 2 && parts[1] == "segments" {
                                // GET /recordings/{stream}/segments — check session then daemon dir
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    // Fallback to daemon recordings dir
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let json: Vec<serde_json::Value> = segments
                                    .iter()
                                    .map(|s| {
                                        serde_json::json!({
                                            "filename": s.filename,
                                            "start_secs": s.start_secs,
                                            "end_secs": s.end_secs,
                                        })
                                    })
                                    .collect();
                                let body = serde_json::to_string(&json).unwrap_or("[]".to_string());
                                let response = HttpResponse::with_content("200 OK", "application/json", body)
                                    .header("Cache-Control", "no-cache")
                                    .header("Connection", "close")
                                    .into_string();
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 && parts[1] == "playlist.m3u8" {
                                // GET /recordings/{stream}/playlist.m3u8 — HLS playlist
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let m3u8 = recording_playlist_m3u8(&segments);
                                let response = HttpResponse::with_content("200 OK", "application/vnd.apple.mpegurl", m3u8)
                                    .header("Cache-Control", "no-cache")
                                    .header("Connection", "close")
                                    .into_string();
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 {
                                // GET /recordings/{stream}/{filename} — serve segment file
                                let stream_name = parts[0];
                                let filename = parts[1];
                                // Validate filename to prevent path traversal
                                let valid = filename.starts_with("seg_")
                                    && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                                    && filename.len() < 30
                                    && !filename.contains("..");
                                if valid {
                                    // Check session dir first, then daemon dir
                                    let session_path = reg
                                        .session_dir()
                                        .join("recordings")
                                        .join(stream_name)
                                        .join(filename);
                                    let daemon_path = crate::debug::daemon_recordings_dir()
                                        .join(stream_name)
                                        .join(filename);
                                    let seg_path = if session_path.exists() {
                                        session_path
                                    } else {
                                        daemon_path
                                    };
                                    let content_type = if filename.ends_with(".ts") {
                                        "video/mp2t"
                                    } else {
                                        "video/mp4"
                                    };
                                    match tokio::fs::read(&seg_path).await {
                                        Ok(data) => {
                                            let header = HttpResponse::new("200 OK")
                                                .header("Content-Type", content_type)
                                                .header("Content-Length", data.len().to_string())
                                                .header("Cache-Control", "public, max-age=3600")
                                                .header("Connection", "close")
                                                .into_string();
                                            let _ = stream.write_all(header.as_bytes()).await;
                                            let _ = stream.write_all(&data).await;
                                        }
                                        Err(_) => {
                                            let body = "Segment not found";
                                            let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                                                .header("Connection", "close")
                                                .into_string();
                                            let _ = stream.write_all(response.as_bytes()).await;
                                        }
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = HttpResponse::with_content("400 Bad Request", "text/plain", body)
                                        .header("Connection", "close")
                                        .into_string();
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                let body = "Not found";
                                let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                                    .header("Connection", "close")
                                    .into_string();
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else {
                            let body = "Recording not available";
                            let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                                .header("Connection", "close")
                                .into_string();
                            use tokio::io::AsyncWriteExt;
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if req_path == "/recordings" {
                        // GET /recordings — list all streams (session + daemon-scoped)
                        use tokio::io::AsyncWriteExt;

                        let body = recordings_list_response_body(recording_registry.clone()).await;
                        let response = HttpResponse::with_content("200 OK", "application/json", body)
                            .header("Cache-Control", "no-cache")
                            .header("Connection", "close")
                            .into_string();
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_path == "/debug" {
                        // Debug endpoint: returns agent state + voice connection info
                        let state = query_ctx.as_ref().map(|ctx| {
                            ctx.agent_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                        });
                        let vd = voice_debug
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let active_id = active_presence
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                            .map(|a| a.connection_id.clone());
                        let debug_json = serde_json::json!({
                            "agent_state": state,
                            "voice": vd,
                            "active_connection_id": active_id,
                        })
                        .to_string();
                        let response = HttpResponse::with_content("200 OK", "application/json", debug_json)
                            .header("Connection", "close")
                            .into_string();
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if let Some(response) = dashboard_local_file_response(request_line) {
                        use tokio::io::AsyncWriteExt;
                        match response {
                            DashboardLocalFileResponse::Html { status, body } => {
                                let response = HttpResponse::with_content(status, "text/html; charset=utf-8", body)
                                    .header("Cache-Control", "no-cache")
                                    .header("Connection", "close")
                                    .into_string();
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                            DashboardLocalFileResponse::Bytes {
                                status,
                                content_type,
                                bytes,
                            } => {
                                let header = HttpResponse::new(status)
                                    .header("Content-Type", content_type)
                                    .header("Content-Length", bytes.len().to_string())
                                    .header("Cache-Control", "no-cache")
                                    .header("X-Content-Type-Options", "nosniff")
                                    .header("Connection", "close")
                                    .into_string();
                                let _ = stream.write_all(header.as_bytes()).await;
                                let _ = stream.write_all(&bytes).await;
                            }
                        }
                    } else if let Some(asset) = static_asset_arm(
                        req_method,
                        req_path,
                        &[
                            "/wasm-web/presence_web.js",
                            "/wasm-station/station_web.js",
                            "/three.module.min.js",
                            "/codemirror-bundle.js",
                            "/codemirror-bundle.css",
                            "/audio-processor.js",
                            "/icon-128.png",
                            "/favicon.ico",
                            "/icon-512.png",
                            "/icon-512-maskable.png",
                            "/apple-touch-icon.png",
                            "/manifest.webmanifest",
                        ],
                    ) {
                        let response = build_static_asset_response(
                            req_method,
                            &header_text,
                            req_query,
                            asset_version(),
                            asset.view(),
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else if req_path == "/.well-known/agent-card.json" || req_path == "/config" {
                        let body = if req_path == "/.well-known/agent-card.json" {
                            // Canonical peer identity + capability surface.
                            // Served alongside /config so the browser and
                            // federated peers can discover who this daemon
                            // is without parsing the voice-runtime config.
                            agent_card_json.clone()
                        } else {
                            config_json.clone()
                        };
                        // CORS: allow the multi-host dashboard to
                        // `fetch()` /config and /.well-known/agent-card.json
                        // on this daemon from a page served by a sibling
                        // daemon (cross-origin). `*` works because our
                        // fetches don't send credentials.
                        let response = HttpResponse::with_content("200 OK", "application/json", body)
                            .header("Cache-Control", "no-cache")
                            .header("Access-Control-Allow-Origin", "*")
                            .header("Connection", "close")
                            .into_string();
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "GET" || req_method == "HEAD" {
                        // Default: serve app.html (also matches /app for
                        // backward compat). The entry point stays no-cache —
                        // it carries the rewritten `?v=` busters — but gets
                        // an ETag (cheap 304 revalidation) and gzip. ETag +
                        // gzip are computed once per gateway spawn, on first
                        // page load: the rewritten HTML is gateway-scoped,
                        // unlike the constants behind `embedded_static_asset`.
                        // Under INTENDANT_APP_HTML_PATH the disk copy is
                        // re-read (and re-tagged) on every request instead.
                        let response = if let Some(path) = app_html_override.as_deref() {
                            app_html_override_response(req_method, &header_text, req_query, path)
                        } else {
                            let (etag, gzip) = app_html_cache.get_or_init(|| {
                                (
                                    asset_etag(app_html.as_bytes()),
                                    gzip_compress(app_html.as_bytes()),
                                )
                            });
                            build_static_asset_response(
                                req_method,
                                &header_text,
                                req_query,
                                asset_version(),
                                StaticAssetView {
                                    content_type: "text/html; charset=utf-8",
                                    body: app_html.as_bytes(),
                                    etag,
                                    gzip: Some(gzip),
                                    cache_control: Some("no-cache"),
                                },
                            )
                        };
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else {
                        // Non-GET/HEAD fallback: plain app.html, as before.
                        let response = HttpResponse::with_content("200 OK", "text/html; charset=utf-8", app_html.as_bytes())
                            .header("Cache-Control", "no-cache")
                            .header("Access-Control-Allow-Origin", "*")
                            .header("Connection", "close")
                            .into_string();
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    }

                    // Flush + cleanly shut down the stream before this task
                    // returns and drops it. Mandatory for the TLS path so the
                    // final ciphertext records reach the socket (rustls buffers
                    // them; dropping mid-buffer truncates large bodies); a
                    // harmless pass-through on plain TCP. Covers every
                    // fall-through chain arm above in one place; the early
                    // `return`s (OPTIONS / failed federation auth) finalize
                    // inline before returning, and every table-dispatched
                    // handler owns its stream and finalizes it itself.
                    finalize_http_stream(&mut stream).await;
                }
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutboundEvent;
    use tokio::io::AsyncWriteExt;
    use crate::test_support::TEST_ENV_LOCK;
    use crate::web_gateway::tests::{EnvVarGuard, next_ws_json_matching};

    async fn next_ws_json_type<S>(ws_rx: &mut S, ty: &str) -> serde_json::Value
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        next_ws_json_matching(ws_rx, |json| json["t"] == ty).await
    }

    #[test]
    fn accept_error_classifier_keeps_listener_alive_for_transient_errors() {
        assert!(should_continue_after_accept_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionAborted
        )));
        assert!(should_continue_after_accept_error(
            &std::io::Error::from_raw_os_error(24)
        ));
        assert!(!should_continue_after_accept_error(
            &std::io::Error::from_raw_os_error(9)
        ));
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
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
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
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
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
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
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
            .write_all(b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\n\r\n")
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
            Some(crate::peer::PeerKind::Intendant),
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
            .write_all(b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
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
            .write_all(b"GET /audio-processor.js HTTP/1.1\r\nHost: localhost\r\n\r\n")
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

    /// The gateway must be able to re-establish its listener on the exact
    /// address a dead one occupied (accept() EINVAL/EBADF recovery path),
    /// and the fresh listener must actually accept connections.
    #[tokio::test]
    async fn rebind_dead_tcp_listener_restores_reachability() {
        let original = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = original.local_addr().unwrap();
        drop(original);

        let rebound = rebind_dead_tcp_listener(addr).expect("rebind on the freed address");
        assert_eq!(rebound.local_addr().unwrap(), addr);

        let (client, (server, _peer)) = tokio::join!(
            tokio::net::TcpStream::connect(addr),
            async { rebound.accept().await.unwrap() },
        );
        client.expect("client connects to rebound listener");
        drop(server);
    }

    /// SO_REUSEADDR does not override an actively bound listener on Unix —
    /// the accept-loop recovery MUST drop the dead socket before rebinding,
    /// or every attempt self-inflicts EADDRINUSE (seen live: a daemon whose
    /// accept loop died spun on rebind for over an hour while its own dead
    /// listener still owned the port). Windows semantics differ, so the
    /// still-bound assertion is Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn rebind_fails_while_dead_listener_is_still_bound() {
        let holder = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = holder.local_addr().unwrap();

        let err = rebind_dead_tcp_listener(addr)
            .expect_err("rebinding must fail while the previous listener still holds the address");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        drop(holder);
        assert!(rebind_dead_tcp_listener(addr).is_ok());
    }
}
