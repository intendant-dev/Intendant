//! Peer identity.
//!
//! A `PeerId` is a stable opaque token of the form `"<kind>:<label>"`, e.g.
//! `"intendant:nicks-mac"` or `"openclaw:home-server"`. The kind prefix lets
//! the registry de-collide peers across daemon types that might otherwise
//! share a label, and lets readers route by kind without needing the full
//! `AgentCard` in hand.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable opaque identifier for a peer agent daemon.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

impl PeerId {
    pub fn new(kind: PeerKind, label: &str) -> Self {
        Self(format!("{}:{}", kind.as_str(), label))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse the kind prefix from the id.
    pub fn kind(&self) -> Option<PeerKind> {
        let prefix = self.0.split(':').next()?;
        PeerKind::from_str(prefix)
    }

    /// Return the label portion (everything after the first `:`).
    /// If the id has no colon at all, the whole id is the label.
    pub fn label(&self) -> &str {
        match self.0.split_once(':') {
            Some((_, rest)) => rest,
            None => &self.0,
        }
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of agent daemon a peer is.
///
/// Used by the registry to dispatch to the right transport: an `Intendant`
/// peer uses the native Intendant WebSocket transport; an `OpenClaw` peer
/// uses the operator+node Gateway protocol; etc. `Other` is a forward-compat
/// escape hatch for future agent kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerKind {
    Intendant,
    OpenClaw,
    Hermes,
    Letta,
    /// Generic A2A-speaking peer (Linux Foundation Agent2Agent protocol).
    A2A,
    /// Generic MCP-server-shaped peer.
    Mcp,
    Other,
}

impl PeerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intendant => "intendant",
            Self::OpenClaw => "openclaw",
            Self::Hermes => "hermes",
            Self::Letta => "letta",
            Self::A2A => "a2a",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "intendant" => Some(Self::Intendant),
            "openclaw" => Some(Self::OpenClaw),
            "hermes" => Some(Self::Hermes),
            "letta" => Some(Self::Letta),
            "a2a" => Some(Self::A2A),
            "mcp" => Some(Self::Mcp),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trip() {
        let id = PeerId::new(PeerKind::Intendant, "nicks-mac");
        assert_eq!(id.as_str(), "intendant:nicks-mac");
        assert_eq!(id.kind(), Some(PeerKind::Intendant));
        assert_eq!(id.label(), "nicks-mac");
    }

    #[test]
    fn id_with_colon_in_label() {
        // Labels are allowed to contain colons; only the first colon is
        // the kind separator. Useful for `tcp:host:port`-style labels.
        let id = PeerId::new(PeerKind::OpenClaw, "tcp:host:8080");
        assert_eq!(id.kind(), Some(PeerKind::OpenClaw));
        assert_eq!(id.label(), "tcp:host:8080");
    }

    #[test]
    fn id_with_unknown_prefix() {
        let id = PeerId("zzz:foo".into());
        assert_eq!(id.kind(), None);
        assert_eq!(id.label(), "foo");
    }

    #[test]
    fn id_without_prefix() {
        let id = PeerId("just-a-label".into());
        assert_eq!(id.kind(), None);
        assert_eq!(id.label(), "just-a-label");
    }

    #[test]
    fn kind_serde_round_trip() {
        for k in [
            PeerKind::Intendant,
            PeerKind::OpenClaw,
            PeerKind::Hermes,
            PeerKind::Letta,
            PeerKind::A2A,
            PeerKind::Mcp,
            PeerKind::Other,
        ] {
            let json = serde_json::to_string(&k).unwrap();
            let parsed: PeerKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, k);
            assert_eq!(PeerKind::from_str(k.as_str()), Some(k));
        }
    }
}
