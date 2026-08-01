//! Read-only dashboard-API fetches on a peer's gateway over the
//! federation HTTP lane: one authenticated GET against the same origin
//! that serves the peer's `/ws`, using the exact mTLS identity, pins,
//! and bearer the federation transport uses (the [`super::mcp_http`]
//! pattern — request/response, so it works over every link class the
//! control WS works over, including a reachability relay; no WebRTC
//! datachannel is involved).
//!
//! Authority stays entirely peer-side: the caller presents its daemon
//! identity and the *peer's* route IAM evaluates the profile it granted
//! that identity for the route's operation class (`GET /api/session/{id}`
//! is `SessionInspect`). A peer-side denial comes back as the peer's own
//! HTTP status + JSON error body, passed through verbatim so the
//! dashboard can surface the governing daemon's honest refusal.

use super::handle::PeerHandle;
use super::transport::intendant::{PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE};
use super::transport::ws_url_to_http_base;
use std::time::Duration;

/// Ceiling for one transcript-page round trip. Matches the dashboard's
/// peer-lane request budget; connection-level failures surface earlier.
const PEER_API_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on a fetched response body. A transcript page is bounded by
/// the peer's own `SESSION_DETAIL_ENTRY_LIMIT_MAX` page cap, but the
/// fetch must not trust the remote end to be well-behaved with this
/// daemon's memory.
const PEER_API_MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// One passed-through peer HTTP reply: the peer's own status and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerApiReply {
    pub status: u16,
    pub body: String,
}

/// Derive the peer's gateway HTTP origin from its Agent Card: the
/// gateway that serves `/ws` serves the dashboard API on the same
/// origin. Operator `via_urls` overrides are already folded into the
/// card snapshot by the actor.
fn gateway_http_base(card: &super::card::AgentCard) -> Option<String> {
    for spec in &card.transports {
        if let super::card::TransportSpec::IntendantWs { url } = spec {
            return Some(ws_url_to_http_base(url));
        }
    }
    None
}

/// True when every byte is safe to splice into a URL path segment or
/// query value without encoding. Deliberately conservative — session
/// ids and backend source ids live well inside this set, and the peer's
/// own handler rejects unsafe ids anyway; refusing locally gives the
/// caller an honest error instead of a mangled URL.
fn url_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Fetch one page of a peer session's transcript: authenticated
/// `GET {gateway}/api/session/{id}?source=&limit=&before=` with the
/// federation credentials. Returns the peer's status + body verbatim
/// (including peer-side 403/404 refusals); `Err` is reserved for
/// transport-level failures and local refusals.
pub async fn fetch_peer_session_detail(
    handle: &PeerHandle,
    session_id: &str,
    source: Option<&str>,
    limit: Option<u64>,
    before: Option<u64>,
) -> Result<PeerApiReply, String> {
    if !url_safe(session_id) {
        return Err(format!(
            "session id {session_id:?} contains characters outside the safe URL set"
        ));
    }
    if let Some(source) = source {
        if !url_safe(source) {
            return Err(format!(
                "source {source:?} contains characters outside the safe URL set"
            ));
        }
    }
    let card = handle.card_snapshot();
    let base = gateway_http_base(&card).ok_or_else(|| {
        format!(
            "peer {} advertises no transport a gateway HTTP origin can be derived from",
            handle.id().0
        )
    })?;
    let mut url = format!("{base}/api/session/{session_id}");
    let mut sep = '?';
    if let Some(source) = source {
        url.push(sep);
        url.push_str(&format!("source={source}"));
        sep = '&';
    }
    if let Some(limit) = limit {
        url.push(sep);
        url.push_str(&format!("limit={limit}"));
        sep = '&';
    }
    if let Some(before) = before {
        url.push(sep);
        url.push_str(&format!("before={before}"));
    }
    let creds = handle.transport_credentials();
    let client = creds
        .tls
        .http_client(&creds.pinned_fingerprints, creds.client_identity.as_ref())
        .map_err(|e| format!("build peer http client: {e}"))?;
    // The peer-client marker opts into fail-closed handling on the
    // gateway: an unresolvable client cert is a 403, never a silent
    // downgrade to the anonymous path.
    let mut request = client
        .get(&url)
        .timeout(PEER_API_TIMEOUT)
        .header(PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE);
    if let Some(token) = &creds.bearer_token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = response.status().as_u16();
    let body = read_body_bounded(response, PEER_API_MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| format!("read {url} response: {e}"))?;
    Ok(PeerApiReply { status, body })
}

