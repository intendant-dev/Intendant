//! Client-egress relays — credential custody, rollout step 5.
//!
//! The other half of custody: instead of leasing a credential to the
//! daemon, a browser session that holds `credentials.manage` registers
//! as an **egress relay** for provider kinds. The daemon then ships each
//! provider request — auth-less — over the E2E tunnel; the browser
//! attaches the credential from the unlocked vault, performs the fetch
//! against the provider's fixed origin, and streams the response body
//! back chunk by chunk under a credit window. The credential never
//! leaves the browser, and the capability vanishes the moment the tab
//! detaches. Leases stay the default (daemon-direct egress); this mode
//! covers the maximally cautious and the try-before-fueling flow.
//!
//! Frames (all ride the dashboard-control channel):
//!   daemon → browser: `egress_request` (head + initial credit; plus the
//!     request-side window when the relay declared `request_credits`),
//!     `egress_request_chunk`*, `egress_request_end`, `egress_ack`
//!     (credit refill), `egress_cancel`
//!   browser → daemon: `egress_response` (status), `egress_chunk`*,
//!     `egress_end`, `egress_error`, `egress_request_ack` (request-side
//!     credit refill) — gated by `credentials.manage` and bound to the
//!     registering session.

use base64::Engine as _;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const KIND_ANTHROPIC: &str = "api_key:anthropic";
pub const KIND_GEMINI: &str = "api_key:gemini";
/// Kinds a browser may relay. OpenAI is structurally excluded (its
/// completions API refuses browser CORS), as are the external agents
/// (local child processes by nature).
pub const RELAY_KINDS: &[&str] = &[KIND_ANTHROPIC, KIND_GEMINI];

/// Mirror of the control byte-stream chunk size.
const REQUEST_CHUNK_BYTES: usize = 16 * 1024;
/// Refuse browser chunks bigger than this (the relay is told to slice).
const MAX_RESPONSE_CHUNK_BYTES: usize = 64 * 1024;
/// Bytes the relay may send before waiting for `egress_ack` refills.
pub const BODY_CREDIT_WINDOW_BYTES: u64 = 1024 * 1024;
/// Bytes of request body the daemon may put in flight before waiting for
/// `egress_request_ack` refills — the request-direction mirror of
/// `BODY_CREDIT_WINDOW_BYTES`. Without it a multi-MB provider request
/// (an image-heavy conversation) base64-balloons ~4/3× into the relay's
/// unbounded frame queue all at once, per request; with it the queue
/// holds at most one window of chunks. Engaged only for relays that
/// declared `request_credits` at registration — a relay page that would
/// never ack gets the legacy push-all instead of a wedged daemon.
pub const REQUEST_CREDIT_WINDOW_BYTES: u64 = 1024 * 1024;
/// Max silence between `egress_request_ack` refills, once the request
/// window is exhausted, before the request is failed. The request-side
/// `BODY_IDLE_TIMEOUT` analog, but tighter: acks track chunk frames
/// draining off the tunnel into the relay page — pure transport
/// progress, with no provider call in the loop yet (the browser fetch
/// starts only at `egress_request_end`) — so a minute of ack silence
/// means the page is gone or wedged, not a model thinking.
const REQUEST_ACK_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Body channel capacity in chunks. Kept above the credit window so an
/// honest relay can never observe a full channel; hitting it means the
/// relay ignored its window and the request is killed.
const BODY_CHANNEL_CHUNKS: usize = 96;
/// How long the relay has to produce the response head (the provider's
/// status line — arrives well before any model thinking completes).
const HEAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Max silence between body chunks before the request is failed.
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone)]
struct RelayEntry {
    session_id: String,
    label: String,
    frames_tx: mpsc::UnboundedSender<serde_json::Value>,
    since_unix_ms: u64,
    /// Relay-attach handshake capability: the page acks request chunks
    /// (`egress_request_ack`) as it consumes them, so the daemon may
    /// window the request send. Absent (an old/cached relay page whose
    /// registration never declared it) means the legacy push-all path.
    request_credits: bool,
}

