//! Reconnecting Kimi `/api/v1/ws` client with durable cursor handling.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::error::CallerError;

use super::super::AgentEvent;
use super::events::{EventTranslator, KimiSharedState};
use super::wire::{external, KimiApi};

#[derive(Debug)]
pub(crate) enum WsCommand {
    Subscribe {
        session_id: String,
        snapshot_first: bool,
    },
    Shutdown,
}

#[derive(Debug, Clone, Default)]
struct Cursor {
    seq: u64,
    epoch: Option<String>,
}

type ClientWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type VolatileOffsetKey = (String, String, String, String);

pub(crate) async fn spawn_driver(
    api: KimiApi,
    shared: Arc<KimiSharedState>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    protocol_watch: Option<super::super::protocol_watch::ProtocolWatchHandle>,
) -> Result<
    (
        mpsc::UnboundedSender<WsCommand>,
        tokio::task::JoinHandle<()>,
    ),
    CallerError,
> {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let handle = tokio::spawn(driver_task(
        api,
        shared,
        event_tx,
        protocol_watch,
        command_rx,
        ready_tx,
    ));
    match tokio::time::timeout(Duration::from_secs(15), ready_rx).await {
        Ok(Ok(Ok(()))) => Ok((command_tx, handle)),
        Ok(Ok(Err(error))) => {
            handle.abort();
            let _ = handle.await;
            Err(error)
        }
        Ok(Err(_)) => {
            handle.abort();
            let _ = handle.await;
            Err(external("Kimi WebSocket driver stopped during handshake"))
        }
        Err(_) => {
            handle.abort();
            let _ = handle.await;
            Err(external("timed out connecting to Kimi event stream"))
        }
    }
}

pub(crate) async fn await_driver_shutdown(
    mut handle: tokio::task::JoinHandle<()>,
    grace: Duration,
) {
    if tokio::time::timeout(grace, &mut handle).await.is_err() {
        handle.abort();
        // Await cancellation so no websocket task remains detached from the
        // Kimi process whose credentials and state are about to be removed.
        let _ = handle.await;
    }
}

