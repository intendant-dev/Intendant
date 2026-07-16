//! The shared actor seam: **who** an authenticated request acts as, resolved
//! once at the gate that authenticated it and carried — never re-derived —
//! to the write paths that record attribution (the agenda ledger today,
//! Memory P1's proposals next).
//!
//! Ruling contract (`~/agenda-a2-token-coordination.md`, steward RULING
//! 2026-07-16 — binding for both consumers):
//!
//! - **Construction is confined to authenticated edges.** An [`ActorBinding`]
//!   derives only from what a gate already computed — the MCP token binding
//!   plus its bound [`AccessPrincipal`], the authenticated dashboard-control
//!   grant, an `HttpAccessContext` principal, the ctl loopback posture.
//!   Never parse one from request fields or bodies; a caller that states no
//!   actor is explicitly [`ActorBinding::unattributed`], never a defaulted
//!   principal.
//! - **In-memory seam type, never a storage or wire format.** Tenants map it
//!   into their own versioned record fields (agenda → `AgendaActor`; Memory
//!   P1 → its proposal provenance) and must not serde this type into durable
//!   logs — that is what keeps later evolution additive. Serialization
//!   exists for in-process/tunnel plumbing only.
//! - **Identity only, never authorization results.** No ring, permission, or
//!   verb fields (tenant edges decide those from kind/principal + IAM); no
//!   zone/space fields (resource-side); no signer fields (op-envelope
//!   signing is tenant-side, e.g. P1's device-signed ops); no display
//!   strings (human labels are tenant-side enrichment at write time).
//! - **`principal_id` carries the IAM principal exactly as the gate names
//!   it** — no prettifying or lossy transformation. P1's exit test asserts
//!   recorded actor == token-bound principal.
//!
//! Relationship to the neighbouring types: `McpTokenBinding` is *token
//! classification* (how a request authenticated), `ActorBinding` is the
//! *resolved identity* (who it acts as), and `ToolCallerTrust` is the
//! *coarse trust posture* — the latter two must derive from the same gate
//! output at the same edge so they cannot skew.
//!
//! The session principal-binding token itself is the **existing**
//! session-scoped MCP token (ratified, Q1): a per-process secret with a
//! per-session SHA-256 derivation, injected into supervised sessions'
//! `INTENDANT_MCP_URL`, presented by ctl, and classified by the MCP gate.
//! Accepted properties (documented posture, not new exposure): lifetime is
//! the daemon process lifetime (a restart re-derives; supervised sessions
//! get fresh env at spawn/resume); possession-based; scoping and revocation
//! ride the existing `agent_session` IAM grant lifecycle.

use serde::{Deserialize, Serialize};

/// What class of authenticated caller an [`ActorBinding`] resolved from.
/// Day-one vocabulary per the ruling; additions are additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// A supervised agent session (session-scoped token possession, or the
    /// daemon's own root-equivalent process token acting for a named
    /// session). A5's scheduled-session principal is expected to arrive as
    /// this kind under its own principal id.
    AgentSession,
    /// A tokenless loopback process (bare `intendant ctl` on a plain local
    /// daemon) bound to the `local_process` principal.
    LocalProcess,
    /// A human dashboard surface: trusted-local browser, enrolled user
    /// client (mTLS/identity-key/passkey), or the packaged app.
    Dashboard,
    /// A federated peer daemon acting under its granted profile.
    Peer,
    /// No authenticated actor was stated. The explicit representation the
    /// contract requires — never substitute a defaulted principal.
    Unattributed,
}

impl ActorKind {
    /// The snake_case name tenants record in their own fields (matches the
    /// serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::AgentSession => "agent_session",
            ActorKind::LocalProcess => "local_process",
            ActorKind::Dashboard => "dashboard",
            ActorKind::Peer => "peer",
            ActorKind::Unattributed => "unattributed",
        }
    }
}

