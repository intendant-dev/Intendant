//! The Anthropic provider: Messages-API types, cache-TTL and usage
//! accounting, the streaming and non-streaming ChatProvider impl,
//! computer-use action parsing, and message assembly.

use super::*;

// --- Anthropic ---

#[derive(Serialize)]
pub(crate) struct AnthropicChatRequest {
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
pub(crate) struct AnthropicMessage {
    role: String,
    content: serde_json::Value, // String or array of content blocks
}

#[derive(Deserialize)]
pub(crate) struct AnthropicChatResponse {
    content: Vec<AnthropicContent>,
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
pub(crate) struct AnthropicContent {
    #[serde(rename = "type")]
    content_type: Option<String>,
    text: Option<String>,
    // tool_use fields
    id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct AnthropicUsage {
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
pub(crate) struct AnthropicCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

/// The TTL flavor a response's cache writes imply. Only creation makes a
/// flavor statement — read-only responses return `None` and consumers keep
/// the last known flavor.
pub(crate) fn anthropic_cache_ttl_seconds(
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
    pub(crate) fn to_token_usage(&self) -> TokenUsage {
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

pub(crate) fn anthropic_endpoint() -> String {
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

    /// The `max_tokens` actually sent for this request. Claude models from
    /// before the 4.5 generation hard-400 when `input + max_tokens` exceeds
    /// the context window instead of clamping server-side — and with the
    /// 32K/64K family defaults that fires *below* the loop's 90%
    /// auto-compact threshold, killing the session on a non-retryable 400
    /// before compaction can ever run. For those models the ceiling is
    /// clamped against a conservative input estimate; 4.5+ models pass the
    /// configured value through untouched.
    ///
    /// Shape: `min(configured, max(headroom, floor))` — the configured
    /// value is a hard ceiling and is never raised (an explicit
    /// `MAX_OUTPUT_TOKENS=1024` override must stay 1024), while a
    /// sub-floor headroom is still honored up to the floor: the input
    /// estimate overestimates, so real headroom is at least the computed
    /// one, and a truncated completion beats a dead session.
    fn effective_max_tokens(
        &self,
        messages: &[Message],
        tools: Option<&[serde_json::Value]>,
    ) -> u64 {
        if !anthropic_needs_output_clamp(&self.model) {
            return self.max_output_tokens;
        }
        let estimated_input = estimated_input_tokens(messages) + estimated_tools_tokens(tools);
        let headroom = self
            .context_window
            .saturating_sub(estimated_input)
            .saturating_sub(CLAMP_MARGIN_TOKENS);
        self.max_output_tokens.min(headroom.max(CLAMP_FLOOR_TOKENS))
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
    /// Takes the request pre-serialized so the body is produced exactly once
    /// per call (retries memcpy the bytes instead of re-walking the DOM).
    pub(crate) async fn post_messages(
        &self,
        request_body: &[u8],
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
                        .body(request_body.to_vec());
                    if streaming {
                        request.timeout(STREAM_TIMEOUT)
                    } else {
                        request
                    }
                };
                // Streaming goes through the same retry policy: the status
                // is known before any body bytes stream, so a 429/529/5xx at
                // request-open — routine provider throttling — retries with
                // backoff instead of killing the session turn.
                let response = send_with_retry(&self.client, builder, MAX_RETRIES).await?;
                Ok(ProviderHttpResponse::Direct(response))
            }
            ProviderAuth::ClientEgress { kind } => {
                let headers = vec![
                    ("anthropic-version".to_string(), "2023-06-01".to_string()),
                    ("anthropic-beta".to_string(), beta_header.to_string()),
                    ("content-type".to_string(), "application/json".to_string()),
                ];
                crate::credential_egress::fetch(kind, "POST", &url, headers, request_body.to_vec())
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
            max_tokens: self.effective_max_tokens(messages, tools.as_deref()),
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
            max_tokens: self.effective_max_tokens(messages, tools.as_deref()),
            tools,
            stream: false,
        };

        // Serialize once, straight to bytes — `to_value` + `.json()` walked
        // the full request (images included) twice per call.
        let request_body = serde_json::to_vec(&request).map_err(CallerError::Json)?;

        let beta_header = if self.cu_enabled {
            "prompt-caching-2024-07-31,computer-use-2025-11-24"
        } else {
            "prompt-caching-2024-07-31"
        };

        let response = self
            .post_messages(&request_body, beta_header, false)
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
                                cu_calls.push(crate::computer_use::CuToolCall {
                                    call_id: id.clone(),
                                    actions: vec![action],
                                    metadata: crate::computer_use::CuCallMetadata::default(),
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

    /// Anthropic accepts multi-image histories, so superseded screenshots
    /// stay in place: `strip_old_images` mutates earlier messages, which
    /// would invalidate the prompt-cache prefix from the mutation point.
    fn requires_image_stripping(&self) -> bool {
        false
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
            max_tokens: self.effective_max_tokens(messages, tools.as_deref()),
            tools,
            stream: true,
        };
        let request_body = serde_json::to_vec(&request).map_err(CallerError::Json)?;

        let beta_header = if self.cu_enabled {
            "prompt-caching-2024-07-31,computer-use-2025-11-24"
        } else {
            "prompt-caching-2024-07-31"
        };

        let response = self.post_messages(&request_body, beta_header, true).await?;

        if !response.status_success() {
            let status = response.status_line();
            let body = response.body_text().await;
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let mut fold =
            AnthropicStreamFold::new(self.cu_enabled, response.anthropic_rate_limit_windows());
        streaming::run_sse_stream(response, &mut fold, on_event)
            .await
            .map_err(streaming::StreamFailure::into_caller_error)?;
        let response = fold.finish();
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// The Anthropic arm of the shared SSE driver: exactly the per-event
/// mutable state the old hand-rolled loop carried (current tool
/// assembly, prompt-side usage from `message_start`, output tokens from
/// `message_delta`), with the mechanics living in `provider::streaming`.
pub(crate) struct AnthropicStreamFold {
    cu_enabled: bool,
    json: streaming::EventJson,
    text_parts: Vec<String>,
    tool_calls: Vec<ToolCall>,
    cu_calls: Vec<crate::computer_use::CuToolCall>,
    current_tool_json: String,
    current_tool_id: String,
    current_tool_name: String,
    usage: TokenUsage,
}

impl AnthropicStreamFold {
    pub(crate) fn new(
        cu_enabled: bool,
        rate_limit_windows: Vec<crate::types::SessionLimitWindow>,
    ) -> Self {
        Self {
            cu_enabled,
            json: streaming::EventJson::new(),
            text_parts: Vec::new(),
            tool_calls: Vec::new(),
            cu_calls: Vec::new(),
            current_tool_json: String::new(),
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            usage: TokenUsage {
                rate_limit_windows,
                ..Default::default()
            },
        }
    }

    /// Assemble the final response after the stream ends.
    pub(crate) fn finish(mut self) -> ChatResponse {
        self.usage.total_tokens = self.usage.prompt_tokens + self.usage.completion_tokens;
        ChatResponse {
            content: self.text_parts.join(""),
            usage: self.usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls: self.tool_calls,
            cu_calls: self.cu_calls,
            raw_output: None,
        }
    }
}

impl streaming::SseFold for AnthropicStreamFold {
    fn on_data(
        &mut self,
        data: &str,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<(), CallerError> {
        let Some(event) = self.json.parse(data) else {
            return Ok(());
        };
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "content_block_start" => {
                if let Some(cb) = event.get("content_block") {
                    let cb_type = cb.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if cb_type == "tool_use" {
                        self.current_tool_id = cb
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        self.current_tool_name = cb
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        self.current_tool_json.clear();
                    }
                }
            }
            "content_block_delta" => {
                if let Some(delta) = event.get("delta") {
                    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match delta_type {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                self.text_parts.push(text.to_string());
                                on_event(StreamEvent::Delta(text.to_string()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(json) = delta.get("partial_json").and_then(|t| t.as_str())
                            {
                                self.current_tool_json.push_str(json);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if !self.current_tool_id.is_empty() {
                    if self.current_tool_name == "computer" && self.cu_enabled {
                        if let Ok(input) =
                            serde_json::from_str::<serde_json::Value>(&self.current_tool_json)
                        {
                            if let Some(action) = parse_anthropic_cu_action(&input) {
                                self.cu_calls.push(crate::computer_use::CuToolCall {
                                    call_id: self.current_tool_id.clone(),
                                    actions: vec![action],
                                    metadata: crate::computer_use::CuCallMetadata::default(),
                                });
                            }
                        }
                    } else {
                        self.tool_calls.push(ToolCall {
                            id: self.current_tool_id.clone(),
                            call_id: self.current_tool_id.clone(),
                            name: self.current_tool_name.clone(),
                            arguments: self.current_tool_json.clone(),
                        });
                    }
                    self.current_tool_id.clear();
                    self.current_tool_name.clear();
                    self.current_tool_json.clear();
                }
            }
            "message_delta" => {
                if let Some(u) = event.get("usage") {
                    let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    self.usage.completion_tokens = output;
                }
            }
            "message_start" => {
                if let Some(msg) = event.get("message") {
                    if let Some(parsed) = msg
                        .get("usage")
                        .cloned()
                        .and_then(|u| serde_json::from_value::<AnthropicUsage>(u).ok())
                    {
                        // Prompt-side counters only; output
                        // arrives later via message_delta.
                        let prompt_side = parsed.to_token_usage();
                        self.usage.prompt_tokens = prompt_side.prompt_tokens;
                        self.usage.cached_tokens = prompt_side.cached_tokens;
                        self.usage.cache_creation_tokens = prompt_side.cache_creation_tokens;
                        self.usage.cache_ttl_seconds = prompt_side.cache_ttl_seconds;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Build Anthropic API messages from our message format (shared between streaming and non-streaming).
/// Parse an Anthropic computer tool_use input into a CuAction.
pub(crate) fn parse_anthropic_cu_action(
    input: &serde_json::Value,
) -> Option<crate::computer_use::CuAction> {
    use crate::computer_use::*;

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
pub(crate) fn anthropic_duration_ms(input: &serde_json::Value, default_ms: u64) -> u64 {
    match input.get("duration").and_then(|v| v.as_f64()) {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            ((seconds * 1000.0).round() as u64).min(30_000)
        }
        _ => default_ms,
    }
}

/// Claude models released before the 4.5 generation reject requests where
/// `input_tokens + max_tokens` exceeds the context window (no server-side
/// clamp; 4.5+ models cap gracefully and report
/// `model_context_window_exceeded`). Matches the Claude 3 family plus the
/// Opus 4/4.1 and Sonnet 4 pins, dated or aliased.
fn anthropic_needs_output_clamp(model: &str) -> bool {
    model.starts_with("claude-3")
        || model.starts_with("claude-opus-4-0")
        || model.starts_with("claude-opus-4-1")
        || model.starts_with("claude-opus-4-2")
        || model.starts_with("claude-sonnet-4-0")
        || model.starts_with("claude-sonnet-4-2")
}

/// Headroom subtracted on top of the input estimate when clamping: covers
/// request JSON structure and estimator error on dense scripts where
/// chars/4 underestimates. Tool schemas are measured directly (see
/// [`estimated_tools_tokens`]), not covered here.
const CLAMP_MARGIN_TOKENS: u64 = 8_192;

/// Clamp floor: estimator-noise protection only. Headroom below the floor
/// is still requested at the floor because the input estimate deliberately
/// overestimates — real headroom is at least the computed one, so the
/// floor can truncate but not 400. It is applied to the *headroom*, never
/// to the configured ceiling: an explicit low `MAX_OUTPUT_TOKENS` override
/// passes through unraised.
const CLAMP_FLOOR_TOKENS: u64 = 1_024;

/// Conservative request-size estimate in tokens, ~chars/4. The compaction
/// path's budget arithmetic rides API-reported usage held by the
/// `Conversation`, which the provider seam never sees — so this local
/// heuristic deliberately overestimates (base64 image bytes count at full
/// character weight): overestimating only shrinks the output ceiling
/// toward the floor, while underestimating would re-introduce the 400 the
/// clamp exists to prevent.
fn estimated_input_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|m| {
            m.content.len()
                + m.images.as_ref().map_or(0, |imgs| {
                    imgs.iter()
                        .map(|i| i.data.len() + i.media_type.len())
                        .sum::<usize>()
                })
                + m.tool_calls.as_ref().map_or(0, |tcs| {
                    tcs.iter()
                        .map(|tc| tc.name.len() + tc.arguments.len())
                        .sum::<usize>()
                })
        })
        .sum();
    (chars / 4) as u64
}

/// The tools payload counted with the same ~chars/4 overestimate
/// discipline. Runtime-registered MCP tool schemas are unbounded, so a
/// fixed margin can't stand in for them — a large registered tool set
/// would silently eat the margin and re-create the overflow 400 the clamp
/// exists to prevent. Only evaluated on the pre-4.5 clamp path.
fn estimated_tools_tokens(tools: Option<&[serde_json::Value]>) -> u64 {
    let chars: usize = tools
        .map(|defs| {
            defs.iter()
                .map(|def| serde_json::to_string(def).map_or(0, |s| s.len()))
                .sum()
        })
        .unwrap_or(0);
    (chars / 4) as u64
}

/// Attach rolling `cache_control: {type: "ephemeral"}` breakpoints to the
/// conversation tail. Anthropic caches the prefix up to each breakpoint, so
/// markers that advance with the conversation make every request re-read
/// the previous request's prefix at ~0.1× input price instead of re-billing
/// the whole transcript at full rate each turn.
///
/// Placement is turn-boundary based, two markers:
/// - the final block of the last user-side message (the current request's
///   tail), and
/// - the final block of the last user-side message *before the most recent
///   assistant message* — i.e. exactly the message the previous request
///   ended with, which is where its tail marker sat.
///
/// The second marker is what guarantees continuity for any batch size: a
/// turn that appends many tool_result messages ("last two user messages"
/// would land both markers inside the new batch, past Anthropic's
/// ~20-content-block cache lookback, re-billing the entire history) still
/// re-reads everything up to the previous turn at cache price; at worst the
/// new turn's own blocks bill uncached once.
///
/// Budget: 1 breakpoint on system (which also covers tools, rendered
/// before it) + 2 rolling here = 3 of the allowed 4.
fn apply_rolling_cache_breakpoints(api_messages: &mut [AnthropicMessage]) {
    // Current turn tail: the last user-side message (plain user turn or
    // tool_result carrier). Walk back over unmarkable bodies (e.g. empty
    // text) so the marker still lands as close to the tail as possible.
    let mut tail_marked_at: Option<usize> = None;
    for idx in (0..api_messages.len()).rev() {
        if api_messages[idx].role == "user" && attach_cache_control(&mut api_messages[idx].content)
        {
            tail_marked_at = Some(idx);
            break;
        }
    }
    let Some(tail_idx) = tail_marked_at else {
        return;
    };

    // Previous turn tail: the last user-side message before the most
    // recent assistant message that precedes the tail marker.
    let Some(divider) = api_messages[..tail_idx]
        .iter()
        .rposition(|m| m.role == "assistant")
    else {
        return;
    };
    for idx in (0..divider).rev() {
        if api_messages[idx].role == "user" && attach_cache_control(&mut api_messages[idx].content)
        {
            break;
        }
    }
}

/// Set `cache_control` on the final content block of a message body.
/// Plain-string content is promoted to an equivalent single text block
/// (the API-documented shorthand equivalence, so the promotion itself
/// never changes the cached prefix). Returns false when the message has
/// no block to carry the marker (e.g. empty text).
fn attach_cache_control(content: &mut serde_json::Value) -> bool {
    let marker = serde_json::json!({"type": "ephemeral"});
    match content {
        serde_json::Value::Array(blocks) => match blocks.last_mut() {
            Some(serde_json::Value::Object(block)) => {
                block.insert("cache_control".to_string(), marker);
                true
            }
            _ => false,
        },
        serde_json::Value::String(text) => {
            if text.is_empty() {
                return false;
            }
            *content = serde_json::json!([{
                "type": "text",
                "text": std::mem::take(text),
                "cache_control": marker,
            }]);
            true
        }
        _ => false,
    }
}

/// Newest image blocks kept in an outgoing Anthropic request. The API
/// rejects requests around ~100 image blocks — a ceiling a
/// screenshot-heavy session can reach below the token-based auto-compact
/// threshold now that Anthropic history is no longer image-stripped every
/// turn. 40 keeps a comfortable margin while retaining far more visual
/// context than the single-image OpenAI policy. Overflow elides the
/// *oldest* images, replacing each with a fixed placeholder block instead
/// of running into a hard 400. Below the cap this path renders identically
/// to an uncapped build.
const MAX_REQUEST_IMAGES: usize = 40;

/// Byte budget for the kept images' base64 payload — the count cap alone
/// doesn't bound the body (a handful of ~5 MiB screenshots base64-expand
/// past Anthropic's ~32 MB request ceiling while staying far under 40
/// images). 16 MiB leaves generous room for text, tool schemas, and JSON
/// structure in the remaining half of the ceiling.
const MAX_REQUEST_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Elision-boundary quantum — the hysteresis that keeps the cap from
/// permanently defeating the rolling prompt cache. A boundary recomputed
/// exactly per request would advance by one image every request at steady
/// state (one new screenshot per turn), changing an early-prefix block
/// *every* request, so the previous-tail cache marker would never hit
/// again. Quantizing the boundary to multiples of 8 images means it only
/// moves once a new step is actually required — the prefix then stays
/// byte-stable for the next ~8 images (kept count oscillates in
/// (cap−8, cap]), amortizing invalidation to roughly 1-in-8 requests. The
/// same stepped boundary serves the byte budget, so byte-driven elision
/// jumps in the same 8-image steps rather than sliding per request.
const IMAGE_ELISION_STEP: usize = 8;

/// How many oldest images to elide so the kept tail satisfies both the
/// count cap and the byte budget, with the boundary quantized to
/// [`IMAGE_ELISION_STEP`] (see there for why). Always keeps the newest
/// image, even when it alone exceeds the byte budget — the current screen
/// is what the model acts on, and a single image cannot realistically
/// breach the request ceiling the budget protects.
fn image_elision_count(image_sizes: &[usize]) -> usize {
    let total = image_sizes.len();
    let mut boundary = 0usize;
    while boundary < total {
        let kept = total - boundary;
        let kept_bytes: usize = image_sizes[boundary..].iter().sum();
        if kept <= MAX_REQUEST_IMAGES && kept_bytes <= MAX_REQUEST_IMAGE_BYTES {
            return boundary;
        }
        boundary += IMAGE_ELISION_STEP;
    }
    total.saturating_sub(1)
}

/// Render one image as a content block, or as the fixed elision
/// placeholder while `elide_remaining` is being consumed (oldest first).
fn push_image_or_placeholder(
    parts: &mut Vec<serde_json::Value>,
    img: &crate::conversation::ImageData,
    elide_remaining: &mut usize,
) {
    if *elide_remaining > 0 {
        *elide_remaining -= 1;
        parts.push(serde_json::json!({
            "type": "text",
            "text": "[image elided: superseded by newer screenshots]",
        }));
    } else {
        parts.push(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": img.media_type,
                "data": img.data,
            }
        }));
    }
}

pub(crate) fn build_anthropic_messages(
    messages: &[Message],
) -> (serde_json::Value, Vec<AnthropicMessage>) {
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

    // Oldest-first elision budget for the image cap + byte budget: only
    // user-side messages render images (tool_result carriers and plain
    // user turns), in conversation order.
    let image_sizes: Vec<usize> = messages
        .iter()
        .filter(|m| (m.role == "tool" && m.tool_call_id.is_some()) || m.role == "user")
        .flat_map(|m| {
            m.images
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|img| img.data.len())
        })
        .collect();
    let mut elide_remaining = image_elision_count(&image_sizes);

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
                        push_image_or_placeholder(&mut parts, img, &mut elide_remaining);
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
                        push_image_or_placeholder(&mut parts, img, &mut elide_remaining);
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
    apply_rolling_cache_breakpoints(&mut api_messages);
    (system, api_messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::tests::tool_msg_with_images;

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
        assert_eq!(
            anthropic_cache_ttl_seconds(8, Some(&five_minute)),
            Some(300)
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

    // --- Rolling conversation cache breakpoints ---

    fn user_text(text: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: text.to_string(),
            ..Default::default()
        }
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: text.to_string(),
            ..Default::default()
        }
    }

    fn tool_result_msg(call_id: &str, text: &str) -> Message {
        Message {
            role: "tool".to_string(),
            content: text.to_string(),
            tool_call_id: Some(call_id.to_string()),
            ..Default::default()
        }
    }

    /// Final content block of a built message, as an object.
    fn last_block(msg: &AnthropicMessage) -> &serde_json::Map<String, serde_json::Value> {
        msg.content
            .as_array()
            .and_then(|blocks| blocks.last())
            .and_then(|b| b.as_object())
            .expect("anchored message should have a block-array body")
    }

    fn has_marker(block: &serde_json::Map<String, serde_json::Value>) -> bool {
        block.get("cache_control") == Some(&serde_json::json!({"type": "ephemeral"}))
    }

    #[test]
    fn rolling_breakpoints_land_on_last_two_user_messages() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            user_text("turn one"),
            assistant_text("reply one"),
            tool_result_msg("call_1", "older result"),
            assistant_text("reply two"),
            tool_result_msg("call_2", "newest result"),
        ];
        let (system, api_msgs) = build_anthropic_messages(&messages);
        assert_eq!(api_msgs.len(), 5);

        // The two newest user-side messages carry the marker...
        assert!(has_marker(last_block(&api_msgs[4])), "newest tool_result");
        assert!(has_marker(last_block(&api_msgs[2])), "previous tool_result");
        // ...and it rides the tool_result block itself.
        assert_eq!(
            last_block(&api_msgs[4])
                .get("type")
                .and_then(|t| t.as_str()),
            Some("tool_result")
        );

        // Older user turns and assistant turns stay unmarked (byte-stable
        // prefix), and assistant bodies remain plain strings.
        assert!(api_msgs[0].content.is_string(), "older user turn untouched");
        assert!(api_msgs[1].content.is_string(), "assistant untouched");
        assert!(api_msgs[3].content.is_string(), "assistant untouched");

        // Whole-request budget: system + 2 rolling = 3 of Anthropic's 4.
        let serialized = serde_json::to_string(&(system, api_msgs)).unwrap();
        assert_eq!(serialized.matches("cache_control").count(), 3);
    }

    #[test]
    fn rolling_breakpoint_promotes_plain_string_user_message() {
        let messages = vec![user_text("hello")];
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        let block = last_block(&api_msgs[0]);
        assert_eq!(block.get("type").and_then(|t| t.as_str()), Some("text"));
        assert_eq!(block.get("text").and_then(|t| t.as_str()), Some("hello"));
        assert!(has_marker(block));
    }

    #[test]
    fn rolling_breakpoint_rides_final_image_block() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            tool_msg_with_images(),
        ];
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        // tool_result carrier: the marker goes on the tool_result block
        // (the message's final block), leaving its inner content intact.
        let block = last_block(&api_msgs[0]);
        assert_eq!(
            block.get("type").and_then(|t| t.as_str()),
            Some("tool_result")
        );
        assert!(has_marker(block));
        let inner = block["content"].as_array().unwrap();
        assert_eq!(inner[1]["type"].as_str(), Some("image"));
        assert!(inner[1].get("cache_control").is_none());
    }

    #[test]
    fn rolling_breakpoints_skip_empty_user_messages() {
        let messages = vec![user_text("real turn"), user_text("")];
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        // The empty tail can't carry a marker; the previous turn still does.
        assert!(api_msgs[1].content.is_string());
        assert!(has_marker(last_block(&api_msgs[0])));
    }

    #[test]
    fn rolling_breakpoints_straddle_a_multi_result_batch() {
        // One turn appending many tool_result messages must NOT absorb both
        // markers into the new batch: the second marker belongs on the
        // previous turn's tail (where the previous request's tail marker
        // sat), or a big batch pushes both markers past Anthropic's
        // ~20-block cache lookback and the whole history re-bills.
        let mut messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            user_text("the task"),
            assistant_text("running a large batch"),
        ];
        for i in 0..25 {
            messages.push(tool_result_msg(
                &format!("call_{i}"),
                &format!("result {i}"),
            ));
        }
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        assert_eq!(api_msgs.len(), 27);

        // Marker on the batch tail…
        assert!(has_marker(last_block(&api_msgs[26])), "batch tail");
        // …and on the previous turn's tail (the user task), NOT on the
        // second-to-last batch result.
        assert!(has_marker(last_block(&api_msgs[0])), "previous turn tail");
        for idx in 2..26 {
            let unmarked = api_msgs[idx]
                .content
                .as_array()
                .and_then(|blocks| blocks.last())
                .and_then(|b| b.as_object())
                .is_some_and(|b| !b.contains_key("cache_control"));
            assert!(unmarked, "batch interior message {idx} must stay unmarked");
        }
    }

    // --- Pre-4.5 max_tokens clamp ---

    #[test]
    fn pre45_models_clamp_max_tokens_to_context_headroom() {
        let provider = AnthropicProvider::new_plain(
            "key".to_string(),
            "claude-opus-4-1-20250805".to_string(),
            200_000,
            32_000,
        );
        // Moderate history: the family ceiling fits untouched.
        let small = vec![user_text(&"x".repeat(40_000))]; // est ~10K tokens
        assert_eq!(provider.effective_max_tokens(&small, None), 32_000);
        // Deep history: ceiling shrinks to window − estimate − margin, so
        // the request stays valid below the 90% auto-compact threshold.
        let big = vec![user_text(&"x".repeat(700_000))]; // est ~175K tokens
        assert_eq!(
            provider.effective_max_tokens(&big, None),
            200_000 - 175_000 - CLAMP_MARGIN_TOKENS
        );
        // Estimate at/over the window: the floor keeps the request shaped
        // (the estimate overestimates, so the floor truncates, never 400s).
        let huge = vec![user_text(&"x".repeat(1_000_000))];
        assert_eq!(
            provider.effective_max_tokens(&huge, None),
            CLAMP_FLOOR_TOKENS
        );
    }

    #[test]
    fn clamp_never_raises_an_explicit_low_ceiling() {
        // An explicit MAX_OUTPUT_TOKENS override below the floor is a hard
        // ceiling: the floor applies to headroom only, never to the
        // configured value.
        let provider = AnthropicProvider::new_plain(
            "key".to_string(),
            "claude-opus-4-1-20250805".to_string(),
            200_000,
            512,
        );
        let small = vec![user_text("hi")];
        assert_eq!(provider.effective_max_tokens(&small, None), 512);
        let huge = vec![user_text(&"x".repeat(1_000_000))];
        assert_eq!(provider.effective_max_tokens(&huge, None), 512);
    }

    #[test]
    fn clamp_counts_the_tools_payload() {
        // Runtime-registered MCP schemas are unbounded — a giant tool set
        // must shrink the ceiling like any other input, or it eats the
        // fixed margin and re-creates the overflow 400.
        let provider = AnthropicProvider::new_plain(
            "key".to_string(),
            "claude-opus-4-1-20250805".to_string(),
            200_000,
            32_000,
        );
        let messages = vec![user_text(&"x".repeat(40_000))]; // est ~10K tokens
        assert_eq!(provider.effective_max_tokens(&messages, None), 32_000);
        let giant_tool = serde_json::json!({
            "name": "mcp_bulk",
            "description": "y".repeat(700_000), // est ~175K tokens
            "input_schema": {"type": "object"},
        });
        let with_tools =
            provider.effective_max_tokens(&messages, Some(std::slice::from_ref(&giant_tool)));
        assert!(
            with_tools < 32_000,
            "giant tool schema must shrink the ceiling (got {with_tools})"
        );
        assert!(with_tools >= CLAMP_FLOOR_TOKENS);
    }

    #[test]
    fn post45_models_pass_configured_max_tokens_through() {
        let provider = AnthropicProvider::new_plain(
            "key".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
            200_000,
            64_000,
        );
        let huge = vec![user_text(&"x".repeat(1_000_000))];
        assert_eq!(provider.effective_max_tokens(&huge, None), 64_000);

        // Clamp family membership, dated and alias forms.
        assert!(anthropic_needs_output_clamp("claude-3-5-sonnet-20241022"));
        assert!(anthropic_needs_output_clamp("claude-opus-4-0"));
        assert!(anthropic_needs_output_clamp("claude-opus-4-1"));
        assert!(anthropic_needs_output_clamp("claude-opus-4-20250514"));
        assert!(anthropic_needs_output_clamp("claude-sonnet-4-0"));
        assert!(anthropic_needs_output_clamp("claude-sonnet-4-20250514"));
        assert!(!anthropic_needs_output_clamp("claude-haiku-4-5-20251001"));
        assert!(!anthropic_needs_output_clamp("claude-sonnet-4-5-20250929"));
    }

    // --- Request image cap ---

    #[test]
    fn image_cap_elides_oldest_in_quantized_steps() {
        // 45 single-image tool results: over the 40 cap, the boundary
        // quantizes to the 8-image step — 8 oldest elided, 37 kept (not a
        // per-request sliding 5).
        let mut messages = vec![Message {
            role: "system".to_string(),
            content: "sys".to_string(),
            ..Default::default()
        }];
        for i in 0..(MAX_REQUEST_IMAGES + 5) {
            let mut m = tool_msg_with_images();
            m.tool_call_id = Some(format!("call_{i}"));
            messages.push(m);
        }
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        let serialized = serde_json::to_string(&api_msgs).unwrap();
        assert_eq!(
            serialized.matches("\"type\":\"image\"").count(),
            MAX_REQUEST_IMAGES - 3
        );
        assert_eq!(
            serialized.matches("image elided").count(),
            IMAGE_ELISION_STEP
        );

        // Oldest carriers hold the placeholder; the newest keeps its image.
        let first = serde_json::to_string(&api_msgs[0]).unwrap();
        assert!(first.contains("image elided") && !first.contains("\"type\":\"image\""));
        let last = serde_json::to_string(api_msgs.last().unwrap()).unwrap();
        assert!(last.contains("\"type\":\"image\"") && !last.contains("image elided"));

        // Determinism: the same over-cap conversation builds byte-identical
        // requests — no per-request drift in the elision pattern.
        let (_system2, api_msgs2) = build_anthropic_messages(&messages);
        assert_eq!(serialized, serde_json::to_string(&api_msgs2).unwrap());
    }

    #[test]
    fn image_elision_boundary_moves_in_steps_not_per_request() {
        // Hysteresis: at one-new-image-per-request steady state the
        // boundary must hold still across a whole step's worth of growth,
        // then jump — otherwise an early-prefix block changes every
        // request and the previous-tail cache marker never hits.
        let sizes = |n: usize| vec![1usize; n];
        assert_eq!(image_elision_count(&sizes(MAX_REQUEST_IMAGES)), 0);
        assert_eq!(image_elision_count(&sizes(41)), IMAGE_ELISION_STEP);
        assert_eq!(image_elision_count(&sizes(45)), IMAGE_ELISION_STEP);
        assert_eq!(image_elision_count(&sizes(48)), IMAGE_ELISION_STEP);
        assert_eq!(image_elision_count(&sizes(49)), 2 * IMAGE_ELISION_STEP);
        assert_eq!(image_elision_count(&sizes(56)), 2 * IMAGE_ELISION_STEP);
    }

    #[test]
    fn image_byte_budget_elides_few_but_huge_images() {
        const MIB: usize = 1024 * 1024;
        // Four ~6 MiB images: far under the count cap, over the 16 MiB
        // byte budget → elide down (boundary quantum capped at newest-1).
        assert_eq!(image_elision_count(&[6 * MIB; 4]), 3);
        // Two 5 MiB images fit the budget: untouched.
        assert_eq!(image_elision_count(&[5 * MIB; 2]), 0);
        // A single over-budget image is still sent: the newest screenshot
        // is what the model acts on, and one image can't realistically
        // breach the request ceiling.
        assert_eq!(image_elision_count(&[20 * MIB]), 0);
    }

    #[test]
    fn image_cap_leaves_under_cap_requests_untouched() {
        let mut messages = vec![Message {
            role: "system".to_string(),
            content: "sys".to_string(),
            ..Default::default()
        }];
        for i in 0..3 {
            let mut m = tool_msg_with_images();
            m.tool_call_id = Some(format!("call_{i}"));
            messages.push(m);
        }
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        let serialized = serde_json::to_string(&api_msgs).unwrap();
        assert_eq!(serialized.matches("\"type\":\"image\"").count(), 3);
        assert_eq!(serialized.matches("image elided").count(), 0);
    }

    // --- Stream fold (the Anthropic arm of the shared SSE driver) ---

    /// A realistic Messages-API SSE transcript: prompt-side usage in
    /// message_start, split text deltas (multibyte content), a tool_use
    /// block assembled from input_json_delta fragments, and the output
    /// count in message_delta.
    const ANTHROPIC_SSE_TRANSCRIPT: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":90,\"cache_creation_input_tokens\":20}}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n",
        "\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo \\u00e9🦀\"}}\n",
        "\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"exec_command\"}}\n",
        "\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"comm\"}}\n",
        "\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"and\\\":\\\"ls\\\"}\"}}\n",
        "\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
        "\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n",
        "\n",
    );

    #[tokio::test]
    async fn anthropic_fold_assembles_text_tools_and_usage() {
        // 7-byte chunks split every line, the multibyte é, and the 🦀
        // mid-sequence — the framer must never manufacture U+FFFD or
        // fragment an event.
        let mut fold = AnthropicStreamFold::new(false, Vec::new());
        let deltas = streaming::test_support::drive_transcript(
            &mut fold,
            ANTHROPIC_SSE_TRANSCRIPT,
            7,
        )
        .await;
        assert_eq!(deltas, vec!["Hel", "lo é🦀"]);

        let response = fold.finish();
        assert_eq!(response.content, "Hello é🦀");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_1");
        assert_eq!(response.tool_calls[0].call_id, "toolu_1");
        assert_eq!(response.tool_calls[0].name, "exec_command");
        assert_eq!(response.tool_calls[0].arguments, "{\"command\":\"ls\"}");
        assert!(response.cu_calls.is_empty());
        assert!(response.raw_output.is_none());

        // Usage: prompt side folded from message_start (uncached + reads
        // + writes), completion from message_delta, total assembled at
        // finish, TTL from the flat creation counter.
        assert_eq!(response.usage.prompt_tokens, 120);
        assert_eq!(response.usage.completion_tokens, 42);
        assert_eq!(response.usage.total_tokens, 162);
        assert_eq!(response.usage.cached_tokens, 90);
        assert_eq!(response.usage.cache_creation_tokens, 20);
        assert_eq!(response.usage.cache_ttl_seconds, Some(300));
    }

    #[tokio::test]
    async fn anthropic_fold_routes_computer_tool_use_to_cu_calls() {
        let transcript = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_cu\",\"name\":\"computer\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"action\\\":\\\"screenshot\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        // CU-enabled: the computer tool becomes a CU call, not a ToolCall.
        let mut fold = AnthropicStreamFold::new(true, Vec::new());
        streaming::test_support::drive_transcript(&mut fold, transcript, 11).await;
        let response = fold.finish();
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.cu_calls.len(), 1);
        assert_eq!(response.cu_calls[0].call_id, "toolu_cu");
        assert!(matches!(
            response.cu_calls[0].actions[..],
            [crate::computer_use::CuAction::Screenshot]
        ));

        // CU-disabled: the same block is an ordinary tool call.
        let mut fold = AnthropicStreamFold::new(false, Vec::new());
        streaming::test_support::drive_transcript(&mut fold, transcript, 11).await;
        let response = fold.finish();
        assert!(response.cu_calls.is_empty());
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "computer");
    }

    #[tokio::test]
    async fn anthropic_fold_carries_rate_limit_windows_and_drops_garbage() {
        let windows = vec![crate::types::SessionLimitWindow {
            label: "req/min".to_string(),
            used_pct: Some(3),
            resets_at_epoch: None,
            status: None,
        }];
        let transcript = concat!(
            "data: not json at all\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n",
        );
        let mut fold = AnthropicStreamFold::new(false, windows);
        streaming::test_support::drive_transcript(&mut fold, transcript, 64).await;
        let response = fold.finish();
        // The unparseable payload degrades to a drop; the stream keeps
        // folding, and the pre-attached header windows survive.
        assert_eq!(response.usage.completion_tokens, 5);
        assert_eq!(response.usage.rate_limit_windows.len(), 1);
        assert_eq!(response.usage.rate_limit_windows[0].label, "req/min");
    }
}
