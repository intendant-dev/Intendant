//! Direct MCP tool invocation on a peer's gateway: one stateless
//! JSON-RPC `tools/call` POST to the peer's `/mcp` endpoint,
//! authenticated with the exact mTLS identity, pins, and bearer the
//! federation transport uses (retained on the handle — see
//! [`PeerHandle::transport_credentials`]).
//!
//! This is the trust architecture working as designed: the caller
//! presents its daemon identity, and the *peer's* IAM — its `/mcp`
//! gate binds the client cert to the profile granted to this daemon
//! and evaluates each tool's `PeerOperation` against it — is the only
//! authority over what happens on the peer. Tool-level denials come
//! back as `isError` tool results with diagnostic text, not transport
//! failures.
//!
//! The peer's `/mcp` needs no `initialize` handshake, session, or SSE
//! stream: each POST is a self-contained JSON-RPC exchange, which is
//! why request/response computer use (screenshot in, actions out)
//! works over it without any WebRTC plumbing.

use super::card::{AgentCard, McpTransportKind, TransportSpec};
use super::handle::PeerHandle;
use super::transport::intendant::{PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE};
use super::transport::{tls_client, ws_url_to_http_base};
use std::time::Duration;

/// Ceiling for one peer tool round-trip. Generous because
/// `execute_cu_actions` legitimately carries `wait` actions and
/// post-action screenshot settling; connection-level failures surface
/// long before this fires.
const PEER_MCP_TIMEOUT: Duration = Duration::from_secs(120);

/// One image content part from a peer tool reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerImageContent {
    /// MIME type as advertised by the peer (`image/png` in practice).
    pub media_type: String,
    /// Base64-encoded image bytes, unmodified.
    pub data: String,
}

/// One parsed `tools/call` reply from a peer's `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerToolReply {
    /// The text content parts joined with newlines: metadata JSON,
    /// per-action status lines, or the diagnostic when `is_error`.
    pub text: String,
    /// Image content parts in reply order (screenshots).
    pub images: Vec<PeerImageContent>,
    /// The MCP `isError` flag: the peer handled the call and reports a
    /// tool-level failure — e.g. its IAM profile denied the operation.
    pub is_error: bool,
}

/// Derive the peer's `/mcp` endpoint from its Agent Card.
///
/// Prefers an explicitly advertised streamable-HTTP MCP transport;
/// otherwise derives from the first native WebSocket transport, since
/// the gateway that serves `/ws` serves `/mcp` on the same origin.
/// Cards list transports in preference order, and operator `via_urls`
/// overrides are already folded into the snapshot by the actor.
pub fn mcp_endpoint(card: &AgentCard) -> Option<String> {
    for spec in &card.transports {
        if let TransportSpec::Mcp {
            url,
            transport: McpTransportKind::StreamableHttp,
        } = spec
        {
            return Some(url.clone());
        }
    }
    for spec in &card.transports {
        if let TransportSpec::IntendantWs { url } = spec {
            return Some(format!("{}/mcp", ws_url_to_http_base(url)));
        }
    }
    None
}

