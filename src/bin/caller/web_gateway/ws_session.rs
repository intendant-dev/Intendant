//! Local `/ws` session tasks, extracted from `spawn_web_gateway`'s
//! per-connection body: the outbound writer (broadcast + direct responses +
//! personalized input-authority frames -> the WebSocket) and, in later
//! slices, the inbound frame-dispatch reader.

use super::*;

/// Outbound half of a local `/ws` session: broadcast + direct responses ->
/// the WebSocket, converting each input-authority change into a personalized
/// `display_input_authority_state` wire message. Connection IDs never leave
/// the daemon -- only the resolved `you|other|unclaimed` state does.
pub(crate) async fn ws_outbound_task(
    mut outbound_rx: broadcast::Receiver<String>,
    mut direct_rx: mpsc::UnboundedReceiver<String>,
    mut ws_tx: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<DemuxStream>,
        Message,
    >,
    mut authority_change_rx: broadcast::Receiver<DisplayInputAuthorityChange>,
    connection_id: String,
    display_input_authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    session_registry: Option<crate::display::SharedSessionRegistry>,
) {
    loop {
        tokio::select! {
            msg = outbound_rx.recv() => {
                match msg {
                    Ok(line) => {
                        if ws_tx
                            .send(Message::Text(line.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            msg = direct_rx.recv() => {
                match msg {
                    Some(line) => {
                        if ws_tx
                            .send(Message::Text(line.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                }
            }
            msg = authority_change_rx.recv() => {
                match msg {
                    Ok(change) => {
                        // Personalize: never ship the holder's identity.
                        let state = match &change.holder {
                            Some(h) if h.matches_local_ws(&connection_id) => "you",
                            Some(_) => "other",
                            None => "unclaimed",
                        };
                        let frame = serde_json::json!({
                            "t": "display_input_authority_state",
                            "display_id": change.display_id,
                            "state": state,
                        }).to_string();
                        if ws_tx
                            .send(Message::Text(frame.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Phase 5a.1: a lagged subscriber missed at least one
                        // authority transition.  Send a fresh personalized
                        // snapshot for every currently-active display so the
                        // browser's chip cannot be left stuck on stale state.
                        // Snapshot is computed under the std lock (held briefly,
                        // released before any send) plus the session registry's
                        // tokio lock for the active-display list — order
                        // matters: take the std lock LAST and drop it before
                        // awaiting the send to avoid awaiting under a sync guard.
                                        // `all_display_ids`: authority chips are a
                        // dashboard surface — private user views hold
                        // input authority like any other display.
                        let display_ids: Vec<u32> = match session_registry.as_ref() {
                            Some(sr) => sr.read().await.all_display_ids(),
                            None => Vec::new(),
                        };
                        let snapshots: Vec<(u32, &'static str)> = {
                            let auth = display_input_authority
                                .read()
                                .unwrap_or_else(|e| e.into_inner());
                            display_ids.into_iter().map(|did| {
                                let state = match auth.get(&did) {
                                    Some(entry) if entry.matches_local_ws(&connection_id) => "you",
                                    Some(_) => "other",
                                    None => "unclaimed",
                                };
                                (did, state)
                            }).collect()
                        };
                        let mut send_failed = false;
                        for (did, state) in snapshots {
                            let frame = serde_json::json!({
                                "t": "display_input_authority_state",
                                "display_id": did,
                                "state": state,
                            }).to_string();
                            if ws_tx
                                .send(Message::Text(frame.into()))
                                .await
                                .is_err()
                            {
                                send_failed = true;
                                break;
                            }
                        }
                        if send_failed { break; }
                    }
                }
            }
        }
    }
}

/// Shared handles the inbound frame-dispatch task needs, cloned once per
/// `/ws` connection at the spawn site.
pub(crate) struct WsInboundCtx {
    pub(crate) bus: EventBus,
    pub(crate) query_ctx: Option<WebQueryCtx>,
    pub(crate) direct_tx: mpsc::UnboundedSender<String>,
    pub(crate) voice_debug: Arc<Mutex<VoiceDebugState>>,
    pub(crate) live_provider: String,
    pub(crate) live_model: String,
    pub(crate) transcriber: Option<Arc<dyn crate::transcription::Transcriber>>,
    pub(crate) active_presence: Arc<Mutex<Option<ActivePresence>>>,
    pub(crate) display_input_authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    pub(crate) authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    pub(crate) federated_authority_subscribers: FederatedAuthoritySubscribers,
    pub(crate) connection_id: String,
    pub(crate) frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    pub(crate) recording_registry:
        Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    pub(crate) session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    pub(crate) session_registry: Option<crate::display::SharedSessionRegistry>,
    pub(crate) task_tx: Option<tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>>,
    pub(crate) terminal_registry: Arc<crate::terminal::TerminalRegistry>,
    pub(crate) dashboard_control: Arc<crate::dashboard_control::DashboardControlRegistry>,
    pub(crate) dashboard_control_grant: crate::dashboard_control::DashboardControlGrant,
    pub(crate) peer_file_transfer_registry:
        Arc<crate::peer_file_transfer::PeerFileTransferRegistry>,
    pub(crate) peer_identity: Option<PeerConnectionIdentity>,
    pub(crate) browser_host_ip: Option<std::net::IpAddr>,
    pub(crate) ice_config: crate::display::IceConfig,
    pub(crate) tcp_advertised_port: Option<u16>,
    pub(crate) tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
}

/// Inbound half of a local `/ws` session: WebSocket -> EventBus frame
/// dispatch (presence, voice, tool requests, control commands, display
/// signaling, input-authority claims, dashboard-control tunneling, file
/// transfer). Extracted verbatim from `spawn_web_gateway`'s per-connection
/// body; the destructuring below maps the context onto the `_inbound`
/// locals the body was written against.
pub(crate) async fn ws_inbound_task(
    ctx: WsInboundCtx,
    mut ws_rx: futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<DemuxStream>>,
    peer_id: u64,
) {
    let WsInboundCtx {
        bus: bus_inbound,
        query_ctx: query_ctx_inbound,
        direct_tx: direct_tx_inbound,
        voice_debug: voice_debug_inbound,
        live_provider,
        live_model,
        transcriber: transcriber_inbound,
        active_presence: active_presence_inbound,
        display_input_authority: display_input_authority_inbound,
        authority_change_tx: authority_change_tx_inbound,
        federated_authority_subscribers: federated_authority_subscribers_inbound,
        connection_id: connection_id_inbound,
        frame_registry: frame_registry_inbound,
        recording_registry: recording_registry_inbound,
        session_log: session_log_inbound,
        session_registry: session_registry_inbound,
        task_tx: task_tx_inbound,
        terminal_registry: terminal_registry_inbound,
        dashboard_control: dashboard_control_inbound,
        dashboard_control_grant: dashboard_control_grant_inbound,
        peer_file_transfer_registry: peer_file_transfer_registry_inbound,
        peer_identity: peer_identity_inbound,
        browser_host_ip,
        ice_config,
        tcp_advertised_port,
        tcp_peer_registry,
    } = ctx;
    // Track whether this connection has an active presence model,
    // so we can auto-send PresenceDisconnected if the WebSocket drops
    // without a clean presence_disconnect message (e.g. tab close
    // before beforeunload fires, network failure).
    let mut is_presence_connected = false;
    // Whether this connection is the active voice owner
    let mut is_active = false;

    // Per-connection clip accumulators for batched clip_frame messages
    // Per-connection accumulators, in the same clip-operation shape the
    // tunnel's media_clip_ops map stores (web_gateway::media_store).
    let mut clip_accumulators: std::collections::HashMap<String, DashboardMediaClipOperation> =
        std::collections::HashMap::new();

    // Display IDs this peer has WebRTC connections to,
    // used for cleanup when the WebSocket disconnects.
    let mut peer_display_ids: Vec<u32> = Vec::new();
    let mut dashboard_control_session_ids: Vec<String> = Vec::new();

    // Frame types already denied+logged once on this
    // connection — dedupes the warn log only; the denial
    // frame itself is sent for every rejected frame.
    let mut ws_denied_logged: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Shell-session lane for this connection: root sees
    // every session, scoped principals see owned/shared.
    let ws_terminal_actor = dashboard_control_grant_inbound.terminal_actor();

    // Per-connection audio transcription buffer.
    // PCM16 bytes are accumulated and drained every ~3s.
    let mut audio_buf: Vec<u8> = Vec::new();
    let mut audio_seq: u64 = 0;
    // Input sample rate (known from config, default 16kHz)
    let audio_sample_rate: u32 = 16000;

    while let Some(Ok(msg)) = ws_rx.next().await {
        if let Message::Text(text) = msg {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Try to parse as JSON for type-tagged messages
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                // Per-frame IAM enforcement on the direct
                // /ws path — the same frame→operation
                // table the dashboard-control tunnel
                // enforces, so a scoped grant means the
                // same thing on every transport.
                if deny_ws_frame_if_unauthorized(
                    &dashboard_control_grant_inbound,
                    &json,
                    &direct_tx_inbound,
                    &bus_inbound,
                    &mut ws_denied_logged,
                ) {
                    continue;
                }
                match json.get("t").and_then(|v| v.as_str()) {
                    Some("presence_connect") => {
                        is_presence_connected = true;
                        voice_debug_inbound
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .connected = true;
                        let server_session_id = json
                            .get("server_session_id")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        let last_event_seq = json
                            .get("last_event_seq")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        // Use provider/model from the browser if sent,
                        // fall back to config defaults.
                        let msg_provider = json
                            .get("provider")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .unwrap_or_else(|| live_provider.clone());
                        let msg_model = json
                            .get("model")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .unwrap_or_else(|| live_model.clone());

                        // Determine if this connection becomes active or passive.
                        // Browsers can request always-passive mode (observer/follow-along).
                        let force_passive = json
                            .get("passive")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let becomes_active = if force_passive {
                            false
                        } else {
                            let slot = active_presence_inbound
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            // Empty slot → first connect wins.
                            // Slot occupied by THIS connection → already active
                            // (happens when active browser reconnects voice after handover).
                            slot.is_none()
                                || slot
                                    .as_ref()
                                    .map(|a| a.connection_id == connection_id_inbound)
                                    .unwrap_or(false)
                        };

                        let was_already_active = is_active;
                        if becomes_active {
                            // First-connect wins (or re-confirm already-active)
                            *active_presence_inbound
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = Some(ActivePresence {
                                connection_id: connection_id_inbound.clone(),
                                direct_tx: direct_tx_inbound.clone(),
                            });
                            is_active = true;
                        }

                        // Send welcome with replay window if presence session is available
                        if let Some(ref ctx) = query_ctx_inbound {
                            // Build conversation context from recent voice transcripts
                            let conversation_ctx =
                                presence::build_conversation_context(&ctx.log_dir, 20);

                            if let Some(ref ps) = ctx.presence_session {
                                let mut session = ps.lock().unwrap_or_else(|e| e.into_inner());
                                if becomes_active {
                                    session.set_connected(true);
                                }
                                let state = ctx
                                    .agent_state
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone();
                                let welcome = session.build_welcome(last_event_seq, &state);
                                let welcome_msg = serde_json::json!({
                                    "t": "presence_welcome",
                                    "session_id": welcome.session_id,
                                    "state": welcome.state,
                                    "events": welcome.events,
                                    "last_checkpoint_summary": welcome.last_checkpoint_summary,
                                    "current_seq": welcome.current_seq,
                                    "is_active": becomes_active,
                                    "conversation_context": conversation_ctx,
                                });
                                let _ = direct_tx_inbound.send(welcome_msg.to_string());
                            } else {
                                let welcome_msg = serde_json::json!({
                                    "t": "presence_welcome",
                                    "is_active": becomes_active,
                                    "conversation_context": conversation_ctx,
                                });
                                let _ = direct_tx_inbound.send(welcome_msg.to_string());
                            }
                        } else {
                            // No presence session — still send a minimal welcome with is_active
                            let welcome_msg = serde_json::json!({
                                "t": "presence_welcome",
                                "is_active": becomes_active,
                            });
                            let _ = direct_tx_inbound.send(welcome_msg.to_string());
                        }

                        // Only emit PresenceConnected for the active browser
                        // (passive browsers don't pause server-side presence).
                        // Skip if already active (e.g. voice reconnect after make_active
                        // handover — PresenceConnected was already emitted by make_active).
                        if becomes_active && !was_already_active {
                            if let Some(ref sl) = session_log_inbound {
                                if let Ok(mut l) = sl.lock() {
                                    l.presence_connected(Some(&msg_provider), Some(&msg_model));
                                }
                            }
                            bus_inbound.send(AppEvent::PresenceConnected {
                                server_session_id,
                                last_event_seq,
                                live_provider: Some(msg_provider),
                                live_model: Some(msg_model),
                            });
                        }
                    }
                    Some("presence_disconnect") => {
                        is_presence_connected = false;
                        voice_debug_inbound
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .connected = false;
                        if let Some(ref ctx) = query_ctx_inbound {
                            if let Some(ref ps) = ctx.presence_session {
                                ps.lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .set_connected(false);
                            }
                        }
                        // Only emit PresenceDisconnected if this was the active browser
                        if is_active {
                            // Clear the active slot
                            let mut slot = active_presence_inbound
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if slot
                                .as_ref()
                                .map(|a| a.connection_id == connection_id_inbound)
                                .unwrap_or(false)
                            {
                                *slot = None;
                            }
                            is_active = false;
                            if let Some(ref sl) = session_log_inbound {
                                if let Ok(mut l) = sl.lock() {
                                    l.presence_disconnected();
                                }
                            }
                            bus_inbound.send(AppEvent::PresenceDisconnected);
                        }
                    }
                    Some("make_active") => {
                        // Request to become the active voice owner
                        let mut slot = active_presence_inbound
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let previous_active =
                            slot.as_ref().map(|active| active.connection_id.clone());
                        if let Some(ref sl) = session_log_inbound {
                            if let Ok(mut l) = sl.lock() {
                                l.voice_diagnostic(
                                    "make_active_received_gateway",
                                    &format!(
                                        "request from connection={} previous_active={}",
                                        connection_id_inbound,
                                        previous_active.as_deref().unwrap_or("none"),
                                    ),
                                );
                            }
                        }

                        // Tell old active to disconnect voice
                        if let Some(ref old) = *slot {
                            if old.connection_id != connection_id_inbound {
                                let force_msg = serde_json::json!({
                                    "t": "force_disconnect_voice",
                                    "reason": "handover",
                                });
                                let _ = old.direct_tx.send(force_msg.to_string());
                                if let Some(ref sl) = session_log_inbound {
                                    if let Ok(mut l) = sl.lock() {
                                        l.voice_diagnostic(
                                            "make_active_force_disconnect_gateway",
                                            &format!(
                                                "old_active={} new_active={}",
                                                old.connection_id, connection_id_inbound,
                                            ),
                                        );
                                    }
                                }
                            } else if let Some(ref sl) = session_log_inbound {
                                if let Ok(mut l) = sl.lock() {
                                    l.voice_diagnostic(
                                        "make_active_noop_gateway",
                                        &format!(
                                            "request from already-active connection={}",
                                            connection_id_inbound,
                                        ),
                                    );
                                }
                            }
                        }

                        // Install this connection as new active
                        *slot = Some(ActivePresence {
                            connection_id: connection_id_inbound.clone(),
                            direct_tx: direct_tx_inbound.clone(),
                        });
                        drop(slot);

                        is_active = true;
                        is_presence_connected = true;
                        voice_debug_inbound
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .connected = true;

                        // Build handover context from latest checkpoint
                        let handover_context = query_ctx_inbound
                            .as_ref()
                            .and_then(|ctx| ctx.presence_session.as_ref())
                            .and_then(|ps| {
                                let session = ps.lock().unwrap_or_else(|e| e.into_inner());
                                session.last_checkpoint_summary()
                            })
                            .unwrap_or_default();

                        // Build conversation context from recent voice transcripts
                        let conversation_ctx = query_ctx_inbound
                            .as_ref()
                            .and_then(|ctx| presence::build_conversation_context(&ctx.log_dir, 20));
                        let has_handover_context = !handover_context.is_empty();
                        let has_conversation_context = conversation_ctx
                            .as_deref()
                            .map(|s| !s.is_empty())
                            .unwrap_or(false);

                        // Send active_granted to this connection
                        let granted_msg = serde_json::json!({
                            "t": "active_granted",
                            "is_active": true,
                            "handover_context": handover_context,
                            "conversation_context": conversation_ctx,
                        });
                        let _ = direct_tx_inbound.send(granted_msg.to_string());
                        if let Some(ref sl) = session_log_inbound {
                            if let Ok(mut l) = sl.lock() {
                                l.voice_diagnostic(
                                    "make_active_granted_gateway",
                                    &format!(
                                        "connection={} handover_context={} conversation_context={}",
                                        connection_id_inbound,
                                        if has_handover_context { "yes" } else { "no" },
                                        if has_conversation_context {
                                            "yes"
                                        } else {
                                            "no"
                                        },
                                    ),
                                );
                            }
                        }

                        // Emit PresenceConnected for the new active browser
                        if let Some(ref sl) = session_log_inbound {
                            if let Ok(mut l) = sl.lock() {
                                l.presence_connected(Some(&live_provider), Some(&live_model));
                            }
                        }
                        bus_inbound.send(AppEvent::PresenceConnected {
                            server_session_id: None,
                            last_event_seq: 0,
                            live_provider: Some(live_provider.clone()),
                            live_model: Some(live_model.clone()),
                        });
                    }
                    Some("voice_log") => {
                        let text = json["text"].as_str().unwrap_or("").to_string();
                        let seq = json["seq"].as_u64().unwrap_or(0);
                        let tool_context = json
                            .get("tool_context")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        {
                            let mut vd = voice_debug_inbound
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            vd.voice_log_count += 1;
                            vd.last_voice_log = text.clone();
                        }
                        if let Some(ref sl) = session_log_inbound {
                            if let Ok(mut l) = sl.lock() {
                                l.voice_log(&text, seq, tool_context.as_deref());
                            }
                        }
                        bus_inbound.send(AppEvent::VoiceLog {
                            text,
                            seq,
                            tool_context,
                        });
                    }
                    Some("live_usage_update") => {
                        bus_inbound.send(AppEvent::LiveUsageUpdate {
                            provider: json["provider"].as_str().unwrap_or("").to_string(),
                            model: json["model"].as_str().unwrap_or("").to_string(),
                            input_tokens: json["input_tokens"].as_u64().unwrap_or(0),
                            output_tokens: json["output_tokens"].as_u64().unwrap_or(0),
                            cached_tokens: json["cached_tokens"].as_u64().unwrap_or(0),
                            total_tokens: json["total_tokens"].as_u64().unwrap_or(0),
                            thinking_tokens: json["thinking_tokens"].as_u64().unwrap_or(0),
                            input_text_tokens: json["input_text_tokens"].as_u64().unwrap_or(0),
                            input_audio_tokens: json["input_audio_tokens"].as_u64().unwrap_or(0),
                            input_image_tokens: json["input_image_tokens"].as_u64().unwrap_or(0),
                            cached_text_tokens: json["cached_text_tokens"].as_u64().unwrap_or(0),
                            cached_audio_tokens: json["cached_audio_tokens"].as_u64().unwrap_or(0),
                            cached_image_tokens: json["cached_image_tokens"].as_u64().unwrap_or(0),
                            output_text_tokens: json["output_text_tokens"].as_u64().unwrap_or(0),
                            output_audio_tokens: json["output_audio_tokens"].as_u64().unwrap_or(0),
                        });
                    }
                    Some("presence_checkpoint") => {
                        let summary = json["summary"].as_str().unwrap_or("").to_string();
                        let last_event_seq = json
                            .get("last_event_seq")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);

                        // Record checkpoint and send ack
                        if let Some(ref ctx) = query_ctx_inbound {
                            if let Some(ref ps) = ctx.presence_session {
                                let checkpoint = presence_core::PresenceCheckpoint {
                                    summary: summary.clone(),
                                    last_event_seq,
                                };
                                let ack = ps
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .record_checkpoint(checkpoint);
                                let ack_msg = serde_json::json!({
                                    "t": "presence_checkpoint_ack",
                                    "seq": ack.seq,
                                });
                                let _ = direct_tx_inbound.send(ack_msg.to_string());
                            }
                        }

                        if let Some(ref sl) = session_log_inbound {
                            if let Ok(mut l) = sl.lock() {
                                l.presence_checkpoint(&summary, last_event_seq);
                            }
                        }
                        bus_inbound.send(AppEvent::PresenceCheckpointReceived {
                            summary,
                            last_event_seq,
                        });
                    }
                    Some("voice_diagnostic") => {
                        let kind = json["kind"].as_str().unwrap_or("unknown").to_string();
                        let detail = json["detail"].as_str().unwrap_or("").to_string();
                        if let Some(ref sl) = session_log_inbound {
                            if let Ok(mut l) = sl.lock() {
                                l.voice_diagnostic(&kind, &detail);
                            }
                        }
                        bus_inbound.send(AppEvent::VoiceDiagnostic { kind, detail });
                    }
                    Some("user_audio") => {
                        // Browser sends base64-encoded PCM16 audio for server-side transcription.
                        if let Some(ref transcriber) = transcriber_inbound {
                            if let Some(data_b64) = json["data"].as_str() {
                                use base64::Engine;
                                if let Ok(pcm_bytes) =
                                    base64::engine::general_purpose::STANDARD.decode(data_b64)
                                {
                                    audio_buf.extend_from_slice(&pcm_bytes);
                                    // Drain at ~3s of audio (16kHz * 2 bytes/sample * 1 channel * 3s = 96000)
                                    let threshold = (audio_sample_rate as usize) * 2 * 3;
                                    if audio_buf.len() >= threshold {
                                        // Skip silent buffers — compute RMS energy of PCM16 samples.
                                        // Whisper hallucinates on silence (outputs "you", ".", etc).
                                        let rms = {
                                            let samples = audio_buf
                                                .chunks_exact(2)
                                                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64);
                                            let sum_sq: f64 = samples.map(|s| s * s).sum();
                                            let n = audio_buf.len() / 2;
                                            if n > 0 {
                                                (sum_sq / n as f64).sqrt()
                                            } else {
                                                0.0
                                            }
                                        };
                                        if rms < 1000.0 {
                                            // Below speech threshold — skip transcription.
                                            // Whisper hallucinates aggressively on low-energy
                                            // audio ("Thank you", "Bye bye", etc).
                                            audio_buf.clear();
                                            continue;
                                        }
                                        let wav = crate::transcription::encode_wav(
                                            &audio_buf,
                                            audio_sample_rate,
                                            1,
                                        );
                                        audio_buf.clear();
                                        audio_seq += 1;
                                        let seq = audio_seq;
                                        let t = transcriber.clone();
                                        let bus_tx = bus_inbound.clone();
                                        let session_log_tx = session_log_inbound.clone();
                                        tokio::spawn(async move {
                                            match t.transcribe(&wav).await {
                                                Ok(segment) => {
                                                    let text = segment.text.trim().to_string();
                                                    if !text.is_empty() {
                                                        if let Some(ref sl) = session_log_tx {
                                                            if let Ok(mut l) = sl.lock() {
                                                                l.user_transcript(&text, seq);
                                                            }
                                                        }
                                                        bus_tx.send(AppEvent::UserTranscript {
                                                            text,
                                                            seq,
                                                        });
                                                    }
                                                }
                                                Err(e) => {
                                                    eprintln!("transcription failed: {}", e);
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Some("video_frame") => {
                        // Browser sends a video frame for HQ archival in the
                        // frame registry plus the recording pipeline
                        // (auto-starts on first frame) — the same store fn
                        // the tunnel's api_presence_video_frame commits
                        // through (fire-and-forget: no response frame).
                        let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                        let stream = json["stream"].as_str().unwrap_or("cam0").to_string();
                        if let Some(data_b64) = json["data"].as_str() {
                            use base64::Engine;
                            if let Ok(jpeg_bytes) =
                                base64::engine::general_purpose::STANDARD.decode(data_b64)
                            {
                                let _ = register_presence_video_frame(
                                    frame_registry_inbound.clone(),
                                    recording_registry_inbound.clone(),
                                    &bus_inbound,
                                    &frame_id,
                                    &stream,
                                    &jpeg_bytes,
                                )
                                .await;
                            }
                        }
                    }
                    Some("annotation_attach") => {
                        // User clicked "Attach" on an annotation/frame: register
                        // the JPEG in the frame registry but DO NOT inject into
                        // the agent context. The browser tracks this frame ID as
                        // a pending attachment and submits it with the next task.
                        //
                        // Works regardless of presence/agent state — attachments
                        // are independent of any running task. Same store fn as
                        // the tunnel's api_media_annotation_attach.
                        let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                        let stream = json["stream"].as_str().unwrap_or("annotation").to_string();
                        let note = json["note"].as_str().unwrap_or("").to_string();
                        if let Some(data_b64) = json["data"].as_str() {
                            use base64::Engine;
                            if let Ok(jpeg_bytes) =
                                base64::engine::general_purpose::STANDARD.decode(data_b64)
                            {
                                let (saved_path, registered) = register_dashboard_media_frame(
                                    frame_registry_inbound.clone(),
                                    &frame_id,
                                    &stream,
                                    if note.is_empty() {
                                        None
                                    } else {
                                        Some(note.clone())
                                    },
                                    &jpeg_bytes,
                                    "annotation_attach",
                                )
                                .await;
                                let _ = direct_tx_inbound.send(
                                    serde_json::json!({
                                        "t": "annotation_attached",
                                        "frame_id": frame_id,
                                        "stream": stream,
                                        "path": saved_path,
                                        "note": note,
                                        "ok": registered,
                                    })
                                    .to_string(),
                                );
                                bus_inbound.send(AppEvent::PresenceLog {
                                    message: format!(
                                        "[annotation] {} attached (pending)",
                                        frame_id
                                    ),
                                    level: Some(LogLevel::Info),
                                    turn: None,
                                });
                            }
                        }
                    }
                    Some("annotation_submit") => {
                        // User drew annotations on a frame and submitted it
                        // with a note — register + optional context injection
                        // through the same store fns as the tunnel's
                        // api_media_annotation_submit.
                        let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                        let stream = json["stream"].as_str().unwrap_or("annotation").to_string();
                        let note = json["note"].as_str().unwrap_or("").to_string();
                        let inject = json["inject"].as_bool().unwrap_or(false);
                        if let Some(data_b64) = json["data"].as_str() {
                            use base64::Engine;
                            if let Ok(jpeg_bytes) =
                                base64::engine::general_purpose::STANDARD.decode(data_b64)
                            {
                                let (saved_path, _registered) = register_dashboard_media_frame(
                                    frame_registry_inbound.clone(),
                                    &frame_id,
                                    &stream,
                                    if note.is_empty() {
                                        None
                                    } else {
                                        Some(note.clone())
                                    },
                                    &jpeg_bytes,
                                    "annotation",
                                )
                                .await;
                                let injected_to_queue = inject
                                    && inject_annotation_context(
                                        query_ctx_inbound.as_ref(),
                                        &note,
                                        data_b64.to_string(),
                                    );
                                // Send path back to browser. Report whether the injection
                                // actually landed in the queue (not just whether the user
                                // pressed Send), so the UI doesn't lie when no presence is
                                // running.
                                let _ = direct_tx_inbound.send(
                                    serde_json::json!({
                                        "t": "annotation_saved",
                                        "frame_id": frame_id,
                                        "path": saved_path,
                                        "injected": injected_to_queue,
                                    })
                                    .to_string(),
                                );
                                let status_label = if inject {
                                    if injected_to_queue {
                                        " (sent to agent)"
                                    } else {
                                        " (saved — no agent connected)"
                                    }
                                } else {
                                    ""
                                };
                                bus_inbound.send(AppEvent::PresenceLog {
                                    message: format!(
                                        "[annotation] {} on {}{}",
                                        frame_id, stream, status_label
                                    ),
                                    level: Some(LogLevel::Info),
                                    turn: None,
                                });
                            }
                        }
                    }
                    Some("clip_start") => {
                        let clip_id = json["clip_id"].as_str().unwrap_or("").to_string();
                        let stream = json["stream"].as_str().unwrap_or("recording").to_string();
                        let note = json["note"].as_str().unwrap_or("").to_string();
                        let inject = json["inject"].as_bool().unwrap_or(false);
                        let in_secs = json["in_secs"].as_f64().unwrap_or(0.0);
                        let out_secs = json["out_secs"].as_f64().unwrap_or(0.0);
                        let fps = json["fps"].as_u64().unwrap_or(2) as u32;
                        let total = json["total_frames"].as_u64().unwrap_or(0) as usize;
                        clip_accumulators.insert(
                            clip_id.clone(),
                            DashboardMediaClipOperation {
                                stream,
                                note,
                                inject,
                                in_secs,
                                out_secs,
                                fps,
                                expected_frames: total,
                                frames: Vec::with_capacity(total),
                            },
                        );
                        bus_inbound.send(AppEvent::PresenceLog {
                            message: format!(
                                "[clip] started {} ({} frames, {}fps)",
                                clip_id, total, fps
                            ),
                            level: Some(LogLevel::Debug),
                            turn: None,
                        });
                    }
                    Some("clip_frame") => {
                        let clip_id = json["clip_id"].as_str().unwrap_or("").to_string();
                        let frame_id = json["frame_id"].as_str().unwrap_or("").to_string();
                        if let Some(data_b64) = json["data"].as_str() {
                            // Register frame in frame registry — the same
                            // store fn as the tunnel's api_media_clip_frame.
                            use base64::Engine;
                            if let Ok(jpeg_bytes) =
                                base64::engine::general_purpose::STANDARD.decode(data_b64)
                            {
                                let _ = register_dashboard_media_frame(
                                    frame_registry_inbound.clone(),
                                    &frame_id,
                                    &format!("clip:{}", clip_id),
                                    None,
                                    &jpeg_bytes,
                                    "clip",
                                )
                                .await;
                            }
                            // Accumulate for context injection
                            if let Some(acc) = clip_accumulators.get_mut(&clip_id) {
                                acc.frames.push((frame_id, data_b64.to_string()));
                            }
                        }
                    }
                    Some("clip_end") => {
                        let clip_id = json["clip_id"].as_str().unwrap_or("").to_string();

                        if let Some(acc) = clip_accumulators.remove(&clip_id) {
                            let frames_registered = acc.frames.len();
                            // Optional context injection through the same
                            // store fn as the tunnel's api_media_clip_end.
                            let injected = acc.inject
                                && inject_clip_context(
                                    query_ctx_inbound.as_ref(),
                                    &clip_id,
                                    &acc,
                                );

                            let _ = direct_tx_inbound.send(
                                serde_json::json!({
                                    "t": "clip_saved",
                                    "clip_id": clip_id,
                                    "frames_registered": frames_registered,
                                    "injected": injected,
                                })
                                .to_string(),
                            );

                            bus_inbound.send(AppEvent::PresenceLog {
                                message: format!(
                                    "[clip] {} — {} frames{}",
                                    clip_id,
                                    frames_registered,
                                    if injected {
                                        " (sent to agent)"
                                    } else {
                                        " (saved)"
                                    }
                                ),
                                level: Some(LogLevel::Info),
                                turn: None,
                            });
                        }
                    }
                    Some("tool_request") => {
                        let req_id = json["id"].as_str().unwrap_or("").to_string();
                        let tool = json["tool"].as_str().unwrap_or("").to_string();
                        let args = json
                            .get("args")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(Default::default()));

                        // Log the incoming tool request at Debug level
                        let args_preview = {
                            let s = serde_json::to_string(&args).unwrap_or_default();
                            preview_text(&s, 200)
                        };
                        bus_inbound.send(AppEvent::PresenceLog {
                            message: format!("[tool_request] {}({})", tool, args_preview),
                            level: Some(LogLevel::Debug),
                            turn: None,
                        });

                        // Dispatch through presence-core (single canonical layer)
                        let state = query_ctx_inbound
                            .as_ref()
                            .map(|ctx| {
                                ctx.agent_state
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone()
                            })
                            .unwrap_or_default();
                        let action = presence::dispatch_tool_call(&tool, &args, &state);

                        // SubmitTask: send directly to task_tx (bypasses TUI)
                        let query_result = if let presence::PresenceAction::SubmitTask(envelope) =
                            action
                        {
                            let msg = format!("Task submitted: {}", envelope.task);
                            if let Some(ref tx) = task_tx_inbound {
                                let _ = tx.send(envelope).await;
                            } else {
                                // Fallback: dispatch via EventBus if no task_tx
                                let ctrl_action = presence::PresenceAction::SubmitTask(envelope);
                                if let Some((ctrl, _)) =
                                    presence::action_to_control_msg(&ctrl_action)
                                {
                                    bus_inbound.send(AppEvent::ControlCommand(ctrl));
                                }
                            }
                            presence::ToolQueryResult::text(msg)
                        } else if let Some((ctrl, msg)) = presence::action_to_control_msg(&action) {
                            // Other action tools: dispatch via EventBus
                            bus_inbound.send(AppEvent::ControlCommand(ctrl));
                            presence::ToolQueryResult::text(msg)
                        } else {
                            match action {
                                presence::PresenceAction::TextResult(text) => {
                                    presence::ToolQueryResult::text(text)
                                }
                                presence::PresenceAction::NeedsIO {
                                    tool_name,
                                    args: io_args,
                                } => {
                                    if let Some(ref ctx) = query_ctx_inbound {
                                        if let Some(result) = presence::handle_tool_query(
                                            &ctx.agent_state,
                                            &ctx.project_root,
                                            &ctx.log_dir,
                                            &ctx.knowledge_path,
                                            &tool_name,
                                            &io_args,
                                            frame_registry_inbound.as_ref(),
                                            ctx.context_injection.as_ref(),
                                        )
                                        .await
                                        {
                                            result
                                        } else {
                                            presence::ToolQueryResult::text(format!(
                                                "Unknown tool: {}",
                                                tool
                                            ))
                                        }
                                    } else {
                                        presence::ToolQueryResult::text(
                                            "Presence query context not available".to_string(),
                                        )
                                    }
                                }
                                _ => unreachable!(),
                            }
                        };

                        // Log the tool response at Debug level
                        let result_preview = preview_text(&query_result.text, 200);
                        bus_inbound.send(AppEvent::PresenceLog {
                            message: format!("[tool_response] {} → {}", tool, result_preview),
                            level: Some(LogLevel::Debug),
                            turn: None,
                        });

                        let mut response = serde_json::json!({
                            "t": "tool_response",
                            "id": req_id,
                            "result": query_result.text,
                        });
                        if !query_result.images.is_empty() {
                            let img_array: Vec<serde_json::Value> = query_result
                                .images
                                .iter()
                                .map(|img| {
                                    serde_json::json!({
                                        "mime_type": img.media_type,
                                        "data": img.data,
                                    })
                                })
                                .collect();
                            response["images"] = serde_json::Value::Array(img_array);
                        }
                        let _ = direct_tx_inbound.send(response.to_string());
                    }
                    Some("async_query") => {
                        // Async query from browser — same dispatch as tool_request
                        // but result goes back as async_query_result (injected into
                        // voice session as text, not as a tool response).
                        let req_id = json["id"].as_str().unwrap_or("").to_string();
                        let tool = json["tool"].as_str().unwrap_or("").to_string();
                        let args = json
                            .get("args")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(Default::default()));

                        bus_inbound.send(AppEvent::PresenceLog {
                            message: format!("[async_query] {}", tool),
                            level: Some(LogLevel::Debug),
                            turn: None,
                        });

                        let query_result = if let Some(ref ctx) = query_ctx_inbound {
                            if let Some(result) = presence::handle_tool_query(
                                &ctx.agent_state,
                                &ctx.project_root,
                                &ctx.log_dir,
                                &ctx.knowledge_path,
                                &tool,
                                &args,
                                frame_registry_inbound.as_ref(),
                                ctx.context_injection.as_ref(),
                            )
                            .await
                            {
                                result
                            } else {
                                presence::ToolQueryResult::text(format!(
                                    "Unknown query tool: {}",
                                    tool
                                ))
                            }
                        } else {
                            presence::ToolQueryResult::text(
                                "Presence query context not available".to_string(),
                            )
                        };

                        let result_preview = preview_text(&query_result.text, 200);
                        bus_inbound.send(AppEvent::PresenceLog {
                            message: format!("[async_query_result] {} → {}", tool, result_preview),
                            level: Some(LogLevel::Debug),
                            turn: None,
                        });

                        let mut response = serde_json::json!({
                            "t": "async_query_result",
                            "id": req_id,
                            "tool": tool,
                            "result": query_result.text,
                        });
                        if !query_result.images.is_empty() {
                            let img_array: Vec<serde_json::Value> = query_result
                                .images
                                .iter()
                                .map(|img| {
                                    serde_json::json!({
                                        "mime_type": img.media_type,
                                        "data": img.data,
                                    })
                                })
                                .collect();
                            response["images"] = serde_json::Value::Array(img_array);
                        }
                        let _ = direct_tx_inbound.send(response.to_string());
                    }
                    Some("display_offer") => {
                        // WebRTC SDP offer from browser for a display session
                        let display_id = json["display_id"].as_u64().unwrap_or(0) as u32;
                        let sdp = json["sdp"].as_str().unwrap_or("").to_string();

                        // Clone the Arc<DisplaySession> out of the read
                        // lock before calling handle_offer. Holding the
                        // guard across the await chokes any writer
                        // (notably deactivate_user_display's
                        // registry.write()) for as long as this block
                        // runs. The Arc keeps the session alive
                        // independently of the lock. `get_any`: local
                        // dashboard viewers are the user surface —
                        // private user views stream here (and only here).
                        let session: Option<Arc<crate::display::DisplaySession>> =
                            match session_registry_inbound.as_ref() {
                                Some(sr) => sr.read().await.get_any(display_id),
                                None => None,
                            };
                        if let Some(session) = session {
                            let (ice_tx, mut ice_rx) =
                                mpsc::channel::<(crate::display::PeerId, String)>(64);
                            // Combine the Host-header IP with the
                            // port we want to advertise (HTTP port
                            // for Phase 3 multiplex, or standalone
                            // Phase 2 port) to form the single TCP
                            // candidate the peer will emit. None
                            // if either piece is missing (typically
                            // because the browser connected via
                            // hostname).
                            let tcp_advertised_addr: Option<std::net::SocketAddr> =
                                match (browser_host_ip, tcp_advertised_port) {
                                    (Some(ip), Some(port)) => {
                                        Some(std::net::SocketAddr::new(ip, port))
                                    }
                                    _ => None,
                                };
                            // Phase 5a.1 input authority gate.  The closure
                            // returns true when this connection is the
                            // authority holder OR when the display has no
                            // holder (unclaimed = pre-phase-5 default).
                            // `display/mod.rs` only sees this boolean; it
                            // never learns about DisplayInputHolder, the
                            // map, or connection IDs.  See
                            // [`build_local_ws_input_authorizer`] for the
                            // closure semantics + tests.
                            let input_authorized = build_local_ws_input_authorizer(
                                display_id,
                                connection_id_inbound.clone(),
                                Arc::clone(&display_input_authority_inbound),
                            );
                            // F-1.3b2 transport plumbing: local DisplaySlot's
                            // browser doesn't create the
                            // `display_input_authority` data channel
                            // (5a/5c uses the WS path), so the handler is
                            // never invoked here. The no-op keeps the
                            // transport-layer signature uniform across
                            // both peer kinds; the real federated handler
                            // is wired by the federated path's caller in
                            // a later slice.
                            let authority_handler =
                                crate::display::webrtc::noop_authority_handler();
                            match session
                                .handle_offer(
                                    peer_id,
                                    &sdp,
                                    &ice_config,
                                    Some(Arc::clone(&tcp_peer_registry)),
                                    tcp_advertised_addr,
                                    ice_tx,
                                    input_authorized,
                                    authority_handler,
                                )
                                .await
                            {
                                Ok(answer_sdp) => {
                                    peer_display_ids.push(display_id);
                                    let answer = serde_json::json!({
                                        "t": "display_answer",
                                        "display_id": display_id,
                                        "sdp": answer_sdp,
                                    });
                                    let _ = direct_tx_inbound.send(answer.to_string());

                                    // Forward server ICE candidates to browser
                                    let ice_direct_tx = direct_tx_inbound.clone();
                                    tokio::spawn(async move {
                                        while let Some((_pid, candidate_json)) = ice_rx.recv().await
                                        {
                                            let msg = serde_json::json!({
                                                "t": "display_ice",
                                                "display_id": display_id,
                                                "candidate": serde_json::from_str::<serde_json::Value>(&candidate_json).unwrap_or_default(),
                                            });
                                            if ice_direct_tx.send(msg.to_string()).is_err() {
                                                break;
                                            }
                                        }
                                    });
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[web_gateway] WebRTC offer failed for display {}: {}",
                                        display_id, e
                                    );
                                }
                            }
                        }
                    }
                    Some("display_ice") => {
                        // Trickle ICE candidate from browser. Spawn the
                        // handling off the ws reader loop because
                        // `add_ice_candidate` resolves mDNS hostnames
                        // (browsers obfuscate host candidates as
                        // `<uuid>.local`). On hosts without an mDNS
                        // responder — every headless VM without Avahi,
                        // which is the common deployment — each lookup
                        // blocks on the system resolver's full timeout
                        // (5-20s). With multiple candidates and ICE
                        // retries, that piles 20-30s of blocking inside
                        // this reader, stalling every other ws frame
                        // behind it including grant/revoke — the root
                        // cause of the "second ON takes 20+s" bug.
                        //
                        // Spawning decouples candidate processing from
                        // frame intake. Failed lookups still log the
                        // same "mdns resolve failed" diagnostic; losing
                        // a candidate is survivable (ICE has others),
                        // whereas blocking the reader is not.
                        let display_id = json["display_id"].as_u64().unwrap_or(0) as u32;
                        let candidate = json["candidate"].to_string();
                        let sr_clone = session_registry_inbound.clone();
                        let pid = peer_id;
                        tokio::spawn(async move {
                            // Clone the session Arc out of the read
                            // lock first. The previous spread-across-
                            // `if let` form held the guard across
                            // add_ice_candidate's mDNS resolution,
                            // which on hosts without Avahi blocks for
                            // 5-20s per candidate — starving any
                            // concurrent writer (notably
                            // deactivate_user_display's
                            // registry.write()). Dropping the guard
                            // first lets deactivate proceed
                            // immediately; the session Arc keeps the
                            // target alive while mDNS resolves.
                            // `get_any`: same local-dashboard leg as the
                            // display_offer handler — private user views
                            // stream to the owner's dashboards.
                            let session: Option<Arc<crate::display::DisplaySession>> =
                                match sr_clone.as_ref() {
                                    Some(sr) => sr.read().await.get_any(display_id),
                                    None => None,
                                };
                            if let Some(session) = session {
                                if let Err(e) = session.add_ice_candidate(pid, &candidate).await {
                                    eprintln!(
                                        "[web_gateway] ICE candidate failed for display {}: {}",
                                        display_id, e
                                    );
                                }
                            }
                        });
                    }
                    Some("dashboard_control_offer") => {
                        let sdp = json["sdp"].as_str().unwrap_or("").to_string();
                        let client_nonce = json["client_nonce"]
                            .as_str()
                            .map(str::trim)
                            .filter(|nonce| !nonce.is_empty())
                            .map(str::to_string);
                        if sdp.is_empty() {
                            let msg = serde_json::json!({
                                "t": "dashboard_control_error",
                                "error": "missing sdp",
                            });
                            let _ = direct_tx_inbound.send(msg.to_string());
                            continue;
                        }
                        match dashboard_control_inbound
                            .answer_offer_with_grant(
                                sdp,
                                None,
                                client_nonce,
                                dashboard_control_grant_inbound.clone(),
                            )
                            .await
                        {
                            Ok(answer) => {
                                dashboard_control_session_ids.push(answer.session_id.clone());
                                let msg = serde_json::json!({
                                    "t": "dashboard_control_answer",
                                    "session_id": answer.session_id,
                                    "sdp": answer.sdp,
                                    "binding": answer.binding,
                                });
                                let _ = direct_tx_inbound.send(msg.to_string());
                            }
                            Err(e) => {
                                let msg = serde_json::json!({
                                    "t": "dashboard_control_error",
                                    "error": e,
                                });
                                let _ = direct_tx_inbound.send(msg.to_string());
                            }
                        }
                    }
                    Some("dashboard_control_ice") => {
                        let session_id = json["session_id"].as_str().unwrap_or("").to_string();
                        let candidate = json
                            .get("candidate")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({}));
                        if session_id.is_empty() {
                            continue;
                        }
                        let registry = Arc::clone(&dashboard_control_inbound);
                        tokio::spawn(async move {
                            if let Err(e) =
                                registry.add_ice_candidate(&session_id, &candidate).await
                            {
                                eprintln!("[dashboard/control] add ICE failed: {e}");
                            }
                        });
                    }
                    Some("dashboard_control_close") => {
                        let session_id = json["session_id"].as_str().unwrap_or("").to_string();
                        if !session_id.is_empty() {
                            dashboard_control_inbound.close(&session_id).await;
                            dashboard_control_session_ids.retain(|s| s != &session_id);
                        }
                    }
                    Some("terminal_open") => {
                        // {"t":"terminal_open","host_id":"local","terminal_id":"shell-0","cols":80,"rows":24}
                        let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                        let terminal_id = json["terminal_id"]
                            .as_str()
                            .unwrap_or("shell-0")
                            .to_string();
                        let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                        let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                        let key = crate::terminal::TerminalKey {
                            host_id: host_id.clone(),
                            terminal_id: terminal_id.clone(),
                        };

                        // Attach needs only the terminal.view
                        // floor already enforced; creating a
                        // shell needs shell.spawn, decided at
                        // frame time so expiry mid-connection
                        // is honored. A grant-level fs scope
                        // makes the new shell a sandboxed one.
                        let spawn_policy = crate::terminal::ShellSpawnPolicy {
                            may_spawn: dashboard_control_grant_inbound
                                .access_decision(
                                    crate::peer::access_policy::PeerOperation::ShellSpawn,
                                )
                                .allowed,
                            shared: json["shared"].as_bool().unwrap_or(false),
                            scope: dashboard_control_grant_inbound.filesystem().cloned(),
                        };
                        match terminal_registry_inbound
                            .open_or_attach(
                                key.clone(),
                                cols,
                                rows,
                                &ws_terminal_actor,
                                spawn_policy,
                            )
                            .await
                        {
                            Ok((session, _created)) => {
                                // Spawn a forwarder task that drains the session's
                                // per-listener channel and sends base64-encoded
                                // output to this WS connection.
                                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                                session.attach(tx);

                                let forwarder_tx = direct_tx_inbound.clone();
                                let fwd_host = host_id.clone();
                                let fwd_term = terminal_id.clone();
                                tokio::spawn(async move {
                                    use base64::Engine as _;
                                    while let Some(event) = rx.recv().await {
                                        let msg = match event {
                                            crate::terminal::TerminalEvent::Output(bytes) => {
                                                let b64 = base64::engine::general_purpose::STANDARD
                                                    .encode(&bytes);
                                                serde_json::json!({
                                                    "t": "terminal_output",
                                                    "host_id": fwd_host,
                                                    "terminal_id": fwd_term,
                                                    "data": b64,
                                                })
                                            }
                                            crate::terminal::TerminalEvent::Exited { status } => {
                                                serde_json::json!({
                                                    "t": "terminal_exited",
                                                    "host_id": fwd_host,
                                                    "terminal_id": fwd_term,
                                                    "status": status,
                                                })
                                            }
                                        };
                                        if forwarder_tx.send(msg.to_string()).is_err() {
                                            break;
                                        }
                                    }
                                });

                                let ack = serde_json::json!({
                                    "t": "terminal_opened",
                                    "host_id": host_id,
                                    "terminal_id": terminal_id,
                                    "shared": session.shared(),
                                    "can_share": session
                                        .managed_by(&ws_terminal_actor),
                                });
                                let _ = direct_tx_inbound.send(ack.to_string());
                            }
                            Err(e) => {
                                let err = serde_json::json!({
                                    "t": "terminal_error",
                                    "host_id": host_id,
                                    "terminal_id": terminal_id,
                                    "error": e.to_string(),
                                });
                                let _ = direct_tx_inbound.send(err.to_string());
                            }
                        }
                    }
                    Some("terminal_input") => {
                        // {"t":"terminal_input","host_id":"local","terminal_id":"shell-0","data":"<base64>"}
                        let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                        let terminal_id = json["terminal_id"]
                            .as_str()
                            .unwrap_or("shell-0")
                            .to_string();
                        let data_b64 = json["data"].as_str().unwrap_or("");
                        use base64::Engine as _;
                        if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64)
                        {
                            let key = crate::terminal::TerminalKey {
                                host_id,
                                terminal_id,
                            };
                            if let Some(session) = terminal_registry_inbound
                                .get_visible(&key, &ws_terminal_actor)
                                .await
                            {
                                session.write_input(&data);
                            }
                        }
                    }
                    Some("terminal_resize") => {
                        // {"t":"terminal_resize","host_id":"local","terminal_id":"shell-0","cols":N,"rows":N}
                        let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                        let terminal_id = json["terminal_id"]
                            .as_str()
                            .unwrap_or("shell-0")
                            .to_string();
                        let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                        let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                        let key = crate::terminal::TerminalKey {
                            host_id,
                            terminal_id,
                        };
                        if let Some(session) = terminal_registry_inbound
                            .get_visible(&key, &ws_terminal_actor)
                            .await
                        {
                            session.resize(cols, rows);
                        }
                    }
                    Some("terminal_close") => {
                        // {"t":"terminal_close","host_id":"local","terminal_id":"shell-0"}
                        let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                        let terminal_id = json["terminal_id"]
                            .as_str()
                            .unwrap_or("shell-0")
                            .to_string();
                        let key = crate::terminal::TerminalKey {
                            host_id,
                            terminal_id,
                        };
                        terminal_registry_inbound
                            .close_visible(&key, &ws_terminal_actor)
                            .await;
                    }
                    Some("terminal_share") => {
                        // {"t":"terminal_share","host_id":"local","terminal_id":"shell-0","shared":true}
                        let host_id = json["host_id"].as_str().unwrap_or("local").to_string();
                        let terminal_id = json["terminal_id"]
                            .as_str()
                            .unwrap_or("shell-0")
                            .to_string();
                        let shared = json["shared"].as_bool().unwrap_or(true);
                        let key = crate::terminal::TerminalKey {
                            host_id: host_id.clone(),
                            terminal_id: terminal_id.clone(),
                        };
                        let msg = match terminal_registry_inbound
                            .set_shared(&key, &ws_terminal_actor, shared)
                            .await
                        {
                            Some(state) => serde_json::json!({
                                "t": "terminal_shared",
                                "host_id": host_id,
                                "terminal_id": terminal_id,
                                "shared": state,
                            }),
                            None => serde_json::json!({
                                "t": "terminal_error",
                                "host_id": host_id,
                                "terminal_id": terminal_id,
                                "error": "not allowed: only the session owner or root can change sharing",
                            }),
                        };
                        let _ = direct_tx_inbound.send(msg.to_string());
                    }
                    Some("display_input") => {
                        // Input event (keyboard/mouse) for a display session.
                        // Drop the registry read lock before the inject
                        // (which runs xdotool/cliclick subprocesses) so a
                        // concurrent deactivate can take the write lock
                        // without waiting on subprocess exits.
                        let display_id = json["display_id"].as_u64().unwrap_or(0) as u32;

                        // Phase 5 authority gate: if someone has claimed
                        // input authority for this display, only that
                        // connection's input flows through. Unclaimed
                        // (no entry in the map) = pre-phase-5 default,
                        // every connection can input. See the
                        // `DisplayInputHolder` doc for the full
                        // contract.
                        let allowed = {
                            let authority = display_input_authority_inbound
                                .read()
                                .unwrap_or_else(|e| e.into_inner());
                            match authority.get(&display_id) {
                                Some(entry) => entry.matches_local_ws(&connection_id_inbound),
                                None => true,
                            }
                        };
                        if !allowed {
                            // Silent drop — matches the "force_disconnect_voice"
                            // convention where demoted connections don't get
                            // per-message denial feedback; the browser already
                            // knows it's passive from the authority_revoked
                            // notification it received when it was demoted.
                            continue;
                        }

                        if let Some(evt) = json.get("event") {
                            if let Ok(input_event) =
                                serde_json::from_value::<crate::display::InputEvent>(evt.clone())
                            {
                                // `get_any`: dashboard input drives private
                                // user views too (that's the remote-control
                                // point of "View this machine").
                                let session: Option<Arc<crate::display::DisplaySession>> =
                                    match session_registry_inbound.as_ref() {
                                        Some(sr) => sr.read().await.get_any(display_id),
                                        None => None,
                                    };
                                if let Some(session) = session {
                                    if let Err(e) = session.inject_input(input_event).await {
                                        eprintln!(
                                            "[web_gateway] display input injection failed: {}",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Some("set_diagnostics_visual_marker") => {
                        // **Phase 0 visual-freshness diagnostic toggle**
                        // (task #83). Inline rather than going through
                        // the ControlMsg dispatch path because the
                        // effect is a single atomic store on the
                        // matching DisplaySession — no shared autonomy
                        // state, no event-bus side effects, no listener
                        // chain to wait on. Symmetric with the
                        // `display_input` arm above for the same reason
                        // (direct session access, no bus round-trip).
                        //
                        // No authority gate: diagnostics is operator-
                        // initiated and the marker affects every viewer
                        // of this display when on (it's stamped pre-
                        // encoder, lands in every encoded layer). An
                        // operator running a smoke run sets it, all
                        // viewers see the marker until they unset it.
                        // No covert-stamp scenario worth gating against.
                        let display_id = json["display_id"].as_u64().unwrap_or(0) as u32;
                        let enabled = json["enabled"].as_bool().unwrap_or(false);
                        match session_registry_inbound.as_ref() {
                            Some(sr) => {
                                let applied = sr
                                    .write()
                                    .await
                                    .set_diagnostics_visual_marker(display_id, enabled);
                                eprintln!(
                                    "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                    display_id,
                                    enabled,
                                    if applied { "" } else { " (pending)" },
                                );
                            }
                            None => {
                                eprintln!(
                                    "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                    display_id, enabled,
                                );
                            }
                        }
                    }
                    _ => {
                        // Fall through to ControlMsg parsing.
                        // WebRtcSignal needs special handling because
                        // it requires session_registry / direct_tx
                        // access for the response leg; everything else
                        // gets re-broadcast as AppEvent::ControlCommand
                        // for the agent loop / TUI / MCP consumers.
                        match serde_json::from_value::<ControlMsg>(json) {
                            Ok(ctrl)
                                if !peer_identity_allows_ws_control(
                                    peer_identity_inbound.as_ref(),
                                    &ctrl,
                                    &bus_inbound,
                                ) => {}
                            Ok(ctrl)
                                if !ws_grant_allows_control(
                                    &dashboard_control_grant_inbound,
                                    peer_identity_inbound.as_ref(),
                                    &ctrl,
                                    &bus_inbound,
                                ) => {}
                            Ok(ControlMsg::WebRtcSignal {
                                display_id,
                                session_id,
                                signal,
                            }) => {
                                let federated_display_input_allowed =
                                    peer_identity_allows_operation(
                                        peer_identity_inbound.as_ref(),
                                        crate::peer::access_policy::PeerOperation::DisplayInput,
                                        "peer-webrtc-display",
                                    );
                                handle_federated_webrtc_signal(
                                    display_id,
                                    session_id,
                                    signal,
                                    session_registry_inbound.as_ref(),
                                    &ice_config,
                                    Arc::clone(&tcp_peer_registry),
                                    direct_tx_inbound.clone(),
                                    &bus_inbound,
                                    // F-1.3b3 federated authority context.
                                    // `connection_id_inbound` is this WS's
                                    // id, which doubles as the federation
                                    // transport's `federation_connection_id`
                                    // when this connection is acting as a
                                    // federation transport.
                                    connection_id_inbound.clone(),
                                    Arc::clone(&display_input_authority_inbound),
                                    authority_change_tx_inbound.clone(),
                                    Arc::clone(&federated_authority_subscribers_inbound),
                                    federated_display_input_allowed,
                                )
                                .await;
                            }
                            Ok(ControlMsg::PeerFileTransferSignal { session_id, signal }) => {
                                handle_peer_file_transfer_signal(
                                    session_id,
                                    signal,
                                    Arc::clone(&peer_file_transfer_registry_inbound),
                                    peer_identity_inbound.clone(),
                                    direct_tx_inbound.clone(),
                                    &bus_inbound,
                                )
                                .await;
                            }
                            Ok(ControlMsg::PeerDashboardControlSignal { session_id, signal }) => {
                                handle_peer_dashboard_control_signal(
                                    session_id,
                                    signal,
                                    Arc::clone(&dashboard_control_inbound),
                                    peer_identity_inbound.clone(),
                                    direct_tx_inbound.clone(),
                                    &bus_inbound,
                                )
                                .await;
                            }
                            Ok(ControlMsg::RequestDisplayInputAuthority { display_id }) => {
                                // Phase 5a.1: handler body lives in
                                // `apply_grant_input_authority` so the
                                // authority-change emission is unit-testable
                                // without standing up a WS lifecycle.  This
                                // arm keeps the bus log + the per-connection
                                // confirm message at the call site to avoid
                                // baking logging dependencies into the helper.
                                apply_grant_input_authority(
                                    display_id,
                                    connection_id_inbound.clone(),
                                    direct_tx_inbound.clone(),
                                    &display_input_authority_inbound,
                                    &authority_change_tx_inbound,
                                );
                                // Confirm to the new holder (kept here so the
                                // helper has no dependency on the call site's
                                // direct_tx — and so the failure-to-send case
                                // doesn't bubble past the gate).
                                let granted = serde_json::json!({
                                    "t": "display_input_authority_granted",
                                    "display_id": display_id,
                                })
                                .to_string();
                                let _ = direct_tx_inbound.send(granted);
                                bus_inbound.send(AppEvent::PresenceLog {
                                    message: format!(
                                        "[ws] display_input_authority granted display={} holder={}",
                                        display_id, connection_id_inbound,
                                    ),
                                    level: Some(LogLevel::Debug),
                                    turn: None,
                                });
                            }
                            Ok(ControlMsg::ReleaseDisplayInputAuthority { display_id }) => {
                                let removed = apply_release_input_authority(
                                    display_id,
                                    connection_id_inbound.as_str(),
                                    &display_input_authority_inbound,
                                    &authority_change_tx_inbound,
                                );
                                if removed {
                                    bus_inbound.send(AppEvent::PresenceLog {
                                        message: format!(
                                            "[ws] display_input_authority released display={} holder={}",
                                            display_id, connection_id_inbound,
                                        ),
                                        level: Some(LogLevel::Debug),
                                        turn: None,
                                    });
                                }
                            }
                            Ok(ControlMsg::SetDiagnosticsVisualMarker {
                                display_id,
                                enabled,
                            }) => {
                                // Accept the documented ControlMsg wire form
                                // (`{"action":"set_diagnostics_visual_marker", ...}`)
                                // in addition to the low-level `t` form
                                // handled above. The smoke script uses
                                // ControlMsg JSON so the toggle must be
                                // applied here instead of falling through to
                                // the generic bus path, where this variant is
                                // intentionally a no-op for TUI/MCP parity.
                                let display_id = display_id.unwrap_or(0);
                                match session_registry_inbound.as_ref() {
                                    Some(sr) => {
                                        let applied = sr
                                            .write()
                                            .await
                                            .set_diagnostics_visual_marker(display_id, enabled);
                                        eprintln!(
                                            "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                            display_id,
                                            enabled,
                                            if applied { "" } else { " (pending)" },
                                        );
                                    }
                                    None => {
                                        eprintln!(
                                            "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                            display_id, enabled,
                                        );
                                    }
                                }
                            }
                            Ok(ctrl @ ControlMsg::ResumeSession { .. }) => {
                                let ControlMsg::ResumeSession {
                                    source,
                                    session_id,
                                    resume_id,
                                    task,
                                    ..
                                } = &ctrl
                                else {
                                    unreachable!();
                                };
                                let source = source.clone();
                                let session_id = session_id.clone();
                                let resume_id = resume_id.clone();
                                let task = task.clone();
                                let direct_tx_resume = direct_tx_inbound.clone();
                                let bus_resume = bus_inbound.clone();
                                tokio::spawn(async move {
                                    let replay = tokio::task::spawn_blocking(move || {
                                        resume_session_activity_replay(
                                            &source,
                                            &session_id,
                                            resume_id.as_deref(),
                                            task.as_deref(),
                                            EXTERNAL_ACTIVITY_REPLAY_LIMIT,
                                        )
                                    })
                                    .await
                                    .ok()
                                    .flatten();
                                    if let Some(replay) = replay {
                                        let _ = direct_tx_resume.send(replay);
                                    }
                                    bus_resume.send(AppEvent::PresenceLog {
                                        message: format!("[ws] ControlMsg: {:?}", ctrl),
                                        level: Some(LogLevel::Debug),
                                        turn: None,
                                    });
                                    bus_resume.send(AppEvent::ControlCommand(ctrl));
                                });
                            }
                            Ok(ctrl) => {
                                bus_inbound.send(AppEvent::PresenceLog {
                                    message: format!(
                                        "[ws] ControlMsg: {:?}",
                                        match &ctrl {
                                            ControlMsg::StartTask { task, .. } =>
                                                format!("StartTask({})", preview_text(task, 60)),
                                            other => format!("{:?}", other),
                                        }
                                    ),
                                    level: Some(LogLevel::Debug),
                                    turn: None,
                                });
                                bus_inbound.send(AppEvent::ControlCommand(ctrl));
                            }
                            Err(e) => {
                                bus_inbound.send(AppEvent::PresenceLog {
                                    message: format!("[ws] ControlMsg parse failed: {}", e),
                                    level: Some(LogLevel::Warn),
                                    turn: None,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // WebSocket closed — clean up active slot and auto-resume
    // server presence if this was the active browser (covers tab
    // close without beforeunload, network drops, etc.)
    if is_active {
        let mut slot = active_presence_inbound
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .map(|a| a.connection_id == connection_id_inbound)
            .unwrap_or(false)
        {
            *slot = None;
        }
    }
    // Also release any display input authority this
    // connection held (phase 5).  Without this, a
    // dangling entry would block other browsers from
    // claiming the display until someone explicitly
    // sent RequestDisplayInputAuthority to force-take
    // it — the `retain` below is the normal-drop
    // cleanup that keeps the map consistent with
    // live connections.
    //
    // Phase 5a.1: helper handles map mutation + per-
    // display None-holder change emit so other
    // browsers don't stay stuck on `other` after the
    // holder's WS drops.  See
    // `apply_ws_close_input_authority` for the
    // semantics + tests.
    apply_ws_close_input_authority(
        connection_id_inbound.as_str(),
        &display_input_authority_inbound,
        &authority_change_tx_inbound,
    );
    // F-1.3b3: federation-transport WS-close
    // cleanup. Two disjoint registry entries can
    // belong to one connection_id — `LocalWs` from
    // direct-browser use or `FederatedWebRtc` from
    // federation-transport use — so both apply_*
    // helpers fire from the same WS-close hook.
    // The single WS in practice acts in only one
    // role at a time, so the second helper is a
    // no-op in the typical case; the cost of
    // running both is the bookkeeping above.
    //
    // Order: unregister subscribers first (stops
    // new fanout sends) → release authority (so
    // observers see `unclaimed`) → close
    // WebRtcPeers (so the data channels stop
    // accepting incoming `display_input_authority_request`
    // frames under the now-defunct federation
    // identity). Without the peer-teardown step,
    // the authority handler closure on each
    // surviving peer would keep mutating the
    // registry under an identity whose WS is
    // gone — the structural bug F-1.3b3 fix #2
    // closes.
    let released_federated_subs = unregister_all_federated_subscribers_for_connection(
        connection_id_inbound.as_str(),
        &federated_authority_subscribers_inbound,
    );
    apply_federated_ws_close_input_authority(
        connection_id_inbound.as_str(),
        &display_input_authority_inbound,
        &authority_change_tx_inbound,
    );
    close_federated_peers_for_sessions(&released_federated_subs, session_registry_inbound.as_ref())
        .await;
    if is_presence_connected && is_active {
        bus_inbound.send(AppEvent::PresenceDisconnected);
    }
    // Remove this peer from display sessions it connected to. `get_any`:
    // teardown must find private user views too, or their RTC peers leak.
    if !peer_display_ids.is_empty() {
        if let Some(ref sr) = session_registry_inbound {
            let reg = sr.read().await;
            for did in &peer_display_ids {
                if let Some(session) = reg.get_any(*did) {
                    session.remove_peer(peer_id).await;
                }
            }
        }
    }
    for session_id in dashboard_control_session_ids {
        dashboard_control_inbound.close(&session_id).await;
    }
}