async fn driver_task(
    api: KimiApi,
    shared: Arc<KimiSharedState>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    protocol_watch: Option<super::super::protocol_watch::ProtocolWatchHandle>,
    mut commands: mpsc::UnboundedReceiver<WsCommand>,
    ready: oneshot::Sender<Result<(), CallerError>>,
) {
    let mut ready = Some(ready);
    let mut subscriptions = HashSet::<String>::new();
    let mut pending_snapshots = HashSet::<String>::new();
    let mut cursors = HashMap::<String, Cursor>::new();
    let mut offsets = HashMap::<VolatileOffsetKey, usize>::new();
    let mut translator = EventTranslator::new(shared);
    let mut consecutive_failures = 0u32;
    let mut consecutive_resync_failures = 0u32;
    let mut control_id = 0u64;
    let mut output_refresh = tokio::time::interval(Duration::from_secs(2));
    output_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let connection = connect_and_hello(&api, &subscriptions, &cursors, &mut control_id).await;
        let (mut socket, resync) = match connection {
            Ok(connection) => {
                consecutive_failures = 0;
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
                connection
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(error));
                    return;
                }
                if consecutive_failures >= 8 {
                    let _ = event_tx.send(AgentEvent::Terminated {
                        reason: "Kimi server event stream could not reconnect".into(),
                        exit_code: None,
                    });
                    return;
                }
                let _ = event_tx.send(AgentEvent::Log {
                    level: "warn".into(),
                    message: format!(
                        "Kimi event stream disconnected; reconnecting ({consecutive_failures}/8)"
                    ),
                });
                let delay = Duration::from_millis(
                    200u64.saturating_mul(1u64 << consecutive_failures.min(4)),
                );
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    command = commands.recv() => {
                        if !apply_offline_command(
                            command,
                            &mut subscriptions,
                            &mut pending_snapshots,
                        ) {
                            return;
                        }
                    }
                }
                continue;
            }
        };

        let required_resync =
            required_resync_sessions(&subscriptions, &cursors, &pending_snapshots, resync);
        pending_snapshots.extend(required_resync.iter().cloned());
        for session_id in required_resync {
            if let Err(error) = resync_snapshot(
                &api,
                &session_id,
                &mut translator,
                &event_tx,
                &mut cursors,
                &mut offsets,
            )
            .await
            {
                let _ = event_tx.send(AgentEvent::Log {
                    level: "warn".into(),
                    message: format!(
                        "Kimi snapshot for {session_id} failed before reconnect: {error}"
                    ),
                });
                break;
            }
            pending_snapshots.remove(&session_id);
        }

        let mut reconnect = !pending_snapshots.is_empty();
        if reconnect {
            consecutive_resync_failures = consecutive_resync_failures.saturating_add(1);
            if consecutive_resync_failures >= 8 {
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: "Kimi event stream could not rebuild task/session state".into(),
                    exit_code: None,
                });
                return;
            }
            let _ = socket.close(None).await;
            let delay = Duration::from_millis(
                200u64.saturating_mul(1u64 << consecutive_resync_failures.min(4)),
            );
            tokio::time::sleep(delay).await;
        } else {
            consecutive_resync_failures = 0;
        }
        while !reconnect {
            tokio::select! {
                _ = output_refresh.tick(), if !subscriptions.is_empty() => {
                    refresh_registered_task_outputs(&api, &subscriptions).await;
                }
                command = commands.recv() => {
                    match command {
                        Some(WsCommand::Subscribe { session_id, snapshot_first }) => {
                            subscriptions.insert(session_id.clone());
                            if snapshot_first {
                                pending_snapshots.insert(session_id.clone());
                                if let Err(error) = resync_snapshot(
                                    &api,
                                    &session_id,
                                    &mut translator,
                                    &event_tx,
                                    &mut cursors,
                                    &mut offsets,
                                ).await {
                                    let _ = event_tx.send(AgentEvent::Log {
                                        level: "warn".into(),
                                        message: format!(
                                            "Kimi snapshot for {session_id} failed before subscription: {error}"
                                        ),
                                    });
                                    reconnect = true;
                                    continue;
                                }
                                pending_snapshots.remove(&session_id);
                            }
                            control_id = control_id.saturating_add(1);
                            let message = subscribe_message(&session_id, cursors.get(&session_id), control_id);
                            if send_json(&mut socket, &message).await.is_err() {
                                reconnect = true;
                            }
                        }
                        Some(WsCommand::Shutdown) | None => {
                            let _ = socket.close(None).await;
                            if let Some(watch) = protocol_watch.as_ref() {
                                watch.flush_async().await;
                            }
                            return;
                        }
                    }
                }
                frame = socket.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                                if let Some(watch) = protocol_watch.as_ref() {
                                    if let Some(message) = watch.observe(
                                        super::super::protocol_watch::ProtocolFinding::malformed(),
                                    ) {
                                        let _ = event_tx.send(AgentEvent::Log {
                                            level: "warn".into(),
                                            message,
                                        });
                                    }
                                }
                                let _ = event_tx.send(AgentEvent::Log {
                                    level: "warn".into(),
                                    message: "Kimi sent malformed JSON on its event stream".into(),
                                });
                                continue;
                            };
                            let event_frame = is_event_frame(&value);
                            if event_frame
                                && !event_belongs_to_requested_subscription(
                                    &value,
                                    &subscriptions,
                                )
                            {
                                continue;
                            }
                            if event_frame {
                                if let Some(watch) = protocol_watch.as_ref() {
                                    for message in watch.observe_all(
                                        super::super::protocol_watch::kimi_findings(&value),
                                    ) {
                                        let _ = event_tx.send(AgentEvent::Log {
                                            level: "warn".into(),
                                            message,
                                        });
                                    }
                                }
                            }
                            match value.get("type").and_then(Value::as_str).unwrap_or_default() {
                                "ping" => {
                                    let pong = serde_json::json!({
                                        "type": "pong",
                                        "payload": {
                                            "nonce": value
                                                .get("payload")
                                                .and_then(|payload| payload.get("nonce"))
                                                .cloned()
                                                .unwrap_or(Value::String(String::new()))
                                        }
                                    });
                                    if send_json(&mut socket, &pong).await.is_err() {
                                        reconnect = true;
                                    }
                                }
                                "resync_required" => {
                                    if let Some(session_id) =
                                        requested_resync_session(&value, &subscriptions)
                                    {
                                        pending_snapshots.insert(session_id.to_string());
                                        if resync_snapshot(
                                            &api,
                                            session_id,
                                            &mut translator,
                                            &event_tx,
                                            &mut cursors,
                                            &mut offsets,
                                        ).await.is_ok() {
                                            pending_snapshots.remove(session_id);
                                            control_id = control_id.saturating_add(1);
                                            let message = subscribe_message(
                                                session_id,
                                                cursors.get(session_id),
                                                control_id,
                                            );
                                            if send_json(&mut socket, &message).await.is_err() {
                                                reconnect = true;
                                            }
                                        } else {
                                            reconnect = true;
                                        }
                                    }
                                }
                                "error" if value.get("seq").is_none() => {
                                    let fatal = value
                                        .get("payload")
                                        .and_then(|payload| payload.get("fatal"))
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    let message = value
                                        .get("payload")
                                        .and_then(|payload| payload.get("msg"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("Kimi WebSocket error")
                                        .to_string();
                                    let _ = event_tx.send(AgentEvent::Log {
                                        level: if fatal { "error" } else { "warn" }.into(),
                                        message,
                                    });
                                    reconnect = fatal;
                                }
                                "ack" => {
                                    if value.get("code").and_then(Value::as_i64) != Some(0) {
                                        let _ = event_tx.send(AgentEvent::Log {
                                            level: "warn".into(),
                                            message: format!(
                                                "Kimi rejected WebSocket subscription: {}",
                                                value
                                                    .get("msg")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or("unknown error")
                                            ),
                                        });
                                        reconnect = true;
                                        continue;
                                    }
                                    for session_id in
                                        ack_resync_sessions(&value, &subscriptions)
                                    {
                                        pending_snapshots.insert(session_id.clone());
                                        if resync_snapshot(
                                            &api,
                                            &session_id,
                                            &mut translator,
                                            &event_tx,
                                            &mut cursors,
                                            &mut offsets,
                                        ).await.is_err() {
                                            reconnect = true;
                                            break;
                                        }
                                        pending_snapshots.remove(&session_id);
                                        control_id = control_id.saturating_add(1);
                                        let message = subscribe_message(
                                            &session_id,
                                            cursors.get(&session_id),
                                            control_id,
                                        );
                                        if send_json(&mut socket, &message).await.is_err() {
                                            reconnect = true;
                                            break;
                                        }
                                    }
                                }
                                "server_hello" => {}
                                _ => {
                                    let mut decision = event_decision(&value, &cursors, &offsets);
                                    if decision.needs_resync {
                                        if let Some(session_id) = decision.session_id.as_deref() {
                                            pending_snapshots.insert(session_id.to_string());
                                            if resync_snapshot(
                                                &api,
                                                session_id,
                                                &mut translator,
                                                &event_tx,
                                                &mut cursors,
                                                &mut offsets,
                                            ).await.is_ok() {
                                                pending_snapshots.remove(session_id);
                                                decision = event_decision(&value, &cursors, &offsets);
                                            } else {
                                                reconnect = true;
                                                continue;
                                            }
                                        }
                                    }
                                    if decision.apply {
                                        update_event_position(&value, &mut cursors, &mut offsets);
                                        let translated = translator.translate_envelope(&value);
                                        let terminal_refresh_error =
                                            refresh_terminal_task_output(&api, &value)
                                                .await
                                                .err();
                                        for event in translated {
                                            let _ = event_tx.send(event);
                                        }
                                        if terminal_refresh_error.is_some() {
                                            let _ = event_tx.send(AgentEvent::Log {
                                                level: "warn".into(),
                                                message: "Kimi task ended, but its final output preview could not be refreshed".into(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Ping(bytes))) => {
                            if socket.send(Message::Pong(bytes)).await.is_err() {
                                reconnect = true;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => reconnect = true,
                        Some(Ok(_)) => {}
                    }
                }
            }
        }
    }
}

fn is_event_frame(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some(
            "server_hello" | "ack" | "ping" | "pong" | "resync_required" | "terminal_output"
            | "terminal_exit",
        ) => false,
        // Kimi uses `error` for both a v1 agent event (which carries `seq`)
        // and a WS system error (whose payload carries `fatal`). Keep the
        // latter out of event-schema drift reporting without hiding a
        // malformed agent error envelope.
        Some("error")
            if value
                .get("payload")
                .and_then(|payload| payload.get("fatal"))
                .and_then(Value::as_bool)
                .is_some() =>
        {
            false
        }
        _ => true,
    }
}

fn event_belongs_to_requested_subscription(value: &Value, subscriptions: &HashSet<String>) -> bool {
    value
        .get("session_id")
        .and_then(Value::as_str)
        .is_some_and(|session_id| subscriptions.contains(session_id))
}

fn requested_resync_session<'a>(
    value: &'a Value,
    subscriptions: &HashSet<String>,
) -> Option<&'a str> {
    value
        .get("payload")
        .and_then(|payload| payload.get("session_id"))
        .and_then(Value::as_str)
        .filter(|session_id| subscriptions.contains(*session_id))
}

fn apply_offline_command(
    command: Option<WsCommand>,
    subscriptions: &mut HashSet<String>,
    pending_snapshots: &mut HashSet<String>,
) -> bool {
    match command {
        Some(WsCommand::Subscribe {
            session_id,
            snapshot_first,
        }) => {
            if snapshot_first {
                pending_snapshots.insert(session_id.clone());
            }
            subscriptions.insert(session_id);
            true
        }
        Some(WsCommand::Shutdown) | None => false,
    }
}

fn ack_resync_sessions(ack: &Value, subscriptions: &HashSet<String>) -> Vec<String> {
    ack.get("payload")
        .and_then(|payload| payload.get("resync_required"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|session_id| subscriptions.contains(*session_id))
        .map(str::to_string)
        .collect()
}

fn required_resync_sessions(
    subscriptions: &HashSet<String>,
    cursors: &HashMap<String, Cursor>,
    pending_snapshots: &HashSet<String>,
    acknowledged: Vec<String>,
) -> HashSet<String> {
    let mut required = pending_snapshots
        .intersection(subscriptions)
        .cloned()
        .collect::<HashSet<_>>();
    required.extend(
        acknowledged
            .into_iter()
            .filter(|session_id| subscriptions.contains(session_id)),
    );
    required.extend(
        subscriptions
            .iter()
            .filter(|session| !cursors.contains_key(*session))
            .cloned(),
    );
    required
}

async fn connect_and_hello(
    api: &KimiApi,
    subscriptions: &HashSet<String>,
    cursors: &HashMap<String, Cursor>,
    control_id: &mut u64,
) -> Result<(ClientWs, Vec<String>), CallerError> {
    let mut request = api
        .websocket_url()
        .into_client_request()
        .map_err(|_| external("failed to build Kimi WebSocket request"))?;
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        api.authorization_value()?,
    );
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(8),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| external("Kimi WebSocket connection timed out"))?
    .map_err(|_| external("failed to connect to Kimi WebSocket"))?;

    let hello = next_json(&mut socket, Duration::from_secs(8)).await?;
    if hello.get("type").and_then(Value::as_str) != Some("server_hello") {
        return Err(external("Kimi WebSocket omitted server_hello"));
    }
    let protocol = server_protocol_version(&hello);
    if protocol != Some(2) {
        return Err(external(format!(
            "unsupported Kimi WebSocket protocol version {} (need 2)",
            protocol
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".into())
        )));
    }

    *control_id = control_id.saturating_add(1);
    let id = format!("intendant-hello-{control_id}");
    let cursor_json = cursors_json(cursors, subscriptions.iter());
    let hello = serde_json::json!({
        "type": "client_hello",
        "id": id,
        "payload": {
            "client_id": format!("intendant-{}", std::process::id()),
            "subscriptions": subscriptions.iter().cloned().collect::<Vec<_>>(),
            "cursors": cursor_json,
        }
    });
    send_json(&mut socket, &hello).await?;
    loop {
        let ack = next_json(&mut socket, Duration::from_secs(8)).await?;
        if ack.get("type").and_then(Value::as_str) != Some("ack")
            || ack.get("id").and_then(Value::as_str) != Some(&id)
        {
            continue;
        }
        if ack.get("code").and_then(Value::as_i64) != Some(0) {
            return Err(external(format!(
                "Kimi rejected WebSocket client hello: {}",
                ack.get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            )));
        }
        let resync = ack_resync_sessions(&ack, subscriptions);
        return Ok((socket, resync));
    }
}

