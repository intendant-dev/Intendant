//! The OpenAI provider: Responses-API request/response types, request-part
//! assembly, the streaming and non-streaming ChatProvider impl, and
//! computer-use action parsing.

use super::*;

// --- OpenAI (Responses API) ---

#[derive(Serialize)]
pub(crate) struct OpenAIResponsesRequest {
    model: String,
    input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Serialize, Clone)]
pub(crate) struct ReasoningConfig {
    pub(crate) effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct TextConfig {
    format: TextFormat,
}

#[derive(Serialize)]
pub(crate) struct TextFormat {
    r#type: String,
}

/// Build a Responses API message input item.
pub(crate) fn openai_message_item(role: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "role": role,
        "content": content,
    })
}

/// Build a Responses API function_call_output input item.
pub(crate) fn openai_function_call_output(call_id: &str, output: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

/// Parse an OpenAI computer_call action into a CuAction.
pub(crate) fn parse_openai_cu_action(action: &serde_json::Value) -> Option<crate::computer_use::CuAction> {
    use crate::computer_use::*;

    let action_type = action.get("type")?.as_str()?;
    let x = || action.get("x").and_then(|v| v.as_i64()).map(|v| v as i32);
    let y = || action.get("y").and_then(|v| v.as_i64()).map(|v| v as i32);

    match action_type {
        "screenshot" => Some(CuAction::Screenshot),
        "click" => {
            let button = match action.get("button").and_then(|v| v.as_str()) {
                Some("right") => MouseButton::Right,
                Some("middle") => MouseButton::Middle,
                _ => MouseButton::Left,
            };
            Some(CuAction::Click {
                x: x()?,
                y: y()?,
                button,
            })
        }
        "double_click" => Some(CuAction::DoubleClick {
            x: x()?,
            y: y()?,
            button: MouseButton::Left,
        }),
        "type" => {
            let text = action.get("text")?.as_str()?.to_string();
            Some(CuAction::Type { text })
        }
        "keypress" => {
            let keys = action.get("keys")?.as_array()?;
            let key = keys
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("+");
            Some(CuAction::Key { key })
        }
        "scroll" => {
            let scroll_x = action.get("scroll_x").and_then(|v| v.as_i64()).unwrap_or(0);
            let scroll_y = action.get("scroll_y").and_then(|v| v.as_i64()).unwrap_or(0);
            let (direction, amount) = if scroll_y < 0 {
                (ScrollDirection::Up, (-scroll_y) as i32)
            } else if scroll_y > 0 {
                (ScrollDirection::Down, scroll_y as i32)
            } else if scroll_x < 0 {
                (ScrollDirection::Left, (-scroll_x) as i32)
            } else {
                (ScrollDirection::Right, scroll_x.max(1) as i32)
            };
            // Convert pixel scroll to click counts (roughly 120px per notch)
            let clicks = (amount / 120).max(1);
            Some(CuAction::Scroll {
                x: x()?,
                y: y()?,
                direction,
                amount: clicks,
            })
        }
        "drag" => {
            let path = action.get("path")?.as_array()?;
            let start = path.first()?;
            let end = path.last()?;
            Some(CuAction::Drag {
                start_x: start.get("x")?.as_i64()? as i32,
                start_y: start.get("y")?.as_i64()? as i32,
                end_x: end.get("x")?.as_i64()? as i32,
                end_y: end.get("y")?.as_i64()? as i32,
            })
        }
        "move" => Some(CuAction::MoveMouse { x: x()?, y: y()? }),
        "wait" => {
            let ms = action.get("ms").and_then(|v| v.as_u64()).unwrap_or(1000);
            Some(CuAction::Wait { ms })
        }
        _ => None,
    }
}

#[derive(Deserialize)]
pub(crate) struct OpenAIResponsesResponse {
    output_text: Option<String>,
    output: Option<Vec<ResponsesOutputItem>>,
    usage: Option<ResponsesUsage>,
}

/// Minimal wrapper to capture raw output items as JSON values.
#[derive(Deserialize)]
pub(crate) struct OpenAIResponsesRawOutput {
    output: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
pub(crate) struct ResponsesOutputItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    content: Option<Vec<ResponsesContentItem>>,
    summary: Option<Vec<ResponsesSummaryItem>>,
    // function_call fields (type="function_call")
    /// Item ID (fc_-prefixed), used when echoing function_call back in input.
    id: Option<String>,
    /// Correlation key (call_-prefixed), used for function_call_output.
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    // computer_call fields (type="computer_call")
    actions: Option<Vec<serde_json::Value>>,
    pending_safety_checks: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
pub(crate) struct ResponsesContentItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ResponsesSummaryItem {
    text: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    /// Cached input tokens (subset of input_tokens). OpenAI Responses API
    /// returns this in `input_tokens_details.cached_tokens`.
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputTokenDetails>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesInputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    structured_output: bool,
    reasoning: Option<ReasoningConfig>,
    use_tools: bool,
    custom_tools: Option<Vec<ToolDefinition>>,
    /// When true, include native computer-use tool in API requests.
    pub cu_enabled: bool,
    /// Display dimensions for CU (width, height).
    pub cu_display: Option<(u32, u32)>,
}

impl OpenAIProvider {
    pub fn new(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let structured_output = resolve_structured_output(&model);
        let reasoning = resolve_reasoning(&model);
        let use_tools = resolve_use_tools();

        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output,
            reasoning,
            use_tools,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    #[allow(dead_code)]
    pub fn new_plain(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output: false,
            reasoning: None,
            use_tools: false,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_with_tools(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output: false,
            reasoning: None,
            use_tools: true,
            custom_tools: Some(tools),
            cu_enabled: false,
            cu_display: None,
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAIProvider {
    fn request_snapshot(
        &self,
        messages: &[Message],
        stream: bool,
    ) -> Result<(String, serde_json::Value), CallerError> {
        let (instructions, input, text, tools) = build_openai_request_parts(messages, self);
        let request = OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            instructions,
            max_output_tokens: Some(self.max_output_tokens),
            reasoning: self.reasoning.clone(),
            text,
            tools,
            stream,
        };
        Ok((
            "openai.responses.request.v1".to_string(),
            serde_json::to_value(&request).map_err(CallerError::Json)?,
        ))
    }

    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        let (instructions, input, text, tools) = build_openai_request_parts(messages, self);

        let request = OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            instructions,
            max_output_tokens: Some(self.max_output_tokens),
            reasoning: self.reasoning.clone(),
            text,
            tools,
            stream: false,
        };

        // Note: OpenAI Responses API uses automatic prompt caching for prompts
        // longer than 1024 tokens. No explicit API changes are needed — caching
        // is applied server-side and reported via usage.prompt_tokens_details.
        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;
        let response = send_with_retry(
            client,
            || {
                client
                    .post("https://api.openai.com/v1/responses")
                    .header("Authorization", format!("Bearer {}", api_key))
                    .json(&request_json)
            },
            MAX_RETRIES,
        )
        .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let body = response.text().await?;
        let resp: OpenAIResponsesResponse = serde_json::from_str(&body)?;
        // Capture raw output items for verbatim echo-back (reasoning + function_call items)
        let raw_output = serde_json::from_str::<OpenAIResponsesRawOutput>(&body)
            .ok()
            .and_then(|r| r.output);

        // Extract function_call and computer_call items from the output array
        let mut tool_calls = Vec::new();
        let mut cu_calls = Vec::new();
        if let Some(ref output_items) = resp.output {
            for item in output_items {
                match item.item_type.as_deref() {
                    Some("function_call") => {
                        if let (Some(call_id), Some(name), Some(arguments)) =
                            (&item.call_id, &item.name, &item.arguments)
                        {
                            tool_calls.push(ToolCall {
                                id: item.id.clone().unwrap_or_else(|| call_id.clone()),
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                            });
                        }
                    }
                    Some("computer_call") if self.cu_enabled => {
                        if let Some(call_id) = &item.call_id {
                            let actions = item
                                .actions
                                .as_ref()
                                .map(|arr| arr.iter().filter_map(parse_openai_cu_action).collect())
                                .unwrap_or_default();
                            let safety = item.pending_safety_checks.clone().unwrap_or_default();
                            cu_calls.push(crate::computer_use::CuToolCall {
                                call_id: call_id.clone(),
                                actions,
                                metadata: crate::computer_use::CuCallMetadata {
                                    pending_safety_checks: safety,
                                    ..Default::default()
                                },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Prefer output_text, fall back to digging into output array.
        // When tool calls are present, text content is optional.
        let content = resp
            .output_text
            .or_else(|| {
                resp.output.as_ref().and_then(|items| {
                    items.iter().find_map(|item| {
                        item.content
                            .as_ref()
                            .and_then(|contents| contents.iter().find_map(|c| c.text.clone()))
                    })
                })
            })
            .unwrap_or_default();

        let usage = resp
            .usage
            .map(|u| {
                let cached = u
                    .input_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                TokenUsage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                    total_tokens: u.total_tokens,
                    cached_tokens: cached,
                    // OpenAI has no cache-write concept and an undocumented
                    // (~5–10 min) TTL — no flavor statement.
                    ..Default::default()
                }
            })
            .unwrap_or_default();

        // Extract reasoning summary and full content if present in Responses output.
        let reasoning_summary = resp.output.as_ref().and_then(|items| {
            let parts: Vec<String> = items
                .iter()
                .filter(|item| item.item_type.as_deref() == Some("reasoning"))
                .flat_map(|item| {
                    if let Some(summary) = &item.summary {
                        summary
                            .iter()
                            .filter_map(|s| s.text.clone())
                            .collect::<Vec<String>>()
                    } else if let Some(content) = &item.content {
                        content
                            .iter()
                            .filter(|c| {
                                c.item_type
                                    .as_deref()
                                    .is_some_and(|t| t.contains("summary"))
                            })
                            .filter_map(|c| c.text.clone())
                            .collect::<Vec<String>>()
                    } else {
                        Vec::new()
                    }
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        });

        // Extract full reasoning content (all text from reasoning items, not just summaries).
        let reasoning_content = resp.output.as_ref().and_then(|items| {
            let parts: Vec<String> = items
                .iter()
                .filter(|item| item.item_type.as_deref() == Some("reasoning"))
                .flat_map(|item| {
                    let mut texts = Vec::new();
                    if let Some(content) = &item.content {
                        for c in content {
                            if let Some(text) = &c.text {
                                texts.push(text.clone());
                            }
                        }
                    }
                    texts
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        });

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary,
            reasoning_content,
            tool_calls,
            cu_calls,
            raw_output,
        })
    }

    fn name(&self) -> &str {
        "openai"
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
        let (instructions, input, text, tools) = build_openai_request_parts(messages, self);
        let request = OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            instructions,
            max_output_tokens: Some(self.max_output_tokens),
            reasoning: self.reasoning.clone(),
            text,
            tools,
            stream: true,
        };
        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;

        let response = client
            .post("https://api.openai.com/v1/responses")
            .header("Authorization", format!("Bearer {}", api_key))
            .timeout(STREAM_TIMEOUT)
            .json(&request_json)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
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
        let mut raw_output_items: Vec<serde_json::Value> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut reasoning_summary_parts = Vec::new();
        let reasoning_content_parts: Vec<String> = Vec::new();
        // Track in-progress function calls by index
        let mut pending_tools: std::collections::HashMap<usize, ToolCall> =
            std::collections::HashMap::new();
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
                if let Some(("data", data)) = parse_sse_line(&line) {
                    if data == "[DONE]" {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match event_type {
                            "response.output_text.delta" => {
                                if let Some(delta) = event.get("delta").and_then(|d| d.as_str()) {
                                    text_parts.push(delta.to_string());
                                    on_event(StreamEvent::Delta(delta.to_string()));
                                }
                            }
                            "response.output_item.added" => {
                                // Track raw output items
                                if let Some(item) = event.get("item") {
                                    raw_output_items.push(item.clone());
                                    let item_type =
                                        item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    if item_type == "function_call" {
                                        let idx = event
                                            .get("output_index")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as usize;
                                        let id = item
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let call_id = item
                                            .get("call_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let name = item
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        pending_tools.insert(
                                            idx,
                                            ToolCall {
                                                id,
                                                call_id,
                                                name,
                                                arguments: String::new(),
                                            },
                                        );
                                    }
                                }
                            }
                            "response.function_call_arguments.delta" => {
                                let idx = event
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as usize;
                                if let Some(delta) = event.get("delta").and_then(|d| d.as_str()) {
                                    if let Some(tc) = pending_tools.get_mut(&idx) {
                                        tc.arguments.push_str(delta);
                                    }
                                }
                            }
                            "response.function_call_arguments.done" => {
                                let idx = event
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as usize;
                                if let Some(tc) = pending_tools.remove(&idx) {
                                    tool_calls.push(tc);
                                }
                            }
                            "response.output_item.done" => {
                                // Update raw output with final item
                                if let Some(item) = event.get("item") {
                                    let idx = event
                                        .get("output_index")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        as usize;
                                    if idx < raw_output_items.len() {
                                        raw_output_items[idx] = item.clone();
                                    }
                                    let item_type =
                                        item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    // Parse computer_call items
                                    if item_type == "computer_call" && self.cu_enabled {
                                        if let Some(call_id) =
                                            item.get("call_id").and_then(|v| v.as_str())
                                        {
                                            let actions = item
                                                .get("actions")
                                                .and_then(|a| a.as_array())
                                                .map(|arr| {
                                                    arr.iter()
                                                        .filter_map(parse_openai_cu_action)
                                                        .collect()
                                                })
                                                .unwrap_or_default();
                                            let safety = item
                                                .get("pending_safety_checks")
                                                .and_then(|v| v.as_array())
                                                .cloned()
                                                .unwrap_or_default();
                                            cu_calls.push(crate::computer_use::CuToolCall {
                                                call_id: call_id.to_string(),
                                                actions,
                                                metadata: crate::computer_use::CuCallMetadata {
                                                    pending_safety_checks: safety,
                                                    ..Default::default()
                                                },
                                            });
                                        }
                                    }
                                    // Extract reasoning summary
                                    if item_type == "reasoning" {
                                        if let Some(summary) =
                                            item.get("summary").and_then(|s| s.as_array())
                                        {
                                            for s in summary {
                                                if let Some(text) =
                                                    s.get("text").and_then(|t| t.as_str())
                                                {
                                                    reasoning_summary_parts.push(text.to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            "response.completed" => {
                                if let Some(resp) = event.get("response") {
                                    if let Some(u) = resp.get("usage") {
                                        usage.prompt_tokens = u
                                            .get("input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        usage.completion_tokens = u
                                            .get("output_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        usage.total_tokens = u
                                            .get("total_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(
                                                usage.prompt_tokens + usage.completion_tokens,
                                            );
                                        usage.cached_tokens = u
                                            .get("input_tokens_details")
                                            .and_then(|d| d.get("cached_tokens"))
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Flush any remaining pending tool calls
        let mut remaining_indices: Vec<usize> = pending_tools.keys().copied().collect();
        remaining_indices.sort();
        for idx in remaining_indices {
            if let Some(tc) = pending_tools.remove(&idx) {
                tool_calls.push(tc);
            }
        }

        let content = text_parts.join("");
        let reasoning_summary = if reasoning_summary_parts.is_empty() {
            None
        } else {
            Some(reasoning_summary_parts.join("\n"))
        };
        let reasoning_content = if reasoning_content_parts.is_empty() {
            None
        } else {
            Some(reasoning_content_parts.join(""))
        };
        let raw_output = if raw_output_items.is_empty() {
            None
        } else {
            Some(raw_output_items)
        };

        let response = ChatResponse {
            content,
            usage,
            reasoning_summary,
            reasoning_content,
            tool_calls,
            cu_calls,
            raw_output,
        };
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// Build OpenAI request parts (shared between streaming and non-streaming).
pub(crate) fn build_openai_request_parts(
    messages: &[Message],
    provider: &OpenAIProvider,
) -> (
    Option<String>,
    Vec<serde_json::Value>,
    Option<TextConfig>,
    Option<Vec<serde_json::Value>>,
) {
    let instructions = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());

    let mut input: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.role != "system")
        .flat_map(|m| {
            let mut items = Vec::new();
            if m.role == "assistant" && m.tool_calls.is_some() {
                if let Some(ref raw) = m.raw_output {
                    items.extend(raw.iter().cloned());
                    return items;
                }
                if let Some(ref tcs) = m.tool_calls {
                    if !m.content.is_empty() {
                        items.push(openai_message_item(&m.role, &m.content));
                    }
                    for tc in tcs {
                        items.push(serde_json::json!({
                            "type": "function_call",
                            "id": tc.id,
                            "call_id": tc.call_id,
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }));
                    }
                    return items;
                }
            }
            if m.role == "tool" {
                if let Some(ref call_id) = m.tool_call_id {
                    if m.is_cu_result {
                        // Native CU result: computer_call_output format.
                        // CU images are preserved by strip_old_images so
                        // the screenshot should always be present.
                        let screenshot = m.images.as_ref().and_then(|imgs| imgs.first());
                        let mut output_item = serde_json::json!({
                            "type": "computer_call_output",
                            "call_id": call_id,
                        });
                        if let Some(img) = screenshot {
                            output_item["output"] = serde_json::json!({
                                "type": "computer_screenshot",
                                "image_url": format!("data:{};base64,{}", img.media_type, img.data),
                            });
                        }
                        items.push(output_item);
                    } else {
                        items.push(openai_function_call_output(call_id, &m.content));
                        if let Some(ref images) = m.images {
                            let mut content_parts = vec![serde_json::json!({
                                "type": "input_text",
                                "text": "Screenshot from the previous tool call:",
                            })];
                            for img in images {
                                content_parts.push(serde_json::json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{};base64,{}", img.media_type, img.data),
                                }));
                            }
                            items.push(serde_json::json!({
                                "role": "user",
                                "content": content_parts,
                            }));
                        }
                    }
                    return items;
                }
            }
            // User messages with images: multipart content
            if m.role == "user" {
                if let Some(ref images) = m.images {
                    let mut content_parts = vec![serde_json::json!({
                        "type": "input_text",
                        "text": m.content,
                    })];
                    for img in images {
                        content_parts.push(serde_json::json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", img.media_type, img.data),
                        }));
                    }
                    items.push(serde_json::json!({
                        "role": "user",
                        "content": content_parts,
                    }));
                    return items;
                }
            }
            items.push(openai_message_item(&m.role, &m.content));
            items
        })
        .collect();

    let use_structured = provider.structured_output && !provider.use_tools;
    if use_structured {
        input.insert(
            0,
            openai_message_item(
                "developer",
                "Always respond with valid JSON matching the command schema.",
            ),
        );
    }

    let text = if use_structured {
        Some(TextConfig {
            format: TextFormat {
                r#type: "json_object".to_string(),
            },
        })
    } else {
        None
    };

    let mut tools_vec: Vec<serde_json::Value> = Vec::new();
    if provider.use_tools {
        let defs = provider.tools();
        tools_vec.extend(defs.iter().map(|t| t.to_openai()));
    }
    if provider.cu_enabled {
        tools_vec.push(serde_json::json!({
            "type": "computer"
        }));
    }
    let tools = if tools_vec.is_empty() {
        None
    } else {
        Some(tools_vec)
    };

    (instructions, input, text, tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::tests::{tool_msg_with_images};

    #[test]
    fn openai_provider_name() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn responses_api_response_deserialization() {
        let json = r#"{
            "id": "resp_123",
            "object": "response",
            "output_text": "Hello from Responses API!",
            "output": [
                {
                    "content": [
                        {
                            "text": "Hello from Responses API!",
                            "type": "output_text"
                        }
                    ],
                    "role": "assistant",
                    "type": "message"
                }
            ],
            "usage": {"input_tokens": 25, "output_tokens": 8, "total_tokens": 33}
        }"#;
        let resp: OpenAIResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.output_text.as_deref(),
            Some("Hello from Responses API!")
        );
        let usage = resp.usage.unwrap();
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.total_tokens, 33);
    }

    #[test]
    fn responses_api_fallback_to_output_array() {
        let json = r#"{
            "id": "resp_456",
            "object": "response",
            "output": [
                {
                    "content": [
                        {
                            "text": "Fallback text",
                            "type": "output_text"
                        }
                    ],
                    "role": "assistant",
                    "type": "message"
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
        }"#;
        let resp: OpenAIResponsesResponse = serde_json::from_str(json).unwrap();
        assert!(resp.output_text.is_none());
        let text = resp.output.as_ref().and_then(|items| {
            items.iter().find_map(|item| {
                item.content
                    .as_ref()
                    .and_then(|contents| contents.iter().find_map(|c| c.text.clone()))
            })
        });
        assert_eq!(text.as_deref(), Some("Fallback text"));
    }

    #[test]
    fn responses_api_request_serialization() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hello")],
            instructions: Some("Be helpful.".to_string()),
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: None,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"model\":\"gpt-5.2-codex\""));
        assert!(json.contains("\"instructions\":\"Be helpful.\""));
        assert!(json.contains("\"max_output_tokens\":128000"));
        assert!(json.contains("\"role\":\"user\""));
    }

    #[test]
    fn responses_api_request_omits_null_instructions() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: None,
            reasoning: None,
            text: None,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("instructions"));
        assert!(!json.contains("max_output_tokens"));
        assert!(!json.contains("reasoning"));
        assert!(!json.contains("text"));
        assert!(!json.contains("tools"));
    }

    #[test]
    fn responses_api_request_with_reasoning() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: Some(128_000),
            reasoning: Some(ReasoningConfig {
                effort: "high".to_string(),
                summary: Some("auto".to_string()),
            }),
            text: None,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"reasoning\""));
        assert!(json.contains("\"effort\":\"high\""));
        assert!(json.contains("\"summary\":\"auto\""));
    }

    #[test]
    fn responses_api_request_reasoning_without_summary() {
        let request = OpenAIResponsesRequest {
            model: "o3-mini".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: Some(100_000),
            reasoning: Some(ReasoningConfig {
                effort: "medium".to_string(),
                summary: None,
            }),
            text: None,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"effort\":\"medium\""));
        assert!(!json.contains("\"summary\""));
    }

    #[test]
    fn responses_api_request_with_structured_output() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: Some(TextConfig {
                format: TextFormat {
                    r#type: "json_object".to_string(),
                },
            }),
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"text\""));
        assert!(json.contains("\"json_object\""));
    }

    #[test]
    fn responses_api_request_with_tools() {
        let tool_defs = crate::tools::all_tools();
        let tools: Vec<serde_json::Value> = tool_defs.iter().map(|t| t.to_openai()).collect();
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "list files")],
            instructions: Some("You are an agent.".to_string()),
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: None,
            tools: Some(tools),
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tools\""));
        assert!(json.contains("exec_command"));
        // When tools are present, text/json_object should not be
        assert!(!json.contains("json_object"));
    }

    #[test]
    fn responses_api_function_call_deserialization() {
        let json = r#"{
            "output": [
                {
                    "id": "fc_abc123",
                    "type": "function_call",
                    "call_id": "call_abc123",
                    "name": "exec_command",
                    "arguments": "{\"nonce\":1,\"command\":\"ls -la\"}"
                },
                {
                    "id": "fc_def456",
                    "type": "function_call",
                    "call_id": "call_def456",
                    "name": "fetch_status",
                    "arguments": "{\"nonce\":1,\"status_type\":\"stdout\"}"
                }
            ],
            "usage": {"input_tokens": 100, "output_tokens": 50, "total_tokens": 150}
        }"#;
        let resp: OpenAIResponsesResponse = serde_json::from_str(json).unwrap();
        let items = resp.output.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item_type.as_deref(), Some("function_call"));
        assert_eq!(items[0].id.as_deref(), Some("fc_abc123"));
        assert_eq!(items[0].call_id.as_deref(), Some("call_abc123"));
        assert_eq!(items[0].name.as_deref(), Some("exec_command"));
        assert!(items[0].arguments.as_ref().unwrap().contains("ls -la"));
        assert_eq!(items[1].id.as_deref(), Some("fc_def456"));
        assert_eq!(items[1].name.as_deref(), Some("fetch_status"));
    }

    #[test]
    fn openai_function_call_output_format() {
        let item = openai_function_call_output("call_abc", "1c0");
        assert_eq!(item["type"].as_str(), Some("function_call_output"));
        assert_eq!(item["call_id"].as_str(), Some("call_abc"));
        assert_eq!(item["output"].as_str(), Some("1c0"));
    }

    #[test]
    fn responses_role_mapping_preserves_developer() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "System prompt".to_string(),
                ..Default::default()
            },
            Message {
                role: "developer".to_string(),
                content: "Developer note".to_string(),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                ..Default::default()
            },
            Message {
                role: "assistant".to_string(),
                content: "Hi".to_string(),
                ..Default::default()
            },
        ];

        let instructions = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());
        assert_eq!(instructions.as_deref(), Some("System prompt"));

        let input: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| openai_message_item(&m.role, &m.content))
            .collect();

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"].as_str(), Some("developer"));
        assert_eq!(input[1]["role"].as_str(), Some("user"));
        assert_eq!(input[2]["role"].as_str(), Some("assistant"));
    }

    #[test]
    fn openai_provider_stores_config() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        assert_eq!(provider.max_output_tokens(), 128_000);
        // gpt-5 supports structured output by default
        assert!(provider.structured_output);
    }

    #[test]
    fn openai_provider_use_tools_trait() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        // use_tools depends on env, but tools() should return matching vec
        if provider.use_tools() {
            assert!(!provider.tools().is_empty());
        } else {
            assert!(provider.tools().is_empty());
        }
    }

    #[test]
    fn openai_request_stream_field_serialization() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: None,
            reasoning: None,
            text: None,
            tools: None,
            stream: true,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn openai_request_no_stream_when_false() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: None,
            reasoning: None,
            text: None,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("stream"));
    }

    #[test]
    fn openai_builder_includes_image_after_tool_result() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-4".to_string(), 128_000, 16_384);
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            tool_msg_with_images(),
        ];
        let (_instr, input, _text, _tools) = build_openai_request_parts(&messages, &provider);
        // Should have function_call_output + user message with image
        assert!(input.len() >= 2);
        let image_msg = &input[1];
        assert_eq!(image_msg["role"].as_str(), Some("user"));
        let content = image_msg["content"].as_array().unwrap();
        assert_eq!(content[0]["type"].as_str(), Some("input_text"));
        assert_eq!(content[1]["type"].as_str(), Some("input_image"));
        let url = content[1]["image_url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn openai_builder_no_image_without_images_field() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-4".to_string(), 128_000, 16_384);
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
        let (_instr, input, _text, _tools) = build_openai_request_parts(&messages, &provider);
        // Should have only the function_call_output, no user image message
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"].as_str(), Some("function_call_output"));
    }
}
