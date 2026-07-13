//! Shared implementations of the agent-facing peer operations, behind
//! every control surface: the MCP tools (`mcp/tools_peer.rs`), the
//! native `peer` tool (`agent_loop.rs`), and — through the MCP tools —
//! `intendant ctl peer`. One implementation so argument handling and
//! result shapes cannot drift between surfaces. Mirrors the
//! `/api/peers` HTTP handlers in `web_gateway/routes_peers.rs`.
//!
//! Results are JSON strings: `{"peers": [...]}` for listing, and
//! `{"ok": true, ...}` / `{"ok": false, "error": ...}` for actions —
//! the same envelope the display/browser MCP tools use. The direct
//! computer-use operations (screenshot, cu) return [`PeerToolOutput`]
//! instead: the same text conventions plus conversation-ready image
//! attachments, because their whole point is that the agent *sees*
//! the peer's screen.

use super::mcp_http::{self, PeerToolReply};
use super::{
    MessageContent, MessageRole, PeerHandle, PeerId, PeerMessage, PeerRegistry, PeerSnapshot,
    PeerTask,
};
use crate::conversation::ImageData;

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
        Ok(message_id) => serde_json::json!({ "ok": true, "message_id": message_id.0 }).to_string(),
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
        Ok(delegation) => serde_json::json!({
            "ok": true,
            "task_id": delegation.task_id.0,
            "delegation_id": delegation.delegation_id,
            // "acknowledged": the peer confirmed acceptance and task_id
            // is its real session id. "unconfirmed": fire-and-forget
            // fallback (older peer build, or the link kept dropping
            // before an ack) — task_id is a sender-side marker only.
            "delivery": if delegation.confirmed { "acknowledged" } else { "unconfirmed" },
            // StartTask frames written (>1 = re-sent across a reconnect).
            "sends": delegation.sends,
            "note": if delegation.confirmed {
                "accepted by the peer; its agent executes this under its own autonomy and approval policy"
            } else {
                "the peer did not acknowledge receipt (pre-receipt build or unstable link); \
                 delivery is fire-and-forget and the task id is a local marker"
            },
        })
        .to_string(),
        Err(err) => error_json(err.to_string()),
    }
}

/// Rich output from a direct peer tool invocation: transcript text
/// plus any screenshots as conversation-ready image attachments.
/// `is_error` mirrors the MCP `isError` flag so the MCP surface can
/// mark its result without re-parsing the text envelope; the native
/// surface encodes failure in the `{"ok": false}` text alone.
#[derive(Debug)]
pub struct PeerToolOutput {
    pub text: String,
    pub images: Vec<ImageData>,
    pub is_error: bool,
}

impl PeerToolOutput {
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            is_error: true,
        }
    }
}

/// Fold a peer `/mcp` reply into [`PeerToolOutput`]. Tool-level
/// failures keep their images (a partially failed CU batch still
/// attaches the annotated post-action screenshot) but wrap the peer's
/// diagnostic in the `{"ok": false, ...}` envelope so every surface
/// reports errors the same way.
fn peer_tool_output(result: Result<PeerToolReply, String>) -> PeerToolOutput {
    match result {
        Ok(reply) => {
            let images = reply
                .images
                .into_iter()
                .map(|img| ImageData {
                    media_type: img.media_type,
                    data: img.data,
                })
                .collect();
            let text = if reply.is_error {
                error_json(reply.text)
            } else {
                reply.text
            };
            PeerToolOutput {
                text,
                images,
                is_error: reply.is_error,
            }
        }
        Err(err) => PeerToolOutput::error(error_json(err)),
    }
}

/// List the displays a peer currently offers — the remote
/// `list_displays`, invoked over the peer's `/mcp` and gated there by
/// the DisplayView grant of the profile the peer issued this daemon.
pub async fn list_displays_json(registry: Option<&PeerRegistry>, peer_id: &str) -> String {
    let handle = match peer_handle(registry, peer_id) {
        Ok(handle) => handle,
        Err(error) => return error,
    };
    match mcp_http::call_peer_mcp_tool(&handle, "list_displays", serde_json::json!({})).await {
        Ok(reply) if reply.is_error => error_json(reply.text),
        Ok(reply) => reply.text,
        Err(err) => error_json(err),
    }
}

/// Screenshot a display on the peer (remote `take_screenshot`,
/// DisplayView-gated peer-side). The peer's capture metadata rides
/// the text part; the PNG rides `images`.
pub async fn take_screenshot(
    registry: Option<&PeerRegistry>,
    peer_id: &str,
    display_target: Option<String>,
) -> PeerToolOutput {
    let handle = match peer_handle(registry, peer_id) {
        Ok(handle) => handle,
        Err(error) => return PeerToolOutput::error(error),
    };
    let mut arguments = serde_json::Map::new();
    if let Some(target) = display_target {
        arguments.insert("display_target".into(), target.into());
    }
    peer_tool_output(
        mcp_http::call_peer_mcp_tool(
            &handle,
            "take_screenshot",
            serde_json::Value::Object(arguments),
        )
        .await,
    )
}

/// Execute computer-use actions on a peer's display (remote
/// `execute_cu_actions`, DisplayInput-gated peer-side — only the
/// peer-operator and peer-root profiles hold it). `actions` is passed
/// through verbatim in the peer's own `CuAction` vocabulary; the
/// reply text carries per-action status and the annotated post-action
/// screenshot rides `images`.
pub async fn execute_cu_actions(
    registry: Option<&PeerRegistry>,
    peer_id: &str,
    actions: serde_json::Value,
    display_target: Option<String>,
    coordinate_space: Option<String>,
) -> PeerToolOutput {
    let handle = match peer_handle(registry, peer_id) {
        Ok(handle) => handle,
        Err(error) => return PeerToolOutput::error(error),
    };
    let mut arguments = serde_json::Map::new();
    arguments.insert("actions".into(), actions);
    if let Some(target) = display_target {
        arguments.insert("display_target".into(), target.into());
    }
    if let Some(space) = coordinate_space {
        arguments.insert("coordinate_space".into(), space.into());
    }
    peer_tool_output(
        mcp_http::call_peer_mcp_tool(
            &handle,
            "execute_cu_actions",
            serde_json::Value::Object(arguments),
        )
        .await,
    )
}
