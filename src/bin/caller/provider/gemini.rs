//! The Gemini provider: generateContent request assembly, the streaming
//! and non-streaming ChatProvider impl, and computer-use function parsing.

use super::*;

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
    pub(crate) async fn post_generate(
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
pub(crate) fn gemini_role(role: &str) -> &str {
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
                            cu_calls.push(crate::computer_use::CuToolCall {
                                call_id: id,
                                actions: vec![action],
                                metadata: crate::computer_use::CuCallMetadata::default(),
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
        let mut cu_calls: Vec<crate::computer_use::CuToolCall> = Vec::new();
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
                                        cu_calls.push(crate::computer_use::CuToolCall {
                                            call_id: id,
                                            actions: vec![action],
                                            metadata: crate::computer_use::CuCallMetadata::default(
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
pub(crate) fn build_gemini_request_parts(
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
pub(crate) const GEMINI_CU_FUNCTIONS: &[&str] = &[
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
pub(crate) fn parse_gemini_cu_action(
    name: &str,
    args: &serde_json::Value,
    display_width: u32,
    display_height: u32,
) -> Option<crate::computer_use::CuAction> {
    use crate::computer_use::*;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::tests::tool_msg_with_images;

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