async fn next_json(socket: &mut ClientWs, timeout: Duration) -> Result<Value, CallerError> {
    loop {
        let frame = tokio::time::timeout(timeout, socket.next())
            .await
            .map_err(|_| external("timed out waiting for Kimi WebSocket handshake"))?
            .ok_or_else(|| external("Kimi WebSocket closed during handshake"))?
            .map_err(|_| external("Kimi WebSocket handshake failed"))?;
        match frame {
            Message::Text(text) => {
                return serde_json::from_str(&text)
                    .map_err(|_| external("Kimi WebSocket sent malformed handshake JSON"))
            }
            Message::Ping(bytes) => {
                socket
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| external("failed to answer Kimi WebSocket ping"))?;
            }
            Message::Close(_) => return Err(external("Kimi WebSocket closed during handshake")),
            _ => {}
        }
    }
}

async fn send_json(socket: &mut ClientWs, value: &Value) -> Result<(), CallerError> {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_| external("failed to write Kimi WebSocket control message"))
}

fn subscribe_message(session_id: &str, cursor: Option<&Cursor>, id: u64) -> Value {
    let mut payload = serde_json::json!({ "session_ids": [session_id] });
    if let Some(cursor) = cursor {
        payload["cursors"] = serde_json::json!({
            session_id: cursor_value(cursor)
        });
    }
    serde_json::json!({
        "type": "subscribe",
        "id": format!("intendant-subscribe-{id}"),
        "payload": payload,
    })
}

