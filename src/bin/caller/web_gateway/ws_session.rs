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
                                        let display_ids: Vec<u32> = match session_registry.as_ref() {
                            Some(sr) => sr.read().await.display_ids(),
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
