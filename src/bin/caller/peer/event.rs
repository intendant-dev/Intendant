//! Lean transport-neutral event vocabulary for peer federation.
//!
//! The federation layer must work uniformly across heterogeneous peers
//! (Intendant, OpenClaw, A2A, MCP), so this enum is the convex hull of
//! what every transport can map into. It deliberately does NOT carry
//! Intendant-specific concepts like [`crate::event::AppEvent`] — the
//! native Intendant transport upcasts `AppEvent` into these variants in
//! `transport/intendant.rs` (via `crate::event::app_event_to_peer_event`).
//!
//! Variants are organized into categories that map to the dashboard UI:
//! lifecycle, activity stream, conversation, task delegation, approval,
//! capability state, usage, session, and log. Every category corresponds
//! to a renderable surface — there is no "miscellaneous" variant and no
//! `Native(AppEvent)` escape hatch.

use crate::peer::card::{AgentCard, Capability};
use crate::peer::id::PeerId;
use serde::{Deserialize, Serialize};

/// One event from a peer. The originating `PeerId` is attached at the
/// registry layer via [`TaggedPeerEvent`] — the inner enum stays unaware
/// of which peer produced it so transport adapters can construct events
/// without round-tripping the id.
///
/// The serde tag is `event` (not `kind`) so it doesn't collide with
/// inner fields named `kind` (e.g. `ActivityStarted::kind: ActivityKind`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PeerEvent {
    // ---- Connection lifecycle ----
    /// Peer just completed handshake and sent its (possibly updated) card.
    Connected { card: AgentCard },

    /// Peer disconnected. The transport may auto-reconnect; if so, a
    /// follow-up `Connected` will arrive when the handshake completes.
    Disconnected { reason: String },

    /// Peer's overall status changed.
    StatusChanged { status: PeerStatus },

    // ---- Activity stream — what the peer is doing right now ----
    /// A unit of work has begun (turn, tool call, sub-agent run, delegated
    /// task, etc). Activities have an opaque id and a kind for routing.
    ActivityStarted {
        id: ActivityId,
        kind: ActivityKind,
        label: String,
    },

    /// Incremental progress on an in-flight activity. `text` is
    /// kind-specific — model output for `ModelTurn`, stdout for `ToolCall`,
    /// progress messages for `SubAgent`. Empty `text` is a heartbeat.
    ActivityProgress {
        id: ActivityId,
        text: Option<String>,
    },

    /// Activity completed.
    ActivityCompleted {
        id: ActivityId,
        outcome: ActivityOutcome,
    },

    // ---- Conversation — user-visible messages ----
    /// A message in the peer's conversation. `partial: true` signals a
    /// streaming chunk; `false` signals a complete message (final or
    /// non-streaming). Streaming chunks share the same `id` so the
    /// renderer can assemble them.
    Message {
        id: MessageId,
        role: MessageRole,
        content: MessageContent,
        partial: bool,
    },

    // ---- Task delegation lifecycle ----
    /// Update for a task that was delegated *to* this peer (i.e. the
    /// federation coordinator initiated the work, peer is reporting back).
    /// Distinct from `ActivityStarted`/etc. which are the peer's own
    /// internal activities — task updates are scoped to delegated work.
    TaskUpdate { task: TaskId, update: TaskUpdate },

    // ---- Approval flow (federated) ----
    /// Peer wants to do something that requires approval. May be forwarded
    /// to a human via the local presence layer or auto-resolved by policy.
    ApprovalRequested { request: ApprovalRequest },

    /// Approval was resolved (locally or remotely). Echoed so observers
    /// can update UI consistently regardless of which side made the call.
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },

    // ---- Capability state ----
    /// Peer engaged a capability (started using its display, opened a
    /// voice session, started recording, picked up a chat channel). The
    /// typed replacement for `AppEvent::DisplayTaken` / `RecordingStarted`
    /// / `PresenceConnected` and OpenClaw's analogous events. `detail` is
    /// capability-specific structured data.
    CapabilityEngaged {
        capability: Capability,
        detail: serde_json::Value,
    },

    /// Peer released a capability. `reason` is optional structured context
    /// (e.g. `Some("capture_lost")` for an involuntary release).
    CapabilityReleased {
        capability: Capability,
        reason: Option<String>,
    },

    // ---- Resource accounting ----
    Usage { snapshot: UsageSnapshot },

    // ---- Session lifecycle ----
    SessionStarted { session: SessionInfo },
    SessionEnded {
        session_id: String,
        reason: String,
    },

    // ---- Structured log line ----
    /// Levelled, sourced log entry. Replaces `AppEvent::LogEntry`,
    /// `PresenceLog`, `VoiceLog`, `OrchestratorLog`, `ContextManagement`.
    /// `source` is a free-form tag like `"orchestrator"` / `"voice"` /
    /// `"presence"` so the renderer can group/filter.
    Log {
        level: LogLevel,
        source: String,
        message: String,
        /// RFC3339 timestamp string, matching the existing session_log
        /// convention (see `web_gateway::replay_jsonl_to_outbound_entries`).
        ts: String,
    },
}

