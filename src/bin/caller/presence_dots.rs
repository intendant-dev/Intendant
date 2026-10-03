//! Stage A of dots powering Presence: an explicitly invoked, read-only
//! subscription-authenticated diagnostic. This is NOT a ChatProvider and
//! does not enable a Presence backend. See docs/src/presence-dots.md.
//!
//! The private provider transport is pinned; there is no arbitrary URL,
//! generic RPC, account switch, thread resume, message, voice, or input lane.
//! Raw request/response bodies and credential-bearing errors never escape.

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, http::HeaderMap, protocol::WebSocketConfig, Message,
};

mod http;
mod routing;

const CLOUD_URL: &str = "wss://codex-cloud-backend.chatgpt.com/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const PAGE_LIMIT: usize = 25;
const MAX_IGNORED_MESSAGES: usize = 200;

/// A closed vocabulary, not a public arbitrary-RPC escape hatch.
#[derive(Clone, Copy)]
enum ReadRequest {
    Initialize,
    ListThreads,
    ReadThread,
}

impl ReadRequest {
    fn method(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::ListThreads => "thread/list",
            Self::ReadThread => "thread/read",
        }
    }
}

/// Deliberately excludes account identifiers, thread titles, previews,
/// environment identifiers, filesystem paths, and provider response bodies.
#[derive(Debug, Serialize)]
struct DoctorReport {
    integration_status: &'static str,
    transport: &'static str,
    authenticated_catalog: bool,
    candidate_root_threads_on_page: usize,
    catalog_has_more: bool,
    candidate_metadata_verified: bool,
    presence_backend_enabled: bool,
    message_send_validated: bool,
    dots_voice_validated: bool,
    cloud_desktop_validated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<http::HttpReport>,
}

pub(crate) async fn run(argv: Vec<String>) -> Result<(), String> {
    if argv.is_empty() || argv.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: intendant presence-dots doctor [--json] [--http]\n\n\
             Read-only experimental dots backend diagnostic using the existing\n\
             Codex ChatGPT login. Creates no dot, sends no message, places no\n\
             call, and takes no desktop control. Does not enable Presence.\n\
             --http also checks account routing and the HTTP admission gate."
        );
        return Ok(());
    }
    if argv[0] != "doctor"
        || argv[1..]
            .iter()
            .any(|arg| arg != "--json" && arg != "--http")
    {
        return Err("Usage: intendant presence-dots doctor [--json] [--http]".into());
    }
    let auth = crate::codex_cloud::subscription_cloud_auth()?;
    let headers = auth.websocket_headers()?;
    let mut report = tokio::time::timeout(DIAGNOSTIC_TIMEOUT, diagnose(CLOUD_URL, headers))
        .await
        .map_err(|_| "dots backend diagnostic exceeded its 60-second deadline".to_string())??;
    if argv.iter().any(|arg| arg == "--http") {
        report.http = Some(http::diagnose(&auth).await);
    }
    if argv.iter().any(|arg| arg == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|_| "could not encode dots diagnostic".to_string())?
        );
    } else {
        println!("Dots cloud transport: authenticated catalog reachable (experimental)");
        println!(
            "  candidate root threads on this page: {}{}",
            report.candidate_root_threads_on_page,
            if report.catalog_has_more {
                " (more pages exist)"
            } else {
                ""
            }
        );
        println!(
            "  candidate metadata verified: {}",
            report.candidate_metadata_verified
        );
        println!("  Presence backend: NOT enabled");
        println!("  message admission, dots voice, cloud desktop: NOT validated");
        println!("  root threads are not stable dot identities");
        if let Some(http) = &report.http {
            println!("  HTTP admission: {}", http.state_label());
            println!("  HTTP probe does not validate dots eligibility or a stable dot identity");
        }
    }
    Ok(())
}

