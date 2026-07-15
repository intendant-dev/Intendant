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
            max_tokens: self.max_output_tokens,
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

        // Parse SSE stream
        let mut text_parts = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut cu_calls: Vec<crate::computer_use::CuToolCall> = Vec::new();
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
                                                cu_calls.push(crate::computer_use::CuToolCall {
                                                    call_id: current_tool_id.clone(),
                                                    actions: vec![action],
                                                    metadata: crate::computer_use::CuCallMetadata::default(),
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
                                    if let Some(parsed) = msg.get("usage").cloned().and_then(|u| {
                                        serde_json::from_value::<AnthropicUsage>(u).ok()
                                    }) {
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

/// Number of rolling `cache_control` breakpoints placed on the conversation
/// tail. Two (not one) so the chain survives Anthropic's ~20-content-block
/// cache lookback: the marker on the previous user turn sits exactly where
/// last request's tail marker sat, guaranteeing a hit there even when a
/// large tool batch pushes the newest marker more than 20 blocks forward.
const ROLLING_CACHE_BREAKPOINTS: usize = 2;

/// Attach `cache_control: {type: "ephemeral"}` to the final content block of
/// the last [`ROLLING_CACHE_BREAKPOINTS`] user-side messages. Anthropic
/// caches the prefix up to each breakpoint, so a marker that advances with
/// the conversation makes every request re-read the previous request's
/// prefix at ~0.1× input price instead of re-billing the whole transcript
/// at full rate each turn. Budget: 1 breakpoint on system (which also
/// covers tools, rendered before it) + 2 rolling here = 3 of the allowed 4.
fn apply_rolling_cache_breakpoints(api_messages: &mut [AnthropicMessage]) {
    let mut remaining = ROLLING_CACHE_BREAKPOINTS;
    for msg in api_messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        // Anchor on user-side messages only (plain user turns and
        // tool_result carriers): the last message of every request is
        // user-side, and anchoring the same two positions across
        // consecutive requests is what makes the prefix re-readable.
        if msg.role == "user" && attach_cache_control(&mut msg.content) {
            remaining -= 1;
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
}
