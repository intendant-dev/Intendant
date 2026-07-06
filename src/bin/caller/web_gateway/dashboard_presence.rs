//! Dashboard-control presence sessions: the active-presence registry and
//! the connect/disconnect/make-active/cleanup lifecycle over the dashboard
//! control channel.

use super::*;

/// Tracks which WebSocket connection currently owns the voice model (is "active").
/// Only one connection can be active at a time; all others are "passive" (TUI-only).
pub(crate) struct ActivePresence {
    pub(crate) connection_id: String,
    pub(crate) direct_tx: mpsc::UnboundedSender<String>,
}

pub(crate) fn dashboard_control_presence_sender(
    control_tx: mpsc::UnboundedSender<serde_json::Value>,
) -> mpsc::UnboundedSender<String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let payload = serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or_else(|_| serde_json::json!({"t": "raw", "text": text}));
            let _ = control_tx.send(serde_json::json!({
                "t": "event",
                "payload": payload,
            }));
        }
    });
    tx
}

pub(crate) fn send_dashboard_control_presence_event(
    control_tx: &mpsc::UnboundedSender<serde_json::Value>,
    payload: serde_json::Value,
) {
    let _ = control_tx.send(serde_json::json!({
        "t": "event",
        "payload": payload,
    }));
}

pub(crate) async fn dashboard_control_presence_connect(
    request: crate::dashboard_control::DashboardPresenceConnectRequest,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
    default_provider: String,
    default_model: String,
) {
    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = true;

    let provider = request.provider.unwrap_or(default_provider);
    let model = request.model.unwrap_or(default_model);
    let sender = dashboard_control_presence_sender(request.control_tx.clone());
    let (becomes_active, was_already_active) = {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        let was_already_active = slot
            .as_ref()
            .map(|active| active.connection_id == request.session_id)
            .unwrap_or(false);
        let becomes_active = !request.passive && (slot.is_none() || was_already_active);
        if becomes_active {
            *slot = Some(ActivePresence {
                connection_id: request.session_id.clone(),
                direct_tx: sender,
            });
        }
        (becomes_active, was_already_active)
    };

    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);

    if let Some(ctx) = &query_ctx {
        let conversation_ctx = presence::build_conversation_context(&ctx.log_dir, 20);
        if let Some(ps) = &ctx.presence_session {
            let mut session = ps.lock().unwrap_or_else(|e| e.into_inner());
            if becomes_active {
                session.set_connected(true);
            }
            let state = ctx
                .agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let welcome = session.build_welcome(request.last_event_seq, &state);
            send_dashboard_control_presence_event(
                &request.control_tx,
                serde_json::json!({
                    "t": "presence_welcome",
                    "session_id": welcome.session_id,
                    "state": welcome.state,
                    "events": welcome.events,
                    "last_checkpoint_summary": welcome.last_checkpoint_summary,
                    "current_seq": welcome.current_seq,
                    "is_active": becomes_active,
                    "conversation_context": conversation_ctx,
                }),
            );
        } else {
            send_dashboard_control_presence_event(
                &request.control_tx,
                serde_json::json!({
                    "t": "presence_welcome",
                    "is_active": becomes_active,
                    "conversation_context": conversation_ctx,
                }),
            );
        }
    } else {
        send_dashboard_control_presence_event(
            &request.control_tx,
            serde_json::json!({
                "t": "presence_welcome",
                "is_active": becomes_active,
            }),
        );
    }

    if becomes_active && !was_already_active {
        if let Some(sl) = session_log {
            if let Ok(mut log) = sl.lock() {
                log.presence_connected(Some(&provider), Some(&model));
            }
        }
        bus.send(AppEvent::PresenceConnected {
            server_session_id: request.server_session_id,
            last_event_seq: request.last_event_seq,
            live_provider: Some(provider),
            live_model: Some(model),
        });
    }
}

