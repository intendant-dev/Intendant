//! Peer/federation HTTP request glue: add/remove-peer payloads + manual
//! peer persistence, identity summary, send-message/delegate/resolve
//! payloads, federated answer rewriting, and capability query parsing.

use super::*;

#[derive(Deserialize)]
pub(crate) struct AddPeerRequest {
    pub(crate) card_url: String,
    /// Optional display label override for this peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    /// Persist this manual add into `intendant.toml` after the live
    /// registration succeeds. Unchecked manual adds remain runtime-only.
    #[serde(default)]
    pub(crate) persist: bool,
    /// Optional connecting-side override for the peer's transport
    /// URLs. When non-empty, the card's `transports` field is
    /// replaced with one `IntendantWs` entry per URL. Lets the
    /// operator route around topologies the advertising peer's card
    /// doesn't know about (port-forwards, proxies, named tunnels).
    /// `#[serde(default)]` so older clients without this field
    /// continue to work.
    #[serde(default)]
    pub(crate) via_urls: Vec<String>,
    /// Optional outbound bearer token sent to this peer (the
    /// `[[peer]] bearer_token` equivalent for dashboard-added
    /// peers). When set, sent on the agent-card fetch and the
    /// WebSocket upgrade. Required when the peer's card declares
    /// `auth.application = Some(Bearer)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bearer_token: Option<String>,
    /// Optional operator-supplied pinned cert fingerprints. When
    /// non-empty, REPLACES whatever the peer's card declares for
    /// `auth.transport` — eliminates the TOFU window when the
    /// operator got the fingerprint out-of-band. Same wire format
    /// as the card's: lowercase hex with optional `:` separators.
    #[serde(default)]
    pub(crate) pinned_fingerprints: Vec<String>,
    /// Explicit URL the **browser** uses to reach this peer's HTTP
    /// port for WebRTC ICE-TCP. When set, the dashboard uses this
    /// (not `d.ws_url`) as the `advertise_tcp_via_url` hint in the
    /// federated WebRTC offer. Decouples the browser-side URL from
    /// the via URL the primary uses for federation, which matters
    /// when the two network positions differ (primary-side localhost
    /// tunnel, browser on a different machine, etc.). `None` falls
    /// back to the slice 3a.2 behavior of using the primary's via URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) browser_tcp_via_url: Option<String>,
}


#[derive(Deserialize)]
pub(crate) struct RemovePeerRequest {
    pub(crate) peer_id: String,
}


pub(crate) fn trimmed_nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn persist_manual_peer(
    project_root: &Path,
    req: &AddPeerRequest,
    label: Option<String>,
) -> Result<PathBuf, crate::error::CallerError> {
    let mut project = crate::project::Project::from_root(project_root.to_path_buf())?;
    let existing = project
        .config
        .peers
        .iter_mut()
        .find(|peer| peer.card_url == req.card_url);
    match existing {
        Some(peer) => {
            if label.is_some() {
                peer.label = label;
            }
            peer.bearer_token = req.bearer_token.clone();
            peer.via_urls = req.via_urls.clone();
            peer.pinned_fingerprints = req.pinned_fingerprints.clone();
            peer.browser_tcp_via_url = req.browser_tcp_via_url.clone();
        }
        None => {
            project.config.peers.push(crate::project::PeerConfig {
                card_url: req.card_url.clone(),
                label,
                bearer_token: req.bearer_token.clone(),
                via_urls: req.via_urls.clone(),
                client_cert: None,
                client_key: None,
                pinned_fingerprints: req.pinned_fingerprints.clone(),
                browser_tcp_via_url: req.browser_tcp_via_url.clone(),
            });
        }
    }
    project.save_config()?;
    Ok(project.root.join("intendant.toml"))
}


pub(crate) fn target_card_url_from_request(header_text: &str, is_tls: bool) -> Option<String> {
    let host = header_text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        })
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())?;
    let scheme = if is_tls { "https" } else { "http" };
    Some(format!(
        "{scheme}://{}{}",
        host.trim_end_matches('/'),
        crate::peer::pairing::AGENT_CARD_PATH
    ))
}