/// Read a response body with a hard byte ceiling — a misbehaving peer
/// must not be able to balloon this daemon's memory through one fetch.
async fn read_body_bounded(response: reqwest::Response, max: usize) -> Result<String, String> {
    use futures_util::StreamExt as _;
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > max {
            return Err(format!("response exceeds the {max}-byte ceiling"));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|e| format!("response is not UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
    use crate::peer::id::{PeerId, PeerKind};

    fn card_with(transports: Vec<TransportSpec>) -> AgentCard {
        AgentCard {
            id: PeerId::new(PeerKind::Intendant, "test-peer"),
            label: "test-peer".to_string(),
            version: "test".into(),
            git_sha: None,
            transports,
            capabilities: Vec::new(),
            auth: AuthRequirements::none(),
        }
    }

    #[test]
    fn gateway_base_derives_from_ws_transport() {
        let card = card_with(vec![TransportSpec::IntendantWs {
            url: "wss://peer.example:8443/ws".into(),
        }]);
        assert_eq!(
            gateway_http_base(&card).as_deref(),
            Some("https://peer.example:8443")
        );
        assert_eq!(gateway_http_base(&card_with(Vec::new())), None);
    }

    #[test]
    fn url_safe_rejects_separator_and_control_bytes() {
        assert!(url_safe("sess-01HZX.y_z:2"));
        for bad in ["", "a/b", "a?b", "a#b", "a b", "a&b", "a%2Fb", "å"] {
            assert!(!url_safe(bad), "{bad:?} must be refused");
        }
    }

    /// End-to-end over a real socket: the GET hits the path + query
    /// derived from the arguments, carries the peer-client marker, and
    /// the peer's status/body pass through verbatim — including a
    /// peer-side refusal status. A hand-rolled one-shot HTTP responder
    /// keeps this out of the heavy gateway-rig test family (the
    /// `mcp_http` pattern).
    #[test]
    fn fetch_passes_through_peer_status_and_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let server = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let request = loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    assert!(n > 0, "client closed before sending a full request");
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if text.contains("\r\n\r\n") {
                        break text;
                    }
                };
                let body = r#"{"error":"peer identity profile denies session.inspect"}"#;
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(response.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
                request
            });

            let (log_tx, _log_rx) = tokio::sync::mpsc::channel(crate::peer::LOG_CHANNEL_CAPACITY);
            let card = card_with(vec![TransportSpec::IntendantWs {
                url: format!("ws://{addr}/ws"),
            }]);
            let url_for_closure = format!("ws://{addr}/ws");
            let handle = crate::peer::handle::spawn_peer(
                card.id.clone(),
                card,
                Vec::new(),
                None,
                None,
                crate::peer::PeerWitnessVantage::Unknown,
                crate::peer::transport::intendant::TransportCredentials::default(),
                log_tx,
                move |events_tx| {
                    Box::new(crate::peer::transport::intendant::IntendantWsTransport::new(
                        url_for_closure,
                        events_tx,
                    ))
                },
            );

            let reply = fetch_peer_session_detail(
                &handle,
                "sess-abc123",
                Some("intendant"),
                Some(250),
                Some(500),
            )
            .await
            .expect("transport-level success");
            assert_eq!(reply.status, 403, "peer status passes through");
            assert!(reply.body.contains("denies session.inspect"));

            let request = server.await.unwrap();
            let request_line = request.lines().next().unwrap_or_default();
            assert_eq!(
                request_line,
                "GET /api/session/sess-abc123?source=intendant&limit=250&before=500 HTTP/1.1"
            );
            assert!(
                request.to_ascii_lowercase().contains("x-intendant-peer: 1"),
                "peer-client marker present: {request}"
            );
        });
    }

    /// Unsafe ids are refused locally with an honest error instead of
    /// being spliced into a URL.
    #[test]
    fn unsafe_ids_are_refused_locally() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (log_tx, _log_rx) = tokio::sync::mpsc::channel(crate::peer::LOG_CHANNEL_CAPACITY);
            let card = card_with(vec![TransportSpec::IntendantWs {
                url: "ws://127.0.0.1:1/ws".into(),
            }]);
            let handle = crate::peer::handle::spawn_peer(
                card.id.clone(),
                card,
                Vec::new(),
                None,
                None,
                crate::peer::PeerWitnessVantage::Unknown,
                crate::peer::transport::intendant::TransportCredentials::default(),
                log_tx,
                move |events_tx| {
                    Box::new(
                        crate::peer::transport::intendant::IntendantWsTransport::new(
                            "ws://127.0.0.1:1/ws".to_string(),
                            events_tx,
                        ),
                    )
                },
            );
            let err = fetch_peer_session_detail(&handle, "../etc", None, None, None)
                .await
                .expect_err("unsafe id refused");
            assert!(err.contains("safe URL set"), "{err}");
        });
    }
}