fn cursor_value(cursor: &Cursor) -> Value {
    let mut value = serde_json::json!({ "seq": cursor.seq });
    if let Some(epoch) = cursor.epoch.as_ref() {
        value["epoch"] = Value::String(epoch.clone());
    }
    value
}

fn cursors_json<'a>(
    cursors: &HashMap<String, Cursor>,
    sessions: impl Iterator<Item = &'a String>,
) -> Value {
    Value::Object(
        sessions
            .filter_map(|session| {
                cursors
                    .get(session)
                    .map(|cursor| (session.clone(), cursor_value(cursor)))
            })
            .collect(),
    )
}

struct EventDecision {
    apply: bool,
    needs_resync: bool,
    session_id: Option<String>,
}

fn event_decision(
    event: &Value,
    cursors: &HashMap<String, Cursor>,
    offsets: &HashMap<VolatileOffsetKey, usize>,
) -> EventDecision {
    let session_id = event
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(session) = session_id.as_deref() else {
        return EventDecision {
            apply: true,
            needs_resync: false,
            session_id,
        };
    };
    if event.get("volatile").and_then(Value::as_bool) == Some(true) {
        if let Some(offset) = event.get("offset").and_then(Value::as_u64) {
            let local = offsets
                .get(&volatile_offset_key(event, session))
                .copied()
                .unwrap_or(0);
            return EventDecision {
                apply: offset as usize >= local,
                needs_resync: offset as usize > local,
                session_id,
            };
        }
        return EventDecision {
            apply: true,
            needs_resync: false,
            session_id,
        };
    }

    let Some(seq) = event.get("seq").and_then(Value::as_u64) else {
        return EventDecision {
            apply: true,
            needs_resync: false,
            session_id,
        };
    };
    let epoch = event.get("epoch").and_then(Value::as_str);
    let Some(cursor) = cursors.get(session) else {
        return EventDecision {
            apply: true,
            needs_resync: seq > 1,
            session_id,
        };
    };
    if cursor.epoch.as_deref() != epoch && cursor.epoch.is_some() {
        return EventDecision {
            apply: false,
            needs_resync: true,
            session_id,
        };
    }
    EventDecision {
        apply: seq > cursor.seq,
        needs_resync: seq > cursor.seq.saturating_add(1),
        session_id,
    }
}