pub(crate) fn identity_summary_json(
    record: crate::peer::access_policy::PeerIdentityRecord,
) -> serde_json::Value {
    // `active` mirrors the gateway auth gate (approved AND unexpired), so
    // an org-materialized identity past its expiry reads as inert here —
    // the raw status/expiry/provenance fields let the UI say why.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    serde_json::json!({
        "fingerprint": record.fingerprint,
        "label": record.label,
        "profile": record.profile,
        "status": record.status,
        "active": record.is_active(now_unix),
        "card_url": record.card_url,
        "request_id": record.request_id,
        "created_at_unix": record.created_at_unix,
        "revoked_at_unix": record.revoked_at_unix,
        "expires_at_unix": record.expires_at_unix,
        "source": record.source,
        "org_grant_id": record.org_grant_id,
        "issued_via": record.issued_via,
    })
}


// ---------------------------------------------------------------------------
// Per-peer outbound op handlers
// ---------------------------------------------------------------------------
//
// These three endpoints let the dashboard drive the read-write peer
// transport directly. Each maps a JSON body to the matching
// [`crate::peer::PeerHandle`] method:
//
//   POST /api/peers/{id}/message  →  PeerHandle::send_message
//   POST /api/peers/{id}/task     →  PeerHandle::delegate_task
//   POST /api/peers/{id}/approval →  PeerHandle::resolve_approval
//
// Error model (uniform across the three):
//
//   400  bad JSON / missing required field
//   404  peer not in registry
//   405  peer's transport doesn't support this op (UnsupportedCapability)
//   502  transport-level failure (NotConnected, Transport, Auth, …)
//   500  catch-all for unexpected errors
//
// Status codes pick a meaningful HTTP semantic per [`PeerError`] variant
// rather than collapsing everything to 502 — the dashboard renders a
// different message for "wrong peer kind" vs "peer is offline".

/// Shared body for `POST /api/peers/{id}/message`.
///
/// Two equivalent shapes accepted:
///
/// 1. Shorthand: `{"text": "hello"}` — implicit user role + Text content.
/// 2. Full:     `{"role": "user", "content": {"type": "text", "text": "hello"}, "session": null}`.
///
/// The `content` field, when present, wins over `text`. Either `text`
/// or `content` is required; everything else is optional.
#[derive(Deserialize)]
pub(crate) struct SendMessageRequest {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    role: Option<crate::peer::MessageRole>,
    #[serde(default)]
    content: Option<crate::peer::MessageContent>,
    #[serde(default)]
    session: Option<String>,
}