/// Invoke one MCP tool on the peer's gateway and parse the reply.
///
/// Transport/protocol problems (unreachable, TLS, bad envelope,
/// JSON-RPC error object) are `Err`; a delivered tool result — even a
/// peer-side denial — is `Ok` so callers can surface the peer's own
/// diagnostic text verbatim.
pub async fn call_peer_mcp_tool(
    handle: &PeerHandle,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<PeerToolReply, String> {
    let card = handle.card_snapshot();
    let endpoint = mcp_endpoint(&card).ok_or_else(|| {
        format!(
            "peer {} advertises no transport a /mcp endpoint can be derived from",
            handle.id().0
        )
    })?;
    let creds = handle.transport_credentials();
    let client = tls_client::reqwest_client(
        PEER_MCP_TIMEOUT,
        &creds.pinned_fingerprints,
        creds.client_identity.as_ref(),
    )
    .map_err(|e| format!("build peer http client: {e}"))?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    // The peer-client marker opts into fail-closed handling on the
    // gateway: an unresolvable client cert is a 403, never a silent
    // downgrade to the anonymous path.
    let mut request = client
        .post(&endpoint)
        .header(PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE)
        .json(&body);
    if let Some(token) = &creds.bearer_token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("POST {endpoint}: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("read {endpoint} response: {e}"))?;
    if !status.is_success() {
        return Err(format!("peer /mcp returned HTTP {status}: {text}"));
    }
    let envelope: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("peer /mcp returned invalid JSON-RPC: {e}"))?;
    parse_tool_reply(&envelope)
}

/// Split a JSON-RPC `tools/call` envelope into text, images, and the
/// tool-level error flag.
fn parse_tool_reply(envelope: &serde_json::Value) -> Result<PeerToolReply, String> {
    if let Some(error) = envelope.get("error") {
        let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("peer /mcp error {code}: {message}"));
    }
    let result = envelope
        .get("result")
        .ok_or_else(|| "peer /mcp reply carries neither result nor error".to_string())?;
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut texts: Vec<&str> = Vec::new();
    let mut images = Vec::new();
    for item in result
        .get("content")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    texts.push(text);
                }
            }
            Some("image") => {
                if let Some(data) = item.get("data").and_then(|v| v.as_str()) {
                    images.push(PeerImageContent {
                        media_type: item
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .unwrap_or("image/png")
                            .to_string(),
                        data: data.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(PeerToolReply {
        text: texts.join("\n"),
        images,
        is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::card::AuthRequirements;
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
    fn endpoint_prefers_advertised_streamable_mcp() {
        let card = card_with(vec![
            TransportSpec::IntendantWs {
                url: "wss://peer.example:8443/ws".into(),
            },
            TransportSpec::Mcp {
                url: "https://peer.example:9000/mcp".into(),
                transport: McpTransportKind::StreamableHttp,
            },
        ]);
        assert_eq!(
            mcp_endpoint(&card).as_deref(),
            Some("https://peer.example:9000/mcp")
        );
    }

    #[test]
    fn endpoint_derives_from_ws_when_no_usable_mcp_transport() {
        // An SSE-only MCP advert is not usable for one-shot POSTs; the
        // WS origin carries /mcp on the same gateway.
        let card = card_with(vec![
            TransportSpec::Mcp {
                url: "https://peer.example/sse".into(),
                transport: McpTransportKind::Sse,
            },
            TransportSpec::IntendantWs {
                url: "wss://peer.example:8443/ws".into(),
            },
        ]);
        assert_eq!(
            mcp_endpoint(&card).as_deref(),
            Some("https://peer.example:8443/mcp")
        );
    }

    #[test]
    fn endpoint_maps_plain_ws_to_http() {
        let card = card_with(vec![TransportSpec::IntendantWs {
            url: "ws://127.0.0.1:8765/ws".into(),
        }]);
        assert_eq!(
            mcp_endpoint(&card).as_deref(),
            Some("http://127.0.0.1:8765/mcp")
        );
    }

    #[test]
    fn endpoint_none_when_card_advertises_nothing_usable() {
        assert_eq!(mcp_endpoint(&card_with(Vec::new())), None);
    }

    #[test]
    fn parse_splits_text_and_image_content() {
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    { "type": "text", "text": "{\"status\":\"ok\",\"width\":1920}" },
                    { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" },
                ],
            },
        });
        let reply = parse_tool_reply(&envelope).expect("parses");
        assert_eq!(reply.text, "{\"status\":\"ok\",\"width\":1920}");
        assert_eq!(
            reply.images,
            vec![PeerImageContent {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }]
        );
        assert!(!reply.is_error);
    }

    #[test]
    fn parse_carries_tool_level_denial_as_ok_with_is_error() {
        // A peer-side IAM denial is a delivered tool result — callers
        // surface its text — not a transport failure.
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "isError": true,
                "content": [
                    { "type": "text", "text": "permission denied: execute_cu_actions requires display.input" },
                ],
            },
        });
        let reply = parse_tool_reply(&envelope).expect("parses");
        assert!(reply.is_error);
        assert!(reply.text.contains("permission denied"));
        assert!(reply.images.is_empty());
    }

    #[test]
    fn parse_maps_json_rpc_error_object_to_err() {
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32601, "message": "method not found" },
        });
        let err = parse_tool_reply(&envelope).expect_err("error object is Err");
        assert!(err.contains("-32601"));
        assert!(err.contains("method not found"));
    }

    #[test]
    fn parse_rejects_envelope_without_result_or_error() {
        let envelope = serde_json::json!({ "jsonrpc": "2.0", "id": 1 });
        assert!(parse_tool_reply(&envelope).is_err());
    }

    /// End-to-end over a real socket: the client POSTs a JSON-RPC
    /// `tools/call` to the endpoint derived from the peer's WS
    /// transport, marks itself as a peer client, and parses the reply.
    /// A hand-rolled one-shot HTTP responder keeps this out of the
    /// heavy gateway-rig test family.
    #[test]
    fn call_peer_mcp_tool_posts_json_rpc_with_peer_marker() {
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
                    if let Some(header_end) = text.find("\r\n\r\n") {
                        let content_length = text[..header_end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .expect("request carries Content-Length");
                        if raw.len() >= header_end + 4 + content_length {
                            break text;
                        }
                    }
                };

                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "content": [
                            { "type": "text", "text": "[]" },
                        ],
                    },
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(response.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
                request
            });

            let (log_tx, _log_rx) =
                tokio::sync::mpsc::channel(crate::peer::LOG_CHANNEL_CAPACITY);
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
                crate::peer::transport::intendant::TransportCredentials::default(),
                log_tx,
                move |events_tx| {
                    Box::new(
                        crate::peer::transport::intendant::IntendantWsTransport::new(
                            url_for_closure,
                            events_tx,
                        ),
                    )
                },
            );

            let reply = call_peer_mcp_tool(&handle, "list_displays", serde_json::json!({}))
                .await
                .expect("roundtrip succeeds");
            assert_eq!(reply.text, "[]");
            assert!(!reply.is_error);

            let request = server.await.unwrap();
            let (headers, request_body) = request.split_once("\r\n\r\n").unwrap();
            assert!(
                headers.starts_with("POST /mcp HTTP/1.1"),
                "endpoint derived from the WS transport: {headers}"
            );
            assert!(
                headers.to_ascii_lowercase().contains("x-intendant-peer: 1"),
                "peer-client marker present: {headers}"
            );
            let sent: serde_json::Value = serde_json::from_str(request_body).unwrap();
            assert_eq!(sent["method"], "tools/call");
            assert_eq!(sent["params"]["name"], "list_displays");
        });
    }
}
