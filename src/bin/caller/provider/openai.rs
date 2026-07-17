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
pub(crate) fn parse_openai_cu_action(
    action: &serde_json::Value,
) -> Option<crate::computer_use::CuAction> {
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
    /// GPT-5.6+ prompt tokens written to cache this request.
    #[serde(default)]
    cache_write_tokens: u64,
}

impl ResponsesUsage {
    fn into_token_usage(self) -> TokenUsage {
        let details = self
            .input_tokens_details
            .unwrap_or(ResponsesInputTokenDetails {
                cached_tokens: 0,
                cache_write_tokens: 0,
            });
        TokenUsage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            cached_tokens: details.cached_tokens,
            cache_creation_tokens: details.cache_write_tokens,
            // GPT-5.6's explicit prompt cache currently has a 30-minute TTL.
            cache_ttl_seconds: (details.cache_write_tokens > 0).then_some(30 * 60),
            ..Default::default()
        }
    }
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
        // Serialize once, straight to bytes — `to_value` + `.json()` walked
        // the full request (images included) twice per call.
        let request_body = serde_json::to_vec(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;
        let response = send_with_retry(
            client,
            || {
                client
                    .post("https://api.openai.com/v1/responses")
                    .header("Authorization", format!("Bearer {}", api_key))
                    .header("content-type", "application/json")
                    .body(request_body.clone())
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

        // Parse the body once; the typed view and the raw echo-back items
        // both project from the same DOM (this used to re-tokenize the
        // full body a second time).
        let body: serde_json::Value = serde_json::from_str(&response.text().await?)?;
        // Capture raw output items for verbatim echo-back (reasoning + function_call items)
        let raw_output = body.get("output").and_then(|o| o.as_array()).cloned();
        let resp: OpenAIResponsesResponse = serde_json::from_value(body)?;

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
            .map(ResponsesUsage::into_token_usage)
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

    fn reasoning_effort(&self) -> Option<String> {
        self.reasoning.as_ref().map(|r| r.effort.clone())
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
        let request_body = serde_json::to_vec(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;

        // Same retry policy as non-streaming: the status is known before any
        // body bytes stream, so a 429/5xx at request-open retries with
        // backoff instead of killing the session turn.
        let response = send_with_retry(
            client,
            || {
                client
                    .post("https://api.openai.com/v1/responses")
                    .header("Authorization", format!("Bearer {}", api_key))
                    .header("content-type", "application/json")
                    .timeout(STREAM_TIMEOUT)
                    .body(request_body.clone())
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

        let mut fold = OpenAIStreamFold::new(self.cu_enabled);
        streaming::run_sse_stream(ProviderHttpResponse::Direct(response), &mut fold, on_event)
            .await
            .map_err(streaming::StreamFailure::into_caller_error)?;
        let response = fold.finish();
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// The OpenAI Responses-API arm of the shared SSE driver: exactly the
/// per-event mutable state the old hand-rolled loop carried (pending
/// function calls by output index, raw echo-back items, usage from
/// `response.completed`), with the mechanics living in
/// `provider::streaming`.
pub(crate) struct OpenAIStreamFold {
    cu_enabled: bool,
    json: streaming::EventJson,
    text_parts: Vec<String>,
    tool_calls: Vec<ToolCall>,
    cu_calls: Vec<crate::computer_use::CuToolCall>,
    raw_output_items: Vec<serde_json::Value>,
    usage: TokenUsage,
    reasoning_summary_parts: Vec<String>,
    /// Never populated by the streaming vocabulary (full reasoning
    /// content only arrives on the non-streaming path); kept so the final
    /// assembly matches the old loop field-for-field.
    reasoning_content_parts: Vec<String>,
    /// In-progress function calls by output index.
    pending_tools: std::collections::HashMap<usize, ToolCall>,
}

impl OpenAIStreamFold {
    pub(crate) fn new(cu_enabled: bool) -> Self {
        Self {
            cu_enabled,
            json: streaming::EventJson::new(),
            text_parts: Vec::new(),
            tool_calls: Vec::new(),
            cu_calls: Vec::new(),
            raw_output_items: Vec::new(),
            usage: TokenUsage::default(),
            reasoning_summary_parts: Vec::new(),
            reasoning_content_parts: Vec::new(),
            pending_tools: std::collections::HashMap::new(),
        }
    }

    /// Assemble the final response after the stream ends.
    pub(crate) fn finish(mut self) -> ChatResponse {
        // Flush any remaining pending tool calls
        let mut remaining_indices: Vec<usize> = self.pending_tools.keys().copied().collect();
        remaining_indices.sort();
        for idx in remaining_indices {
            if let Some(tc) = self.pending_tools.remove(&idx) {
                self.tool_calls.push(tc);
            }
        }

        let content = self.text_parts.join("");
        let reasoning_summary = if self.reasoning_summary_parts.is_empty() {
            None
        } else {
            Some(self.reasoning_summary_parts.join("\n"))
        };
        let reasoning_content = if self.reasoning_content_parts.is_empty() {
            None
        } else {
            Some(self.reasoning_content_parts.join(""))
        };
        let raw_output = if self.raw_output_items.is_empty() {
            None
        } else {
            Some(self.raw_output_items)
        };

        ChatResponse {
            content,
            usage: self.usage,
            reasoning_summary,
            reasoning_content,
            tool_calls: self.tool_calls,
            cu_calls: self.cu_calls,
            raw_output,
        }
    }
}

impl streaming::SseFold for OpenAIStreamFold {
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
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(|d| d.as_str()) {
                    self.text_parts.push(delta.to_string());
                    on_event(StreamEvent::Delta(delta.to_string()));
                }
            }
            "response.output_item.added" => {
                // Track raw output items
                if let Some(item) = event.get("item") {
                    self.raw_output_items.push(item.clone());
                    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if item_type == "function_call" {
                        let idx = event
                            .get("output_index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
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
                        self.pending_tools.insert(
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
                    .unwrap_or(0) as usize;
                if let Some(delta) = event.get("delta").and_then(|d| d.as_str()) {
                    if let Some(tc) = self.pending_tools.get_mut(&idx) {
                        tc.arguments.push_str(delta);
                    }
                }
            }
            "response.function_call_arguments.done" => {
                let idx = event
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if let Some(tc) = self.pending_tools.remove(&idx) {
                    self.tool_calls.push(tc);
                }
            }
            "response.output_item.done" => {
                // Update raw output with final item
                if let Some(item) = event.get("item") {
                    let idx = event
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    if idx < self.raw_output_items.len() {
                        self.raw_output_items[idx] = item.clone();
                    }
                    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    // Parse computer_call items
                    if item_type == "computer_call" && self.cu_enabled {
                        if let Some(call_id) = item.get("call_id").and_then(|v| v.as_str()) {
                            let actions = item
                                .get("actions")
                                .and_then(|a| a.as_array())
                                .map(|arr| {
                                    arr.iter().filter_map(parse_openai_cu_action).collect()
                                })
                                .unwrap_or_default();
                            let safety = item
                                .get("pending_safety_checks")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            self.cu_calls.push(crate::computer_use::CuToolCall {
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
                        if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                            for s in summary {
                                if let Some(text) = s.get("text").and_then(|t| t.as_str()) {
                                    self.reasoning_summary_parts.push(text.to_string());
                                }
                            }
                        }
                    }
                }
            }
            "response.completed" => {
                if let Some(resp) = event.get("response") {
                    if let Some(u) = resp.get("usage") {
                        self.usage.prompt_tokens = u
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.usage.completion_tokens = u
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.usage.total_tokens = u
                            .get("total_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(self.usage.prompt_tokens + self.usage.completion_tokens);
                        self.usage.cached_tokens = u
                            .get("input_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.usage.cache_creation_tokens = u
                            .get("input_tokens_details")
                            .and_then(|d| d.get("cache_write_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.usage.cache_ttl_seconds =
                            (self.usage.cache_creation_tokens > 0).then_some(30 * 60);
                    }
                }
            }
            _ => {}
        }
        Ok(())
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
    use crate::provider::tests::tool_msg_with_images;

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
    fn responses_api_usage_accounts_for_gpt_5_6_cache_writes() {
        let json = r#"{
            "input_tokens": 3000,
            "output_tokens": 400,
            "total_tokens": 3400,
            "input_tokens_details": {
                "cached_tokens": 1000,
                "cache_write_tokens": 750
            }
        }"#;
        let usage: ResponsesUsage = serde_json::from_str(json).unwrap();
        let normalized = usage.into_token_usage();
        assert_eq!(normalized.prompt_tokens, 3000);
        assert_eq!(normalized.cached_tokens, 1000);
        assert_eq!(normalized.cache_creation_tokens, 750);
        assert_eq!(normalized.cache_ttl_seconds, Some(1800));
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

    // --- Stream fold (the OpenAI arm of the shared SSE driver) ---

    /// A realistic Responses-API SSE transcript: a function call assembled
    /// from argument deltas, split text deltas (multibyte content), the
    /// done-event overwrite of the raw echo-back item, a reasoning
    /// summary, usage in `response.completed`, and the trailing `[DONE]`.
    const OPENAI_SSE_TRANSCRIPT: &str = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"exec_command\",\"arguments\":\"\"}}\n",
        "\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"command\\\":\"}\n",
        "\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"ls\\\"}\"}\n",
        "\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0}\n",
        "\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\"}}\n",
        "\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi \"}\n",
        "\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"🦀\"}\n",
        "\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"exec_command\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\",\"status\":\"completed\"}}\n",
        "\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thought about it\"}]}}\n",
        "\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":25,\"total_tokens\":125,\"input_tokens_details\":{\"cached_tokens\":80,\"cache_write_tokens\":10}}}}\n",
        "\n",
        "data: [DONE]\n",
        "\n",
    );

    #[tokio::test]
    async fn openai_fold_assembles_tools_text_raw_output_and_usage() {
        let mut fold = OpenAIStreamFold::new(false);
        let deltas =
            streaming::test_support::drive_transcript(&mut fold, OPENAI_SSE_TRANSCRIPT, 7).await;
        assert_eq!(deltas, vec!["Hi ", "🦀"]);

        let response = fold.finish();
        assert_eq!(response.content, "Hi 🦀");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "fc_1");
        assert_eq!(response.tool_calls[0].call_id, "call_1");
        assert_eq!(response.tool_calls[0].name, "exec_command");
        assert_eq!(response.tool_calls[0].arguments, "{\"command\":\"ls\"}");

        // Raw echo-back items: the done event replaced index 0 with the
        // final item; the out-of-range done (index 2) was ignored but its
        // reasoning summary still extracted.
        let raw = response.raw_output.as_ref().unwrap();
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0]["status"].as_str(), Some("completed"));
        assert_eq!(raw[1]["type"].as_str(), Some("message"));
        assert_eq!(response.reasoning_summary.as_deref(), Some("thought about it"));
        assert_eq!(response.reasoning_content, None);

        assert_eq!(response.usage.prompt_tokens, 100);
        assert_eq!(response.usage.completion_tokens, 25);
        assert_eq!(response.usage.total_tokens, 125);
        assert_eq!(response.usage.cached_tokens, 80);
        assert_eq!(response.usage.cache_creation_tokens, 10);
        assert_eq!(response.usage.cache_ttl_seconds, Some(1800));
    }

    #[tokio::test]
    async fn openai_fold_flushes_pending_tools_in_index_order() {
        // Calls whose arguments.done never arrives flush at finish,
        // ordered by output index — exactly the old loop's tail flush.
        let transcript = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_b\",\"call_id\":\"call_b\",\"name\":\"second\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_a\",\"call_id\":\"call_a\",\"name\":\"first\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{}\"}\n\n",
        );
        let mut fold = OpenAIStreamFold::new(false);
        streaming::test_support::drive_transcript(&mut fold, transcript, 64).await;
        let response = fold.finish();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].name, "first");
        assert_eq!(response.tool_calls[0].arguments, "{}");
        assert_eq!(response.tool_calls[1].name, "second");
    }

    #[tokio::test]
    async fn openai_fold_routes_computer_calls_when_cu_enabled() {
        let transcript = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"computer_call\",\"call_id\":\"cu_1\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"computer_call\",\"call_id\":\"cu_1\",\"actions\":[{\"type\":\"click\",\"x\":10,\"y\":20,\"button\":\"left\"}],\"pending_safety_checks\":[{\"id\":\"sc_1\"}]}}\n\n",
        );
        let mut fold = OpenAIStreamFold::new(true);
        streaming::test_support::drive_transcript(&mut fold, transcript, 32).await;
        let response = fold.finish();
        assert_eq!(response.cu_calls.len(), 1);
        assert_eq!(response.cu_calls[0].call_id, "cu_1");
        assert!(matches!(
            response.cu_calls[0].actions[..],
            [crate::computer_use::CuAction::Click { x: 10, y: 20, .. }]
        ));
        assert_eq!(
            response.cu_calls[0].metadata.pending_safety_checks.len(),
            1
        );

        // CU-disabled: the computer_call is ignored (no CU lane, no tool
        // call), matching the old loop's guard.
        let mut fold = OpenAIStreamFold::new(false);
        streaming::test_support::drive_transcript(&mut fold, transcript, 32).await;
        let response = fold.finish();
        assert!(response.cu_calls.is_empty());
        assert!(response.tool_calls.is_empty());
    }
}