impl SendMessageRequest {
    pub(crate) fn into_message(self) -> Result<crate::peer::PeerMessage, String> {
        let role = self.role.unwrap_or(crate::peer::MessageRole::User);
        let content = match (self.content, self.text) {
            (Some(c), _) => c,
            (None, Some(t)) => crate::peer::MessageContent::Text { text: t },
            (None, None) => {
                return Err("either 'text' or 'content' is required".to_string());
            }
        };
        Ok(crate::peer::PeerMessage {
            session: self.session,
            role,
            content,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct DelegateTaskRequest {
    pub(crate) instructions: String,
    #[serde(default)]
    pub(crate) context: serde_json::Value,
    #[serde(default)]
    pub(crate) client_correlation_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ResolveApprovalRequest {
    pub(crate) request_id: String,
    pub(crate) decision: crate::peer::ApprovalDecision,
}


/// Slice 3b: rewrite an outgoing federated `WebRtcSignal::Answer` to
/// (a) register the peer's ICE ufrag in the relay registry and
/// (b) inject a TCP candidate pointing at the primary's own address
/// alongside the peer's direct candidate.
///
/// After the rewrite, a browser receiving this Answer has two TCP
/// candidates: the peer's direct TCP candidate (if the peer provided
/// one via `advertise_tcp_via_url`) and the primary's relay
/// candidate. Browser ICE tries both and uses whichever forms first.
/// Because the relay candidate is emitted with a lower priority
/// (see `inject_relay_tcp_candidate`), direct wins on reachable
/// topologies and relay is the fallback.
///
/// Non-Answer events pass through verbatim. Events with malformed
/// SDPs, missing ufrags, or a peer URL that can't be resolved fall
/// through without rewriting — the browser still sees the original
/// Answer, just without the relay candidate.
pub(crate) async fn maybe_rewrite_federated_answer(
    peer: &crate::peer::PeerId,
    event: crate::peer::PeerEvent,
    registry: &crate::peer::PeerRegistry,
    relay_registry: &Arc<crate::display::webrtc::TcpRelayRegistry>,
    relay_advertise_url: Option<&str>,
    bus: &EventBus,
) -> crate::peer::PeerEvent {
    const LOG_SOURCE: &str = "webrtc-peer";

    // Match only the specific variant that carries an Answer SDP; all
    // other event variants (Log, Usage, ActivityStarted, IceCandidate,
    // etc.) pass through unchanged.
    let (display_id, session_id, sdp) = match &event {
        crate::peer::PeerEvent::WebRtcSignal {
            display_id,
            session_id,
            signal: crate::peer::WebRtcSignal::Answer { sdp, .. },
        } => (*display_id, session_id.clone(), sdp.clone()),
        _ => return event,
    };

    // Extract the peer's ICE ufrag from the Answer SDP. Without it we
    // can't key the relay registry, so we skip rewriting and let the
    // browser try whatever direct candidate the peer advertised.
    let ufrag = match crate::display::webrtc::parse_sdp_ice_ufrag(&sdp) {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     Answer SDP missing a=ice-ufrag attribute"
                ),
                turn: None,
            });
            return event;
        }
    };

    // Resolve the peer's outbound TCP address — where the primary
    // will dial when it sees a relay-destined TCP connection. Prefer
    // `browser_tcp_via_url` (operator's split-browser-side URL) then
    // fall back to `ws_url` (primary-side via URL). In the typical
    // co-located case the two are the same; in split topologies the
    // operator uses browser_tcp_via_url to point at where they'd
    // like the BROWSER to reach the peer. Here we're dialing FROM
    // the primary, but the primary typically shares the LAN position
    // of the operator's browser-reachable URL when one is set.
    let outbound_url = registry.get(peer).and_then(|h| {
        let snap = h.snapshot();
        snap.browser_tcp_via_url.or(snap.ws_url)
    });
    let outbound_url = match outbound_url {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     no outbound URL on the peer's snapshot (peer removed mid-Answer?)"
                ),
                turn: None,
            });
            return event;
        }
    };
    let outbound_addr = match resolve_url_to_socket_addr(&outbound_url).await {
        Some(addr) => addr,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     outbound URL {outbound_url:?} didn't resolve to a SocketAddr"
                ),
                turn: None,
            });
            return event;
        }
    };
    relay_registry.register(ufrag.clone(), outbound_addr);

    // Resolve the primary's own relay URL into a SocketAddr we can
    // put in an SDP candidate line. When the primary has no
    // advertised URL we can work with (local_addr() was None at
    // spawn, headless mode, etc), skip injection and just forward
    // the Answer unchanged — the browser still has the peer's
    // direct candidate to try.
    let primary_relay_addr = match relay_advertise_url {
        Some(url) => match resolve_url_to_socket_addr(url).await {
            Some(addr) => addr,
            None => {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                         but can't inject relay candidate — primary's own URL {url:?} doesn't resolve"
                    ),
                    turn: None,
                });
                return event;
            }
        },
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                     but no primary relay URL configured — skipping candidate injection"
                ),
                turn: None,
            });
            return event;
        }
    };

    let rewritten_sdp =
        crate::display::webrtc::inject_relay_tcp_candidate(&sdp, primary_relay_addr);
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "info".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "relay registered ufrag={ufrag} peer={peer} session={session_id} \
             primary_relay={primary_relay_addr} outbound={outbound_addr}"
        ),
        turn: None,
    });

    crate::peer::PeerEvent::WebRtcSignal {
        display_id,
        session_id,
        signal: crate::peer::WebRtcSignal::Answer {
            sdp: rewritten_sdp,
            binding: None,
        },
    }
}

