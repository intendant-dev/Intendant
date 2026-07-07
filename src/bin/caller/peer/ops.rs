//! Shared implementations of the agent-facing peer operations, behind
//! every control surface: the MCP tools (`mcp/tools_peer.rs`), the
//! native `peer` tool (`agent_loop.rs`), and — through the MCP tools —
//! `intendant ctl peer`. One implementation so argument handling and
//! result shapes cannot drift between surfaces. Mirrors the
//! `/api/peers` HTTP handlers in `web_gateway/routes_peers.rs`.
//!
//! Results are JSON strings: `{"peers": [...]}` for listing, and
//! `{"ok": true, ...}` / `{"ok": false, "error": ...}` for actions —
//! the same envelope the display/browser MCP tools use.

use super::{
    MessageContent, MessageRole, PeerHandle, PeerId, PeerMessage, PeerRegistry, PeerSnapshot,
    PeerTask,
};

/// Note returned when this process runs without a peer registry
/// (standalone `--mcp`, `--no-web` shapes, tests).
pub const FEDERATION_INACTIVE_NOTE: &str = "peer federation is not active on this daemon";

fn error_json(error: impl Into<String>) -> String {
    serde_json::json!({ "ok": false, "error": error.into() }).to_string()
}

/// Look up a connected peer by id, mirroring `peer_handle_or_404` on
/// the HTTP surface. The error is a ready-to-return result JSON.
fn peer_handle(registry: Option<&PeerRegistry>, peer_id: &str) -> Result<PeerHandle, String> {
    let Some(registry) = registry else {
        return Err(error_json(FEDERATION_INACTIVE_NOTE));
    };
    let id = PeerId(peer_id.to_string());
    registry
        .get(&id)
        .ok_or_else(|| error_json(format!("peer not found: {peer_id} (see list_peers)")))
}

/// List federated peers as `{"peers": [PeerSnapshot...]}` — the same
/// snapshot payload `GET /api/peers` serves. Without a registry the
/// list is empty and carries an explanatory note.
pub fn list_peers_json(registry: Option<&PeerRegistry>) -> String {
    let Some(registry) = registry else {
        return serde_json::json!({ "peers": [], "note": FEDERATION_INACTIVE_NOTE }).to_string();
    };
    let peers: Vec<PeerSnapshot> = registry.list().iter().map(|h| h.snapshot()).collect();
    serde_json::to_string_pretty(&serde_json::json!({ "peers": peers }))
        .unwrap_or_else(|_| "{\"peers\":[]}".to_string())
}

/// Send a user-role text message to a peer's agent, optionally scoped
/// to a peer-side session.
pub async fn send_message_json(
    registry: Option<&PeerRegistry>,
    peer_id: &str,
    message: String,
    session: Option<String>,
) -> String {
    let handle = match peer_handle(registry, peer_id) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let message = PeerMessage {
        session,
        role: MessageRole::User,
        content: MessageContent::Text { text: message },
    };
    match handle.send_message(message).await {
        Ok(message_id) => {
            serde_json::json!({ "ok": true, "message_id": message_id.0 }).to_string()
        }
        Err(err) => error_json(err.to_string()),
    }
}

/// Delegate a task to a peer. The peer's own agent executes the
/// instructions on its machine under its own autonomy and approval
/// policy; the returned task id correlates its progress events.
pub async fn delegate_task_json(
    registry: Option<&PeerRegistry>,
    peer_id: &str,
    instructions: String,
    context: Option<serde_json::Value>,
) -> String {
    let handle = match peer_handle(registry, peer_id) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    let task = PeerTask {
        instructions,
        context: context.unwrap_or(serde_json::Value::Null),
        client_correlation_id: None,
    };
    match handle.delegate_task(task).await {
        Ok(task_id) => serde_json::json!({
            "ok": true,
            "task_id": task_id.0,
            "note": "the peer's agent executes this under its own autonomy and approval policy",
        })
        .to_string(),
        Err(err) => error_json(err.to_string()),
    }
}
