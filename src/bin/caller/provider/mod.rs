use crate::conversation::Message;
use crate::error::CallerError;
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;

mod openai;
pub(crate) use openai::*;

/// HTTP client timeout for API requests (120 seconds).
const API_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum number of retries for rate-limited or server-error responses.
const MAX_RETRIES: u32 = 5;

fn api_client() -> Client {
    Client::builder()
        .timeout(API_TIMEOUT)
        .build()
        .unwrap_or_else(|_| Client::new())
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn backoff_delay(attempt: u32) -> Duration {
    let base_ms = 1000u64 * 2u64.saturating_pow(attempt);
    // Add simple jitter: up to 500ms
    let jitter_ms = (attempt as u64 * 137) % 500;
    Duration::from_millis(base_ms + jitter_ms)
}

async fn send_with_retry(
    _client: &Client,
    build_request: impl Fn() -> reqwest::RequestBuilder,
    max_retries: u32,
) -> Result<reqwest::Response, CallerError> {
    let mut last_err = None;
    for attempt in 0..=max_retries {
        let response = build_request().send().await?;
        if response.status().is_success() || !is_retryable_status(response.status()) {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        last_err = Some(format!("{}: {}", status, mask_api_keys(&body)));
        if attempt < max_retries {
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }
    Err(CallerError::Provider(last_err.unwrap_or_else(|| {
        "request failed after retries".to_string()
    })))
}

/// Parse Server-Sent Events from a byte stream. Returns (event_type, data) pairs.
/// Handles multi-line data fields by joining with newlines.
fn parse_sse_line(line: &str) -> Option<(&str, &str)> {
    if let Some(rest) = line.strip_prefix("data: ") {
        Some(("data", rest))
    } else if let Some(rest) = line.strip_prefix("event: ") {
        Some(("event", rest))
    } else {
        None
    }
}

/// Streaming timeout for SSE connections (10 minutes).
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);

// TokenUsage is hoisted to intendant-core (its consumers span the event
// vocabulary and session layers); re-exported at the old mount point.
pub use intendant_core::usage::TokenUsage;

/// Parse Anthropic's per-minute rate-limit headers into vitals windows.
/// `-reset` is RFC 3339; a missing or unparsable header degrades to a
/// gauge without a countdown.
fn anthropic_rate_limit_windows_from_headers(
    headers: &reqwest::header::HeaderMap,
) -> Vec<crate::types::SessionLimitWindow> {
    let read = |name: String| headers.get(name).and_then(|v| v.to_str().ok());
    let mut windows = Vec::new();
    for (label, prefix) in [
        ("req/min", "anthropic-ratelimit-requests"),
        ("tok/min", "anthropic-ratelimit-tokens"),
    ] {
        let limit = read(format!("{prefix}-limit")).and_then(|s| s.parse::<f64>().ok());
        let remaining = read(format!("{prefix}-remaining")).and_then(|s| s.parse::<f64>().ok());
        let (Some(limit), Some(remaining)) = (limit, remaining) else {
            continue;
        };
        if limit <= 0.0 {
            continue;
        }
        let used_pct = (((limit - remaining) / limit) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8;
        let resets_at_epoch = read(format!("{prefix}-reset"))
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp().max(0) as u64);
        windows.push(crate::types::SessionLimitWindow {
            label: label.to_string(),
            used_pct,
            resets_at_epoch,
        });
    }
    windows
}

/// A tool call returned by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Provider item identity when the provider exposes one.
    pub id: String,
    /// Local correlation key used to pair calls with tool results.
    /// For some providers this is distinct from `id`; for others it equals
    /// the provider item identity or a synthetic local id.
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub usage: TokenUsage,
    pub reasoning_summary: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Native computer-use tool calls (parsed from provider-specific format).
    pub cu_calls: Vec<super::computer_use::CuToolCall>,
    /// Opaque provider transcript items that must be echoed back verbatim in
    /// subsequent requests. Currently used for OpenAI Responses output items
    /// and Gemini raw parts carrying `thoughtSignature`.
    pub raw_output: Option<Vec<serde_json::Value>>,
}

/// How a provider instance authenticates its requests: a real key
/// (lease or environment) attached daemon-side, or the client-egress
/// marker — a registered browser relay holds the credential, the daemon
/// ships auth-less requests to it, and the browser attaches the key
/// (credential custody, rollout step 5). `From<String>` keeps every
/// existing key-shaped construction site source-compatible.
#[derive(Debug, Clone)]
pub enum ProviderAuth {
    Key(String),
    ClientEgress { kind: &'static str },
}

impl From<String> for ProviderAuth {
    fn from(api_key: String) -> Self {
        ProviderAuth::Key(api_key)
    }
}

/// The credential a selector binds for a provider: a real key first
/// (lease shadows env, as everywhere), else the egress marker when a
/// browser relay is attached for the kind.
fn provider_auth_for(env_name: &str, egress_kind: &'static str) -> Option<ProviderAuth> {
    crate::credential_leases::provider_api_key(env_name)
        .map(ProviderAuth::Key)
        .or_else(|| {
            crate::credential_egress::available(egress_kind)
                .then_some(ProviderAuth::ClientEgress { kind: egress_kind })
        })
}

/// The provider API-key environment variables — the single authoritative
/// list. The Settings save endpoint (`POST /api/api-keys`), the key-status
/// endpoint, the `fueled` aggregate, and the per-session project `.env`
/// overlay all derive from it.
pub const PROVIDER_KEY_ENV_VARS: &[&str] =
    &["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"];

/// Where the startup `.env` search actually looked, recorded once by
/// `main()`. Under a daemon, the CLI-era "your project root" reads as the
/// project a session was created with — which is NOT searched at startup —
/// so credential errors name these concrete paths instead.
#[derive(Debug, Clone, Default)]
pub struct EnvSearchReport {
    /// The `.env` the cwd walk-up found and loaded, if any.
    pub cwd_env: Option<std::path::PathBuf>,
    /// The daemon project root's `.env` and whether it loaded.
    pub project_env: Option<(std::path::PathBuf, bool)>,
    /// `~/.config/intendant/.env` and whether it loaded.
    pub global_env: Option<(std::path::PathBuf, bool)>,
}

static ENV_SEARCH: std::sync::OnceLock<EnvSearchReport> = std::sync::OnceLock::new();

/// Record the startup `.env` search (first call wins; later calls no-op).
pub fn record_env_search(report: EnvSearchReport) {
    let _ = ENV_SEARCH.set(report);
}

/// Provider API keys read from a session project's `.env` — the LAST
/// resolution layer: credential leases, the daemon's environment, and a
/// registered browser egress relay all win over it. Only the names in
/// [`PROVIDER_KEY_ENV_VARS`] are honored. A project directory is
/// agent-writable, so nothing with endpoint-shaped power (base URLs,
/// `PROVIDER`/model selection) may load from there — a planted key bills
/// the planter, a planted endpoint would exfiltrate conversations. Parsed
/// per session and never written into the process environment, so
/// concurrent sessions with different projects cannot contaminate each
/// other.
#[derive(Debug, Clone, Default)]
pub struct ProjectEnvKeys {
    env_path: Option<std::path::PathBuf>,
    file_present: bool,
    keys: std::collections::HashMap<String, String>,
}

impl ProjectEnvKeys {
    /// The empty overlay: resolution is exactly the classic
    /// lease → environment → relay chain.
    pub fn none() -> Self {
        Self::default()
    }

    /// Parse `<project_root>/.env`, keeping only provider API keys.
    pub fn load(project_root: &std::path::Path) -> Self {
        let env_path = project_root.join(".env");
        let mut keys = std::collections::HashMap::new();
        let mut file_present = false;
        if let Ok(iter) = dotenvy::from_path_iter(&env_path) {
            file_present = true;
            for (name, value) in iter.flatten() {
                if PROVIDER_KEY_ENV_VARS.contains(&name.as_str()) && !value.trim().is_empty() {
                    keys.insert(name, value);
                }
            }
        }
        Self {
            env_path: Some(env_path),
            file_present,
            keys,
        }
    }

    fn get(&self, name: &str) -> Option<String> {
        self.keys.get(name).cloned()
    }

    #[cfg(test)]
    fn has_any(&self) -> bool {
        !self.keys.is_empty()
    }
}

/// A provider HTTP response from either path. Only the surface the
/// provider parsers actually consume: status, text, JSON, chunk stream.
enum ProviderHttpResponse {
    Direct(reqwest::Response),
    Egress(crate::credential_egress::EgressResponse),
}

impl ProviderHttpResponse {
    fn status_success(&self) -> bool {
        match self {
            ProviderHttpResponse::Direct(response) => response.status().is_success(),
            ProviderHttpResponse::Egress(response) => response.status_success(),
        }
    }

    fn status_line(&self) -> String {
        match self {
            ProviderHttpResponse::Direct(response) => response.status().to_string(),
            ProviderHttpResponse::Egress(response) => response.status.to_string(),
        }
    }

    async fn body_text(self) -> String {
        match self {
            ProviderHttpResponse::Direct(response) => response.text().await.unwrap_or_default(),
            ProviderHttpResponse::Egress(response) => {
                response.body_text().await.unwrap_or_default()
            }
        }
    }

    async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, CallerError> {
        match self {
            ProviderHttpResponse::Direct(response) => Ok(response.json().await?),
            ProviderHttpResponse::Egress(response) => {
                let body = response.body_text().await.map_err(CallerError::Provider)?;
                serde_json::from_str(&body).map_err(CallerError::Json)
            }
        }
    }

    fn bytes_stream(
        self,
    ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Vec<u8>, String>> + Send>> {
        match self {
            ProviderHttpResponse::Direct(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(|e| e.to_string())),
            ),
            ProviderHttpResponse::Egress(response) => Box::pin(response.bytes_stream()),
        }
    }

    /// Anthropic rate-limit windows from the response headers; empty for
    /// egress-relayed calls (the relay strips headers).
    fn anthropic_rate_limit_windows(&self) -> Vec<crate::types::SessionLimitWindow> {
        match self {
            ProviderHttpResponse::Direct(response) => {
                anthropic_rate_limit_windows_from_headers(response.headers())
            }
            ProviderHttpResponse::Egress(_) => Vec::new(),
        }
    }
}

/// Events emitted during streaming responses.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text delta from the model.
    Delta(String),
    /// A tool call delta (accumulated; final call emitted with Complete).
    #[allow(dead_code)]
    ToolCallDelta {
        index: usize,
        id: String,
        name: String,
        arguments_delta: String,
    },
    /// The complete response (same as non-streaming `chat()` would return).
    #[allow(dead_code)]
    Complete(ChatResponse),
}

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError>;
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn context_window(&self) -> u64;
    #[allow(dead_code)]
    fn max_output_tokens(&self) -> u64;

    /// Whether this provider instance has native tool calling enabled.
    fn use_tools(&self) -> bool {
        false
    }

    /// Whether this provider instance has native computer-use enabled.
    fn cu_enabled(&self) -> bool {
        false
    }

    /// Display dimensions for CU (width, height), if CU is enabled.
    fn cu_display(&self) -> Option<(u32, u32)> {
        None
    }

    /// Enable or disable native computer-use on this provider instance.
    fn set_cu_enabled(&mut self, _enabled: bool) {}

    /// Override display dimensions for CU. Used when the actual display size
    /// differs from the default (e.g. user's real display vs virtual display).
    fn set_cu_display(&mut self, _dims: (u32, u32)) {}

    /// Return tool definitions when native tool calling is enabled.
    #[allow(dead_code)]
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }

    /// Build the provider-specific request body that will be sent for this
    /// message slice. Used by the dashboard Context tab to show the exact
    /// model request payload after provider role conversion, system/developer
    /// fields, tools, and native computer-use blocks are applied.
    fn request_snapshot(
        &self,
        _messages: &[Message],
        _stream: bool,
    ) -> Result<(String, serde_json::Value), CallerError> {
        Err(CallerError::Provider(
            "provider does not expose a request payload snapshot".to_string(),
        ))
    }

    /// Stream a chat response, emitting deltas via the callback.
    /// Default implementation falls back to non-streaming `chat()`.
    async fn chat_stream(
        &self,
        messages: &[Message],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ChatResponse, CallerError> {
        let response = self.chat(messages).await?;
        if !response.content.is_empty() {
            on_event(StreamEvent::Delta(response.content.clone()));
        }
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

// --- Anthropic ---

#[derive(Serialize)]
struct AnthropicChatRequest {
    model: String,
    system: serde_json::Value,
    messages: Vec<AnthropicMessage>,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

/// Anthropic message with content as either a plain string or structured blocks.
#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value, // String or array of content blocks
}

#[derive(Deserialize)]
struct AnthropicChatResponse {
    content: Vec<AnthropicContent>,
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    content_type: Option<String>,
    text: Option<String>,
    // tool_use fields
    id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    /// Uncached prompt tokens only — Anthropic reports cache reads and
    /// writes as separate counters, unlike OpenAI where `cached` is a
    /// subset of the prompt total.
    input_tokens: u64,
    output_tokens: u64,
    /// Tokens read from Anthropic prompt cache.
    #[serde(default)]
    cache_read_input_tokens: u64,
    /// Tokens written to the prompt cache this request.
    #[serde(default)]
    cache_creation_input_tokens: u64,
    /// Per-TTL breakdown of cache writes (present when the extended-TTL
    /// beta is active).
    #[serde(default)]
    cache_creation: Option<AnthropicCacheCreation>,
}

#[derive(Default, Deserialize)]
struct AnthropicCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

/// The TTL flavor a response's cache writes imply. Only creation makes a
/// flavor statement — read-only responses return `None` and consumers keep
/// the last known flavor.
fn anthropic_cache_ttl_seconds(
    cache_creation_input_tokens: u64,
    cache_creation: Option<&AnthropicCacheCreation>,
) -> Option<u32> {
    if let Some(split) = cache_creation {
        if split.ephemeral_1h_input_tokens > 0 {
            return Some(3600);
        }
        if split.ephemeral_5m_input_tokens > 0 {
            return Some(300);
        }
    }
    (cache_creation_input_tokens > 0).then_some(300)
}

impl AnthropicUsage {
    /// Normalize to the [`TokenUsage`] convention: `prompt_tokens` is the
    /// full context footprint (uncached + cache reads + cache writes), with
    /// the cache counters as subsets. Anthropic's raw `input_tokens`
    /// excludes cache traffic, which used to make cached sessions underread
    /// context pressure and misprice input tokens.
    fn to_token_usage(&self) -> TokenUsage {
        let prompt_tokens =
            self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens;
        TokenUsage {
            prompt_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: prompt_tokens + self.output_tokens,
            cached_tokens: self.cache_read_input_tokens,
            cache_creation_tokens: self.cache_creation_input_tokens,
            cache_ttl_seconds: anthropic_cache_ttl_seconds(
                self.cache_creation_input_tokens,
                self.cache_creation.as_ref(),
            ),
            // Header-derived; attached by the transport paths.
            rate_limit_windows: Vec::new(),
        }
    }
}

pub struct AnthropicProvider {
    client: Client,
    auth: ProviderAuth,
    endpoint: String,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    use_tools: bool,
    custom_tools: Option<Vec<ToolDefinition>>,
    /// When true, include native computer-use tool in API requests.
    pub cu_enabled: bool,
    /// Display dimensions for CU (width, height).
    pub cu_display: Option<(u32, u32)>,
}

fn anthropic_endpoint() -> String {
    env::var("ANTHROPIC_ENDPOINT").unwrap_or_else(|_| "https://api.anthropic.com".to_string())
}

impl AnthropicProvider {
    pub fn new(
        api_key: impl Into<ProviderAuth>,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let use_tools = resolve_use_tools();
        Self {
            client: api_client(),
            auth: api_key.into(),
            endpoint: anthropic_endpoint(),
            model,
            context_window,
            max_output_tokens,
            use_tools,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_plain(
        api_key: impl Into<ProviderAuth>,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        Self {
            client: api_client(),
            auth: api_key.into(),
            endpoint: anthropic_endpoint(),
            model,
            context_window,
            max_output_tokens,
            use_tools: false,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_with_tools(
        api_key: impl Into<ProviderAuth>,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            client: api_client(),
            auth: api_key.into(),
            endpoint: anthropic_endpoint(),
            model,
            context_window,
            max_output_tokens,
            use_tools: true,
            custom_tools: Some(tools),
            cu_enabled: false,
            cu_display: None,
        }
    }

    /// A tool-less instance forced through the client-egress relay —
    /// the probe path; normal selection converts availability into
    /// `ProviderAuth::ClientEgress` instead.
    pub fn new_client_egress(model: String, context_window: u64, max_output_tokens: u64) -> Self {
        let mut provider = Self::new_plain(String::new(), model, context_window, max_output_tokens);
        provider.auth = ProviderAuth::ClientEgress {
            kind: crate::credential_egress::KIND_ANTHROPIC,
        };
        provider
    }

    /// Build the messages POST through whichever auth path this instance
    /// carries. `headers` excludes credentials; the relay adds those.
    async fn post_messages(
        &self,
        request_json: &serde_json::Value,
        beta_header: &str,
        streaming: bool,
    ) -> Result<ProviderHttpResponse, CallerError> {
        let url = format!("{}/v1/messages", self.endpoint);
        match &self.auth {
            ProviderAuth::Key(api_key) => {
                let builder = || {
                    let request = self
                        .client
                        .post(&url)
                        .header("x-api-key", api_key)
                        .header("anthropic-version", "2023-06-01")
                        .header("anthropic-beta", beta_header)
                        .header("content-type", "application/json")
                        .json(request_json);
                    if streaming {
                        request.timeout(STREAM_TIMEOUT)
                    } else {
                        request
                    }
                };
                let response = if streaming {
                    builder().send().await?
                } else {
                    send_with_retry(&self.client, builder, MAX_RETRIES).await?
                };
                Ok(ProviderHttpResponse::Direct(response))
            }
            ProviderAuth::ClientEgress { kind } => {
                let headers = vec![
                    ("anthropic-version".to_string(), "2023-06-01".to_string()),
                    ("anthropic-beta".to_string(), beta_header.to_string()),
                    ("content-type".to_string(), "application/json".to_string()),
                ];
                let body = serde_json::to_vec(request_json).map_err(CallerError::Json)?;
                crate::credential_egress::fetch(kind, "POST", &url, headers, body)
                    .await
                    .map(ProviderHttpResponse::Egress)
                    .map_err(CallerError::Provider)
            }
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    fn request_snapshot(
        &self,
        messages: &[Message],
        stream: bool,
    ) -> Result<(String, serde_json::Value), CallerError> {
        let (system, api_messages) = build_anthropic_messages(messages);

        let mut tools_vec: Vec<serde_json::Value> = Vec::new();
        if self.use_tools {
            let defs = self.tools();
            tools_vec.extend(defs.iter().map(|t| t.to_anthropic()));
        }
        if self.cu_enabled {
            if let Some((w, h)) = self.cu_display {
                tools_vec.push(serde_json::json!({
                    "type": "computer_20251124",
                    "name": "computer",
                    "display_width_px": w,
                    "display_height_px": h
                }));
            }
        }
        let tools = if tools_vec.is_empty() {
            None
        } else {
            Some(tools_vec)
        };

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: self.max_output_tokens,
            tools,
            stream,
        };
        Ok((
            "anthropic.messages.request.v1".to_string(),
            serde_json::to_value(&request).map_err(CallerError::Json)?,
        ))
    }

    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        let (system, api_messages) = build_anthropic_messages(messages);

        let mut tools_vec: Vec<serde_json::Value> = Vec::new();
        if self.use_tools {
            let defs = self.tools();
            tools_vec.extend(defs.iter().map(|t| t.to_anthropic()));
        }
        if self.cu_enabled {
            if let Some((w, h)) = self.cu_display {
                tools_vec.push(serde_json::json!({
                    "type": "computer_20251124",
                    "name": "computer",
                    "display_width_px": w,
                    "display_height_px": h
                }));
            }
        }
        let tools = if tools_vec.is_empty() {
            None
        } else {
            Some(tools_vec)
        };

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: self.max_output_tokens,
            tools,
            stream: false,
        };

        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;

        let beta_header = if self.cu_enabled {
            "prompt-caching-2024-07-31,computer-use-2025-11-24"
        } else {
            "prompt-caching-2024-07-31"
        };

        let response = self
            .post_messages(&request_json, beta_header, false)
            .await?;

        if !response.status_success() {
            let status = response.status_line();
            let body = response.body_text().await;
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let rate_limit_windows = response.anthropic_rate_limit_windows();
        let chat_response: AnthropicChatResponse = response.json().await?;

        // Extract text content, tool_use blocks, and CU blocks
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut cu_calls = Vec::new();

        for block in &chat_response.content {
            match block.content_type.as_deref() {
                Some("text") => {
                    if let Some(ref text) = block.text {
                        text_parts.push(text.clone());
                    }
                }
                Some("tool_use") => {
                    if let (Some(id), Some(name), Some(input)) =
                        (&block.id, &block.name, &block.input)
                    {
                        if name == "computer" && self.cu_enabled {
                            // Native CU tool call
                            if let Some(action) = parse_anthropic_cu_action(input) {
                                cu_calls.push(super::computer_use::CuToolCall {
                                    call_id: id.clone(),
                                    actions: vec![action],
                                    metadata: super::computer_use::CuCallMetadata::default(),
                                });
                            }
                        } else {
                            tool_calls.push(ToolCall {
                                id: id.clone(),
                                call_id: id.clone(),
                                name: name.clone(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            });
                        }
                    }
                }
                _ => {
                    // Legacy: text field without explicit type
                    if let Some(ref text) = block.text {
                        text_parts.push(text.clone());
                    }
                }
            }
        }

        let content = text_parts.join("");

        let mut usage = chat_response
            .usage
            .map(|u| u.to_token_usage())
            .unwrap_or_default();
        usage.rate_limit_windows = rate_limit_windows;

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output: None,
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> u64 {
        self.context_window
    }

    fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }

    fn use_tools(&self) -> bool {
        self.use_tools
    }

    fn cu_enabled(&self) -> bool {
        self.cu_enabled
    }

    fn set_cu_enabled(&mut self, enabled: bool) {
        self.cu_enabled = enabled;
    }

    fn cu_display(&self) -> Option<(u32, u32)> {
        self.cu_display
    }

    fn set_cu_display(&mut self, dims: (u32, u32)) {
        self.cu_display = Some(dims);
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            self.custom_tools
                .clone()
                .unwrap_or_else(crate::tools::all_tools)
        } else {
            vec![]
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ChatResponse, CallerError> {
        let (system, api_messages) = build_anthropic_messages(messages);

        let mut tools_vec: Vec<serde_json::Value> = Vec::new();
        if self.use_tools {
            let defs = self.tools();
            tools_vec.extend(defs.iter().map(|t| t.to_anthropic()));
        }
        if self.cu_enabled {
            if let Some((w, h)) = self.cu_display {
                tools_vec.push(serde_json::json!({
                    "type": "computer_20251124",
                    "name": "computer",
                    "display_width_px": w,
                    "display_height_px": h
                }));
            }
        }
        let tools = if tools_vec.is_empty() {
            None
        } else {
            Some(tools_vec)
        };

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: self.max_output_tokens,
            tools,
            stream: true,
        };
        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;

        let beta_header = if self.cu_enabled {
            "prompt-caching-2024-07-31,computer-use-2025-11-24"
        } else {
            "prompt-caching-2024-07-31"
        };

        let response = self.post_messages(&request_json, beta_header, true).await?;

        if !response.status_success() {
            let status = response.status_line();
            let body = response.body_text().await;
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        // Parse SSE stream
        let mut text_parts = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut cu_calls: Vec<super::computer_use::CuToolCall> = Vec::new();
        let mut current_tool_json = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut usage = TokenUsage {
            rate_limit_windows: response.anthropic_rate_limit_windows(),
            ..Default::default()
        };
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| CallerError::Provider(format!("Stream error: {}", e)))?;
            let chunk_str = String::from_utf8_lossy(&chunk);

            line_buf.push_str(&chunk_str);

            // Process complete lines
            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if let Some(("data", data)) = parse_sse_line(&line) {
                    if data == "[DONE]" {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match event_type {
                            "content_block_start" => {
                                if let Some(cb) = event.get("content_block") {
                                    let cb_type =
                                        cb.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    if cb_type == "tool_use" {
                                        current_tool_id = cb
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        current_tool_name = cb
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        current_tool_json.clear();
                                    }
                                }
                            }
                            "content_block_delta" => {
                                if let Some(delta) = event.get("delta") {
                                    let delta_type =
                                        delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    match delta_type {
                                        "text_delta" => {
                                            if let Some(text) =
                                                delta.get("text").and_then(|t| t.as_str())
                                            {
                                                text_parts.push(text.to_string());
                                                on_event(StreamEvent::Delta(text.to_string()));
                                            }
                                        }
                                        "input_json_delta" => {
                                            if let Some(json) =
                                                delta.get("partial_json").and_then(|t| t.as_str())
                                            {
                                                current_tool_json.push_str(json);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            "content_block_stop" => {
                                if !current_tool_id.is_empty() {
                                    if current_tool_name == "computer" && self.cu_enabled {
                                        if let Ok(input) = serde_json::from_str::<serde_json::Value>(
                                            &current_tool_json,
                                        ) {
                                            if let Some(action) = parse_anthropic_cu_action(&input)
                                            {
                                                cu_calls.push(super::computer_use::CuToolCall {
                                                    call_id: current_tool_id.clone(),
                                                    actions: vec![action],
                                                    metadata: super::computer_use::CuCallMetadata::default(),
                                                });
                                            }
                                        }
                                    } else {
                                        tool_calls.push(ToolCall {
                                            id: current_tool_id.clone(),
                                            call_id: current_tool_id.clone(),
                                            name: current_tool_name.clone(),
                                            arguments: current_tool_json.clone(),
                                        });
                                    }
                                    current_tool_id.clear();
                                    current_tool_name.clear();
                                    current_tool_json.clear();
                                }
                            }
                            "message_delta" => {
                                if let Some(u) = event.get("usage") {
                                    let output = u
                                        .get("output_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    usage.completion_tokens = output;
                                }
                            }
                            "message_start" => {
                                if let Some(msg) = event.get("message") {
                                    if let Some(parsed) = msg
                                        .get("usage")
                                        .cloned()
                                        .and_then(|u| {
                                            serde_json::from_value::<AnthropicUsage>(u).ok()
                                        })
                                    {
                                        // Prompt-side counters only; output
                                        // arrives later via message_delta.
                                        let prompt_side = parsed.to_token_usage();
                                        usage.prompt_tokens = prompt_side.prompt_tokens;
                                        usage.cached_tokens = prompt_side.cached_tokens;
                                        usage.cache_creation_tokens =
                                            prompt_side.cache_creation_tokens;
                                        usage.cache_ttl_seconds = prompt_side.cache_ttl_seconds;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
        let content = text_parts.join("");
        let response = ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output: None,
        };
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// Build Anthropic API messages from our message format (shared between streaming and non-streaming).
/// Parse an Anthropic computer tool_use input into a CuAction.
fn parse_anthropic_cu_action(input: &serde_json::Value) -> Option<super::computer_use::CuAction> {
    use super::computer_use::*;

    let action = input.get("action")?.as_str()?;
    let coord = || -> Option<(i32, i32)> {
        let arr = input.get("coordinate")?.as_array()?;
        Some((arr.first()?.as_i64()? as i32, arr.get(1)?.as_i64()? as i32))
    };

    match action {
        "screenshot" => Some(CuAction::Screenshot),
        "left_click" => {
            let (x, y) = coord()?;
            Some(CuAction::Click {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "right_click" => {
            let (x, y) = coord()?;
            Some(CuAction::Click {
                x,
                y,
                button: MouseButton::Right,
            })
        }
        "middle_click" => {
            let (x, y) = coord()?;
            Some(CuAction::Click {
                x,
                y,
                button: MouseButton::Middle,
            })
        }
        "double_click" => {
            let (x, y) = coord()?;
            Some(CuAction::DoubleClick {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "triple_click" => {
            let (x, y) = coord()?;
            Some(CuAction::TripleClick {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "left_mouse_down" => {
            let (x, y) = coord()?;
            Some(CuAction::MouseDown {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "left_mouse_up" => {
            let (x, y) = coord()?;
            Some(CuAction::MouseUp {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "type" => {
            let text = input.get("text")?.as_str()?.to_string();
            Some(CuAction::Type { text })
        }
        "key" => {
            let key = input.get("text")?.as_str()?.to_string();
            Some(CuAction::Key { key })
        }
        "hold_key" => {
            let key = input.get("text")?.as_str()?.to_string();
            // Anthropic sends duration in seconds (fractional allowed).
            let ms = anthropic_duration_ms(input, 1000);
            Some(CuAction::HoldKey { key, ms })
        }
        "zoom" => {
            // region is [x0, y0, x1, y1] in screenshot coordinates.
            let region = input.get("region")?.as_array()?;
            let x0 = region.first()?.as_i64()? as i32;
            let y0 = region.get(1)?.as_i64()? as i32;
            let x1 = region.get(2)?.as_i64()? as i32;
            let y1 = region.get(3)?.as_i64()? as i32;
            Some(CuAction::Zoom {
                x: x0.min(x1),
                y: y0.min(y1),
                width: (x1 - x0).unsigned_abs(),
                height: (y1 - y0).unsigned_abs(),
            })
        }
        "mouse_move" => {
            let (x, y) = coord()?;
            Some(CuAction::MoveMouse { x, y })
        }
        "scroll" => {
            let (x, y) = coord()?;
            let dir_str = input.get("scroll_direction")?.as_str()?;
            let direction = match dir_str {
                "up" => ScrollDirection::Up,
                "down" => ScrollDirection::Down,
                "left" => ScrollDirection::Left,
                "right" => ScrollDirection::Right,
                _ => return None,
            };
            let amount = input
                .get("scroll_amount")
                .and_then(|v| v.as_i64())
                .unwrap_or(3) as i32;
            Some(CuAction::Scroll {
                x,
                y,
                direction,
                amount,
            })
        }
        "left_click_drag" => {
            let (sx, sy) = coord()?;
            let end = input.get("end_coordinate")?.as_array()?;
            let ex = end.first()?.as_i64()? as i32;
            let ey = end.get(1)?.as_i64()? as i32;
            Some(CuAction::Drag {
                start_x: sx,
                start_y: sy,
                end_x: ex,
                end_y: ey,
            })
        }
        "wait" => {
            // Anthropic sends duration in seconds (the previous read treated
            // it as milliseconds, turning a 2-second wait into 2 ms).
            let ms = anthropic_duration_ms(input, 1000);
            Some(CuAction::Wait { ms })
        }
        _ => None,
    }
}

/// Convert an Anthropic CU `duration` field (seconds, fractional allowed)
/// into clamped milliseconds.
fn anthropic_duration_ms(input: &serde_json::Value, default_ms: u64) -> u64 {
    match input.get("duration").and_then(|v| v.as_f64()) {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            ((seconds * 1000.0).round() as u64).min(30_000)
        }
        _ => default_ms,
    }
}

fn build_anthropic_messages(messages: &[Message]) -> (serde_json::Value, Vec<AnthropicMessage>) {
    let system_text = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let system = serde_json::json!([{
        "type": "text",
        "text": system_text,
        "cache_control": {"type": "ephemeral"}
    }]);

    let mut api_messages: Vec<AnthropicMessage> = Vec::new();
    for m in messages {
        if m.role == "system" {
            continue;
        }
        if m.role == "tool" {
            if let Some(ref call_id) = m.tool_call_id {
                let tool_content = if let Some(ref images) = m.images {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": m.content,
                    })];
                    for img in images {
                        parts.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": img.media_type,
                                "data": img.data,
                            }
                        }));
                    }
                    serde_json::Value::Array(parts)
                } else {
                    serde_json::Value::String(m.content.clone())
                };
                let block = serde_json::json!([{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": tool_content,
                }]);
                api_messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: block,
                });
                continue;
            }
        }
        if m.role == "assistant" {
            if let Some(ref tcs) = m.tool_calls {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": m.content,
                    }));
                }
                for tc in tcs {
                    let input: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": input,
                    }));
                }
                api_messages.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::Value::Array(blocks),
                });
                continue;
            }
        }
        if m.role == "user" || m.role == "assistant" {
            let content = if m.role == "user" {
                if let Some(ref images) = m.images {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": m.content,
                    })];
                    for img in images {
                        parts.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": img.media_type,
                                "data": img.data,
                            }
                        }));
                    }
                    serde_json::Value::Array(parts)
                } else {
                    serde_json::Value::String(m.content.clone())
                }
            } else {
                serde_json::Value::String(m.content.clone())
            };
            api_messages.push(AnthropicMessage {
                role: m.role.clone(),
                content,
            });
        }
    }
    (system, api_messages)
}

// --- Gemini ---

pub struct GeminiProvider {
    client: Client,
    auth: ProviderAuth,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    use_tools: bool,
    custom_tools: Option<Vec<ToolDefinition>>,
    endpoint: String,
    /// When true, include native computer-use tool in API requests.
    pub cu_enabled: bool,
    /// Display dimensions for CU (width, height).
    pub cu_display: Option<(u32, u32)>,
}

impl GeminiProvider {
    pub fn new(
        api_key: impl Into<ProviderAuth>,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let use_tools = resolve_use_tools();
        let endpoint = env::var("GEMINI_ENDPOINT")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
        Self {
            client: api_client(),
            auth: api_key.into(),
            model,
            context_window,
            max_output_tokens,
            use_tools,
            custom_tools: None,
            endpoint,
            cu_enabled: false,
            cu_display: None,
        }
    }

    /// Create a provider with native tool calling explicitly disabled.
    pub fn new_plain(
        api_key: impl Into<ProviderAuth>,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let endpoint = env::var("GEMINI_ENDPOINT")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
        Self {
            client: api_client(),
            auth: api_key.into(),
            model,
            context_window,
            max_output_tokens,
            use_tools: false,
            custom_tools: None,
            endpoint,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_with_tools(
        api_key: impl Into<ProviderAuth>,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        let endpoint = env::var("GEMINI_ENDPOINT")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
        Self {
            client: api_client(),
            auth: api_key.into(),
            model,
            context_window,
            max_output_tokens,
            use_tools: true,
            custom_tools: Some(tools),
            endpoint,
            cu_enabled: false,
            cu_display: None,
        }
    }

    /// A tool-less instance forced through the client-egress relay —
    /// the probe path; normal selection converts availability into
    /// `ProviderAuth::ClientEgress` instead.
    pub fn new_client_egress(model: String, context_window: u64, max_output_tokens: u64) -> Self {
        let mut provider = Self::new_plain(String::new(), model, context_window, max_output_tokens);
        provider.auth = ProviderAuth::ClientEgress {
            kind: crate::credential_egress::KIND_GEMINI,
        };
        provider
    }

    /// POST a generateContent-family request through whichever auth path
    /// this instance carries. Auth never rides an egress request — the
    /// relay attaches `x-goog-api-key` from the vault.
    async fn post_generate(
        &self,
        url: &str,
        request_body: &serde_json::Value,
        streaming: bool,
    ) -> Result<ProviderHttpResponse, CallerError> {
        match &self.auth {
            ProviderAuth::Key(api_key) => {
                let builder = || {
                    let request = self
                        .client
                        .post(url)
                        .header("content-type", "application/json")
                        .header("x-goog-api-key", api_key)
                        .json(request_body);
                    if streaming {
                        request.timeout(STREAM_TIMEOUT)
                    } else {
                        request
                    }
                };
                let response = if streaming {
                    builder().send().await?
                } else {
                    send_with_retry(&self.client, builder, MAX_RETRIES).await?
                };
                Ok(ProviderHttpResponse::Direct(response))
            }
            ProviderAuth::ClientEgress { kind } => {
                let headers = vec![("content-type".to_string(), "application/json".to_string())];
                let body = serde_json::to_vec(request_body).map_err(CallerError::Json)?;
                crate::credential_egress::fetch(kind, "POST", url, headers, body)
                    .await
                    .map(ProviderHttpResponse::Egress)
                    .map_err(CallerError::Provider)
            }
        }
    }
}

/// Map our role names to Gemini roles.
fn gemini_role(role: &str) -> &str {
    match role {
        "assistant" => "model",
        "user" | "developer" | "tool" => "user",
        _ => "user",
    }
}

#[async_trait]
impl ChatProvider for GeminiProvider {
    fn request_snapshot(
        &self,
        messages: &[Message],
        stream: bool,
    ) -> Result<(String, serde_json::Value), CallerError> {
        let _ = stream;
        let (system_text, _contents, mut request_body) = build_gemini_request_parts(messages, self);

        if let Some(ref sys) = system_text {
            request_body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": sys}]
            });
        }

        Ok((
            "gemini.generate-content.request.v1".to_string(),
            request_body,
        ))
    }

    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        let (system_text, _contents, mut request_body) = build_gemini_request_parts(messages, self);

        if let Some(ref sys) = system_text {
            request_body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": sys}]
            });
        }

        // Note: Gemini API uses implicit context caching. Requests with the same
        // prefix are automatically cached server-side. No explicit API changes needed.
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.endpoint, self.model
        );

        let response = self.post_generate(&url, &request_body, false).await?;

        if !response.status_success() {
            let status = response.status_line();
            let body = response.body_text().await;
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let resp: serde_json::Value = response.json().await?;

        // Extract content from candidates[0].content.parts[]
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut cu_calls = Vec::new();

        if let Some(parts) = resp
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_val = fc.get("args").cloned().unwrap_or(serde_json::json!({}));

                    // Check if this is a CU function call
                    if self.cu_enabled && GEMINI_CU_FUNCTIONS.contains(&name.as_str()) {
                        let (dw, dh) = self.cu_display.unwrap_or((1440, 900));
                        if let Some(action) = parse_gemini_cu_action(&name, &args_val, dw, dh) {
                            let id = format!("gemini_cu_{}", cu_calls.len());
                            cu_calls.push(super::computer_use::CuToolCall {
                                call_id: id,
                                actions: vec![action],
                                metadata: super::computer_use::CuCallMetadata::default(),
                            });
                        }
                    } else {
                        let args =
                            serde_json::to_string(&args_val).unwrap_or_else(|_| "{}".to_string());
                        let id = format!("gemini_call_{}", tool_calls.len());
                        tool_calls.push(ToolCall {
                            id: id.clone(),
                            call_id: id,
                            name,
                            arguments: args,
                        });
                    }
                }
            }
        }

        let content = text_parts.join("");

        // Extract usage
        let usage = resp
            .get("usageMetadata")
            .map(|u| {
                let prompt = u
                    .get("promptTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let completion = u
                    .get("candidatesTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let total = u
                    .get("totalTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(prompt + completion);
                let cached = u
                    .get("cachedContentTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                TokenUsage {
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    total_tokens: total,
                    cached_tokens: cached,
                    ..Default::default()
                }
            })
            .unwrap_or_default();

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output: None,
        })
    }

    fn name(&self) -> &str {
        "gemini"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> u64 {
        self.context_window
    }

    fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }

    fn use_tools(&self) -> bool {
        self.use_tools
    }

    fn cu_enabled(&self) -> bool {
        self.cu_enabled
    }

    fn set_cu_enabled(&mut self, enabled: bool) {
        self.cu_enabled = enabled;
    }

    fn cu_display(&self) -> Option<(u32, u32)> {
        self.cu_display
    }

    fn set_cu_display(&mut self, dims: (u32, u32)) {
        self.cu_display = Some(dims);
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            self.custom_tools
                .clone()
                .unwrap_or_else(crate::tools::all_tools)
        } else {
            vec![]
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ChatResponse, CallerError> {
        let (system_text, contents, request_body_base) = build_gemini_request_parts(messages, self);

        let mut request_body = request_body_base;
        if let Some(ref sys) = system_text {
            request_body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": sys}]
            });
        }

        // Use streamGenerateContent endpoint
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.endpoint, self.model
        );

        let response = self.post_generate(&url, &request_body, true).await?;

        if !response.status_success() {
            let status = response.status_line();
            let body = response.body_text().await;
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let mut text_parts = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut cu_calls: Vec<super::computer_use::CuToolCall> = Vec::new();
        let mut raw_model_parts: Vec<serde_json::Value> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| CallerError::Provider(format!("Stream error: {}", e)))?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            line_buf.push_str(&chunk_str);

            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                // Gemini streaming with alt=sse returns SSE format
                let data = if let Some(("data", d)) = parse_sse_line(&line) {
                    d
                } else {
                    continue;
                };

                if let Ok(resp) = serde_json::from_str::<serde_json::Value>(data) {
                    // Extract text and function calls from candidates
                    if let Some(parts) = resp
                        .pointer("/candidates/0/content/parts")
                        .and_then(|p| p.as_array())
                    {
                        for part in parts {
                            // Capture raw parts for verbatim echo-back (preserves thoughtSignature)
                            raw_model_parts.push(part.clone());

                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(text.to_string());
                                on_event(StreamEvent::Delta(text.to_string()));
                            }
                            if let Some(fc) = part.get("functionCall") {
                                let name = fc
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args_val =
                                    fc.get("args").cloned().unwrap_or(serde_json::json!({}));

                                if self.cu_enabled && GEMINI_CU_FUNCTIONS.contains(&name.as_str()) {
                                    let (dw, dh) = self.cu_display.unwrap_or((1440, 900));
                                    if let Some(action) =
                                        parse_gemini_cu_action(&name, &args_val, dw, dh)
                                    {
                                        let id = format!("gemini_cu_{}", cu_calls.len());
                                        cu_calls.push(super::computer_use::CuToolCall {
                                            call_id: id,
                                            actions: vec![action],
                                            metadata: super::computer_use::CuCallMetadata::default(
                                            ),
                                        });
                                    }
                                } else {
                                    let args = serde_json::to_string(&args_val)
                                        .unwrap_or_else(|_| "{}".to_string());
                                    let id = format!("gemini_call_{}", tool_calls.len());
                                    tool_calls.push(ToolCall {
                                        id: id.clone(),
                                        call_id: id,
                                        name,
                                        arguments: args,
                                    });
                                }
                            }
                        }
                    }

                    // Extract usage from the last chunk
                    if let Some(u) = resp.get("usageMetadata") {
                        let prompt = u
                            .get("promptTokenCount")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let completion = u
                            .get("candidatesTokenCount")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let total = u
                            .get("totalTokenCount")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(prompt + completion);
                        let cached = u
                            .get("cachedContentTokenCount")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        usage = TokenUsage {
                            prompt_tokens: prompt,
                            completion_tokens: completion,
                            total_tokens: total,
                            cached_tokens: cached,
                            ..Default::default()
                        };
                    }
                }
            }
        }

        let content = text_parts.join("");
        let _ = (contents, system_text); // consumed above
                                         // Store raw parts for echo-back (preserves thoughtSignature for Gemini CU)
        let raw_output = if !raw_model_parts.is_empty() {
            Some(raw_model_parts)
        } else {
            None
        };
        let response = ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output,
        };
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// Build Gemini request parts (shared between streaming and non-streaming).
fn build_gemini_request_parts(
    messages: &[Message],
    provider: &GeminiProvider,
) -> (Option<String>, Vec<serde_json::Value>, serde_json::Value) {
    let system_text = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());

    let mut contents: Vec<serde_json::Value> = Vec::new();
    for m in messages {
        if m.role == "system" {
            continue;
        }
        let role = gemini_role(&m.role);
        if m.role == "tool" {
            if let (Some(ref _call_id), Some(ref tool_name)) = (&m.tool_call_id, &m.tool_name) {
                if m.is_cu_result {
                    // CU result: screenshot goes INSIDE functionResponse.parts (not as sibling)
                    let response_val = serde_json::json!({
                        "output": m.content,
                        "url": "desktop://local",
                    });
                    let mut fr = serde_json::json!({
                        "functionResponse": {
                            "name": tool_name,
                            "response": response_val,
                        }
                    });
                    if let Some(ref images) = m.images {
                        let fr_parts: Vec<serde_json::Value> = images
                            .iter()
                            .map(|img| {
                                serde_json::json!({
                                    "inlineData": {
                                        "mimeType": img.media_type,
                                        "data": img.data,
                                    }
                                })
                            })
                            .collect();
                        if !fr_parts.is_empty() {
                            fr["functionResponse"]["parts"] = serde_json::Value::Array(fr_parts);
                        }
                    }
                    contents.push(serde_json::json!({
                        "role": role,
                        "parts": [fr],
                    }));
                } else {
                    let response_val: serde_json::Value = serde_json::from_str(&m.content)
                        .unwrap_or(serde_json::json!({
                            "output": m.content,
                        }));
                    contents.push(serde_json::json!({
                        "role": role,
                        "parts": [{
                            "functionResponse": {
                                "name": tool_name,
                                "response": response_val,
                            }
                        }]
                    }));
                    if let Some(ref images) = m.images {
                        let mut parts = vec![serde_json::json!({
                            "text": "Screenshot from the previous tool call:",
                        })];
                        for img in images {
                            parts.push(serde_json::json!({
                                "inlineData": {
                                    "mimeType": img.media_type,
                                    "data": img.data,
                                }
                            }));
                        }
                        contents.push(serde_json::json!({
                            "role": "user",
                            "parts": parts,
                        }));
                    }
                }
                continue;
            }
        }
        if m.role == "assistant" {
            if let Some(ref tcs) = m.tool_calls {
                // Use raw_output if available (preserves thoughtSignature for Gemini CU)
                if let Some(ref raw) = m.raw_output {
                    contents.push(serde_json::json!({
                        "role": role,
                        "parts": raw,
                    }));
                    continue;
                }
                let mut parts = Vec::new();
                if !m.content.is_empty() {
                    parts.push(serde_json::json!({"text": m.content}));
                }
                for tc in tcs {
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));
                    parts.push(serde_json::json!({
                        "functionCall": {
                            "name": tc.name,
                            "args": args,
                        }
                    }));
                }
                contents.push(serde_json::json!({
                    "role": role,
                    "parts": parts,
                }));
                continue;
            }
        }
        if m.role == "user" {
            if let Some(ref images) = m.images {
                let mut parts = vec![serde_json::json!({"text": m.content})];
                for img in images {
                    parts.push(serde_json::json!({
                        "inlineData": {
                            "mimeType": img.media_type,
                            "data": img.data,
                        }
                    }));
                }
                contents.push(serde_json::json!({
                    "role": role,
                    "parts": parts,
                }));
                continue;
            }
        }
        contents.push(serde_json::json!({
            "role": role,
            "parts": [{"text": m.content}]
        }));
    }

    let mut request_body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": provider.max_output_tokens,
        }
    });

    let has_func_tools = provider.use_tools;
    let has_cu = provider.cu_enabled;
    if has_func_tools || has_cu {
        let mut tools_arr = Vec::new();
        if has_func_tools {
            let defs = provider.tools();
            let func_decls: Vec<serde_json::Value> = defs.iter().map(|t| t.to_gemini()).collect();
            tools_arr.push(serde_json::json!({
                "functionDeclarations": func_decls,
            }));
        }
        if has_cu {
            // Gemini v1beta only supports ENVIRONMENT_BROWSER for computer_use.
            // No display_size field is available — the model infers dimensions
            // from screenshot resolution and uses normalized 0-999 coordinates.
            tools_arr.push(serde_json::json!({
                "computer_use": {
                    "environment": "ENVIRONMENT_BROWSER"
                }
            }));
        }
        request_body["tools"] = serde_json::Value::Array(tools_arr);
    }

    (system_text, contents, request_body)
}