/// Who an authenticated request acts as — see the module docs for the
/// binding contract. Field set is the ratified day-one minimum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorBinding {
    pub kind: ActorKind,
    /// The IAM principal id exactly as the gate names it
    /// (e.g. `principal:agent-session:<slug>`), when one is bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// The supervised session this actor acts as, **only** when the gate
    /// bound one (session-scoped token possession, or root-equivalent
    /// process-token possession naming a session). Query-string session
    /// ids from unauthenticated-for-that-session callers must never land
    /// here — that is the forgeable-attribution hole this seam closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl ActorBinding {
    /// The explicit "no actor stated" value. Prefer this over `Option` so
    /// call sites cannot conflate "not yet threaded" with "anonymous".
    pub fn unattributed() -> Self {
        Self {
            kind: ActorKind::Unattributed,
            principal_id: None,
            session_id: None,
        }
    }

    /// An agent-session actor, from a gate that bound `session_id` by
    /// token possession (see the field docs).
    pub fn agent_session(principal_id: Option<String>, session_id: String) -> Self {
        Self {
            kind: ActorKind::AgentSession,
            principal_id,
            session_id: Some(session_id),
        }
    }

    /// A dashboard-surface actor. `principal_id: None` is the trusted-local
    /// posture (the owner at the machine, no named principal).
    pub fn dashboard(principal_id: Option<String>) -> Self {
        Self {
            kind: ActorKind::Dashboard,
            principal_id,
            session_id: None,
        }
    }

    /// A tokenless-loopback `local_process` actor.
    pub fn local_process(principal_id: Option<String>) -> Self {
        Self {
            kind: ActorKind::LocalProcess,
            principal_id,
            session_id: None,
        }
    }

    /// A federated-peer actor, named by the identity the peer gate binds
    /// (certificate fingerprint / peer principal id).
    pub fn peer(principal_id: Option<String>) -> Self {
        Self {
            kind: ActorKind::Peer,
            principal_id,
            session_id: None,
        }
    }

    /// Resolve the actor a gate-bound principal acts as. `gate_session` is
    /// the session identity the *gate itself* bound (session-scoped token
    /// possession, or root-equivalent process-token possession naming a
    /// session) — never a bare request field. Authenticated-edge use only,
    /// per the module contract.
    ///
    /// Classification order: the gates' own `authn` statements first —
    /// several gate constructors derive from `root_dashboard_session`
    /// (whose `kind` stays `root_session`) and state the real class in the
    /// authn record they push (`agent_session`, `loopback_mcp`) — then the
    /// principal `kind` for everything that names its class directly.
    pub fn from_principal(
        principal: &crate::access::iam::AccessPrincipal,
        gate_session: Option<String>,
    ) -> Self {
        let id = Some(principal.id.clone());
        if let Some(session_id) = gate_session {
            return Self::agent_session(id, session_id);
        }
        let authn_kind = |wanted: &str| {
            principal.authn.iter().any(|statement| {
                statement.get("kind").and_then(serde_json::Value::as_str) == Some(wanted)
            })
        };
        if authn_kind("loopback_mcp") {
            return Self::local_process(id);
        }
        if authn_kind("agent_session") {
            return Self {
                kind: ActorKind::AgentSession,
                principal_id: id,
                session_id: None,
            };
        }
        match principal.kind.as_str() {
            // An agent-session principal without a gate-bound session id
            // (shouldn't arise on token lanes; keep the kind honest).
            "agent_session" => Self {
                kind: ActorKind::AgentSession,
                principal_id: id,
                session_id: None,
            },
            "local_process" => Self::local_process(id),
            // The human dashboard surfaces: trusted-local root sessions,
            // enrolled user clients, account-backed identities.
            "root_session"
            | "browser_certificate"
            | "client_key"
            | "passkey_account"
            | "human_user"
            | "organization_group"
            | "connect_account"
            | "" => Self::dashboard(id),
            "peer_daemon" => Self::peer(id),
            // Unknown principal classes stay visibly unclassified while
            // still naming the principal the gate bound.
            _ => Self {
                kind: ActorKind::Unattributed,
                principal_id: id,
                session_id: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattributed_is_explicit_and_empty() {
        let actor = ActorBinding::unattributed();
        assert_eq!(actor.kind, ActorKind::Unattributed);
        assert_eq!(actor.principal_id, None);
        assert_eq!(actor.session_id, None);
    }

    #[test]
    fn only_agent_sessions_carry_session_ids() {
        assert_eq!(
            ActorBinding::dashboard(Some("principal:x".into())).session_id,
            None
        );
        assert_eq!(ActorBinding::local_process(None).session_id, None);
        assert_eq!(ActorBinding::peer(None).session_id, None);
        let agent = ActorBinding::agent_session(None, "sess-1".into());
        assert_eq!(agent.session_id.as_deref(), Some("sess-1"));
        assert_eq!(agent.kind, ActorKind::AgentSession);
    }

    /// Gate constructors that derive from `root_dashboard_session` keep
    /// `kind: root_session` and state their real class in the authn
    /// record — classification must read it (live bug: a bare-ctl
    /// loopback write recorded `dashboard` instead of `local_process`).
    #[test]
    fn root_derived_principals_classify_by_authn_statement() {
        let loopback = crate::access::iam::AccessPrincipal::local_loopback_mcp_default("http");
        let actor = ActorBinding::from_principal(&loopback, None);
        assert_eq!(actor.kind, ActorKind::LocalProcess);
        assert_eq!(actor.principal_id.as_deref(), Some(loopback.id.as_str()));

        let session = crate::access::iam::AccessPrincipal::supervised_agent_session_default(
            "sess-x", "http", true,
        );
        // Even without a gate-bound sid, the class stays agent-session.
        let actor = ActorBinding::from_principal(&session, None);
        assert_eq!(actor.kind, ActorKind::AgentSession);
        assert_eq!(actor.session_id, None);

        // The genuine trusted-local dashboard stays a dashboard actor.
        let dashboard =
            crate::access::iam::AccessPrincipal::root_dashboard_session("test", "https");
        let actor = ActorBinding::from_principal(&dashboard, None);
        assert_eq!(actor.kind, ActorKind::Dashboard);
    }

    /// In-process plumbing serialization round-trips; the shape is NOT a
    /// durable format (tenants map into their own versioned fields — the
    /// module-doc contract), so this pins behavior, not compatibility.
    #[test]
    fn plumbing_serialization_round_trips() {
        let actor = ActorBinding::agent_session(
            Some("principal:agent-session:abc".into()),
            "session-abc".into(),
        );
        let json = serde_json::to_string(&actor).unwrap();
        let back: ActorBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back, actor);
    }
}
