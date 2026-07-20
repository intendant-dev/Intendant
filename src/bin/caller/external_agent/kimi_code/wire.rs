//! Authenticated REST helpers for Kimi server 0.27's `/api/v1` contract.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::Method;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::error::CallerError;

// Uploaded attachments are consumed asynchronously by Kimi prompts, so they
// cannot be deleted immediately after submit. Bound their server-side lifetime
// while leaving ample room for a queued prompt or a long autonomous turn.
const UPLOAD_EXPIRES_IN_SEC: u64 = 7 * 24 * 60 * 60;
const MAX_JSON_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct KimiApi {
    client: reqwest::Client,
    origin: String,
    token: String,
}

impl KimiApi {
    pub(crate) fn new(origin: String, token: String) -> Result<Self, CallerError> {
        let origin = normalize_loopback_origin(&origin)?;
        let client = reqwest::Client::builder()
            // This bearer-authenticated client is intentionally confined to
            // the validated loopback origin. Inheriting HTTP(S)_PROXY could
            // disclose both the token and private control traffic.
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| external(format!("failed to build Kimi HTTP client: {error}")))?;
        Ok(Self {
            client,
            origin,
            token,
        })
    }

    pub(crate) fn websocket_url(&self) -> String {
        format!(
            "{}://{}/api/v1/ws",
            if self.origin.starts_with("https:") {
                "wss"
            } else {
                "ws"
            },
            self.origin
                .split_once("://")
                .map(|(_, authority)| authority)
                .unwrap_or(&self.origin)
        )
    }

    pub(crate) fn authorization_value(&self) -> Result<reqwest::header::HeaderValue, CallerError> {
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| external("Kimi server token contains invalid header bytes"))
    }

    pub(crate) async fn health(&self) -> Result<(), CallerError> {
        let value = self.get("/healthz").await?;
        if value.is_null() || value.is_object() {
            Ok(())
        } else {
            Err(external("Kimi server returned a malformed health response"))
        }
    }

    pub(crate) async fn meta(&self) -> Result<Value, CallerError> {
        self.get("/meta").await
    }

    pub(crate) async fn create_session(
        &self,
        working_dir: &Path,
        agent_config: Value,
    ) -> Result<Value, CallerError> {
        self.post(
            "/sessions",
            &serde_json::json!({
                "metadata": { "cwd": working_dir.to_string_lossy() },
                "agent_config": agent_config,
            }),
        )
        .await
    }

    pub(crate) async fn get_session(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!("/sessions/{}", component(session_id)))
            .await
    }

    pub(crate) async fn snapshot(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!("/sessions/{}/snapshot", component(session_id)))
            .await
    }

    pub(crate) async fn goal(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!("/sessions/{}/goal", component(session_id)))
            .await
    }

    pub(crate) async fn warnings(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!("/sessions/{}/warnings", component(session_id)))
            .await
    }

    pub(crate) async fn list_tasks(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!("/sessions/{}/tasks", component(session_id)))
            .await
    }

    pub(crate) async fn task(
        &self,
        session_id: &str,
        task_id: &str,
        output_bytes: usize,
    ) -> Result<Value, CallerError> {
        self.get(&format!(
            "/sessions/{}/tasks/{}?with_output=true&output_bytes={}",
            component(session_id),
            component(task_id),
            output_bytes.clamp(1, 1_048_576)
        ))
        .await
    }

    pub(crate) async fn cancel_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<Value, CallerError> {
        self.post(
            &format!(
                "/sessions/{}/tasks/{}:cancel",
                component(session_id),
                component(task_id)
            ),
            &serde_json::json!({}),
        )
        .await
    }

    pub(crate) async fn update_profile(
        &self,
        session_id: &str,
        agent_config: Value,
    ) -> Result<Value, CallerError> {
        self.post(
            &format!("/sessions/{}/profile", component(session_id)),
            &serde_json::json!({ "agent_config": agent_config }),
        )
        .await
    }

    pub(crate) async fn update_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<Value, CallerError> {
        self.post(
            &format!("/sessions/{}/profile", component(session_id)),
            &serde_json::json!({ "title": title }),
        )
        .await
    }

    pub(crate) async fn session_action(
        &self,
        session_id: &str,
        action: &str,
        body: Value,
    ) -> Result<Value, CallerError> {
        if !matches!(
            action,
            "fork" | "compact" | "undo" | "abort" | "btw" | "archive" | "restore"
        ) {
            return Err(external(format!(
                "unsupported Kimi session action {action}"
            )));
        }
        self.post(
            &format!("/sessions/{}:{action}", component(session_id)),
            &body,
        )
        .await
    }

    pub(crate) async fn submit_prompt(
        &self,
        session_id: &str,
        content: Vec<Value>,
        overrides: Value,
    ) -> Result<Value, CallerError> {
        let mut body = serde_json::Map::new();
        body.insert("content".to_string(), Value::Array(content));
        if let Some(overrides) = overrides.as_object() {
            body.extend(overrides.clone());
        }
        self.post(
            &format!("/sessions/{}/prompts", component(session_id)),
            &Value::Object(body),
        )
        .await
    }

    pub(crate) async fn list_prompts(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!("/sessions/{}/prompts", component(session_id)))
            .await
    }

    /// Promote queued prompts into the active turn. Kimi 0.27's collection
    /// action is deliberately spelled `prompts::steer` (double colon).
    pub(crate) async fn steer_prompts(
        &self,
        session_id: &str,
        prompt_ids: &[String],
    ) -> Result<Value, CallerError> {
        self.post(
            &format!("/sessions/{}/prompts::steer", component(session_id)),
            &serde_json::json!({ "prompt_ids": prompt_ids }),
        )
        .await
    }

    pub(crate) async fn abort_prompt(
        &self,
        session_id: &str,
        prompt_id: &str,
    ) -> Result<Value, CallerError> {
        self.post(
            &format!(
                "/sessions/{}/prompts/{}:abort",
                component(session_id),
                component(prompt_id)
            ),
            &serde_json::json!({}),
        )
        .await
    }

    pub(crate) async fn list_approvals(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!(
            "/sessions/{}/approvals?status=pending",
            component(session_id)
        ))
        .await
    }

    pub(crate) async fn resolve_approval(
        &self,
        session_id: &str,
        approval_id: &str,
        body: Value,
    ) -> Result<Value, CallerError> {
        self.post(
            &format!(
                "/sessions/{}/approvals/{}",
                component(session_id),
                component(approval_id)
            ),
            &body,
        )
        .await
    }

    pub(crate) async fn list_questions(&self, session_id: &str) -> Result<Value, CallerError> {
        self.get(&format!(
            "/sessions/{}/questions?status=pending",
            component(session_id)
        ))
        .await
    }

    pub(crate) async fn resolve_question(
        &self,
        session_id: &str,
        question_id: &str,
        body: Value,
    ) -> Result<Value, CallerError> {
        self.post(
            &format!(
                "/sessions/{}/questions/{}",
                component(session_id),
                component(question_id)
            ),
            &body,
        )
        .await
    }

    pub(crate) async fn dismiss_question(
        &self,
        session_id: &str,
        question_id: &str,
    ) -> Result<Value, CallerError> {
        self.post(
            &format!(
                "/sessions/{}/questions/{}:dismiss",
                component(session_id),
                component(question_id)
            ),
            &serde_json::json!({}),
        )
        .await
    }

    pub(crate) async fn upload_file(
        &self,
        path: &Path,
        name: &str,
        media_type: &str,
    ) -> Result<Value, CallerError> {
        let file = tokio::fs::File::open(path).await.map_err(|error| {
            external(format!(
                "failed to open Kimi attachment {}: {error}",
                path.display()
            ))
        })?;
        let size = file
            .metadata()
            .await
            .map_err(|error| {
                external(format!(
                    "failed to inspect Kimi attachment {}: {error}",
                    path.display()
                ))
            })?
            .len();
        let stream =
            futures_util::stream::try_unfold((file, size), |(mut file, remaining)| async move {
                if remaining == 0 {
                    return Ok(None);
                }
                let capacity = remaining.min(UPLOAD_CHUNK_BYTES as u64) as usize;
                let mut chunk = vec![0u8; capacity];
                let read = file.read(&mut chunk).await?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Kimi attachment changed while it was being streamed",
                    ));
                }
                chunk.truncate(read);
                Ok(Some((
                    Bytes::from(chunk),
                    (file, remaining.saturating_sub(read as u64)),
                )))
            });
        let part =
            reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), size)
                .file_name(name.to_string())
                .mime_str(media_type)
                .map_err(|error| external(format!("invalid attachment media type: {error}")))?;
        let form = reqwest::multipart::Form::new()
            .text("name", name.to_string())
            .text("expires_in_sec", UPLOAD_EXPIRES_IN_SEC.to_string())
            .part("file", part);
        let response = self
            .client
            .post(self.endpoint("/files"))
            .header(reqwest::header::AUTHORIZATION, self.authorization_value()?)
            .multipart(form)
            .send()
            .await
            .map_err(|error| external(format!("Kimi file upload failed: {error}")))?;
        decode_response(response, "upload file").await
    }

    async fn get(&self, path: &str) -> Result<Value, CallerError> {
        self.request(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, CallerError> {
        self.request(Method::POST, path, Some(body)).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, CallerError> {
        let mut request = self
            .client
            .request(method, self.endpoint(path))
            .header(reqwest::header::AUTHORIZATION, self.authorization_value()?)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| external(format!("Kimi server request failed: {error}")))?;
        decode_response(response, path).await
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.origin, path)
    }
}