/// CU function names used by Gemini's computer_use tool.
const GEMINI_CU_FUNCTIONS: &[&str] = &[
    "click_at",
    "type_text_at",
    "hover_at",
    "scroll_document",
    "scroll_at",
    "key_combination",
    "navigate",
    "go_back",
    "go_forward",
    "search",
    "open_web_browser",
    "wait_5_seconds",
    "drag_and_drop",
];

/// Parse a Gemini CU function call into a CuAction.
/// Gemini uses 0-999 normalized coordinates; they are converted to pixels here.
fn parse_gemini_cu_action(
    name: &str,
    args: &serde_json::Value,
    display_width: u32,
    display_height: u32,
) -> Option<super::computer_use::CuAction> {
    use super::computer_use::*;

    let coord = |xk: &str, yk: &str| -> Option<(i32, i32)> {
        let nx = args.get(xk)?.as_i64()? as i32;
        let ny = args.get(yk)?.as_i64()? as i32;
        Some(normalized_to_pixels(nx, ny, display_width, display_height))
    };

    match name {
        "click_at" => {
            let (x, y) = coord("x", "y")?;
            Some(CuAction::Click {
                x,
                y,
                button: MouseButton::Left,
            })
        }
        "type_text_at" => {
            let (x, y) = coord("x", "y")?;
            let text = args.get("text")?.as_str()?.to_string();
            let press_enter = args
                .get("press_enter")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Click to focus, then type
            // We return just the Type action; the click is handled by the executor
            // Actually, return Click + Type as separate actions is complex.
            // For simplicity, just return Type and let caller handle focus.
            let mut result_text = text;
            if press_enter {
                result_text.push('\n');
            }
            // First click to position, then type. We'll do this as a Click action
            // followed by a Type action at the agent loop level.
            // For now, just return Type — the model already positions via click_at.
            let _ = (x, y); // coordinates ignored; model handles focus separately
            Some(CuAction::Type { text: result_text })
        }
        "hover_at" => {
            let (x, y) = coord("x", "y")?;
            Some(CuAction::MoveMouse { x, y })
        }
        "scroll_document" | "scroll_at" => {
            let dir_str = args.get("direction")?.as_str()?;
            let direction = match dir_str {
                "up" => ScrollDirection::Up,
                "down" => ScrollDirection::Down,
                "left" => ScrollDirection::Left,
                "right" => ScrollDirection::Right,
                _ => return None,
            };
            let amount = args.get("magnitude").and_then(|v| v.as_i64()).unwrap_or(3) as i32;
            let (x, y) = if name == "scroll_at" {
                coord("x", "y").unwrap_or((display_width as i32 / 2, display_height as i32 / 2))
            } else {
                (display_width as i32 / 2, display_height as i32 / 2)
            };
            Some(CuAction::Scroll {
                x,
                y,
                direction,
                amount,
            })
        }
        "key_combination" => {
            let keys = args.get("keys")?.as_str()?.to_string();
            Some(CuAction::Key { key: keys })
        }
        "wait_5_seconds" => Some(CuAction::Wait { ms: 5000 }),
        "drag_and_drop" => {
            let (sx, sy) = coord("x", "y")?;
            let (ex, ey) = coord("destination_x", "destination_y")?;
            Some(CuAction::Drag {
                start_x: sx,
                start_y: sy,
                end_x: ex,
                end_y: ey,
            })
        }
        // Browser-like navigation actions — mapped to keyboard shortcuts / xdg-open
        "navigate" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            // Type the URL into the address bar via xdg-open (best-effort)
            Some(CuAction::Key {
                key: format!("xdg-open {}", url),
            })
        }
        "open_web_browser" => {
            // No-op screenshot — the model wants to see the screen
            Some(CuAction::Screenshot)
        }
        "go_back" => Some(CuAction::Key {
            key: "alt+Left".to_string(),
        }),
        "go_forward" => Some(CuAction::Key {
            key: "alt+Right".to_string(),
        }),
        "search" => Some(CuAction::Key {
            key: "ctrl+l".to_string(),
        }),
        _ => None,
    }
}