// Production only calls the pinned WSS origin above. The endpoint seam is
// private and exists for hermetic loopback transport tests, never configuration.
async fn diagnose(url: &str, headers: HeaderMap) -> Result<DoctorReport, String> {
    let mut request = url
        .into_client_request()
        .map_err(|_| "invalid pinned dots transport URL".to_string())?;
    request.headers_mut().extend(headers);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let (mut socket, _) = tokio::time::timeout(
        REQUEST_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| "dots cloud transport connection timed out".to_string())?
    .map_err(|error| {
        // Never format the handshake, request headers, server body, or error
        // Debug: the bearer travels in Sec-WebSocket-Protocol.
        if let tokio_tungstenite::tungstenite::Error::Http(response) = error {
            format!(
                "dots cloud transport refused the handshake (HTTP {})",
                response.status().as_u16()
            )
        } else {
            "could not connect to the dots cloud transport".to_string()
        }
    })?;

    let result = async {
        let initialized = rpc(
            &mut socket,
            1,
            ReadRequest::Initialize,
            json!({
                "clientInfo": {"name": "intendant-dots-diagnostic", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        ).await?;
        if initialized.get("userAgent").and_then(Value::as_str).is_none() {
            return Err("dots initialize response has an unrecognized private schema".into());
        }
        socket
            .send(Message::Text(json!({"method": "initialized"}).to_string().into()))
            .await
            .map_err(|_| "dots initialized notification could not be delivered".to_string())?;
        let catalog = rpc(
            &mut socket,
            2,
            ReadRequest::ListThreads,
            json!({"limit": PAGE_LIMIT, "archived": false, "useStateDbOnly": true}),
        ).await?;
        let (candidates, has_more) = parse_catalog(&catalog)?;
        let mut metadata_verified = false;
        if let Some(id) = candidates.first() {
            let read = rpc(
                &mut socket,
                3,
                ReadRequest::ReadThread,
                json!({"threadId": id, "includeTurns": false}),
            ).await?;
            validate_candidate_read(&read, id)?;
            metadata_verified = true;
        }
        Ok(DoctorReport {
            integration_status: "read_only_diagnostic",
            transport: CLOUD_URL,
            authenticated_catalog: true,
            candidate_root_threads_on_page: candidates.len(),
            catalog_has_more: has_more,
            candidate_metadata_verified: metadata_verified,
            presence_backend_enabled: false,
            message_send_validated: false,
            dots_voice_validated: false,
            cloud_desktop_validated: false,
            http: None,
        })
    }.await;
    // A private provider may not finish a close handshake. Do not hang the
    // diagnostic or let cleanup replace its observed result.
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
    result
}

async fn rpc<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    id: u64,
    request: ReadRequest,
    params: Value,
) -> Result<Value, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        socket
            .send(Message::Text(
                json!({"id": id, "method": request.method(), "params": params})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| "dots read-only request could not be delivered".to_string())?;
        for _ in 0..MAX_IGNORED_MESSAGES {
            let message = socket
                .next()
                .await
                .ok_or_else(|| "dots transport closed before its read-only response".to_string())?
                .map_err(|_| {
                    "dots transport failed while awaiting its read-only response".to_string()
                })?;
            match message {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|_| "dots transport returned invalid JSON".to_string())?;
                    if let Some(result) = read_response(value, id)? {
                        return Ok(result);
                    }
                }
                Message::Ping(_) | Message::Pong(_) => {}
                _ => return Err("dots transport returned an unexpected message type".into()),
            }
        }
        Err("dots transport exceeded the bounded read-only response window".into())
    })
    .await
    .map_err(|_| "dots read-only request timed out; no mutating request was sent".to_string())?
}

fn read_response(mut value: Value, expected_id: u64) -> Result<Option<Value>, String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| "dots RPC response is not an object".to_string())?;
    if object.contains_key("method") {
        if object.contains_key("id") {
            return Err(
                "dots requested a client-side action; the read-only diagnostic refuses it".into(),
            );
        }
        return Ok(None);
    }
    if object.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Err("dots RPC response correlation changed".into());
    }
    if object.contains_key("error") {
        // Remote error strings can contain reflected credentials or private
        // conversation content. Only a numeric code is eligible for output.
        let code = object
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64);
        return Err(match code {
            Some(code) => format!("dots read-only RPC was refused (code {code})"),
            None => "dots read-only RPC was refused".into(),
        });
    }
    object
        .remove("result")
        .map(Some)
        .ok_or_else(|| "dots RPC result is missing".to_string())
}

fn parse_catalog(value: &Value) -> Result<(Vec<String>, bool), String> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .filter(|data| data.len() <= PAGE_LIMIT)
        .ok_or_else(|| {
            "dots catalog has an unrecognized or oversized private schema".to_string()
        })?;
    let has_more = match value.get("nextCursor") {
        Some(Value::Null) => false,
        Some(Value::String(cursor)) if !cursor.is_empty() => true,
        _ => return Err("dots catalog pagination schema changed".into()),
    };
    let mut candidates = Vec::new();
    for thread in data {
        if !thread.is_object() || thread.get("threadSource").is_none() {
            return Err("dots catalog thread-source schema changed".into());
        }
        if thread.get("threadSource").and_then(Value::as_str) == Some("aeon") {
            let id = thread
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
                .ok_or_else(|| "dots candidate root-thread identity schema changed".to_string())?;
            if candidates.iter().any(|candidate| candidate == id) {
                return Err("dots catalog repeated a candidate root-thread identity".into());
            }
            candidates.push(id.to_string());
        }
    }
    Ok((candidates, has_more))
}