/// Parse a WebSocket / HTTP URL and resolve it to a [`SocketAddr`].
///
/// Used to convert the browser's view of a peer's HTTP port (the
/// `advertise_tcp_via_url` hint in a federated
/// [`crate::peer::WebRtcSignal::Offer`]) into the concrete address
/// the peer advertises in its ICE-TCP candidate.
///
/// Accepts `ws://` / `wss://` / `http://` / `https://` schemes (all
/// produce the same authority shape). The host can be an IPv4
/// literal, a bracketed IPv6 literal, or a hostname — hostnames are
/// resolved via [`tokio::net::lookup_host`] and the first returned
/// address is used. The port must be explicit; there's no default-
/// port fallback, because we can't know what the peer's HTTP
/// listener bound to without being told.
///
/// Returns `None` on any parse or resolution failure. Callers treat
/// that as "no TCP candidate, UDP-only path" — the same behavior as
/// slice 3a's pre-3a.2 baseline.
pub(crate) async fn resolve_url_to_socket_addr(url: &str) -> Option<std::net::SocketAddr> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))?;
    // Strip any path / query that follows the authority. Authority
    // for an IPv6 literal is `[::1]:8766`, which contains neither
    // `/` nor `?` inside the brackets, so split-on-first is safe.
    let authority = rest.split(['/', '?']).next()?;
    // Fast path for `ipv4:port` or `[ipv6]:port`: parse directly.
    if let Ok(addr) = authority.parse::<std::net::SocketAddr>() {
        return Some(addr);
    }
    // Hostname:port — needs DNS. `lookup_host` accepts `host:port`
    // and returns the resolved SocketAddrs in OS-chosen order; first
    // is the winner (matches what the kernel would pick for a
    // regular connect()).
    tokio::net::lookup_host(authority).await.ok()?.next()
}


// ---------------------------------------------------------------------------
// Coordinator endpoints — capability-based discovery + delegation
// ---------------------------------------------------------------------------