pub(crate) async fn dashboard_control_presence_disconnect(
    request: crate::dashboard_control::DashboardPresenceDisconnectRequest,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
) {
    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = false;
    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            ps.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_connected(false);
        }
    }
    let was_active = {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .map(|active| active.connection_id == request.session_id)
            .unwrap_or(false)
        {
            *slot = None;
            true
        } else {
            false
        }
    };
    if was_active {
        if let Some(sl) = session_log {
            if let Ok(mut log) = sl.lock() {
                log.presence_disconnected();
            }
        }
        bus.send(AppEvent::PresenceDisconnected);
    }
}

pub(crate) async fn dashboard_control_presence_make_active(
    request: crate::dashboard_control::DashboardPresenceMakeActiveRequest,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
    default_provider: String,
    default_model: String,
) {
    let provider = request.provider.unwrap_or(default_provider);
    let model = request.model.unwrap_or(default_model);
    let sender = dashboard_control_presence_sender(request.control_tx.clone());

    let previous_active = {
        let slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|active| active.connection_id.clone())
    };
    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);

    if let Some(sl) = &session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_diagnostic(
                "make_active_received_gateway",
                &format!(
                    "request from connection={} previous_active={}",
                    request.session_id,
                    previous_active.as_deref().unwrap_or("none"),
                ),
            );
        }
    }

    {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = slot.as_ref() {
            if old.connection_id != request.session_id {
                let _ = old.direct_tx.send(
                    serde_json::json!({
                        "t": "force_disconnect_voice",
                        "reason": "handover",
                    })
                    .to_string(),
                );
                if let Some(sl) = &session_log {
                    if let Ok(mut log) = sl.lock() {
                        log.voice_diagnostic(
                            "make_active_force_disconnect_gateway",
                            &format!(
                                "old_active={} new_active={}",
                                old.connection_id, request.session_id,
                            ),
                        );
                    }
                }
            } else if let Some(sl) = &session_log {
                if let Ok(mut log) = sl.lock() {
                    log.voice_diagnostic(
                        "make_active_noop_gateway",
                        &format!(
                            "request from already-active connection={}",
                            request.session_id,
                        ),
                    );
                }
            }
        }
        *slot = Some(ActivePresence {
            connection_id: request.session_id.clone(),
            direct_tx: sender,
        });
    }

    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = true;

    let handover_context = query_ctx
        .as_ref()
        .and_then(|ctx| ctx.presence_session.as_ref())
        .and_then(|ps| {
            let session = ps.lock().unwrap_or_else(|e| e.into_inner());
            session.last_checkpoint_summary()
        })
        .unwrap_or_default();
    let conversation_ctx = query_ctx
        .as_ref()
        .and_then(|ctx| presence::build_conversation_context(&ctx.log_dir, 20));
    let has_handover_context = !handover_context.is_empty();
    let has_conversation_context = conversation_ctx
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    send_dashboard_control_presence_event(
        &request.control_tx,
        serde_json::json!({
            "t": "active_granted",
            "is_active": true,
            "handover_context": handover_context,
            "conversation_context": conversation_ctx,
        }),
    );

    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_diagnostic(
                "make_active_granted_gateway",
                &format!(
                    "connection={} handover_context={} conversation_context={}",
                    request.session_id,
                    if has_handover_context { "yes" } else { "no" },
                    if has_conversation_context {
                        "yes"
                    } else {
                        "no"
                    },
                ),
            );
            log.presence_connected(Some(&provider), Some(&model));
        }
    }
    bus.send(AppEvent::PresenceConnected {
        server_session_id: None,
        last_event_seq: 0,
        live_provider: Some(provider),
        live_model: Some(model),
    });
}

pub(crate) async fn dashboard_control_presence_cleanup(
    session_id: String,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
) {
    let was_active = {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .map(|active| active.connection_id == session_id)
            .unwrap_or(false)
        {
            *slot = None;
            true
        } else {
            false
        }
    };
    if !was_active {
        return;
    }
    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = false;
    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            ps.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_connected(false);
        }
    }
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_disconnected();
        }
    }
    bus.send(AppEvent::PresenceDisconnected);
}
