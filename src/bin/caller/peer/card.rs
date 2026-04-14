//! Agent Card — the canonical identity and capability descriptor for a peer.
//!
//! Served at `/.well-known/agent-card.json` by every Intendant daemon, and
//! fetched from non-Intendant peers via the same path (A2A-style discovery).
//! The card is the single source of truth for: who this peer is, what it
//! can do, how to reach it, and how to authenticate against it. Replaces
//! the host_label/version/git_sha fields of [`crate::web_gateway::WebGatewayConfig`],
//! which now carries only voice runtime config.

use crate::peer::id::PeerId;
use serde::{Deserialize, Serialize};

/// Identity + capability + transport descriptor for one peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    /// Stable opaque ID. The peer identifies itself with this in every
    /// event and request. `id.kind()` is the source of truth for the
    /// peer's daemon kind — there is no separate `kind` field on the
    /// card, by design (one source of truth).
    pub id: PeerId,

    /// Human-readable display name. May change without affecting `id`.
    pub label: String,

    /// Cargo package version (or equivalent) of the daemon binary.
    pub version: String,

    /// Short git commit SHA the binary was built from, if known.
    /// `None` for non-Intendant peers that don't expose build metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,

    /// One or more transports this peer can be reached on. Listed in
    /// preference order — the registry picks the first one whose type
    /// is supported and reachable. A peer may offer several (e.g. an
    /// Intendant daemon will expose its native WebSocket *and* an MCP
    /// server *and*, once shipped, an A2A endpoint, all in one card).
    pub transports: Vec<TransportSpec>,

    /// What this peer can do. The federation coordinator routes work
    /// by matching required capabilities against this list.
    pub capabilities: Vec<Capability>,

    /// How to authenticate against this peer.
    pub auth: AuthScheme,
}

/// One way to reach a peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TransportSpec {
    /// Native Intendant↔Intendant WebSocket. Carries the full `AppEvent`
    /// stream, mapped through the upcaster into `PeerEvent` variants.
    /// This is the highest-fidelity transport between Intendants.
    IntendantWs { url: String },

    /// Linux Foundation Agent2Agent — JSON-RPC over HTTPS + SSE.
    /// The standardizing bet for cross-daemon-kind federation.
    A2A { url: String },

    /// MCP server (any transport variant). Used for peers that expose
    /// themselves as MCP servers — Hermes Agent's `hermes mcp serve`,
    /// Intendant's own MCP server, etc. Lossy compared to native or A2A
    /// because MCP is structurally vertical (agent→tool) rather than
    /// peer-symmetric, but covers a lot of ground cheaply.
    Mcp {
        url: String,
        transport: McpTransportKind,
    },

    /// OpenClaw Gateway WebSocket. Intendant connects as an `operator`
    /// (drive sessions) and/or a `node` (lend capabilities back to the
    /// gateway). One peer entry corresponds to one role; a daemon that
    /// wants both registers two peers with the same underlying URL.
    OpenClawWs { url: String, role: OpenClawRole },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransportKind {
    Stdio,
    Sse,
    StreamableHttp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenClawRole {
    /// Control-plane client — drive OpenClaw sessions, send chat,
    /// invoke nodes, resolve approvals.
    Operator,
    /// Capability host — OpenClaw routes `node.invoke` calls to us so
    /// we can offer screen, voice, computer-use back to the gateway.
    Node,
}

/// Capabilities advertised by a peer. The coordinator routes work by
/// matching required capabilities against this list. `Custom` is the
/// forward-compat escape hatch for capabilities not yet enumerated.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "kebab-case")]
pub enum Capability {
    /// Has a graphical display the peer can show / control / share.
    Display,
    /// Has a live voice / audio session.
    Voice,
    /// Can place phone calls (e.g. Intendant's phone-call skill).
    Phone,
    /// Has computer-use (screen + keyboard + mouse) on its own host.
    ComputerUse,
    /// Has a tagged knowledge store the peer can be queried against.
    Knowledge,
    /// Has display / session recording.
    Recording,
    /// Accepts task delegation from peers (implements `PeerDelegator`).
    TaskDelegation,
    /// Forwards messages to / from external channels (chat, sms, email,
    /// WhatsApp, Telegram). The OpenClaw category, basically.
    MessageRelay,
    /// Custom capability — string-tagged for forward compat.
    Custom(String),
}

/// How a peer authenticates inbound connections.
///
/// Each transport understands the `AuthScheme`s relevant to it. The
/// federation coordinator does not need to interpret them — it forwards
/// the scheme + any local credentials to the transport at connect time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "kebab-case")]
pub enum AuthScheme {
    /// No authentication. Trust on the network layer (LAN, Tailscale,
    /// Unix socket). Default for phase-1 multi-host between Intendants
    /// on a trusted LAN.
    None,
    /// Static bearer token in `Authorization: Bearer <token>` header.
    /// `hint` is an optional human-readable credential reference like
    /// `"intendant.toml [peer.foo] token"` so the registry can locate
    /// the actual secret without leaking it into the card.
    Bearer { hint: Option<String> },
    /// Device keypair challenge/response, OpenClaw-style. The `nonce_url`
    /// is where the challenge is fetched; clients sign it with a per-device
    /// key registered via a pairing flow.
    DeviceKeypair { nonce_url: String },
    /// mTLS — the TLS layer authenticates the peer via a client cert
    /// signed by a CA both sides trust. Reuses the `intendant lan` CA
    /// infrastructure when both peers are Intendants on the same LAN.
    MutualTls,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::id::PeerKind;

    #[test]
    fn card_serde_round_trip() {
        let card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "nicks-mac"),
            label: "Nick's Mac".to_string(),
            version: "0.42.0".to_string(),
            git_sha: Some("deadbeef".to_string()),
            transports: vec![
                TransportSpec::IntendantWs {
                    url: "wss://nicks-mac.local:8443/ws".to_string(),
                },
                TransportSpec::Mcp {
                    url: "https://nicks-mac.local:8443/mcp".to_string(),
                    transport: McpTransportKind::StreamableHttp,
                },
            ],
            capabilities: vec![
                Capability::Display,
                Capability::Voice,
                Capability::ComputerUse,
                Capability::Custom("vortex-audio".to_string()),
            ],
            auth: AuthScheme::MutualTls,
        };
        let json = serde_json::to_string(&card).unwrap();
        let parsed: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, card);
    }

    #[test]
    fn capability_serde_kebab_case() {
        let json = serde_json::to_string(&Capability::ComputerUse).unwrap();
        assert!(json.contains("computer-use"), "got: {json}");
    }

    #[test]
    fn auth_scheme_serde_round_trip() {
        for scheme in [
            AuthScheme::None,
            AuthScheme::Bearer { hint: None },
            AuthScheme::Bearer {
                hint: Some("env:INTENDANT_PEER_TOKEN".to_string()),
            },
            AuthScheme::DeviceKeypair {
                nonce_url: "https://example.test/auth/nonce".to_string(),
            },
            AuthScheme::MutualTls,
        ] {
            let json = serde_json::to_string(&scheme).unwrap();
            let parsed: AuthScheme = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, scheme);
        }
    }
}
