//! Advertisement + agent card: advertise-URL resolution/auto-detection,
//! the local agent card, and gateway config assembly.

use super::*;

/// Build a `WebGatewayConfig` from the presence config's live fields,
/// falling back to environment variable detection.
///
/// Returns voice/runtime fields only. Daemon identity (host label,
/// version, git sha) lives on the Agent Card at
/// `/.well-known/agent-card.json` and is assembled at gateway spawn
/// time via [`build_local_agent_card`].
pub fn build_config(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_config: crate::display::IceConfig,
    federation_allow_h264: bool,
) -> WebGatewayConfig {
    let mut config = build_config_inner(
        live_provider,
        live_model,
        transcription_enabled,
        ice_config.ice_servers,
        federation_allow_h264,
    );
    // Stamped once here — the /config route and the tunnel `config` RPC both
    // serve this instance, so every lane reports the same served-bundle
    // build and stale tabs can nudge themselves to reload.
    config.app_build = super::static_assets::app_build().to_string();
    config
}

// ---------------------------------------------------------------------------
// /api/peers helpers
// ---------------------------------------------------------------------------

/// Resolve the list of WebSocket URLs to advertise in the Agent
/// Card for this daemon, in preference order.
///
/// **Additive auto-detection.** Mirrors WebRTC's host-candidate
/// gathering pattern: the daemon enumerates its own routable
/// interfaces via [`crate::access::routable_local_addrs`] and emits one
/// URL per address by default, so the operator doesn't need to type
/// their own LAN IP into `--advertise-url`. The operator's overrides
/// (CLI `--advertise-url` or `[server.advertise]` in intendant.toml)
/// are *prepended* — they win on preference order, but the auto-
/// detected entries still ride along as fallbacks. The connecting
/// peer's `MultiTransport::connect` walks the merged list top-down
/// and picks the first that succeeds.
///
/// ## Bind-address rules
///
/// - **Specific bind** (e.g. `192.168.1.42:8765`): only that one IP
///   is auto-detected. The operator narrowed the listener for a
///   reason; we don't second-guess by also enumerating other
///   interfaces.
/// - **Wildcard bind** (`0.0.0.0` / `::`): every routable interface
///   becomes one URL. Loopback is excluded — advertising loopback to
///   remote peers is useless. If the operator wants to expose
///   loopback (e.g. for self-peering tests), they can pass it via
///   `--advertise-url`.
///
/// ## Fallbacks (in order, when auto-detection finds nothing)
///
/// 1. Resolved host label ([`crate::access::resolve_host_label`]) —
///    works on a trusted LAN with mDNS, fragile elsewhere. Last-
///    ditch best-effort.
/// 2. `ws://localhost:0/ws` if there's no listener at all
///    (shouldn't happen in practice; the listener is always bound by
///    the time spawn is called). Card stays valid; URL won't work.
///
/// Dedup: exact-string match. If the operator's override happens to
/// match an auto-detected URL, only the operator's copy is kept.
///
/// ## Scheme
///
/// `tls_enabled` selects the auto-detected URL scheme: `wss://` when the
/// dashboard is served over TLS (`--tls` / `[server.tls]`), `ws://`
/// otherwise. This keeps advertised peer URLs honest — a TLS daemon is
/// HTTPS/WSS-only (see the strict-TLS demux in `spawn_web_gateway`), so a
/// peer handed a `ws://` URL would be refused. Operator overrides are
/// taken verbatim (the operator owns their scheme) and the final
/// no-listener fallback tracks the flag too.
pub(crate) fn resolve_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    overrides: &[String],
    tls_enabled: bool,
) -> Vec<String> {
    let port = local_addr.map(|a| a.port()).unwrap_or(0);

    // Auto-detect. Operator overrides come first; auto entries append.
    let auto = auto_detect_advertise_urls(local_addr, port, tls_enabled);

    let mut out: Vec<String> = Vec::with_capacity(overrides.len() + auto.len());
    for url in overrides {
        if !out.contains(url) {
            out.push(url.clone());
        }
    }
    for url in auto {
        if !out.contains(&url) {
            out.push(url);
        }
    }

    if out.is_empty() {
        // No bind, no overrides, no interfaces. Card stays valid;
        // URL just won't work until the next daemon restart. Match the
        // TLS scheme so even this degenerate fallback is scheme-honest.
        out.push(format_ws_url("localhost", 0, tls_enabled));
    }
    out
}