async fn decode_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<Value, CallerError> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_JSON_RESPONSE_BYTES as u64)
    {
        return Err(external(format!(
            "Kimi {operation} response exceeded the {} byte limit",
            MAX_JSON_RESPONSE_BYTES
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| external(format!("failed to read Kimi server response: {error}")))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_JSON_RESPONSE_BYTES {
            return Err(external(format!(
                "Kimi {operation} response exceeded the {} byte limit",
                MAX_JSON_RESPONSE_BYTES
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    let envelope: Value = serde_json::from_slice(&bytes).map_err(|error| {
        external(format!(
            "Kimi server returned non-JSON for {operation} (HTTP {status}): {error}"
        ))
    })?;
    let code = envelope.get("code").and_then(Value::as_i64);
    if status.is_success() && code == Some(0) {
        return Ok(envelope.get("data").cloned().unwrap_or(Value::Null));
    }
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    if status.is_success() && idempotent_success(operation, &data) {
        return Ok(data);
    }

    let wire_message = envelope
        .get("msg")
        .or_else(|| envelope.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("request rejected");
    let wire_code = envelope
        .get("code")
        .map(Value::to_string)
        .unwrap_or_else(|| "unknown".to_string());
    Err(external(format!(
        "Kimi {operation} failed (HTTP {}, code {}): {}",
        status.as_u16(),
        wire_code,
        wire_message
    )))
}

fn idempotent_success(operation: &str, data: &Value) -> bool {
    if operation.contains("/questions/") && operation.ends_with(":dismiss") {
        return data.get("dismissed").and_then(Value::as_bool) == Some(true);
    }
    if (operation.contains("/prompts/") && operation.ends_with(":abort"))
        || (operation.starts_with("/sessions/") && operation.ends_with(":abort"))
    {
        return data.get("aborted").and_then(Value::as_bool).is_some();
    }
    if (operation.contains("/questions/") || operation.contains("/approvals/"))
        && !operation.ends_with(":dismiss")
    {
        return data.get("resolved").and_then(Value::as_bool) == Some(false);
    }
    if operation.contains("/tasks/") && operation.ends_with(":cancel") {
        return data.get("cancelled").and_then(Value::as_bool) == Some(false)
            && matches!(
                data.get("status").and_then(Value::as_str),
                Some("completed" | "failed" | "cancelled" | "killed" | "timed_out")
            );
    }
    false
}

fn normalize_loopback_origin(origin: &str) -> Result<String, CallerError> {
    let url = reqwest::Url::parse(origin)
        .map_err(|error| external(format!("invalid Kimi server origin: {error}")))?;
    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "::1" | "localhost") {
        return Err(external(
            "refusing non-loopback Kimi server origin for supervised session",
        ));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(external("unsupported Kimi server URL scheme"));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| external("Kimi server origin has no port"))?;
    Ok(format!("{}://{}:{}", url.scheme(), format_host(host), port))
}

fn format_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

pub(crate) fn component(value: &str) -> String {
    // WHATWG URL parsers normalize even percent-encoded dot segments. Double
    // encode these two degenerate ids so they remain one opaque route
    // component and can only fail as an unknown id, never traverse a route.
    if value == "." {
        return "%252E".to_string();
    }
    if value == ".." {
        return "%252E%252E".to_string();
    }
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub(crate) fn external(message: impl Into<String>) -> CallerError {
    CallerError::ExternalAgent(message.into())
}

pub(crate) fn validate_meta(value: &Value) -> Result<String, CallerError> {
    let version = value
        .get("server_version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| external("Kimi /meta omitted a non-empty server_version"))?;
    let capabilities = value
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| external("Kimi /meta omitted its capabilities object"))?;
    for capability in ["websocket", "file_upload", "mcp", "tasks"] {
        if capabilities.get(capability).and_then(Value::as_bool) != Some(true) {
            return Err(external(format!(
                "Kimi server does not advertise required {capability} capability"
            )));
        }
    }
    if value.get("dangerous_bypass_auth").and_then(Value::as_bool) != Some(false) {
        return Err(external(
            "refusing Kimi server with missing or enabled dangerous_bypass_auth",
        ));
    }
    Ok(version.to_string())
}

pub(crate) fn active_prompt_id(value: &Value) -> Option<String> {
    value
        .get("active")
        .and_then(|active| active.get("prompt_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Strictly decode every active/queued prompt id.
///
/// Submission error recovery snapshots this set before sending a review and
/// aborts only new ids. A malformed list is not equivalent to an empty one:
/// callers keep tools confined instead of widening around an unknown prompt.
pub(crate) fn pending_prompt_ids(value: &Value) -> Result<HashSet<String>, CallerError> {
    let active = value
        .get("active")
        .ok_or_else(|| external("Kimi prompt list omitted active state"))?;
    let mut ids = HashSet::new();
    match active {
        Value::Null => {}
        Value::Object(active) => {
            let id = active
                .get("prompt_id")
                .and_then(Value::as_str)
                .ok_or_else(|| external("Kimi active prompt omitted prompt_id"))?;
            ids.insert(id.to_string());
        }
        _ => return Err(external("Kimi prompt list returned malformed active state")),
    }
    let queued = value
        .get("queued")
        .and_then(Value::as_array)
        .ok_or_else(|| external("Kimi prompt list omitted its queued array"))?;
    for prompt in queued {
        let id = prompt
            .get("prompt_id")
            .and_then(Value::as_str)
            .ok_or_else(|| external("Kimi queued prompt omitted prompt_id"))?;
        ids.insert(id.to_string());
    }
    Ok(ids)
}

/// Whether one exact prompt remains active or queued.
///
/// Review-mode tool restoration must follow the submitted review prompt, not
/// merely "the current prompt": a review can be queued behind an existing
/// turn, and restoring tools when that first turn ends would let the review
/// run with the caller's full tool set.
pub(crate) fn prompt_is_pending(value: &Value, prompt_id: &str) -> Result<bool, CallerError> {
    Ok(pending_prompt_ids(value)?.contains(prompt_id))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;

    async fn mock_server(
        response_status: &str,
        response_body: Value,
    ) -> (
        String,
        oneshot::Receiver<Vec<u8>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = oneshot::channel();
        let status = response_status.to_string();
        let body = response_body.to_string();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let expected_len = loop {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0, "mock client closed before request completed");
                request.extend_from_slice(&buf[..read]);
                let Some(header_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                break header_end + content_len;
            };
            while request.len() < expected_len {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0, "mock client closed before body completed");
                request.extend_from_slice(&buf[..read]);
            }
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            let _ = request_tx.send(request);
        });
        (format!("http://{address}"), request_rx, handle)
    }

    fn request_parts(request: &[u8]) -> (&str, String, Value) {
        let raw = std::str::from_utf8(request).unwrap();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        let request_line = headers.lines().next().unwrap();
        (
            request_line,
            headers.to_ascii_lowercase(),
            if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(body).unwrap()
            },
        )
    }

    #[test]
    fn origin_is_loopback_and_fragment_free() {
        assert_eq!(
            normalize_loopback_origin("http://127.0.0.1:51035/#token=secret").unwrap(),
            "http://127.0.0.1:51035"
        );
        assert!(normalize_loopback_origin("http://example.com:51035").is_err());
    }

    #[test]
    fn path_component_escapes_action_delimiters_and_slashes() {
        assert_eq!(component("session/a:b"), "session%2Fa%3Ab");
        assert_eq!(component("."), "%252E");
        assert_eq!(component(".."), "%252E%252E");
    }

    #[test]
    fn active_prompt_reads_only_active_item() {
        assert_eq!(
            active_prompt_id(&serde_json::json!({
                "active": {"prompt_id": "p1"},
                "queued": [{"prompt_id": "p2"}]
            }))
            .as_deref(),
            Some("p1")
        );
        assert_eq!(active_prompt_id(&serde_json::json!({"active": null})), None);
    }

    #[test]
    fn pending_prompt_matches_active_or_queued_by_exact_id() {
        let prompts = serde_json::json!({
            "active": {"prompt_id": "p1"},
            "queued": [{"prompt_id": "p2"}, {"prompt_id": "p20"}]
        });
        assert_eq!(
            pending_prompt_ids(&prompts).unwrap(),
            HashSet::from(["p1".to_string(), "p2".to_string(), "p20".to_string()])
        );
        assert!(prompt_is_pending(&prompts, "p1").unwrap());
        assert!(prompt_is_pending(&prompts, "p2").unwrap());
        assert!(!prompt_is_pending(&prompts, "p").unwrap());
        assert!(!prompt_is_pending(&prompts, "p3").unwrap());
        assert!(prompt_is_pending(&serde_json::json!({}), "p1").is_err());
        assert!(
            prompt_is_pending(&serde_json::json!({"active": null, "queued": [{}]}), "p1").is_err()
        );
    }

    #[test]
    fn websocket_url_uses_authenticated_server_origin() {
        let api = KimiApi::new("http://localhost:1234".into(), "secret".into()).unwrap();
        assert_eq!(api.websocket_url(), "ws://localhost:1234/api/v1/ws");
    }

    #[test]
    fn meta_requires_hardened_server_and_complete_capabilities() {
        let good = serde_json::json!({
            "server_version": "0.27.0",
            "capabilities": {
                "websocket": true,
                "file_upload": true,
                "mcp": true,
                "tasks": true
            },
            "dangerous_bypass_auth": false
        });
        assert_eq!(validate_meta(&good).unwrap(), "0.27.0");
        let mut missing = good.clone();
        missing["capabilities"]["tasks"] = Value::Bool(false);
        assert!(validate_meta(&missing).is_err());
        let mut unsafe_server = good;
        unsafe_server["dangerous_bypass_auth"] = Value::Bool(true);
        assert!(validate_meta(&unsafe_server).is_err());
    }

    #[tokio::test]
    async fn upload_sets_a_bounded_server_side_expiry() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 0,
                "data": {
                    "id": "file_test",
                    "name": "notes.txt",
                    "media_type": "text/plain",
                    "size": 5
                }
            }),
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, b"hello").await.unwrap();
        let api = KimiApi::new(origin, "token".into()).unwrap();
        api.upload_file(&path, "notes.txt", "text/plain")
            .await
            .unwrap();
        let request = request.await.unwrap();
        let raw = String::from_utf8(request).unwrap();
        assert!(raw.starts_with("POST /api/v1/files HTTP/1.1\r\n"));
        assert!(raw.contains("name=\"expires_in_sec\""));
        assert!(raw.contains(&format!("\r\n\r\n{UPLOAD_EXPIRES_IN_SEC}\r\n")));
        assert!(raw.contains("name=\"file\"; filename=\"notes.txt\""));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn steer_uses_double_colon_collection_action_and_bearer_auth() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "data": {"steered": 2}}),
        )
        .await;
        let api = KimiApi::new(origin, "test-wire-token".into()).unwrap();
        let data = api
            .steer_prompts("session/a", &["p1".into(), "p2".into()])
            .await
            .unwrap();
        assert_eq!(data["steered"], 2);
        let request = request.await.unwrap();
        let (line, headers, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v1/sessions/session%2Fa/prompts::steer HTTP/1.1"
        );
        assert!(headers.contains("\r\nauthorization: bearer test-wire-token\r\n"));
        assert_eq!(body, serde_json::json!({"prompt_ids": ["p1", "p2"]}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_fork_action_uses_session_colon_route() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 0, "data": {"id": "session_child"}}),
        )
        .await;
        let api = KimiApi::new(origin, "token".into()).unwrap();
        let data = api
            .session_action(
                "session_main",
                "fork",
                serde_json::json!({"title": "branch", "metadata": {"source": "intendant"}}),
            )
            .await
            .unwrap();
        assert_eq!(data["id"], "session_child");
        let request = request.await.unwrap();
        let (line, _, body) = request_parts(&request);
        assert_eq!(line, "POST /api/v1/sessions/session_main:fork HTTP/1.1");
        assert_eq!(
            body,
            serde_json::json!({"title": "branch", "metadata": {"source": "intendant"}})
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idempotent_question_dismiss_envelope_is_success() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 40909,
                "msg": "already dismissed",
                "data": {"dismissed": true}
            }),
        )
        .await;
        let api = KimiApi::new(origin, "token".into()).unwrap();
        assert_eq!(
            api.dismiss_question("session_x", "question/y")
                .await
                .unwrap(),
            serde_json::json!({"dismissed": true})
        );
        let request = request.await.unwrap();
        let (line, _, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v1/sessions/session_x/questions/question%2Fy:dismiss HTTP/1.1"
        );
        assert_eq!(body, serde_json::json!({}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn task_output_route_is_bounded_and_component_safe() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 0,
                "data": {"taskId": "task/a", "output_preview": "done"}
            }),
        )
        .await;
        let api = KimiApi::new(origin, "token".into()).unwrap();
        let task = api.task("session/a", "task/a", usize::MAX).await.unwrap();
        assert_eq!(task["output_preview"], "done");
        let request = request.await.unwrap();
        let (line, _, _) = request_parts(&request);
        assert_eq!(
            line,
            "GET /api/v1/sessions/session%2Fa/tasks/task%2Fa?with_output=true&output_bytes=1048576 HTTP/1.1"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn already_terminal_task_cancel_is_idempotent_success() {
        let (origin, request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 40921,
                "msg": "task already terminated",
                "data": {"cancelled": false, "status": "completed"}
            }),
        )
        .await;
        let api = KimiApi::new(origin, "token".into()).unwrap();
        let result = api.cancel_task("session_x", "task/y").await.unwrap();
        assert_eq!(result["cancelled"], false);
        let request = request.await.unwrap();
        let (line, _, body) = request_parts(&request);
        assert_eq!(
            line,
            "POST /api/v1/sessions/session_x/tasks/task%2Fy:cancel HTTP/1.1"
        );
        assert_eq!(body, serde_json::json!({}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn nonzero_envelope_code_is_rejected_even_on_http_success() {
        let (origin, _request, server) = mock_server(
            "200 OK",
            serde_json::json!({"code": 40001, "msg": "bad request", "data": null}),
        )
        .await;
        let api = KimiApi::new(origin, "token".into()).unwrap();
        let error = api.get_session("session_x").await.unwrap_err();
        assert!(error.to_string().contains("code 40001"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idempotent_fields_never_mask_an_unrelated_endpoint_failure() {
        let (origin, _request, server) = mock_server(
            "200 OK",
            serde_json::json!({
                "code": 40909,
                "msg": "not a session",
                "data": {
                    "dismissed": true,
                    "aborted": false,
                    "resolved": false,
                    "cancelled": false,
                    "status": "completed"
                }
            }),
        )
        .await;
        let api = KimiApi::new(origin, "token".into()).unwrap();
        let error = api.get_session("session_x").await.unwrap_err();
        assert!(error.to_string().contains("code 40909"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_content_length_is_rejected_before_body_buffering() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                MAX_JSON_RESPONSE_BYTES + 1
            );
            stream.write_all(header.as_bytes()).await.unwrap();
        });
        let api = KimiApi::new(format!("http://{address}"), "token".into()).unwrap();
        let error = api.get_session("session_x").await.unwrap_err();
        assert!(error.to_string().contains("response exceeded"));
        server.await.unwrap();
    }
}
