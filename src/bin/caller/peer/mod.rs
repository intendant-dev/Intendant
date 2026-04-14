//! Peer agent federation layer.
//!
//! Intendant federates with other autonomous agent daemons — other
//! Intendants, OpenClaw gateways, A2A-speaking peers, MCP-server-shaped
//! peers — as equals. Federation is distinct from [`crate::external_agent`],
//! which models subordinate coding-CLI processes that Intendant supervises:
//!
//! - `external_agent` = "I spawn a process and give it a task." Master/worker.
//!   ACP-shaped. Right for Codex / Claude Code / Aider / goose.
//! - `peer`           = "I federate with a peer daemon." Peer/peer.
//!   A2A-shaped. Right for OpenClaw / Hermes / Letta / another Intendant.
//!
//! The two are orthogonal and compose: a peer Intendant can itself
//! supervise a Codex subprocess via its local `external_agent` layer
//! while being driven from this side as a `peer`.
//!
//! ## Module layout
//!
//! - [`id`]      — `PeerId`, `PeerKind`. Stable opaque identity.
//! - [`card`]    — `AgentCard`, `Capability`, `TransportSpec`, `AuthScheme`.
//!                 Served at `/.well-known/agent-card.json`. Replaces the
//!                 host_label/version/git_sha fields of `WebGatewayConfig`.
//! - [`event`]   — `PeerEvent`, the lean transport-neutral event vocabulary.
//!                 The native Intendant transport upcasts `AppEvent` into
//!                 these variants; there is no `Native(AppEvent)` escape
//!                 hatch by design.
//! - [`traits`]  — `PeerTransport` (single trait), `PeerOp`/`PeerOpAck`
//!                 envelope, `TransportFeatures`, and the `check_feature`
//!                 invariant guard.
//! - [`handle`]  — `PeerHandle` (registry-facing concrete struct),
//!                 `ConnectionState`, `spawn_peer` constructor.
//! - [`actor`]   — Internal per-peer actor task that owns the transport
//!                 and runs the connect → main-loop → reconnect state
//!                 machine.
//!
//! Transport implementations and the registry/coordinator land in
//! follow-up modules (`transport::intendant`, `transport::a2a`,
//! `transport::openclaw`, `transport::mcp_client`, `registry`,
//! `coordinator`) once the abstractions here are settled.

mod actor;
pub mod card;
pub mod event;
pub mod handle;
pub mod id;
pub mod traits;

pub use card::{
    AgentCard, AuthScheme, Capability, McpTransportKind, OpenClawRole, TransportSpec,
};
pub use event::{
    ActivityId, ActivityKind, ActivityOutcome, ApprovalDecision, ApprovalRequest, LogLevel,
    MessageContent, MessageId, MessagePart, MessageRole, ModelUsage, PeerEvent, PeerMessage,
    PeerStatus, SessionInfo, TaggedPeerEvent, TaskId, TaskUpdate, UsageSnapshot,
};
pub use handle::{
    spawn_peer, ConnectionState, PeerHandle, BROADCAST_CAPACITY, COMMANDS_CAPACITY,
    EVENTS_CAPACITY,
};
pub use id::{PeerId, PeerKind};
pub use traits::{
    check_feature, PeerOp, PeerOpAck, PeerTask, PeerTransport, TransportFeatures,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the peer federation layer.
///
/// Self-contained — does not depend on `crate::external_agent` or
/// `crate::error::CallerError`. A `From<PeerError> for CallerError` impl
/// can be added when the registry layer needs to bubble peer errors up
/// into general caller code.
#[derive(Debug)]
pub enum PeerError {
    /// Peer not found in the registry.
    NotFound(String),
    /// Underlying transport (WebSocket, HTTP, stdio) failed.
    Transport(String),
    /// Peer is currently disconnected; reconnect before retrying.
    NotConnected,
    /// Peer is connected but lacks the requested capability.
    UnsupportedCapability(String),
    /// Failed to fetch or parse a peer's Agent Card.
    CardFetch(String),
    /// Auth handshake failed.
    Auth(String),
    /// Peer rejected the operation with a structured error.
    Rejected { code: String, message: String },
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "peer not found: {id}"),
            Self::Transport(s) => write!(f, "peer transport error: {s}"),
            Self::NotConnected => write!(f, "peer is not connected"),
            Self::UnsupportedCapability(c) => {
                write!(f, "peer does not support capability: {c}")
            }
            Self::CardFetch(s) => write!(f, "agent card fetch failed: {s}"),
            Self::Auth(s) => write!(f, "peer auth failed: {s}"),
            Self::Rejected { code, message } => {
                write!(f, "peer rejected operation [{code}]: {message}")
            }
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for PeerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PeerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for PeerError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
