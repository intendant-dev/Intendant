//! OpenAI Realtime API WebSocket voice provider.

use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{CloseEvent, MessageEvent, Request, RequestInit, RequestMode, Response, WebSocket};

use crate::callbacks::Callbacks;

const DEFAULT_MODEL: &str = "gpt-4o-realtime-preview";
const SESSION_URL: &str = "https://api.openai.com/v1/realtime/sessions";

pub struct OpenAIProvider {
    ws: Option<WebSocket>,
    pub connected: bool,
    model: String,
    callbacks: Rc<Callbacks>,
    _onopen: Option<Closure<dyn FnMut()>>,
    _onmessage: Option<Closure<dyn FnMut(MessageEvent)>>,
    _onclose: Option<Closure<dyn FnMut(CloseEvent)>>,
    _onerror: Option<Closure<dyn FnMut()>>,
}

impl OpenAIProvider {
    pub fn new(callbacks: Rc<Callbacks>) -> Self {
        Self {
            ws: None,
            connected: false,
            model: DEFAULT_MODEL.to_string(),
            callbacks,
            _onopen: None,
            _onmessage: None,
            _onclose: None,
            _onerror: None,
        }
    }

    /// Connect to OpenAI Realtime. This is async because it first fetches
    /// an ephemeral session key via REST, then opens the WebSocket.
    pub async fn connect(
        &mut self,
        api_key: &str,
        model: Option<&str>,
        system_prompt: &str,
        tools: &JsValue,
    ) {
        if let Some(m) = model {
            self.model = m.to_string();
        }

        self.disconnect();

        // Step 1: Fetch ephemeral session key
        let ephemeral_key = match self.fetch_session_key(api_key).await {
            Ok(key) => key,
            Err(e) => {
                self.callbacks
                    .invoke_error(&format!("OpenAI session: {}", e));
                return;
            }
        };

        // Step 2: Open WebSocket with subprotocols
        let url = format!("wss://api.openai.com/v1/realtime?model={}", self.model);
        let protocols = js_sys::Array::new();
        protocols.push(&JsValue::from_str("realtime"));
        protocols.push(&JsValue::from_str(&format!(
            "openai-insecure-api-key.{}",
            ephemeral_key
        )));
        protocols.push(&JsValue::from_str("openai-beta.realtime-v1"));

        let ws = match WebSocket::new_with_str_sequence(&url, &protocols) {
            Ok(ws) => ws,
            Err(e) => {
                self.callbacks
                    .invoke_error(&format!("OpenAI connect failed: {:?}", e));
                return;
            }
        };

        // Build setup message
        let setup_msg = Self::build_setup_message(system_prompt, tools);

        // onopen
        let ws_setup = ws.clone();
        let onopen = Closure::wrap(Box::new(move || {
            let _ = ws_setup.send_with_str(&setup_msg);
        }) as Box<dyn FnMut()>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        // onmessage
        let callbacks = self.callbacks.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Some(text) = e.data().as_string() {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                    Self::handle_message_static(&callbacks, &msg);
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onclose
        let callbacks_close = self.callbacks.clone();
        let onclose = Closure::wrap(Box::new(move |e: CloseEvent| {
            callbacks_close.invoke_error(&format!("OpenAI disconnected ({})", e.code()));
        }) as Box<dyn FnMut(CloseEvent)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        // onerror
        let callbacks_err = self.callbacks.clone();
        let onerror = Closure::wrap(Box::new(move || {
            callbacks_err.invoke_error("OpenAI WebSocket error");
        }) as Box<dyn FnMut()>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        self.ws = Some(ws);
        self._onopen = Some(onopen);
        self._onmessage = Some(onmessage);
        self._onclose = Some(onclose);
        self._onerror = Some(onerror);
    }

    async fn fetch_session_key(&self, api_key: &str) -> Result<String, String> {
        let opts = RequestInit::new();
        opts.set_method("POST");
        opts.set_mode(RequestMode::Cors);

        let body = serde_json::json!({
            "model": self.model,
            "modalities": ["audio", "text"],
            "voice": "alloy"
        });
        opts.set_body(&JsValue::from_str(&body.to_string()));

        let request =
            Request::new_with_str_and_init(SESSION_URL, &opts).map_err(|e| format!("{:?}", e))?;

        request
            .headers()
            .set("Authorization", &format!("Bearer {}", api_key))
            .map_err(|e| format!("{:?}", e))?;
        request
            .headers()
            .set("Content-Type", "application/json")
            .map_err(|e| format!("{:?}", e))?;

        let window = web_sys::window().ok_or("no window")?;
        let resp_value = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|e| format!("{:?}", e))?;

        let resp: Response = resp_value.dyn_into().map_err(|_| "not a Response")?;
        if !resp.ok() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let json = JsFuture::from(resp.json().map_err(|e| format!("{:?}", e))?)
            .await
            .map_err(|e| format!("{:?}", e))?;

        let data: serde_json::Value =
            serde_wasm_bindgen::from_value(json).map_err(|e| format!("{}", e))?;

        data["client_secret"]["value"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| "No client_secret in response".to_string())
    }

    fn build_setup_message(system_prompt: &str, tools: &JsValue) -> String {
        let tools_val: serde_json::Value =
            serde_wasm_bindgen::from_value(tools.clone()).unwrap_or(serde_json::Value::Array(vec![]));

        // Convert tool definitions to OpenAI format
        let openai_tools: Vec<serde_json::Value> = tools_val
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "name": t["name"],
                            "description": t["description"],
                            "parameters": t["parameters"],
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let msg = serde_json::json!({
            "type": "session.update",
            "session": {
                "modalities": ["audio", "text"],
                "instructions": system_prompt,
                "voice": "alloy",
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "tools": openai_tools,
            }
        });
        msg.to_string()
    }

    fn handle_message_static(callbacks: &Callbacks, msg: &serde_json::Value) {
        let msg_type = msg["type"].as_str().unwrap_or("");
        match msg_type {
            "session.created" | "session.updated" => {
                callbacks.invoke_voice_ready();
            }
            "response.audio.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    callbacks.invoke_voice_audio(delta);
                }
            }
            "response.text.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    callbacks.invoke_voice_text(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let call = serde_json::json!({
                    "name": msg["name"],
                    "args": serde_json::from_str::<serde_json::Value>(
                        msg["arguments"].as_str().unwrap_or("{}")
                    ).unwrap_or_default(),
                    "id": msg["item_id"],
                    "call_id": msg["call_id"],
                });
                let call_js = serde_wasm_bindgen::to_value(&call).unwrap_or(JsValue::NULL);
                callbacks.invoke_voice_tool_call(&call_js);
            }
            "input_audio_buffer.speech_started" => {
                callbacks.invoke_voice_interrupted();
            }
            _ => {}
        }
    }

    pub fn send_audio(&self, base64_pcm: &str) {
        if let Some(ref ws) = self.ws {
            let msg = serde_json::json!({
                "type": "input_audio_buffer.append",
                "audio": base64_pcm
            });
            let _ = ws.send_with_str(&msg.to_string());
        }
    }

    pub fn send_text(&self, text: &str) {
        if let Some(ref ws) = self.ws {
            let msg = serde_json::json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }
            });
            let _ = ws.send_with_str(&msg.to_string());
            let _ = ws.send_with_str(r#"{"type":"response.create"}"#);
        }
    }

    pub fn send_tool_response(&self, call: &JsValue, result: &JsValue) {
        if let Some(ref ws) = self.ws {
            let call_val: serde_json::Value =
                serde_wasm_bindgen::from_value(call.clone()).unwrap_or_default();
            let result_val: serde_json::Value =
                serde_wasm_bindgen::from_value(result.clone()).unwrap_or_default();

            let call_id = call_val["call_id"]
                .as_str()
                .or_else(|| call_val["id"].as_str())
                .or_else(|| call_val["name"].as_str())
                .unwrap_or("");

            let msg = serde_json::json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": serde_json::to_string(&result_val).unwrap_or_default()
                }
            });
            let _ = ws.send_with_str(&msg.to_string());
            let _ = ws.send_with_str(r#"{"type":"response.create"}"#);
        }
    }

    pub fn disconnect(&mut self) {
        if let Some(ref ws) = self.ws {
            let _ = ws.close();
        }
        self.ws = None;
        self.connected = false;
        self._onopen = None;
        self._onmessage = None;
        self._onclose = None;
        self._onerror = None;
    }
}