struct PendingEgress {
    session_id: String,
    frames_tx: mpsc::UnboundedSender<serde_json::Value>,
    head_tx: Option<oneshot::Sender<Result<u16, String>>>,
    body_tx: mpsc::Sender<Result<Vec<u8>, String>>,
    /// Request-side credit refills (`egress_request_ack` bytes) back to
    /// the windowed sender in `fetch`. Dropped with the entry, which is
    /// how a parked sender learns the request died mid-ship.
    request_ack_tx: mpsc::UnboundedSender<u64>,
}

fn relays() -> &'static RwLock<HashMap<String, RelayEntry>> {
    static RELAYS: OnceLock<RwLock<HashMap<String, RelayEntry>>> = OnceLock::new();
    RELAYS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn pending() -> &'static RwLock<HashMap<String, PendingEgress>> {
    static PENDING: OnceLock<RwLock<HashMap<String, PendingEgress>>> = OnceLock::new();
    PENDING.get_or_init(|| RwLock::new(HashMap::new()))
}

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Register (or refresh) a session as the relay for the given kinds.
/// One relay per kind — the latest registration wins, mirroring leases.
/// `request_credits` is the page's capability declaration that it acks
/// request chunks (see `REQUEST_CREDIT_WINDOW_BYTES`).
pub fn register(
    session_id: &str,
    label: &str,
    via: &str,
    kinds: &[String],
    request_credits: bool,
    frames_tx: mpsc::UnboundedSender<serde_json::Value>,
) -> Result<Vec<String>, String> {
    let mut accepted: Vec<String> = kinds
        .iter()
        .map(|kind| kind.trim().to_string())
        .filter(|kind| RELAY_KINDS.contains(&kind.as_str()))
        .collect();
    accepted.sort_unstable();
    accepted.dedup();
    if accepted.is_empty() {
        return Err(format!(
            "no relayable credential kinds (supported: {})",
            RELAY_KINDS.join(", ")
        ));
    }
    let now = now_unix_ms();
    {
        let mut relays = relays().write().expect("egress relays poisoned");
        for kind in &accepted {
            relays.insert(
                kind.clone(),
                RelayEntry {
                    session_id: session_id.to_string(),
                    label: label.trim().to_string(),
                    frames_tx: frames_tx.clone(),
                    since_unix_ms: now,
                    request_credits,
                },
            );
        }
    }
    for kind in &accepted {
        crate::credential_audit::record_with_origin(
            crate::credential_audit::EVENT_EGRESS_REGISTERED,
            kind,
            label.trim(),
            label.trim(),
            via,
            format!("provider calls relay through browser session {session_id}"),
        );
    }
    Ok(accepted)
}

/// Unregister a session's relays — the given kinds, or all of them.
/// In-flight requests are left to finish; the browser keeps streaming
/// responses it already started.
pub fn unregister(session_id: &str, kinds: Option<&[String]>) -> usize {
    let mut dropped: Vec<(String, String)> = Vec::new();
    {
        let mut relays = relays().write().expect("egress relays poisoned");
        relays.retain(|kind, entry| {
            if entry.session_id != session_id {
                return true;
            }
            let keep = match kinds {
                Some(kinds) => !kinds.iter().any(|k| k.trim() == kind),
                None => false,
            };
            if !keep {
                dropped.push((kind.clone(), entry.label.clone()));
            }
            keep
        });
    }
    for (kind, label) in &dropped {
        crate::credential_audit::record(
            crate::credential_audit::EVENT_EGRESS_UNREGISTERED,
            kind,
            label,
            label,
            format!("browser relay detached (session {session_id})"),
        );
    }
    dropped.len()
}

/// Session teardown: drop its relays and fail its in-flight requests —
/// no more frames can ever arrive from it.
pub fn unregister_session(session_id: &str) -> usize {
    let removed = unregister(session_id, None);
    let stranded: Vec<String> = pending()
        .read()
        .expect("egress pending poisoned")
        .iter()
        .filter(|(_, entry)| entry.session_id == session_id)
        .map(|(id, _)| id.clone())
        .collect();
    for id in stranded {
        fail_pending(&id, "egress relay session detached".to_string());
    }
    removed
}

pub fn available(kind: &str) -> bool {
    relays()
        .read()
        .expect("egress relays poisoned")
        .get(kind)
        .map(|entry| !entry.frames_tx.is_closed())
        .unwrap_or(false)
}

pub struct RelayStatusEntry {
    pub kind: String,
    pub label: String,
    pub session_id: String,
    pub since_unix_ms: u64,
}

