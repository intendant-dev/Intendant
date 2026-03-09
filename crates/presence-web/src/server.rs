//! Server WebSocket connection to the intendant web gateway.
//!
//! Handles: TUI ANSI frames, state snapshots, tool requests/responses,
//! outbound events, keyboard/resize input, live_connected/disconnected.

use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use crate::callbacks::Callbacks;

/// Reconnect delay in milliseconds.
const RECONNECT_DELAY_MS: i32 = 3000;

/// Server connection state.
pub struct ServerConnection {
    ws: Option<WebSocket>,
    url: String,
    connected: bool,
    /// Whether the voice model is live (for re-sending live_connected on reconnect).
    voice_live: bool,
    callbacks: Rc<Callbacks>,
    /// Closures must be stored to prevent drop while WebSocket holds references.
    _onopen: Option<Closure<dyn FnMut()>>,
    _onmessage: Option<Closure<dyn FnMut(MessageEvent)>>,
    _onclose: Option<Closure<dyn FnMut(CloseEvent)>>,
    _onerror: Option<Closure<dyn FnMut()>>,
    /// Handles server messages (term, state_snapshot, tool_response, events).
    /// Stored as a shared handler so the main module can process messages.
    on_message_handler: Option<Rc<RefCell<Box<dyn FnMut(serde_json::Value)>>>>,
}

impl ServerConnection {
    pub fn new(callbacks: Rc<Callbacks>) -> Self {
        Self {
            ws: None,
            url: String::new(),
            connected: false,
            voice_live: false,
            callbacks,
            _onopen: None,
            _onmessage: None,
            _onclose: None,
            _onerror: None,
            on_message_handler: None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn set_voice_live(&mut self, live: bool) {
        self.voice_live = live;
    }

    /// Set a handler for parsed server messages.
    pub fn set_message_handler(
        &mut self,
        handler: Rc<RefCell<Box<dyn FnMut(serde_json::Value)>>>,
    ) {
        self.on_message_handler = Some(handler);
    }

    /// Connect to the server WebSocket.
    pub fn connect(&mut self, url: &str) {
        // Close any existing connection
        self.disconnect();
        self.url = url.to_string();

        let ws = match WebSocket::new(url) {
            Ok(ws) => ws,
            Err(e) => {
                self.callbacks
                    .invoke_error(&format!("WebSocket connect failed: {:?}", e));
                return;
            }
        };

        // Set up event handlers using closures stored in self
        let callbacks = self.callbacks.clone();
        let url_clone = url.to_string();

        // onopen
        let callbacks_open = callbacks.clone();
        // We need shared mutable state for the connection flag and voice_live.
        // Since WASM is single-threaded, Rc<RefCell<>> is safe.
        let connected_flag = Rc::new(RefCell::new(false));
        let voice_live_flag = Rc::new(RefCell::new(self.voice_live));
        let ws_clone = ws.clone();

        let connected_open = connected_flag.clone();
        let voice_open = voice_live_flag.clone();
        let onopen = Closure::wrap(Box::new(move || {
            *connected_open.borrow_mut() = true;
            callbacks_open.invoke_server_state(true);
            // Re-send live_connected if voice model was active before reconnect
            if *voice_open.borrow() {
                let msg = serde_json::json!({"t": "live_connected"});
                let _ = ws_clone.send_with_str(&msg.to_string());
            }
        }) as Box<dyn FnMut()>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        // onmessage
        let handler = self.on_message_handler.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Some(text) = e.data().as_string() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(ref h) = handler {
                        (h.borrow_mut())(json);
                    }
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onclose — reconnect after delay
        let callbacks_close = callbacks.clone();
        let connected_close = connected_flag.clone();
        let onclose = Closure::wrap(Box::new(move |_e: CloseEvent| {
            *connected_close.borrow_mut() = false;
            callbacks_close.invoke_server_state(false);
            // Schedule reconnect
            let url_rc = url_clone.clone();
            let _ = web_sys::window().map(|w| {
                // We can't call self.connect() from a closure, so we just
                // signal the disconnection. The main module handles reconnect.
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    &js_sys::Function::new_no_args(&format!(
                        "if (window.__presenceWeb) window.__presenceWeb.reconnect_server('{}')",
                        url_rc.replace('\'', "\\'")
                    )),
                    RECONNECT_DELAY_MS,
                );
            });
        }) as Box<dyn FnMut(CloseEvent)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        // onerror
        let callbacks_err = callbacks;
        let onerror = Closure::wrap(Box::new(move || {
            callbacks_err.invoke_error("Server WebSocket error");
        }) as Box<dyn FnMut()>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        self.ws = Some(ws);
        self._onopen = Some(onopen);
        self._onmessage = Some(onmessage);
        self._onclose = Some(onclose);
        self._onerror = Some(onerror);
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

    /// Send a JSON message to the server.
    pub fn send_json(&self, msg: &serde_json::Value) -> bool {
        if let Some(ref ws) = self.ws {
            ws.send_with_str(&msg.to_string()).is_ok()
        } else {
            false
        }
    }

    /// Send a keyboard event.
    pub fn send_key(&self, key: &str, ctrl: bool, alt: bool, shift: bool) {
        let msg = serde_json::json!({
            "t": "key",
            "key": key,
            "ctrl": ctrl,
            "alt": alt,
            "shift": shift,
        });
        self.send_json(&msg);
    }

    /// Send a resize event.
    pub fn send_resize(&self, cols: u16, rows: u16) {
        let msg = serde_json::json!({
            "t": "resize",
            "cols": cols,
            "rows": rows,
        });
        self.send_json(&msg);
    }

    /// Send live_connected notification.
    pub fn send_live_connected(&self) {
        self.send_json(&serde_json::json!({"t": "live_connected"}));
    }

    /// Send live_disconnected notification.
    pub fn send_live_disconnected(&self) {
        self.send_json(&serde_json::json!({"t": "live_disconnected"}));
    }

    /// Send a tool_request to the server.
    pub fn send_tool_request(&self, id: &str, tool: &str, args: &serde_json::Value) {
        let msg = serde_json::json!({
            "t": "tool_request",
            "id": id,
            "tool": tool,
            "args": args,
        });
        self.send_json(&msg);
    }

    /// Send a ControlMsg action to the server.
    pub fn send_action(&self, action: &serde_json::Value) {
        self.send_json(action);
    }
}