/// Operational status reported by a peer.
///
/// Deliberately scoped to *what the peer is doing*, not *whether the
/// connection is up*. Transport lifecycle lives on
/// [`crate::peer::handle::ConnectionState`] — the two are separate
/// concerns and separate watch channels on the handle. The dashboard
/// composes both: a peer can be `ConnectionState::Reconnecting` while
/// its last observed `PeerStatus` was `Working`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerStatus {
    Idle,
    Working,
    NeedsApproval,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActivityId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// A model turn (request → streamed response → completion).
    ModelTurn,
    /// A tool / command execution.
    ToolCall,
    /// A sub-agent run.
    SubAgent,
    /// A task this peer is executing on behalf of a delegating peer.
    DelegatedTask,
    /// Custom or transport-specific activity kind.
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActivityOutcome {
    Success,
    Failed { message: String },
    Cancelled,
    /// Activity was paused mid-flight (e.g. waiting on an approval, or
    /// hit a budget cap that requires human resolution).
    Suspended { reason: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum TaskUpdate {
    Accepted,
    Progress {
        pct: Option<f32>,
        message: Option<String>,
    },
    Completed { result: serde_json::Value },
    Failed { message: String },
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    Text { text: String },
    /// Reasoning / chain-of-thought trace from a model that emits one.
    Reasoning { text: String },
    /// Image attachment.
    Image { mime_type: String, base64: String },
    /// Multi-part content (mix of text + images + tool calls).
    Parts { parts: Vec<MessagePart> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text { text: String },
    Image { mime_type: String, base64: String },
    ToolCall { name: String, args: serde_json::Value },
    ToolResult { name: String, result: serde_json::Value },
}

/// A message to send *to* a peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerMessage {
    /// Optional session/thread to scope the message to. If `None`, the
    /// transport picks the default (peer's current session, or starts a
    /// new one).
    pub session: Option<String>,
    pub role: MessageRole,
    pub content: MessageContent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    /// Free-form category tag — `"command"`, `"file_change"`,
    /// `"human_question"`, etc. Free-form because peer kinds have
    /// different category sets and a closed enum would either bloat
    /// or leak details.
    pub category: String,
    /// Human-readable preview of what's being approved (e.g. the command
    /// line, the file diff, the question).
    pub preview: String,
    /// Whether local autonomy policy is allowed to auto-resolve this.
    pub auto_resolvable: bool,
}

/// Approval decision. Mirrors `external_agent::ApprovalDecision` by design
/// — both encode the same four-way user response. Kept separate today to
/// avoid coupling `peer` to `external_agent`; a follow-up should extract a
/// shared `crate::approval` module that both consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub cost_usd: Option<f64>,
    /// Optional per-model breakdown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model: Vec<ModelUsage>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub label: Option<String>,
    /// RFC3339 timestamp string.
    pub started_at: String,
}

/// `PeerEvent` tagged with the originating `PeerId` and a per-peer
/// monotonic sequence number. Produced by the registry; consumed by the
/// dashboard renderer and the session log. The inner event lives under
/// `payload` (not `event`) so the wire JSON doesn't have two `event`
/// keys at different nesting levels.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaggedPeerEvent {
    pub peer: PeerId,
    pub payload: PeerEvent,
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serde_round_trip() {
        let evt = PeerEvent::Message {
            id: MessageId("msg-1".into()),
            role: MessageRole::Assistant,
            content: MessageContent::Text {
                text: "hello".into(),
            },
            partial: false,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: PeerEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PeerEvent::Message { content, .. } => match content {
                MessageContent::Text { text } => assert_eq!(text, "hello"),
                _ => panic!("wrong content variant"),
            },
            _ => panic!("wrong event variant"),
        }
    }

    #[test]
    fn capability_engaged_carries_detail() {
        let evt = PeerEvent::CapabilityEngaged {
            capability: Capability::Display,
            detail: serde_json::json!({"display_id": ":99", "resolution": "1920x1080"}),
        };
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: PeerEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PeerEvent::CapabilityEngaged { detail, .. } => {
                assert_eq!(detail["display_id"], ":99");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn activity_lifecycle_round_trip() {
        let id = ActivityId("act-1".into());
        let started = PeerEvent::ActivityStarted {
            id: id.clone(),
            kind: ActivityKind::ModelTurn,
            label: "turn 7".into(),
        };
        let progress = PeerEvent::ActivityProgress {
            id: id.clone(),
            text: Some("partial response".into()),
        };
        let completed = PeerEvent::ActivityCompleted {
            id,
            outcome: ActivityOutcome::Success,
        };
        for evt in [started, progress, completed] {
            let json = serde_json::to_string(&evt).unwrap();
            let _: PeerEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn tagged_event_carries_peer_and_seq() {
        use crate::peer::id::PeerKind;
        let tagged = TaggedPeerEvent {
            peer: PeerId::new(PeerKind::Intendant, "nicks-mac"),
            payload: PeerEvent::StatusChanged {
                status: PeerStatus::Working,
            },
            seq: 42,
        };
        let json = serde_json::to_string(&tagged).unwrap();
        let parsed: TaggedPeerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.seq, 42);
        assert_eq!(parsed.peer.as_str(), "intendant:nicks-mac");
    }
}