pub fn relay_status() -> Vec<RelayStatusEntry> {
    let mut entries: Vec<RelayStatusEntry> = relays()
        .read()
        .expect("egress relays poisoned")
        .iter()
        .filter(|(_, entry)| !entry.frames_tx.is_closed())
        .map(|(kind, entry)| RelayStatusEntry {
            kind: kind.clone(),
            label: entry.label.clone(),
            session_id: entry.session_id.clone(),
            since_unix_ms: entry.since_unix_ms,
        })
        .collect();
    entries.sort_by(|a, b| a.kind.cmp(&b.kind));
    entries
}

/// Fail a pending request with a reason, signalling whichever side the
/// consumer is currently waiting on, and tell the relay to abort.
fn fail_pending(id: &str, reason: String) {
    let Some(mut entry) = pending()
        .write()
        .expect("egress pending poisoned")
        .remove(id)
    else {
        return;
    };
    let _ = entry
        .frames_tx
        .send(serde_json::json!({ "t": "egress_cancel", "id": id }));
    if let Some(head_tx) = entry.head_tx.take() {
        let _ = head_tx.send(Err(reason));
    } else {
        let body_tx = entry.body_tx.clone();
        tokio::spawn(async move {
            let _ = body_tx.send(Err(reason)).await;
        });
    }
}

/// Why the request-ship phase of `fetch` stopped early.
enum RequestShipError {
    /// The relay's frame channel closed mid-send (transport torn down).
    ChannelClosed,
    /// A windowed send saw no `egress_request_ack` refill within
    /// `REQUEST_ACK_IDLE_TIMEOUT`.
    Stalled,
    /// The pending entry vanished mid-ship — the relay detached or sent
    /// `egress_error` — and the concrete reason is already queued on the
    /// head channel.
    Aborted,
}

/// Send the request body under the request credit window: chunks go out
/// only while window bytes remain, and the window refills from the
/// relay's `egress_request_ack` frames as it consumes chunks. This
/// bounds the daemon-side frame queue at one window per request no
/// matter how large the provider request is.
async fn ship_request_windowed(
    frames_tx: &mpsc::UnboundedSender<serde_json::Value>,
    id: &str,
    body: &[u8],
    ack_rx: &mut mpsc::UnboundedReceiver<u64>,
) -> Result<(), RequestShipError> {
    let mut credit = REQUEST_CREDIT_WINDOW_BYTES;
    for chunk in body.chunks(REQUEST_CHUNK_BYTES) {
        while credit < chunk.len() as u64 {
            match tokio::time::timeout(REQUEST_ACK_IDLE_TIMEOUT, ack_rx.recv()).await {
                Err(_) => return Err(RequestShipError::Stalled),
                Ok(None) => return Err(RequestShipError::Aborted),
                Ok(Some(bytes)) => credit = credit.saturating_add(bytes),
            }
        }
        frames_tx
            .send(serde_json::json!({ "t": "egress_request_chunk", "id": id, "data": b64(chunk) }))
            .map_err(|_| RequestShipError::ChannelClosed)?;
        credit -= chunk.len() as u64;
    }
    frames_tx
        .send(serde_json::json!({ "t": "egress_request_end", "id": id }))
        .map_err(|_| RequestShipError::ChannelClosed)?;
    Ok(())
}