/// Build the auto-detected URL list from the listener bind address.
/// See [`resolve_advertise_urls`] for the full resolution rules.
/// `tls_enabled` selects `wss://` vs `ws://` (see that fn's docstring).
pub(crate) fn auto_detect_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    port: u16,
    tls_enabled: bool,
) -> Vec<String> {
    use std::net::IpAddr;
    let Some(addr) = local_addr else {
        return Vec::new();
    };

    // Specific bind: that one IP wins, no enumeration.
    match addr.ip() {
        IpAddr::V4(v4) if !v4.is_unspecified() => {
            return vec![format_ws_url(&v4.to_string(), port, tls_enabled)];
        }
        IpAddr::V6(v6) if !v6.is_unspecified() => {
            return vec![format_ws_url(&format!("[{v6}]"), port, tls_enabled)];
        }
        _ => {}
    }

    // Wildcard bind: enumerate every non-loopback routable interface.
    // IPv4 entries sort before IPv6 — WebRTC ICE-TCP in WebKit/WKWebView
    // silently drops IPv6 ULA candidates (seen empirically against
    // fdc2::/8 addresses on macOS 15), so the *first* URL in the list
    // — which slice 3b's `maybe_rewrite_federated_answer` takes as the
    // relay candidate verbatim — needs to be the one browsers actually
    // dial. Within each address family we preserve `getifaddrs` order
    // (`stable_sort_by`), so a multi-NIC host that already had a
    // preferred primary interface keeps it.
    let mut ips = crate::access::routable_local_addrs(false);
    ips.sort_by(|a, b| match (a, b) {
        (IpAddr::V4(_), IpAddr::V6(_)) => std::cmp::Ordering::Less,
        (IpAddr::V6(_), IpAddr::V4(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    let mut urls: Vec<String> = ips
        .into_iter()
        .map(|ip| match ip {
            IpAddr::V6(v6) => format_ws_url(&format!("[{v6}]"), port, tls_enabled),
            ip => format_ws_url(&ip.to_string(), port, tls_enabled),
        })
        .collect();

    // No interfaces found (unusual — host with no networking?). Fall
    // back to the resolved host label so the card carries *something*
    // dialable on a trusted LAN with mDNS.
    if urls.is_empty() {
        urls.push(format_ws_url(
            &crate::access::resolve_host_label(),
            port,
            tls_enabled,
        ));
    }
    urls
}

/// Format one advertised WebSocket URL. `tls_enabled` picks the secure
/// scheme (`wss://`) so a TLS daemon never advertises a `ws://` URL a peer
/// would be refused on.
pub(crate) fn format_ws_url(host: &str, port: u16, tls_enabled: bool) -> String {
    let scheme = if tls_enabled { "wss" } else { "ws" };
    format!("{scheme}://{host}:{port}/ws")
}

/// Assemble the [`crate::peer::AgentCard`] for this daemon from live
/// runtime state.
///
/// Called once per `spawn_web_gateway` invocation, right after the
/// config is serialized — the result is cached as `agent_card_json`
/// and cloned into each per-connection handler, matching the pattern
/// used for `/config`.
///
/// Capabilities:
/// - `ComputerUse` and `Display` are advertised by every build. `Display`
///   offers are handled against whatever the local dashboard has activated;
///   an unopened display returns "no such display".
/// - Additional capabilities are not advertised here; doing so requires
///   runtime capability plumbing that does not yet exist.
///
/// `advertise_urls` is the preference-ordered list of WebSocket URLs
/// peers should try when dialing this daemon. Each becomes a
/// [`crate::peer::TransportSpec::IntendantWs`] entry in the card.
/// Built by [`resolve_advertise_urls`], which merges operator
/// overrides (`--advertise-url`, `[server.advertise]`) with auto-
/// detected fallback. The list is non-empty by construction.
///
/// `auth` is the [`crate::peer::AuthRequirements`] to advertise —
/// what connecting peers should send. Built by
/// `crate::main::build_local_advertised_auth` from
/// `[server.auth]` (advertised_transport + bearer_token) and the
/// access cert dir (for `pin-self-cert` fingerprint). Phase 1 of slice
/// 2c always passed `AuthRequirements::none()`; this signature
/// change lets the operator advertise mTLS / pinned-mTLS / bearer
/// in the card so connecting peers know what to send.
pub fn build_local_agent_card(
    advertise_urls: Vec<String>,
    auth: crate::peer::AuthRequirements,
) -> crate::peer::AgentCard {
    use crate::peer::{Capability, TransportSpec};
    let transports: Vec<TransportSpec> = advertise_urls
        .into_iter()
        .map(|url| TransportSpec::IntendantWs { url, relay: false })
        .collect();
    crate::peer::AgentCard::local_intendant(
        crate::access::resolve_host_label(),
        env!("CARGO_PKG_VERSION").to_string(),
        Some(env!("INTENDANT_GIT_SHA").to_string()),
        transports,
        vec![Capability::ComputerUse, Capability::Display],
        auth,
    )
}

// ---------------------------------------------------------------------------
// Live card rendering (RC-B2): identity attestation + relay candidate
// ---------------------------------------------------------------------------

/// Pure compose step for the card's dynamic blocks, split from
/// [`AgentCardLive`] so it is unit-testable without process-global
/// certificate or fleet state.
///
/// - `attestation` grafts the `identity_attestation` block (see
///   [`crate::access::identity_attestation`]).
/// - `relay_candidate_url` appends one relay-classed `intendant-ws`
///   transport **last** — operator overrides and auto-detected entries
///   in `base` keep walk-order precedence (`MultiTransport` probes in
///   card order) — deduplicated against URLs the operator already
///   listed (their verbatim entry wins, unmarked).
pub(crate) fn render_card_with_dynamics(
    base: &serde_json::Value,
    attestation: Option<&crate::access::identity_attestation::DaemonIdentityAttestation>,
    relay_candidate_url: Option<&str>,
) -> serde_json::Value {
    let mut value = base.clone();
    if let Some(attestation) = attestation {
        if let Ok(block) = serde_json::to_value(attestation) {
            value["identity_attestation"] = block;
        }
    }
    if let Some(url) = relay_candidate_url {
        if let Some(transports) = value
            .get_mut("transports")
            .and_then(serde_json::Value::as_array_mut)
        {
            let already_listed = transports
                .iter()
                .any(|t| t.get("url").and_then(serde_json::Value::as_str) == Some(url));
            if !already_listed {
                transports.push(serde_json::json!({
                    "type": "intendant-ws",
                    "url": url,
                    "relay": true,
                }));
            }
        }
    }
    value
}

/// Inputs that decide one rendered card revision. Re-rendering (and
/// re-signing the attestation) happens exactly when one of them moves —
/// which is how certificate rotation re-signs on the
/// `install_fleet_certificate` hook path (B0 ruling A5): the hook swaps
/// the resolver state this key is derived from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CardRenderKey {
    access_fingerprint: Option<String>,
    fleet_fingerprint: Option<String>,
    fleet_name: Option<String>,
}

/// The live agent card: the spawn-built base plus the dynamic blocks —
/// the daemon-identity attestation over the *current* TLS leaves, and
/// the relay-mode fleet-name candidate. Serve-time state because both
/// inputs move while the gateway runs (certificate renewal; the fleet
/// name arriving from Connect registration after spawn).
pub(crate) struct AgentCardLive {
    base: serde_json::Value,
    fallback_json: Arc<String>,
    /// Injection point for the signing identity (the
    /// `hosted_control_identity_path` pattern): production wires the
    /// daemon identity store's key path; tests inject a temp path or
    /// leave `None`, which serves the card without an attestation.
    attestation_identity_path: Option<std::path::PathBuf>,
    identity: std::sync::OnceLock<Option<crate::daemon_identity::DaemonIdentity>>,
    daemon_id: Option<String>,
    access_cert_dir: std::path::PathBuf,
    /// `Some(port)` exactly when the relay candidate is advertised:
    /// `[connect] relay_enabled` AND `relay_peer_admission` (the B0
    /// ruling's OPEN-3 auto-append condition), with the port taken from
    /// the relay endpoint. The fleet name itself is read live per
    /// render — it only exists after Connect registration.
    relay_candidate_port: Option<u16>,
    cache: Mutex<Option<(CardRenderKey, Arc<String>)>>,
}

impl AgentCardLive {
    pub(crate) fn new(
        base: serde_json::Value,
        config: &WebGatewayConfig,
        access_cert_dir: std::path::PathBuf,
    ) -> Self {
        let fallback_json =
            Arc::new(serde_json::to_string(&base).unwrap_or_else(|_| "{}".to_string()));
        let relay_candidate_port = (config.connect.relay_enabled
            && config.connect.relay_peer_admission)
            .then(|| relay_public_port(config.connect.relay_endpoint.as_deref()))
            .flatten();
        Self {
            base,
            fallback_json,
            attestation_identity_path: config.attestation_identity_path.clone(),
            identity: std::sync::OnceLock::new(),
            daemon_id: config
                .connect
                .daemon_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            access_cert_dir,
            relay_candidate_port,
            cache: Mutex::new(None),
        }
    }

    fn identity(&self) -> Option<&crate::daemon_identity::DaemonIdentity> {
        self.identity
            .get_or_init(|| {
                let path = self.attestation_identity_path.as_ref()?;
                match crate::daemon_identity::DaemonIdentity::load_or_create(path) {
                    Ok(identity) => Some(identity),
                    Err(e) => {
                        // Served-without-attestation is the honest
                        // degradation: identity-paired dialers then fail
                        // closed on public-name candidates (A2) instead
                        // of trusting an unattested leaf.
                        eprintln!(
                            "[agent-card] daemon identity unavailable ({e}); serving the card without an identity attestation"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    /// This daemon's identity public key, for pairing surfaces that
    /// stamp it into approval results (the doorbell).
    pub(crate) fn identity_public_key(&self) -> Option<String> {
        self.identity()
            .map(crate::daemon_identity::DaemonIdentity::public_key_b64u)
    }

    /// The card JSON to serve right now. Cached; re-rendered (and the
    /// attestation re-signed) when the live inputs move.
    pub(crate) fn render_json(&self) -> Arc<String> {
        let access_fingerprint =
            crate::access::certs::read_server_cert_fingerprint(&self.access_cert_dir);
        let fleet_fingerprint = crate::web_tls::current_fleet_leaf_fingerprint();
        let fleet_name = self.relay_candidate_port.and_then(|_| {
            crate::fleet_cert::status_snapshot()
                .name
                .filter(|name| !name.trim().is_empty())
        });
        let key = CardRenderKey {
            access_fingerprint,
            fleet_fingerprint,
            fleet_name,
        };
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached_key, json)) = cache.as_ref() {
                if *cached_key == key {
                    return json.clone();
                }
            }
        }

        let attestation = match self.identity() {
            Some(identity)
                if key.access_fingerprint.is_some() || key.fleet_fingerprint.is_some() =>
            {
                let now_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Some(crate::access::identity_attestation::sign_attestation(
                    identity,
                    self.daemon_id.as_deref(),
                    key.access_fingerprint.as_deref(),
                    key.fleet_fingerprint.as_deref(),
                    now_unix_ms,
                ))
            }
            _ => None,
        };
        let relay_url = match (&key.fleet_name, self.relay_candidate_port) {
            (Some(name), Some(port)) => Some(format!("wss://{name}:{port}/ws")),
            _ => None,
        };
        if attestation.is_none() && relay_url.is_none() {
            return self.fallback_json.clone();
        }
        let value =
            render_card_with_dynamics(&self.base, attestation.as_ref(), relay_url.as_deref());
        let json = Arc::new(serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()));
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, json.clone()));
        json
    }
}

/// The public dial-in port of the relay, from the daemon's own
/// `[connect] relay_endpoint` (`host:port`; a bare host means 443, the
/// wss default). The relay serves dial-ins and daemon dial-backs on the
/// same listener, so the endpoint this daemon tunnels to is also where
/// peers dial its fleet name.
fn relay_public_port(relay_endpoint: Option<&str>) -> Option<u16> {
    let endpoint = relay_endpoint.map(str::trim).filter(|e| !e.is_empty())?;
    let url = url::Url::parse(&format!("wss://{endpoint}")).ok()?;
    Some(url.port().unwrap_or(443))
}

pub(crate) fn build_config_inner(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_servers: Vec<crate::display::IceServer>,
    federation_allow_h264: bool,
) -> WebGatewayConfig {
    // If an explicit provider is given, use it directly.
    if let Some(provider) = live_provider {
        let model = live_model.unwrap_or(match provider {
            "openai" => "gpt-4o-realtime-preview",
            _ => "gemini-2.5-flash-native-audio-preview-12-2025",
        });
        let (input_rate, output_rate) = if provider == "openai" {
            (24000, 24000)
        } else {
            (16000, 24000)
        };
        return WebGatewayConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            input_sample_rate: input_rate,
            output_sample_rate: output_rate,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        };
    }

    // If an explicit live model is given, detect provider from the model name.
    if let Some(model) = live_model {
        if model.starts_with("gpt")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
        {
            return WebGatewayConfig {
                provider: "openai".to_string(),
                model: model.to_string(),
                input_sample_rate: 24000,
                output_sample_rate: 24000,
                transcription_enabled,
                ice_servers,
                federation_allow_h264,
                ..Default::default()
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        };
    }

    // Fall back to usable-key detection (leases shadow env vars).
    if crate::credential_leases::provider_key_available("OPENAI_API_KEY")
        && !crate::credential_leases::provider_key_available("GEMINI_API_KEY")
    {
        WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        }
    } else {
        WebGatewayConfig {
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_gateway::tests::{http_request, setup_peer_op_test};

    fn base_card_value() -> serde_json::Value {
        serde_json::to_value(build_local_agent_card(
            vec![
                "ws://operator.example:9000/ws".to_string(),
                "ws://192.168.1.42:8765/ws".to_string(),
            ],
            crate::peer::AuthRequirements::none(),
        ))
        .unwrap()
    }

    /// OPEN-3 as ruled: the relay candidate appends LAST — operator and
    /// auto-detected entries keep walk-order precedence — carrying the
    /// `relay: true` class stamp, and the whole card still parses as a
    /// typed `AgentCard` whose last transport classifies `Relayed`.
    #[test]
    fn relay_candidate_appends_last_with_class_stamp() {
        let base = base_card_value();
        let rendered = render_card_with_dynamics(
            &base,
            None,
            Some("wss://d-0123456789abcdef0123.fleet.example:443/ws"),
        );
        let transports = rendered["transports"].as_array().unwrap();
        assert_eq!(transports.len(), 3);
        assert_eq!(
            transports[0]["url"], "ws://operator.example:9000/ws",
            "operator entry keeps first position"
        );
        let last = &transports[2];
        assert_eq!(
            last["url"],
            "wss://d-0123456789abcdef0123.fleet.example:443/ws"
        );
        assert_eq!(last["relay"], true);

        let card: crate::peer::AgentCard = serde_json::from_value(rendered).unwrap();
        let last_spec = card.transports.last().unwrap();
        assert!(
            matches!(
                last_spec,
                crate::peer::TransportSpec::IntendantWs { relay: true, .. }
            ),
            "parsed spec carries the relay class: {last_spec:?}"
        );
        let link = crate::peer::handle::PeerLinkInfo::from_spec(last_spec).unwrap();
        assert_eq!(
            link.transport_class,
            crate::peer::handle::PeerTransportClass::Relayed,
            "the relay-candidate constructor is the one Relayed classifier"
        );
        // The other entries stay Direct — the class rides the spec, not
        // the peer.
        let first = crate::peer::handle::PeerLinkInfo::from_spec(&card.transports[0]).unwrap();
        assert_eq!(
            first.transport_class,
            crate::peer::handle::PeerTransportClass::Direct
        );
    }

    /// A URL the operator already listed is never duplicated — their
    /// verbatim entry wins (unmarked).
    #[test]
    fn relay_candidate_dedupes_operator_listed_url() {
        let base = serde_json::to_value(build_local_agent_card(
            vec!["wss://d-aa.fleet.example:443/ws".to_string()],
            crate::peer::AuthRequirements::none(),
        ))
        .unwrap();
        let rendered =
            render_card_with_dynamics(&base, None, Some("wss://d-aa.fleet.example:443/ws"));
        let transports = rendered["transports"].as_array().unwrap();
        assert_eq!(transports.len(), 1, "no duplicate: {transports:?}");
        assert!(
            transports[0].get("relay").is_none(),
            "the operator's verbatim entry stays unmarked"
        );
    }

    /// The attestation block grafts verifiably and the no-dynamics
    /// render is the base card unchanged.
    #[test]
    fn attestation_grafts_and_verifies_on_the_rendered_card() {
        let dir = tempfile::tempdir().unwrap();
        let identity =
            crate::daemon_identity::DaemonIdentity::load_or_create(dir.path().join("id.pk8"))
                .unwrap();
        let attestation = crate::access::identity_attestation::sign_attestation(
            &identity,
            Some("daemon-1"),
            Some("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"),
            None,
            42,
        );
        let base = base_card_value();
        let rendered = render_card_with_dynamics(&base, Some(&attestation), None);
        let card: crate::peer::AgentCard = serde_json::from_value(rendered).unwrap();
        let block = card.identity_attestation.expect("block grafted");
        assert!(crate::access::identity_attestation::verify_attestation(
            &block,
            &identity.public_key_b64u()
        )
        .is_ok());

        assert_eq!(
            render_card_with_dynamics(&base, None, None),
            base,
            "no dynamics = the base card byte-identically"
        );
    }

    /// The live renderer signs with the injected identity, binds the
    /// access store's current `server.crt`, re-signs when the leaf
    /// changes (the A5 rotation shape), and caches between renders (a
    /// stable `issued_at` while inputs hold still).
    #[test]
    fn agent_card_live_signs_caches_and_resigns_on_rotation() {
        let access_dir = tempfile::tempdir().unwrap();
        let first_leaf = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(access_dir.path().join("server.crt"), first_leaf.cert.pem()).unwrap();
        let identity_path = access_dir.path().join("id.pk8");
        let identity =
            crate::daemon_identity::DaemonIdentity::load_or_create(&identity_path).unwrap();

        let mut config = WebGatewayConfig::default();
        config.attestation_identity_path = Some(identity_path);
        let live = AgentCardLive::new(base_card_value(), &config, access_dir.path().to_path_buf());

        let first = live.render_json();
        let card: crate::peer::AgentCard = serde_json::from_str(&first).unwrap();
        let block = card.identity_attestation.expect("attestation served");
        let expected_fp = crate::access::pinning::format_fingerprint(
            &crate::access::pinning::fingerprint_of_der(first_leaf.cert.der().as_ref()),
        );
        assert_eq!(
            block.access_cert_fingerprint.as_deref(),
            Some(&*expected_fp)
        );
        assert!(crate::access::identity_attestation::verify_attestation(
            &block,
            &identity.public_key_b64u()
        )
        .is_ok());

        // Unchanged inputs: the cached render (same issued_at).
        let second = live.render_json();
        assert_eq!(*first, *second, "stable inputs serve the cached render");

        // Rotate the access leaf on disk: the next render re-signs over
        // the new fingerprint with a fresh (monotonic) stamp.
        let second_leaf = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(access_dir.path().join("server.crt"), second_leaf.cert.pem()).unwrap();
        let third = live.render_json();
        let rotated: crate::peer::AgentCard = serde_json::from_str(&third).unwrap();
        let rotated_block = rotated.identity_attestation.expect("re-signed");
        assert_eq!(
            rotated_block.access_cert_fingerprint.as_deref(),
            Some(&*crate::access::pinning::format_fingerprint(
                &crate::access::pinning::fingerprint_of_der(second_leaf.cert.der().as_ref())
            )),
            "rotation re-binds the CURRENT leaf"
        );
        assert!(
            rotated_block.issued_at_unix_ms >= block.issued_at_unix_ms,
            "re-signs move forward"
        );
    }

    /// No identity path (the test default): the card serves without an
    /// attestation instead of loading machine-global identity state.
    #[test]
    fn agent_card_live_without_identity_serves_base() {
        let access_dir = tempfile::tempdir().unwrap();
        let live = AgentCardLive::new(
            base_card_value(),
            &WebGatewayConfig::default(),
            access_dir.path().to_path_buf(),
        );
        let json = live.render_json();
        let card: crate::peer::AgentCard = serde_json::from_str(&json).unwrap();
        assert!(card.identity_attestation.is_none());
        assert!(live.identity_public_key().is_none());
    }

    /// The auto-append arming condition is (relay live AND admission
    /// key on); the port comes from the relay endpoint with the wss
    /// default.
    #[test]
    fn relay_candidate_arms_only_with_relay_and_admission() {
        let access_dir = tempfile::tempdir().unwrap();
        let mut config = WebGatewayConfig::default();
        config.connect.relay_enabled = true;
        config.connect.relay_endpoint = Some("relay.example.com:9443".to_string());
        // Admission off: no candidate port.
        let live = AgentCardLive::new(base_card_value(), &config, access_dir.path().to_path_buf());
        assert_eq!(live.relay_candidate_port, None);
        // Both on: armed with the endpoint's port.
        config.connect.relay_peer_admission = true;
        let live = AgentCardLive::new(base_card_value(), &config, access_dir.path().to_path_buf());
        assert_eq!(live.relay_candidate_port, Some(9443));
        // Relay off (admission alone): no candidate.
        config.connect.relay_enabled = false;
        let live = AgentCardLive::new(base_card_value(), &config, access_dir.path().to_path_buf());
        assert_eq!(live.relay_candidate_port, None);
    }

    #[test]
    fn relay_public_port_parses_endpoint_shapes() {
        assert_eq!(relay_public_port(Some("relay.example.com:443")), Some(443));
        assert_eq!(
            relay_public_port(Some("relay.example.com:9443")),
            Some(9443)
        );
        assert_eq!(relay_public_port(Some("relay.example.com")), Some(443));
        assert_eq!(relay_public_port(Some("[fdc2::1]:8443")), Some(8443));
        assert_eq!(relay_public_port(Some("  ")), None);
        assert_eq!(relay_public_port(None), None);
    }

    /// A specific bind address is preserved verbatim in the
    /// advertised URL. The operator chose it; we trust them.
    #[test]
    fn advertise_url_preserves_specific_bind_address() {
        use std::net::{Ipv4Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[], false),
            vec!["ws://127.0.0.1:8765/ws".to_string()]
        );
        let lan_ip = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(lan_ip), &[], false),
            vec!["ws://192.168.1.42:8765/ws".to_string()]
        );
    }

    /// With TLS enabled the auto-detected scheme is `wss://`, not `ws://`
    /// — a TLS daemon is HTTPS/WSS-only, so advertising `ws://` would hand
    /// peers a URL they'd be refused on. Operator overrides are still
    /// taken verbatim (they own their scheme).
    #[test]
    fn advertise_url_uses_wss_when_tls_enabled() {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[], true),
            vec!["wss://192.168.1.42:8765/ws".to_string()]
        );
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(v6), &[], true);
        assert_eq!(urls, vec!["wss://[::1]:8765/ws".to_string()]);
        // Wildcard bind with TLS: every auto-detected URL is wss://.
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        for url in resolve_advertise_urls(Some(wildcard), &[], true) {
            assert!(url.starts_with("wss://"), "tls scheme on every URL: {url}");
        }
        // Operator override is verbatim — its scheme is not rewritten.
        let overrides = vec!["ws://operator.example:9000/ws".to_string()];
        let urls = resolve_advertise_urls(Some(specific), &overrides, true);
        assert_eq!(urls[0], "ws://operator.example:9000/ws");
    }

    /// Wildcard bind (0.0.0.0) gets replaced with one URL per routable
    /// interface (auto-detection), never the literal wildcard. This
    /// is the guard against the production case where main.rs binds
    /// to 0.0.0.0:8765 and an earlier implementation was handing out
    /// `ws://0.0.0.0:8765/ws` in the Agent Card — an unusable URL
    /// that the transport-url-is-the-listener-addr assumption let
    /// slip through localhost-only tests.
    ///
    /// The exact set of interfaces is environment-dependent so we
    /// can't pin specific addresses; we only assert that no entry is
    /// the wildcard literal and the port is preserved everywhere.
    #[test]
    fn advertise_url_replaces_ipv4_wildcard_with_interface_urls() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "auto-detect should produce at least one URL"
        );
        for url in &urls {
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.starts_with("ws://"), "scheme preserved: {url}");
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
            let host = url
                .strip_prefix("ws://")
                .and_then(|rest| rest.strip_suffix(":8765/ws"))
                .expect("url has expected prefix/suffix");
            assert!(
                !host.is_empty(),
                "host must resolve to something non-empty: {url}"
            );
        }
    }

    /// Same guard for IPv6 wildcards (::), which have the same
    /// unreachability problem as 0.0.0.0. Auto-detected v6 entries
    /// are bracketed per RFC 3986; we don't pin which interfaces are
    /// found because that's environment-dependent.
    #[test]
    fn advertise_url_replaces_ipv6_wildcard_with_interface_urls() {
        use std::net::{Ipv6Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "wildcard v6 bind should still produce some auto-detected URLs"
        );
        for url in &urls {
            assert!(
                !url.contains("[::]"),
                "ipv6 wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// IPv6 specific addresses are bracketed in the URL per RFC 3986
    /// so a literal address like `::1` doesn't collide with the
    /// `:port` separator.
    #[test]
    fn advertise_url_brackets_specific_ipv6_address() {
        use std::net::{Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(specific), &[], false);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].contains("[::1]"),
            "ipv6 literal must be bracketed: {}",
            urls[0]
        );
    }

    /// Operator overrides come first in the merged list (preference
    /// order), but auto-detected entries are appended as fallbacks.
    /// The connecting peer's `MultiTransport::connect` walks the list
    /// top-down and uses the first that succeeds, so overrides win on
    /// preference while auto entries provide redundancy.
    #[test]
    fn advertise_overrides_prepend_to_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        // Specific bind so we can assert exactly one auto-detected entry
        // (wildcard bind would enumerate every host interface — non-
        // deterministic in CI). Specific-bind also covers the
        // intentionally-narrowed-listener case.
        let bind = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        let overrides = vec![
            "ws://192.168.1.42:8765/ws".to_string(),
            "wss://laptop.tail-abcd.ts.net:8443/ws".to_string(),
        ];
        let urls = resolve_advertise_urls(Some(bind), &overrides, false);
        // Overrides come first, auto-detected entry appended.
        assert_eq!(urls.len(), 3, "got: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
        assert_eq!(urls[1], "wss://laptop.tail-abcd.ts.net:8443/ws");
        assert_eq!(urls[2], "ws://127.0.0.1:8765/ws");
    }

    /// An empty overrides list relies entirely on auto-detection.
    /// With a specific bind the result is exactly that one URL.
    #[test]
    fn empty_overrides_use_only_auto_detected_url() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan), &[], false);
        assert_eq!(urls, vec!["ws://192.168.1.42:8765/ws".to_string()]);
    }

    /// Dedup: an operator URL that happens to match an auto-detected
    /// entry is kept exactly once (in operator position, since
    /// overrides are processed first). Avoids advertising the same
    /// URL twice when the operator types out their LAN IP that the
    /// daemon would have auto-detected anyway.
    #[test]
    fn advertise_dedupes_overrides_matching_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let overrides = vec!["ws://192.168.1.42:8765/ws".to_string()];
        let urls = resolve_advertise_urls(Some(lan), &overrides, false);
        assert_eq!(urls.len(), 1, "duplicate suppressed: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }

    /// A wildcard bind enumerates every routable non-loopback
    /// interface. We can't pin exact addresses (CI hosts vary) but
    /// can assert: (a) at least one URL is produced, (b) loopback is
    /// excluded (advertising loopback to remote peers is useless),
    /// (c) the port matches the bind port.
    #[test]
    fn advertise_wildcard_bind_enumerates_interfaces_excluding_loopback() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "expected at least one auto-detected URL, got: {urls:?}"
        );
        for url in &urls {
            assert!(
                !url.contains("127.0.0.1"),
                "loopback must not appear in auto-detected federation URLs: {url}"
            );
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in auto-detected URLs: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// When operator wants to override completely (e.g. for security
    /// reasons — only advertise the Tailscale URL even though the
    /// daemon binds wildcard), they bind to a specific interface
    /// instead of wildcard. Specific bind narrows auto-detection to
    /// just that interface, so combined with operator override the
    /// effective list is `[override..., that_one_interface]`.
    #[test]
    fn specific_bind_narrows_auto_detection_to_one_interface() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan_only = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan_only), &[], false);
        assert_eq!(urls.len(), 1, "specific bind = exactly one auto entry");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }

    #[test]
    fn test_build_config_gemini_model() {
        let config = build_config(
            None,
            Some("gemini-2.5-flash-native-audio-preview-12-2025"),
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(
            None,
            Some("gpt-4o-realtime-preview"),
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(
            Some("openai"),
            None,
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(
            None,
            None,
            false,
            crate::display::IceConfig::default(),
            false,
        );
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }

    /// With one connected stock peer (which advertises ComputerUse and
    /// Display), `?capability=computer-use` returns the peer while an
    /// unadvertised capability returns an empty list.
    #[tokio::test]
    async fn test_api_peers_eligible_returns_matching_peers() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Hits: the test peer's card advertises ComputerUse.
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=computer-use HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peers = parsed["peers"].as_array().expect("peers array");
        assert_eq!(peers.len(), 1, "expected one matching peer");
        assert_eq!(peers[0]["id"].as_str().unwrap(), peer_id);

        // Misses: the fixture doesn't advertise Voice. The local card likewise
        // advertises only ComputerUse + Display.
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=voice HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["peers"].as_array().unwrap().len(), 0);

        target_handle.abort();
        dash_handle.abort();
    }

    /// Routing a capability no connected peer satisfies returns 404
    /// with the considered peer ids surfaced for diagnostics.
    #[tokio::test]
    async fn test_api_coordinator_route_no_match_returns_404() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Voice is the "gated, not-advertised-by-default" capability
        // that the stock build_local_agent_card fixture doesn't claim
        // — so routing by it hits no-route and surfaces the considered
        // list. Display moved to always-on in the 3a.1 fix, so it can
        // no longer serve as the deliberately-unsatisfied capability.
        let body = serde_json::json!({
            "required_capabilities": ["voice"],
            "task": {"instructions": "needs voice"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), "no route");
        let considered = parsed["considered"].as_array().expect("considered array");
        assert!(
            considered.iter().any(|v| v.as_str() == Some(&peer_id)),
            "considered list should include the peer that didn't match"
        );

        target_handle.abort();
        dash_handle.abort();
    }
}