fn validate_candidate_read(value: &Value, id: &str) -> Result<(), String> {
    let thread = value.get("thread");
    if thread
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        != Some(id)
        || thread
            .and_then(|thread| thread.get("threadSource"))
            .and_then(Value::as_str)
            != Some("aeon")
    {
        return Err("dots metadata did not match the requested candidate root thread".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderValue};

    const ID: &str = "00000000-0000-7000-8000-000000000001";

    #[test]
    fn catalog_distinguishes_candidates_from_workers_and_other_threads() {
        let catalog = json!({"data": [
            {"id": ID, "threadSource": "aeon", "preview": "private transcript"},
            {"id": "worker", "threadSource": "aeon_child"},
            {"id": "sleep", "threadSource": "dreaming"},
            {"id": "ordinary", "threadSource": "user"}
        ], "nextCursor": null});
        assert_eq!(
            parse_catalog(&catalog).unwrap(),
            (vec![ID.to_string()], false)
        );
        assert!(
            parse_catalog(&json!({"data": [], "nextCursor": "opaque"}))
                .unwrap()
                .1
        );
        assert!(parse_catalog(&json!({"data": []})).is_err());
        assert!(parse_catalog(&json!({"data": [null], "nextCursor": null})).is_err());
        assert!(parse_catalog(
            &json!({"data": [{"threadSource": "aeon", "id": "unrecognized"}], "nextCursor": null})
        )
        .is_err());
        assert!(parse_catalog(&json!({"data": vec![json!({"threadSource": "user"}); PAGE_LIMIT + 1], "nextCursor": null})).is_err());
    }

    #[test]
    fn responses_fail_closed_without_reflecting_private_content() {
        assert!(read_response(
            json!({"method": "notification", "params": {"secret": "private"}}),
            1
        )
        .unwrap()
        .is_none());
        for response in [
            json!({"id": 1, "method": "command/exec", "params": {"secret": "private"}}),
            json!({"id": 2, "result": {"secret": "private"}}),
            json!({"id": 1, "error": {"code": -32601, "message": "private"}}),
            json!({"id": 1}),
            json!(["private"]),
        ] {
            let error = read_response(response, 1).unwrap_err();
            assert!(!error.contains("private"));
        }
        validate_candidate_read(&json!({"thread": {"id": ID, "threadSource": "aeon"}}), ID)
            .unwrap();
        assert!(validate_candidate_read(
            &json!({"thread": {"id": ID, "threadSource": "user"}}),
            ID
        )
        .is_err());
    }

    #[tokio::test]
    async fn diagnostic_transport_is_read_only_and_hermetic() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    response.headers_mut().insert(
                        SEC_WEBSOCKET_PROTOCOL,
                        HeaderValue::from_static("codex-app-server"),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let mut seen = Vec::new();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap();
                seen.push(method.to_string());
                let result = match method {
                    "initialize" => json!({"userAgent": "fixture"}),
                    "initialized" => continue,
                    "thread/list" => {
                        json!({"data": [{"id": ID, "threadSource": "aeon"}], "nextCursor": null})
                    }
                    "thread/read" => {
                        assert_eq!(request["params"]["includeTurns"], false);
                        json!({"thread": {"id": ID, "threadSource": "aeon", "preview": "fixture-private"}})
                    }
                    _ => panic!("diagnostic sent a non-read-only method"),
                };
                socket
                    .send(Message::Text(
                        json!({"id": request["id"], "result": result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                if method == "thread/read" {
                    break;
                }
            }
            seen
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("codex-app-server"),
        );
        let report = diagnose(&format!("ws://{address}/"), headers)
            .await
            .unwrap();
        assert_eq!(
            server.await.unwrap(),
            ["initialize", "initialized", "thread/list", "thread/read"]
        );
        assert!(report.authenticated_catalog);
        assert!(report.candidate_metadata_verified);
        assert!(!report.presence_backend_enabled);
        assert!(!report.message_send_validated);
        assert!(!report.dots_voice_validated);
        assert!(!report.cloud_desktop_validated);
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("fixture-private"));
    }
}
