use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::types::OutboundEvent;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::broadcast;

/// Get the socket path for this process.
pub fn socket_path() -> PathBuf {
    PathBuf::from(format!("/tmp/intendant-{}.sock", std::process::id()))
}

/// Spawn the Unix control socket server.
/// Returns a broadcast sender for pushing events to connected clients.
///
/// The control socket is a Unix-domain socket (`--control-socket`). On
/// non-Unix targets (Windows) there is no `UnixListener`, so this is a
/// no-op: it returns the same `(JoinHandle, Sender)` shape with a task
/// that exits immediately. A Tier-1 Windows port could back this with a
/// named pipe or a localhost TCP socket.
#[cfg(unix)]
pub fn spawn_control_server(
    bus: EventBus,
) -> (tokio::task::JoinHandle<()>, broadcast::Sender<String>) {
    let (outbound_tx, _) = broadcast::channel::<String>(256);
    let outbound_tx_clone = outbound_tx.clone();

    let path = socket_path();
    // Clean up stale socket
    let _ = std::fs::remove_file(&path);

    let handle = tokio::spawn(async move {
        let mut listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Control socket bind failed: {}", e);
                return;
            }
        };
        #[cfg(unix)]
        {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    #[cfg(unix)]
                    {
                        let current_uid = super::platform::current_uid();
                        if let Ok(cred) = stream.peer_cred() {
                            if cred.uid() != current_uid {
                                continue;
                            }
                        }
                    }

                    let bus = bus.clone();
                    let mut outbound_rx = outbound_tx_clone.subscribe();

                    tokio::spawn(async move {
                        let (reader, mut writer) = stream.into_split();
                        let mut reader = BufReader::new(reader);

                        // Per-client channel for sending error responses from reader to writer
                        let (error_tx, mut error_rx) =
                            tokio::sync::mpsc::unbounded_channel::<String>();

                        // Read inbound commands in one task
                        let bus_inbound = bus.clone();
                        let inbound = tokio::spawn(async move {
                            let mut line = String::new();
                            loop {
                                line.clear();
                                match reader.read_line(&mut line).await {
                                    Ok(0) => break, // EOF
                                    Ok(_) => {
                                        let trimmed = line.trim();
                                        if !trimmed.is_empty() {
                                            match serde_json::from_str::<ControlMsg>(trimmed) {
                                                Ok(msg) => {
                                                    bus_inbound.send(AppEvent::ControlCommand(msg));
                                                }
                                                Err(e) => {
                                                    let err_json = serde_json::json!({
                                                        "event": "error",
                                                        "ok": false,
                                                        "message": format!("Invalid message: {}", e),
                                                    });
                                                    let _ = error_tx.send(err_json.to_string());
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        });

                        // Write outbound events and error responses in another task
                        let outbound = tokio::spawn(async move {
                            loop {
                                tokio::select! {
                                    result = outbound_rx.recv() => {
                                        match result {
                                            Ok(line) => {
                                                let mut data = line.into_bytes();
                                                data.push(b'\n');
                                                if writer.write_all(&data).await.is_err() {
                                                    break;
                                                }
                                            }
                                            Err(broadcast::error::RecvError::Closed) => break,
                                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                        }
                                    }
                                    Some(err_line) = error_rx.recv() => {
                                        let mut data = err_line.into_bytes();
                                        data.push(b'\n');
                                        if writer.write_all(&data).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        });

                        let _ = tokio::join!(inbound, outbound);
                    });
                }
                Err(e) => {
                    if crate::web_gateway::should_continue_after_accept_error(&e) {
                        eprintln!("Control socket accept failed: {e} (continuing)");
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                    // The listening socket itself is dead (same fd-invalidation
                    // class the web gateway recovers from): re-listen on the
                    // same path instead of silently dropping the control
                    // socket for the rest of the daemon's life.
                    eprintln!(
                        "Control socket accept failed: {e} (rebinding {})",
                        path.display()
                    );
                    let mut delay = std::time::Duration::from_millis(250);
                    listener = loop {
                        tokio::time::sleep(delay).await;
                        let _ = std::fs::remove_file(&path);
                        match UnixListener::bind(&path) {
                            Ok(fresh) => {
                                let _ = std::fs::set_permissions(
                                    &path,
                                    std::fs::Permissions::from_mode(0o600),
                                );
                                eprintln!("Control socket rebound at {}", path.display());
                                break fresh;
                            }
                            Err(err) => {
                                delay = (delay * 2).min(std::time::Duration::from_secs(30));
                                eprintln!(
                                    "Control socket rebind failed: {err} (retrying in {:.1}s)",
                                    delay.as_secs_f32()
                                );
                            }
                        }
                    };
                }
            }
        }
    });

    (handle, outbound_tx)
}

/// Non-Unix stub: no Unix-domain control socket. Returns an
/// immediately-completing task and a live (but unused) broadcast sender
/// so callers behind `--control-socket` keep the same shape.
#[cfg(not(unix))]
pub fn spawn_control_server(
    _bus: EventBus,
) -> (tokio::task::JoinHandle<()>, broadcast::Sender<String>) {
    let (outbound_tx, _) = broadcast::channel::<String>(256);
    eprintln!("Control socket is not available on this platform (Unix-only)");
    let handle = tokio::spawn(async {});
    (handle, outbound_tx)
}

/// Clean up the socket file.
pub fn cleanup() {
    let _ = std::fs::remove_file(socket_path());
}

/// Broadcast an outbound event to all connected clients.
#[allow(dead_code)]
pub fn broadcast_event(tx: &broadcast::Sender<String>, event: &OutboundEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        let _ = tx.send(json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_contains_pid() {
        let path = socket_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.starts_with("/tmp/intendant-"));
        assert!(path_str.ends_with(".sock"));
        assert!(path_str.contains(&std::process::id().to_string()));
    }

    #[test]
    fn outbound_event_turn_started_serialize() {
        let event = OutboundEvent::TurnStarted {
            session_id: None,
            turn: 5,
            budget_pct: 12.3,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"turn_started\""));
        assert!(json.contains("\"turn\":5"));
    }

    #[test]
    fn outbound_event_agent_output_serialize() {
        let event = OutboundEvent::AgentOutput {
            session_id: None,
            stdout: "hello".to_string(),
            stderr: "".to_string(),
            source: None,
            output_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"agent_output\""));
    }

    #[test]
    fn outbound_event_approval_required_serialize() {
        let event = OutboundEvent::ApprovalRequired {
            session_id: None,
            id: 42,
            command: "rm -rf /tmp".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"approval_required\""));
        assert!(json.contains("\"id\":42"));
    }

    #[test]
    fn outbound_event_ask_human_serialize() {
        let event = OutboundEvent::AskHuman {
            question: "Which database?".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"ask_human\""));
    }

    #[test]
    fn outbound_event_task_complete_serialize() {
        let event = OutboundEvent::TaskComplete {
            session_id: None,
            reason: "done signal".to_string(),
            summary: Some("files listed".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_complete\""));
    }

    #[test]
    fn outbound_event_status_serialize() {
        let event = OutboundEvent::Status {
            turn: 3,
            phase: "thinking".to_string(),
            autonomy: "medium".to_string(),
            session_id: "abc-123".to_string(),
            task: "list files".to_string(),
            external_agent: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"status\""));
        assert!(json.contains("\"turn\":3"));
        assert!(json.contains("\"session_id\":\"abc-123\""));
        assert!(json.contains("\"task\":\"list files\""));
    }

    #[test]
    fn outbound_event_command_result_serialize() {
        let event = OutboundEvent::CommandResult {
            action: "get_restart_status".to_string(),
            ok: true,
            message: "ok".to_string(),
            data: Some(serde_json::json!({"phase":"ready"})),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"command_result\""));
        assert!(json.contains("\"action\":\"get_restart_status\""));
    }

    #[test]
    fn broadcast_event_to_sender() {
        let (tx, mut rx) = broadcast::channel::<String>(16);
        let event = OutboundEvent::TurnStarted {
            session_id: None,
            turn: 1,
            budget_pct: 5.0,
        };
        broadcast_event(&tx, &event);
        let received = rx.try_recv().unwrap();
        assert!(received.contains("turn_started"));
    }

    #[tokio::test]
    async fn control_server_lifecycle() {
        let bus = EventBus::new();
        let (handle, _tx) = spawn_control_server(bus);

        // In restricted sandboxes Unix socket bind can be blocked.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let path = socket_path();
        if !path.exists() {
            handle.abort();
            cleanup();
            return;
        }

        // Cleanup
        handle.abort();
        cleanup();
        assert!(!path.exists());
    }
}
