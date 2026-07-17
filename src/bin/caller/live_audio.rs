use crate::audio_routing::AudioBridge;
use crate::error::CallerError;
use crate::live_audio_types::*;
use crate::quarantine;
use crate::schema_validator;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------------------------------------------------------------------------
// Live audio events
// ---------------------------------------------------------------------------

/// Events emitted by the live audio session's read loop.
#[derive(Debug)]
pub enum LiveAudioEvent {
    Connected,
    SetupComplete,
    /// Model produced audio to play to the app (raw PCM16 bytes).
    AudioOut(Vec<u8>),
    /// Model transcription of what it said.
    ModelTranscript(String),
    /// Model text output (non-audio, e.g. the structured response).
    ModelText(String),
    /// Model called a whitelisted function (submit_response or end_call).
    FunctionCall {
        name: String,
        call_id: String,
        args: serde_json::Value,
    },
    /// Model attempted an unknown tool call (will be quarantined).
    ToolCallAttempted {
        name: String,
        args: serde_json::Value,
    },
    TurnComplete,
    Interrupted,
    Disconnected(String),
    Error(String),
}

/// Names of the two whitelisted functions the live audio model may call.
const FN_SUBMIT_RESPONSE: &str = "submit_response";
const FN_END_CALL: &str = "end_call";

/// Build tool definitions for the live audio session from a ResponseSchema.
/// Returns JSON arrays suitable for OpenAI and Gemini tool formats.
fn build_live_audio_tools(schema: &ResponseSchema) -> (serde_json::Value, serde_json::Value) {
    // Build JSON Schema properties from ResponseSchema fields
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for field in &schema.fields {
        let prop = match &field.field_type {
            FieldType::String {
                max_length,
                allowed_values,
                ..
            } => {
                let mut p = serde_json::json!({"type": "string"});
                if let Some(max) = max_length {
                    p["maxLength"] = serde_json::json!(max);
                }
                if let Some(vals) = allowed_values {
                    p["enum"] = serde_json::json!(vals);
                }
                p
            }
            FieldType::Integer { min, max } => {
                let mut p = serde_json::json!({"type": "integer"});
                if let Some(min) = min {
                    p["minimum"] = serde_json::json!(min);
                }
                if let Some(max) = max {
                    p["maximum"] = serde_json::json!(max);
                }
                p
            }
            FieldType::Boolean => serde_json::json!({"type": "boolean"}),
            FieldType::Array {
                element_type,
                max_items,
            } => {
                let items = match element_type.as_ref() {
                    FieldType::String { .. } => serde_json::json!({"type": "string"}),
                    FieldType::Integer { .. } => serde_json::json!({"type": "integer"}),
                    FieldType::Boolean => serde_json::json!({"type": "boolean"}),
                    _ => serde_json::json!({"type": "string"}),
                };
                let mut p = serde_json::json!({"type": "array", "items": items});
                if let Some(max) = max_items {
                    p["maxItems"] = serde_json::json!(max);
                }
                p
            }
        };
        if let Some(desc) = &field.description {
            let mut prop = prop;
            prop["description"] = serde_json::json!(desc);
            properties.insert(field.name.clone(), prop);
        } else {
            properties.insert(field.name.clone(), prop);
        }
        if field.required {
            required.push(serde_json::Value::String(field.name.clone()));
        }
    }

    let submit_params = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });

    // OpenAI format
    let openai_tools = serde_json::json!([
        {
            "type": "function",
            "name": FN_SUBMIT_RESPONSE,
            "description": "Submit the structured response data collected from the conversation. Call this once you have all the information.",
            "parameters": submit_params,
        },
        {
            "type": "function",
            "name": FN_END_CALL,
            "description": "Signal that the conversation is complete and you are ready to hang up. Call this after submit_response, or if the call cannot be completed.",
            "parameters": {"type": "object", "properties": {}},
        }
    ]);

    // Gemini format
    let gemini_tools = serde_json::json!([{
        "function_declarations": [
            {
                "name": FN_SUBMIT_RESPONSE,
                "description": "Submit the structured response data collected from the conversation.",
                "parameters": submit_params,
            },
            {
                "name": FN_END_CALL,
                "description": "Signal that the conversation is complete.",
                "parameters": {"type": "object", "properties": {}},
            }
        ]
    }]);

    (openai_tools, gemini_tools)
}

// ---------------------------------------------------------------------------
// Live audio session
// ---------------------------------------------------------------------------

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;

/// A running live audio session connected to a model via WebSocket.
pub struct LiveAudioSession {
    ws_write: Arc<Mutex<WsSink>>,
    pub event_rx: mpsc::UnboundedReceiver<LiveAudioEvent>,
    pub provider: LiveAudioProvider,
    pub sample_rate: u32,
    read_handle: tokio::task::JoinHandle<()>,
}

impl LiveAudioSession {
    /// Send raw PCM16 audio to the model.
    #[allow(dead_code)]
    pub async fn send_audio(&self, pcm16: &[u8]) -> Result<(), CallerError> {
        let b64 = BASE64.encode(pcm16);
        let msg = match self.provider {
            LiveAudioProvider::Gemini => serde_json::json!({
                "realtime_input": {
                    "media_chunks": [{
                        "mime_type": format!("audio/pcm;rate={}", self.sample_rate),
                        "data": b64
                    }]
                }
            }),
            LiveAudioProvider::OpenAI => serde_json::json!({
                "type": "input_audio_buffer.append",
                "audio": b64
            }),
        };
        let mut sink = self.ws_write.lock().await;
        sink.send(WsMessage::Text(msg.to_string().into()))
            .await
            .map_err(|e| CallerError::Agent(format!("WebSocket send error: {}", e)))?;
        Ok(())
    }

    /// Send a text message to the model.
    pub async fn send_text(&self, text: &str) -> Result<(), CallerError> {
        let msg = match self.provider {
            LiveAudioProvider::Gemini => serde_json::json!({
                "client_content": {
                    "turns": [{"role": "user", "parts": [{"text": text}]}],
                    "turn_complete": true
                }
            }),
            LiveAudioProvider::OpenAI => serde_json::json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }
            }),
        };

        let mut sink = self.ws_write.lock().await;
        sink.send(WsMessage::Text(msg.to_string().into()))
            .await
            .map_err(|e| CallerError::Agent(format!("WebSocket send error: {}", e)))?;

        // OpenAI requires an explicit response.create after sending content
        if self.provider == LiveAudioProvider::OpenAI {
            sink.send(WsMessage::Text(
                r#"{"type":"response.create"}"#.to_string().into(),
            ))
            .await
            .map_err(|e| CallerError::Agent(format!("WebSocket send error: {}", e)))?;
        }

        Ok(())
    }

    /// Gracefully close the WebSocket connection.
    pub async fn close(self) {
        let mut sink = self.ws_write.lock().await;
        let _ = sink.send(WsMessage::Close(None)).await;
        drop(sink);
        self.read_handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Gemini Live connection
// ---------------------------------------------------------------------------

const GEMINI_API_BASE: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContent";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash-native-audio-preview-12-2025";

pub async fn connect_gemini(
    api_key: &str,
    model: Option<&str>,
    playbook: &str,
    voice: Option<&str>,
    sample_rate: u32,
    tools: &serde_json::Value,
) -> Result<LiveAudioSession, CallerError> {
    let model_name = model.unwrap_or(DEFAULT_GEMINI_MODEL);
    let url = format!("{}?key={}", GEMINI_API_BASE, api_key);
    let voice_name = voice.unwrap_or("Aoede");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| CallerError::Agent(format!("Gemini WebSocket connect failed: {}", e)))?;

    let (ws_write, ws_read) = ws_stream.split();
    let ws_write = Arc::new(Mutex::new(ws_write));
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // Send setup message
    let setup = serde_json::json!({
        "setup": {
            "model": format!("models/{}", model_name),
            "generation_config": {
                "response_modalities": ["AUDIO"],
                "speech_config": {
                    "voice_config": {
                        "prebuilt_voice_config": {
                            "voice_name": voice_name
                        }
                    }
                }
            },
            "output_audio_transcription": {},
            "system_instruction": {
                "parts": [{ "text": playbook }]
            },
            "tools": tools
        }
    });

    {
        let mut sink = ws_write.lock().await;
        sink.send(WsMessage::Text(setup.to_string().into()))
            .await
            .map_err(|e| CallerError::Agent(format!("Gemini setup send failed: {}", e)))?;
    }

    let _ = event_tx.send(LiveAudioEvent::Connected);

    // Spawn read loop
    let read_handle = tokio::spawn(gemini_read_loop(ws_read, event_tx));

    Ok(LiveAudioSession {
        ws_write,
        event_rx,
        provider: LiveAudioProvider::Gemini,
        sample_rate,
        read_handle,
    })
}

type WsReadStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn gemini_read_loop(
    mut ws_read: WsReadStream,
    event_tx: mpsc::UnboundedSender<LiveAudioEvent>,
) {
    while let Some(msg_result) = ws_read.next().await {
        let text = match msg_result {
            Ok(WsMessage::Text(t)) => t.to_string(),
            Ok(WsMessage::Binary(b)) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Ok(WsMessage::Close(_)) => {
                let _ = event_tx.send(LiveAudioEvent::Disconnected("close frame".into()));
                break;
            }
            Err(e) => {
                let _ = event_tx.send(LiveAudioEvent::Error(format!("WS read error: {}", e)));
                break;
            }
            _ => continue,
        };

        let msg: serde_json::Value = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // setupComplete
        if msg.get("setupComplete").is_some() {
            let _ = event_tx.send(LiveAudioEvent::SetupComplete);
            continue;
        }

        // toolCall — dispatch whitelisted, quarantine others
        if let Some(tool_call) = msg.get("toolCall") {
            if let Some(fcs) = tool_call.get("functionCalls").and_then(|v| v.as_array()) {
                for fc in fcs {
                    let name = fc["name"].as_str().unwrap_or("unknown").to_string();
                    let args = fc.get("args").cloned().unwrap_or_default();
                    let call_id = fc["id"].as_str().unwrap_or("").to_string();
                    if name == FN_SUBMIT_RESPONSE || name == FN_END_CALL {
                        let _ = event_tx.send(LiveAudioEvent::FunctionCall {
                            name,
                            call_id,
                            args,
                        });
                    } else {
                        let _ = event_tx.send(LiveAudioEvent::ToolCallAttempted { name, args });
                    }
                }
            }
            continue;
        }

        // toolCallCancellation — ignore
        if msg.get("toolCallCancellation").is_some() {
            continue;
        }

        // serverContent
        if let Some(response) = msg.get("serverContent") {
            // Output transcription
            if let Some(transcript) = response.get("outputTranscription") {
                if let Some(text) = transcript.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        let _ = event_tx.send(LiveAudioEvent::ModelTranscript(text.to_string()));
                    }
                }
                continue;
            }

            // turnComplete
            if response.get("turnComplete").is_some() {
                let _ = event_tx.send(LiveAudioEvent::TurnComplete);
                continue;
            }

            // interrupted
            if response.get("interrupted").is_some() {
                let _ = event_tx.send(LiveAudioEvent::Interrupted);
                continue;
            }

            // modelTurn parts
            if let Some(model_turn) = response.get("modelTurn") {
                if let Some(parts) = model_turn.get("parts").and_then(|v| v.as_array()) {
                    for part in parts {
                        // Audio data
                        if let Some(inline) = part.get("inlineData") {
                            if let Some(mime) = inline.get("mimeType").and_then(|v| v.as_str()) {
                                if mime.starts_with("audio/") {
                                    if let Some(data) = inline.get("data").and_then(|v| v.as_str())
                                    {
                                        if let Ok(pcm) = BASE64.decode(data) {
                                            let _ = event_tx.send(LiveAudioEvent::AudioOut(pcm));
                                        }
                                    }
                                }
                            }
                        }
                        // Text output
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            let _ = event_tx.send(LiveAudioEvent::ModelText(text.to_string()));
                        }
                        // Function call in model turn
                        if let Some(fc) = part.get("functionCall") {
                            let name = fc["name"].as_str().unwrap_or("unknown").to_string();
                            let args = fc.get("args").cloned().unwrap_or_default();
                            let call_id = fc["id"].as_str().unwrap_or("").to_string();
                            if name == FN_SUBMIT_RESPONSE || name == FN_END_CALL {
                                let _ = event_tx.send(LiveAudioEvent::FunctionCall {
                                    name,
                                    call_id,
                                    args,
                                });
                            } else {
                                let _ =
                                    event_tx.send(LiveAudioEvent::ToolCallAttempted { name, args });
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI Realtime connection
// ---------------------------------------------------------------------------

const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-realtime-preview";

pub async fn connect_openai(
    api_key: &str,
    model: Option<&str>,
    playbook: &str,
    voice: Option<&str>,
    sample_rate: u32,
    tools: &serde_json::Value,
) -> Result<LiveAudioSession, CallerError> {
    let model_name = model.unwrap_or(DEFAULT_OPENAI_MODEL);
    let url = format!("wss://api.openai.com/v1/realtime?model={}", model_name);
    let voice_name = voice.unwrap_or("alloy");

    // Build WebSocket request with auth headers via sub-protocols
    use tokio_tungstenite::tungstenite::http;
    let request = http::Request::builder()
        .uri(&url)
        .header(
            "Sec-WebSocket-Protocol",
            format!(
                "realtime, openai-insecure-api-key.{}, openai-beta.realtime-v1",
                api_key
            ),
        )
        .header("Host", "api.openai.com")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .map_err(|e| CallerError::Agent(format!("failed to build request: {}", e)))?;

    let (ws_stream, _): (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        _,
    ) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| CallerError::Agent(format!("OpenAI WebSocket connect failed: {}", e)))?;

    let (ws_write, ws_read) = ws_stream.split();
    let ws_write: Arc<Mutex<WsSink>> = Arc::new(Mutex::new(ws_write));
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // Send session.update with whitelisted tools
    let setup = serde_json::json!({
        "type": "session.update",
        "session": {
            "modalities": ["audio", "text"],
            "instructions": playbook,
            "voice": voice_name,
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "tools": tools
        }
    });

    {
        let mut sink = ws_write.lock().await;
        sink.send(WsMessage::Text(setup.to_string().into()))
            .await
            .map_err(|e| CallerError::Agent(format!("OpenAI setup send failed: {}", e)))?;
    }

    let _ = event_tx.send(LiveAudioEvent::Connected);

    // Spawn read loop
    let read_handle = tokio::spawn(openai_read_loop(ws_read, event_tx));

    Ok(LiveAudioSession {
        ws_write,
        event_rx,
        provider: LiveAudioProvider::OpenAI,
        sample_rate,
        read_handle,
    })
}

async fn openai_read_loop(
    mut ws_read: WsReadStream,
    event_tx: mpsc::UnboundedSender<LiveAudioEvent>,
) {
    while let Some(msg_result) = ws_read.next().await {
        let text = match msg_result {
            Ok(WsMessage::Text(t)) => t.to_string(),
            Ok(WsMessage::Close(_)) => {
                let _ = event_tx.send(LiveAudioEvent::Disconnected("close frame".into()));
                break;
            }
            Err(e) => {
                let _ = event_tx.send(LiveAudioEvent::Error(format!("WS read error: {}", e)));
                break;
            }
            _ => continue,
        };

        let msg: serde_json::Value = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let msg_type = msg["type"].as_str().unwrap_or("");
        match msg_type {
            "session.created" | "session.updated" => {
                let _ = event_tx.send(LiveAudioEvent::SetupComplete);
            }
            "response.audio.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    if let Ok(pcm) = BASE64.decode(delta) {
                        let _ = event_tx.send(LiveAudioEvent::AudioOut(pcm));
                    }
                }
            }
            "response.text.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    let _ = event_tx.send(LiveAudioEvent::ModelText(delta.to_string()));
                }
            }
            "response.audio_transcript.delta" => {
                if let Some(delta) = msg["delta"].as_str() {
                    let _ = event_tx.send(LiveAudioEvent::ModelTranscript(delta.to_string()));
                }
            }
            "response.function_call_arguments.done" => {
                let name = msg["name"].as_str().unwrap_or("unknown").to_string();
                let call_id = msg["call_id"].as_str().unwrap_or("").to_string();
                let args = serde_json::from_str::<serde_json::Value>(
                    msg["arguments"].as_str().unwrap_or("{}"),
                )
                .unwrap_or_default();
                if name == FN_SUBMIT_RESPONSE || name == FN_END_CALL {
                    let _ = event_tx.send(LiveAudioEvent::FunctionCall {
                        name,
                        call_id,
                        args,
                    });
                } else {
                    let _ = event_tx.send(LiveAudioEvent::ToolCallAttempted { name, args });
                }
            }
            "input_audio_buffer.speech_started" => {
                let _ = event_tx.send(LiveAudioEvent::Interrupted);
            }
            "response.done" => {
                let _ = event_tx.send(LiveAudioEvent::TurnComplete);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Audio bridge (PulseAudio <-> live model)
// ---------------------------------------------------------------------------

/// Bidirectional audio bridge between PulseAudio virtual devices and a live model session.
pub struct AudioStreamBridge {
    capture_handle: tokio::task::JoinHandle<()>,
    playback_handle: tokio::task::JoinHandle<()>,
}

impl AudioStreamBridge {
    /// Stop the audio bridge tasks.
    pub fn stop(self) {
        self.capture_handle.abort();
        self.playback_handle.abort();
    }
}

// Start a bidirectional audio bridge.
//
// - **Capture**: reads from PulseAudio monitor (app audio) and sends to the live model
// - **Playback**: receives model audio output and writes to PulseAudio sink (app mic input)
// ---------------------------------------------------------------------------
// Vortex wire protocol (must match VortexAudioDaemon)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
const VORTEX_MSG_CONFIGURE: u32 = 0x01;
#[allow(dead_code)]
const VORTEX_MSG_PCM_OUTPUT: u32 = 0x02;
#[allow(dead_code)]
const VORTEX_MSG_PCM_INPUT: u32 = 0x03;
#[allow(dead_code)]
const VORTEX_MSG_START: u32 = 0x04;
#[allow(dead_code)]
const VORTEX_MSG_STOP: u32 = 0x05;

/// Read one Vortex wire protocol message: [u32 LE type][u32 LE len][payload].
#[allow(dead_code)]
async fn vortex_read_msg(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<(u32, Vec<u8>), CallerError> {
    let mut hdr = [0u8; 8];
    reader
        .read_exact(&mut hdr)
        .await
        .map_err(|e| CallerError::Agent(format!("vortex: read header: {}", e)))?;
    let msg_type = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let payload_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| CallerError::Agent(format!("vortex: read payload: {}", e)))?;
    }
    Ok((msg_type, payload))
}

/// Write one Vortex wire protocol message.
#[allow(dead_code)]
async fn vortex_write_msg(
    writer: &mut (impl AsyncWriteExt + Unpin),
    msg_type: u32,
    payload: &[u8],
) -> Result<(), CallerError> {
    // Write header + payload as a single buffer to avoid partial messages.
    // The daemon's readExactly busy-waits on EAGAIN, so split writes can
    // cause it to spin between header and payload arrival.
    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.extend_from_slice(&msg_type.to_le_bytes());
    msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    msg.extend_from_slice(payload);
    writer
        .write_all(&msg)
        .await
        .map_err(|e| CallerError::Agent(format!("vortex: write msg: {}", e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Format conversion: Vortex (Float32 stereo 48kHz) ↔ Model (PCM16 mono 24kHz)
// ---------------------------------------------------------------------------

/// Core of [`vortex_capture_convert`]: downmix + decimate f32 stereo 48kHz
/// samples directly to PCM16 mono 24kHz bytes, appended to `out`. Operating
/// on f32 samples (not a little-endian byte image of them) lets the shm
/// capture path convert straight from ring reads with no intermediate byte
/// buffer, into a reusable output buffer.
fn downmix_decimate_f32_to_pcm16(f32_stereo_48k: &[f32], out: &mut Vec<u8>) {
    // Each stereo frame = 2 floats. After mono downmix + 2:1 decimation +
    // i16 conversion: 2 frames → one 2-byte sample.
    let num_stereo_frames = f32_stereo_48k.len() / 2;
    out.reserve(num_stereo_frames.div_ceil(2) * 2);

    for i in (0..num_stereo_frames).step_by(2) {
        let left = f32_stereo_48k[i * 2];
        let right = f32_stereo_48k[i * 2 + 1];
        let mono = (left + right) * 0.5;
        let clamped = mono.clamp(-1.0, 1.0);
        let sample = (clamped * 32767.0) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
    }
}

/// Convert Vortex daemon PCM_OUTPUT (Float32 stereo 48kHz, little-endian
/// bytes) to model input (PCM16 mono 24kHz). 8:1 size reduction. Byte-image
/// front-end over [`downmix_decimate_f32_to_pcm16`] for the daemon-socket
/// path, which receives the ring contents as wire bytes.
fn vortex_capture_convert(f32_stereo_48k: &[u8]) -> Vec<u8> {
    let mut floats = Vec::with_capacity(f32_stereo_48k.len() / 4);
    for chunk in f32_stereo_48k.chunks_exact(4) {
        floats.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let mut out = Vec::new();
    downmix_decimate_f32_to_pcm16(&floats, &mut out);
    out
}

/// Core of [`vortex_playback_convert`]: expand model output (PCM16 mono
/// 24kHz) to Vortex f32 stereo 48kHz samples appended to `out` (duplicate
/// 2:1 upsample, L=R). The shm playback path writes these f32 samples
/// straight into the input ring without round-tripping a byte image.
fn pcm16_to_f32_stereo_upsampled(pcm16_mono_24k: &[u8], out: &mut Vec<f32>) {
    let num_samples = pcm16_mono_24k.len() / 2;
    // Each mono sample → 2 stereo frames (upsample) × 2 channels
    out.reserve(num_samples * 4);

    for i in 0..num_samples {
        let sample = i16::from_le_bytes([pcm16_mono_24k[i * 2], pcm16_mono_24k[i * 2 + 1]]);
        let f = sample as f32 / 32768.0;
        // Duplicate sample for 2:1 upsample, stereo (L=R)
        for _ in 0..2 {
            out.push(f); // left
            out.push(f); // right
        }
    }
}

/// Convert model output (PCM16 mono 24kHz) to Vortex daemon PCM_INPUT
/// (Float32 stereo 48kHz, little-endian bytes). 8:1 size expansion.
/// Byte-image front-end over [`pcm16_to_f32_stereo_upsampled`] for the
/// daemon-socket wire path.
fn vortex_playback_convert(pcm16_mono_24k: &[u8]) -> Vec<u8> {
    let mut samples = Vec::new();
    pcm16_to_f32_stereo_upsampled(pcm16_mono_24k, &mut samples);
    let mut out = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Capture frame aggregation (vortex shm path)
// ---------------------------------------------------------------------------

/// f32 ring samples per millisecond in the Vortex capture ring (48kHz × 2
/// channels — fixed by the HAL plugin's shared-memory format).
const VORTEX_RING_F32_PER_MS: usize = 96;

/// One 2:1 decimation group: two stereo frames (4 f32 samples) collapse into
/// one output sample, so frames are cut on this boundary to keep the
/// decimation phase continuous across WS messages.
const CAPTURE_DECIMATION_GROUP_F32: usize = 4;

/// Minimum audio per capture WS message (~20ms): the aggregation floor that
/// turns the 5ms ring poll into 4-8× fewer, larger messages while adding at
/// most one frame of latency — well inside a 40ms budget.
const CAPTURE_FRAME_MIN_MS: usize = 20;

/// Maximum audio per capture WS message (~40ms): caps message size when a
/// scheduling hiccup piles up ring backlog, so recovery sends a few normal
/// frames instead of one giant one.
const CAPTURE_FRAME_MAX_MS: usize = 40;

const CAPTURE_FRAME_MIN_F32: usize = CAPTURE_FRAME_MIN_MS * VORTEX_RING_F32_PER_MS;
const CAPTURE_FRAME_MAX_F32: usize = CAPTURE_FRAME_MAX_MS * VORTEX_RING_F32_PER_MS;

// Frame cuts must land on decimation-group boundaries.
const _: () = assert!(
    CAPTURE_FRAME_MIN_F32 % CAPTURE_DECIMATION_GROUP_F32 == 0
        && CAPTURE_FRAME_MAX_F32 % CAPTURE_DECIMATION_GROUP_F32 == 0
        && CAPTURE_FRAME_MIN_F32 <= CAPTURE_FRAME_MAX_F32
);

/// Accumulates raw f32 ring samples across 5ms poll ticks and cuts them into
/// 20-40ms frames for conversion + WS send. Both buffers live for the whole
/// capture task and are reused (clear-don't-realloc), so steady-state capture
/// allocates nothing per tick.
struct CaptureFrameAggregator {
    /// Samples read from the ring but not yet emitted in a frame.
    pending: Vec<f32>,
    /// Scratch for the frame currently being emitted.
    frame: Vec<f32>,
}

impl CaptureFrameAggregator {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(CAPTURE_FRAME_MAX_F32),
            frame: Vec::with_capacity(CAPTURE_FRAME_MAX_F32),
        }
    }

    /// Append one ring sample (called straight from the masked ring read).
    fn push_sample(&mut self, sample: f32) {
        self.pending.push(sample);
    }

    /// Cut the next full frame if at least [`CAPTURE_FRAME_MIN_F32`] samples
    /// are pending: up to [`CAPTURE_FRAME_MAX_F32`] samples, always aligned
    /// to a decimation-group boundary; the remainder stays pending.
    fn next_full_frame(&mut self) -> Option<&[f32]> {
        if self.pending.len() < CAPTURE_FRAME_MIN_F32 {
            return None;
        }
        let cut = self.pending.len().min(CAPTURE_FRAME_MAX_F32);
        let cut = cut - (cut % CAPTURE_DECIMATION_GROUP_F32);
        self.frame.clear();
        self.frame.extend(self.pending.drain(..cut));
        Some(&self.frame)
    }

    /// Emit whatever is pending regardless of size — used when the ring goes
    /// quiet so utterance tails are not held back waiting for a full frame.
    fn flush(&mut self) -> Option<&[f32]> {
        if self.pending.is_empty() {
            return None;
        }
        self.frame.clear();
        self.frame.extend(self.pending.drain(..));
        Some(&self.frame)
    }
}

/// Convert one aggregated capture frame and forward it to the model
/// WebSocket (and the optional transcription tee). Reuses the caller's
/// conversion/encoding buffers; the WS message layout is byte-identical to
/// the historical per-tick sends apart from carrying a larger PCM chunk.
#[cfg(unix)]
#[allow(clippy::result_unit_err)]
async fn send_capture_frame(
    frame: &[f32],
    pcm16_buf: &mut Vec<u8>,
    b64_buf: &mut String,
    provider: LiveAudioProvider,
    sample_rate: u32,
    tee: Option<&AudioQueueSender>,
    sink: &Mutex<WsSink>,
) -> Result<(), ()> {
    pcm16_buf.clear();
    downmix_decimate_f32_to_pcm16(frame, pcm16_buf);
    if pcm16_buf.is_empty() {
        return Ok(());
    }
    b64_buf.clear();
    BASE64.encode_string(&*pcm16_buf, b64_buf);
    let ws_msg = match provider {
        LiveAudioProvider::Gemini => serde_json::json!({
            "realtime_input": {
                "media_chunks": [{
                    "mime_type": format!("audio/pcm;rate={}", sample_rate),
                    "data": &*b64_buf
                }]
            }
        }),
        LiveAudioProvider::OpenAI => serde_json::json!({
            "type": "input_audio_buffer.append",
            "audio": &*b64_buf
        }),
    };
    if let Some(tee) = tee {
        let _ = tee.send(pcm16_buf.clone());
    }
    let mut sink = sink.lock().await;
    sink.send(WsMessage::Text(ws_msg.to_string().into()))
        .await
        .map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Duration-bounded drop-oldest audio queues
// ---------------------------------------------------------------------------
//
// Live audio lanes must never buffer unboundedly: a stalled consumer (slow
// WS peer, wedged playback sink, slow Whisper spell) would otherwise grow
// the queue for the lifetime of the call. Every lane instead carries a
// duration budget; overflow evicts the OLDEST audio first — for a live
// stream a skip is strictly better than unbounded lag, because dropping the
// oldest keeps the survivors near real time.

/// Playback lane budget: model audio waiting to enter the output device.
/// TTS bursts arrive faster than real time, so a healthy lane briefly holds
/// several seconds; 30s absorbs a full long utterance while capping a wedged
/// playback sink at ~1.4 MiB instead of the whole call.
const PLAYBACK_QUEUE_SECONDS: usize = 30;

/// Capture-side lane budget (the daemon-socket capture stage and the Whisper
/// tee): capture is produced in real time, so any backlog beyond a few
/// seconds means the consumer stalled; 30s (matching the playback lane) is
/// far more than a healthy lane ever holds while still bounding a stalled
/// one.
const CAPTURE_QUEUE_SECONDS: usize = 30;

/// Rate limit for the per-lane overflow log: one line per interval
/// summarising everything dropped since the last, so a sustained stall logs
/// a heartbeat instead of a flood.
const QUEUE_DROP_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Rate-limited accounting for a lane's drop-oldest evictions.
struct DropLog {
    lane: &'static str,
    dropped_chunks: u64,
    dropped_secs: f64,
    last_log: Option<Instant>,
}

impl DropLog {
    fn new(lane: &'static str) -> Self {
        Self {
            lane,
            dropped_chunks: 0,
            dropped_secs: 0.0,
            last_log: None,
        }
    }

    /// Record `chunks` evicted chunks worth `secs` of audio, emitting at most
    /// one summary line per [`QUEUE_DROP_LOG_INTERVAL`].
    fn note(&mut self, chunks: u64, secs: f64) {
        self.dropped_chunks += chunks;
        self.dropped_secs += secs;
        let now = Instant::now();
        let due = self
            .last_log
            .is_none_or(|at| now.duration_since(at) >= QUEUE_DROP_LOG_INTERVAL);
        if due {
            eprintln!(
                "live_audio: {} lane over budget — dropped {} oldest chunk(s) (~{:.1}s of audio)",
                self.lane, self.dropped_chunks, self.dropped_secs
            );
            self.dropped_chunks = 0;
            self.dropped_secs = 0.0;
            self.last_log = Some(now);
        }
    }
}

struct AudioQueueState {
    chunks: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    sender_dropped: bool,
    receiver_dropped: bool,
    drop_log: DropLog,
}

struct AudioQueueShared {
    state: std::sync::Mutex<AudioQueueState>,
    notify: tokio::sync::Notify,
    max_bytes: usize,
    bytes_per_sec: usize,
}

/// Producer half of a [`bounded_audio_queue`]. Not `Clone`: every lane has a
/// single producer, and dropping it closes the lane for the receiver.
pub(crate) struct AudioQueueSender {
    shared: Arc<AudioQueueShared>,
}

/// Consumer half of a [`bounded_audio_queue`].
pub(crate) struct AudioQueueReceiver {
    shared: Arc<AudioQueueShared>,
}

/// Build a duration-bounded drop-oldest audio lane: `seconds` ×
/// `bytes_per_sec` is the byte budget, and queueing beyond it evicts the
/// oldest chunks (never the chunk being queued) with a rate-limited log
/// naming the lane.
fn bounded_audio_queue(
    lane: &'static str,
    seconds: usize,
    bytes_per_sec: usize,
) -> (AudioQueueSender, AudioQueueReceiver) {
    let shared = Arc::new(AudioQueueShared {
        state: std::sync::Mutex::new(AudioQueueState {
            chunks: VecDeque::new(),
            queued_bytes: 0,
            sender_dropped: false,
            receiver_dropped: false,
            drop_log: DropLog::new(lane),
        }),
        notify: tokio::sync::Notify::new(),
        max_bytes: seconds * bytes_per_sec,
        bytes_per_sec,
    });
    (
        AudioQueueSender {
            shared: shared.clone(),
        },
        AudioQueueReceiver { shared },
    )
}

impl AudioQueueSender {
    /// Queue one chunk. An over-budget lane evicts its oldest chunks first;
    /// returns `Err(())` once the receiver is gone so producers can wind
    /// down (mirrors `mpsc::UnboundedSender::send`'s error contract).
    #[allow(clippy::result_unit_err)]
    fn send(&self, chunk: Vec<u8>) -> Result<(), ()> {
        let mut st = self.shared.state.lock().unwrap();
        if st.receiver_dropped {
            return Err(());
        }
        st.queued_bytes += chunk.len();
        st.chunks.push_back(chunk);
        let mut dropped_chunks = 0u64;
        let mut dropped_bytes = 0usize;
        while st.queued_bytes > self.shared.max_bytes && st.chunks.len() > 1 {
            let oldest = st.chunks.pop_front().expect("len > 1");
            st.queued_bytes -= oldest.len();
            dropped_chunks += 1;
            dropped_bytes += oldest.len();
        }
        if dropped_chunks > 0 {
            let secs = dropped_bytes as f64 / self.shared.bytes_per_sec.max(1) as f64;
            st.drop_log.note(dropped_chunks, secs);
        }
        drop(st);
        self.shared.notify.notify_one();
        Ok(())
    }
}

impl Drop for AudioQueueSender {
    fn drop(&mut self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.sender_dropped = true;
        }
        self.shared.notify.notify_one();
    }
}

impl AudioQueueReceiver {
    /// Await the next chunk; `None` once the sender is gone and the queue is
    /// drained (mirrors `mpsc::UnboundedReceiver::recv`). Cancel-safe: state
    /// lives in the shared queue, and every call re-checks it before
    /// sleeping, so a chunk enqueued between checks is never missed.
    async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            let notified = self.shared.notify.notified();
            {
                let mut st = self.shared.state.lock().unwrap();
                if let Some(chunk) = st.chunks.pop_front() {
                    st.queued_bytes -= chunk.len();
                    return Some(chunk);
                }
                if st.sender_dropped {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl Drop for AudioQueueReceiver {
    fn drop(&mut self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.receiver_dropped = true;
            st.chunks.clear();
            st.queued_bytes = 0;
        }
    }
}

pub async fn start_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    bridge: &AudioBridge,
    audio_out_rx: AudioQueueReceiver,
    capture_tee_tx: Option<AudioQueueSender>,
) -> Result<AudioStreamBridge, CallerError> {
    if bridge.uses_vortex_shm() {
        // The Vortex shared-memory bridge is POSIX-only (shm_open/mmap).
        // On Windows `create_vortex_bridge` is never selected, so this
        // branch is unreachable in practice; gate it so the Unix-only
        // helper isn't referenced on Windows.
        #[cfg(unix)]
        {
            return start_vortex_shm_bridge(
                session_write,
                provider,
                sample_rate,
                audio_out_rx,
                capture_tee_tx,
            )
            .await;
        }
        #[cfg(not(unix))]
        {
            return Err(CallerError::Agent(
                "Vortex audio bridge is not supported on this platform".into(),
            ));
        }
    }

    if let Some(host) = bridge.network_host() {
        return start_network_audio_bridge(
            session_write,
            provider,
            sample_rate,
            host,
            audio_out_rx,
            capture_tee_tx,
        )
        .await;
    }

    start_local_audio_bridge(
        session_write,
        provider,
        sample_rate,
        bridge,
        audio_out_rx,
        capture_tee_tx,
    )
    .await
}

// Network audio bridge: connects to a bh-bridge on the host over TCP.
// The TCP stream is full-duplex raw PCM16 mono -- host to client is captured
// app audio, client to host is model audio for playback.
// ---------------------------------------------------------------------------
// Vortex direct shared memory bridge (no daemon needed)
// ---------------------------------------------------------------------------

// Layout constants matching VortexSharedAudio.h
const VORTEX_SHM_NAME: &[u8] = b"/vortex-audio\0";
const VORTEX_SHM_MAGIC: u32 = 0x56585348;
const VORTEX_RING_FRAMES: usize = 65536;
const VORTEX_MAX_CHANNELS: usize = 2;
const VORTEX_RING_SAMPLES: usize = VORTEX_RING_FRAMES * VORTEX_MAX_CHANNELS;
const VORTEX_RING_MASK: u64 = (VORTEX_RING_SAMPLES - 1) as u64;

// Field offsets into VortexSharedAudioState (bytes)
const OFF_MAGIC: usize = 0;
#[allow(dead_code)]
const OFF_IS_ACTIVE: usize = 16;
const OFF_OUT_WRITE_POS: usize = 24;
const OFF_OUT_READ_POS: usize = 32;
const OFF_IN_WRITE_POS: usize = 40;
const OFF_IN_READ_POS: usize = 48;
const OFF_OUT_BUFFER: usize = 56;
const OFF_IN_BUFFER: usize = OFF_OUT_BUFFER + VORTEX_RING_SAMPLES * 4;

/// Direct shared memory bridge: reads/writes the Vortex HAL plugin's ring
/// buffers without the daemon. No sockets, no IPC, no deadlocks.
///
/// POSIX-only: uses `shm_open` + `mmap`. The Vortex HAL plugin is a
/// macOS/Linux guest-tools component, so this bridge is gated off Windows;
/// the dispatcher in [`start_audio_bridge`] never routes here on Windows.
#[cfg(unix)]
async fn start_vortex_shm_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    audio_out_rx: AudioQueueReceiver,
    capture_tee_tx: Option<AudioQueueSender>,
) -> Result<AudioStreamBridge, CallerError> {
    // Open and mmap the shared memory
    // SAFETY: VORTEX_SHM_NAME is a static NUL-terminated POSIX shm name.
    // O_RDWR opens the Vortex HAL-owned object for shared ring access.
    let fd = unsafe {
        libc::shm_open(
            VORTEX_SHM_NAME.as_ptr() as *const libc::c_char,
            libc::O_RDWR,
            0,
        )
    };
    if fd < 0 {
        return Err(CallerError::Agent(format!(
            "vortex shm_open failed (errno {}). Are Vortex guest tools installed?",
            std::io::Error::last_os_error()
        )));
    }

    let shm_size = OFF_IN_BUFFER + VORTEX_RING_SAMPLES * 4;
    // SAFETY: fd is a live descriptor from shm_open; shm_size covers the
    // VortexSharedAudioState header and both f32 ring buffers.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            shm_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    // SAFETY: fd was returned by shm_open above and is not used after close.
    unsafe { libc::close(fd) };
    if ptr == libc::MAP_FAILED {
        return Err(CallerError::Agent("vortex mmap failed".into()));
    }
    let base = ptr as *mut u8;

    // Verify magic
    // SAFETY: base points to a mapped VortexSharedAudioState; OFF_MAGIC is the
    // first u32 field and remains valid for the mapping's process lifetime.
    let magic = unsafe {
        (base.add(OFF_MAGIC) as *const std::sync::atomic::AtomicU32)
            .as_ref()
            .unwrap()
    }
    .load(std::sync::atomic::Ordering::Acquire);
    if magic != VORTEX_SHM_MAGIC {
        return Err(CallerError::Agent(format!(
            "vortex shm magic mismatch: expected 0x{:08X}, got 0x{:08X}",
            VORTEX_SHM_MAGIC, magic
        )));
    }

    eprintln!("live_audio: vortex shm bridge attached");

    // Raw pointers aren't Send. We use usize to pass the address to spawned
    // tasks. This is safe because the mmap region lives for the process lifetime
    // and both tasks only access disjoint rings (output vs input).
    let base_usize = base as usize;

    // Helper: read/write atomics via raw address (Send-safe)
    fn atomic_load_u64(base: usize, offset: usize, order: std::sync::atomic::Ordering) -> u64 {
        // SAFETY: callers pass offsets for aligned AtomicU64 fields inside the
        // mapped VortexSharedAudioState, whose lifetime outlives the bridge tasks.
        unsafe { &*((base + offset) as *const std::sync::atomic::AtomicU64) }.load(order)
    }
    fn atomic_store_u64(base: usize, offset: usize, val: u64, order: std::sync::atomic::Ordering) {
        // SAFETY: same invariant as atomic_load_u64; the selected field is an
        // aligned AtomicU64 owned by the shared Vortex state.
        unsafe { &*((base + offset) as *const std::sync::atomic::AtomicU64) }.store(val, order);
    }
    fn read_f32(base: usize, buf_offset: usize, idx: usize) -> f32 {
        // SAFETY: idx is masked by VORTEX_RING_MASK before each call, so the
        // access stays within the selected f32 ring buffer.
        unsafe { *((base + buf_offset) as *const f32).add(idx) }
    }
    fn write_f32(base: usize, buf_offset: usize, idx: usize, val: f32) {
        // SAFETY: idx is masked by VORTEX_RING_MASK before each call, so the
        // write stays within the selected f32 ring buffer.
        unsafe { *((base + buf_offset) as *mut f32).add(idx) = val };
    }

    // Capture task: poll output ring → aggregate → convert → model WebSocket.
    // The ring is still polled every 5ms (reads stay per-sample with masked
    // indices), but samples accumulate in the aggregator and only go out as
    // 20-40ms frames — 4-8× fewer WS messages than the old per-tick sends —
    // through buffers reused across the task's lifetime.
    let capture_write = session_write;
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_base = base_usize;
    let capture_handle = tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let b = capture_base;
        let mut ticker = tokio::time::interval(Duration::from_millis(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut agg = CaptureFrameAggregator::new();
        let mut pcm16_buf: Vec<u8> = Vec::new();
        let mut b64_buf = String::new();

        'capture: loop {
            ticker.tick().await;

            let w = atomic_load_u64(b, OFF_OUT_WRITE_POS, Ordering::Acquire);
            let r = atomic_load_u64(b, OFF_OUT_READ_POS, Ordering::Relaxed);
            let avail = w.wrapping_sub(r) as usize;
            if avail == 0 {
                // Stream went quiet with a partial frame pending: flush it so
                // utterance tails are not held back waiting for a full frame.
                if let Some(frame) = agg.flush() {
                    if send_capture_frame(
                        frame,
                        &mut pcm16_buf,
                        &mut b64_buf,
                        capture_provider,
                        capture_rate,
                        capture_tee_tx.as_ref(),
                        &capture_write,
                    )
                    .await
                    .is_err()
                    {
                        break 'capture;
                    }
                }
                continue;
            }

            let to_read = avail.min(VORTEX_RING_SAMPLES);
            for i in 0..to_read {
                let idx = ((r + i as u64) & VORTEX_RING_MASK) as usize;
                agg.push_sample(read_f32(b, OFF_OUT_BUFFER, idx));
            }
            atomic_store_u64(b, OFF_OUT_READ_POS, r + to_read as u64, Ordering::Release);

            while let Some(frame) = agg.next_full_frame() {
                if send_capture_frame(
                    frame,
                    &mut pcm16_buf,
                    &mut b64_buf,
                    capture_provider,
                    capture_rate,
                    capture_tee_tx.as_ref(),
                    &capture_write,
                )
                .await
                .is_err()
                {
                    break 'capture;
                }
            }
        }
    });

    // Playback task: model audio → convert → write to input ring
    let playback_base = base_usize;
    let playback_handle = tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let b = playback_base;
        let mut rx = audio_out_rx;
        // Reused across chunks (clear-don't-realloc); converted directly from
        // PCM16 to f32 samples with no intermediate byte image.
        let mut samples: Vec<f32> = Vec::new();

        while let Some(pcm_data) = rx.recv().await {
            samples.clear();
            pcm16_to_f32_stereo_upsampled(&pcm_data, &mut samples);

            let mut written = 0;
            while written < samples.len() {
                let w = atomic_load_u64(b, OFF_IN_WRITE_POS, Ordering::Relaxed);
                let r = atomic_load_u64(b, OFF_IN_READ_POS, Ordering::Acquire);
                let space = VORTEX_RING_SAMPLES - (w.wrapping_sub(r)) as usize;
                if space == 0 {
                    tokio::task::yield_now().await;
                    continue;
                }
                let to_write = (samples.len() - written).min(space);
                for i in 0..to_write {
                    let idx = ((w + i as u64) & VORTEX_RING_MASK) as usize;
                    write_f32(b, OFF_IN_BUFFER, idx, samples[written + i]);
                }
                atomic_store_u64(b, OFF_IN_WRITE_POS, w + to_write as u64, Ordering::Release);
                written += to_write;
            }
        }
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

/// Legacy Vortex daemon-socket bridge. The active dispatcher uses
/// [`start_vortex_shm_bridge`] instead; this parked path listens on a Unix
/// socket for the Vortex guest daemon, speaks the Vortex wire protocol, and
/// converts between Float32 stereo 48kHz (daemon) and PCM16 mono 24kHz (model).
///
/// Unix-only: binds a `UnixListener`. Gated off Windows for Tier-0.
#[cfg(unix)]
#[allow(dead_code)]
async fn start_vortex_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    socket_path: &str,
    audio_out_rx: AudioQueueReceiver,
    capture_tee_tx: Option<AudioQueueSender>,
) -> Result<AudioStreamBridge, CallerError> {
    // Clean up stale socket and bind
    let _ = std::fs::remove_file(socket_path);
    let listener = tokio::net::UnixListener::bind(socket_path)
        .map_err(|e| CallerError::Agent(format!("vortex: bind {}: {}", socket_path, e)))?;
    eprintln!("live_audio: vortex bridge listening on {}", socket_path);

    // Wait for daemon to connect (it retries every 2s)
    let stream = tokio::time::timeout(Duration::from_secs(30), listener.accept())
        .await
        .map_err(|_| CallerError::Agent("vortex: daemon did not connect within 30s".into()))?
        .map_err(|e| CallerError::Agent(format!("vortex: accept: {}", e)))?
        .0;
    eprintln!("live_audio: vortex daemon connected");

    let (mut read_half, write_half) = tokio::io::split(stream);

    // Handshake: read CONFIGURE
    let (msg_type, payload) = vortex_read_msg(&mut read_half).await?;
    if msg_type != VORTEX_MSG_CONFIGURE {
        return Err(CallerError::Agent(format!(
            "vortex: expected CONFIGURE (0x01), got 0x{:02x}",
            msg_type
        )));
    }
    if payload.len() >= 8 {
        let daemon_rate = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let daemon_channels = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
        eprintln!(
            "live_audio: vortex daemon format: {}Hz {}ch float32",
            daemon_rate, daemon_channels
        );
    }

    // Read START
    let (msg_type, _) = vortex_read_msg(&mut read_half).await?;
    if msg_type != VORTEX_MSG_START {
        return Err(CallerError::Agent(format!(
            "vortex: expected START (0x04), got 0x{:02x}",
            msg_type
        )));
    }
    eprintln!("live_audio: vortex streaming started");

    // Wrap write_half in Arc<Mutex> for shared access from playback task
    let write_half = Arc::new(Mutex::new(write_half));

    // Capture: two tasks to decouple socket reads from WebSocket writes.
    // Task A drains the daemon socket as fast as possible (prevents buffer
    // backup that deadlocks the daemon's send/recv). Task B forwards the
    // converted PCM to the model WebSocket at its own pace — a slow WS peer
    // costs the oldest queued audio, never unbounded memory.
    let (cap_tx, mut cap_rx) = bounded_audio_queue(
        "vortex-capture",
        CAPTURE_QUEUE_SECONDS,
        sample_rate as usize * 2,
    );

    // Task A: drain daemon socket → channel
    let capture_drain = tokio::spawn(async move {
        let mut reader = read_half;
        loop {
            match vortex_read_msg(&mut reader).await {
                Ok((VORTEX_MSG_PCM_OUTPUT, payload)) => {
                    let pcm16 = vortex_capture_convert(&payload);
                    if !pcm16.is_empty() && cap_tx.send(pcm16).is_err() {
                        break;
                    }
                }
                Ok((VORTEX_MSG_STOP, _)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        eprintln!("live_audio: vortex capture drain ended");
    });

    // Task B: channel → model WebSocket
    let capture_write = session_write;
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_handle = tokio::spawn(async move {
        while let Some(pcm16) = cap_rx.recv().await {
            let b64 = BASE64.encode(&pcm16);
            let ws_msg = match capture_provider {
                LiveAudioProvider::Gemini => serde_json::json!({
                    "realtime_input": {
                        "media_chunks": [{
                            "mime_type": format!("audio/pcm;rate={}", capture_rate),
                            "data": b64
                        }]
                    }
                }),
                LiveAudioProvider::OpenAI => serde_json::json!({
                    "type": "input_audio_buffer.append",
                    "audio": b64
                }),
            };
            if let Some(ref tee) = capture_tee_tx {
                let _ = tee.send(pcm16);
            }
            let mut sink = capture_write.lock().await;
            if sink
                .send(WsMessage::Text(ws_msg.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        capture_drain.abort();
        eprintln!("live_audio: vortex capture ended");
    });

    // Playback task: model audio → convert → daemon PCM_INPUT
    let playback_write = write_half;
    let playback_handle = tokio::spawn(async move {
        let mut rx = audio_out_rx;
        while let Some(pcm_data) = rx.recv().await {
            let f32_stereo = vortex_playback_convert(&pcm_data);
            let mut writer = playback_write.lock().await;
            if vortex_write_msg(&mut *writer, VORTEX_MSG_PCM_INPUT, &f32_stereo)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

async fn start_network_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    host_addr: &str,
    audio_out_rx: AudioQueueReceiver,
    capture_tee_tx: Option<AudioQueueSender>,
) -> Result<AudioStreamBridge, CallerError> {
    let stream = tokio::net::TcpStream::connect(host_addr)
        .await
        .map_err(|e| {
            CallerError::Agent(format!("bh-bridge connect to {} failed: {}", host_addr, e))
        })?;

    let (read_half, write_half) = tokio::io::split(stream);

    eprintln!("live_audio: network bridge connected to {}", host_addr);

    // Capture task: read PCM from TCP (host captures app audio) → send to model
    let capture_write = session_write;
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_handle = tokio::spawn(async move {
        let mut reader = read_half;
        let chunk_size = (capture_rate as usize) * 2 / 10; // ~100ms
        let mut buf = vec![0u8; chunk_size];
        let mut chunks_sent = 0usize;

        while reader.read_exact(&mut buf).await.is_ok() {
            chunks_sent += 1;
            let b64 = BASE64.encode(&buf);
            let msg = match capture_provider {
                LiveAudioProvider::Gemini => serde_json::json!({
                    "realtime_input": {
                        "media_chunks": [{
                            "mime_type": format!("audio/pcm;rate={}", capture_rate),
                            "data": b64
                        }]
                    }
                }),
                LiveAudioProvider::OpenAI => serde_json::json!({
                    "type": "input_audio_buffer.append",
                    "audio": b64
                }),
            };
            if let Some(ref tee) = capture_tee_tx {
                let _ = tee.send(buf.clone());
            }
            let mut sink = capture_write.lock().await;
            if sink
                .send(WsMessage::Text(msg.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        eprintln!(
            "live_audio: network capture ended after {} chunks",
            chunks_sent
        );
    });

    // Playback task: model audio → write PCM to TCP (host plays to app mic)
    let playback_handle = tokio::spawn(async move {
        let mut writer = write_half;
        let mut rx = audio_out_rx;
        let mut total = 0usize;
        while let Some(pcm_data) = rx.recv().await {
            total += pcm_data.len();
            if writer.write_all(&pcm_data).await.is_err() {
                break;
            }
        }
        eprintln!("live_audio: network playback ended — {} bytes", total);
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

/// Local fallback audio bridge: spawns capture/playback subprocesses via
/// platform virtual devices (PulseAudio null sinks or BlackHole).
async fn start_local_audio_bridge(
    session_write: Arc<Mutex<WsSink>>,
    provider: LiveAudioProvider,
    sample_rate: u32,
    bridge: &AudioBridge,
    audio_out_rx: AudioQueueReceiver,
    capture_tee_tx: Option<AudioQueueSender>,
) -> Result<AudioStreamBridge, CallerError> {
    // Capture task: platform command -> model
    let (capture_cmd, capture_args) = bridge.capture_command(sample_rate);
    let capture_write = session_write.clone();
    let capture_rate = sample_rate;
    let capture_provider = provider;
    let capture_cmd = capture_cmd.to_string();

    let capture_handle = tokio::spawn(async move {
        let result = tokio::process::Command::new(&capture_cmd)
            .args(&capture_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                eprintln!("live_audio: {} spawn failed: {}", capture_cmd, e);
                return;
            }
        };

        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };

        let chunk_size = (capture_rate as usize) * 2 / 10;
        let mut buf = vec![0u8; chunk_size];

        while stdout.read_exact(&mut buf).await.is_ok() {
            let b64 = BASE64.encode(&buf);
            let msg = match capture_provider {
                LiveAudioProvider::Gemini => serde_json::json!({
                    "realtime_input": {
                        "media_chunks": [{
                            "mime_type": format!("audio/pcm;rate={}", capture_rate),
                            "data": b64
                        }]
                    }
                }),
                LiveAudioProvider::OpenAI => serde_json::json!({
                    "type": "input_audio_buffer.append",
                    "audio": b64
                }),
            };
            if let Some(ref tee) = capture_tee_tx {
                let _ = tee.send(buf.clone());
            }
            let mut sink = capture_write.lock().await;
            if sink
                .send(WsMessage::Text(msg.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }

        let _ = child.kill().await;
    });

    // Playback task: model audio -> platform playback command
    let (playback_cmd, playback_args) = bridge.playback_command(sample_rate);
    let playback_cmd = playback_cmd.to_string();

    let playback_handle = tokio::spawn(async move {
        let result = tokio::process::Command::new(&playback_cmd)
            .args(&playback_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                eprintln!("live_audio: {} spawn failed: {}", playback_cmd, e);
                return;
            }
        };

        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => return,
        };

        let mut rx = audio_out_rx;
        while let Some(pcm_data) = rx.recv().await {
            if stdin.write_all(&pcm_data).await.is_err() {
                break;
            }
        }

        let _ = child.kill().await;
    });

    Ok(AudioStreamBridge {
        capture_handle,
        playback_handle,
    })
}

// ---------------------------------------------------------------------------
// JSON extraction from transcript text
// ---------------------------------------------------------------------------

/// Extract the first valid JSON object from a transcript string.
///
/// Realtime models speak their responses, so the transcript may contain prose
/// before or after the JSON. This scans for balanced `{ ... }` and returns the
/// first substring that parses as a JSON object.
fn extract_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escape = false;
            let start = i;
            for j in start..bytes.len() {
                if escape {
                    escape = false;
                    continue;
                }
                match bytes[j] {
                    b'\\' if in_string => escape = true,
                    b'"' => in_string = !in_string,
                    b'{' if !in_string => depth += 1,
                    b'}' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            let candidate = &text[start..=j];
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(candidate)
                            {
                                if parsed.is_object() {
                                    return Some(candidate.to_string());
                                }
                            }
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Transcript logger
// ---------------------------------------------------------------------------

pub struct TranscriptLogger {
    file: tokio::fs::File,
    path: PathBuf,
}

impl TranscriptLogger {
    pub async fn new(dir: &Path, live_audio_id: &str) -> Result<Self, CallerError> {
        let transcript_dir = dir.join(format!("live_audio_{}", live_audio_id));
        tokio::fs::create_dir_all(&transcript_dir).await?;
        let path = transcript_dir.join("transcript.jsonl");
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self { file, path })
    }

    pub async fn log(&mut self, speaker: &str, text: &str) -> Result<(), CallerError> {
        let entry = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "speaker": speaker,
            "text": text,
        });
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Full session orchestrator
// ---------------------------------------------------------------------------

/// Inbound transcription window length: seconds of buffered call audio per
/// Whisper request. ~3s balances transcript latency against per-request
/// overhead (unchanged from the original inline pipeline).
const WHISPER_WINDOW_SECONDS: usize = 3;

/// Bound on transcription windows queued behind the in-flight Whisper call.
/// Each window is [`WHISPER_WINDOW_SECONDS`] of voiced audio, so 4 windows
/// keeps at most ~12s of speech waiting through a slow API spell; beyond
/// that the oldest window is dropped — a transcript gap beats an
/// ever-growing backlog a slow API may never catch up on.
const WHISPER_MAX_PENDING_WINDOWS: usize = 4;

/// Buffer inbound audio chunks from the capture tee, accumulate
/// [`WHISPER_WINDOW_SECONDS`] windows, run silence detection, and send
/// voiced windows to the transcriber. Results are appended to the transcript
/// JSONL as "app" speaker entries.
///
/// Requests are sequential (at most one in flight) so transcript entries
/// land in capture order, but the intake never stalls behind them: while a
/// request is in flight, chunks keep draining and completed windows queue in
/// a bounded drop-oldest FIFO ([`WHISPER_MAX_PENDING_WINDOWS`]). The
/// in-flight future is held inside this task rather than spawned, so
/// aborting the task cancels the request with it.
///
/// The transcriber is constructed by `run_session` from the project's
/// `[transcription]` config — this loop only ever runs when the project has
/// explicitly opted in.
async fn whisper_inbound_loop<T>(
    transcriber: T,
    mut rx: AudioQueueReceiver,
    sample_rate: u32,
    transcript_path: &Path,
) where
    T: crate::transcription::Transcriber + 'static,
{
    use crate::transcription;
    use std::future::Future;
    use std::pin::Pin;

    let transcriber = Arc::new(transcriber);

    let threshold = (sample_rate as usize) * 2 * WHISPER_WINDOW_SECONDS; // 16-bit mono
    let mut audio_buf: Vec<u8> = Vec::with_capacity(threshold);
    let rms_threshold = 1000.0f64;

    let mut pending_windows: VecDeque<Vec<u8>> = VecDeque::new();
    let mut window_drop_log = DropLog::new("whisper-window");
    let mut in_flight: Option<Pin<Box<dyn Future<Output = Option<String>> + Send>>> = None;
    let mut intake_open = true;

    // Open transcript file for appending
    let mut transcript_file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(transcript_path)
        .await
    {
        Ok(f) => f,
        Err(_) => return,
    };

    loop {
        // Launch the next queued window whenever the lane is idle; requests
        // stay sequential so results publish in capture order.
        if in_flight.is_none() {
            if let Some(window) = pending_windows.pop_front() {
                let transcriber = Arc::clone(&transcriber);
                in_flight = Some(Box::pin(async move {
                    let wav = transcription::encode_wav(&window, sample_rate, 1);
                    match transcriber.transcribe(&wav).await {
                        Ok(segment) => Some(segment.text),
                        Err(_) => None,
                    }
                }));
            } else if !intake_open {
                break; // intake closed and every window drained
            }
        }

        tokio::select! {
            chunk = rx.recv(), if intake_open => {
                let Some(chunk) = chunk else {
                    intake_open = false;
                    continue;
                };
                audio_buf.extend_from_slice(&chunk);
                if audio_buf.len() < threshold {
                    continue;
                }

                // RMS silence detection
                let rms = {
                    let samples = audio_buf
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64);
                    let n = audio_buf.len() / 2;
                    let sum_sq: f64 = samples.map(|s| s * s).sum();
                    if n > 0 {
                        (sum_sq / n as f64).sqrt()
                    } else {
                        0.0
                    }
                };

                if rms < rms_threshold {
                    audio_buf.clear();
                    continue;
                }

                if pending_windows.len() >= WHISPER_MAX_PENDING_WINDOWS {
                    pending_windows.pop_front();
                    window_drop_log.note(1, WHISPER_WINDOW_SECONDS as f64);
                }
                pending_windows.push_back(std::mem::take(&mut audio_buf));
            }
            text = async {
                match in_flight.as_mut() {
                    Some(request) => request.await,
                    None => std::future::pending().await,
                }
            }, if in_flight.is_some() => {
                in_flight = None;
                let Some(text) = text else { continue };
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let entry = serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "speaker": "app",
                    "text": text,
                });
                let mut line = serde_json::to_string(&entry).unwrap_or_default();
                line.push('\n');
                let _ = transcript_file.write_all(line.as_bytes()).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Always-consent spawn gate
// ---------------------------------------------------------------------------
//
// Spawning a live audio session hands an untrusted sub-agent a microphone
// and speaker, so `ActionCategory::LiveAudioSpawn` is policy-pinned to
// "always ask" at every autonomy level (`AutonomyState::needs_approval`).
// Runtime-command classification never sees controller-side tools, though,
// so the policy has to be enforced at dispatch: this gate is the one
// enforcement point shared by every path that can start live audio (the
// native agent loop's tool batch and the MCP/ctl tool), and it runs before
// any spawn side effect (audio bridge creation, default-device switch,
// provider connection).
//
// Resolution mirrors `ask_user` (mcp/tools_ask.rs): the gate races the
// dispatch path's approval registry (popped directly by the MCP
// approve/deny tools) against the event bus's `ControlCommand` approval
// verbs (how the web dashboard, tunnel, and control socket resolve), so it
// works uniformly across daemon shapes. Ids come from a dedicated high
// range so they can never collide with per-session turn-keyed approvals or
// the ask range, and the session supervisor's approval routing consults
// [`spawn_consent_pending`] so a gate id is not misreported as an unknown
// approval.

/// Base of the live-audio consent id range. Per-session loops key approvals
/// by small turn counters and `ask_user` draws from `1 << 40`; this range is
/// disjoint from both while staying below JS's `Number.MAX_SAFE_INTEGER`.
const SPAWN_CONSENT_ID_BASE: u64 = 1 << 41;

static SPAWN_CONSENT_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(SPAWN_CONSENT_ID_BASE);

fn next_spawn_consent_id() -> u64 {
    SPAWN_CONSENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Default blocking wait for the consent prompt (same as `ask_user`).
pub(crate) const SPAWN_CONSENT_WAIT: Duration = Duration::from_secs(300);

/// Ids of live-audio consent prompts currently blocked on a decision.
/// Advisory: resolution happens in the gate's own waiter, but other
/// resolution paths (the session supervisor's per-session approval
/// registries) consult this set so a gate id is not misreported as an
/// unknown/expired approval — the same contract as
/// `mcp::ask_user_question_pending`.
fn pending_consents() -> &'static std::sync::Mutex<std::collections::HashSet<u64>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u64>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Whether `id` is a live-audio consent prompt still waiting for a decision.
/// Consulted by the session supervisor before warning about an approval id
/// it does not know: the gate's own waiter resolves it and emits
/// `ApprovalResolved`.
pub(crate) fn spawn_consent_pending(id: u64) -> bool {
    pending_consents()
        .lock()
        .map(|set| set.contains(&id))
        .unwrap_or(false)
}

/// Proof that a human approved one live-audio spawn.
///
/// The only mint is [`request_spawn_consent`]; [`run_session`] demands one,
/// so a dispatch path that skips the gate is a compile error rather than a
/// policy bug. Single-use by move, deliberately neither `Clone` nor `Copy`,
/// and the private field keeps construction inside this module.
#[derive(Debug)]
pub(crate) struct SpawnConsent(());

/// Everything the consent gate needs from a dispatch path.
pub(crate) struct SpawnConsentRequest<'a> {
    pub bus: &'a crate::event::EventBus,
    /// The approval registry this path's direct resolvers pop (the session
    /// loop's registry on the native path, the MCP state's on `/mcp`).
    pub approval_registry: Option<&'a crate::event::ApprovalRegistry>,
    /// Native JSON mode: approvals resolve over the stdin slot instead of
    /// the registry/bus.
    pub json_approval: Option<&'a crate::JsonApprovalSlot>,
    /// True when no frontend can possibly answer (native headless without a
    /// JSON slot; an MCP state without interactive frontends). The gate then
    /// fails closed immediately with a clear error.
    pub no_approver: bool,
    pub session_id: Option<String>,
    /// Human-readable request line for the approval rail.
    pub preview: String,
}

/// Cleanup for one blocked consent prompt. Whatever way the gate returns —
/// approved, denied, timed out, or the future dropped mid-wait (ctl killed,
/// HTTP connection gone) — the pending entry, any unclaimed registry
/// responder, and the frontend rails are all cleared, so a dead prompt can
/// never leave a zombie approval pending on dashboards. Mirrors
/// `PendingAskGuard`.
struct ConsentGuard {
    id: u64,
    session_id: Option<String>,
    bus: crate::event::EventBus,
    registry: Option<crate::event::ApprovalRegistry>,
    resolved: bool,
}

impl ConsentGuard {
    fn register(
        id: u64,
        session_id: Option<String>,
        bus: crate::event::EventBus,
        registry: Option<crate::event::ApprovalRegistry>,
    ) -> Self {
        if let Ok(mut set) = pending_consents().lock() {
            set.insert(id);
        }
        Self {
            id,
            session_id,
            bus,
            registry,
            resolved: false,
        }
    }

    /// Mark the prompt resolved as `action`: drop the pending entry, remove
    /// any unclaimed registry responder, and tell every frontend to clear
    /// the rail. Idempotent.
    fn resolve(&mut self, action: &str) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        if let Ok(mut set) = pending_consents().lock() {
            set.remove(&self.id);
        }
        if let Some(registry) = &self.registry {
            if let Ok(mut reg) = registry.lock() {
                reg.remove(&self.id);
            }
        }
        self.bus.send(crate::event::AppEvent::ApprovalResolved {
            session_id: self.session_id.clone(),
            id: self.id,
            action: action.to_string(),
        });
    }
}

impl Drop for ConsentGuard {
    fn drop(&mut self) {
        self.resolve("cancelled");
    }
}

/// Denial handed to the model when nobody can approve.
pub(crate) const SPAWN_CONSENT_NO_APPROVER: &str =
    "Denied: live audio always requires explicit human approval, and no approver surface is \
     available (headless). Ask the user to run with the dashboard or another interactive \
     frontend if live audio is needed.";

const SPAWN_CONSENT_DENIED: &str = "Denied: the user declined the live audio session.";
const SPAWN_CONSENT_SKIPPED: &str = "Denied: the user skipped the live audio session.";

fn spawn_consent_timeout_message(wait: Duration) -> String {
    format!(
        "Denied: no approval arrived within {}s for the live audio session. The user may be \
         away; ask again later if it is still needed.",
        wait.as_secs()
    )
}

/// Block until a human approves or denies this live-audio spawn.
///
/// Returns the [`SpawnConsent`] token on approval and
/// `Err(message_for_the_model)` otherwise (denied, skipped, timed out, or
/// unapprovable). `ApproveAll` approves this prompt only — it never records
/// a session-wide grant, because `LiveAudioSpawn` is always-ask by policy;
/// the gate deliberately never touches the autonomy guard or the
/// approved-command dedup set.
pub(crate) async fn request_spawn_consent(
    req: SpawnConsentRequest<'_>,
    wait: Duration,
) -> Result<SpawnConsent, String> {
    use crate::event::{AppEvent, ApprovalResponse, ControlMsg};

    let id = next_spawn_consent_id();

    // Native JSON mode: the stdin loop owns the response channel. Arm the
    // slot before announcing so an instant response cannot miss it.
    if let Some(slot) = req.json_approval {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut guard = slot.lock().unwrap();
            *guard = Some((id, tx));
        }
        req.bus.send(AppEvent::ApprovalRequired {
            session_id: req.session_id.clone(),
            id,
            command_preview: req.preview.clone(),
            category: crate::autonomy::ActionCategory::LiveAudioSpawn,
        });
        let outcome = tokio::time::timeout(wait, rx).await;
        if outcome.is_err() {
            // Timed out: clear the slot if our prompt still holds it.
            let mut guard = slot.lock().unwrap();
            if matches!(&*guard, Some((slot_id, _)) if *slot_id == id) {
                *guard = None;
            }
        }
        let (action, verdict) = match outcome {
            Ok(Ok(ApprovalResponse::Approve | ApprovalResponse::ApproveAll)) => {
                ("approve", Ok(SpawnConsent(())))
            }
            Ok(Ok(ApprovalResponse::Skip)) => ("skip", Err(SPAWN_CONSENT_SKIPPED.to_string())),
            Ok(Ok(ApprovalResponse::Deny | ApprovalResponse::Answer { .. })) | Ok(Err(_)) => {
                ("deny", Err(SPAWN_CONSENT_DENIED.to_string()))
            }
            Err(_) => ("timeout", Err(spawn_consent_timeout_message(wait))),
        };
        req.bus.send(AppEvent::ApprovalResolved {
            session_id: req.session_id.clone(),
            id,
            action: action.to_string(),
        });
        return verdict;
    }

    if req.no_approver {
        return Err(SPAWN_CONSENT_NO_APPROVER.to_string());
    }

    // Subscribe BEFORE announcing (an instant resolution must find the
    // waiter listening), then arm both resolution sources.
    let mut events = req.bus.subscribe();
    let mut guard = ConsentGuard::register(
        id,
        req.session_id.clone(),
        req.bus.clone(),
        req.approval_registry.cloned(),
    );
    let mut registry_rx = req.approval_registry.map(|registry| {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut reg) = registry.lock() {
            reg.insert(id, tx);
        }
        rx
    });
    req.bus.send(AppEvent::ApprovalRequired {
        session_id: req.session_id.clone(),
        id,
        command_preview: req.preview.clone(),
        category: crate::autonomy::ActionCategory::LiveAudioSpawn,
    });

    let deadline = Instant::now() + wait;
    loop {
        tokio::select! {
            response = async {
                match registry_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                // A direct resolver popped the registry responder.
                return match response {
                    Ok(ApprovalResponse::Approve | ApprovalResponse::ApproveAll) => {
                        guard.resolve("approve");
                        Ok(SpawnConsent(()))
                    }
                    Ok(ApprovalResponse::Skip) => {
                        guard.resolve("skip");
                        Err(SPAWN_CONSENT_SKIPPED.to_string())
                    }
                    Ok(ApprovalResponse::Deny | ApprovalResponse::Answer { .. }) | Err(_) => {
                        guard.resolve("deny");
                        Err(SPAWN_CONSENT_DENIED.to_string())
                    }
                };
            }
            event = tokio::time::timeout_at(deadline, events.recv()) => {
                match event {
                    Err(_) => {
                        guard.resolve("timeout");
                        return Err(spawn_consent_timeout_message(wait));
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        guard.resolve("cancelled");
                        return Err(
                            "Denied: the approval channel closed before a decision arrived."
                                .to_string(),
                        );
                    }
                    Ok(Ok(AppEvent::ControlCommand(msg))) => match msg {
                        // Ids from the dedicated consent range are globally
                        // unique — match on id alone (same as `ask_user`).
                        ControlMsg::Approve { id: verb_id, .. }
                        | ControlMsg::ApproveAll { id: verb_id, .. }
                            if verb_id == id =>
                        {
                            guard.resolve("approve");
                            return Ok(SpawnConsent(()));
                        }
                        ControlMsg::Deny { id: verb_id, .. } if verb_id == id => {
                            guard.resolve("deny");
                            return Err(SPAWN_CONSENT_DENIED.to_string());
                        }
                        ControlMsg::Skip { id: verb_id, .. } if verb_id == id => {
                            guard.resolve("skip");
                            return Err(SPAWN_CONSENT_SKIPPED.to_string());
                        }
                        _ => continue,
                    },
                    Ok(Ok(_)) => continue,
                }
            }
        }
    }
}

/// The approval-rail preview line for a spawn request.
pub(crate) fn spawn_consent_preview(spec: &LiveAudioSpec) -> String {
    format!("spawn_live_audio ({:?}, id: {})", spec.provider, spec.id)
}

/// Run a complete live audio session: connect, bridge audio, capture transcript,
/// validate response, quarantine unexpected content.
///
/// This is the main entry point called from the agent loop when handling a
/// `spawn_live_audio` tool call. It blocks until the call finishes or times out.
/// Requires a [`SpawnConsent`] minted by [`request_spawn_consent`]: the
/// always-ask policy holds by construction, not by caller discipline.
pub async fn run_session(
    spec: &LiveAudioSpec,
    _consent: SpawnConsent,
    api_key: &str,
    bridge: &AudioBridge,
    session_log_dir: &Path,
    event_bus: Option<&crate::event::EventBus>,
    transcription: &crate::transcription::TranscriptionConfig,
) -> Result<LiveAudioResult, CallerError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(spec.timeout_secs);

    // Build whitelisted tool definitions from the response schema
    let (openai_tools, gemini_tools) = build_live_audio_tools(&spec.response_schema);

    // Connect to the live model
    let mut session = match spec.provider {
        LiveAudioProvider::Gemini => {
            connect_gemini(
                api_key,
                spec.model.as_deref(),
                &spec.playbook,
                spec.voice.as_deref(),
                24000,
                &gemini_tools,
            )
            .await?
        }
        LiveAudioProvider::OpenAI => {
            connect_openai(
                api_key,
                spec.model.as_deref(),
                &spec.playbook,
                spec.voice.as_deref(),
                24000,
                &openai_tools,
            )
            .await?
        }
    };

    // Emit started event
    if let Some(bus) = event_bus {
        bus.send(crate::event::AppEvent::LiveAudioStarted {
            id: spec.id.clone(),
            provider: format!("{:?}", spec.provider),
        });
    }

    // Set up transcript logger
    let mut transcript = TranscriptLogger::new(session_log_dir, &spec.id).await?;

    // PCM16 mono byte rate at the session's negotiated sample rate — the
    // duration accounting for both bounded audio lanes below.
    let pcm_bytes_per_sec = session.sample_rate as usize * 2;

    // Lane for routing model audio output to the playback task: duration-
    // bounded drop-oldest, so a wedged playback sink skips ahead instead of
    // buffering the whole call.
    let (audio_out_tx, audio_out_rx) =
        bounded_audio_queue("playback", PLAYBACK_QUEUE_SECONDS, pcm_bytes_per_sec);

    // Set up the Whisper transcription tee for inbound audio — only when the
    // project has explicitly opted in ([transcription] enabled = true).
    // Fail-closed: by default no call audio leaves the box for transcription.
    let (capture_tee_tx, whisper_handle) = if transcription.enabled {
        match crate::transcription::WhisperTranscriber::new(transcription) {
            Ok(transcriber) => {
                let (tee_tx, tee_rx) =
                    bounded_audio_queue("whisper-tee", CAPTURE_QUEUE_SECONDS, pcm_bytes_per_sec);
                let whisper_transcript_path = transcript.path().to_path_buf();
                let handle = tokio::spawn(async move {
                    whisper_inbound_loop(transcriber, tee_rx, 24000, &whisper_transcript_path)
                        .await;
                });
                (Some(tee_tx), Some(handle))
            }
            Err(e) => {
                if let Some(bus) = event_bus {
                    bus.send(crate::event::AppEvent::PresenceLog {
                        message: format!(
                            "Live audio: transcription enabled but unavailable ({e}); \
                             continuing without inbound transcription"
                        ),
                        level: None,
                        turn: None,
                    });
                }
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // Start the audio bridge

    let audio_bridge = start_audio_bridge(
        session.ws_write.clone(),
        session.provider,
        session.sample_rate,
        bridge,
        audio_out_rx,
        capture_tee_tx,
    )
    .await?;

    // Send initial message if provided (e.g. "The call has connected.")
    if let Some(ref msg) = spec.initial_message {
        session.send_text(msg).await?;
    }

    // Collect model text output and quarantine payloads
    let mut model_text = String::new();
    let mut model_transcript_buf = String::new();
    let mut quarantine_ids = Vec::new();
    // Silence watchdog state
    let mut last_model_output = Instant::now();
    let mut silence_nudged = false;
    // Turn counter: nudge the model to emit JSON after enough turns
    let mut turn_complete_count = 0u32;
    let mut json_nudged = false;
    // Throttle progress events to avoid flooding the event bus
    let mut last_progress_emit = Instant::now();

    // Event processing loop
    let mut status = loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            break LiveAudioStatus::TimedOut;
        }
        let remaining = timeout - elapsed;

        // Silence watchdog: if no model output for 15s, nudge the model.
        // This prevents indefinite hangs when the model freezes on
        // unexpected input.
        let silence_limit = Duration::from_secs(15);
        let time_since_output = last_model_output.elapsed();
        if time_since_output >= silence_limit && !silence_nudged {
            silence_nudged = true;
            let _ = session
                .send_text("Are you still there? Please continue the conversation.")
                .await;
        }

        match tokio::time::timeout(remaining.min(silence_limit), session.event_rx.recv()).await {
            Ok(Some(event)) => match event {
                LiveAudioEvent::AudioOut(pcm) => {
                    last_model_output = Instant::now();
                    silence_nudged = false;
                    let _ = audio_out_tx.send(pcm);
                }
                LiveAudioEvent::ModelTranscript(text) => {
                    last_model_output = Instant::now();
                    silence_nudged = false;
                    let _ = transcript.log("model", &text).await;
                    model_transcript_buf.push_str(&text);

                    // Emit progress (throttled to ~2s intervals to avoid
                    // flooding the event bus with 100+ near-identical events)
                    if let Some(bus) = event_bus {
                        if last_progress_emit.elapsed() >= Duration::from_secs(2) {
                            last_progress_emit = Instant::now();
                            let preview = if model_transcript_buf.len() > 200 {
                                {
                                    let start = model_transcript_buf.len() - 200;
                                    let start = model_transcript_buf.ceil_char_boundary(start);
                                    model_transcript_buf[start..].to_string()
                                }
                            } else {
                                model_transcript_buf.clone()
                            };
                            bus.send(crate::event::AppEvent::LiveAudioProgress {
                                id: spec.id.clone(),
                                state: "speaking".into(),
                                elapsed_secs: start.elapsed().as_secs_f64(),
                                transcript_preview: preview,
                            });
                        } // throttle
                    } // bus
                }
                LiveAudioEvent::ModelText(text) => {
                    model_text.push_str(&text);
                }
                LiveAudioEvent::FunctionCall {
                    name,
                    call_id,
                    args,
                } => {
                    if name == FN_SUBMIT_RESPONSE {
                        // Reject null/empty submissions — the voice model sometimes
                        // calls submit_response with no arguments. Keep the session
                        // alive so it can try again.
                        if args.is_null()
                            || (args.is_object() && args.as_object().unwrap().is_empty())
                        {
                            continue;
                        }
                        // The model submitted structured response via function call.
                        // Don't break yet — the model may still be speaking.
                        // Wait for end_call or a drain timeout.
                        model_text = serde_json::to_string(&args).unwrap_or_default();
                        // Send function call output back to acknowledge (OpenAI requires this)
                        if spec.provider == LiveAudioProvider::OpenAI && !call_id.is_empty() {
                            let ack = serde_json::json!({
                                "type": "conversation.item.create",
                                "item": {
                                    "type": "function_call_output",
                                    "call_id": call_id,
                                    "output": "{\"status\":\"ok\"}"
                                }
                            });
                            let mut sink = session.ws_write.lock().await;
                            let _ = sink.send(WsMessage::Text(ack.to_string().into())).await;
                        }
                        // Drain: keep playing audio for up to 5s waiting for end_call
                        let drain_deadline = Instant::now() + Duration::from_secs(5);
                        loop {
                            let remaining =
                                drain_deadline.saturating_duration_since(Instant::now());
                            if remaining.is_zero() {
                                break;
                            }
                            match tokio::time::timeout(remaining, session.event_rx.recv()).await {
                                Ok(Some(LiveAudioEvent::FunctionCall { name, .. }))
                                    if name == FN_END_CALL =>
                                {
                                    break
                                }
                                Ok(Some(LiveAudioEvent::AudioOut(pcm))) => {
                                    let _ = audio_out_tx.send(pcm);
                                }
                                Ok(Some(LiveAudioEvent::Disconnected(_))) | Ok(None) | Err(_) => {
                                    break
                                }
                                Ok(Some(_)) => {} // keep draining other events
                            }
                        }
                        break LiveAudioStatus::Completed;
                    } else if name == FN_END_CALL {
                        // Model signaled call is done — allow buffered audio to finish
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        break LiveAudioStatus::Completed;
                    }
                }
                LiveAudioEvent::ToolCallAttempted { name, args } => {
                    // Quarantine unknown tool call attempts
                    let content = serde_json::json!({"name": name, "args": args}).to_string();
                    match quarantine::store_payload(&spec.id, "tool_call_attempt", &content) {
                        Ok(payload) => quarantine_ids.push(payload.payload_id),
                        Err(e) => eprintln!("live_audio: quarantine write failed: {}", e),
                    }
                }
                LiveAudioEvent::Disconnected(_reason) => {
                    break LiveAudioStatus::Disconnected;
                }
                LiveAudioEvent::Error(e) => {
                    break LiveAudioStatus::Failed(e);
                }
                LiveAudioEvent::TurnComplete => {
                    turn_complete_count += 1;

                    // Fallback: check if JSON appeared in text/transcript (for models
                    // that speak JSON instead of using function calls).
                    let extracted = extract_json_object(&model_text)
                        .or_else(|| extract_json_object(&model_transcript_buf));

                    if let Some(json_str) = extracted {
                        model_text = json_str;
                        break LiveAudioStatus::Completed;
                    }

                    // Nudge the model to call submit_response after enough turns
                    if turn_complete_count >= 6 && !json_nudged {
                        json_nudged = true;
                        let _ = session.send_text(
                            "The conversation is complete. Please call the submit_response function now with the data you collected, then call end_call."
                        ).await;
                    }
                }
                LiveAudioEvent::Interrupted => {}
                LiveAudioEvent::Connected | LiveAudioEvent::SetupComplete => {}
            },
            Ok(None) => {
                // Channel closed — session ended
                break LiveAudioStatus::Disconnected;
            }
            Err(_) => {
                // Inner timeout (silence_limit) expired — check if the
                // overall session timeout has been reached. If not, loop
                // back so the silence watchdog nudge gets a chance to work.
                if start.elapsed() >= timeout {
                    break LiveAudioStatus::TimedOut;
                }
                // Otherwise continue the loop — the silence nudge at the
                // top of the loop will fire on the next iteration.
            }
        }
    };

    // Stop the audio bridge and whisper task
    audio_bridge.stop();
    if let Some(handle) = whisper_handle {
        handle.abort();
    }

    // Close the WebSocket
    session.close().await;

    // Final attempt to extract JSON from accumulated buffers (covers timeout/disconnect
    // cases where no TurnComplete fired after the model produced JSON).
    if model_text.is_empty() || serde_json::from_str::<serde_json::Value>(&model_text).is_err() {
        if let Some(json_str) =
            extract_json_object(&model_text).or_else(|| extract_json_object(&model_transcript_buf))
        {
            model_text = json_str;
            if status == LiveAudioStatus::TimedOut {
                status = LiveAudioStatus::Completed;
            }
        }
    }

    // Validate the structured response
    let (response_data, final_status) = if !model_text.is_empty() {
        match serde_json::from_str::<serde_json::Value>(&model_text) {
            Ok(value) => {
                let mut qfn = quarantine::make_quarantine_fn(spec.id.clone());
                match schema_validator::validate(&spec.response_schema, &value, &mut qfn) {
                    Ok((validated, extra_quarantined)) => {
                        for q in &extra_quarantined {
                            quarantine_ids.push(q.payload_id.clone());
                        }
                        (Some(validated), status)
                    }
                    Err(errors) => {
                        let error_msg = errors
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join("; ");
                        // Quarantine the raw model output
                        if let Ok(payload) =
                            quarantine::store_payload(&spec.id, "schema_violation", &model_text)
                        {
                            quarantine_ids.push(payload.payload_id);
                        }
                        (None, LiveAudioStatus::SchemaError(error_msg))
                    }
                }
            }
            Err(_) => {
                // Model text wasn't valid JSON — quarantine it
                if let Ok(payload) =
                    quarantine::store_payload(&spec.id, "invalid_json", &model_text)
                {
                    quarantine_ids.push(payload.payload_id);
                }
                (
                    None,
                    LiveAudioStatus::SchemaError("model output was not valid JSON".into()),
                )
            }
        }
    } else {
        (None, status)
    };

    let duration_secs = start.elapsed().as_secs_f64();

    // Emit completed event
    if let Some(bus) = event_bus {
        bus.send(crate::event::AppEvent::LiveAudioCompleted {
            id: spec.id.clone(),
            status: format!("{:?}", final_status),
            quarantine_count: quarantine_ids.len(),
        });
    }

    let result = LiveAudioResult {
        id: spec.id.clone(),
        status: final_status,
        response_data,
        quarantine_ids,
        transcript_path: transcript.path().to_path_buf(),
        duration_secs,
    };

    // Persist result to disk immediately — if the process is killed before
    // run_session returns, the caller never gets the result. Writing it next
    // to the transcript ensures it survives crashes.
    let result_path = transcript.path().with_file_name("result.json");
    if let Ok(json) = serde_json::to_string_pretty(&result) {
        if let Err(err) = tokio::fs::write(&result_path, json).await {
            let message = format!(
                "live_audio: CRITICAL: failed to persist call result {}: {}",
                result_path.display(),
                err
            );
            eprintln!("{message}");
            if let Err(log_err) = transcript.log("app", &message).await {
                eprintln!(
                    "live_audio: CRITICAL: failed to record result-persist failure in transcript: {}",
                    log_err
                );
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Vortex format conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn vortex_capture_convert_stereo_48k_to_mono_24k() {
        // 4 stereo frames at 48kHz → 2 mono samples at 24kHz (2:1 decimation)
        // Frame 0: L=0.5, R=0.5 → mono=0.5 → kept (even index)
        // Frame 1: L=0.25, R=0.75 → mono=0.5 → skipped (odd index)
        // Frame 2: L=-1.0, R=-1.0 → mono=-1.0 → kept
        // Frame 3: L=0.0, R=0.0 → mono=0.0 → skipped
        let mut input = Vec::new();
        for &(l, r) in &[
            (0.5f32, 0.5f32),
            (0.25f32, 0.75f32),
            (-1.0f32, -1.0f32),
            (0.0f32, 0.0f32),
        ] {
            input.extend_from_slice(&l.to_le_bytes());
            input.extend_from_slice(&r.to_le_bytes());
        }
        let output = vortex_capture_convert(&input);
        assert_eq!(output.len(), 4); // 2 i16 samples × 2 bytes

        let s0 = i16::from_le_bytes([output[0], output[1]]);
        let s1 = i16::from_le_bytes([output[2], output[3]]);
        // 0.5 * 32767 ≈ 16383
        assert!((s0 - 16383).abs() <= 1, "s0={}", s0);
        // -1.0 * 32767 = -32767
        assert_eq!(s1, -32767);
    }

    #[test]
    fn vortex_playback_convert_mono_24k_to_stereo_48k() {
        // 2 mono samples at 24kHz → 4 stereo frames at 48kHz
        let input: Vec<u8> = [16383i16, -32767i16]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let output = vortex_playback_convert(&input);
        // 2 samples × 2 (upsample) × 2 (stereo) × 4 (f32) = 32 bytes
        assert_eq!(output.len(), 32);

        // First sample duplicated twice as stereo
        let f0 = f32::from_le_bytes([output[0], output[1], output[2], output[3]]);
        let f1 = f32::from_le_bytes([output[4], output[5], output[6], output[7]]);
        assert!((f0 - 16383.0 / 32768.0).abs() < 0.001);
        assert_eq!(f0, f1); // stereo: L == R
    }

    #[test]
    fn vortex_round_trip_preserves_signal() {
        // Create a 440Hz tone as PCM16 mono 24kHz (model format)
        let num_samples = 240; // 10ms
        let mut pcm16 = Vec::with_capacity(num_samples * 2);
        for i in 0..num_samples {
            let t = i as f32 / 24000.0;
            let val = (t * 440.0 * 2.0 * std::f32::consts::PI).sin();
            let sample = (val * 32767.0) as i16;
            pcm16.extend_from_slice(&sample.to_le_bytes());
        }

        // Playback convert (24k mono → 48k stereo float32)
        let f32_stereo = vortex_playback_convert(&pcm16);
        // Capture convert back (48k stereo float32 → 24k mono pcm16)
        let round_trip = vortex_capture_convert(&f32_stereo);

        assert_eq!(round_trip.len(), pcm16.len());

        // Samples should be close (quantization error ≤ 1)
        for i in 0..num_samples {
            let orig = i16::from_le_bytes([pcm16[i * 2], pcm16[i * 2 + 1]]);
            let rt = i16::from_le_bytes([round_trip[i * 2], round_trip[i * 2 + 1]]);
            assert!(
                (orig - rt).abs() <= 1,
                "sample {}: orig={} round_trip={}",
                i,
                orig,
                rt
            );
        }
    }

    #[test]
    fn vortex_capture_empty_input() {
        assert!(vortex_capture_convert(&[]).is_empty());
    }

    #[test]
    fn vortex_playback_empty_input() {
        assert!(vortex_playback_convert(&[]).is_empty());
    }

    // -----------------------------------------------------------------------
    // Direct-conversion parity tests
    // -----------------------------------------------------------------------

    /// The pre-refactor capture pipeline, kept verbatim as the reference the
    /// direct conversion is pinned against: f32 samples → little-endian byte
    /// image → per-frame byte parsing → PCM16.
    fn reference_capture_two_step(samples: &[f32]) -> Vec<u8> {
        let f32_bytes: Vec<u8> = samples.iter().flat_map(|f| f.to_le_bytes()).collect();
        let num_floats = f32_bytes.len() / 4;
        let num_stereo_frames = num_floats / 2;
        let mut out = Vec::with_capacity((num_stereo_frames / 2) * 2);
        for i in (0..num_stereo_frames).step_by(2) {
            let base = i * 8;
            if base + 8 > f32_bytes.len() {
                break;
            }
            let left = f32::from_le_bytes([
                f32_bytes[base],
                f32_bytes[base + 1],
                f32_bytes[base + 2],
                f32_bytes[base + 3],
            ]);
            let right = f32::from_le_bytes([
                f32_bytes[base + 4],
                f32_bytes[base + 5],
                f32_bytes[base + 6],
                f32_bytes[base + 7],
            ]);
            let mono = (left + right) * 0.5;
            let clamped = mono.clamp(-1.0, 1.0);
            let sample = (clamped * 32767.0) as i16;
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    #[test]
    fn direct_capture_convert_matches_two_step_reference() {
        let mut cases: Vec<Vec<f32>> = vec![
            vec![],
            vec![0.5, -0.5],                    // one stereo frame
            vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6], // odd frame count
            vec![2.0, 2.0, -3.0, 1.0, 1.0, 1.0, -1.0, -1.0], // clamping
        ];
        // Deterministic sweep across the clamped range, unaligned length.
        let sweep: Vec<f32> = (0..1001).map(|i| ((i as f32 * 0.7311) % 3.0) - 1.5).collect();
        cases.push(sweep);

        for samples in &cases {
            let mut direct = Vec::new();
            downmix_decimate_f32_to_pcm16(samples, &mut direct);
            assert_eq!(
                direct,
                reference_capture_two_step(samples),
                "n={}",
                samples.len()
            );
        }
    }

    #[test]
    fn capture_convert_ignores_trailing_partial_frames() {
        // One complete stereo frame + half a frame + 2 stray bytes.
        let mut bytes = Vec::new();
        for v in [0.5f32, 0.5, 0.25] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&[0xAA, 0xBB]);
        let out = vortex_capture_convert(&bytes);
        assert_eq!(out.len(), 2, "one mono sample from the one complete frame");
        let s = i16::from_le_bytes([out[0], out[1]]);
        assert!((s - 16383).abs() <= 1, "s={s}");
    }

    #[test]
    fn direct_playback_convert_matches_byte_image() {
        let pcm: Vec<u8> = [0i16, 16383, -32768, 32767, -1]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut direct = Vec::new();
        pcm16_to_f32_stereo_upsampled(&pcm, &mut direct);
        let parsed: Vec<f32> = vortex_playback_convert(&pcm)
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(direct, parsed);
    }

    // -----------------------------------------------------------------------
    // Capture frame aggregation tests
    // -----------------------------------------------------------------------

    #[test]
    fn capture_aggregator_aggregates_5ms_ticks_into_min_frames() {
        let mut agg = CaptureFrameAggregator::new();
        let tick = 5 * VORTEX_RING_F32_PER_MS; // one 5ms poll's worth
        // Below the 20ms floor nothing is emitted...
        for _ in 0..3 {
            for _ in 0..tick {
                agg.push_sample(0.25);
            }
            assert!(agg.next_full_frame().is_none(), "no frame below the floor");
        }
        // ...the fourth tick crosses it and yields exactly one 20ms frame.
        for _ in 0..tick {
            agg.push_sample(0.25);
        }
        let frame = agg.next_full_frame().expect("full frame at 20ms");
        assert_eq!(frame.len(), CAPTURE_FRAME_MIN_F32);
        assert!(
            agg.next_full_frame().is_none(),
            "nothing pending after an exact frame"
        );
    }

    #[test]
    fn capture_aggregator_splits_bursts_into_max_frames() {
        let mut agg = CaptureFrameAggregator::new();
        // A 100ms burst (ring backlog after a stall) cuts into 40+40+20ms.
        let burst = 100 * VORTEX_RING_F32_PER_MS;
        for i in 0..burst {
            agg.push_sample(i as f32);
        }
        let mut sizes = Vec::new();
        let mut samples = Vec::new();
        while let Some(frame) = agg.next_full_frame() {
            sizes.push(frame.len());
            samples.extend_from_slice(frame);
        }
        assert_eq!(
            sizes,
            vec![
                CAPTURE_FRAME_MAX_F32,
                CAPTURE_FRAME_MAX_F32,
                CAPTURE_FRAME_MIN_F32
            ]
        );
        // Order preserved sample-for-sample across the cuts.
        for (i, s) in samples.iter().enumerate() {
            assert_eq!(*s, i as f32);
        }
        assert!(agg.flush().is_none(), "burst divided evenly, nothing pending");
    }

    #[test]
    fn capture_aggregator_quiet_flush_releases_partial_tail() {
        let mut agg = CaptureFrameAggregator::new();
        let tail = 7 * VORTEX_RING_F32_PER_MS; // 7ms tail, under the 20ms floor
        for i in 0..tail {
            agg.push_sample(i as f32);
        }
        assert!(agg.next_full_frame().is_none());
        let frame = agg.flush().expect("quiet flush emits the partial tail");
        assert_eq!(frame.len(), tail);
        for (i, s) in frame.iter().enumerate() {
            assert_eq!(*s, i as f32);
        }
        assert!(agg.flush().is_none(), "flush drains the aggregator");
    }

    #[test]
    fn capture_aggregator_cuts_on_decimation_boundaries() {
        let mut agg = CaptureFrameAggregator::new();
        // An unaligned backlog (not a multiple of the 4-sample decimation
        // group) must leave the stragglers pending, not smear the phase.
        let n = CAPTURE_FRAME_MIN_F32 + 3;
        for i in 0..n {
            agg.push_sample(i as f32);
        }
        let frame = agg.next_full_frame().expect("frame at the floor");
        assert_eq!(frame.len(), CAPTURE_FRAME_MIN_F32);
        assert_eq!(frame.len() % CAPTURE_DECIMATION_GROUP_F32, 0);
        let tail = agg.flush().expect("stragglers stay pending");
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0], CAPTURE_FRAME_MIN_F32 as f32);
    }

    // -----------------------------------------------------------------------
    // Bounded drop-oldest queue tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn bounded_queue_drops_oldest_on_overflow() {
        // 1s × 1000 B/s = 1000-byte budget.
        let (tx, mut rx) = bounded_audio_queue("test", 1, 1000);
        for tag in 1u8..=5 {
            tx.send(vec![tag; 300]).expect("receiver alive");
        }
        // 5×300 = 1500 > 1000: the two oldest chunks were evicted.
        drop(tx);
        let mut tags = Vec::new();
        while let Some(chunk) = rx.recv().await {
            assert_eq!(chunk.len(), 300);
            tags.push(chunk[0]);
        }
        assert_eq!(tags, vec![3, 4, 5], "oldest dropped, survivor order intact");
    }

    #[tokio::test]
    async fn bounded_queue_never_evicts_the_only_chunk() {
        let (tx, mut rx) = bounded_audio_queue("test", 1, 100); // 100-byte budget
        tx.send(vec![7u8; 500]).expect("receiver alive");
        let got = rx.recv().await.expect("oversized chunk still delivered");
        assert_eq!(got.len(), 500);
    }

    #[tokio::test]
    async fn bounded_queue_send_fails_once_receiver_gone() {
        let (tx, rx) = bounded_audio_queue("test", 1, 1000);
        drop(rx);
        assert!(tx.send(vec![1, 2, 3]).is_err());
    }

    #[tokio::test]
    async fn bounded_queue_recv_drains_then_ends_after_sender_drop() {
        let (tx, mut rx) = bounded_audio_queue("test", 1, 1000);
        tx.send(vec![1]).unwrap();
        tx.send(vec![2]).unwrap();
        drop(tx);
        assert_eq!(rx.recv().await, Some(vec![1]));
        assert_eq!(rx.recv().await, Some(vec![2]));
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn bounded_queue_wakes_blocked_receiver() {
        let (tx, mut rx) = bounded_audio_queue("test", 1, 1000);
        let recv_task = tokio::spawn(async move { rx.recv().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(vec![9]).unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), recv_task)
            .await
            .expect("receiver woke")
            .expect("task");
        assert_eq!(got, Some(vec![9]));
    }

    // -----------------------------------------------------------------------
    // Whisper intake tests (hermetic — stub transcriber, no network)
    // -----------------------------------------------------------------------

    /// Stub transcriber gated on a watch flag: calls stall until the test
    /// opens the gate, then identify their window by its first PCM sample
    /// (offset 44 = end of the standard WAV header).
    struct GatedStubTranscriber {
        release: tokio::sync::watch::Receiver<bool>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::transcription::Transcriber for GatedStubTranscriber {
        async fn transcribe(
            &self,
            audio_wav: &[u8],
        ) -> Result<crate::transcription::TranscriptSegment, CallerError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut release = self.release.clone();
            release
                .wait_for(|open| *open)
                .await
                .map_err(|_| CallerError::Agent("gate dropped".into()))?;
            let tag = i16::from_le_bytes([audio_wav[44], audio_wav[45]]);
            Ok(crate::transcription::TranscriptSegment {
                text: format!("w{tag}"),
                language: None,
                duration_secs: 0.0,
            })
        }
    }

    /// One voiced [`WHISPER_WINDOW_SECONDS`] window of constant amplitude
    /// `tag` (must clear the RMS gate at 1000).
    fn voiced_window(tag: i16, rate: u32) -> Vec<u8> {
        let bytes = rate as usize * 2 * WHISPER_WINDOW_SECONDS;
        let mut window = Vec::with_capacity(bytes);
        for _ in 0..bytes / 2 {
            window.extend_from_slice(&tag.to_le_bytes());
        }
        window
    }

    fn transcript_texts(path: &Path) -> Vec<String> {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        content
            .lines()
            .map(|line| {
                let entry: serde_json::Value = serde_json::from_str(line).expect("jsonl entry");
                assert_eq!(entry["speaker"], "app");
                entry["text"].as_str().expect("text").to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn whisper_intake_keeps_draining_while_transcription_stalls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript_path = dir.path().join("transcript.jsonl");

        // Tiny synthetic rate keeps windows small (600 bytes each).
        let rate: u32 = 100;

        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stub = GatedStubTranscriber {
            release: release_rx,
            calls: calls.clone(),
        };

        let (tee_tx, tee_rx) =
            bounded_audio_queue("whisper-tee", CAPTURE_QUEUE_SECONDS, rate as usize * 2);
        let loop_path = transcript_path.clone();
        let handle = tokio::spawn(async move {
            whisper_inbound_loop(stub, tee_rx, rate, &loop_path).await;
        });

        // Feed 7 voiced windows while the first transcription is stalled.
        // Intake must keep accepting: window 2000 goes in flight, and of the
        // rest the bounded FIFO keeps the newest 4 (2003..=2006), dropping
        // the two oldest queued windows (2001, 2002).
        for tag in 2000i16..=2006 {
            tee_tx
                .send(voiced_window(tag, rate))
                .expect("intake accepts while a transcription is stalled");
        }
        drop(tee_tx);

        // The consumer keeps draining while stalled: exactly one request in
        // flight, everything else queued/bounded, nothing blocked.
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            assert!(Instant::now() < deadline, "first request never launched");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "requests stay sequential while the first is in flight"
        );
        assert!(
            transcript_texts(&transcript_path).is_empty(),
            "nothing published before the stall resolves"
        );

        // Release the gate: the stalled request and the surviving windows
        // resolve in capture order.
        release_tx.send(true).expect("loop alive");
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("whisper loop drains and exits")
            .expect("whisper task");

        assert_eq!(
            transcript_texts(&transcript_path),
            vec!["w2000", "w2003", "w2004", "w2005", "w2006"],
            "results in capture order; only the oldest queued windows dropped"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn whisper_silent_windows_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript_path = dir.path().join("transcript.jsonl");
        let rate: u32 = 100;

        let (release_tx, release_rx) = tokio::sync::watch::channel(true); // gate open
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stub = GatedStubTranscriber {
            release: release_rx,
            calls: calls.clone(),
        };

        let (tee_tx, tee_rx) =
            bounded_audio_queue("whisper-tee", CAPTURE_QUEUE_SECONDS, rate as usize * 2);
        let loop_path = transcript_path.clone();
        let handle = tokio::spawn(async move {
            whisper_inbound_loop(stub, tee_rx, rate, &loop_path).await;
        });

        // A silent window is discarded by the RMS gate; a voiced one lands.
        tee_tx.send(voiced_window(0, rate)).expect("loop alive");
        tee_tx.send(voiced_window(3000, rate)).expect("loop alive");
        drop(tee_tx);

        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("whisper loop exits")
            .expect("whisper task");
        drop(release_tx);

        assert_eq!(transcript_texts(&transcript_path), vec!["w3000"]);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // -----------------------------------------------------------------------
    // Existing tests
    // -----------------------------------------------------------------------

    #[test]
    fn gemini_setup_message_has_no_tools() {
        let setup = serde_json::json!({
            "setup": {
                "model": "models/gemini-2.5-flash",
                "generation_config": {
                    "response_modalities": ["AUDIO"],
                    "speech_config": {
                        "voice_config": {
                            "prebuilt_voice_config": {
                                "voice_name": "Aoede"
                            }
                        }
                    }
                },
                "output_audio_transcription": {},
                "system_instruction": {
                    "parts": [{ "text": "test playbook" }]
                },
                "tools": []
            }
        });

        let tools = setup["setup"]["tools"].as_array().unwrap();
        assert!(tools.is_empty(), "untrusted agent must have zero tools");
    }

    #[test]
    fn openai_setup_message_has_no_tools() {
        let setup = serde_json::json!({
            "type": "session.update",
            "session": {
                "modalities": ["audio", "text"],
                "instructions": "test playbook",
                "voice": "alloy",
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "tools": []
            }
        });

        let tools = setup["session"]["tools"].as_array().unwrap();
        assert!(tools.is_empty(), "untrusted agent must have zero tools");
    }

    #[test]
    fn gemini_audio_send_format() {
        let b64 = BASE64.encode(&[0i16.to_le_bytes()[0], 0i16.to_le_bytes()[1]]);
        let msg = serde_json::json!({
            "realtime_input": {
                "media_chunks": [{
                    "mime_type": "audio/pcm;rate=24000",
                    "data": b64
                }]
            }
        });
        assert!(msg["realtime_input"]["media_chunks"][0]["data"].is_string());
    }

    #[test]
    fn openai_audio_send_format() {
        let b64 = BASE64.encode(&[0u8; 2]);
        let msg = serde_json::json!({
            "type": "input_audio_buffer.append",
            "audio": b64
        });
        assert_eq!(msg["type"], "input_audio_buffer.append");
    }

    #[test]
    fn parse_gemini_server_content_audio() {
        // Simulate a Gemini serverContent message with audio
        let audio_data = BASE64.encode(&[1u8, 2, 3, 4]);
        let msg = serde_json::json!({
            "serverContent": {
                "modelTurn": {
                    "parts": [{
                        "inlineData": {
                            "mimeType": "audio/pcm",
                            "data": audio_data
                        }
                    }]
                }
            }
        });

        // Verify we can extract audio data
        let parts = msg["serverContent"]["modelTurn"]["parts"]
            .as_array()
            .unwrap();
        let inline = &parts[0]["inlineData"];
        assert!(inline["mimeType"].as_str().unwrap().starts_with("audio/"));
        let decoded = BASE64.decode(inline["data"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, vec![1, 2, 3, 4]);
    }

    #[test]
    fn parse_gemini_tool_call_attempt() {
        let msg = serde_json::json!({
            "toolCall": {
                "functionCalls": [{
                    "name": "browse_url",
                    "args": {"url": "http://evil.com"}
                }]
            }
        });

        let fcs = msg["toolCall"]["functionCalls"].as_array().unwrap();
        assert_eq!(fcs.len(), 1);
        assert_eq!(fcs[0]["name"], "browse_url");
    }

    #[test]
    fn parse_openai_function_call_done() {
        let msg = serde_json::json!({
            "type": "response.function_call_arguments.done",
            "name": "exec_command",
            "arguments": "{\"command\":\"ls\"}"
        });

        assert_eq!(msg["type"], "response.function_call_arguments.done");
        let name = msg["name"].as_str().unwrap();
        assert_eq!(name, "exec_command");
        let args: serde_json::Value =
            serde_json::from_str(msg["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn extract_json_from_plain_object() {
        let text = r#"{"status": "ok"}"#;
        assert_eq!(extract_json_object(text).unwrap(), text);
    }

    #[test]
    fn extract_json_from_transcript_with_prose() {
        let text = r#"Test complete. {"status": "ok"}"#;
        assert_eq!(extract_json_object(text).unwrap(), r#"{"status": "ok"}"#);
    }

    #[test]
    fn extract_json_with_trailing_prose() {
        let text = r#"Here it is: {"a": 1, "b": "hello"} That's all."#;
        assert_eq!(
            extract_json_object(text).unwrap(),
            r#"{"a": 1, "b": "hello"}"#
        );
    }

    #[test]
    fn extract_json_nested_braces() {
        let text = r#"Result: {"data": {"inner": true}, "ok": false}"#;
        assert_eq!(
            extract_json_object(text).unwrap(),
            r#"{"data": {"inner": true}, "ok": false}"#
        );
    }

    #[test]
    fn extract_json_with_braces_in_strings() {
        let text = r#"{"msg": "use {x} here"}"#;
        assert_eq!(extract_json_object(text).unwrap(), text);
    }

    #[test]
    fn extract_json_none_when_no_json() {
        assert!(extract_json_object("no json here").is_none());
        assert!(extract_json_object("").is_none());
        assert!(extract_json_object("{ broken").is_none());
    }

    // -----------------------------------------------------------------------
    // Integration tests — real API calls to OpenAI Realtime
    //
    // Requires OPENAI_API_KEY in env. Skipped by `cargo test --bins`.
    // Run with:
    //   cargo test --bin intendant test_live_audio_openai -- --ignored --nocapture
    // -----------------------------------------------------------------------

    const TEST_MODEL: &str = "gpt-realtime-1.5";

    fn require_openai_key() -> Option<String> {
        match std::env::var("OPENAI_API_KEY") {
            Ok(k) if !k.is_empty() => Some(k),
            _ => {
                eprintln!("OPENAI_API_KEY not set, skipping");
                None
            }
        }
    }

    /// Layer 1: WebSocket connects and session.update is accepted.
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_connect() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        let empty_tools = serde_json::json!([]);
        let mut session = connect_openai(
            &api_key,
            Some(TEST_MODEL),
            "You are a test agent.",
            Some("alloy"),
            24000,
            &empty_tools,
        )
        .await
        .expect("connect_openai failed");

        let mut got_setup = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(LiveAudioEvent::SetupComplete)) => {
                    got_setup = true;
                    eprintln!("  SetupComplete received");
                    break;
                }
                Ok(Some(LiveAudioEvent::Connected)) => {
                    eprintln!("  Connected");
                }
                Ok(Some(LiveAudioEvent::Error(e))) => {
                    panic!("session error during setup: {}", e);
                }
                Ok(Some(other)) => {
                    eprintln!("  setup event: {:?}", other);
                }
                Ok(None) | Err(_) => break,
            }
        }

        session.close().await;
        assert!(
            got_setup,
            "did not receive SetupComplete from OpenAI Realtime"
        );
    }

    /// Layer 2: Send text, receive audio + transcript + turn_complete.
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_text_round_trip() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        let empty_tools = serde_json::json!([]);
        let mut session = connect_openai(
            &api_key,
            Some(TEST_MODEL),
            "You are a test assistant. Respond in one short sentence.",
            Some("alloy"),
            24000,
            &empty_tools,
        )
        .await
        .expect("connect_openai failed");

        // Wait for setup
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(LiveAudioEvent::SetupComplete)) => break,
                Ok(Some(_)) => continue,
                _ => panic!("did not receive SetupComplete"),
            }
        }

        // Send text
        session
            .send_text("Say hello.")
            .await
            .expect("send_text failed");
        eprintln!("  Sent text prompt, waiting for response...");

        let mut got_audio = false;
        let mut got_transcript = false;
        let mut got_turn_complete = false;
        let mut audio_bytes = 0usize;
        let mut transcript_buf = String::new();

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(event)) => match event {
                    LiveAudioEvent::AudioOut(pcm) => {
                        audio_bytes += pcm.len();
                        got_audio = true;
                    }
                    LiveAudioEvent::ModelTranscript(text) => {
                        transcript_buf.push_str(&text);
                        got_transcript = true;
                    }
                    LiveAudioEvent::ModelText(text) => {
                        eprintln!("  ModelText: {}", text);
                    }
                    LiveAudioEvent::TurnComplete => {
                        got_turn_complete = true;
                        break;
                    }
                    LiveAudioEvent::Error(e) => panic!("session error: {}", e),
                    LiveAudioEvent::Disconnected(r) => panic!("disconnected: {}", r),
                    _ => {}
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }

        session.close().await;

        eprintln!("  Audio: {} bytes received", audio_bytes);
        eprintln!("  Transcript: {:?}", transcript_buf);
        eprintln!("  TurnComplete: {}", got_turn_complete);

        assert!(got_turn_complete, "did not receive TurnComplete");
        assert!(got_audio, "no audio output received");
        // Transcript is expected but not strictly guaranteed by all models
        if !got_transcript {
            eprintln!("  WARN: no transcript received (model may not support audio_transcript)");
        }
    }

    /// Layer 2.5: Connect with audio bridge, send text kick-off, log all events.
    /// Diagnoses whether the model speaks, produces text, or just sits silent.
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_bridge_diagnostics() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        if !crate::audio_routing::is_available().await {
            eprintln!("virtual audio routing not available, skipping");
            return;
        }

        let session_id = format!(
            "diag-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let mut bridge = crate::audio_routing::create_bridge(&session_id)
            .await
            .expect("create_bridge failed");

        if let Err(e) = crate::audio_routing::set_as_default(&mut bridge).await {
            eprintln!("  WARN: set_as_default: {}", e);
        }

        let playbook = "You are running an automated test. There is nobody on the line. \
                         Say 'test complete' once, then output the JSON: {\"status\": \"ok\"}";

        let empty_tools = serde_json::json!([]);
        let mut session = connect_openai(
            &api_key,
            Some(TEST_MODEL),
            playbook,
            Some("alloy"),
            24000,
            &empty_tools,
        )
        .await
        .expect("connect_openai failed");

        // Wait for setup
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(LiveAudioEvent::SetupComplete)) => {
                    eprintln!("  SetupComplete");
                    break;
                }
                Ok(Some(e)) => eprintln!("  setup: {:?}", e),
                _ => panic!("no SetupComplete"),
            }
        }

        // Start audio bridge (capture silence → model, model audio → playback)
        let (audio_out_tx, audio_out_rx) = bounded_audio_queue(
            "playback",
            PLAYBACK_QUEUE_SECONDS,
            session.sample_rate as usize * 2,
        );
        let audio_bridge = start_audio_bridge(
            session.ws_write.clone(),
            session.provider,
            session.sample_rate,
            &bridge,
            audio_out_rx,
            None,
        )
        .await
        .expect("start_audio_bridge failed");
        eprintln!("  Audio bridge started");

        // Send a text kick-off to prompt the model
        session
            .send_text("Begin the test now.")
            .await
            .expect("send_text failed");
        eprintln!("  Sent text kick-off");

        // Collect all events for up to 20 seconds
        let mut audio_bytes = 0usize;
        let mut audio_chunks = 0usize;
        let mut transcript_buf = String::new();
        let mut text_buf = String::new();
        let mut turn_completes = 0usize;
        let mut tool_calls = Vec::new();

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match tokio::time::timeout(
                deadline.duration_since(Instant::now()),
                session.event_rx.recv(),
            )
            .await
            {
                Ok(Some(event)) => match event {
                    LiveAudioEvent::AudioOut(pcm) => {
                        audio_bytes += pcm.len();
                        audio_chunks += 1;
                        let _ = audio_out_tx.send(pcm);
                    }
                    LiveAudioEvent::ModelTranscript(t) => {
                        eprintln!("  TRANSCRIPT: {:?}", t);
                        transcript_buf.push_str(&t);
                    }
                    LiveAudioEvent::ModelText(t) => {
                        eprintln!("  MODEL_TEXT: {:?}", t);
                        text_buf.push_str(&t);
                    }
                    LiveAudioEvent::TurnComplete => {
                        turn_completes += 1;
                        eprintln!("  TURN_COMPLETE #{}", turn_completes);
                        // Stop after first completed turn
                        break;
                    }
                    LiveAudioEvent::FunctionCall { name, args, .. } => {
                        eprintln!("  FUNCTION_CALL: {} {:?}", name, args);
                        tool_calls.push(name);
                    }
                    LiveAudioEvent::ToolCallAttempted { name, args } => {
                        eprintln!("  TOOL_CALL: {} {:?}", name, args);
                        tool_calls.push(name);
                    }
                    LiveAudioEvent::Interrupted => {
                        eprintln!("  INTERRUPTED");
                    }
                    other => {
                        eprintln!("  {:?}", other);
                    }
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }

        audio_bridge.stop();
        session.close().await;
        drop(bridge);

        eprintln!("\n  === Diagnostics ===");
        eprintln!("  Audio: {} bytes in {} chunks", audio_bytes, audio_chunks);
        eprintln!("  Transcript: {:?}", transcript_buf);
        eprintln!("  ModelText: {:?}", text_buf);
        eprintln!("  TurnCompletes: {}", turn_completes);
        eprintln!("  ToolCalls: {:?}", tool_calls);

        // At minimum, the model should have produced something
        assert!(
            audio_bytes > 0 || !transcript_buf.is_empty() || !text_buf.is_empty(),
            "model produced no output at all"
        );
    }

    /// Interactive phone call test via Vortex audio bridge + pjsua SIP client.
    ///
    /// IMPORTANT: This test requires a GUI login session. macOS gates audio
    /// input behind the WindowServer session — processes from SSH get silence.
    /// Run this test from Terminal.app inside the VM's display, or install
    /// pjsua as a LaunchAgent. Intendant can run from any context; only pjsua
    /// (the app opening mic input) needs GUI session.
    ///
    /// Prerequisites:
    ///   - Vortex guest tools installed (VortexAudioPlugin)
    ///   - Vortex POSIX shared-memory segment available at /vortex-audio
    ///   - ~/bin/pjsua built from pjsip source
    ///   - ~/lin containing the SIP password
    ///   - OPENAI_API_KEY in environment
    ///
    /// Run with:
    ///   cargo test --bin intendant test_live_audio_phone_call -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_phone_call() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        let session_id = format!(
            "call-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let bridge = crate::audio_routing::create_vortex_bridge();

        // Discover Vortex Audio's pjsua device index dynamically.
        let pjsua_bin = dirs::home_dir().unwrap().join("bin/pjsua");
        if !pjsua_bin.exists() {
            eprintln!("~/bin/pjsua not found, skipping");
            return;
        }
        let dev_output = std::process::Command::new(&pjsua_bin)
            .args(["--null-audio"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                if let Some(mut stdin) = c.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(b"q\n");
                }
                c.wait_with_output()
            });
        let vortex_dev_idx = match dev_output {
            Ok(out) => {
                // pjsua prints device list to stdout
                let output = String::from_utf8_lossy(&out.stdout);
                output
                    .lines()
                    .filter(|l| l.contains("dev_id"))
                    .position(|l| l.contains("Vortex Audio"))
                    .map(|i| i.to_string())
            }
            Err(_) => None,
        };
        let dev_idx = match vortex_dev_idx {
            Some(idx) => idx,
            None => {
                eprintln!("Vortex Audio not found in pjsua device list, skipping");
                return;
            }
        };
        let capture_arg = format!("--capture-dev={}", dev_idx);
        let playback_arg = format!("--playback-dev={}", dev_idx);

        let sip_password = match std::fs::read_to_string(dirs::home_dir().unwrap().join("lin")) {
            Ok(p) => p.trim().to_string(),
            Err(_) => {
                eprintln!("~/lin not found (SIP password), skipping");
                return;
            }
        };

        // Launch pjsua AFTER the bridge connects (spawned with delay).
        // pjsua opening Vortex Audio writes to the shared-memory rings. The
        // bridge's poll tasks must be running first.
        // NOTE: pjsua must run in the GUI login session for mic input.
        let pjsua_bin_clone = pjsua_bin.clone();
        let pjsua_handle = tokio::spawn(async move {
            // Wait for the bridge to connect and start draining
            tokio::time::sleep(Duration::from_secs(8)).await;
            let mut pjsua = tokio::process::Command::new(&pjsua_bin_clone)
                .args([
                    "--id=sip:intendant7@sip.linphone.org",
                    "--registrar=sip:sip.linphone.org",
                    "--realm=sip.linphone.org",
                    "--username=intendant7",
                    &format!("--password={}", sip_password),
                    &capture_arg,
                    &playback_arg,
                    "--auto-answer=200",
                    "--ec-tail=0",
                    "--no-vad",
                    "--use-srtp=2",
                    "--srtp-secure=0",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("failed to start pjsua");

            // Wait for registration, then make outbound call
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Some(ref mut stdin) = pjsua.stdin {
                let _ = stdin.write_all(b"m\n").await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = stdin.write_all(b"sip:intendant8@sip.linphone.org\n").await;
            }
            eprintln!("  pjsua calling intendant8 — ANSWER YOUR PHONE!");
            pjsua
        });
        eprintln!("  pjsua will start after bridge connects. Timeout: 120s\n");

        let schema = crate::live_audio_types::ResponseSchema {
            fields: vec![
                crate::live_audio_types::FieldSpec {
                    name: "summary".to_string(),
                    field_type: crate::live_audio_types::FieldType::String {
                        max_length: Some(500),
                        allowed_values: None,
                        tainted: true,
                    },
                    required: true,
                    description: Some(
                        "Brief summary of what was discussed in the call".to_string(),
                    ),
                },
                crate::live_audio_types::FieldSpec {
                    name: "caller_mood".to_string(),
                    field_type: crate::live_audio_types::FieldType::String {
                        max_length: Some(50),
                        allowed_values: Some(vec![
                            "friendly".into(),
                            "neutral".into(),
                            "frustrated".into(),
                            "unknown".into(),
                        ]),
                        tainted: false,
                    },
                    required: true,
                    description: Some("The caller's apparent mood".to_string()),
                },
            ],
        };

        let system_prompt = crate::prompts::build_live_audio_prompt(
            "You are a friendly AI assistant answering a phone call. \
             Greet the caller warmly, ask how you can help, and have a brief \
             natural conversation. After the caller says goodbye or the \
             conversation reaches a natural end, output the response JSON. \
             Keep the call under 60 seconds.",
            &schema,
            None,
        );

        let spec = crate::live_audio_types::LiveAudioSpec {
            id: session_id.clone(),
            provider: crate::live_audio_types::LiveAudioProvider::OpenAI,
            model: Some(TEST_MODEL.to_string()),
            playbook: system_prompt,
            response_schema: schema,
            timeout_secs: 120,
            voice: Some("alloy".to_string()),
            display_id: None,
            initial_message: None,
        };

        let tmp_dir = tempfile::tempdir().expect("create temp dir");

        // In-module mint: the live test stands in for an approved dispatch.
        let result = run_session(
            &spec,
            SpawnConsent(()),
            &api_key,
            &bridge,
            tmp_dir.path(),
            None,
            &crate::transcription::TranscriptionConfig::default(),
        )
        .await
        .expect("run_session failed");

        // Stop pjsua
        if let Ok(mut pjsua) = pjsua_handle.await {
            if let Some(mut stdin) = pjsua.stdin.take() {
                let _ = stdin.write_all(b"q\n").await;
            }
            let _ = pjsua.wait().await;
        }
        drop(bridge);

        // Display transcript
        if result.transcript_path.exists() {
            eprintln!("  === Transcript ===");
            if let Ok(content) = std::fs::read_to_string(&result.transcript_path) {
                for line in content.lines() {
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                        let speaker = entry["speaker"].as_str().unwrap_or("?");
                        let text = entry["text"].as_str().unwrap_or("");
                        eprintln!("  [{}] {}", speaker, text);
                    }
                }
            }
            eprintln!();
        }

        eprintln!("  Status: {:?}", result.status);
        eprintln!("  Duration: {:.1}s", result.duration_secs);
        eprintln!("  Response: {:?}", result.response_data);

        match &result.status {
            LiveAudioStatus::Completed => {
                let data = result.response_data.as_ref().unwrap();
                eprintln!("  Summary: {}", data["summary"].as_str().unwrap_or("?"));
                eprintln!(
                    "  Caller mood: {}",
                    data["caller_mood"].as_str().unwrap_or("?")
                );
            }
            LiveAudioStatus::TimedOut => {
                eprintln!("  Session timed out — model did not produce JSON response");
            }
            other => {
                eprintln!("  Unexpected status: {:?}", other);
            }
        }
    }

    /// Layer 3: Full run_session pipeline with audio bridge + schema validation.
    /// Skips if virtual audio routing is not available (no Vortex / PulseAudio / BlackHole).
    #[tokio::test]
    #[ignore]
    async fn test_live_audio_openai_full_session() {
        let api_key = match require_openai_key() {
            Some(k) => k,
            None => return,
        };

        if !crate::audio_routing::is_available().await {
            eprintln!("virtual audio routing not available, skipping full session test");
            return;
        }

        let session_id = format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        let mut bridge = crate::audio_routing::create_bridge(&session_id)
            .await
            .expect("create_bridge failed");

        if let Err(e) = crate::audio_routing::set_as_default(&mut bridge).await {
            eprintln!("  WARN: could not set bridge as default: {}", e);
        }

        let schema = crate::live_audio_types::ResponseSchema {
            fields: vec![crate::live_audio_types::FieldSpec {
                name: "status".to_string(),
                field_type: crate::live_audio_types::FieldType::String {
                    max_length: Some(100),
                    allowed_values: None,
                    tainted: false,
                },
                required: true,
                description: Some("Test status".to_string()),
            }],
        };

        let system_prompt = crate::prompts::build_live_audio_prompt(
            "You are running an automated test. There is no one on the other end of \
             the call — you will hear silence. Say 'test complete' once, then \
             immediately output the JSON response with status set to 'ok'.",
            &schema,
            None,
        );

        let spec = crate::live_audio_types::LiveAudioSpec {
            id: session_id.clone(),
            provider: crate::live_audio_types::LiveAudioProvider::OpenAI,
            model: Some(TEST_MODEL.to_string()),
            playbook: system_prompt,
            response_schema: schema,
            timeout_secs: 45,
            voice: Some("alloy".to_string()),
            display_id: None,
            // No real counterparty in the test — kick the model off via text
            initial_message: Some("Begin.".to_string()),
        };

        let tmp_dir = tempfile::tempdir().expect("create temp dir");
        eprintln!("  Session ID: {}", session_id);
        eprintln!("  Log dir: {}", tmp_dir.path().display());

        // In-module mint: the live test stands in for an approved dispatch.
        let result = run_session(
            &spec,
            SpawnConsent(()),
            &api_key,
            &bridge,
            tmp_dir.path(),
            None,
            &crate::transcription::TranscriptionConfig::default(),
        )
        .await
        .expect("run_session failed");

        drop(bridge);

        eprintln!("  Status: {:?}", result.status);
        eprintln!("  Duration: {:.1}s", result.duration_secs);
        eprintln!("  Response data: {:?}", result.response_data);
        eprintln!("  Quarantine IDs: {:?}", result.quarantine_ids);

        // Check transcript file was created
        assert!(
            result.transcript_path.exists(),
            "transcript file should exist at {}",
            result.transcript_path.display()
        );

        match &result.status {
            LiveAudioStatus::Completed => {
                let data = result
                    .response_data
                    .as_ref()
                    .expect("Completed but no response_data");
                assert!(
                    data.get("status").is_some(),
                    "response missing 'status' field: {}",
                    data
                );
                eprintln!("  PASS: session completed with valid response");
            }
            LiveAudioStatus::TimedOut => {
                // Acceptable — the model heard silence and may not have produced JSON
                eprintln!("  WARN: session timed out (model did not output JSON within timeout)");
            }
            LiveAudioStatus::SchemaError(e) => {
                // The model produced JSON but it didn't match — still useful signal
                eprintln!("  WARN: schema validation failed: {}", e);
            }
            other => {
                panic!("unexpected session status: {:?}", other);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Always-consent spawn gate tests
    // -----------------------------------------------------------------------

    use crate::event::{AppEvent, ApprovalResponse, ControlMsg, EventBus};

    fn consent_spec() -> LiveAudioSpec {
        serde_json::from_value(serde_json::json!({
            "id": "consent-test",
            "provider": "gemini",
            "playbook": "test playbook",
            "response_schema": { "fields": [] },
        }))
        .expect("valid spec")
    }

    /// Wait for the gate's ApprovalRequired and return its id, asserting the
    /// prompt rides the approval rail with the live-audio category and an id
    /// from the dedicated consent range.
    async fn wait_for_consent_prompt(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>) -> u64 {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("consent prompt within 5s")
                .expect("bus open")
            {
                AppEvent::ApprovalRequired {
                    id,
                    category,
                    command_preview,
                    ..
                } => {
                    assert_eq!(category, crate::autonomy::ActionCategory::LiveAudioSpawn);
                    assert!(
                        id >= SPAWN_CONSENT_ID_BASE,
                        "consent ids come from the dedicated range: {id}"
                    );
                    assert!(
                        command_preview.contains("spawn_live_audio"),
                        "{command_preview}"
                    );
                    return id;
                }
                _ => continue,
            }
        }
    }

    /// Wait for the rail-clearing ApprovalResolved for `id`, returning its action.
    async fn wait_for_consent_resolution(
        rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        id: u64,
    ) -> String {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("resolution within 5s")
                .expect("bus open")
            {
                AppEvent::ApprovalResolved {
                    id: resolved_id,
                    action,
                    ..
                } if resolved_id == id => return action,
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn spawn_consent_denied_via_control_command() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let gate = {
            let bus = bus.clone();
            tokio::spawn(async move {
                request_spawn_consent(
                    SpawnConsentRequest {
                        bus: &bus,
                        approval_registry: None,
                        json_approval: None,
                        no_approver: false,
                        session_id: Some("sess-deny".to_string()),
                        preview: spawn_consent_preview(&consent_spec()),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };

        let id = wait_for_consent_prompt(&mut rx).await;
        assert!(spawn_consent_pending(id), "prompt registers as pending");
        bus.send(AppEvent::ControlCommand(ControlMsg::Deny {
            session_id: None,
            id,
        }));

        let result = tokio::time::timeout(Duration::from_secs(5), gate)
            .await
            .expect("gate returns")
            .expect("gate task");
        let denied = result.expect_err("deny fails the gate");
        assert!(denied.contains("declined"), "{denied}");
        assert!(!spawn_consent_pending(id), "pending entry cleared");
        assert_eq!(wait_for_consent_resolution(&mut rx, id).await, "deny");
    }

    #[tokio::test]
    async fn spawn_consent_approved_via_control_command() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let gate = {
            let bus = bus.clone();
            tokio::spawn(async move {
                request_spawn_consent(
                    SpawnConsentRequest {
                        bus: &bus,
                        approval_registry: None,
                        json_approval: None,
                        no_approver: false,
                        session_id: None,
                        preview: "spawn_live_audio (Gemini, id: t)".to_string(),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };

        let id = wait_for_consent_prompt(&mut rx).await;
        bus.send(AppEvent::ControlCommand(ControlMsg::Approve {
            session_id: None,
            id,
        }));

        let result = tokio::time::timeout(Duration::from_secs(5), gate)
            .await
            .expect("gate returns")
            .expect("gate task");
        assert!(result.is_ok(), "approve passes the gate: {result:?}");
        assert_eq!(wait_for_consent_resolution(&mut rx, id).await, "approve");
    }

    #[tokio::test]
    async fn spawn_consent_approved_via_registry_responder() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let registry = crate::event::ApprovalRegistry::default();
        let gate = {
            let bus = bus.clone();
            let registry = registry.clone();
            tokio::spawn(async move {
                request_spawn_consent(
                    SpawnConsentRequest {
                        bus: &bus,
                        approval_registry: Some(&registry),
                        json_approval: None,
                        no_approver: false,
                        session_id: None,
                        preview: "spawn_live_audio (OpenAI, id: t)".to_string(),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };

        let id = wait_for_consent_prompt(&mut rx).await;
        // A direct resolver (the MCP approve tool) pops the responder.
        let responder = registry
            .lock()
            .unwrap()
            .remove(&id)
            .expect("gate armed a registry responder");
        responder
            .send(ApprovalResponse::Approve)
            .expect("gate is listening");

        let result = tokio::time::timeout(Duration::from_secs(5), gate)
            .await
            .expect("gate returns")
            .expect("gate task");
        assert!(
            result.is_ok(),
            "registry approve passes the gate: {result:?}"
        );
        assert_eq!(wait_for_consent_resolution(&mut rx, id).await, "approve");
        assert!(!spawn_consent_pending(id));
    }

    #[tokio::test]
    async fn spawn_consent_times_out_fail_closed() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let registry = crate::event::ApprovalRegistry::default();
        let result = request_spawn_consent(
            SpawnConsentRequest {
                bus: &bus,
                approval_registry: Some(&registry),
                json_approval: None,
                no_approver: false,
                session_id: None,
                preview: "spawn_live_audio (Gemini, id: t)".to_string(),
            },
            Duration::from_millis(100),
        )
        .await;
        let denied = result.expect_err("timeout fails closed");
        assert!(denied.contains("no approval arrived"), "{denied}");

        let id = wait_for_consent_prompt(&mut rx).await;
        assert_eq!(wait_for_consent_resolution(&mut rx, id).await, "timeout");
        assert!(
            registry.lock().unwrap().is_empty(),
            "stale registry responder cleaned up"
        );
    }

    #[tokio::test]
    async fn spawn_consent_no_approver_fails_closed_without_prompt() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let err = request_spawn_consent(
            SpawnConsentRequest {
                bus: &bus,
                approval_registry: None,
                json_approval: None,
                no_approver: true,
                session_id: None,
                preview: "spawn_live_audio (Gemini, id: t)".to_string(),
            },
            Duration::from_secs(5),
        )
        .await
        .expect_err("fails closed with nobody to ask");
        assert_eq!(err, SPAWN_CONSENT_NO_APPROVER);
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, AppEvent::ApprovalRequired { .. }),
                "no prompt is raised when nobody can answer"
            );
        }
    }

    #[tokio::test]
    async fn spawn_consent_json_slot_approve_and_deny() {
        let bus = EventBus::new();
        let slot = crate::new_json_approval_slot();

        // Approve leg: the stdin loop answers over the armed slot.
        let gate = {
            let bus = bus.clone();
            let slot = slot.clone();
            tokio::spawn(async move {
                request_spawn_consent(
                    SpawnConsentRequest {
                        bus: &bus,
                        approval_registry: None,
                        json_approval: Some(&slot),
                        no_approver: false,
                        session_id: None,
                        preview: "spawn_live_audio (Gemini, id: t)".to_string(),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };
        let (_id, responder) = loop {
            if let Some(armed) = slot.lock().unwrap().take() {
                break armed;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        responder
            .send(ApprovalResponse::Approve)
            .expect("gate is listening");
        let result = tokio::time::timeout(Duration::from_secs(5), gate)
            .await
            .expect("gate returns")
            .expect("gate task");
        assert!(result.is_ok(), "json-slot approve passes: {result:?}");

        // Deny leg.
        let gate = {
            let bus = bus.clone();
            let slot = slot.clone();
            tokio::spawn(async move {
                request_spawn_consent(
                    SpawnConsentRequest {
                        bus: &bus,
                        approval_registry: None,
                        json_approval: Some(&slot),
                        no_approver: false,
                        session_id: None,
                        preview: "spawn_live_audio (Gemini, id: t)".to_string(),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };
        let (_id, responder) = loop {
            if let Some(armed) = slot.lock().unwrap().take() {
                break armed;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        responder
            .send(ApprovalResponse::Deny)
            .expect("gate is listening");
        let result = tokio::time::timeout(Duration::from_secs(5), gate)
            .await
            .expect("gate returns")
            .expect("gate task");
        let denied = result.expect_err("json-slot deny fails the gate");
        assert!(denied.contains("declined"), "{denied}");
    }

    #[tokio::test]
    async fn spawn_consent_approve_all_grants_this_prompt_only() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        // ApproveAll resolves this prompt like Approve...
        let gate = {
            let bus = bus.clone();
            tokio::spawn(async move {
                request_spawn_consent(
                    SpawnConsentRequest {
                        bus: &bus,
                        approval_registry: None,
                        json_approval: None,
                        no_approver: false,
                        session_id: None,
                        preview: "spawn_live_audio (Gemini, id: t)".to_string(),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };
        let first_id = wait_for_consent_prompt(&mut rx).await;
        bus.send(AppEvent::ControlCommand(ControlMsg::ApproveAll {
            session_id: None,
            id: first_id,
        }));
        let result = tokio::time::timeout(Duration::from_secs(5), gate)
            .await
            .expect("gate returns")
            .expect("gate task");
        assert!(result.is_ok(), "approve-all passes this prompt: {result:?}");

        // ...but the next spawn still asks: the gate holds no grant state
        // (LiveAudioSpawn is always-ask by policy).
        let gate = {
            let bus = bus.clone();
            tokio::spawn(async move {
                request_spawn_consent(
                    SpawnConsentRequest {
                        bus: &bus,
                        approval_registry: None,
                        json_approval: None,
                        no_approver: false,
                        session_id: None,
                        preview: "spawn_live_audio (Gemini, id: t)".to_string(),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };
        let second_id = wait_for_consent_prompt(&mut rx).await;
        assert_ne!(second_id, first_id, "every spawn prompts afresh");
        bus.send(AppEvent::ControlCommand(ControlMsg::Deny {
            session_id: None,
            id: second_id,
        }));
        let result = tokio::time::timeout(Duration::from_secs(5), gate)
            .await
            .expect("gate returns")
            .expect("gate task");
        assert!(result.is_err(), "second prompt still required a decision");
    }
}