fn update_event_position(
    event: &Value,
    cursors: &mut HashMap<String, Cursor>,
    offsets: &mut HashMap<VolatileOffsetKey, usize>,
) {
    let Some(session) = event.get("session_id").and_then(Value::as_str) else {
        return;
    };
    // Kimi restarts assistant/thinking offsets at zero for every model step.
    if event.get("type").and_then(Value::as_str) == Some("turn.step.started") {
        reset_step_offsets(event, session, offsets);
    }
    if event.get("volatile").and_then(Value::as_bool) == Some(true) {
        let Some(offset) = event.get("offset").and_then(Value::as_u64) else {
            return;
        };
        let delta_utf16_units = event
            .get("payload")
            .and_then(|payload| payload.get("delta"))
            .and_then(Value::as_str)
            .map(utf16_len)
            .unwrap_or(0);
        offsets.insert(
            volatile_offset_key(event, session),
            offset as usize + delta_utf16_units,
        );
        return;
    }
    if let Some(seq) = event.get("seq").and_then(Value::as_u64) {
        cursors.insert(
            session.to_string(),
            Cursor {
                seq,
                epoch: event
                    .get("epoch")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
        );
    }
}

fn reset_step_offsets(
    event: &Value,
    session_id: &str,
    offsets: &mut HashMap<VolatileOffsetKey, usize>,
) {
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let agent = payload
        .get("agentId")
        .or_else(|| payload.get("agent_id"))
        .and_then(Value::as_str)
        .unwrap_or("main");
    let turn = payload
        .get("turnId")
        .or_else(|| payload.get("turn_id"))
        .map(Value::to_string)
        .unwrap_or_default();
    offsets.retain(|(session, offset_agent, kind, offset_turn), _| {
        session != session_id
            || offset_agent != agent
            || offset_turn != &turn
            || !matches!(kind.as_str(), "assistant.delta" | "thinking.delta")
    });
}

fn utf16_len(text: &str) -> usize {
    // The Kimi server derives offsets from JavaScript `String.length`.
    text.encode_utf16().count()
}

fn snapshot_suffix_after_utf16_offset(text: &str, offset: usize) -> &str {
    let mut utf16_offset = 0usize;
    for (byte_offset, character) in text.char_indices() {
        if utf16_offset == offset {
            return &text[byte_offset..];
        }
        utf16_offset += character.len_utf16();
        if utf16_offset > offset {
            return text;
        }
    }
    if utf16_offset == offset {
        ""
    } else {
        text
    }
}

fn volatile_offset_key(event: &Value, session_id: &str) -> VolatileOffsetKey {
    let payload = event.get("payload").unwrap_or(&Value::Null);
    let agent = payload
        .get("agentId")
        .or_else(|| payload.get("agent_id"))
        .and_then(Value::as_str)
        .unwrap_or("main");
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let turn = payload
        .get("turnId")
        .or_else(|| payload.get("turn_id"))
        .map(Value::to_string)
        .unwrap_or_default();
    (
        session_id.to_string(),
        agent.to_string(),
        kind.to_string(),
        turn,
    )
}

async fn resync_snapshot(
    api: &KimiApi,
    session_id: &str,
    translator: &mut EventTranslator,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    cursors: &mut HashMap<String, Cursor>,
    offsets: &mut HashMap<VolatileOffsetKey, usize>,
) -> Result<(), CallerError> {
    let snapshot = api.snapshot(session_id).await?;
    let mut translated_snapshot = snapshot.clone();
    let seq = snapshot
        .get("as_of_seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| external("Kimi snapshot omitted as_of_seq"))?;
    let epoch = snapshot
        .get("epoch")
        .and_then(Value::as_str)
        .ok_or_else(|| external("Kimi snapshot omitted epoch"))?
        .to_string();
    cursors.insert(
        session_id.to_string(),
        Cursor {
            seq,
            epoch: Some(epoch),
        },
    );
    let previous_offsets = offsets.clone();
    offsets.retain(|(session, _, _, _), _| session != session_id);
    if let Some(turn) = snapshot
        .get("in_flight_turn")
        .filter(|turn| !turn.is_null())
    {
        let turn_id = turn
            .get("turn_id")
            .or_else(|| turn.get("turnId"))
            .map(Value::to_string)
            .unwrap_or_default();
        let agent_id = turn
            .get("agent_id")
            .or_else(|| turn.get("agentId"))
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string();
        for (kind, field) in [
            ("assistant.delta", "assistant_text"),
            ("thinking.delta", "thinking_text"),
        ] {
            let text = turn.get(field).and_then(Value::as_str).unwrap_or_default();
            let count = utf16_len(text);
            let prior = previous_offsets
                .get(&(
                    session_id.to_string(),
                    agent_id.clone(),
                    kind.to_string(),
                    turn_id.clone(),
                ))
                .copied()
                .unwrap_or(0);
            let suffix = snapshot_suffix_after_utf16_offset(text, prior).to_string();
            if let Some(target) = translated_snapshot
                .get_mut("in_flight_turn")
                .and_then(Value::as_object_mut)
            {
                target.insert(field.to_string(), Value::String(suffix));
            }
            offsets.insert(
                (
                    session_id.to_string(),
                    agent_id.clone(),
                    kind.to_string(),
                    turn_id.clone(),
                ),
                count,
            );
        }
    }
    for event in translator.translate_snapshot(&translated_snapshot, session_id) {
        let _ = event_tx.send(event);
    }
    let tasks = api.list_tasks(session_id).await?;
    for event in translator.sync_tasks(&tasks, session_id) {
        let _ = event_tx.send(event);
    }
    let mut output_failures = 0usize;
    for task_id in tasks
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(task_wire_id)
        .filter(|task_id| crate::background_tasks::find_task(session_id, task_id).is_some())
    {
        if refresh_task_output(api, session_id, task_id, true)
            .await
            .is_err()
        {
            output_failures += 1;
        }
    }
    if output_failures > 0 {
        let _ = event_tx.send(AgentEvent::Log {
            level: "warn".into(),
            message: format!(
                "{output_failures} Kimi task output preview(s) could not be refreshed during resync"
            ),
        });
    }
    Ok(())
}

async fn refresh_task_output(
    api: &KimiApi,
    session_id: &str,
    task_id: &str,
    force: bool,
) -> Result<(), CallerError> {
    if !force
        && crate::background_tasks::find_task(session_id, task_id)
            .is_some_and(|record| record.inline_output.is_some())
    {
        return Ok(());
    }
    let task = api
        .task(
            session_id,
            task_id,
            crate::background_tasks::INLINE_OUTPUT_RETAINED_BYTES,
        )
        .await?;
    let output = task
        .get("output_preview")
        .and_then(Value::as_str)
        .unwrap_or_default();
    crate::background_tasks::record_inline_output(
        session_id,
        task_id,
        output.as_bytes(),
        task.get("output_bytes").and_then(Value::as_u64),
    );
    Ok(())
}

async fn refresh_terminal_task_output(api: &KimiApi, event: &Value) -> Result<(), CallerError> {
    let Some((session_id, task_id)) = terminal_task_target(event) else {
        return Ok(());
    };
    // A periodic running-task refresh may already have populated the retained
    // tail. Terminal is the authoritative last chance, so it must bypass that
    // cache before the completion event is emitted.
    refresh_task_output(api, session_id, task_id, true).await
}

async fn refresh_registered_task_outputs(api: &KimiApi, subscriptions: &HashSet<String>) {
    let targets = subscriptions
        .iter()
        .flat_map(|session_id| {
            crate::background_tasks::tasks_for_session(session_id)
                .into_iter()
                .filter(|record| {
                    record.source == "kimi"
                        && (record.status == crate::background_tasks::BackgroundTaskStatus::Running
                            || record.inline_output.is_none())
                })
                .map(|record| (session_id.clone(), record.task_id))
        })
        .collect::<Vec<_>>();
    futures_util::stream::iter(targets.into_iter().map(|(session_id, task_id)| {
        let api = api.clone();
        async move {
            let _ = refresh_task_output(&api, &session_id, &task_id, true).await;
        }
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
}

fn task_wire_id(value: &Value) -> Option<&str> {
    let info = value.get("info").unwrap_or(value);
    info.get("taskId")
        .or_else(|| info.get("task_id"))
        .or_else(|| info.get("id"))
        .and_then(Value::as_str)
}

fn terminal_task_target(value: &Value) -> Option<(&str, &str)> {
    let kind = value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str)
        .or_else(|| value.get("type").and_then(Value::as_str))?;
    if !matches!(
        kind,
        "task.terminated" | "background.task.terminated" | "event.task.completed"
    ) {
        return None;
    }
    let session_id = value.get("session_id").and_then(Value::as_str)?;
    let payload = value.get("payload").unwrap_or(value);
    Some((session_id, task_wire_id(payload)?))
}

fn server_protocol_version(hello: &Value) -> Option<u64> {
    hello
        .get("payload")
        .and_then(|payload| payload.get("protocol_version"))
        .and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;

    async fn one_response_server(
        response_body: Value,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = response_body.to_string();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buf).await.unwrap();
                assert!(read > 0, "mock client closed before request completed");
                request.extend_from_slice(&buf[..read]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn offline_snapshot_request_survives_until_reconnect() {
        let mut subscriptions = HashSet::new();
        let mut pending = HashSet::new();
        assert!(apply_offline_command(
            Some(WsCommand::Subscribe {
                session_id: "session_x".into(),
                snapshot_first: true,
            }),
            &mut subscriptions,
            &mut pending,
        ));
        assert!(subscriptions.contains("session_x"));
        assert!(pending.contains("session_x"));

        let required =
            required_resync_sessions(&subscriptions, &HashMap::new(), &pending, Vec::new());
        assert_eq!(required, HashSet::from(["session_x".to_string()]));
    }

    #[test]
    fn reconnect_resync_union_honors_acks_pending_and_missing_cursors() {
        let subscriptions = HashSet::from([
            "with_cursor".to_string(),
            "without_cursor".to_string(),
            "pending".to_string(),
            "ack-required".to_string(),
        ]);
        let cursors = HashMap::from([(
            "with_cursor".to_string(),
            Cursor {
                seq: 7,
                epoch: Some("epoch-a".into()),
            },
        )]);
        let pending = HashSet::from(["pending".to_string(), "unrequested-pending".to_string()]);
        let ack = serde_json::json!({
            "type": "ack",
            "id": "intendant-subscribe-9",
            "code": 0,
            "msg": "ok",
            "payload": {
                "accepted": ["with_cursor"],
                "not_found": [],
                "resync_required": ["ack-required", "unrequested-ack"],
                "cursors": {}
            }
        });
        assert_eq!(
            ack_resync_sessions(&ack, &subscriptions),
            vec!["ack-required".to_string()]
        );
        let required = required_resync_sessions(
            &subscriptions,
            &cursors,
            &pending,
            vec!["ack-required".to_string(), "unrequested-direct".to_string()],
        );
        assert_eq!(
            required,
            HashSet::from([
                "without_cursor".to_string(),
                "pending".to_string(),
                "ack-required".to_string()
            ])
        );
    }

    #[test]
    fn event_and_resync_frames_are_confined_to_requested_subscriptions() {
        let subscriptions = HashSet::from(["session-a".to_string()]);
        assert!(event_belongs_to_requested_subscription(
            &serde_json::json!({"session_id": "session-a"}),
            &subscriptions,
        ));
        assert!(!event_belongs_to_requested_subscription(
            &serde_json::json!({"session_id": "session-b"}),
            &subscriptions,
        ));
        assert!(!event_belongs_to_requested_subscription(
            &serde_json::json!({}),
            &subscriptions,
        ));

        let requested = serde_json::json!({
            "type": "resync_required",
            "payload": {"session_id": "session-a"}
        });
        let unrelated = serde_json::json!({
            "type": "resync_required",
            "payload": {"session_id": "session-b"}
        });
        assert_eq!(
            requested_resync_session(&requested, &subscriptions),
            Some("session-a")
        );
        assert_eq!(requested_resync_session(&unrelated, &subscriptions), None);
    }

    #[test]
    fn server_hello_reads_the_single_protocol_version_field() {
        assert_eq!(
            server_protocol_version(&serde_json::json!({
                "type": "server_hello",
                "payload": {
                    "ws_connection_id": "ws-1",
                    "protocol_version": 2
                }
            })),
            Some(2)
        );
        assert_eq!(
            server_protocol_version(&serde_json::json!({
                "type": "server_hello",
                "payload": {"protocol_version": {"protocol_version": 2}}
            })),
            None
        );
    }

    #[test]
    fn terminal_task_target_reads_v2_and_compatibility_events() {
        for kind in [
            "task.terminated",
            "background.task.terminated",
            "event.task.completed",
        ] {
            let value = serde_json::json!({
                "type": kind,
                "session_id": "session-a",
                "payload": {
                    "type": kind,
                    "info": {"taskId": "task-1"}
                }
            });
            assert_eq!(terminal_task_target(&value), Some(("session-a", "task-1")));
        }
        assert_eq!(
            terminal_task_target(&serde_json::json!({
                "type": "task.started",
                "session_id": "session-a",
                "payload": {"type": "task.started", "taskId": "task-1"}
            })),
            None
        );
    }

    #[tokio::test]
    async fn terminal_task_refresh_replaces_a_stale_retained_tail() {
        const SESSION: &str = "kimi-ws-terminal-output-refresh-test";
        const TASK: &str = "task-final";
        crate::background_tasks::clear_session(SESSION);
        crate::background_tasks::record_started_for_source(
            SESSION,
            "kimi",
            TASK,
            TASK,
            "test terminal refresh",
            1,
        );
        crate::background_tasks::record_inline_output(
            SESSION,
            TASK,
            b"stale running tail",
            Some(18),
        );

        let (origin, server) = one_response_server(serde_json::json!({
            "code": 0,
            "data": {
                "taskId": TASK,
                "output_preview": "final retained tail",
                "output_bytes": 19
            }
        }))
        .await;
        let api = KimiApi::new(origin, "test-token".into()).unwrap();
        let terminal = serde_json::json!({
            "type": "task.terminated",
            "session_id": SESSION,
            "payload": {
                "type": "task.terminated",
                "taskId": TASK,
                "status": "completed"
            }
        });

        refresh_terminal_task_output(&api, &terminal).await.unwrap();
        let record = crate::background_tasks::find_task(SESSION, TASK).unwrap();
        assert_eq!(
            record.inline_output.as_deref(),
            Some(b"final retained tail".as_slice())
        );
        assert_eq!(record.output_size_bytes, Some(19));
        let request = String::from_utf8(server.await.unwrap()).unwrap();
        assert!(request.starts_with(&format!(
            "GET /api/v1/sessions/{SESSION}/tasks/{TASK}?with_output=true&output_bytes={} HTTP/1.1",
            crate::background_tasks::INLINE_OUTPUT_RETAINED_BYTES
        )));
        crate::background_tasks::clear_session(SESSION);
    }

    #[tokio::test]
    async fn timed_out_driver_shutdown_aborts_and_awaits_the_task() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        let (started_tx, started_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let _drop_flag = DropFlag(task_dropped);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();

        await_driver_shutdown(handle, Duration::from_millis(1)).await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn protocol_watch_excludes_controls_but_keeps_malformed_event_candidates() {
        for control in [
            "server_hello",
            "ack",
            "ping",
            "pong",
            "resync_required",
            "terminal_output",
            "terminal_exit",
        ] {
            assert!(!is_event_frame(&serde_json::json!({
                "type": control,
                "payload": {}
            })));
        }
        assert!(!is_event_frame(&serde_json::json!({
            "type": "error",
            "payload": {"code": 40000, "msg": "bad control", "fatal": false}
        })));
        assert!(is_event_frame(&serde_json::json!({
            "type": "turn.started",
            "payload": {}
        })));
        assert!(!is_event_frame(&serde_json::json!({
            "type": "ack",
            "seq": 9,
            "payload": {}
        })));
        assert!(is_event_frame(&serde_json::json!({
            "type": "error",
            "payload": {"message": "agent failed"}
        })));
    }

    #[test]
    fn subscribe_carries_structured_epoch_cursor() {
        let message = subscribe_message(
            "session_x",
            Some(&Cursor {
                seq: 41,
                epoch: Some("epoch-a".into()),
            }),
            7,
        );
        assert_eq!(message["payload"]["cursors"]["session_x"]["seq"], 41);
        assert_eq!(
            message["payload"]["cursors"]["session_x"]["epoch"],
            "epoch-a"
        );
    }

    #[test]
    fn durable_duplicates_drop_and_gaps_resync() {
        let cursors = HashMap::from([(
            "session_x".to_string(),
            Cursor {
                seq: 10,
                epoch: Some("epoch-a".into()),
            },
        )]);
        let event = serde_json::json!({
            "type": "tool.result",
            "seq": 10,
            "epoch": "epoch-a",
            "session_id": "session_x",
            "payload": {}
        });
        let decision = event_decision(&event, &cursors, &HashMap::new());
        assert!(!decision.apply);
        assert!(!decision.needs_resync);

        let mut gap = event;
        gap["seq"] = Value::from(12);
        let decision = event_decision(&gap, &cursors, &HashMap::new());
        assert!(decision.apply);
        assert!(decision.needs_resync);
    }

    #[test]
    fn epoch_change_refuses_event_until_snapshot() {
        let cursors = HashMap::from([(
            "session_x".to_string(),
            Cursor {
                seq: 10,
                epoch: Some("old".into()),
            },
        )]);
        let event = serde_json::json!({
            "type": "turn.started",
            "seq": 1,
            "epoch": "new",
            "session_id": "session_x",
            "payload": {}
        });
        let decision = event_decision(&event, &cursors, &HashMap::new());
        assert!(!decision.apply);
        assert!(decision.needs_resync);
    }

    #[test]
    fn volatile_offsets_dedupe_and_detect_missed_text() {
        let key = (
            "session_x".to_string(),
            "main".to_string(),
            "assistant.delta".to_string(),
            "1".to_string(),
        );
        let offsets = HashMap::from([(key, 5usize)]);
        let duplicate = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 2,
            "session_id": "session_x",
            "payload": {"turnId": 1, "delta": "llo"}
        });
        assert!(!event_decision(&duplicate, &HashMap::new(), &offsets).apply);
        let gap = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 8,
            "session_id": "session_x",
            "payload": {"turnId": 1, "delta": "world"}
        });
        let decision = event_decision(&gap, &HashMap::new(), &offsets);
        assert!(decision.apply);
        assert!(decision.needs_resync);
    }

    #[test]
    fn volatile_offsets_reset_at_each_model_step() {
        let first_step = serde_json::json!({
            "type": "turn.step.started",
            "seq": 1,
            "epoch": "epoch-a",
            "session_id": "session_x",
            "payload": {
                "agentId": "main",
                "turnId": 7,
                "step": 1,
                "stepId": "step-1"
            }
        });
        let first_assistant = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 0,
            "session_id": "session_x",
            "payload": {
                "agentId": "main",
                "turnId": 7,
                "delta": "first"
            }
        });
        let first_thinking = serde_json::json!({
            "type": "thinking.delta",
            "volatile": true,
            "offset": 0,
            "session_id": "session_x",
            "payload": {
                "agentId": "main",
                "turnId": 7,
                "delta": "ponder"
            }
        });
        let second_step = serde_json::json!({
            "type": "turn.step.started",
            "seq": 2,
            "epoch": "epoch-a",
            "session_id": "session_x",
            "payload": {
                "agentId": "main",
                "turnId": 7,
                "step": 2,
                "stepId": "step-2"
            }
        });
        let second_assistant = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 0,
            "session_id": "session_x",
            "payload": {
                "agentId": "main",
                "turnId": 7,
                "delta": "second"
            }
        });
        let second_thinking = serde_json::json!({
            "type": "thinking.delta",
            "volatile": true,
            "offset": 0,
            "session_id": "session_x",
            "payload": {
                "agentId": "main",
                "turnId": 7,
                "delta": "again"
            }
        });

        let mut cursors = HashMap::new();
        let mut offsets = HashMap::new();
        update_event_position(&first_step, &mut cursors, &mut offsets);
        update_event_position(&first_assistant, &mut cursors, &mut offsets);
        update_event_position(&first_thinking, &mut cursors, &mut offsets);
        assert!(!event_decision(&second_assistant, &cursors, &offsets).apply);
        assert!(!event_decision(&second_thinking, &cursors, &offsets).apply);

        update_event_position(&second_step, &mut cursors, &mut offsets);
        for delta in [&second_assistant, &second_thinking] {
            let decision = event_decision(delta, &cursors, &offsets);
            assert!(decision.apply);
            assert!(!decision.needs_resync);
        }
    }

    #[test]
    fn volatile_offsets_use_javascript_utf16_length_for_emoji() {
        let emoji = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 0,
            "session_id": "session_x",
            "payload": {"turnId": 1, "delta": "😀"}
        });
        let continuation = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 2,
            "session_id": "session_x",
            "payload": {"turnId": 1, "delta": "done"}
        });
        let mut cursors = HashMap::new();
        let mut offsets = HashMap::new();
        update_event_position(&emoji, &mut cursors, &mut offsets);

        assert_eq!(
            offsets.get(&(
                "session_x".into(),
                "main".into(),
                "assistant.delta".into(),
                "1".into(),
            )),
            Some(&2)
        );
        let decision = event_decision(&continuation, &cursors, &offsets);
        assert!(decision.apply);
        assert!(!decision.needs_resync);
    }

    #[test]
    fn snapshot_suffix_uses_utf16_boundaries_for_emoji() {
        assert_eq!(utf16_len("a😀tail"), 7);
        assert_eq!(snapshot_suffix_after_utf16_offset("a😀tail", 3), "tail");
        assert_eq!(snapshot_suffix_after_utf16_offset("a😀tail", 7), "");
        assert_eq!(snapshot_suffix_after_utf16_offset("a😀tail", 2), "a😀tail");
        assert_eq!(snapshot_suffix_after_utf16_offset("a😀tail", 8), "a😀tail");
    }

    #[test]
    fn volatile_offsets_are_independent_for_agents_with_the_same_turn_id() {
        let main = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 0,
            "session_id": "session_x",
            "payload": {
                "agentId": "main",
                "turnId": 1,
                "delta": "parent"
            }
        });
        let child = serde_json::json!({
            "type": "assistant.delta",
            "volatile": true,
            "offset": 0,
            "session_id": "session_x",
            "payload": {
                "agentId": "child-1",
                "turnId": 1,
                "delta": "child"
            }
        });
        let mut cursors = HashMap::new();
        let mut offsets = HashMap::new();
        update_event_position(&main, &mut cursors, &mut offsets);

        let child_decision = event_decision(&child, &cursors, &offsets);
        assert!(child_decision.apply);
        assert!(!child_decision.needs_resync);
        update_event_position(&child, &mut cursors, &mut offsets);

        assert_eq!(offsets.len(), 2);
        assert_eq!(
            offsets.get(&(
                "session_x".into(),
                "main".into(),
                "assistant.delta".into(),
                "1".into(),
            )),
            Some(&6)
        );
        assert_eq!(
            offsets.get(&(
                "session_x".into(),
                "child-1".into(),
                "assistant.delta".into(),
                "1".into(),
            )),
            Some(&5)
        );
    }
}