/// Parse `?capability=display&capability=custom:foo` into a typed
/// `Vec<Capability>` plus a list of unknown strings (for diagnostics).
/// Empty input returns `(vec![], vec![])` — empty-required-capabilities
/// matches every peer, which the handler rejects upstream.
pub(crate) fn parse_capability_query(query: &str) -> (Vec<crate::peer::Capability>, Vec<String>) {
    let mut caps = Vec::new();
    let mut unknown = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k != "capability" {
            continue;
        }
        match crate::peer::Capability::from_query_string(v) {
            Some(cap) => caps.push(cap),
            None => unknown.push(v.to_string()),
        }
    }
    (caps, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_gateway::tests::{http_request, setup_peer_op_test};

    // -----------------------------------------------------------------
    // resolve_url_to_socket_addr (slice 3a.2 — URL hint parsing)
    // -----------------------------------------------------------------

    /// Directly-parseable `ipv4:port` authorities are returned
    /// without any DNS round-trip.
    #[tokio::test]
    async fn resolve_url_parses_ipv4_literal_url() {
        let addr = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws")
            .await
            .expect("parses");
        assert_eq!(addr.to_string(), "127.0.0.1:8766");
    }

    /// Bracketed IPv6 literals round-trip through the parser; the
    /// `/ws` path suffix is stripped before the SocketAddr parse.
    #[tokio::test]
    async fn resolve_url_parses_ipv6_literal_url() {
        let addr = resolve_url_to_socket_addr("wss://[::1]:8443/ws")
            .await
            .expect("parses");
        assert_eq!(addr.port(), 8443);
        assert!(addr.is_ipv6(), "expected IPv6, got {addr}");
    }

    /// `http://` and `https://` are accepted alongside the WebSocket
    /// schemes so the same URL form works whether the operator types
    /// the dashboard URL or the /ws URL.
    #[tokio::test]
    async fn resolve_url_accepts_http_and_https_schemes() {
        let a = resolve_url_to_socket_addr("http://127.0.0.1:8000/")
            .await
            .expect("http parses");
        assert_eq!(a.port(), 8000);
        let b = resolve_url_to_socket_addr("https://127.0.0.1:8443")
            .await
            .expect("https parses");
        assert_eq!(b.port(), 8443);
    }

    /// Hostnames route through `tokio::net::lookup_host`. `localhost`
    /// is the one name we can rely on across every test environment.
    #[tokio::test]
    async fn resolve_url_resolves_localhost_via_dns() {
        let addr = resolve_url_to_socket_addr("ws://localhost:8766/ws")
            .await
            .expect("resolves");
        assert_eq!(addr.port(), 8766);
        assert!(
            addr.ip().is_loopback(),
            "localhost must resolve to a loopback address: {addr}"
        );
    }

    /// URLs with a path + query string strip cleanly: the authority
    /// is everything up to the first `/` or `?`.
    #[tokio::test]
    async fn resolve_url_strips_path_and_query() {
        let a = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws/path?foo=bar")
            .await
            .expect("parses");
        assert_eq!(a.to_string(), "127.0.0.1:8766");
    }

    /// Unknown schemes, missing ports, and unresolvable hostnames
    /// all return `None` — caller falls back to UDP-only path.
    #[tokio::test]
    async fn resolve_url_returns_none_on_malformed_inputs() {
        // Unknown scheme
        assert!(resolve_url_to_socket_addr("foo://127.0.0.1:8766")
            .await
            .is_none());
        // Empty authority
        assert!(resolve_url_to_socket_addr("ws:///path").await.is_none());
        // No port (authority parses as IP but not SocketAddr; lookup_host
        // rejects a bare host with no port).
        assert!(resolve_url_to_socket_addr("ws://127.0.0.1/ws")
            .await
            .is_none());
    }

    #[test]
    fn test_persist_manual_peer_writes_outbound_peer_config() {
        let root = tempfile::TempDir::new().unwrap();
        let req = AddPeerRequest {
            card_url: "https://peer.example:8765/.well-known/agent-card.json".into(),
            label: Some("Ignored Raw Label".into()),
            persist: true,
            via_urls: vec!["wss://tailnet-peer.example:8765/ws".into()],
            bearer_token: Some("legacy-token".into()),
            pinned_fingerprints: vec![
                "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".into(),
            ],
            browser_tcp_via_url: Some("wss://browser-peer.example:8765/ws".into()),
        };

        let path = persist_manual_peer(root.path(), &req, Some("Peer Display".into())).unwrap();

        assert_eq!(path, root.path().join("intendant.toml"));
        let project = crate::project::Project::from_root(root.path().to_path_buf()).unwrap();
        assert_eq!(project.config.peers.len(), 1);
        let peer = &project.config.peers[0];
        assert_eq!(peer.card_url, req.card_url);
        assert_eq!(peer.label.as_deref(), Some("Peer Display"));
        assert_eq!(peer.via_urls, req.via_urls);
        assert_eq!(peer.bearer_token, req.bearer_token);
        assert_eq!(peer.pinned_fingerprints, req.pinned_fingerprints);
        assert_eq!(peer.browser_tcp_via_url, req.browser_tcp_via_url);
        assert!(peer.client_cert.is_none());
        assert!(peer.client_key.is_none());
    }

    /// `POST /api/peers/{id}/message` with a full `{role, content,
    /// session}` body works the same. Verifies the full-control shape
    /// path through `SendMessageRequest::into_message` (where `content`
    /// wins over `text` when both are present).
    #[tokio::test]
    async fn test_api_peers_send_message_full_shape_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "role": "user",
            "content": {"type": "text", "text": "hello"},
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }
}