// --- Provider selection ---

fn default_context_window(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-5") => 1_000_000,
        m if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") => 200_000,
        m if m.contains("claude") => 200_000,
        m if m.starts_with("gemini") => 1_048_576,
        _ => 200_000,
    }
}

fn default_max_output_tokens(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-5") => 128_000,
        m if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") => 100_000,
        m if m.contains("claude") => 8_192,
        m if m.starts_with("gemini") => 65_536,
        _ => 16_384,
    }
}

fn resolve_context_window(model: &str) -> u64 {
    env::var("MODEL_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| default_context_window(model))
}

fn resolve_max_output_tokens(model: &str) -> u64 {
    env::var("MAX_OUTPUT_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| default_max_output_tokens(model))
}

fn supports_structured_output(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with("o3") || model.starts_with("o4")
}

fn resolve_structured_output(model: &str) -> bool {
    env::var("STRUCTURED_OUTPUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| supports_structured_output(model))
}

fn supports_reasoning(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with("o3") || model.starts_with("o4")
}

fn resolve_reasoning(model: &str) -> Option<ReasoningConfig> {
    if !supports_reasoning(model) {
        return None;
    }
    let effort = env::var("REASONING_EFFORT")
        .ok()
        .and_then(|v| {
            let v = v.trim().to_string();
            if v.is_empty() || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(v)
            }
        })
        .unwrap_or_else(|| "high".to_string());
    let summary = env::var("REASONING_SUMMARY")
        .ok()
        .and_then(|s| {
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
        .or_else(|| Some("auto".to_string()));
    Some(ReasoningConfig { effort, summary })
}

/// Resolve whether native tool calling should be enabled.
/// Checks `USE_NATIVE_TOOLS` env var, defaults to `true`.
pub fn resolve_use_tools() -> bool {
    env::var("USE_NATIVE_TOOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(true)
}

/// Mask API keys in error messages to prevent accidental leakage.
pub(crate) fn mask_api_keys(s: &str) -> String {
    static API_KEY_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Match sk- (OpenAI), key- (Anthropic), AIza (Google) prefixed keys
        // Capture first 14 chars (prefix + 10) then mask the rest
        regex::Regex::new(r"(sk-[A-Za-z0-9_-]{10})[A-Za-z0-9_-]+|(key-[A-Za-z0-9_-]{10})[A-Za-z0-9_-]+|(AIzaSy[A-Za-z0-9_-]{6})[A-Za-z0-9_-]+").unwrap()
    });
    API_KEY_RE
        .replace_all(s, |caps: &regex::Captures| {
            if let Some(m) = caps.get(1) {
                format!("{}***", m.as_str())
            } else if let Some(m) = caps.get(2) {
                format!("{}***", m.as_str())
            } else if let Some(m) = caps.get(3) {
                format!("{}***", m.as_str())
            } else {
                caps[0].to_string()
            }
        })
        .to_string()
}

pub fn select_provider() -> Result<Box<dyn ChatProvider>, CallerError> {
    select_provider_with_project_keys(&ProjectEnvKeys::none())
}

/// [`select_provider`] with the session project's `.env` joining key
/// resolution as the last layer (see [`ProjectEnvKeys`] for what is and
/// isn't honored from it). This is what makes "I picked this project in
/// the dashboard and its `.env` has keys" actually work for native
/// sessions on an otherwise unfueled daemon.
pub fn select_provider_for_project(
    project_root: Option<&std::path::Path>,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    let keys = project_root
        .map(ProjectEnvKeys::load)
        .unwrap_or_default();
    select_provider_with_project_keys(&keys)
}

fn select_provider_with_project_keys(
    project_keys: &ProjectEnvKeys,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    let openai_key = crate::credential_leases::provider_api_key("OPENAI_API_KEY")
        .or_else(|| project_keys.get("OPENAI_API_KEY"));
    let anthropic_key = provider_auth_for(
        "ANTHROPIC_API_KEY",
        crate::credential_egress::KIND_ANTHROPIC,
    )
    .or_else(|| project_keys.get("ANTHROPIC_API_KEY").map(ProviderAuth::Key));
    let gemini_key = provider_auth_for("GEMINI_API_KEY", crate::credential_egress::KIND_GEMINI)
        .or_else(|| project_keys.get("GEMINI_API_KEY").map(ProviderAuth::Key));

    let preferred = env::var("PROVIDER").ok();

    // Keyless scripted provider for headless E2E and demos. Never
    // auto-selected — only an explicit PROVIDER=mock opts in, and the
    // script path must be supplied via INTENDANT_MOCK_SCRIPT.
    if preferred.as_deref() == Some("mock") {
        return Ok(Box::new(crate::provider_mock::MockProvider::from_env()?));
    }

    // Explicit Gemini selection
    if preferred.as_deref() == Some("gemini") {
        let key = gemini_key.ok_or_else(|| {
            CallerError::Config("PROVIDER=gemini but no GEMINI_API_KEY found.".to_string())
        })?;
        let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gemini-2.5-pro".to_string());
        let ctx = resolve_context_window(&model);
        let max_out = resolve_max_output_tokens(&model);
        return Ok(Box::new(GeminiProvider::new(key, model, ctx, max_out)));
    }

    match (openai_key, anthropic_key, preferred.as_deref()) {
        // Both available, check PROVIDER preference
        (Some(oai), Some(ant), Some("anthropic")) => {
            let _ = oai;
            let model =
                env::var("MODEL_NAME").unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new(ant, model, ctx, max_out)))
        }
        (Some(oai), Some(_ant), Some("openai")) | (Some(oai), Some(_ant), None) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-5.5".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(oai, model, ctx, max_out)))
        }
        (Some(oai), None, _) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-5.5".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(oai, model, ctx, max_out)))
        }
        (None, Some(ant), _) => {
            let model =
                env::var("MODEL_NAME").unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new(ant, model, ctx, max_out)))
        }
        // Only Gemini key available (no explicit PROVIDER)
        (None, None, _) if gemini_key.is_some() => {
            let key = gemini_key.unwrap();
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gemini-2.5-pro".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(GeminiProvider::new(key, model, ctx, max_out)))
        }
        (Some(_oai), Some(_ant), Some(other)) => Err(CallerError::Config(format!(
            "Unknown PROVIDER value: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        (None, None, _) => Err(CallerError::Unfueled(unfueled_error_text(project_keys))),
    }
}

/// The daemon is unfueled: no leased credential, no browser relay, no
/// local key. Name the places that were actually consulted — under a
/// daemon, "your project root" would read as the project a session was
/// created with, which is only consulted for the whitelisted keys in
/// [`ProjectEnvKeys`] — and point at the remediations that work without a
/// restart. When a lease recently expired, say so — "reconnect a fueling
/// session" is the fix, not editing .env. The opening sentence is stable:
/// automation greps stderr for it.
fn unfueled_error_text(project_keys: &ProjectEnvKeys) -> String {
    let mut text = String::from(
        "No API key found. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY.",
    );
    let mut checked: Vec<String> =
        vec!["credential leases and browser relays (none active)".to_string()];
    if let Some(report) = ENV_SEARCH.get() {
        match &report.cwd_env {
            Some(path) => checked.push(format!("{} (loaded at startup)", path.display())),
            None => checked.push("no .env found from the startup directory upward".to_string()),
        }
        for entry in [&report.project_env, &report.global_env] {
            if let Some((path, loaded)) = entry {
                // Skip duplicates: the walk-up and the project root often
                // resolve to the same file.
                if report.cwd_env.as_deref() == Some(path.as_path()) {
                    continue;
                }
                checked.push(format!(
                    "{} ({})",
                    path.display(),
                    if *loaded { "loaded at startup" } else { "missing" }
                ));
            }
        }
    }
    if let Some(path) = &project_keys.env_path {
        checked.push(format!(
            "session project {} ({})",
            path.display(),
            if project_keys.file_present {
                "no provider keys"
            } else {
                "missing"
            }
        ));
    }
    text.push_str(&format!(" Checked: {}.", checked.join("; ")));
    text.push_str(
        " Fix: Dashboard \u{2192} Settings \u{2192} API Keys (applies immediately, no restart), \
         add the key to ~/.config/intendant/.env, or grant a credential lease from your vault.",
    );
    match crate::credential_leases::expired_lease_note() {
        Some(note) => format!("Unfueled: {note}. {text}"),
        None => text,
    }
}

/// Like `select_provider()` but accepts explicit provider/model overrides
/// instead of reading from the primary `PROVIDER`/`MODEL_NAME` env vars.
/// Falls back to env-based API key resolution.
#[allow(dead_code)]
pub fn select_provider_with_overrides(
    provider_name: Option<&str>,
    model_name: Option<&str>,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    // Also check PRESENCE_PROVIDER / PRESENCE_MODEL env vars as secondary fallback
    let provider_str = provider_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_PROVIDER").ok())
        .or_else(|| env::var("PROVIDER").ok());
    let model_str = model_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_MODEL").ok());

    let openai_key = crate::credential_leases::provider_api_key("OPENAI_API_KEY");
    let anthropic_key = provider_auth_for(
        "ANTHROPIC_API_KEY",
        crate::credential_egress::KIND_ANTHROPIC,
    );
    let gemini_key = provider_auth_for("GEMINI_API_KEY", crate::credential_egress::KIND_GEMINI);

    match provider_str.as_deref() {
        Some("gemini") => {
            let key = gemini_key.ok_or_else(|| {
                CallerError::Config("Presence provider=gemini but no GEMINI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gemini-2.5-flash".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(GeminiProvider::new(key, model, ctx, max_out)))
        }
        Some("anthropic") => {
            let key = anthropic_key.ok_or_else(|| {
                CallerError::Config(
                    "Presence provider=anthropic but no ANTHROPIC_API_KEY found.".into(),
                )
            })?;
            let model = model_str.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new(key, model, ctx, max_out)))
        }
        Some("openai") => {
            let key = openai_key.ok_or_else(|| {
                CallerError::Config("Presence provider=openai but no OPENAI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gpt-5.2-codex".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(key, model, ctx, max_out)))
        }
        // Keyless scripted provider (headless E2E/demos); explicit opt-in
        // only — see `select_provider`.
        Some("mock") => Ok(Box::new(crate::provider_mock::MockProvider::from_env()?)),
        Some(other) => Err(CallerError::Config(format!(
            "Unknown presence provider: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        None => {
            // No explicit override — fall back to the standard select_provider logic
            select_provider()
        }
    }
}

/// Select a provider for computer-use tasks (tasks with reference frames).
///
/// Priority: explicit config > CU_PROVIDER/CU_MODEL env > default select_provider.
/// Select the dedicated computer-use provider/model.
///
/// Reached from two places: the all-tasks CU-first interception, which is
/// VAULTED behind `[experimental] cu_first_routing` (off by default), and
/// the frame-grounded dashboard dispatch (explicit CU request — always
/// available). Priority: `[computer_use]` config > `CU_PROVIDER`/`CU_MODEL`
/// env > auto-detect by available key (OpenAI, Anthropic, then Gemini —
/// the Gemini CU arm is kept runnable but unmaintained).
pub fn select_cu_provider(
    cu_config: &crate::project::ComputerUseConfig,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    let provider_str = cu_config
        .provider
        .as_deref()
        .map(String::from)
        .or_else(|| env::var("CU_PROVIDER").ok())
        .or_else(|| env::var("PROVIDER").ok());
    let model_str = cu_config
        .model
        .as_deref()
        .map(String::from)
        .or_else(|| env::var("CU_MODEL").ok());

    let openai_key = crate::credential_leases::provider_api_key("OPENAI_API_KEY");
    let anthropic_key = provider_auth_for(
        "ANTHROPIC_API_KEY",
        crate::credential_egress::KIND_ANTHROPIC,
    );
    let gemini_key = provider_auth_for("GEMINI_API_KEY", crate::credential_egress::KIND_GEMINI);

    // CU providers get native CU tools + escalation function tool
    let escalate_tools = vec![crate::tools::escalate_to_agent_tool()];

    match provider_str.as_deref() {
        Some("gemini") => {
            let key = gemini_key.ok_or_else(|| {
                CallerError::Config("CU provider=gemini but no GEMINI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gemini-3-flash-preview".to_string());
            let display = crate::vision::display_config_for_provider("gemini");
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            let mut p = GeminiProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
            p.cu_enabled = true;
            p.cu_display = Some((display.width, display.height));
            Ok(Box::new(p))
        }
        Some("anthropic") => {
            let key = anthropic_key.ok_or_else(|| {
                CallerError::Config("CU provider=anthropic but no ANTHROPIC_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
            let display = crate::vision::display_config_for_provider("anthropic");
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            let mut p = AnthropicProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
            p.cu_enabled = true;
            p.cu_display = Some((display.width, display.height));
            Ok(Box::new(p))
        }
        Some("openai") => {
            let key = openai_key.ok_or_else(|| {
                CallerError::Config("CU provider=openai but no OPENAI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gpt-5.4-mini".to_string());
            let display = crate::vision::display_config_for_provider("openai");
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            let mut p = OpenAIProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
            p.cu_enabled = true;
            p.cu_display = Some((display.width, display.height));
            Ok(Box::new(p))
        }
        Some(other) => Err(CallerError::Config(format!(
            "Unknown CU provider: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        None => {
            // No CU-specific override — auto-detect a CU-capable provider.
            // Gemini goes LAST: its CU arm is de-facto unmaintained (kept
            // runnable, not preferred), so keyed deployments land on the
            // maintained OpenAI/Anthropic paths first.
            if let Some(key) = openai_key {
                let model = model_str.unwrap_or_else(|| "gpt-5.4-mini".to_string());
                let display = crate::vision::display_config_for_provider("openai");
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                let mut p =
                    OpenAIProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
                p.cu_enabled = true;
                p.cu_display = Some((display.width, display.height));
                Ok(Box::new(p))
            } else if let Some(key) = anthropic_key {
                let model = model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
                let display = crate::vision::display_config_for_provider("anthropic");
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                let mut p =
                    AnthropicProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
                p.cu_enabled = true;
                p.cu_display = Some((display.width, display.height));
                Ok(Box::new(p))
            } else if let Some(key) = gemini_key {
                let model = model_str.unwrap_or_else(|| "gemini-3-flash-preview".to_string());
                let display = crate::vision::display_config_for_provider("gemini");
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                let mut p =
                    GeminiProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
                p.cu_enabled = true;
                p.cu_display = Some((display.width, display.height));
                Ok(Box::new(p))
            } else {
                Err(CallerError::Config(
                    "No API key found for CU provider. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY.".into(),
                ))
            }
        }
    }
}

/// Select a provider for the presence layer (text mode).
///
/// Priority: explicit config > PRESENCE_PROVIDER/PRESENCE_MODEL env > auto-detect.
/// Auto-detect prefers gemini (gemini-2.5-flash) when GEMINI_API_KEY is set,
/// falling back to the cheapest available provider.
///
/// Presence providers receive the presence-native tool set through provider
/// tool calling. These are distinct from the main agent tools and include
/// presence actions such as submit_task and check_status.
pub fn select_presence_provider(
    provider_name: Option<&str>,
    model_name: Option<&str>,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    use crate::presence;

    let provider_str = provider_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_PROVIDER").ok());
    let model_str = model_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_MODEL").ok());

    let openai_key = crate::credential_leases::provider_api_key("OPENAI_API_KEY");
    let anthropic_key = provider_auth_for(
        "ANTHROPIC_API_KEY",
        crate::credential_egress::KIND_ANTHROPIC,
    );
    let gemini_key = provider_auth_for("GEMINI_API_KEY", crate::credential_egress::KIND_GEMINI);

    let tools = presence::presence_tools();

    match provider_str.as_deref() {
        Some("gemini") => {
            let key = gemini_key.ok_or_else(|| {
                CallerError::Config("Presence provider=gemini but no GEMINI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| presence::DEFAULT_TEXT_MODEL.to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(GeminiProvider::new_with_tools(
                key, model, ctx, max_out, tools,
            )))
        }
        Some("anthropic") => {
            let key = anthropic_key.ok_or_else(|| {
                CallerError::Config(
                    "Presence provider=anthropic but no ANTHROPIC_API_KEY found.".into(),
                )
            })?;
            let model = model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new_with_tools(
                key, model, ctx, max_out, tools,
            )))
        }
        Some("openai") => {
            let key = openai_key.ok_or_else(|| {
                CallerError::Config("Presence provider=openai but no OPENAI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gpt-4.1-mini".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new_with_tools(
                key, model, ctx, max_out, tools,
            )))
        }
        Some(other) => Err(CallerError::Config(format!(
            "Unknown presence provider: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        None => {
            // Auto-detect: prefer gemini (cheapest/fastest for presence)
            if let Some(key) = gemini_key {
                let model = model_str.unwrap_or_else(|| presence::DEFAULT_TEXT_MODEL.to_string());
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                Ok(Box::new(GeminiProvider::new_with_tools(
                    key, model, ctx, max_out, tools,
                )))
            } else if let Some(key) = anthropic_key {
                let model = model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                Ok(Box::new(AnthropicProvider::new_with_tools(
                    key, model, ctx, max_out, tools,
                )))
            } else if let Some(key) = openai_key {
                let model = model_str.unwrap_or_else(|| "gpt-4.1-mini".to_string());
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                Ok(Box::new(OpenAIProvider::new_with_tools(
                    key, model, ctx, max_out, tools,
                )))
            } else {
                Err(CallerError::Config(
                    "No API key found for presence layer. Set GEMINI_API_KEY, ANTHROPIC_API_KEY, or OPENAI_API_KEY.".into(),
                ))
            }
        }
    }
}

/// Deterministic scripted provider for keyless integration tests of the
/// native loop and the sub-agent substrate: an orchestrator parent spawns
/// two children (one succeeds, one fails), waits for both, and synthesizes.
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct MockOrchestrationProvider {
        calls: AtomicUsize,
    }

    impl Default for MockOrchestrationProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockOrchestrationProvider {
        pub fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn tool_call(name: &str, args: serde_json::Value) -> ToolCall {
            static NEXT_CALL: AtomicUsize = AtomicUsize::new(1);
            let n = NEXT_CALL.fetch_add(1, Ordering::Relaxed);
            ToolCall {
                id: format!("mock_call_{n}"),
                call_id: format!("mock_call_{n}"),
                name: name.to_string(),
                arguments: args.to_string(),
            }
        }

        fn response(content: &str, tool_calls: Vec<ToolCall>) -> ChatResponse {
            ChatResponse {
                content: content.to_string(),
                usage: TokenUsage::default(),
                reasoning_summary: None,
                reasoning_content: None,
                tool_calls,
                cu_calls: Vec::new(),
                raw_output: None,
            }
        }
    }

    #[async_trait]
    impl ChatProvider for MockOrchestrationProvider {
        async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            let system = messages
                .first()
                .map(|m| m.content.as_str())
                .unwrap_or_default();
            let transcript: String = messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            if system.contains("You are an autonomous AI orchestrator") {
                return Ok(match call_index {
                    0 => Self::response(
                        "Delegating to two sub-agents.",
                        vec![
                            Self::tool_call(
                                "spawn_sub_agent",
                                serde_json::json!({
                                    "task": "MOCK-RESEARCH: inspect the schema",
                                    "role": "research",
                                    "name": "mock-researcher",
                                }),
                            ),
                            Self::tool_call(
                                "spawn_sub_agent",
                                serde_json::json!({
                                    "task": "MOCK-FAILING: run the suite",
                                    "role": "testing",
                                    "name": "mock-tester",
                                }),
                            ),
                        ],
                    ),
                    1 => Self::response(
                        "Waiting for both sub-agents.",
                        vec![Self::tool_call(
                            "wait_sub_agents",
                            serde_json::json!({ "mode": "all", "timeout_secs": 60 }),
                        )],
                    ),
                    _ => {
                        // Synthesize only when both child results actually
                        // arrived in context; otherwise surface the failure.
                        let saw_success = transcript.contains("research findings ABC");
                        let saw_failure = transcript.contains("boom");
                        let message = if saw_success && saw_failure {
                            "SYNTHESIS: research succeeded, testing failed"
                        } else {
                            "RESULTS-MISSING"
                        };
                        Self::response(
                            message,
                            vec![Self::tool_call(
                                "signal_done",
                                serde_json::json!({ "message": message }),
                            )],
                        )
                    }
                });
            }

            if transcript.contains("MOCK-RESEARCH") {
                // Ends WITHOUT submit_result: a pure text answer. The
                // supervisor synthesizes this child's result from the loop's
                // last_response — regression coverage for the round-loop
                // stats propagation (a dropped last_response turned these
                // results into a content-free "Task completed").
                return Ok(Self::response(
                    "research findings ABC: the schema has 3 tables.\n\nBRIEF: Research done.",
                    vec![],
                ));
            }
            if transcript.contains("MOCK-FAILING") {
                return Ok(Self::response(
                    "Reporting failure.",
                    vec![
                        Self::tool_call(
                            "submit_result",
                            serde_json::json!({
                                "status": "failed",
                                "summary": "suite could not run",
                                "failure_reason": "boom",
                            }),
                        ),
                        Self::tool_call("signal_done", serde_json::json!({})),
                    ],
                ));
            }
            Ok(Self::response(
                "Nothing to do.",
                vec![Self::tool_call("signal_done", serde_json::json!({}))],
            ))
        }

        fn name(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock-orchestration"
        }
        fn context_window(&self) -> u64 {
            1_000_000
        }
        fn max_output_tokens(&self) -> u64 {
            100_000
        }
        fn use_tools(&self) -> bool {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_env_keys_whitelists_provider_keys_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "OPENAI_API_KEY=sk-project\n\
             PROVIDER=gemini\n\
             OPENAI_BASE_URL=https://evil.example\n\
             MODEL_NAME=gpt-hijack\n\
             ANTHROPIC_API_KEY=\n",
        )
        .unwrap();
        let keys = ProjectEnvKeys::load(dir.path());
        assert_eq!(keys.get("OPENAI_API_KEY").as_deref(), Some("sk-project"));
        // Endpoint-shaped or selection-shaped vars must never load from an
        // agent-writable project dir.
        assert!(keys.get("PROVIDER").is_none());
        assert!(keys.get("OPENAI_BASE_URL").is_none());
        assert!(keys.get("MODEL_NAME").is_none());
        // Empty values don't count as configured.
        assert!(keys.get("ANTHROPIC_API_KEY").is_none());
        assert!(keys.file_present);
    }

    #[test]
    fn project_env_keys_missing_file_is_empty_but_named() {
        let dir = tempfile::tempdir().unwrap();
        let keys = ProjectEnvKeys::load(dir.path());
        assert!(!keys.file_present);
        assert!(!keys.has_any());
        // The path is still recorded so the unfueled error can say
        // "checked <project>/.env (missing)".
        assert!(keys.env_path.as_ref().unwrap().ends_with(".env"));
    }

    #[test]
    fn unfueled_error_text_names_session_project_env_and_remediations() {
        let dir = tempfile::tempdir().unwrap();
        let keys = ProjectEnvKeys::load(dir.path());
        let text = unfueled_error_text(&keys);
        // Stable opener: automation greps stderr for it.
        assert!(text.contains("No API key found."), "{text}");
        assert!(text.contains("session project"), "{text}");
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("~/.config/intendant/.env"), "{text}");
    }

    #[test]
    fn unfueled_error_text_without_project_omits_project_line() {
        let text = unfueled_error_text(&ProjectEnvKeys::none());
        assert!(text.contains("No API key found."), "{text}");
        assert!(!text.contains("session project"), "{text}");
    }

    #[test]
    fn anthropic_provider_name() {
        let provider = AnthropicProvider::new(
            "key".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
            200_000,
            8_192,
        );
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn anthropic_extracts_system_message() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "You are helpful.".to_string(),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                ..Default::default()
            },
            Message {
                role: "assistant".to_string(),
                content: "Hi!".to_string(),
                ..Default::default()
            },
        ];

        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        assert_eq!(system, "You are helpful.");

        let api_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| AnthropicMessage {
                role: m.role.clone(),
                content: serde_json::Value::String(m.content.clone()),
            })
            .collect();

        assert_eq!(api_messages.len(), 2);
        assert_eq!(api_messages[0].role, "user");
        assert_eq!(api_messages[1].role, "assistant");
    }

    #[test]
    fn anthropic_no_system_message() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
            ..Default::default()
        }];

        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        assert_eq!(system, "");
    }

    #[test]
    fn token_usage_default() {
        let usage = TokenUsage::default();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn token_usage_serialization() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        let deserialized: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.prompt_tokens, 100);
        assert_eq!(deserialized.completion_tokens, 50);
        assert_eq!(deserialized.total_tokens, 150);
    }

    #[test]
    fn anthropic_usage_deserialization() {
        let json = r#"{
            "content": [{"text": "Hi", "type": "text"}],
            "usage": {"input_tokens": 20, "output_tokens": 10}
        }"#;
        let resp: AnthropicChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content[0].text.as_deref(), Some("Hi"));
        assert_eq!(resp.content[0].content_type.as_deref(), Some("text"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 10);
    }

    #[test]
    fn anthropic_usage_missing() {
        let json = r#"{
            "content": [{"text": "Hi", "type": "text"}]
        }"#;
        let resp: AnthropicChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none());
    }

    #[test]
    fn anthropic_tool_use_deserialization() {
        let json = r#"{
            "content": [
                {"type": "text", "text": "I'll list the files."},
                {
                    "type": "tool_use",
                    "id": "toolu_abc123",
                    "name": "exec_command",
                    "input": {"nonce": 1, "command": "ls -la"}
                }
            ],
            "usage": {"input_tokens": 50, "output_tokens": 30}
        }"#;
        let resp: AnthropicChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.content[0].content_type.as_deref(), Some("text"));
        assert_eq!(
            resp.content[0].text.as_deref(),
            Some("I'll list the files.")
        );
        assert_eq!(resp.content[1].content_type.as_deref(), Some("tool_use"));
        assert_eq!(resp.content[1].id.as_deref(), Some("toolu_abc123"));
        assert_eq!(resp.content[1].name.as_deref(), Some("exec_command"));
        assert!(resp.content[1].input.is_some());
    }

    #[test]
    fn anthropic_request_with_tools() {
        let tool_defs = crate::tools::all_tools();
        let tools: Vec<serde_json::Value> = tool_defs.iter().map(|t| t.to_anthropic()).collect();
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: serde_json::json!([{
                "type": "text",
                "text": "You are an agent.",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("list files".to_string()),
            }],
            max_tokens: 8192,
            tools: Some(tools),
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tools\""));
        assert!(json.contains("exec_command"));
        assert!(json.contains("cache_control"));
    }

    #[test]
    fn anthropic_request_without_tools() {
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: serde_json::json!([{
                "type": "text",
                "text": "You are helpful.",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("hello".to_string()),
            }],
            max_tokens: 8192,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("\"tools\""));
    }

    #[test]
    fn anthropic_message_structured_content() {
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Running command"},
                {"type": "tool_use", "id": "toolu_1", "name": "exec_command", "input": {"nonce": 1}}
            ]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("tool_use"));
        assert!(json.contains("toolu_1"));
    }

    #[test]
    fn default_context_window_known_models() {
        assert_eq!(default_context_window("gpt-5.2-codex"), 1_000_000);
        assert_eq!(default_context_window("gpt-5"), 1_000_000);
        assert_eq!(
            default_context_window("claude-sonnet-4-5-20250929"),
            200_000
        );
        assert_eq!(default_context_window("o1-preview"), 200_000);
        assert_eq!(default_context_window("o3-mini"), 200_000);
    }

    #[test]
    fn default_context_window_unknown_model() {
        assert_eq!(default_context_window("some-unknown-model"), 200_000);
    }

    #[test]
    fn default_max_output_known_models() {
        assert_eq!(default_max_output_tokens("gpt-5.2-codex"), 128_000);
        assert_eq!(default_max_output_tokens("gpt-5"), 128_000);
        assert_eq!(
            default_max_output_tokens("claude-sonnet-4-5-20250929"),
            8_192
        );
        assert_eq!(default_max_output_tokens("o1-preview"), 100_000);
    }

    #[test]
    fn context_window_methods() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        assert_eq!(provider.context_window(), 400_000);
        assert_eq!(provider.max_output_tokens(), 128_000);

        let provider = AnthropicProvider::new(
            "key".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
            200_000,
            8_192,
        );
        assert_eq!(provider.context_window(), 200_000);
        assert_eq!(provider.max_output_tokens(), 8_192);
    }

    #[test]
    fn supports_structured_output_models() {
        assert!(supports_structured_output("gpt-5.2-codex"));
        assert!(supports_structured_output("gpt-5"));
        assert!(supports_structured_output("o3-mini"));
        assert!(supports_structured_output("o4-mini"));
        assert!(!supports_structured_output("claude-sonnet-4-5-20250929"));
        assert!(!supports_structured_output("some-unknown-model"));
    }

    #[test]
    fn supports_reasoning_models() {
        assert!(supports_reasoning("gpt-5.4"));
        assert!(supports_reasoning("gpt-5"));
        assert!(supports_reasoning("o3-mini"));
        assert!(supports_reasoning("o4-mini"));
        assert!(!supports_reasoning("claude-sonnet-4-5-20250929"));
    }

    #[test]
    fn default_context_window_o4() {
        assert_eq!(default_context_window("o4-mini"), 200_000);
        assert_eq!(default_context_window("o4"), 200_000);
    }

    #[test]
    fn default_max_output_o4() {
        assert_eq!(default_max_output_tokens("o4-mini"), 100_000);
        assert_eq!(default_max_output_tokens("o4"), 100_000);
    }

    #[test]
    fn chat_response_default_empty_tool_calls() {
        let resp = ChatResponse {
            content: "hello".to_string(),
            usage: TokenUsage::default(),
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls: vec![],
            cu_calls: vec![],
            raw_output: None,
        };
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn tool_call_fields() {
        let tc = ToolCall {
            id: "fc_123".to_string(),
            call_id: "call_123".to_string(),
            name: "exec_command".to_string(),
            arguments: r#"{"nonce":1,"command":"ls"}"#.to_string(),
        };
        assert_eq!(tc.id, "fc_123");
        assert_eq!(tc.call_id, "call_123");
        assert_eq!(tc.name, "exec_command");
        assert!(tc.arguments.contains("nonce"));
    }

    #[test]
    fn resolve_use_tools_default() {
        // When USE_NATIVE_TOOLS is not set, defaults to true.
        // We can't guarantee the env state, but the function should not panic.
        let _ = resolve_use_tools();
    }

    #[test]
    fn anthropic_provider_use_tools_trait() {
        let provider = AnthropicProvider::new(
            "key".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
            200_000,
            8_192,
        );
        if provider.use_tools() {
            assert!(!provider.tools().is_empty());
        } else {
            assert!(provider.tools().is_empty());
        }
    }

    // --- Gemini tests ---

    #[test]
    fn gemini_provider_name() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        assert_eq!(provider.name(), "gemini");
        assert_eq!(provider.model(), "gemini-2.5-pro");
        assert_eq!(provider.context_window(), 1_048_576);
        assert_eq!(provider.max_output_tokens(), 65_536);
    }

    #[test]
    fn gemini_role_mapping() {
        assert_eq!(gemini_role("assistant"), "model");
        assert_eq!(gemini_role("user"), "user");
        assert_eq!(gemini_role("developer"), "user");
        assert_eq!(gemini_role("tool"), "user");
        assert_eq!(gemini_role("system"), "user");
    }

    #[test]
    fn default_context_window_gemini() {
        assert_eq!(default_context_window("gemini-2.5-pro"), 1_048_576);
        assert_eq!(default_context_window("gemini-2.5-flash"), 1_048_576);
    }

    #[test]
    fn default_max_output_gemini() {
        assert_eq!(default_max_output_tokens("gemini-2.5-pro"), 65_536);
        assert_eq!(default_max_output_tokens("gemini-2.5-flash"), 65_536);
    }

    #[test]
    fn gemini_response_text_parsing() {
        let resp: serde_json::Value = serde_json::from_str(
            r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello from Gemini!"}],
                    "role": "model"
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        }"#,
        )
        .unwrap();

        let text = resp
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(|t| t.as_str());
        assert_eq!(text, Some("Hello from Gemini!"));

        let total = resp
            .pointer("/usageMetadata/totalTokenCount")
            .and_then(|v| v.as_u64());
        assert_eq!(total, Some(15));
    }

    #[test]
    fn gemini_response_function_call_parsing() {
        let resp: serde_json::Value = serde_json::from_str(r#"{
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "functionCall": {
                                "name": "exec_command",
                                "args": {"nonce": 1, "command": "ls -la"}
                            }
                        },
                        {
                            "functionCall": {
                                "name": "fetch_status",
                                "args": {"nonce": 1, "status_type": "stdout"}
                            }
                        }
                    ],
                    "role": "model"
                }
            }],
            "usageMetadata": {"promptTokenCount": 50, "candidatesTokenCount": 20, "totalTokenCount": 70}
        }"#).unwrap();

        let parts = resp
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["functionCall"]["name"].as_str(),
            Some("exec_command")
        );
        assert_eq!(
            parts[1]["functionCall"]["name"].as_str(),
            Some("fetch_status")
        );
    }

    #[test]
    fn gemini_provider_use_tools_trait() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        if provider.use_tools() {
            assert!(!provider.tools().is_empty());
        } else {
            assert!(provider.tools().is_empty());
        }
    }

    #[test]
    fn gemini_endpoint_default() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        assert!(provider
            .endpoint
            .contains("generativelanguage.googleapis.com"));
    }

    #[test]
    fn is_retryable_429() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn is_retryable_500() {
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn is_retryable_502() {
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn not_retryable_400() {
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
    }

    #[test]
    fn not_retryable_401() {
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn not_retryable_200() {
        assert!(!is_retryable_status(reqwest::StatusCode::OK));
    }

    #[test]
    fn backoff_delay_increases() {
        let d0 = backoff_delay(0);
        let d1 = backoff_delay(1);
        let d2 = backoff_delay(2);
        // Base doubles each time: 1s, 2s, 4s
        assert!(d1 > d0);
        assert!(d2 > d1);
    }

    #[test]
    fn mask_openai_key() {
        let s = "Error: key sk-abcdefghijklmnopqrstuvwxyz123456 is invalid";
        let masked = mask_api_keys(s);
        assert!(masked.contains("sk-abcdefghij***"));
        assert!(!masked.contains("klmnopqrstuvwxyz123456"));
    }

    #[test]
    fn mask_gemini_key() {
        let s = "Error with key AIzaSyB12345678901234567890";
        let masked = mask_api_keys(s);
        assert!(masked.contains("AIzaSyB12345***"));
        assert!(!masked.contains("678901234567890"));
    }

    #[test]
    fn mask_preserves_normal_text() {
        let s = "This is a normal error message without any keys";
        assert_eq!(mask_api_keys(s), s);
    }

    #[test]
    fn mask_short_prefix_not_matched() {
        let s = "sk-short";
        // Less than 10 chars after prefix, not matched
        assert_eq!(mask_api_keys(s), s);
    }

    // --- Streaming tests ---

    #[test]
    fn parse_sse_line_data() {
        let (kind, content) = parse_sse_line("data: {\"type\":\"ping\"}").unwrap();
        assert_eq!(kind, "data");
        assert_eq!(content, "{\"type\":\"ping\"}");
    }

    #[test]
    fn parse_sse_line_event() {
        let (kind, content) = parse_sse_line("event: message_start").unwrap();
        assert_eq!(kind, "event");
        assert_eq!(content, "message_start");
    }

    #[test]
    fn parse_sse_line_unknown() {
        assert!(parse_sse_line("id: 123").is_none());
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line("random text").is_none());
    }

    #[test]
    fn stream_event_delta_clone() {
        let event = StreamEvent::Delta("hello".to_string());
        let cloned = event.clone();
        if let StreamEvent::Delta(text) = cloned {
            assert_eq!(text, "hello");
        } else {
            panic!("Expected Delta variant");
        }
    }

    #[test]
    fn stream_event_complete_clone() {
        let resp = ChatResponse {
            content: "done".to_string(),
            usage: TokenUsage::default(),
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls: vec![],
            cu_calls: vec![],
            raw_output: None,
        };
        let event = StreamEvent::Complete(resp);
        let cloned = event.clone();
        if let StreamEvent::Complete(r) = cloned {
            assert_eq!(r.content, "done");
        } else {
            panic!("Expected Complete variant");
        }
    }

    #[test]
    fn anthropic_request_stream_field_serialization() {
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: serde_json::json!([{
                "type": "text",
                "text": "test",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![],
            max_tokens: 8192,
            tools: None,
            stream: true,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn anthropic_request_no_stream_when_false() {
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: serde_json::json!([{
                "type": "text",
                "text": "test",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![],
            max_tokens: 8192,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("stream"));
    }

    #[test]
    fn anthropic_usage_normalizes_cache_counters() {
        // Anthropic reports cache reads/writes OUTSIDE input_tokens; the
        // normalized TokenUsage folds them into prompt_tokens (context
        // footprint, cached ⊆ prompt — the same convention as OpenAI and
        // the external adapters).
        let parsed: AnthropicUsage = serde_json::from_value(serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 100,
            "cache_creation_input_tokens": 20,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 0,
                "ephemeral_1h_input_tokens": 20
            }
        }))
        .unwrap();
        let usage = parsed.to_token_usage();
        assert_eq!(usage.prompt_tokens, 130);
        assert_eq!(usage.total_tokens, 135);
        assert_eq!(usage.cached_tokens, 100);
        assert_eq!(usage.cache_creation_tokens, 20);
        assert_eq!(usage.cache_ttl_seconds, Some(3600));

        // Flat creation (no split object) → the 5-minute default; pure
        // reads make no flavor statement.
        assert_eq!(anthropic_cache_ttl_seconds(40, None), Some(300));
        assert_eq!(anthropic_cache_ttl_seconds(0, None), None);
        let five_minute = AnthropicCacheCreation {
            ephemeral_5m_input_tokens: 8,
            ephemeral_1h_input_tokens: 0,
        };
        assert_eq!(anthropic_cache_ttl_seconds(8, Some(&five_minute)), Some(300));
    }

    #[test]
    fn anthropic_rate_limit_headers_become_windows() {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut set = |k: &'static str, v: &str| {
            headers.insert(k, v.parse().unwrap());
        };
        set("anthropic-ratelimit-requests-limit", "4000");
        set("anthropic-ratelimit-requests-remaining", "3999");
        set("anthropic-ratelimit-requests-reset", "2026-07-05T12:00:00Z");
        set("anthropic-ratelimit-tokens-limit", "400000");
        set("anthropic-ratelimit-tokens-remaining", "80000");
        let windows = anthropic_rate_limit_windows_from_headers(&headers);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "req/min");
        assert_eq!(windows[0].used_pct, 0);
        assert!(windows[0].resets_at_epoch.is_some());
        assert_eq!(windows[1].label, "tok/min");
        assert_eq!(windows[1].used_pct, 80);
        // Missing reset degrades to a gauge without a countdown.
        assert_eq!(windows[1].resets_at_epoch, None);

        // No headers → no windows (e.g. the egress relay strips them).
        assert!(
            anthropic_rate_limit_windows_from_headers(&reqwest::header::HeaderMap::new())
                .is_empty()
        );
    }

    #[test]
    fn build_anthropic_messages_extracts_system() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "You are helpful.".to_string(),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                ..Default::default()
            },
        ];
        let (system, api_msgs) = build_anthropic_messages(&messages);
        let sys_text = system[0]["text"].as_str().unwrap();
        assert_eq!(sys_text, "You are helpful.");
        assert_eq!(api_msgs.len(), 1);
        assert_eq!(api_msgs[0].role, "user");
    }

    #[test]
    fn build_gemini_request_parts_includes_contents() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "System".to_string(),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: "Hi".to_string(),
                ..Default::default()
            },
        ];
        let (sys, contents, body) = build_gemini_request_parts(&messages, &provider);
        assert_eq!(sys.as_deref(), Some("System"));
        assert_eq!(contents.len(), 1);
        assert!(body.get("contents").is_some());
    }

    // --- Image/vision provider tests ---

    pub(crate) fn tool_msg_with_images() -> Message {
        use crate::conversation::ImageData;
        Message {
            role: "tool".to_string(),
            content: "screenshot taken".to_string(),
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("capture_screen".to_string()),
            images: Some(vec![ImageData {
                media_type: "image/png".to_string(),
                data: "iVBORw0KGgo=".to_string(),
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn anthropic_builder_includes_image_in_tool_result() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            tool_msg_with_images(),
        ];
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        assert_eq!(api_msgs.len(), 1);
        let content = api_msgs[0].content.as_array().unwrap();
        let tool_result = &content[0];
        assert_eq!(tool_result["type"].as_str(), Some("tool_result"));
        let inner = tool_result["content"].as_array().unwrap();
        assert_eq!(inner[0]["type"].as_str(), Some("text"));
        assert_eq!(inner[1]["type"].as_str(), Some("image"));
        assert_eq!(inner[1]["source"]["type"].as_str(), Some("base64"));
        assert_eq!(inner[1]["source"]["media_type"].as_str(), Some("image/png"));
    }

    #[test]
    fn anthropic_builder_plain_string_without_images() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            Message {
                role: "tool".to_string(),
                content: "result".to_string(),
                tool_call_id: Some("call_1".to_string()),
                ..Default::default()
            },
        ];
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        let content = api_msgs[0].content.as_array().unwrap();
        let tool_result = &content[0];
        // content should be a plain string, not an array
        assert!(tool_result["content"].is_string());
    }

    #[test]
    fn gemini_builder_includes_image_after_function_response() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            tool_msg_with_images(),
        ];
        let (_sys, contents, _body) = build_gemini_request_parts(&messages, &provider);
        // Should have functionResponse + user message with inlineData
        assert_eq!(contents.len(), 2);
        let img_msg = &contents[1];
        assert_eq!(img_msg["role"].as_str(), Some("user"));
        let parts = img_msg["parts"].as_array().unwrap();
        assert_eq!(
            parts[0]["text"].as_str().unwrap(),
            "Screenshot from the previous tool call:"
        );
        assert_eq!(
            parts[1]["inlineData"]["mimeType"].as_str(),
            Some("image/png")
        );
        assert_eq!(
            parts[1]["inlineData"]["data"].as_str(),
            Some("iVBORw0KGgo=")
        );
    }

    #[test]
    fn gemini_builder_no_image_without_images_field() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            Message {
                role: "tool".to_string(),
                content: r#"{"output":"result"}"#.to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("capture_screen".to_string()),
                ..Default::default()
            },
        ];
        let (_sys, contents, _body) = build_gemini_request_parts(&messages, &provider);
        // Should have only the functionResponse, no user image message
        assert_eq!(contents.len(), 1);
    }
}