/// Ship one provider request through the relay for `kind`. `headers`
/// must not contain credentials — the browser attaches those. Returns
/// once the response status line arrives; the body streams after.
pub async fn fetch(
    kind: &str,
    method: &str,
    url: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<EgressResponse, String> {
    let relay = relays()
        .read()
        .expect("egress relays poisoned")
        .get(kind)
        .cloned();
    let Some(relay) = relay else {
        return Err(format!(
            "no client-egress relay attached for {kind} — open a fueling session and enable relaying, or grant a lease"
        ));
    };
    let id = format!("egr_{}", uuid::Uuid::new_v4().simple());
    let (head_tx, head_rx) = oneshot::channel();
    let (body_tx, body_rx) = mpsc::channel(BODY_CHANNEL_CHUNKS);
    let (request_ack_tx, mut request_ack_rx) = mpsc::unbounded_channel();
    pending().write().expect("egress pending poisoned").insert(
        id.clone(),
        PendingEgress {
            session_id: relay.session_id.clone(),
            frames_tx: relay.frames_tx.clone(),
            head_tx: Some(head_tx),
            body_tx,
            request_ack_tx,
        },
    );

    let header_pairs: Vec<[&str; 2]> = headers
        .iter()
        .map(|(name, value)| [name.as_str(), value.as_str()])
        .collect();
    let mut head_frame = serde_json::json!({
        "t": "egress_request",
        "id": id,
        "kind": kind,
        "method": method,
        "url": url,
        "headers": header_pairs,
        "body_len": body.len(),
        "credit": BODY_CREDIT_WINDOW_BYTES,
    });
    if relay.request_credits {
        // Announce the request-side window so the relay page knows both
        // ends opted into the ack protocol (a `request_credits` page
        // talking to a daemon that never sends this field stays silent).
        head_frame["request_credit"] = serde_json::Value::from(REQUEST_CREDIT_WINDOW_BYTES);
    }
    let shipped = if relay.frames_tx.send(head_frame).is_err() {
        Err(RequestShipError::ChannelClosed)
    } else if relay.request_credits {
        ship_request_windowed(&relay.frames_tx, &id, &body, &mut request_ack_rx).await
    } else {
        // Legacy relay page (registered without `request_credits`): push
        // the whole request immediately — the pre-credit wire behavior
        // the page expects, since it will never send an ack.
        let send = |frame: serde_json::Value| relay.frames_tx.send(frame).map_err(|_| ());
        let pushed = body.chunks(REQUEST_CHUNK_BYTES).all(|chunk| {
            send(serde_json::json!({ "t": "egress_request_chunk", "id": id, "data": b64(chunk) }))
                .is_ok()
        }) && send(serde_json::json!({ "t": "egress_request_end", "id": id })).is_ok();
        if pushed {
            Ok(())
        } else {
            Err(RequestShipError::ChannelClosed)
        }
    };
    match shipped {
        Ok(()) => {}
        Err(RequestShipError::ChannelClosed) => {
            pending()
                .write()
                .expect("egress pending poisoned")
                .remove(&id);
            return Err("egress relay channel closed while sending the request".to_string());
        }
        Err(RequestShipError::Stalled) => {
            let reason = format!(
                "egress relay stalled consuming the request for {}s",
                REQUEST_ACK_IDLE_TIMEOUT.as_secs()
            );
            fail_pending(&id, reason.clone());
            return Err(reason);
        }
        Err(RequestShipError::Aborted) => {
            // Fall through to the head await: `fail_pending` already
            // queued the real reason (detach, relay error) there.
        }
    }

    match tokio::time::timeout(HEAD_TIMEOUT, head_rx).await {
        Err(_) => {
            fail_pending(&id, "timed out".to_string());
            Err(format!(
                "egress relay did not answer within {}s — is the vault still unlocked in that tab?",
                HEAD_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(_)) => Err("egress request aborted".to_string()),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Ok(Ok(status))) => Ok(EgressResponse {
            status,
            request_id: id,
            frames_tx: relay.frames_tx,
            body_rx,
            finished: false,
        }),
    }
}

/// A relayed provider response: the status plus a credit-acked body
/// stream. Dropping it mid-body cancels the browser-side fetch.
pub struct EgressResponse {
    pub status: u16,
    request_id: String,
    frames_tx: mpsc::UnboundedSender<serde_json::Value>,
    body_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    finished: bool,
}

impl EgressResponse {
    pub fn status_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
        if self.finished {
            return None;
        }
        match tokio::time::timeout(BODY_IDLE_TIMEOUT, self.body_rx.recv()).await {
            Err(_) => {
                self.finished = true;
                Some(Err(format!(
                    "egress body stalled for {}s",
                    BODY_IDLE_TIMEOUT.as_secs()
                )))
            }
            Ok(None) => {
                self.finished = true;
                None
            }
            Ok(Some(Ok(chunk))) => {
                // Refill the relay's credit window as the consumer drains.
                let _ = self.frames_tx.send(serde_json::json!({
                    "t": "egress_ack",
                    "id": self.request_id,
                    "bytes": chunk.len(),
                }));
                Some(Ok(chunk))
            }
            Ok(Some(Err(error))) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }

    /// Collect the whole body as text (error bodies, non-stream JSON).
    pub async fn body_text(mut self) -> Result<String, String> {
        let mut collected = Vec::new();
        while let Some(chunk) = self.next_chunk().await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8_lossy(&collected).into_owned())
    }

    /// The body as a chunk stream — the shape the SSE parsers consume.
    pub fn bytes_stream(self) -> impl futures_util::Stream<Item = Result<Vec<u8>, String>> + Send {
        futures_util::stream::unfold(self, |mut response| async move {
            response.next_chunk().await.map(|item| (item, response))
        })
    }
}

impl Drop for EgressResponse {
    fn drop(&mut self) {
        // Consumer went away mid-body (or the head was never consumed):
        // abort the browser-side fetch. A finished request has already
        // been removed from pending by its terminal frame.
        if self.pending_remove().is_some() {
            let _ = self.frames_tx.send(serde_json::json!({
                "t": "egress_cancel",
                "id": self.request_id,
            }));
        }
    }
}

impl EgressResponse {
    fn pending_remove(&self) -> Option<()> {
        pending()
            .write()
            .expect("egress pending poisoned")
            .remove(&self.request_id)
            .map(|_| ())
    }
}

/// Handle a browser→daemon egress frame. `session_id` is the sending
/// session; frames for requests it does not own are ignored, so no
/// session can spoof another relay's response.
pub fn handle_browser_frame(session_id: &str, t: &str, frame: &serde_json::Value) {
    let Some(id) = frame.get("id").and_then(|v| v.as_str()) else {
        return;
    };
    match t {
        "egress_response" => {
            let status = frame.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let mut pend = pending().write().expect("egress pending poisoned");
            let Some(entry) = pend.get_mut(id) else {
                return;
            };
            if entry.session_id != session_id {
                return;
            }
            if let Some(head_tx) = entry.head_tx.take() {
                let _ = head_tx.send(if status == 0 {
                    Err("egress relay sent a malformed response head".to_string())
                } else {
                    Ok(status)
                });
            }
        }
        "egress_chunk" => {
            let data = frame.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let decoded = base64::engine::general_purpose::STANDARD.decode(data);
            let body_tx = {
                let pend = pending().read().expect("egress pending poisoned");
                let Some(entry) = pend.get(id) else { return };
                if entry.session_id != session_id {
                    return;
                }
                entry.body_tx.clone()
            };
            let chunk = match decoded {
                Ok(chunk) if chunk.len() <= MAX_RESPONSE_CHUNK_BYTES => chunk,
                Ok(_) => {
                    fail_pending(id, "egress relay sent an oversized chunk".to_string());
                    return;
                }
                Err(_) => {
                    fail_pending(id, "egress relay sent a malformed chunk".to_string());
                    return;
                }
            };
            match body_tx.try_send(Ok(chunk)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // The relay ignored its credit window; fail closed.
                    fail_pending(id, "egress relay exceeded its credit window".to_string());
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Consumer already dropped; Drop sent the cancel.
                    pending()
                        .write()
                        .expect("egress pending poisoned")
                        .remove(id);
                }
            }
        }
        "egress_end" => {
            let mut pend = pending().write().expect("egress pending poisoned");
            if pend.get(id).map(|e| e.session_id.as_str()) == Some(session_id) {
                // Dropping the entry drops body_tx — the consumer sees EOF.
                pend.remove(id);
            }
        }
        "egress_error" => {
            let owned = pending()
                .read()
                .expect("egress pending poisoned")
                .get(id)
                .map(|e| e.session_id == session_id)
                .unwrap_or(false);
            if owned {
                let reason = frame
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("egress relay error")
                    .to_string();
                fail_pending(id, format!("egress relay: {reason}"));
            }
        }
        "egress_request_ack" => {
            // Request-side credit refill: the relay page consumed this
            // many raw request-body bytes off the tunnel. Harmless for a
            // finished or legacy request — the sender's receiver is gone
            // and the send fails silently.
            let bytes = frame.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
            let pend = pending().read().expect("egress pending poisoned");
            let Some(entry) = pend.get(id) else { return };
            if entry.session_id != session_id {
                return;
            }
            let _ = entry.request_ack_tx.send(bytes);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset() {
        relays().write().unwrap().clear();
        pending().write().unwrap().clear();
    }

    fn kinds(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn register_filters_kinds_and_replaces() {
        let _guard = lock();
        reset();
        let (tx, _rx) = mpsc::unbounded_channel();
        let accepted = register(
            "s1",
            "Tab A",
            "local",
            &kinds(&[KIND_ANTHROPIC, "oauth:codex", "junk"]),
            false,
            tx,
        )
        .unwrap();
        assert_eq!(accepted, vec![KIND_ANTHROPIC.to_string()]);
        assert!(available(KIND_ANTHROPIC));
        assert!(!available(KIND_GEMINI));
        assert!(register("s1", "Tab A", "local", &kinds(&["junk"]), false, {
            let (tx, _rx) = mpsc::unbounded_channel();
            tx
        })
        .is_err());

        // A later session takes the kind over.
        let (tx2, _rx2) = mpsc::unbounded_channel();
        register("s2", "Tab B", "local", &kinds(&[KIND_ANTHROPIC]), false, tx2).unwrap();
        let status = relay_status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].session_id, "s2");

        assert_eq!(unregister_session("s2"), 1);
        assert!(!available(KIND_ANTHROPIC));
        reset();
    }

    #[tokio::test]
    async fn fetch_round_trips_body_with_acks() {
        let _guard = lock();
        reset();
        let (tx, mut relay_rx) = mpsc::unbounded_channel();
        register("s1", "Tab A", "local", &kinds(&[KIND_GEMINI]), false, tx).unwrap();

        let fetch_task = tokio::spawn(fetch(
            KIND_GEMINI,
            "POST",
            "https://generativelanguage.googleapis.com/v1beta/models/m:generateContent",
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"contents\":[]}".to_vec(),
        ));

        // Relay side: read head + body chunks + end.
        let head = relay_rx.recv().await.unwrap();
        assert_eq!(head["t"], "egress_request");
        let id = head["id"].as_str().unwrap().to_string();
        assert_eq!(head["kind"], KIND_GEMINI);
        assert_eq!(head["body_len"], 15);
        let chunk = relay_rx.recv().await.unwrap();
        assert_eq!(chunk["t"], "egress_request_chunk");
        let end = relay_rx.recv().await.unwrap();
        assert_eq!(end["t"], "egress_request_end");

        // Respond: 200 + two chunks + end.
        handle_browser_frame(
            "s1",
            "egress_response",
            &serde_json::json!({"id": id, "status": 200}),
        );
        let response = fetch_task.await.unwrap().unwrap();
        assert!(response.status_success());

        handle_browser_frame(
            "s1",
            "egress_chunk",
            &serde_json::json!({"id": id, "data": b64(b"hello ")}),
        );
        handle_browser_frame(
            "s1",
            "egress_chunk",
            &serde_json::json!({"id": id, "data": b64(b"world")}),
        );
        handle_browser_frame("s1", "egress_end", &serde_json::json!({"id": id}));

        let mut stream = Box::pin(response.bytes_stream());
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, b"hello world");

        // Consumer acks flowed back to the relay.
        let ack1 = relay_rx.recv().await.unwrap();
        assert_eq!(ack1["t"], "egress_ack");
        assert_eq!(ack1["bytes"], 6);
        let ack2 = relay_rx.recv().await.unwrap();
        assert_eq!(ack2["bytes"], 5);
        reset();
    }

    #[tokio::test]
    async fn frames_from_other_sessions_are_ignored_and_errors_propagate() {
        let _guard = lock();
        reset();
        let (tx, mut relay_rx) = mpsc::unbounded_channel();
        register("s1", "Tab A", "local", &kinds(&[KIND_ANTHROPIC]), false, tx).unwrap();

        let fetch_task = tokio::spawn(fetch(
            KIND_ANTHROPIC,
            "POST",
            "https://api.anthropic.com/v1/messages",
            Vec::new(),
            Vec::new(),
        ));
        let head = relay_rx.recv().await.unwrap();
        let id = head["id"].as_str().unwrap().to_string();
        let _ = relay_rx.recv().await; // request_end (empty body: no chunks)

        // A different session cannot answer this request.
        handle_browser_frame(
            "intruder",
            "egress_response",
            &serde_json::json!({"id": id, "status": 200}),
        );
        handle_browser_frame(
            "intruder",
            "egress_error",
            &serde_json::json!({"id": id, "error": "nope"}),
        );

        // The owner errors it for real.
        handle_browser_frame(
            "s1",
            "egress_error",
            &serde_json::json!({"id": id, "error": "vault is locked"}),
        );
        let result = fetch_task.await.unwrap();
        assert!(result.is_err());
        let message = result.err().unwrap();
        assert!(message.contains("vault is locked"), "{message}");
        // The failed request told the relay to abort.
        let cancel = relay_rx.recv().await.unwrap();
        assert_eq!(cancel["t"], "egress_cancel");
        reset();
    }

    #[tokio::test]
    async fn session_detach_fails_inflight_requests() {
        let _guard = lock();
        reset();
        let (tx, mut relay_rx) = mpsc::unbounded_channel();
        register("s1", "Tab A", "local", &kinds(&[KIND_ANTHROPIC]), false, tx).unwrap();
        let fetch_task = tokio::spawn(fetch(
            KIND_ANTHROPIC,
            "POST",
            "https://api.anthropic.com/v1/messages",
            Vec::new(),
            Vec::new(),
        ));
        let head = relay_rx.recv().await.unwrap();
        assert_eq!(head["t"], "egress_request");

        assert_eq!(unregister_session("s1"), 1);
        let result = fetch_task.await.unwrap();
        assert!(result.err().unwrap().contains("detached"));
        assert!(!available(KIND_ANTHROPIC));
        assert!(pending().read().unwrap().is_empty());
        reset();
    }

    #[tokio::test]
    async fn no_relay_is_a_clear_error() {
        let _guard = lock();
        reset();
        let error = fetch(
            KIND_ANTHROPIC,
            "POST",
            "https://api.anthropic.com/v1/messages",
            Vec::new(),
            Vec::new(),
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("no client-egress relay"), "{error}");
        reset();
    }

    const WINDOW: usize = REQUEST_CREDIT_WINDOW_BYTES as usize;
    const WINDOW_CHUNKS: usize = WINDOW / REQUEST_CHUNK_BYTES;

    fn spawn_oversized_fetch(
        extra_chunks: usize,
    ) -> tokio::task::JoinHandle<Result<EgressResponse, String>> {
        tokio::spawn(fetch(
            KIND_ANTHROPIC,
            "POST",
            "https://api.anthropic.com/v1/messages",
            Vec::new(),
            vec![7u8; WINDOW + extra_chunks * REQUEST_CHUNK_BYTES],
        ))
    }

    #[tokio::test]
    async fn windowed_send_waits_for_request_acks() {
        let _guard = lock();
        reset();
        let (tx, mut relay_rx) = mpsc::unbounded_channel();
        register("s1", "Tab A", "local", &kinds(&[KIND_ANTHROPIC]), true, tx).unwrap();
        let fetch_task = spawn_oversized_fetch(2);

        let head = relay_rx.recv().await.unwrap();
        assert_eq!(head["t"], "egress_request");
        assert_eq!(head["request_credit"], REQUEST_CREDIT_WINDOW_BYTES);
        let id = head["id"].as_str().unwrap().to_string();

        // Exactly one window of chunks is in flight before any ack…
        let mut in_flight = 0usize;
        for _ in 0..WINDOW_CHUNKS {
            let frame = relay_rx.recv().await.unwrap();
            assert_eq!(frame["t"], "egress_request_chunk");
            in_flight += base64::engine::general_purpose::STANDARD
                .decode(frame["data"].as_str().unwrap())
                .unwrap()
                .len();
        }
        assert_eq!(in_flight, WINDOW);
        // …and a slow relay that has not acked yet gets nothing more.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), relay_rx.recv())
                .await
                .is_err(),
            "sender exceeded the request credit window"
        );

        // One chunk of credit releases exactly one more chunk.
        handle_browser_frame(
            "s1",
            "egress_request_ack",
            &serde_json::json!({"id": id, "bytes": REQUEST_CHUNK_BYTES}),
        );
        assert_eq!(relay_rx.recv().await.unwrap()["t"], "egress_request_chunk");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), relay_rx.recv())
                .await
                .is_err(),
            "one chunk of credit released more than one chunk"
        );

        // A full refill flushes the tail chunk and the end marker.
        handle_browser_frame(
            "s1",
            "egress_request_ack",
            &serde_json::json!({"id": id, "bytes": REQUEST_CREDIT_WINDOW_BYTES}),
        );
        assert_eq!(relay_rx.recv().await.unwrap()["t"], "egress_request_chunk");
        assert_eq!(relay_rx.recv().await.unwrap()["t"], "egress_request_end");

        handle_browser_frame(
            "s1",
            "egress_response",
            &serde_json::json!({"id": id, "status": 200}),
        );
        let response = fetch_task.await.unwrap().unwrap();
        assert!(response.status_success());
        handle_browser_frame("s1", "egress_end", &serde_json::json!({"id": id}));
        assert_eq!(response.body_text().await.unwrap(), "");
        reset();
    }

    #[tokio::test]
    async fn legacy_relay_without_request_credits_gets_push_all() {
        let _guard = lock();
        reset();
        let (tx, mut relay_rx) = mpsc::unbounded_channel();
        register("s1", "Tab A", "local", &kinds(&[KIND_ANTHROPIC]), false, tx).unwrap();
        let fetch_task = spawn_oversized_fetch(2);

        // The whole request ships without a single ack.
        let head = relay_rx.recv().await.unwrap();
        assert_eq!(head["t"], "egress_request");
        assert!(
            head.get("request_credit").is_none(),
            "legacy relays must not be offered the request-credit protocol"
        );
        let id = head["id"].as_str().unwrap().to_string();
        for _ in 0..(WINDOW_CHUNKS + 2) {
            assert_eq!(relay_rx.recv().await.unwrap()["t"], "egress_request_chunk");
        }
        assert_eq!(relay_rx.recv().await.unwrap()["t"], "egress_request_end");

        handle_browser_frame(
            "s1",
            "egress_response",
            &serde_json::json!({"id": id, "status": 200}),
        );
        let response = fetch_task.await.unwrap().unwrap();
        assert!(response.status_success());
        handle_browser_frame("s1", "egress_end", &serde_json::json!({"id": id}));
        assert_eq!(response.body_text().await.unwrap(), "");
        reset();
    }

    #[tokio::test(start_paused = true)]
    async fn request_ack_stall_fails_the_request() {
        let _guard = lock();
        reset();
        let (tx, mut relay_rx) = mpsc::unbounded_channel();
        register("s1", "Tab A", "local", &kinds(&[KIND_ANTHROPIC]), true, tx).unwrap();
        let fetch_task = spawn_oversized_fetch(1);

        let head = relay_rx.recv().await.unwrap();
        let id = head["id"].as_str().unwrap().to_string();
        for _ in 0..WINDOW_CHUNKS {
            assert_eq!(relay_rx.recv().await.unwrap()["t"], "egress_request_chunk");
        }

        // The relay never acks: paused time auto-advances through
        // REQUEST_ACK_IDLE_TIMEOUT and the request fails closed.
        let error = fetch_task.await.unwrap().err().unwrap();
        assert!(error.contains("stalled consuming the request"), "{error}");
        // The failure told the relay to abort, and nothing leaked.
        let cancel = relay_rx.recv().await.unwrap();
        assert_eq!(cancel["t"], "egress_cancel");
        assert_eq!(cancel["id"].as_str().unwrap(), id);
        assert!(pending().read().unwrap().is_empty());
        reset();
    }

    #[tokio::test]
    async fn relay_detach_mid_request_fails_the_request() {
        let _guard = lock();
        reset();
        let (tx, mut relay_rx) = mpsc::unbounded_channel();
        register("s1", "Tab A", "local", &kinds(&[KIND_ANTHROPIC]), true, tx).unwrap();
        let fetch_task = spawn_oversized_fetch(1);

        let head = relay_rx.recv().await.unwrap();
        assert_eq!(head["t"], "egress_request");
        for _ in 0..WINDOW_CHUNKS {
            assert_eq!(relay_rx.recv().await.unwrap()["t"], "egress_request_chunk");
        }

        // The tab dies while the sender is parked on the ack window: the
        // request fails with the existing detach semantics.
        assert_eq!(unregister_session("s1"), 1);
        let error = fetch_task.await.unwrap().err().unwrap();
        assert!(error.contains("detached"), "{error}");
        assert!(pending().read().unwrap().is_empty());
        reset();
    }
}
