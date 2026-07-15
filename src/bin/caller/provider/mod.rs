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
mod anthropic;
pub(crate) use anthropic::*;
mod gemini;
pub(crate) use gemini::*;

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
    let mut last_err: Option<CallerError> = None;
    for attempt in 0..=max_retries {
        match build_request().send().await {
            Ok(response) => {
                if response.status().is_success() || !is_retryable_status(response.status()) {
                    return Ok(response);
                }
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_err = Some(CallerError::Provider(format!(
                    "{}: {}",
                    status,
                    mask_api_keys(&body)
                )));
            }
            // Transport-level failure before a response arrived (connection
            // reset, refused, DNS): as retryable as a 5xx — nothing was
            // consumed. Timeouts are excluded: each attempt already waited
            // the full request timeout, so backoff-retrying them multiplies
            // a hung endpoint into `timeout × (retries+1)` of wall clock.
            Err(e) if !e.is_timeout() => last_err = Some(CallerError::Http(e)),
            Err(e) => return Err(CallerError::Http(e)),
        }
        if attempt < max_retries {
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }
    Err(last_err
        .unwrap_or_else(|| CallerError::Provider("request failed after retries".to_string())))
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
            used_pct: Some(used_pct),
            resets_at_epoch,
            status: None,
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
pub(crate) enum ProviderHttpResponse {
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

    /// Whether the agent loop must strip superseded non-CU screenshots from
    /// the conversation before each request (`Conversation::strip_old_images`).
    /// The OpenAI `computer` tool rejects requests carrying more than one
    /// non-CU image, so CU-enabled OpenAI instances require it. Anthropic
    /// overrides this to `false`: it accepts multi-image histories, and
    /// mutating old messages would invalidate its prompt-cache prefix from
    /// the mutation point on every new screenshot.
    fn requires_image_stripping(&self) -> bool {
        self.cu_enabled()
    }

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
        // Anthropic ceilings by family: the Claude 3 generation caps at 8K
        // and Opus 4/4.1 at 32K, while every 4.5+ model accepts at least
        // 64K output. The old blanket 8_192 was a Claude-3-era default that
        // truncated long completions (or forced continuation turns, each
        // re-billing the full prompt). `max_tokens` is a ceiling, not a
        // target — raising it costs nothing unless output is generated.
        m if m.starts_with("claude-3") => 8_192,
        m if m.starts_with("claude-opus-4-1") || m.starts_with("claude-opus-4-2") => 32_000,
        m if m.contains("claude") => 64_000,
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
    let keys = project_root.map(ProjectEnvKeys::load).unwrap_or_default();
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
    let mut text =
        String::from("No API key found. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY.");
    let mut checked: Vec<String> =
        vec!["credential leases and browser relays (none active)".to_string()];
    if let Some(report) = ENV_SEARCH.get() {
        match &report.cwd_env {
            Some(path) => checked.push(format!("{} (loaded at startup)", path.display())),
            None => checked.push("no .env found from the startup directory upward".to_string()),
        }
        for (path, loaded) in [&report.project_env, &report.global_env]
            .into_iter()
            .flatten()
        {
            // Skip duplicates: the walk-up and the project root often
            // resolve to the same file.
            if report.cwd_env.as_deref() == Some(path.as_path()) {
                continue;
            }
            checked.push(format!(
                "{} ({})",
                path.display(),
                if *loaded {
                    "loaded at startup"
                } else {
                    "missing"
                }
            ));
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
        // 4.5-generation Claude models accept 64K output; the old 8_192
        // blanket default truncated long completions.
        assert_eq!(
            default_max_output_tokens("claude-sonnet-4-5-20250929"),
            64_000
        );
        assert_eq!(
            default_max_output_tokens("claude-haiku-4-5-20251001"),
            64_000
        );
        // Older families keep their real ceilings: Claude 3 at 8K,
        // Opus 4/4.1 at 32K.
        assert_eq!(
            default_max_output_tokens("claude-3-5-sonnet-20241022"),
            8_192
        );
        assert_eq!(
            default_max_output_tokens("claude-opus-4-1-20250805"),
            32_000
        );
        assert_eq!(default_max_output_tokens("claude-opus-4-20250514"), 32_000);
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
        assert_eq!(windows[0].used_pct, Some(0));
        assert!(windows[0].resets_at_epoch.is_some());
        assert_eq!(windows[1].label, "tok/min");
        assert_eq!(windows[1].used_pct, Some(80));
        // Missing reset degrades to a gauge without a countdown.
        assert_eq!(windows[1].resets_at_epoch, None);

        // No headers → no windows (e.g. the egress relay strips them).
        assert!(
            anthropic_rate_limit_windows_from_headers(&reqwest::header::HeaderMap::new())
                .is_empty()
        );
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
}
