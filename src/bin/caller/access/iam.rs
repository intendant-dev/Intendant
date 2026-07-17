//! Local Access/IAM state.
//!
//! This is deliberately a local daemon-owned access model. The daemon can
//! distinguish trusted owner/root dashboard sessions, approved daemon peers,
//! browser/native mTLS identities, supervised agent sessions, and trusted local
//! processes. Browser `client_key` records are enrollment/audit records only in
//! this alpha: peer offers can verify them for attribution, but no request
//! ingress admits them as the authority-bearing IAM principal. Connect account
//! records are also discovery/audit metadata and never authenticate.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{AccessError, AccessResult};

pub const IAM_STATE_FILE: &str = "iam.json";
pub const BROWSER_MTLS_INITIALIZED_FILE: &str = "browser-mtls-root.initialized";
pub const IAM_SCHEMA_VERSION: u32 = 3;
const DEFAULT_HOSTED_ORIGIN: &str = "https://connect.intendant.dev";
/// Newest audit events retained inside `iam.json` (see `normalize`); the
/// same order of magnitude as the credential custody trail's in-memory cap.
const IAM_AUDIT_EVENTS_CAP: usize = 300;

fn default_schema_version() -> u32 {
    // A missing version is a legacy state and must run migrations.
    0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalIamState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub principals: Vec<IamPrincipal>,
    #[serde(default)]
    pub roles: Vec<IamRole>,
    #[serde(default)]
    pub grants: Vec<IamGrant>,
    #[serde(default)]
    pub audit_events: Vec<IamAuditEvent>,
    /// Legacy persisted hosted-control ceilings, retained for state-file
    /// compatibility. The default build compiles both binding kinds to
    /// `role:none`: Connect account records never authenticate, and client
    /// keys arriving through Connect (or enrolled by a hosted origin) never
    /// exercise daemon control. Loading state rewrites every stored value to
    /// `role:none`, so neither an older state file nor a hand edit can raise
    /// hosted authority.
    #[serde(default = "default_role_ceilings")]
    pub role_ceilings: std::collections::BTreeMap<String, String>,
    /// Origins treated as hosted (low-provenance) app sources when recorded
    /// on a client key's enrollment binding.
    #[serde(default = "default_hosted_origins")]
    pub hosted_origins: Vec<String>,
    /// Organizations whose signed grant documents this daemon accepts
    /// (phase 6). Trusting an org is a local root-session decision, and
    /// `max_role` caps what its documents may grant here.
    #[serde(default)]
    pub trusted_orgs: Vec<TrustedOrg>,
    /// Trust tier of this daemon (docs/src/trust-tiers.md): what a
    /// compromise of this box would cost its owner. `integrated` = holds
    /// the owner's personal world; `disposable` = scratch box, nothing
    /// durable; `None` = the owner has not chosen. The tier is doctrine
    /// carried to grant flows and the Access UI — it grants and denies
    /// nothing by itself; ceilings and grants do the enforcing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Optional fleet-name lease lane. These records are written only by
    /// daemon-local hosted-control transactions; Connect never writes IAM.
    #[serde(default)]
    pub hosted_control: super::hosted_control::HostedControlState,
}

/// The daemon tier vocabulary, in UI presentation order. Single source for
/// the wire values and the dashboard's static mirror (pinned by the
/// `dashboard_tier_vocabulary_mirrors_daemon_tiers` parity test).
pub const DAEMON_TIERS: [&str; 2] = ["integrated", "disposable"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrustedOrg {
    pub handle: String,
    pub root_key: String,
    #[serde(default)]
    pub max_role: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub added_at_unix_ms: Option<u64>,
    /// Cap for org grants whose subject is a peer daemon, in the peer
    /// profile vocabulary. Empty means fail-closed: trusting an org grants
    /// no daemon-to-daemon authority until the owner raises this — the
    /// human and peer lanes are separate trust decisions.
    #[serde(default)]
    pub max_peer_profile: String,
    /// Highest org revocation list `seq` applied on this daemon; lists at
    /// or below it are idempotently ignored.
    #[serde(default)]
    pub last_orl_seq: u64,
    /// The applied list's entries, persisted so materialization and
    /// renewal refuse revoked grant ids / subjects that were never
    /// materialized here in the first place.
    #[serde(default)]
    pub orl_revoked_grant_ids: Vec<String>,
    #[serde(default)]
    pub orl_revoked_subjects: Vec<String>,
    #[serde(default)]
    pub orl_revoked_issuer_keys: Vec<String>,
}

fn default_role_ceilings() -> std::collections::BTreeMap<String, String> {
    let mut ceilings = std::collections::BTreeMap::new();
    ceilings.insert("connect_account".to_string(), "role:none".to_string());
    ceilings.insert("client_key".to_string(), "role:none".to_string());
    ceilings
}

pub fn default_hosted_origins() -> Vec<String> {
    vec![DEFAULT_HOSTED_ORIGIN.to_string()]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamPrincipal {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub account: Option<Value>,
    #[serde(default)]
    pub organization: Option<Value>,
    #[serde(default)]
    pub authn: Vec<Value>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub created_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamRole {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamGrant {
    pub id: String,
    pub principal_id: String,
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub role_id: String,
    #[serde(default)]
    pub policy_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub created_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub revoked_at_unix_ms: Option<u64>,
    /// When set, the grant stops being enforced after this instant without
    /// changing its stored status; the overview then reports it as
    /// `expired`. Backbone for temporary human grants and for org grants,
    /// whose documents must carry an expiry.
    #[serde(default)]
    pub expires_at_unix_ms: Option<u64>,
    /// The delegated issuer key that signed the org document this grant
    /// was materialized from, when it was not the org root (step 6b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_via: Option<String>,
    /// Filesystem scope for this grant. `None` = unrestricted (the
    /// pre-scoping behavior); `Some` = mediated file surfaces are
    /// confined to these roots for every principal kind, humans
    /// included. Enforcement shares `filesystem_access_allowed` with
    /// peer scoping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_scope: Option<crate::access::access_policy::FilesystemAccessPolicy>,
}

impl IamGrant {
    /// Active right now: enforced status and not past expiry.
    pub fn is_active_at(&self, now_unix_ms: i64) -> bool {
        if !is_enforced_status(&self.status) {
            return false;
        }
        match self.expires_at_unix_ms {
            Some(expires) => (now_unix_ms as u128) < (expires as u128),
            None => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamAuditEvent {
    pub id: String,
    #[serde(default)]
    pub at_unix_ms: Option<u64>,
    #[serde(default)]
    pub actor_principal_id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserClientGrantUpsertRequest {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Browser identity-key fingerprint (base64url of sha256 over the raw
    /// P-256 point). Distinct from `fingerprint`, which is the hex mTLS
    /// certificate fingerprint.
    #[serde(default)]
    pub client_key_fingerprint: Option<String>,
    /// Optional full public key (base64url raw point) kept for audit/display.
    #[serde(default)]
    pub client_key: Option<String>,
    /// Origin the key was recorded from. Active pure-key grants from hosted
    /// or fleet origins are refused; an mTLS-backed human may retain the key
    /// and its origin as non-authoritative metadata.
    #[serde(default)]
    pub client_key_origin: Option<String>,
    /// Intendant session id for `agent_session` principals — binds a grant
    /// to the supervised agent driving that session over `/mcp`. The
    /// wildcard `"*"` scopes every supervised agent session at once.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub account_name: Option<String>,
    #[serde(default)]
    pub account_provider: Option<String>,
    #[serde(default)]
    pub handle: Option<String>,
    #[serde(default)]
    pub verified_provider: Option<String>,
    #[serde(default)]
    pub organization_id: Option<String>,
    #[serde(default)]
    pub organization_name: Option<String>,
    #[serde(default)]
    pub role_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    /// Optional absolute expiry; the grant stops enforcing after this
    /// instant. Must be in the future when set.
    #[serde(default)]
    pub expires_at_unix_ms: Option<u64>,
    /// Filesystem roots this grant may read / write through the mediated
    /// file surfaces. Both empty = unrestricted (no scope stored).
    #[serde(default)]
    pub fs_read_roots: Vec<String>,
    #[serde(default)]
    pub fs_write_roots: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserClientGrantUpsertResult {
    pub principal: IamPrincipal,
    pub grant: IamGrant,
    pub created_principal: bool,
    pub created_grant: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IamGrantUpdateRequest {
    pub grant_id: String,
    #[serde(default)]
    pub role_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamGrantUpdateResult {
    pub principal: IamPrincipal,
    pub grant: IamGrant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IamStateStatus {
    Missing,
    Loaded,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedIamState {
    pub path: PathBuf,
    pub state: LocalIamState,
    pub status: IamStateStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessPrincipal {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub source: String,
    pub role_id: String,
    #[serde(default)]
    pub grant_id: Option<String>,
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub peer_profile: Option<String>,
    #[serde(default)]
    pub account: Option<Value>,
    #[serde(default)]
    pub organization: Option<Value>,
    #[serde(default)]
    pub authn: Vec<Value>,
    /// The authn binding kind that actually authenticated this session (for
    /// example `browser_mtls_cert`, `agent_session`, or `loopback_mcp`). Legacy
    /// `client_key` / `connect_account` values can remain in stored records.
    /// Peer offers may verify a client key for attribution, but no alpha request
    /// ingress admits either kind as its controlling IAM principal. Role ceilings
    /// key off this, not the principal kind, because one principal (a
    /// `human_user`) can carry several bindings of different provenance.
    #[serde(default)]
    pub authn_kind: Option<String>,
    /// Normalized value of the binding that authenticated this request
    /// (certificate fingerprint, session id, local-process id, …). Legacy
    /// browser-key fingerprints can remain in stored/session records, but are
    /// not an alpha ingress proof.
    /// This is deliberately distinct from the principal id: one human
    /// principal may carry several certificates, and follow-up signaling
    /// must stay bound to the exact certificate that opened the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authn_binding: Option<String>,
    /// The origin recorded on the matched binding at grant time, when the
    /// binding carries one (client keys do).
    #[serde(default)]
    pub authn_origin: Option<String>,
    /// Legacy daemon-stamped route provenance. `true` identifies a session
    /// attributed to the retired hosted Connect offer lane and makes the
    /// evaluator fail closed; no alpha ingress creates such a session.
    /// Stored/client-sent origin strings cannot clear it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hosted_connect: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessDecision {
    pub allowed: bool,
    pub principal_id: String,
    pub principal_kind: String,
    pub permission: String,
    pub reason: String,
}

impl AccessPrincipal {
    /// True for the owner's own control surfaces: the trusted-local
    /// dashboard (which keeps the root dashboard grant id, minted nowhere
    /// else) plus the documented anything-with-a-shell local loopback
    /// principal. The derived
    /// transport defaults — supervised agent sessions and MCP token
    /// holders — share the `root_session` *kind* but drop the grant id,
    /// so this predicate must never be widened to the kind alone: those
    /// callers are root-compatible for IAM yet are exactly who the
    /// user-display grant exists to hold.
    pub fn is_owner_surface(&self) -> bool {
        self.grant_id.as_deref() == Some("grant:root:dashboard")
            || self.id == "principal:local-process:loopback"
    }

    /// True when this request was authenticated by an independently enrolled
    /// browser mTLS certificate whose active local-IAM grant is `role:root`.
    ///
    /// This is intentionally separate from [`Self::is_owner_surface`]. The
    /// latter identifies synthetic/local ambient owner surfaces without
    /// consulting an IAM grant; widening it to every root-compatible principal
    /// would also admit supervised agents and MCP token holders. Callers use
    /// this narrower predicate only after their normal live-IAM operation gate
    /// has accepted the request.
    pub fn is_enrolled_root_mtls_user_client(&self) -> bool {
        matches!(self.kind.as_str(), "browser_certificate" | "human_user")
            && self.role_id == "role:root"
            && self.authn_kind.as_deref() == Some("browser_mtls_cert")
            && self
                .authn_binding
                .as_deref()
                .is_some_and(|binding| !binding.trim().is_empty())
            && self
                .grant_id
                .as_deref()
                .is_some_and(|grant_id| !grant_id.trim().is_empty())
            && !self.hosted_connect
    }

    pub fn root_dashboard_session(source: impl Into<String>, transport: impl Into<String>) -> Self {
        Self {
            id: "principal:root:dashboard".to_string(),
            kind: "root_session".to_string(),
            label: "Root dashboard session".to_string(),
            source: source.into(),
            role_id: "role:root".to_string(),
            grant_id: Some("grant:root:dashboard".to_string()),
            transport: transport.into(),
            peer_profile: None,
            account: None,
            organization: None,
            authn: Vec::new(),
            authn_kind: None,
            authn_binding: None,
            authn_origin: None,
            hosted_connect: false,
        }
    }

    /// A certificate accepted by the access CA but not enrolled in local
    /// IAM. TLS authentication proves possession of a CA-issued certificate;
    /// it does not itself grant daemon authority after the one-time root
    /// initialization. Carry the real fingerprint into audit/UI surfaces and
    /// let the central IAM evaluator fail closed on the absent grant.
    pub fn ungranted_browser_mtls(fingerprint: Option<&str>, transport: impl Into<String>) -> Self {
        let fingerprint = fingerprint
            .map(normalize_browser_mtls_fingerprint)
            .unwrap_or_default();
        let short = if fingerprint.is_empty() {
            "unresolved".to_string()
        } else {
            fingerprint.chars().take(12).collect()
        };
        let mut authn = serde_json::Map::new();
        authn.insert(
            "kind".to_string(),
            Value::String("browser_mtls_cert".to_string()),
        );
        authn.insert(
            "label".to_string(),
            Value::String("Browser mTLS certificate".to_string()),
        );
        if !fingerprint.is_empty() {
            authn.insert(
                "fingerprint".to_string(),
                Value::String(fingerprint.clone()),
            );
        }
        Self {
            id: format!("principal:browser-cert:{short}"),
            kind: "browser_certificate".to_string(),
            label: if fingerprint.is_empty() {
                "Unresolved browser certificate".to_string()
            } else {
                format!("Unenrolled browser certificate {short}")
            },
            source: "browser-mtls-ungranted".to_string(),
            role_id: "role:none".to_string(),
            grant_id: None,
            transport: transport.into(),
            peer_profile: None,
            account: None,
            organization: None,
            authn: vec![Value::Object(authn)],
            authn_kind: Some("browser_mtls_cert".to_string()),
            authn_binding: if fingerprint.is_empty() {
                None
            } else {
                Some(fingerprint)
            },
            authn_origin: None,
            hosted_connect: false,
        }
    }

    /// Authority-free HTTP bytes (the dashboard shell/static assets, public
    /// discovery, and signed-document doorbells) do not need a controlling
    /// principal. Keep those requests at `role:none` even on loopback and
    /// even when a browser happens to present a client certificate, so merely
    /// fetching public bytes cannot enter root/IAM resolution.
    pub fn authority_free_http(transport: impl Into<String>) -> Self {
        Self {
            id: "principal:anonymous:http".to_string(),
            kind: "anonymous".to_string(),
            label: "Authority-free HTTP request".to_string(),
            source: "public-http".to_string(),
            role_id: "role:none".to_string(),
            grant_id: None,
            transport: transport.into(),
            peer_profile: None,
            account: None,
            organization: None,
            authn: Vec::new(),
            authn_kind: None,
            authn_binding: None,
            authn_origin: None,
            hosted_connect: false,
        }
    }

    /// A supervised agent session bound to `/mcp` by session id that has no
    /// local IAM grant of its own. The transport is daemon-trusted (the
    /// request either carried a token this daemon minted or arrived on the
    /// trusted local path), so the session keeps root-compatible authority —
    /// but the identity is real: the id, label, and `authn` binding name the
    /// session so audit output and the Access UI can scope it later.
    /// `authenticated` records whether the presented token was derived for
    /// this exact session id (true) or was the shared per-process token with
    /// the session id as advisory metadata (false).
    pub fn supervised_agent_session_default(
        session_id: &str,
        transport: impl Into<String>,
        authenticated: bool,
    ) -> Self {
        let mut principal = Self::root_dashboard_session(
            if authenticated {
                "mcp-session-token"
            } else {
                "mcp-shared-token"
            },
            transport,
        );
        principal.id = format!("principal:agent-session:{}", slug_component(session_id));
        principal.label = format!("Supervised agent session {}", short_id(session_id));
        principal.grant_id = None;
        principal.authn.push(serde_json::json!({
            "kind": "agent_session",
            "label": "Supervised agent session",
            "session_id": session_id,
            "session_token": authenticated,
        }));
        principal
    }

    /// A caller that proved possession of this daemon's per-process MCP
    /// token without naming a session — the supervising controller itself or
    /// an operator shell that inherited the injected URL.
    pub fn mcp_token_holder(transport: impl Into<String>) -> Self {
        let mut principal = Self::root_dashboard_session("mcp-loopback-token", transport);
        principal.id = "principal:mcp-token-holder".to_string();
        principal.label = "MCP token holder".to_string();
        principal.grant_id = None;
        principal
    }

    /// A tokenless `/mcp` caller on the loopback interface of a daemon whose
    /// transport posture admits it (no browser origin markers). This is the
    /// documented "anything with a shell on this host" path: root-compatible
    /// by default so bare `intendant ctl` keeps working, but carried as its
    /// own principal so the owner can scope or revoke it with a
    /// `local_process` IAM grant.
    pub fn local_loopback_mcp_default(transport: impl Into<String>) -> Self {
        let mut principal = Self::root_dashboard_session("mcp-loopback-cleartext", transport);
        principal.id = "principal:local-process:loopback".to_string();
        principal.label = "Local loopback MCP client".to_string();
        principal.grant_id = None;
        principal.authn.push(serde_json::json!({
            "kind": "loopback_mcp",
            "label": "Loopback MCP client",
            "scope": "loopback",
        }));
        principal
    }

    pub fn peer_daemon(
        fingerprint: impl Into<String>,
        label: impl Into<String>,
        profile: impl Into<String>,
        transport: impl Into<String>,
    ) -> Self {
        let fingerprint = fingerprint.into();
        let profile = profile.into();
        let label = label.into();
        Self {
            id: format!("principal:peer-daemon:{fingerprint}"),
            kind: "peer_daemon".to_string(),
            label: if label.trim().is_empty() {
                fingerprint.clone()
            } else {
                label
            },
            source: "peer_identity_store".to_string(),
            role_id: format!("role:peer-profile:{profile}"),
            grant_id: Some(format!("grant:peer-profile:{fingerprint}")),
            transport: transport.into(),
            peer_profile: Some(profile),
            account: None,
            organization: None,
            authn: Vec::new(),
            authn_kind: None,
            authn_binding: None,
            authn_origin: None,
            hosted_connect: false,
        }
    }

    pub fn local_user_client(
        principal: &IamPrincipal,
        grant: &IamGrant,
        transport: impl Into<String>,
    ) -> Self {
        let role_id = if grant.role_id.trim().is_empty() {
            "role:scoped-human".to_string()
        } else {
            grant.role_id.clone()
        };
        let kind = if principal.kind.trim().is_empty() {
            "human_user".to_string()
        } else {
            principal.kind.clone()
        };
        Self {
            id: principal.id.clone(),
            kind,
            label: if principal.label.trim().is_empty() {
                principal.id.clone()
            } else {
                principal.label.clone()
            },
            source: if principal.source.trim().is_empty() {
                "local_iam_state".to_string()
            } else {
                principal.source.clone()
            },
            role_id,
            grant_id: Some(grant.id.clone()),
            transport: transport.into(),
            peer_profile: None,
            account: principal.account.clone(),
            organization: principal.organization.clone(),
            authn: principal.authn.clone(),
            authn_kind: None,
            authn_binding: None,
            authn_origin: None,
            hosted_connect: false,
        }
    }

    pub fn as_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

impl AccessDecision {
    pub fn allowed(
        principal: &AccessPrincipal,
        op: crate::access::access_policy::PeerOperation,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            allowed: true,
            principal_id: principal.id.clone(),
            principal_kind: principal.kind.clone(),
            permission: operation_permission_id(op).to_string(),
            reason: reason.into(),
        }
    }

    pub fn denied(
        principal: &AccessPrincipal,
        op: crate::access::access_policy::PeerOperation,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            allowed: false,
            principal_id: principal.id.clone(),
            principal_kind: principal.kind.clone(),
            permission: operation_permission_id(op).to_string(),
            reason: reason.into(),
        }
    }

    pub fn ensure_allowed(self) -> Result<(), String> {
        if self.allowed {
            Ok(())
        } else {
            Err(self.reason)
        }
    }
}

impl IamStateStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Loaded => "loaded",
            Self::Error(_) => "error",
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Error(err) => Some(err.as_str()),
            _ => None,
        }
    }
}

impl Default for LocalIamState {
    fn default() -> Self {
        Self {
            schema_version: IAM_SCHEMA_VERSION,
            principals: Vec::new(),
            roles: builtin_role_templates(),
            grants: Vec::new(),
            audit_events: Vec::new(),
            role_ceilings: default_role_ceilings(),
            hosted_origins: default_hosted_origins(),
            trusted_orgs: Vec::new(),
            tier: None,
            hosted_control: super::hosted_control::HostedControlState::default(),
        }
    }
}

impl LocalIamState {
    fn normalize(mut self) -> Self {
        if self.schema_version < 2 {
            // Alpha migration: earlier first-claim code could mint a
            // role:root client-key grant with `connect-bootstrap` origin,
            // and the retired CLI `--owner` path could pin root to a browser
            // identity key whose hosted provenance was unknowable. Revoke
            // both rather than silently downgrading them, and require trusted
            // direct-mTLS re-enrollment. Existing Connect account-route
            // records live outside IAM and survive as metadata.
            for binding in HOSTED_CEILING_BINDINGS {
                self.role_ceilings
                    .insert(binding.to_string(), "role:none".to_string());
            }
            let legacy_principals: std::collections::BTreeSet<String> = self
                .principals
                .iter()
                .filter(|principal| {
                    principal.authn.iter().any(|authn| {
                        authn.get("kind").and_then(Value::as_str) == Some("client_key")
                            && authn.get("origin").and_then(Value::as_str)
                                == Some("connect-bootstrap")
                    })
                })
                .map(|principal| principal.id.clone())
                .collect();
            let now = now_unix_ms();
            let mut connect_revoked = 0usize;
            let mut owner_bootstrap_revoked = 0usize;
            for grant in &mut self.grants {
                if legacy_principals.contains(&grant.principal_id)
                    && is_enforced_status(&grant.status)
                {
                    grant.status = "revoked".to_string();
                    grant.revoked_at_unix_ms = Some(now);
                    connect_revoked += 1;
                } else if (grant.reason.starts_with("--owner bootstrap:")
                    || grant
                        .reason
                        .starts_with("trusted local --owner enrollment:"))
                    && is_enforced_status(&grant.status)
                {
                    grant.status = "revoked".to_string();
                    grant.revoked_at_unix_ms = Some(now);
                    owner_bootstrap_revoked += 1;
                }
            }
            if connect_revoked > 0 {
                self.audit_events.push(IamAuditEvent {
                    id: format!("audit:migrate-hosted-root-v2:{}", self.audit_events.len() + 1),
                    at_unix_ms: Some(now),
                    actor_principal_id: "principal:system:migration".to_string(),
                    action: "revoke_legacy_connect_bootstrap".to_string(),
                    target_id: "connect-bootstrap".to_string(),
                    summary: format!(
                        "Revoked {connect_revoked} legacy Connect first-claim grant(s); trusted re-enrollment required"
                    ),
                });
            }
            if owner_bootstrap_revoked > 0 {
                self.audit_events.push(IamAuditEvent {
                    id: format!("audit:migrate-owner-bootstrap-v2:{}", self.audit_events.len() + 1),
                    at_unix_ms: Some(now),
                    actor_principal_id: "principal:system:migration".to_string(),
                    action: "revoke_legacy_owner_browser_key_bootstrap".to_string(),
                    target_id: "owner-bootstrap".to_string(),
                    summary: format!(
                        "Revoked {owner_bootstrap_revoked} retired --owner browser-key grant(s); direct-mTLS re-enrollment required"
                    ),
                });
            }
        }
        // Schema 3 adds only serde-defaulted hosted-control state. Persist the
        // normalized version even when the input was already schema 2; keeping
        // the assignment inside the legacy `< 2` migration would make every
        // schema-2 load perpetually migration-required.
        if self.schema_version < IAM_SCHEMA_VERSION {
            self.schema_version = IAM_SCHEMA_VERSION;
        }
        // Default-product invariant: hosted-origin code is a discovery and
        // routing client, never a daemon-control principal. This is compiled
        // policy, not a mutable owner preference; normalize every persisted
        // copy back to role:none so hand edits and older alpha state cannot
        // re-enable the retired hosted-control experiment.
        for binding in HOSTED_CEILING_BINDINGS {
            self.role_ceilings
                .insert(binding.to_string(), "role:none".to_string());
        }
        let templates = builtin_role_templates();
        for role in templates.iter().cloned() {
            match self
                .roles
                .iter_mut()
                .find(|existing| existing.id == role.id)
            {
                // Builtin role definitions are owned by the binary, not the
                // state file: refresh persisted copies to the current
                // template so semantic migrations (e.g. the terminal.use →
                // view/write/spawn split, or a permission added later like
                // credentials.manage) propagate on upgrade. Roles under
                // custom ids are untouched.
                Some(existing) if existing.source == "builtin" => *existing = role,
                Some(_) => {}
                None => self.roles.push(role),
            }
        }
        // The same ownership cuts the other way: a persisted builtin role
        // the binary no longer ships (e.g. the never-enforced
        // role:directory-files, superseded by grant-level fs scopes) is
        // dropped on load. Grants could never reference planned roles, and
        // a grant on any removed role fails closed in the evaluator.
        self.roles.retain(|role| {
            role.source != "builtin" || templates.iter().any(|template| template.id == role.id)
        });
        self.principals.retain(|p| !p.id.trim().is_empty());
        self.roles.retain(|r| !r.id.trim().is_empty());
        self.grants
            .retain(|g| !g.id.trim().is_empty() && !g.principal_id.trim().is_empty());
        self.audit_events.retain(|e| !e.id.trim().is_empty());
        self.hosted_control.normalize();
        // Bound the audit trail to a recent tail, mirroring the credential
        // custody trail's in-memory cap (`credential_audit::MEM_CAP`).
        // Events are appended chronologically, so the head is the oldest.
        // The evaluator never reads `audit_events` — retention is a
        // forensics window, not an authorization input — and an uncapped
        // vec otherwise rides every request-path load, per-mutation
        // pretty-print + fsync, and `/api/access/iam/state` response for
        // the daemon's lifetime.
        if self.audit_events.len() > IAM_AUDIT_EVENTS_CAP {
            let excess = self.audit_events.len() - IAM_AUDIT_EVENTS_CAP;
            self.audit_events.drain(..excess);
        }
        self
    }

    pub fn managed_principal_count(&self) -> usize {
        self.principals
            .iter()
            .filter(|p| p.source != "builtin")
            .count()
    }

    pub fn managed_grant_count(&self) -> usize {
        self.grants.iter().filter(|g| g.source != "builtin").count()
    }
}

pub fn iam_state_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(IAM_STATE_FILE)
}

pub fn browser_mtls_initialized_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(BROWSER_MTLS_INITIALIZED_FILE)
}

fn browser_mtls_initialization_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn principal_has_browser_mtls_fingerprint(principal: &IamPrincipal, fingerprint: &str) -> bool {
    principal.authn.iter().any(|authn| {
        authn.get("kind").and_then(Value::as_str) == Some("browser_mtls_cert")
            && authn
                .get("fingerprint")
                .and_then(Value::as_str)
                .map(normalize_browser_mtls_fingerprint)
                .as_deref()
                == Some(fingerprint)
    })
}

fn state_has_browser_mtls_binding_for(state: &LocalIamState, fingerprint: &str) -> bool {
    state
        .principals
        .iter()
        .any(|principal| principal_has_browser_mtls_fingerprint(principal, fingerprint))
}

fn state_has_browser_mtls_root_history(state: &LocalIamState) -> bool {
    state.principals.iter().any(|principal| {
        let browser_cert = principal
            .authn
            .iter()
            .any(|authn| authn.get("kind").and_then(Value::as_str) == Some("browser_mtls_cert"));
        browser_cert
            && state
                .grants
                .iter()
                .any(|grant| grant.principal_id == principal.id && grant.role_id == "role:root")
    })
}

fn write_browser_mtls_initialized_marker(cert_dir: &Path) -> AccessResult<()> {
    std::fs::create_dir_all(cert_dir)?;
    let path = browser_mtls_initialized_path(cert_dir);
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    file.write_all(b"version=1\n")?;
    file.sync_all()?;
    set_private_perms(&path)?;
    Ok(())
}

fn upsert_browser_mtls_owner_root(
    state: &mut LocalIamState,
    fingerprint: &str,
    source: &str,
) -> AccessResult<bool> {
    if principal_for_browser_mtls_cert(state, fingerprint, source)
        .is_some_and(|principal| principal.role_id == "role:root")
    {
        return Ok(false);
    }
    let short: String = fingerprint.chars().take(12).collect();
    let actor = AccessPrincipal::root_dashboard_session(source, "trusted-local-setup");
    upsert_user_client_grant(
        state,
        UserClientGrantUpsertRequest {
            kind: "browser_certificate".to_string(),
            fingerprint: Some(fingerprint.to_string()),
            label: Some(format!("Owner browser certificate {short}")),
            role_id: Some("role:root".to_string()),
            status: Some("active".to_string()),
            reason: Some(
                "locally generated owner client certificate pinned as direct mTLS root".to_string(),
            ),
            ..Default::default()
        },
        &actor,
    )?;
    Ok(true)
}

/// Trusted-console setup hook: pin the generated `client.crt` as an
/// explicit local-IAM root and make the one-time initialization sticky.
/// Re-running setup is idempotent; `access setup --force` generates a new
/// owner certificate and therefore adds/updates the new fingerprint before
/// returning, rather than leaving the operator locked out behind the old
/// marker.
pub fn seed_generated_browser_mtls_owner_root(cert_dir: &Path) -> AccessResult<bool> {
    let fingerprint = super::certs::read_owner_client_cert_fingerprint(cert_dir)?;
    let _guard = browser_mtls_initialization_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    transact_state(cert_dir, |state, transaction| {
        let changed =
            upsert_browser_mtls_owner_root(state, &fingerprint, "access-setup-owner-certificate")?;
        // The sticky marker must never outrun the root binding. Persist even
        // on the idempotent path so a pending schema migration is durable
        // before the marker is created.
        transaction.persist_now(state)?;
        write_browser_mtls_initialized_marker(cert_dir)?;
        Ok((changed, false))
    })
}

/// Resolve the compatibility migration for pre-marker direct-mTLS installs.
///
/// This primitive is used only by trusted setup/startup paths. Request
/// authentication is read-only and must never call it. The caller passes the
/// fingerprint of the exact locally generated `client.crt`; CA validity alone
/// is deliberately insufficient because peer/scoped certificates share that
/// CA. An already-persisted browser-certificate binding is honored, while
/// unknown CA-valid certificates remain ungranted. A separate sticky marker
/// records completion so losing `iam.json` can never make a later certificate
/// root.
pub fn initialize_browser_mtls_root_if_needed(
    cert_dir: &Path,
    fingerprint: &str,
) -> AccessResult<LocalIamState> {
    let fingerprint = normalize_browser_mtls_fingerprint(fingerprint);
    if fingerprint.is_empty() {
        return Err(AccessError(
            "verified browser mTLS certificate has no fingerprint".to_string(),
        ));
    }
    let _guard = browser_mtls_initialization_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    transact_state(cert_dir, |state, transaction| {
        let marker_path = browser_mtls_initialized_path(cert_dir);
        if marker_path.exists() {
            return Ok((state.clone(), false));
        }

        // Root history on any browser cert proves this store was initialized;
        // backfill the sticky marker so an unknown certificate cannot become a
        // second root.
        if state_has_browser_mtls_root_history(state) {
            transaction.persist_now(state)?;
            write_browser_mtls_initialized_marker(cert_dir)?;
            return Ok((state.clone(), false));
        }

        let client_cert_path = cert_dir.join("client.crt");
        if !client_cert_path.exists() {
            return Ok((state.clone(), false));
        }
        let owner_fingerprint = super::certs::read_owner_client_cert_fingerprint(cert_dir)?;
        if fingerprint != owner_fingerprint {
            // A scoped/revoked binding for some other CA-valid certificate is
            // honored by the caller, but it must not consume the one-time owner
            // initialization marker and lock out the locally generated owner.
            return Ok((state.clone(), false));
        }
        // An explicit binding for the locally generated owner certificate
        // (including scoped or revoked) is a deliberate local-IAM decision and
        // outranks compatibility bootstrap. Only this exact owner binding — not
        // an arbitrary scoped certificate that happens to arrive first — may
        // backfill the global marker.
        if state_has_browser_mtls_binding_for(state, &owner_fingerprint) {
            transaction.persist_now(state)?;
            write_browser_mtls_initialized_marker(cert_dir)?;
            return Ok((state.clone(), false));
        }
        upsert_browser_mtls_owner_root(
            state,
            &fingerprint,
            "browser-mtls-owner-certificate-migration",
        )?;
        // State first, marker second. If marker creation fails, the persisted
        // root history makes the next attempt backfill the marker without ever
        // granting a second certificate.
        transaction.persist_now(state)?;
        write_browser_mtls_initialized_marker(cert_dir)?;
        Ok((state.clone(), false))
    })
}

/// Trusted daemon-startup migration for access stores created before setup
/// persisted the generated owner certificate in local IAM.
///
/// This is deliberately separate from request authentication: fetching a
/// dashboard asset or API route must never mint root. The migration is rooted
/// only in the exact `client.crt` already generated in the daemon-owned access
/// directory, and [`initialize_browser_mtls_root_if_needed`] preserves an
/// explicit scoped/revoked binding instead of upgrading it.
pub fn migrate_generated_browser_mtls_owner_root_at_startup(cert_dir: &Path) -> AccessResult<bool> {
    let marker = browser_mtls_initialized_path(cert_dir);
    if marker.exists() || !cert_dir.join("client.crt").exists() {
        return Ok(false);
    }
    let fingerprint = super::certs::read_owner_client_cert_fingerprint(cert_dir)?;
    let _ = initialize_browser_mtls_root_if_needed(cert_dir, &fingerprint)?;
    Ok(marker.exists())
}

fn load_state_unlocked(cert_dir: &Path) -> AccessResult<(LocalIamState, bool)> {
    let path = iam_state_path(cert_dir);
    if !path.exists() {
        return Ok((LocalIamState::default(), false));
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| AccessError(format!("read {}: {e}", path.display())))?;
    let state: LocalIamState = serde_json::from_str(&contents)
        .map_err(|e| AccessError(format!("parse {}: {e}", path.display())))?;
    let migration_required = state.schema_version < IAM_SCHEMA_VERSION;
    Ok((state.normalize(), migration_required))
}

pub fn load_state(cert_dir: &Path) -> AccessResult<LocalIamState> {
    let (state, migration_required) = load_state_unlocked(cert_dir)?;
    if !migration_required {
        return Ok(state);
    }
    // A schema bump can revoke authority. Serialize and durably persist it
    // before returning so another process cannot overwrite the revocation
    // with a stale pre-migration snapshot.
    transact_state(cert_dir, |latest, _transaction| Ok((latest.clone(), false)))
}

pub fn load_state_for_overview(cert_dir: &Path) -> LoadedIamState {
    let path = iam_state_path(cert_dir);
    if !path.exists() {
        return LoadedIamState {
            path,
            state: LocalIamState::default(),
            status: IamStateStatus::Missing,
        };
    }
    match load_state(cert_dir) {
        Ok(state) => LoadedIamState {
            path,
            state,
            status: IamStateStatus::Loaded,
        },
        Err(err) => LoadedIamState {
            path,
            state: LocalIamState::default(),
            status: IamStateStatus::Error(err.to_string()),
        },
    }
}

pub(crate) struct IamStateTransaction<'a> {
    cert_dir: &'a Path,
    persisted: bool,
}

impl IamStateTransaction<'_> {
    /// Persist the current IAM snapshot immediately while retaining the
    /// authority-store lock. Multi-file operations use this to commit grants
    /// before enabling peer authority; revocations do the inverse and write
    /// the peer record before the transaction's final IAM commit.
    pub(crate) fn persist_now(&mut self, state: &LocalIamState) -> AccessResult<()> {
        save_state_locked(self.cert_dir, state)?;
        self.persisted = true;
        Ok(())
    }
}

/// Serialize one fresh-load → mutate → durable-persist IAM operation across
/// threads and processes. The closure's boolean reports whether it changed
/// state; schema migrations persist regardless. A closure that needs a
/// fail-closed multi-file ordering may call `persist_now` before its external
/// side effect and return `false` to avoid a duplicate write.
pub(crate) fn transact_state<T>(
    cert_dir: &Path,
    operation: impl FnOnce(&mut LocalIamState, &mut IamStateTransaction<'_>) -> AccessResult<(T, bool)>,
) -> AccessResult<T> {
    super::authority_store::with_lock(cert_dir, || {
        let (mut state, migration_required) = load_state_unlocked(cert_dir)?;
        let mut transaction = IamStateTransaction {
            cert_dir,
            persisted: false,
        };
        let (result, changed) = operation(&mut state, &mut transaction)?;
        if (changed || migration_required) && !transaction.persisted {
            transaction.persist_now(&state)?;
        }
        Ok(result)
    })
}

fn save_state_locked(cert_dir: &Path, state: &LocalIamState) -> AccessResult<()> {
    let path = iam_state_path(cert_dir);
    let normalized = state.clone().normalize();
    let mut contents = serde_json::to_vec_pretty(&normalized)
        .map_err(|e| AccessError(format!("serialize {}: {e}", path.display())))?;
    contents.push(b'\n');
    super::authority_store::atomic_write_private_locked(&path, &contents)?;
    // Refresh the read cache with the state just persisted, keyed by the
    // renamed file's fresh fingerprint. Best-effort: the per-read
    // fingerprint re-check below is the correctness backbone, so a lost
    // refresh only costs one re-parse.
    if let Some(fingerprint) = iam_state_fingerprint(&path) {
        let _ = store_cached_iam_state(&path, fingerprint, normalized);
    }
    Ok(())
}

/// Full-state replacement for fixtures. Production read-modify-write paths
/// can only use [`transact_state`], so a stale snapshot cannot accidentally be
/// reintroduced by a new caller.
#[cfg(test)]
pub fn save_state(cert_dir: &Path, state: &LocalIamState) -> AccessResult<()> {
    super::authority_store::with_lock(cert_dir, || save_state_locked(cert_dir, state))
}

/// Stat-level identity of `iam.json`. A `save_state` rename always mints a
/// new inode, and external writers (a second daemon on the same cert dir,
/// `intendant org …` CLI invocations, hand edits) move `len`/`mtime`, so a
/// matching fingerprint proves the cached parse is current.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IamStateFingerprint {
    len: u64,
    mtime_nanos: u128,
    dev: u64,
    ino: u64,
}

fn iam_state_fingerprint(path: &Path) -> Option<IamStateFingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let (dev, ino) = crate::platform::metadata_dev_ino(&metadata);
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(IamStateFingerprint {
        len: metadata.len(),
        mtime_nanos,
        dev,
        ino,
    })
}

struct IamStateCacheEntry {
    fingerprint: IamStateFingerprint,
    state: std::sync::Arc<LocalIamState>,
}

/// Cache is keyed by the state file path so tests (and multi-cert-dir
/// processes) with distinct cert dirs never cross-talk.
fn iam_state_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, IamStateCacheEntry>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, IamStateCacheEntry>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

const IAM_STATE_CACHE_MAX_DIRS: usize = 8;

fn store_cached_iam_state(
    path: &Path,
    fingerprint: IamStateFingerprint,
    state: LocalIamState,
) -> std::sync::Arc<LocalIamState> {
    let state = std::sync::Arc::new(state);
    let mut cache = iam_state_cache().lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= IAM_STATE_CACHE_MAX_DIRS && !cache.contains_key(path) {
        cache.clear();
    }
    cache.insert(
        path.to_path_buf(),
        IamStateCacheEntry {
            fingerprint,
            state: std::sync::Arc::clone(&state),
        },
    );
    state
}

/// [`load_state`] behind a stat-fingerprint cache: the per-request read
/// path (mTLS request authorization stats + parses `iam.json` on every
/// HTTP request, including statics) pays one `stat` instead of a full
/// read + parse + normalize when the file is unchanged. Never trusts
/// invalidation alone — every call re-checks the fingerprint, so writers
/// that bypass [`save_state`] (other processes, hand edits) are picked up
/// on the next request. Parse errors are never cached.
pub fn load_state_cached_arc(cert_dir: &Path) -> AccessResult<std::sync::Arc<LocalIamState>> {
    let path = iam_state_path(cert_dir);
    let Some(fingerprint) = iam_state_fingerprint(&path) else {
        // Missing file: same contract as load_state. Nothing to cache —
        // the default is cheap relative to a request.
        return Ok(std::sync::Arc::new(LocalIamState::default()));
    };
    {
        let cache = iam_state_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache
            .get(&path)
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            return Ok(std::sync::Arc::clone(&entry.state));
        }
    }
    let state = load_state(cert_dir)?;
    // Re-stat AFTER the read: if the file changed between the stat and the
    // read, caching the read under the pre-read fingerprint could pin a
    // torn view. Matching fingerprints prove read and stat saw one file.
    if matches!(iam_state_fingerprint(&path), Some(after) if after == fingerprint) {
        return Ok(store_cached_iam_state(&path, fingerprint, state));
    }
    Ok(std::sync::Arc::new(state))
}

struct UserClientBinding {
    principal_id: String,
    principal_kind: String,
    label: String,
    account: Option<Value>,
    organization: Option<Value>,
    authn: Vec<Value>,
}

pub fn upsert_user_client_grant(
    state: &mut LocalIamState,
    request: UserClientGrantUpsertRequest,
    actor: &AccessPrincipal,
) -> AccessResult<UserClientGrantUpsertResult> {
    let fleet_zone = crate::fleet_cert::fleet_zone();
    upsert_user_client_grant_with_fleet_zone(state, request, actor, fleet_zone.as_deref())
}

fn upsert_user_client_grant_with_fleet_zone(
    state: &mut LocalIamState,
    request: UserClientGrantUpsertRequest,
    actor: &AccessPrincipal,
    fleet_zone: Option<&str>,
) -> AccessResult<UserClientGrantUpsertResult> {
    // Normalize before refreshing built-ins so a rejected legacy-only kind
    // cannot mutate IAM state at all.
    let kind = normalize_user_client_kind(&request)?;
    let status = normalize_user_client_status(request.status.as_deref())?;
    validate_active_pure_client_key_origin(state, &kind, &status, &request, fleet_zone)?;
    for role in builtin_role_templates() {
        if !state.roles.iter().any(|existing| existing.id == role.id) {
            state.roles.push(role);
        }
    }

    let role_id = request
        .role_id
        .as_deref()
        .and_then(trimmed_nonempty)
        .unwrap_or("role:scoped-human")
        .to_string();
    validate_user_client_role(state, &role_id)?;
    let target_id = request
        .target_id
        .as_deref()
        .and_then(trimmed_nonempty)
        .unwrap_or("local")
        .to_string();
    let reason = request
        .reason
        .as_deref()
        .and_then(trimmed_nonempty)
        .unwrap_or("local IAM user/client grant")
        .to_string();
    let now = now_unix_ms();
    let expires_at_unix_ms = match request.expires_at_unix_ms {
        Some(expires) if expires <= now => {
            return Err(AccessError(
                "expires_at_unix_ms must be in the future".to_string(),
            ));
        }
        other => other,
    };
    let normalize_roots = |roots: &[String]| -> AccessResult<Vec<std::path::PathBuf>> {
        let mut out = Vec::new();
        for root in roots {
            let root = root.trim();
            if root.is_empty() {
                continue;
            }
            if !std::path::Path::new(root).is_absolute() {
                return Err(AccessError(format!(
                    "filesystem scope roots must be absolute paths (got {root})"
                )));
            }
            out.push(std::path::PathBuf::from(root));
        }
        out.sort();
        out.dedup();
        Ok(out)
    };
    let fs_scope = {
        let read_roots = normalize_roots(&request.fs_read_roots)?;
        let write_roots = normalize_roots(&request.fs_write_roots)?;
        if read_roots.is_empty() && write_roots.is_empty() {
            None
        } else {
            Some(crate::access::access_policy::FilesystemAccessPolicy {
                read_roots,
                write_roots,
            })
        }
    };

    let binding = build_user_client_binding(&kind, &request)?;
    let principal_id = binding.principal_id;
    let grant_id = format!(
        "grant:user-client:{}:{}:{}",
        slug_component(&principal_id),
        slug_component(&target_id),
        slug_component(&role_id)
    );

    let created_principal;
    let principal = if let Some(existing) = state
        .principals
        .iter_mut()
        .find(|principal| principal.id == principal_id)
    {
        created_principal = false;
        existing.kind = binding.principal_kind.clone();
        existing.label = binding.label.clone();
        existing.status = status.clone();
        existing.source = "local_iam_state".to_string();
        existing.account = binding.account.clone();
        existing.organization = binding.organization.clone();
        existing.authn = binding.authn.clone();
        existing.notes = Some(reason.clone());
        if existing.created_at_unix_ms.is_none() {
            existing.created_at_unix_ms = Some(now);
        }
        existing.clone()
    } else {
        created_principal = true;
        let principal = IamPrincipal {
            id: principal_id.clone(),
            kind: binding.principal_kind.clone(),
            label: binding.label.clone(),
            status: status.clone(),
            source: "local_iam_state".to_string(),
            account: binding.account.clone(),
            organization: binding.organization.clone(),
            authn: binding.authn.clone(),
            notes: Some(reason.clone()),
            created_at_unix_ms: Some(now),
        };
        state.principals.push(principal.clone());
        principal
    };

    let policy_id = policy_for_role(&role_id);
    let created_grant;
    let grant = if let Some(existing) = state.grants.iter_mut().find(|grant| {
        grant.id == grant_id || (grant.principal_id == principal_id && grant.target_id == target_id)
    }) {
        created_grant = false;
        existing.id = grant_id;
        existing.principal_id = principal_id.clone();
        existing.target_id = target_id.clone();
        existing.role_id = role_id.clone();
        existing.policy_id = policy_id.clone();
        existing.status = status.clone();
        existing.source = "local_iam_state".to_string();
        existing.reason = reason.clone();
        if existing.created_at_unix_ms.is_none() {
            existing.created_at_unix_ms = Some(now);
        }
        existing.revoked_at_unix_ms = if status == "revoked" { Some(now) } else { None };
        existing.expires_at_unix_ms = expires_at_unix_ms;
        existing.fs_scope = fs_scope.clone();
        existing.clone()
    } else {
        created_grant = true;
        let grant = IamGrant {
            id: grant_id,
            principal_id: principal_id.clone(),
            target_id: target_id.clone(),
            role_id: role_id.clone(),
            policy_id,
            status: status.clone(),
            source: "local_iam_state".to_string(),
            reason: reason.clone(),
            created_at_unix_ms: Some(now),
            revoked_at_unix_ms: if status == "revoked" { Some(now) } else { None },
            expires_at_unix_ms,
            issued_via: None,
            fs_scope: fs_scope.clone(),
        };
        state.grants.push(grant.clone());
        grant
    };

    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{}:{}", now, state.audit_events.len() + 1),
        at_unix_ms: Some(now),
        actor_principal_id: actor.id.clone(),
        action: "upsert_user_client_grant".to_string(),
        target_id: grant.id.clone(),
        summary: format!(
            "{} {} grant {} for {}",
            if created_grant { "Created" } else { "Updated" },
            status,
            role_id,
            principal.label
        ),
    });

    Ok(UserClientGrantUpsertResult {
        principal,
        grant,
        created_principal,
        created_grant,
    })
}

pub fn update_user_client_grant(
    state: &mut LocalIamState,
    request: IamGrantUpdateRequest,
    actor: &AccessPrincipal,
) -> AccessResult<IamGrantUpdateResult> {
    let fleet_zone = crate::fleet_cert::fleet_zone();
    update_user_client_grant_with_fleet_zone(state, request, actor, fleet_zone.as_deref())
}

fn update_user_client_grant_with_fleet_zone(
    state: &mut LocalIamState,
    request: IamGrantUpdateRequest,
    actor: &AccessPrincipal,
    fleet_zone: Option<&str>,
) -> AccessResult<IamGrantUpdateResult> {
    let grant_id = request.grant_id.as_str().trim().to_string();
    if grant_id.is_empty() {
        return Err(AccessError("grant_id is required".to_string()));
    }
    let grant_index = state
        .grants
        .iter()
        .position(|grant| grant.id == grant_id)
        .ok_or_else(|| AccessError(format!("IAM grant {grant_id} was not found")))?;
    if state.grants[grant_index].source != "local_iam_state" {
        return Err(AccessError(
            "only local IAM user/client grants can be updated".to_string(),
        ));
    }
    let principal_id = state.grants[grant_index].principal_id.clone();
    let principal_index = state
        .principals
        .iter()
        .position(|principal| principal.id == principal_id)
        .ok_or_else(|| AccessError(format!("IAM principal {principal_id} was not found")))?;
    if state.principals[principal_index].kind == "connect_account" {
        return Err(AccessError(
            "legacy Connect account records are metadata-only and read-only; they cannot be granted or updated"
                .to_string(),
        ));
    }
    if !matches!(
        state.principals[principal_index].kind.as_str(),
        "browser_certificate"
            | "human_user"
            | "client_key"
            | "agent_session"
            | "local_process"
            | ""
    ) {
        return Err(AccessError(
            "only user/client principals can be updated through this API".to_string(),
        ));
    }

    let now = now_unix_ms();
    let role_id = request
        .role_id
        .as_deref()
        .and_then(trimmed_nonempty)
        .map(ToOwned::to_owned);
    let status = match request.status.as_deref() {
        Some(_) => Some(normalize_user_client_status(request.status.as_deref())?),
        None => None,
    };
    let effective_status = status
        .as_deref()
        .unwrap_or(state.grants[grant_index].status.as_str());
    if is_enforced_status(effective_status) {
        if let Some(origin_class) = inactive_pure_client_key_origin_class(
            state,
            &state.principals[principal_index],
            fleet_zone,
        ) {
            return Err(inactive_client_key_grant_error(origin_class));
        }
    }
    for role in builtin_role_templates() {
        if !state.roles.iter().any(|existing| existing.id == role.id) {
            state.roles.push(role);
        }
    }
    if let Some(role_id) = role_id.as_deref() {
        validate_user_client_role(state, role_id)?;
    }
    let reason = request
        .reason
        .as_deref()
        .and_then(trimmed_nonempty)
        .map(ToOwned::to_owned);

    {
        let grant = &mut state.grants[grant_index];
        if let Some(role_id) = role_id {
            grant.role_id = role_id.clone();
            grant.policy_id = policy_for_role(&role_id);
        } else if grant.policy_id.trim().is_empty() {
            grant.policy_id = policy_for_role(if grant.role_id.trim().is_empty() {
                "role:scoped-human"
            } else {
                grant.role_id.as_str()
            });
        }
        if let Some(status) = status.as_ref() {
            grant.status = status.clone();
            grant.revoked_at_unix_ms = if status == "revoked" { Some(now) } else { None };
        }
        if let Some(reason) = reason.as_ref() {
            grant.reason = reason.clone();
        }
    }

    let principal_has_active_grant = state
        .grants
        .iter()
        .any(|grant| grant.principal_id == principal_id && is_enforced_status(&grant.status));
    {
        let principal = &mut state.principals[principal_index];
        principal.status = if principal_has_active_grant {
            "active".to_string()
        } else {
            status.clone().unwrap_or_else(|| "draft".to_string())
        };
        if let Some(reason) = reason.as_ref() {
            principal.notes = Some(reason.clone());
        }
    }

    let grant = state.grants[grant_index].clone();
    let principal = state.principals[principal_index].clone();
    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{}:{}", now, state.audit_events.len() + 1),
        at_unix_ms: Some(now),
        actor_principal_id: actor.id.clone(),
        action: "update_user_client_grant".to_string(),
        target_id: grant.id.clone(),
        summary: format!(
            "Updated {} grant {} for {}",
            if grant.status.is_empty() {
                "draft"
            } else {
                grant.status.as_str()
            },
            if grant.role_id.is_empty() {
                "role:scoped-human"
            } else {
                grant.role_id.as_str()
            },
            principal.label
        ),
    });

    Ok(IamGrantUpdateResult { principal, grant })
}

/// Set (or clear, with `None`) this daemon's trust tier
/// (docs/src/trust-tiers.md). Pure state mutation + audit event; the
/// caller persists. Returns the normalized stored value.
pub fn set_daemon_tier(
    state: &mut LocalIamState,
    tier: Option<&str>,
    actor: &AccessPrincipal,
) -> AccessResult<Option<String>> {
    let normalized = match tier.map(str::trim) {
        None | Some("") => None,
        Some(value) => {
            let value = value.to_ascii_lowercase();
            if !DAEMON_TIERS.contains(&value.as_str()) {
                return Err(AccessError(format!(
                    "unknown tier {value:?} (expected one of: {})",
                    DAEMON_TIERS.join(", ")
                )));
            }
            Some(value)
        }
    };
    if state.tier == normalized {
        return Ok(normalized);
    }
    let now = now_unix_ms();
    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{}:{}", now, state.audit_events.len() + 1),
        at_unix_ms: Some(now),
        actor_principal_id: actor.id.clone(),
        action: "set_daemon_tier".to_string(),
        target_id: "local".to_string(),
        summary: format!(
            "Trust tier {} -> {}",
            state.tier.as_deref().unwrap_or("unset"),
            normalized.as_deref().unwrap_or("unset")
        ),
    });
    state.tier = normalized.clone();
    Ok(normalized)
}

/// The legacy binding keys persisted for hosted-control policy. Both are
/// immutable `role:none` in the default build.
pub const HOSTED_CEILING_BINDINGS: [&str; 2] = ["connect_account", "client_key"];

fn validate_user_client_role(state: &LocalIamState, role_id: &str) -> AccessResult<()> {
    if super::hosted_control::HOSTED_ROLE_IDS.contains(&role_id) {
        return Err(AccessError(
            "hosted lease roles are reserved for the dedicated daemon-local lease writer"
                .to_string(),
        ));
    }
    let Some(role) = state.roles.iter().find(|role| role.id == role_id) else {
        return Err(AccessError(format!("unknown IAM role {role_id}")));
    };
    if role.id == "role:peer-profile" {
        return Err(AccessError(
            "peer-profile is a daemon-to-daemon role and cannot be assigned to a user/client"
                .to_string(),
        ));
    }
    if role.id == "role:none" {
        return Err(AccessError(
            "role:none is a ceiling-only sentinel and cannot be granted to a user/client"
                .to_string(),
        ));
    }
    if role.status == "planned" {
        return Err(AccessError(format!(
            "IAM role {role_id} is planned but not enforced"
        )));
    }
    Ok(())
}

fn normalize_user_client_kind(request: &UserClientGrantUpsertRequest) -> AccessResult<String> {
    let explicit = trimmed_nonempty(request.kind.as_str());
    let inferred = if request
        .session_id
        .as_deref()
        .and_then(trimmed_nonempty)
        .is_some()
    {
        Some("agent_session")
    } else if request
        .client_key_fingerprint
        .as_deref()
        .and_then(trimmed_nonempty)
        .is_some()
    {
        Some("client_key")
    } else if request
        .fingerprint
        .as_deref()
        .and_then(trimmed_nonempty)
        .is_some()
    {
        Some("browser_certificate")
    } else if request
        .user_id
        .as_deref()
        .and_then(trimmed_nonempty)
        .is_some()
        || request
            .account_name
            .as_deref()
            .and_then(trimmed_nonempty)
            .is_some()
    {
        Some("connect_account")
    } else {
        None
    };
    let kind = explicit.or(inferred).unwrap_or("").to_ascii_lowercase();
    match kind.as_str() {
        "browser_certificate" | "browser_mtls_cert" | "browser-mtls-cert" => {
            Ok("browser_certificate".to_string())
        }
        "client_key" | "client-key" | "browser_key" | "browser-key" => Ok("client_key".to_string()),
        "connect_account" | "connect-account" | "passkey_account" | "passkey-account" => Err(
            AccessError(
                "Connect account records are legacy discovery metadata and cannot receive IAM grants; grant a browser identity key, browser certificate, or keyed human user instead"
                    .to_string(),
            ),
        ),
        "human_user" | "human-user" | "human" | "human_mtls" | "human-mtls" => {
            Ok("human_user".to_string())
        }
        "agent_session" | "agent-session" => Ok("agent_session".to_string()),
        "local_process" | "local-process" | "loopback_mcp" | "loopback-mcp" => {
            Ok("local_process".to_string())
        }
        _ => Err(AccessError(
            "kind must be client_key, browser_certificate, human_user, agent_session, or local_process"
                .to_string(),
        )),
    }
}

fn normalize_user_client_status(status: Option<&str>) -> AccessResult<String> {
    let status = status
        .and_then(trimmed_nonempty)
        .unwrap_or("active")
        .to_ascii_lowercase();
    match status.as_str() {
        "active" | "draft" | "revoked" => Ok(status),
        _ => Err(AccessError(
            "status must be active, draft, or revoked".to_string(),
        )),
    }
}

fn build_user_client_binding(
    kind: &str,
    request: &UserClientGrantUpsertRequest,
) -> AccessResult<UserClientBinding> {
    match kind {
        "browser_certificate" => {
            let fingerprint = request
                .fingerprint
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(normalize_fingerprint)
                .filter(|fingerprint| !fingerprint.is_empty())
                .ok_or_else(|| AccessError("fingerprint is required".to_string()))?;
            let label = request
                .label
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("Browser certificate {}", short_id(&fingerprint)));
            Ok(UserClientBinding {
                principal_id: format!("principal:browser-cert:{fingerprint}"),
                principal_kind: "browser_certificate".to_string(),
                label,
                account: None,
                organization: organization_metadata(request),
                authn: vec![json!({
                    "kind": "browser_mtls_cert",
                    "label": "Browser mTLS certificate",
                    "fingerprint": fingerprint,
                })],
            })
        }
        "connect_account" => {
            let user_id = request
                .user_id
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned);
            let account_name = request
                .account_name
                .as_deref()
                .or(request.handle.as_deref())
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned);
            if user_id.is_none() && account_name.is_none() {
                return Err(AccessError(
                    "user_id or account_name is required for connect_account".to_string(),
                ));
            }
            let id_source = user_id.as_deref().or(account_name.as_deref()).unwrap_or("");
            let label = request
                .label
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned)
                .or_else(|| account_name.as_ref().map(|name| format!("@{name}")))
                .unwrap_or_else(|| format!("Connect account {}", short_id(id_source)));
            let mut account = serde_json::Map::new();
            account.insert(
                "provider".to_string(),
                Value::String(account_provider(request)),
            );
            if let Some(user_id) = user_id.as_ref() {
                account.insert("user_id".to_string(), Value::String(user_id.clone()));
            }
            if let Some(account_name) = account_name.as_ref() {
                account.insert(
                    "account_name".to_string(),
                    Value::String(account_name.clone()),
                );
                account.insert("handle".to_string(), Value::String(account_name.clone()));
            }
            if let Some(provider) = request
                .verified_provider
                .as_deref()
                .and_then(trimmed_nonempty)
            {
                account.insert(
                    "verified_provider".to_string(),
                    Value::String(provider.to_string()),
                );
            }
            let mut authn = serde_json::Map::new();
            authn.insert(
                "kind".to_string(),
                Value::String("connect_account".to_string()),
            );
            authn.insert(
                "label".to_string(),
                Value::String("Intendant Connect account".to_string()),
            );
            if let Some(user_id) = user_id.as_ref() {
                authn.insert("user_id".to_string(), Value::String(user_id.clone()));
            }
            if let Some(account_name) = account_name.as_ref() {
                authn.insert(
                    "account_name".to_string(),
                    Value::String(account_name.clone()),
                );
            }
            Ok(UserClientBinding {
                principal_id: format!("principal:connect-account:{}", slug_component(id_source)),
                principal_kind: "connect_account".to_string(),
                label,
                account: Some(Value::Object(account)),
                organization: organization_metadata(request),
                authn: vec![Value::Object(authn)],
            })
        }
        "client_key" => {
            let fingerprint = request
                .client_key_fingerprint
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(normalize_client_key_fingerprint)
                .filter(|fingerprint| !fingerprint.is_empty())
                .ok_or_else(|| AccessError("client_key_fingerprint is required".to_string()))?;
            let label = request
                .label
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("Browser key {}", short_id(&fingerprint)));
            Ok(UserClientBinding {
                principal_id: format!("principal:client-key:{fingerprint}"),
                principal_kind: "client_key".to_string(),
                label,
                account: None,
                organization: organization_metadata(request),
                authn: vec![client_key_authn_entry(
                    &fingerprint,
                    request.client_key.as_deref(),
                    request.client_key_origin.as_deref(),
                )],
            })
        }
        "human_user" => build_human_user_binding(request),
        "agent_session" => {
            let session_id = request
                .session_id
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    AccessError(
                        "session_id is required for agent_session (use \"*\" to scope every supervised agent session)"
                            .to_string(),
                    )
                })?;
            let id_component = if session_id == "*" {
                "any".to_string()
            } else {
                slug_component(&session_id)
            };
            let label = request
                .label
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    if session_id == "*" {
                        "All supervised agent sessions".to_string()
                    } else {
                        format!("Agent session {}", short_id(&session_id))
                    }
                });
            Ok(UserClientBinding {
                principal_id: format!("principal:agent-session:{id_component}"),
                principal_kind: "agent_session".to_string(),
                label,
                account: None,
                organization: organization_metadata(request),
                authn: vec![json!({
                    "kind": "agent_session",
                    "label": "Supervised agent session",
                    "session_id": session_id,
                })],
            })
        }
        "local_process" => {
            let label = request
                .label
                .as_deref()
                .and_then(trimmed_nonempty)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "Local loopback processes".to_string());
            Ok(UserClientBinding {
                principal_id: "principal:local-process:loopback".to_string(),
                principal_kind: "local_process".to_string(),
                label,
                account: None,
                organization: organization_metadata(request),
                authn: vec![json!({
                    "kind": "loopback_mcp",
                    "label": "Loopback MCP client",
                    "scope": "loopback",
                })],
            })
        }
        _ => Err(AccessError(format!("unsupported user/client kind {kind}"))),
    }
}

/// Client-key fingerprints are base64url (case-sensitive); unlike hex mTLS
/// fingerprints they must not be case-folded or stripped.
pub fn normalize_client_key_fingerprint(value: &str) -> String {
    value.trim().to_string()
}

fn client_key_authn_entry(
    fingerprint: &str,
    public_key: Option<&str>,
    origin: Option<&str>,
) -> Value {
    let mut authn = serde_json::Map::new();
    authn.insert("kind".to_string(), Value::String("client_key".to_string()));
    authn.insert(
        "label".to_string(),
        Value::String("Browser identity key".to_string()),
    );
    authn.insert(
        "fingerprint".to_string(),
        Value::String(fingerprint.to_string()),
    );
    if let Some(public_key) = public_key.and_then(trimmed_nonempty) {
        authn.insert(
            "public_key".to_string(),
            Value::String(public_key.to_string()),
        );
    }
    if let Some(origin) = origin.and_then(trimmed_nonempty) {
        authn.insert("origin".to_string(), Value::String(origin.to_string()));
    }
    Value::Object(authn)
}

fn build_human_user_binding(
    request: &UserClientGrantUpsertRequest,
) -> AccessResult<UserClientBinding> {
    let fingerprint = request
        .fingerprint
        .as_deref()
        .and_then(trimmed_nonempty)
        .map(normalize_fingerprint)
        .filter(|fingerprint| !fingerprint.is_empty());
    let client_key_fingerprint = request
        .client_key_fingerprint
        .as_deref()
        .and_then(trimmed_nonempty)
        .map(normalize_client_key_fingerprint)
        .filter(|fingerprint| !fingerprint.is_empty());
    let user_id = request
        .user_id
        .as_deref()
        .and_then(trimmed_nonempty)
        .map(ToOwned::to_owned);
    let handle = request
        .handle
        .as_deref()
        .or(request.account_name.as_deref())
        .and_then(trimmed_nonempty)
        .map(ToOwned::to_owned);
    if fingerprint.is_none() && client_key_fingerprint.is_none() {
        return Err(AccessError(
            "human_user requires a browser certificate fingerprint or browser identity key; Connect account fields are metadata only"
                .to_string(),
        ));
    }
    let id_source = user_id
        .as_deref()
        .or(handle.as_deref())
        .or(fingerprint.as_deref())
        .or(client_key_fingerprint.as_deref())
        .unwrap_or("human");
    let label = request
        .label
        .as_deref()
        .and_then(trimmed_nonempty)
        .map(ToOwned::to_owned)
        .or_else(|| handle.as_ref().map(|name| format!("@{name}")))
        .unwrap_or_else(|| format!("Human user {}", short_id(id_source)));

    let mut authn = Vec::new();
    if let Some(fingerprint) = fingerprint.as_ref() {
        authn.push(json!({
            "kind": "browser_mtls_cert",
            "label": "Browser mTLS certificate",
            "fingerprint": fingerprint,
        }));
    }
    if let Some(client_key_fingerprint) = client_key_fingerprint.as_ref() {
        authn.push(client_key_authn_entry(
            client_key_fingerprint,
            request.client_key.as_deref(),
            request.client_key_origin.as_deref(),
        ));
    }
    if user_id.is_some() || handle.is_some() {
        let mut connect = serde_json::Map::new();
        connect.insert(
            "kind".to_string(),
            Value::String("connect_account".to_string()),
        );
        connect.insert(
            "label".to_string(),
            Value::String("Intendant Connect account".to_string()),
        );
        if let Some(user_id) = user_id.as_ref() {
            connect.insert("user_id".to_string(), Value::String(user_id.clone()));
        }
        if let Some(handle) = handle.as_ref() {
            connect.insert("account_name".to_string(), Value::String(handle.clone()));
            connect.insert("handle".to_string(), Value::String(handle.clone()));
        }
        authn.push(Value::Object(connect));
    }

    Ok(UserClientBinding {
        principal_id: format!("principal:human-user:{}", slug_component(id_source)),
        principal_kind: "human_user".to_string(),
        label,
        account: account_metadata(request, user_id.as_deref(), handle.as_deref()),
        organization: organization_metadata(request),
        authn,
    })
}

fn account_provider(request: &UserClientGrantUpsertRequest) -> String {
    request
        .account_provider
        .as_deref()
        .and_then(trimmed_nonempty)
        .unwrap_or("intendant.dev")
        .to_string()
}

fn account_metadata(
    request: &UserClientGrantUpsertRequest,
    user_id: Option<&str>,
    handle: Option<&str>,
) -> Option<Value> {
    if user_id.is_none()
        && handle.is_none()
        && request
            .verified_provider
            .as_deref()
            .and_then(trimmed_nonempty)
            .is_none()
    {
        return None;
    }
    let mut account = serde_json::Map::new();
    account.insert(
        "provider".to_string(),
        Value::String(account_provider(request)),
    );
    if let Some(user_id) = user_id {
        account.insert("user_id".to_string(), Value::String(user_id.to_string()));
    }
    if let Some(handle) = handle {
        account.insert(
            "account_name".to_string(),
            Value::String(handle.to_string()),
        );
        account.insert("handle".to_string(), Value::String(handle.to_string()));
    }
    if let Some(provider) = request
        .verified_provider
        .as_deref()
        .and_then(trimmed_nonempty)
    {
        account.insert(
            "verified_provider".to_string(),
            Value::String(provider.to_string()),
        );
    }
    Some(Value::Object(account))
}

fn organization_metadata(request: &UserClientGrantUpsertRequest) -> Option<Value> {
    let org_id = request
        .organization_id
        .as_deref()
        .and_then(trimmed_nonempty);
    let org_name = request
        .organization_name
        .as_deref()
        .and_then(trimmed_nonempty);
    if org_id.is_none() && org_name.is_none() {
        return None;
    }
    let mut org = serde_json::Map::new();
    if let Some(org_id) = org_id {
        org.insert("id".to_string(), Value::String(org_id.to_string()));
    }
    if let Some(org_name) = org_name {
        org.insert("name".to_string(), Value::String(org_name.to_string()));
    }
    Some(Value::Object(org))
}

pub fn policy_for_role(role_id: &str) -> String {
    match role_id {
        "role:root" => "policy:root".to_string(),
        "role:peer-profile" => "policy:peer-profile".to_string(),
        "role:scoped-human" => "policy:scoped-human".to_string(),
        "role:observer" => "policy:observer".to_string(),
        "role:session-reader" => "policy:session-reader".to_string(),
        "role:terminal" => "policy:terminal".to_string(),
        "role:files-read" => "policy:files-read".to_string(),
        "role:files-write" => "policy:files-write".to_string(),
        "role:peer-user" => "policy:peer-user".to_string(),
        "role:operator" => "policy:operator".to_string(),
        other => format!("policy:{}", slug_component(other)),
    }
}

fn trimmed_nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

pub fn normalize_browser_mtls_fingerprint(value: &str) -> String {
    normalize_fingerprint(value)
}

fn slug_component(value: &str) -> String {
    let slug: String = value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "unknown".to_string()
    } else {
        slug.to_string()
    }
}

fn short_id(value: &str) -> String {
    value.chars().take(12).collect()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub fn overview_metadata(load: &LoadedIamState) -> Value {
    const ENFORCED_PRINCIPAL_KINDS: &[&str] = &[
        "root_session",
        "peer_daemon",
        "human_user",
        "browser_certificate",
        "agent_session",
        "local_process",
    ];
    json!({
        "schema_version": load.state.schema_version,
        "state_path": load.path.display().to_string(),
        "load_status": load.status.as_str(),
        "load_error": load.status.error(),
        "managed_principals": load.state.managed_principal_count(),
        "managed_grants": load.state.managed_grant_count(),
        "roles": load.state.roles.clone(),
        "audit_events": load.state.audit_events.clone(),
        "capabilities": {
            "state_file_supported": true,
            "read_local_state": true,
            "write_api_available": true,
            "operation_evaluator": true,
            "enforce_root_and_peer_grants": true,
            "enforce_user_client_grants": true
        },
        "enforcement": {
            "root_session_grants": true,
            "peer_profile_grants": true,
            "user_client_grants": true,
            "principal_binding": "root_peer_and_local_user_client",
            "enforced_principal_kinds": ENFORCED_PRINCIPAL_KINDS,
            "reason": "The daemon enforces trusted owner/root dashboard sessions, approved daemon peer profiles, browser/native mTLS identities (including mTLS-bound human-user grants), supervised agent sessions, MCP token holders, and trusted local-process grants. Browser client-key and Connect-account records remain available for enrollment, fleet signatures, attribution, migration, and audit. Peer offers can verify a browser key for attribution, but no alpha request ingress admits either record kind as its controlling IAM principal. Every admitted request is still evaluated per operation."
        },
        "role_ceilings": load.state.role_ceilings.clone(),
        "hosted_origins": load.state.hosted_origins.clone(),
        "tier": load.state.tier.clone(),
        "trusted_orgs": load.state.trusted_orgs.clone(),
        "org_issuers": load
            .path
            .parent()
            .map(crate::access::org::local_org_handles)
            .unwrap_or_default()
    })
}

pub fn policy_overview_values(state: &LocalIamState) -> Vec<Value> {
    let mut values: Vec<Value> = state
        .roles
        .iter()
        .map(|role| {
            json!({
                "id": policy_for_role(&role.id),
                "label": role.label.clone(),
                "status": role.status.clone(),
                "summary": role.summary.clone(),
                "role_id": role.id.clone(),
                "permissions": role.permissions.clone(),
                "source": role.source.clone(),
                "assignment": if role.id == "role:peer-profile" {
                    "daemon_peer_only"
                } else if role.status == "planned" {
                    "planned"
                } else {
                    "user_client"
                }
            })
        })
        .collect();
    values.push(json!({
        "id": "policy:public-share",
        "label": "Public share",
        "status": "planned",
        "summary": "Future explicit grants for publishing selected stats or artifacts.",
        "permissions": [],
        "source": "builtin",
        "assignment": "planned"
    }));
    values
}

pub fn permission_catalog_values() -> Vec<Value> {
    root_permission_ids()
        .into_iter()
        .map(|id| {
            let label = permission_label(&id);
            let domain = id.split('.').next().unwrap_or("access").to_string();
            let summary = permission_summary(&id);
            json!({
                "id": id,
                "label": label,
                "domain": domain,
                "status": "enforced",
                "summary": summary,
            })
        })
        .collect()
}

fn permission_label(id: &str) -> &'static str {
    match id {
        "presence.read" => "Presence read",
        "stats.read" => "Stats read",
        "display.view" => "Display view",
        "display.input" => "Display input",
        "message.send" => "Message send",
        "task.run" => "Task run",
        "approval.resolve" => "Approval resolve",
        "access.inspect" => "Access inspect",
        "access.manage" => "Access manage",
        "peer.inspect" => "Peer inspect",
        "peer.manage" => "Peer manage",
        "peer.use" => "Peer use",
        "session.inspect" => "Session inspect",
        "session.manage" => "Session manage",
        "terminal.use" => "Terminal (legacy)",
        "terminal.view" => "Terminal view",
        "terminal.write" => "Terminal write",
        "shell.spawn" => "Shell spawn",
        "settings.manage" => "Settings manage",
        "credentials.manage" => "Credentials manage",
        "runtime.control" => "Runtime control",
        "filesystem.read" => "Filesystem read",
        "filesystem.write" => "Filesystem write",
        "agenda.read" => "Agenda read",
        "agenda.write" => "Agenda write",
        "memory.read" => "Memory read",
        "memory.write" => "Memory propose",
        _ => "Permission",
    }
}

fn permission_summary(id: &str) -> &'static str {
    match id {
        "presence.read" => "Read live presence and basic daemon availability.",
        "stats.read" => "Read daemon health, usage, and status summaries.",
        "display.view" => "View display streams without injecting input.",
        "display.input" => "Inject keyboard, pointer, or display-control input.",
        "message.send" => "Send user messages or dashboard actions into a session.",
        "task.run" => "Start or delegate agent tasks.",
        "approval.resolve" => "Approve or deny pending supervised actions.",
        "access.inspect" => {
            "Read targets, principals, grants, policies, transports, and access architecture notes."
        }
        "access.manage" => {
            "Approve, revoke, or change access grants. Reserved for root sessions unless explicitly delegated later."
        }
        "peer.inspect" => "Read configured peer routes and peer eligibility.",
        "peer.manage" => {
            "Create, remove, and pair daemon peer routes (implies peer.use)."
        }
        "peer.use" => {
            "Act through connected peers with this daemon's peer credentials — open tunnels, send messages, delegate tasks, resolve approvals; what the peer allows is decided by the peer's grants for this daemon, not by this grant."
        }
        "session.inspect" => "Read session lists, logs, reports, recordings, and replay metadata.",
        "session.manage" => "Delete, rewind, prune, upload to, or otherwise mutate sessions.",
        "terminal.use" => {
            "Legacy aggregate: implies terminal.view, terminal.write, and shell.spawn."
        }
        "terminal.view" => "Attach to shared shell sessions read-only (scrollback and live output).",
        "terminal.write" => "Type into, resize, and close shell sessions you can see.",
        "shell.spawn" => "Create new shell sessions on this daemon.",
        "settings.manage" => "Read or write daemon settings and API keys.",
        "credentials.manage" => {
            "Grant, renew, revoke, and inspect borrowed provider-credential leases (vault fueling)."
        }
        "runtime.control" => "Use runtime-control surfaces such as TUI, media, and recording controls.",
        "filesystem.read" => "Stat, list, and read files through dashboard APIs.",
        "filesystem.write" => "Create directories or write uploaded file content.",
        "agenda.read" => "Read the daemon's agenda ledger (parked items and counts).",
        "agenda.write" => "Park, edit, complete, reopen, and retire agenda items.",
        "memory.read" => "Search and read Memory claims (bounded, provenance-labeled).",
        "memory.write" => "Propose Memory claims (the candidate lane; ephemeral in P1.1).",
        _ => "Operation permission.",
    }
}

/// The filesystem scope attached to a principal's active grant, if any.
pub fn fs_scope_for_principal<'a>(
    state: &'a LocalIamState,
    principal: &AccessPrincipal,
) -> Option<&'a crate::access::access_policy::FilesystemAccessPolicy> {
    let grant_id = principal.grant_id.as_deref()?;
    state
        .grants
        .iter()
        .find(|grant| grant.id == grant_id)
        .and_then(|grant| grant.fs_scope.as_ref())
}

pub fn evaluate_principal_operation(
    principal: &AccessPrincipal,
    op: crate::access::access_policy::PeerOperation,
) -> AccessDecision {
    match principal.kind.as_str() {
        "root_session" => AccessDecision::allowed(
            principal,
            op,
            "root dashboard session grants all operations",
        ),
        "peer_daemon" => {
            let Some(profile) = principal.peer_profile.as_deref() else {
                return AccessDecision::denied(
                    principal,
                    op,
                    "peer daemon principal has no profile",
                );
            };
            if crate::access::access_policy::profile_allows_operation(profile, op) {
                AccessDecision::allowed(
                    principal,
                    op,
                    format!(
                        "peer profile {profile} allows {}",
                        operation_permission_id(op)
                    ),
                )
            } else {
                AccessDecision::denied(
                    principal,
                    op,
                    format!(
                        "peer profile {profile} does not allow {}",
                        operation_permission_id(op)
                    ),
                )
            }
        }
        _ => AccessDecision::denied(
            principal,
            op,
            "scoped user/client principal requires local IAM state evaluation",
        ),
    }
}

pub fn evaluate_principal_operation_with_state(
    state: &LocalIamState,
    principal: &AccessPrincipal,
    op: crate::access::access_policy::PeerOperation,
) -> AccessDecision {
    // The zone feeds only the client_key origin classification below —
    // fetch it lazily so every other principal kind (mTLS certificates,
    // root sessions, peers, agent sessions) skips the fleet-status mutex
    // on this per-request/per-frame path.
    let fleet_zone = if principal.authn_kind.as_deref() == Some("client_key") {
        crate::fleet_cert::fleet_zone()
    } else {
        None
    };
    evaluate_principal_operation_with_state_and_fleet_zone(
        state,
        principal,
        op,
        fleet_zone.as_deref(),
    )
}

fn evaluate_principal_operation_with_state_and_fleet_zone(
    state: &LocalIamState,
    principal: &AccessPrincipal,
    op: crate::access::access_policy::PeerOperation,
    fleet_zone: Option<&str>,
) -> AccessDecision {
    // Hosted leases are the one narrow fleet-origin authority class. Their
    // exact IAM/lease records and compiled preset are evaluated before the
    // general hosted-provenance refusal below; no other hosted principal can
    // reach this carve-out.
    if super::hosted_control::is_hosted_lease_principal(principal) {
        return super::hosted_control::evaluate_hosted_operation(state, principal, op);
    }
    // Connect account assertions are service-owned route/display metadata,
    // never a daemon authentication binding. Keep this invariant in the
    // central evaluator as well as the Connect offer resolver so a future
    // or alternate transport cannot accidentally revive account-as-auth.
    if principal.authn_kind.as_deref() == Some("connect_account") {
        return AccessDecision::denied(
            principal,
            op,
            "Connect account records are discovery metadata and never authenticate to the daemon",
        );
    }
    if principal.hosted_connect && matches!(principal.kind.as_str(), "root_session" | "peer_daemon")
    {
        return AccessDecision::denied(
            principal,
            op,
            "hosted Connect transport can never exercise a trusted-root or peer principal",
        );
    }
    // A composite human record may retain a browser key as audit/attribution
    // metadata beside an independently verified mTLS certificate. The
    // credential used for THIS session is what matters: authenticating with
    // that browser key must not inherit the certificate's authority merely
    // because both bindings name the same principal.
    if principal.authn_kind.as_deref() == Some("client_key") {
        let origin_class = client_key_origin_route_class(
            principal.authn_origin.as_deref().unwrap_or_default(),
            &state.hosted_origins,
            fleet_zone,
        );
        if matches!(origin_class, "hosted" | "fleet") {
            return AccessDecision::denied(
                principal,
                op,
                format!(
                    "the default build treats {origin_class}-origin browser keys as discovery-only; current client-key authentication cannot exercise daemon authority"
                ),
            );
        }
    }
    if is_hosted_session(state, principal) {
        return AccessDecision::denied(
            principal,
            op,
            "the default build treats hosted Connect as discovery-only; hosted control is immutably disabled",
        );
    }
    if matches!(principal.kind.as_str(), "root_session" | "peer_daemon") {
        return evaluate_principal_operation(principal, op);
    }

    let Some(grant_id) = principal.grant_id.as_deref() else {
        return AccessDecision::denied(principal, op, "principal has no local IAM grant");
    };
    let Some(grant) = state.grants.iter().find(|grant| grant.id == grant_id) else {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM grant {grant_id} was not found"),
        );
    };
    if grant.principal_id != principal.id {
        return AccessDecision::denied(
            principal,
            op,
            format!(
                "local IAM grant {} belongs to {}",
                grant.id, grant.principal_id
            ),
        );
    }
    if !grant.is_active_at(crate::access::client_key::now_unix_ms()) {
        let expired = is_enforced_status(&grant.status);
        return AccessDecision::denied(
            principal,
            op,
            if expired {
                format!("local IAM grant {} has expired", grant.id)
            } else {
                format!("local IAM grant {} is not active", grant.id)
            },
        );
    }

    let Some(principal_record) = state
        .principals
        .iter()
        .find(|record| record.id == principal.id)
    else {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM principal {} was not found", principal.id),
        );
    };
    if !is_enforced_status(&principal_record.status) {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM principal {} is not active", principal.id),
        );
    }

    let role_id = if grant.role_id.trim().is_empty() {
        "role:scoped-human"
    } else {
        grant.role_id.as_str()
    };
    let Some(role) = state.roles.iter().find(|role| role.id == role_id) else {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM role {role_id} was not found"),
        );
    };
    let permission = operation_permission_id(op);
    if !permissions_allow(&role.permissions, permission) {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM role {role_id} does not allow {permission}"),
        );
    }

    AccessDecision::allowed(
        principal,
        op,
        format!("local IAM role {role_id} allows {permission}"),
    )
}

/// True when this session's daemon-stamped route or authenticated binding
/// has hosted provenance. `hosted_connect` is authoritative: a direct-born
/// key relayed through Connect stays hosted. Stored origins are a secondary
/// defense for direct use of keys enrolled by hosted code, including the
/// retired `connect-bootstrap` sentinel.
pub fn is_hosted_session(state: &LocalIamState, principal: &AccessPrincipal) -> bool {
    if principal.hosted_connect || principal.authn_kind.as_deref() == Some("connect_account") {
        return true;
    }
    if principal.authn_kind.as_deref() != Some("client_key") {
        return false;
    }
    let origin = principal.authn_origin.as_deref().unwrap_or("");
    client_key_origin_route_class(origin, &state.hosted_origins, None) == "hosted"
}

/// Origin class of a session for the custody trail
/// (docs/src/trust-tiers.md): `hosted` (Connect account or a browser key
/// enrolled from one of `hosted_origins`), `direct` (a key or mTLS
/// certificate admitted on an independently reached daemon origin), `local` (the owner's
/// own dashboard / loopback), or `peer` (a federated daemon).
/// Classification mirrors [`is_hosted_session`]'s provenance rules. A public
/// fleet-name connection never reaches this custody classifier: the gateway
/// rejects protected traffic on fleet SNI before constructing an authority.
pub fn session_origin_class(
    hosted_origins: &[String],
    principal: &AccessPrincipal,
) -> &'static str {
    if principal.hosted_connect {
        return "hosted";
    }
    if principal.kind == "peer_daemon" {
        return "peer";
    }
    match principal.authn_kind.as_deref() {
        Some("connect_account") => "hosted",
        Some("client_key") => {
            let origin = principal.authn_origin.as_deref().unwrap_or("");
            let hosted = client_key_origin_route_class(origin, hosted_origins, None) == "hosted";
            if hosted {
                "hosted"
            } else {
                "direct"
            }
        }
        Some(_) => "direct",
        None => {
            if principal.kind == "root_session" {
                "local"
            } else {
                "direct"
            }
        }
    }
}

/// Routing provenance of an enrollment origin — the first-contact rung it
/// arrived on (docs/src/trust-tiers.md, "First contact"):
///
/// - `hosted`: the compiled Connect origin or one of `hosted_origins` — the
///   rendezvous serves the code.
/// - `fleet`: a name under the rendezvous's delegated fleet zone — the
///   daemon may serve public discovery code, but the rendezvous names the
///   route and can redirect it or mint another certificate. The gateway
///   therefore admits no protected request on this route; CT is diagnostic.
/// - `direct`: any other explicit origin (typed IP, mDNS, own domain).
/// - `unknown`: no origin recorded (pre-origin enrollments).
///
/// Distinct from [`session_origin_class`]: that classifies admitted sessions
/// for custody, while this classifies historical/staged enrollment records by
/// route provenance. Fleet traffic is discovery-only and creates no admitted
/// session to classify.
pub fn origin_route_class(
    origin: &str,
    hosted_origins: &[String],
    fleet_zone: Option<&str>,
) -> &'static str {
    origin_route_class_with_provenance_state(
        origin,
        hosted_origins,
        fleet_zone,
        crate::fleet_cert::fleet_origin_provenance_is_incomplete(),
    )
}

fn origin_route_class_with_provenance_state(
    origin: &str,
    hosted_origins: &[String],
    fleet_zone: Option<&str>,
    fleet_provenance_incomplete: bool,
) -> &'static str {
    let origin = origin.trim();
    if origin.is_empty() {
        return "unknown";
    }
    if hosted_origins
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(DEFAULT_HOSTED_ORIGIN))
        .any(|candidate| network_origins_match(candidate, origin))
    {
        return "hosted";
    }
    let host = url::Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    if let Some(zone) = fleet_zone.map(str::trim).filter(|zone| !zone.is_empty()) {
        let zone = zone.trim_end_matches('.').to_ascii_lowercase();
        if let Some(host) = host.as_deref() {
            let host = host.trim_end_matches('.');
            if host == zone || host.ends_with(&format!(".{zone}")) {
                return "fleet";
            }
        }
    }
    // The current zone can be absent (offline startup, Connect disabled) or
    // replaced. The TLS resolver retains every rendezvous-assigned exact name
    // from durable fleet-origin provenance; consult that source too so a
    // formerly service-controlled origin can never decay into `direct`.
    if crate::web_tls::is_fleet_server_name(host.as_deref()) {
        return "fleet";
    }
    // A malformed provenance file or an installed pre-migration certificate
    // whose exact DNS SAN could not be recovered means the daemon cannot
    // prove that an otherwise unknown DNS origin was independently chosen.
    // Fail closed for browser-key bindings until the local authority store is
    // repaired. IP literals cannot have been rendezvous-assigned fleet names
    // and retain their direct classification.
    if fleet_provenance_incomplete
        && host
            .as_deref()
            .is_some_and(|host| host.parse::<std::net::IpAddr>().is_err())
    {
        return "fleet";
    }
    "direct"
}

fn network_origins_match(left: &str, right: &str) -> bool {
    fn normalized(value: &str) -> Option<(String, String, Option<u16>)> {
        let parsed = url::Url::parse(value.trim()).ok()?;
        let host = parsed
            .host_str()?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        Some((
            parsed.scheme().to_ascii_lowercase(),
            host,
            parsed.port_or_known_default(),
        ))
    }

    normalized(left).is_some_and(|left| normalized(right) == Some(left))
}

/// Classify the provenance stored on a browser-key binding. The retired
/// `connect-bootstrap` sentinel predates URL origins but is still hosted
/// provenance and must never escape the same fail-closed rule.
fn client_key_origin_route_class(
    origin: &str,
    hosted_origins: &[String],
    fleet_zone: Option<&str>,
) -> &'static str {
    if origin.trim() == "connect-bootstrap" {
        "hosted"
    } else {
        origin_route_class(origin, hosted_origins, fleet_zone)
    }
}

fn inactive_client_key_grant_error(origin_class: &str) -> AccessError {
    AccessError(format!(
        "active pure client_key grants from {origin_class} origins are refused; bind the person to an independently verified browser mTLS certificate instead"
    ))
}

fn validate_active_pure_client_key_origin(
    state: &LocalIamState,
    kind: &str,
    status: &str,
    request: &UserClientGrantUpsertRequest,
    fleet_zone: Option<&str>,
) -> AccessResult<()> {
    let has_client_key = request
        .client_key_fingerprint
        .as_deref()
        .and_then(trimmed_nonempty)
        .is_some();
    let has_browser_mtls = request
        .fingerprint
        .as_deref()
        .and_then(trimmed_nonempty)
        .map(normalize_fingerprint)
        .is_some_and(|fingerprint| !fingerprint.is_empty());
    let pure_client_key =
        kind == "client_key" || (kind == "human_user" && has_client_key && !has_browser_mtls);
    if !pure_client_key || !is_enforced_status(status) {
        return Ok(());
    }
    let origin_class = client_key_origin_route_class(
        request.client_key_origin.as_deref().unwrap_or_default(),
        &state.hosted_origins,
        fleet_zone,
    );
    if matches!(origin_class, "hosted" | "fleet") {
        return Err(inactive_client_key_grant_error(origin_class));
    }
    Ok(())
}

fn has_valid_browser_mtls_authn(principal: &IamPrincipal) -> bool {
    principal.authn.iter().any(|authn| {
        authn.get("kind").and_then(Value::as_str) == Some("browser_mtls_cert")
            && authn
                .get("fingerprint")
                .and_then(Value::as_str)
                .map(normalize_fingerprint)
                .is_some_and(|fingerprint| !fingerprint.is_empty())
    })
}

/// A pure browser-key principal recorded from a service-controlled origin is
/// historical/staged data, not an authority. A composite `human_user` may
/// carry the same key as metadata while independently authenticating with a
/// valid mTLS certificate; only that composite binding is excluded.
fn inactive_pure_client_key_origin_class(
    state: &LocalIamState,
    principal: &IamPrincipal,
    fleet_zone: Option<&str>,
) -> Option<&'static str> {
    if has_valid_browser_mtls_authn(principal) {
        return None;
    }
    principal.authn.iter().find_map(|authn| {
        if authn.get("kind").and_then(Value::as_str) != Some("client_key") {
            return None;
        }
        let origin = authn.get("origin").and_then(Value::as_str)?;
        let origin_class = client_key_origin_route_class(origin, &state.hosted_origins, fleet_zone);
        matches!(origin_class, "hosted" | "fleet").then_some(origin_class)
    })
}

/// True when a permission list grants `permission`. The legacy aggregate id
/// `terminal.use` implies the three split terminal permissions, so custom
/// roles and org grant caps written before the split keep their meaning
/// (full terminal capability) without an iam.json rewrite. Builtin roles
/// are refreshed from templates on load and never carry the legacy id.
pub fn permissions_allow(permissions: &[String], permission: &str) -> bool {
    permissions.iter().any(|candidate| {
        candidate == permission
            || (candidate == "terminal.use"
                && matches!(
                    permission,
                    "terminal.view" | "terminal.write" | "shell.spawn"
                ))
            // peer.manage predates the manage/use split and covered the
            // signaling relays; grants that carry it keep tunnel access.
            || (candidate == "peer.manage" && permission == "peer.use")
    })
}

/// First permission in `granted` that `cap` does not cover, if any — the
/// set-containment twin of [`permissions_allow`], expanding the legacy
/// `terminal.use` aggregate on BOTH sides (a legacy grant fits under a
/// split-id cap and vice versa).
pub fn permissions_excess<'a>(granted: &'a [String], cap: &[String]) -> Option<&'a String> {
    granted.iter().find(|permission| {
        if permission.as_str() == "terminal.use" {
            !["terminal.view", "terminal.write", "shell.spawn"]
                .iter()
                .all(|split| permissions_allow(cap, split))
        } else {
            !permissions_allow(cap, permission)
        }
    })
}

pub fn operation_permission_id(op: crate::access::access_policy::PeerOperation) -> &'static str {
    use crate::access::access_policy::PeerOperation;
    match op {
        PeerOperation::PresenceRead => "presence.read",
        PeerOperation::StatsRead => "stats.read",
        PeerOperation::DisplayView => "display.view",
        PeerOperation::DisplayInput => "display.input",
        PeerOperation::Message => "message.send",
        PeerOperation::Task => "task.run",
        PeerOperation::Approval => "approval.resolve",
        PeerOperation::AccessInspect => "access.inspect",
        PeerOperation::AccessManage => "access.manage",
        PeerOperation::PeerInspect => "peer.inspect",
        PeerOperation::PeerManage => "peer.manage",
        PeerOperation::PeerUse => "peer.use",
        PeerOperation::SessionInspect => "session.inspect",
        PeerOperation::SessionManage => "session.manage",
        PeerOperation::TerminalView => "terminal.view",
        PeerOperation::TerminalWrite => "terminal.write",
        PeerOperation::ShellSpawn => "shell.spawn",
        PeerOperation::Settings => "settings.manage",
        PeerOperation::CredentialsManage => "credentials.manage",
        PeerOperation::RuntimeControl => "runtime.control",
        PeerOperation::FilesystemRead => "filesystem.read",
        PeerOperation::FilesystemWrite => "filesystem.write",
        PeerOperation::AgendaRead => "agenda.read",
        PeerOperation::AgendaWrite => "agenda.write",
        PeerOperation::MemoryRead => "memory.read",
        PeerOperation::MemoryWrite => "memory.write",
    }
}

pub fn principal_overview_values(state: &LocalIamState) -> Vec<Value> {
    let fleet_zone = crate::fleet_cert::fleet_zone();
    principal_overview_values_with_fleet_zone(state, fleet_zone.as_deref())
}

pub(crate) fn principal_overview_values_with_fleet_zone(
    state: &LocalIamState,
    fleet_zone: Option<&str>,
) -> Vec<Value> {
    state
        .principals
        .iter()
        .map(|principal| {
            let metadata_only = principal.kind == "connect_account";
            let inactive_origin_class =
                inactive_pure_client_key_origin_class(state, principal, fleet_zone);
            let inactive_binding = inactive_origin_class.is_some();
            let stored_status = if principal.status.is_empty() {
                "draft"
            } else {
                principal.status.as_str()
            };
            json!({
                "id": principal.id.clone(),
                "kind": if principal.kind.is_empty() { "human_user" } else { principal.kind.as_str() },
                "kind_label": principal_kind_label(&principal.kind),
                "label": if principal.label.is_empty() { principal.id.as_str() } else { principal.label.as_str() },
                "source": if principal.source.is_empty() { "local_iam_state" } else { principal.source.as_str() },
                "status": if metadata_only { "metadata_only" } else if inactive_binding { "inactive_binding" } else { stored_status },
                "stored_status": stored_status,
                "metadata_only": metadata_only,
                "inactive_binding": inactive_binding,
                "origin_class": inactive_origin_class,
                "authority": if metadata_only || inactive_binding { "none" } else { "local_iam" },
                "local": false,
                "account": principal.account.clone(),
                "organization": principal.organization.clone(),
                "authn": principal.authn.clone(),
                "notes": principal.notes.clone(),
                "created_at_unix_ms": principal.created_at_unix_ms
            })
        })
        .collect()
}

pub fn grant_overview_values(state: &LocalIamState, default_target_id: &str) -> Vec<Value> {
    let fleet_zone = crate::fleet_cert::fleet_zone();
    grant_overview_values_with_fleet_zone(state, default_target_id, fleet_zone.as_deref())
}

pub(crate) fn grant_overview_values_with_fleet_zone(
    state: &LocalIamState,
    default_target_id: &str,
    fleet_zone: Option<&str>,
) -> Vec<Value> {
    let now = crate::access::client_key::now_unix_ms();
    state
        .grants
        .iter()
        .map(|grant| {
            let principal = state
                .principals
                .iter()
                .find(|principal| principal.id == grant.principal_id);
            let metadata_only =
                principal.is_some_and(|principal| principal.kind == "connect_account");
            let inactive_origin_class = principal.and_then(|principal| {
                inactive_pure_client_key_origin_class(state, principal, fleet_zone)
            });
            let inactive_binding = inactive_origin_class.is_some();
            let role_id = if grant.role_id.is_empty() {
                "role:scoped-human"
            } else {
                grant.role_id.as_str()
            };
            // An expired grant keeps its stored status on disk but reports
            // as `expired` so the UI never shows it as live.
            let expired = is_enforced_status(&grant.status) && !grant.is_active_at(now);
            let status = if grant.status.is_empty() {
                "draft"
            } else if expired {
                "expired"
            } else {
                grant.status.as_str()
            };
            // Stored grants carry the literal "local" sentinel when the
            // upsert request named no target (`upsert_user_client_grant`
            // defaults to it); it means "this daemon", never a peer.
            // Project it — like the empty default — to the daemon's real
            // target id so the dashboard resolves the row to this daemon's
            // label instead of echoing the raw sentinel ("on local").
            let target_id = match grant.target_id.as_str() {
                "" | "local" => default_target_id,
                other => other,
            };
            let projected_role_label = if inactive_binding {
                format!("Stored {} (inactive binding)", role_label(state, role_id))
            } else {
                role_label(state, role_id)
            };
            json!({
                "id": grant.id.clone(),
                "principal_id": grant.principal_id.clone(),
                "target_id": target_id,
                "kind": if metadata_only { "connect_account_metadata" } else { "user_client_local_iam" },
                "kind_label": if metadata_only { "Legacy Connect account metadata (no authority)" } else if inactive_binding { "Stored browser-key grant (inactive binding)" } else { "Local IAM user/client grant" },
                "policy_id": if grant.policy_id.is_empty() { "policy:scoped-human" } else { grant.policy_id.as_str() },
                "role": role_id,
                "role_label": projected_role_label,
                "transport_id": "transport:local-user-client-binding",
                "source": if grant.source.is_empty() { "local_iam_state" } else { grant.source.as_str() },
                "status": if metadata_only { "metadata_only" } else if inactive_binding { "inactive_binding" } else { status },
                "stored_status": status,
                "enforced": !metadata_only && !inactive_binding && grant.is_active_at(now),
                "metadata_only": metadata_only,
                "inactive_binding": inactive_binding,
                "origin_class": inactive_origin_class,
                "authority": if metadata_only || inactive_binding { "none" } else { "local_iam" },
                // The dashboard's grant-row fs chip reads this; a grant
                // without a scope serializes null so the chip stays off.
                "fs_scope": grant.fs_scope.as_ref().filter(|scope| !scope.is_empty()),
                "reason": grant.reason.clone(),
                "created_at_unix_ms": grant.created_at_unix_ms,
                "revoked_at_unix_ms": grant.revoked_at_unix_ms,
                "expires_at_unix_ms": grant.expires_at_unix_ms
            })
        })
        .collect()
}

fn builtin_role_templates() -> Vec<IamRole> {
    vec![
        IamRole {
            id: "role:root".to_string(),
            label: "Root".to_string(),
            status: "enforced".to_string(),
            summary: "Current owner/root dashboard authority.".to_string(),
            permissions: root_permission_ids(),
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:peer-profile".to_string(),
            label: "Peer profile".to_string(),
            status: "enforced".to_string(),
            summary: "Daemon-to-daemon grants enforced by the approved peer identity profile."
                .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "display.view".to_string(),
                "display.input".to_string(),
                "message.send".to_string(),
                "task.run".to_string(),
                "approval.resolve".to_string(),
                "access.inspect".to_string(),
                "peer.inspect".to_string(),
                "peer.manage".to_string(),
                "peer.use".to_string(),
                "session.inspect".to_string(),
                "session.manage".to_string(),
                "terminal.view".to_string(),
                "terminal.write".to_string(),
                "shell.spawn".to_string(),
                "settings.manage".to_string(),
                "runtime.control".to_string(),
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:none".to_string(),
            label: "No access".to_string(),
            status: "enforced".to_string(),
            summary: "Ceiling-only sentinel with no permissions: used in role_ceilings to \
                      refuse a binding kind (e.g. hosted-origin control) entirely. Never \
                      assigned to a principal."
                .to_string(),
            permissions: vec![],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:scoped-human".to_string(),
            label: "Scoped human".to_string(),
            status: "enforced".to_string(),
            summary: "Minimal user/client IAM role for stable browser mTLS and Connect account request bindings.".to_string(),
            permissions: vec!["access.inspect".to_string()],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:observer".to_string(),
            label: "Observer".to_string(),
            status: "enforced".to_string(),
            summary:
                "Read-only dashboard visibility without files, terminal, task control, or settings."
                    .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "display.view".to_string(),
                "access.inspect".to_string(),
                "peer.inspect".to_string(),
                "session.inspect".to_string(),
                // Observing the daemon includes seeing what's parked on
                // its agenda — read only.
                "agenda.read".to_string(),
                // Memory: observers may search/read, never propose.
                "memory.read".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:session-reader".to_string(),
            label: "Session reader".to_string(),
            status: "enforced".to_string(),
            summary: "Read sessions, logs, reports, and status without controlling the daemon."
                .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "access.inspect".to_string(),
                "session.inspect".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:terminal".to_string(),
            label: "Terminal".to_string(),
            status: "enforced".to_string(),
            summary: "Collaborate in shared shell sessions (view and type) without \
                      spawning new shells or broader dashboard mutation rights."
                .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "access.inspect".to_string(),
                "session.inspect".to_string(),
                "terminal.view".to_string(),
                "terminal.write".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:files-read".to_string(),
            label: "Files read".to_string(),
            status: "enforced".to_string(),
            summary: "Browse metadata and download files without writing to disk.".to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "access.inspect".to_string(),
                "filesystem.read".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:files-write".to_string(),
            label: "Files write".to_string(),
            status: "enforced".to_string(),
            summary: "Read files and upload/create file content through the dashboard.".to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "access.inspect".to_string(),
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:peer-user".to_string(),
            label: "Peer user".to_string(),
            status: "enforced".to_string(),
            summary: "Reach connected peers through this daemon (peer files, terminal, \
                      display tunnels); what each tunnel may do is decided by that \
                      peer's grants for this daemon. Combine with local roles as needed."
                .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "access.inspect".to_string(),
                "peer.inspect".to_string(),
                "peer.use".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:operator".to_string(),
            label: "Operator".to_string(),
            status: "enforced".to_string(),
            summary:
                "Operate sessions, display, shell, files, peers, and approvals without access/settings administration."
                    .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "display.view".to_string(),
                "display.input".to_string(),
                "message.send".to_string(),
                "task.run".to_string(),
                "approval.resolve".to_string(),
                "access.inspect".to_string(),
                "peer.inspect".to_string(),
                // Operating includes reaching connected peers (peer files,
                // terminal, display) — but not administering the peer
                // relationships themselves (peer.manage stays admin-side).
                "peer.use".to_string(),
                "session.inspect".to_string(),
                "session.manage".to_string(),
                "terminal.view".to_string(),
                "terminal.write".to_string(),
                "shell.spawn".to_string(),
                // Credential management is available to an explicitly
                // granted operator on a trusted direct/local transport.
                // Hosted Connect is discovery-only in the default build.
                "credentials.manage".to_string(),
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
                "agenda.read".to_string(),
                "agenda.write".to_string(),
                "memory.read".to_string(),
                "memory.write".to_string(),
            ],
            source: "builtin".to_string(),
        },
    ]
}

fn root_permission_ids() -> Vec<String> {
    [
        "presence.read",
        "stats.read",
        "display.view",
        "display.input",
        "message.send",
        "task.run",
        "approval.resolve",
        "access.inspect",
        "access.manage",
        "peer.inspect",
        "peer.manage",
        "peer.use",
        "session.inspect",
        "session.manage",
        "terminal.view",
        "terminal.write",
        "shell.spawn",
        "settings.manage",
        "credentials.manage",
        "runtime.control",
        "filesystem.read",
        "filesystem.write",
        "agenda.read",
        "agenda.write",
        "memory.read",
        "memory.write",
    ]
    .iter()
    .map(|permission| (*permission).to_string())
    .collect()
}

fn principal_kind_label(kind: &str) -> &'static str {
    match kind {
        "browser_certificate" => "Browser certificate",
        "client_key" => "Browser key",
        "connect_account" => "Connect account (legacy metadata only)",
        "passkey_account" => "Passkey account",
        "human_user" | "" => "Human user",
        "organization_group" => "Organization group",
        "agent_session" => "Supervised agent session",
        "local_process" => "Local process",
        _ => "IAM principal",
    }
}

fn role_label(state: &LocalIamState, role_id: &str) -> String {
    state
        .roles
        .iter()
        .find(|role| role.id == role_id)
        .map(|role| {
            if role.label.is_empty() {
                role.id.clone()
            } else {
                role.label.clone()
            }
        })
        .unwrap_or_else(|| role_id.to_string())
}

pub fn is_enforced_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "active" | "enforced"
    )
}

pub fn principal_for_browser_mtls_cert(
    state: &LocalIamState,
    fingerprint: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let fingerprint = normalize_fingerprint(fingerprint);
    principal_for_authn(
        state,
        "browser_mtls_cert",
        "fingerprint",
        &fingerprint,
        transport,
    )
}

pub fn principal_for_browser_mtls_cert_any_status(
    state: &LocalIamState,
    fingerprint: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let fingerprint = normalize_fingerprint(fingerprint);
    principal_for_authn_any_status(
        state,
        "browser_mtls_cert",
        "fingerprint",
        &fingerprint,
        transport,
    )
}

pub fn principal_for_client_key(
    state: &LocalIamState,
    fingerprint: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let fingerprint = normalize_client_key_fingerprint(fingerprint);
    principal_for_authn(state, "client_key", "fingerprint", &fingerprint, transport)
}

/// Resolve a supervised agent `/mcp` session to a scoped local IAM
/// principal. An exact `session_id` binding wins; the wildcard `"*"`
/// binding (one grant scoping every supervised agent session) is the
/// fallback. `None` means the owner has not scoped agent sessions and the
/// caller should synthesize the default transport-trusted principal.
pub fn principal_for_agent_session(
    state: &LocalIamState,
    session_id: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let transport = transport.into();
    principal_for_authn(
        state,
        "agent_session",
        "session_id",
        session_id,
        transport.clone(),
    )
    .or_else(|| principal_for_authn(state, "agent_session", "session_id", "*", transport))
}

/// Any-status counterpart of [`principal_for_agent_session`], mirroring
/// the browser-certificate pattern: a *known* binding whose grant has
/// lapsed (expired or revoked) still binds the scoped principal, so the
/// evaluator denies with the real reason instead of the caller falling
/// back to transport-default trust. Once an owner has named an agent
/// session, its authority comes only from grants.
pub fn principal_for_agent_session_any_status(
    state: &LocalIamState,
    session_id: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let transport = transport.into();
    principal_for_authn_any_status(
        state,
        "agent_session",
        "session_id",
        session_id,
        transport.clone(),
    )
    .or_else(|| {
        principal_for_authn_any_status(state, "agent_session", "session_id", "*", transport)
    })
}

/// Resolve the tokenless loopback `/mcp` caller to a scoped local IAM
/// principal, when the owner has created a `local_process` grant. `None`
/// means the default root-compatible loopback principal applies.
pub fn principal_for_loopback_mcp(
    state: &LocalIamState,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    principal_for_authn(state, "loopback_mcp", "scope", "loopback", transport)
}

/// Any-status counterpart of [`principal_for_loopback_mcp`]: a lapsed
/// `local_process` grant denies rather than silently restoring the
/// root-compatible loopback default. Restoring implicit-root behavior is
/// an explicit re-grant (e.g. `role:root`), not a timer or a revocation
/// side effect.
pub fn principal_for_loopback_mcp_any_status(
    state: &LocalIamState,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    principal_for_authn_any_status(state, "loopback_mcp", "scope", "loopback", transport)
}

/// Whether the owner has ever scoped supervised agent sessions: any
/// principal carrying an `agent_session` binding counts, regardless of
/// principal or grant status. Once true, the tokenless loopback `/mcp`
/// default flips from root-compatible to fail-closed — otherwise a scoped
/// agent could shed its injected token and re-enter as the unscoped
/// local-process principal, making the agent grant decorative. The flag is
/// deliberately sticky across expiry *and* revocation: neither a timer
/// running out nor "cut this agent off" may quietly reopen the anonymous
/// local door. The explicit way back is a `local_process` grant stating
/// what bare loopback callers get.
pub fn agent_session_scoping_present(state: &LocalIamState) -> bool {
    state.principals.iter().any(|principal| {
        principal
            .authn
            .iter()
            .any(|authn| authn.get("kind").and_then(Value::as_str) == Some("agent_session"))
    })
}

fn matched_authn_origin(
    principal: &IamPrincipal,
    authn_kind: &str,
    key: &str,
    value: &str,
) -> Option<String> {
    principal
        .authn
        .iter()
        .find(|authn| {
            authn.get("kind").and_then(Value::as_str) == Some(authn_kind)
                && authn.get(key).and_then(Value::as_str) == Some(value)
        })
        .and_then(|authn| authn.get("origin").and_then(Value::as_str))
        .map(str::to_string)
}

fn principal_for_authn(
    state: &LocalIamState,
    authn_kind: &str,
    key: &str,
    value: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let principal = state.principals.iter().find(|principal| {
        is_enforced_status(&principal.status)
            && principal.authn.iter().any(|authn| {
                authn.get("kind").and_then(Value::as_str) == Some(authn_kind)
                    && authn.get(key).and_then(Value::as_str) == Some(value)
            })
    })?;
    let now = crate::access::client_key::now_unix_ms();
    let grant = state
        .grants
        .iter()
        .find(|grant| grant.principal_id == principal.id && grant.is_active_at(now))?;
    let mut access = AccessPrincipal::local_user_client(principal, grant, transport);
    access.authn_kind = Some(authn_kind.to_string());
    access.authn_binding = Some(value.to_string());
    access.authn_origin = matched_authn_origin(principal, authn_kind, key, value);
    Some(access)
}

fn principal_for_authn_any_status(
    state: &LocalIamState,
    authn_kind: &str,
    key: &str,
    value: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let principal = state.principals.iter().find(|principal| {
        principal.authn.iter().any(|authn| {
            authn.get("kind").and_then(Value::as_str) == Some(authn_kind)
                && authn.get(key).and_then(Value::as_str) == Some(value)
        })
    })?;
    let now = crate::access::client_key::now_unix_ms();
    let grant = state
        .grants
        .iter()
        .find(|grant| grant.principal_id == principal.id && grant.is_active_at(now))
        .or_else(|| {
            state
                .grants
                .iter()
                .find(|grant| grant.principal_id == principal.id)
        })?;
    let mut access = AccessPrincipal::local_user_client(principal, grant, transport);
    access.authn_kind = Some(authn_kind.to_string());
    access.authn_binding = Some(value.to_string());
    access.authn_origin = matched_authn_origin(principal, authn_kind, key, value);
    Some(access)
}

#[allow(dead_code)]
fn set_private_perms(path: &Path) -> AccessResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the shape an older release could persist before active
    /// service-controlled browser-key grants became a refused mutation.
    fn insert_legacy_active_client_key_grant(
        state: &mut LocalIamState,
        fingerprint: &str,
        origin: &str,
        role_id: &str,
        fleet_zone: Option<&str>,
    ) -> UserClientGrantUpsertResult {
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let mut result = upsert_user_client_grant_with_fleet_zone(
            state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some(fingerprint.to_string()),
                client_key_origin: Some(origin.to_string()),
                role_id: Some(role_id.to_string()),
                status: Some("draft".to_string()),
                ..Default::default()
            },
            &actor,
            fleet_zone,
        )
        .unwrap();
        let principal = state
            .principals
            .iter_mut()
            .find(|principal| principal.id == result.principal.id)
            .unwrap();
        principal.status = "active".to_string();
        result.principal.status = "active".to_string();
        let grant = state
            .grants
            .iter_mut()
            .find(|grant| grant.id == result.grant.id)
            .unwrap();
        grant.status = "active".to_string();
        result.grant.status = "active".to_string();
        result
    }

    fn generate_owner_access_cert(cert_dir: &Path) -> String {
        let names = super::super::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        super::super::certs::ensure_certs(cert_dir, &names, "owner", false).unwrap();
        super::super::certs::read_owner_client_cert_fingerprint(cert_dir).unwrap()
    }

    /// The dashboard ships static fallback copies of the IAM catalog
    /// (rendered when the daemon predates `/api/access/iam` or the call
    /// fails). Those copies can't derive from this file, so these tests pin
    /// their ID SETS to the builtin catalog — labels and prose stay free.
    fn app_html() -> &'static str {
        include_str!("../../../../static/app.html")
    }

    fn slice_between<'a>(hay: &'a str, start: &str, end: &str) -> &'a str {
        let from = hay
            .find(start)
            .unwrap_or_else(|| panic!("marker {start:?} not found in app.html"))
            + start.len();
        let rest = &hay[from..];
        let to = rest
            .find(end)
            .unwrap_or_else(|| panic!("end marker {end:?} not found after {start:?}"));
        &rest[..to]
    }

    fn quoted_role_ids(text: &str) -> std::collections::BTreeSet<String> {
        let pattern = regex::Regex::new(r"'(role:[a-z-]+)'").unwrap();
        pattern
            .captures_iter(text)
            .map(|caps| caps[1].to_string())
            .collect()
    }

    fn quoted_permission_ids(text: &str) -> std::collections::BTreeSet<String> {
        let pattern = regex::Regex::new(r"'([a-z]+\.[a-z]+)'").unwrap();
        pattern
            .captures_iter(text)
            .map(|caps| caps[1].to_string())
            .collect()
    }

    fn quoted_snake_case_ids(text: &str) -> std::collections::BTreeSet<String> {
        let pattern = regex::Regex::new(r"'([a-z][a-z0-9_]*)'").unwrap();
        pattern
            .captures_iter(text)
            .map(|caps| caps[1].to_string())
            .collect()
    }

    #[test]
    fn dashboard_fallback_role_catalog_mirrors_builtin_roles() {
        let app = app_html();
        let builtin: std::collections::BTreeSet<String> = builtin_role_templates()
            .into_iter()
            .map(|role| role.id)
            .collect();

        let fallback = slice_between(app, "function accessFallbackIamRoles() {", "\nfunction ");
        assert_eq!(
            quoted_role_ids(fallback),
            builtin,
            "accessFallbackIamRoles (static/app.html) drifted from builtin_role_templates"
        );

        let meta = slice_between(app, "const ACCESS_ROLE_META = {", "\n};");
        assert_eq!(
            quoted_role_ids(meta),
            builtin,
            "ACCESS_ROLE_META (static/app.html) drifted from builtin_role_templates"
        );

        let picker = slice_between(app, "const ACCESS_ROLE_PICKER_ORDER = [", "\n];");
        let mut pickable = builtin.clone();
        // The peer-profile role is daemon-peer-only; humans never pick it.
        assert!(pickable.remove("role:peer-profile"));
        // role:none is the ceiling-only sentinel; it is never granted.
        assert!(pickable.remove("role:none"));
        assert_eq!(
            quoted_role_ids(picker),
            pickable,
            "ACCESS_ROLE_PICKER_ORDER (static/app.html) drifted from builtin_role_templates"
        );
    }

    #[test]
    fn dashboard_tier_vocabulary_mirrors_daemon_tiers() {
        let app = app_html();
        let meta = slice_between(app, "const ACCESS_TIER_META = {", "\n};");
        let pattern = regex::Regex::new(r"'([a-z]+)':\s*\{").unwrap();
        let mirrored: Vec<String> = pattern
            .captures_iter(meta)
            .map(|caps| caps[1].to_string())
            .collect();
        let expected: Vec<String> = DAEMON_TIERS.iter().map(|tier| tier.to_string()).collect();
        assert_eq!(
            mirrored, expected,
            "ACCESS_TIER_META (static/app.html) drifted from DAEMON_TIERS — \
             ids and presentation order are both pinned"
        );
    }

    #[test]
    fn dashboard_exposes_no_hosted_ceiling_raise_control() {
        let app = app_html();
        assert!(
            !app.contains("ACCESS_HOSTED_CEILING_CHOICES"),
            "the default dashboard must not expose hosted-ceiling choices"
        );
        assert!(
            !app.contains("api_access_set_hosted_ceiling"),
            "the default dashboard must not expose a hosted-ceiling mutation method"
        );
        assert!(
            app.contains("Nothing (immutable)"),
            "the dashboard should state the compiled discovery-only policy"
        );
    }

    #[test]
    fn dashboard_fallback_enforced_principal_kinds_mirror_overview() {
        let app = app_html();
        let fallback = slice_between(app, "enforced_principal_kinds: [", "],");
        let load = LoadedIamState {
            path: PathBuf::new(),
            state: LocalIamState::default(),
            status: IamStateStatus::Missing,
        };
        let expected: std::collections::BTreeSet<String> = overview_metadata(&load)["enforcement"]
            ["enforced_principal_kinds"]
            .as_array()
            .expect("overview enforced_principal_kinds array")
            .iter()
            .map(|kind| kind.as_str().expect("principal kind string").to_string())
            .collect();
        assert_eq!(
            quoted_snake_case_ids(fallback),
            expected,
            "dashboard fallback enforced_principal_kinds drifted from IAM overview"
        );
    }

    #[test]
    fn dashboard_fallback_permission_catalog_mirrors_root_permission_ids() {
        let app = app_html();
        let root: std::collections::BTreeSet<String> = root_permission_ids().into_iter().collect();

        let root_js = slice_between(app, "const rootPermissions = [", "];");
        assert_eq!(
            quoted_permission_ids(root_js),
            root,
            "rootPermissions (static/app.html) drifted from root_permission_ids"
        );

        let summaries = slice_between(
            app,
            "function accessFallbackPermissions() {",
            "return Object.entries",
        );
        let mut explained = root;
        // Legacy aggregate the catalog still explains for old grants; not a
        // grantable root permission.
        explained.insert("terminal.use".to_string());
        assert_eq!(
            quoted_permission_ids(summaries),
            explained,
            "accessFallbackPermissions (static/app.html) drifted from root_permission_ids"
        );
    }

    /// The peer.manage → manage/use split: grants that predate peer.use and
    /// carry peer.manage keep tunnel access, the reverse never holds, and
    /// the builtin roles divide the two deliberately (operator uses peers
    /// without administering them; files/terminal roles get neither).
    #[test]
    fn peer_use_split_implication_and_roles() {
        let legacy = vec!["peer.manage".to_string()];
        assert!(permissions_allow(&legacy, "peer.use"));
        assert!(permissions_allow(&legacy, "peer.manage"));
        let use_only = vec!["peer.use".to_string()];
        assert!(!permissions_allow(&use_only, "peer.manage"));
        assert!(permissions_excess(&use_only, &legacy).is_none());
        assert_eq!(
            permissions_excess(&legacy, &use_only).map(String::as_str),
            Some("peer.manage")
        );

        let roles = builtin_role_templates();
        let permissions = |id: &str| {
            roles
                .iter()
                .find(|role| role.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .permissions
                .clone()
        };
        assert!(permissions_allow(&permissions("role:operator"), "peer.use"));
        assert!(!permissions_allow(
            &permissions("role:operator"),
            "peer.manage"
        ));
        assert!(permissions_allow(
            &permissions("role:peer-user"),
            "peer.use"
        ));
        assert!(permissions_allow(
            &permissions("role:peer-user"),
            "peer.inspect"
        ));
        assert!(!permissions_allow(
            &permissions("role:peer-user"),
            "filesystem.read"
        ));
        for role in [
            "role:files-write",
            "role:files-read",
            "role:terminal",
            "role:observer",
        ] {
            assert!(
                !permissions_allow(&permissions(role), "peer.use"),
                "{role} must not reach peer tunnels by default"
            );
        }
        assert!(permissions_allow(&permissions("role:root"), "peer.use"));
    }

    /// The terminal.use → view/write/spawn split: the legacy aggregate in a
    /// custom role's permission list keeps granting all three, containment
    /// expands it on both sides, and persisted builtin roles are refreshed
    /// to the current template on load (so role:terminal actually loses
    /// shell.spawn on upgrade instead of keeping it via legacy expansion).
    #[test]
    fn terminal_permission_split_legacy_and_migration() {
        let legacy = vec!["terminal.use".to_string()];
        assert!(permissions_allow(&legacy, "terminal.view"));
        assert!(permissions_allow(&legacy, "terminal.write"));
        assert!(permissions_allow(&legacy, "shell.spawn"));
        assert!(!permissions_allow(&legacy, "filesystem.read"));
        let split = vec!["terminal.view".to_string(), "terminal.write".to_string()];
        assert!(!permissions_allow(&split, "shell.spawn"));
        assert!(!permissions_allow(&split, "terminal.use"));

        // Containment: legacy fits under split caps and vice versa.
        let all_split = vec![
            "terminal.view".to_string(),
            "terminal.write".to_string(),
            "shell.spawn".to_string(),
        ];
        assert!(permissions_excess(&legacy, &all_split).is_none());
        assert!(permissions_excess(&all_split, &legacy).is_none());
        assert_eq!(
            permissions_excess(&legacy, &split).map(String::as_str),
            Some("terminal.use")
        );

        // Migration: a persisted pre-split builtin role is refreshed.
        let mut stale = LocalIamState::default();
        let terminal_role = stale
            .roles
            .iter_mut()
            .find(|role| role.id == "role:terminal")
            .unwrap();
        terminal_role.permissions = vec!["terminal.use".to_string()];
        let migrated = stale.normalize();
        let refreshed = migrated
            .roles
            .iter()
            .find(|role| role.id == "role:terminal")
            .unwrap();
        assert!(refreshed
            .permissions
            .iter()
            .any(|permission| permission == "terminal.view"));
        assert!(!refreshed
            .permissions
            .iter()
            .any(|permission| permission == "terminal.use"));
        assert!(!refreshed
            .permissions
            .iter()
            .any(|permission| permission == "shell.spawn"));

        // Custom roles are never rewritten — and never dropped, while a
        // RETIRED builtin (role:directory-files shipped as a planned
        // placeholder before grant-level fs scopes superseded it) is
        // removed from persisted state on load.
        let mut custom_state = LocalIamState::default();
        custom_state.roles.push(IamRole {
            id: "role:custom-legacy".to_string(),
            label: "Custom".to_string(),
            status: "enforced".to_string(),
            summary: String::new(),
            permissions: vec!["terminal.use".to_string()],
            source: "local".to_string(),
        });
        custom_state.roles.push(IamRole {
            id: "role:directory-files".to_string(),
            label: "Directory scoped files".to_string(),
            status: "planned".to_string(),
            summary: String::new(),
            permissions: vec!["filesystem.read".to_string()],
            source: "builtin".to_string(),
        });
        let normalized = custom_state.normalize();
        let custom = normalized
            .roles
            .iter()
            .find(|role| role.id == "role:custom-legacy")
            .unwrap();
        assert_eq!(custom.permissions, vec!["terminal.use".to_string()]);
        assert!(
            !normalized
                .roles
                .iter()
                .any(|role| role.id == "role:directory-files"),
            "retired builtin role should be dropped on load"
        );
    }

    #[test]
    fn schema_v2_revokes_legacy_connect_bootstrap_grants_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let legacy = insert_legacy_active_client_key_grant(
            &mut state,
            "legacy-key",
            "connect-bootstrap",
            "role:root",
            None,
        );
        let direct = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("direct-key".to_string()),
                client_key_origin: Some("https://anchor.local:8765".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let retired_owner = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("retired-owner-key".to_string()),
                role_id: Some("role:root".to_string()),
                reason: Some(
                    "--owner bootstrap: root authority pinned to this browser key at install time"
                        .to_string(),
                ),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(retired_owner.grant.status, "active");
        assert_eq!(retired_owner.grant.role_id, "role:root");
        state.schema_version = 1;
        state.role_ceilings.clear();
        std::fs::write(
            iam_state_path(tmp.path()),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let migrated = load_state(tmp.path()).unwrap();
        assert_eq!(migrated.schema_version, IAM_SCHEMA_VERSION);
        for binding in HOSTED_CEILING_BINDINGS {
            assert_eq!(
                migrated.role_ceilings.get(binding).map(String::as_str),
                Some("role:none")
            );
        }
        let legacy_grant = migrated
            .grants
            .iter()
            .find(|grant| grant.id == legacy.grant.id)
            .unwrap();
        assert_eq!(legacy_grant.status, "revoked");
        assert!(legacy_grant.revoked_at_unix_ms.is_some());
        assert!(migrated
            .audit_events
            .iter()
            .any(|event| event.action == "revoke_legacy_connect_bootstrap"));
        assert_eq!(
            migrated
                .grants
                .iter()
                .find(|grant| grant.id == retired_owner.grant.id)
                .unwrap()
                .status,
            "revoked",
            "retired browser-key owner bootstrap must require direct-mTLS re-enrollment"
        );
        assert!(migrated
            .audit_events
            .iter()
            .any(|event| { event.action == "revoke_legacy_owner_browser_key_bootstrap" }));
        assert_eq!(
            migrated
                .grants
                .iter()
                .find(|grant| grant.id == direct.grant.id)
                .unwrap()
                .status,
            "active",
            "trusted direct grants must survive the hosted-root migration"
        );

        let persisted: LocalIamState =
            serde_json::from_slice(&std::fs::read(iam_state_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(persisted.schema_version, IAM_SCHEMA_VERSION);
        assert_eq!(
            persisted
                .grants
                .iter()
                .find(|grant| grant.id == legacy.grant.id)
                .unwrap()
                .status,
            "revoked"
        );
    }

    #[test]
    fn schema_v2_adds_default_hosted_control_state_and_persists_v3() {
        let tmp = tempfile::tempdir().unwrap();
        let mut encoded = serde_json::to_value(LocalIamState::default()).unwrap();
        encoded["schema_version"] = serde_json::json!(2);
        encoded.as_object_mut().unwrap().remove("hosted_control");
        std::fs::write(
            iam_state_path(tmp.path()),
            serde_json::to_vec_pretty(&encoded).unwrap(),
        )
        .unwrap();

        let migrated = load_state(tmp.path()).unwrap();
        assert_eq!(migrated.schema_version, IAM_SCHEMA_VERSION);
        assert_eq!(
            migrated.hosted_control,
            super::super::hosted_control::HostedControlState::default()
        );

        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(iam_state_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(persisted["schema_version"], IAM_SCHEMA_VERSION);
        assert!(
            persisted.get("hosted_control").is_some(),
            "schema-3 persistence must materialize the hosted-control store"
        );
    }

    #[test]
    fn unknown_ca_valid_cert_cannot_initialize_root_but_generated_owner_can() {
        let tmp = tempfile::tempdir().unwrap();
        let owner_fingerprint = generate_owner_access_cert(tmp.path());
        let unknown_fingerprint = "11".repeat(32);

        let unknown =
            initialize_browser_mtls_root_if_needed(tmp.path(), &unknown_fingerprint).unwrap();
        assert!(unknown.principals.is_empty());
        assert!(!iam_state_path(tmp.path()).exists());
        assert!(!browser_mtls_initialized_path(tmp.path()).exists());

        let initialized =
            initialize_browser_mtls_root_if_needed(tmp.path(), &owner_fingerprint).unwrap();
        let owner =
            principal_for_browser_mtls_cert(&initialized, &owner_fingerprint, "test-owner-mtls")
                .expect("generated owner certificate must be explicitly enrolled");
        assert_eq!(owner.role_id, "role:root");
        assert!(browser_mtls_initialized_path(tmp.path()).exists());
        assert!(iam_state_path(tmp.path()).exists());

        let after =
            initialize_browser_mtls_root_if_needed(tmp.path(), &unknown_fingerprint).unwrap();
        assert!(
            principal_for_browser_mtls_cert(&after, &unknown_fingerprint, "test-unknown-mtls")
                .is_none()
        );
        assert_eq!(after.grants.len(), 1, "unknown cert must not add a grant");
    }

    #[test]
    fn trusted_startup_migrates_generated_owner_once() {
        let tmp = tempfile::tempdir().unwrap();
        let owner_fingerprint = generate_owner_access_cert(tmp.path());

        assert!(migrate_generated_browser_mtls_owner_root_at_startup(tmp.path()).unwrap());
        assert!(browser_mtls_initialized_path(tmp.path()).exists());
        let state = load_state(tmp.path()).unwrap();
        assert!(principal_for_browser_mtls_cert(
            &state,
            &owner_fingerprint,
            "test-startup-owner-mtls"
        )
        .is_some_and(|principal| principal.role_id == "role:root"));

        assert!(
            !migrate_generated_browser_mtls_owner_root_at_startup(tmp.path()).unwrap(),
            "the sticky marker makes subsequent startups a no-op"
        );
        assert_eq!(load_state(tmp.path()).unwrap().grants.len(), 1);
    }

    #[test]
    fn trusted_startup_without_generated_owner_is_read_only() {
        let tmp = tempfile::tempdir().unwrap();

        assert!(!migrate_generated_browser_mtls_owner_root_at_startup(tmp.path()).unwrap());
        assert!(!iam_state_path(tmp.path()).exists());
        assert!(!browser_mtls_initialized_path(tmp.path()).exists());
    }

    #[test]
    fn scoped_non_owner_first_use_does_not_consume_owner_initialization() {
        let tmp = tempfile::tempdir().unwrap();
        let owner_fingerprint = generate_owner_access_cert(tmp.path());
        let scoped_fingerprint = "33".repeat(32);
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "trusted-local");
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some(scoped_fingerprint.clone()),
                role_id: Some("role:files-read".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        save_state(tmp.path(), &state).unwrap();

        let scoped =
            initialize_browser_mtls_root_if_needed(tmp.path(), &scoped_fingerprint).unwrap();
        assert!(
            principal_for_browser_mtls_cert(&scoped, &scoped_fingerprint, "test-scoped-mtls")
                .is_some_and(|principal| principal.role_id == "role:files-read")
        );
        assert!(
            !browser_mtls_initialized_path(tmp.path()).exists(),
            "a non-owner scoped binding must not lock out the generated owner"
        );

        let initialized =
            initialize_browser_mtls_root_if_needed(tmp.path(), &owner_fingerprint).unwrap();
        assert!(principal_for_browser_mtls_cert(
            &initialized,
            &owner_fingerprint,
            "test-owner-mtls"
        )
        .is_some_and(|principal| principal.role_id == "role:root"));
        assert!(browser_mtls_initialized_path(tmp.path()).exists());
    }

    #[test]
    fn sticky_mtls_marker_makes_missing_iam_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let owner_fingerprint = generate_owner_access_cert(tmp.path());
        seed_generated_browser_mtls_owner_root(tmp.path()).unwrap();
        assert!(browser_mtls_initialized_path(tmp.path()).exists());
        std::fs::remove_file(iam_state_path(tmp.path())).unwrap();

        let state = initialize_browser_mtls_root_if_needed(tmp.path(), &owner_fingerprint).unwrap();
        assert!(state.principals.is_empty());
        assert!(state.grants.is_empty());
        assert!(
            !iam_state_path(tmp.path()).exists(),
            "a sticky marker must not reconstruct deleted IAM state"
        );
    }

    #[test]
    fn concurrent_owner_and_unknown_mtls_first_use_mint_exactly_one_root() {
        let tmp = tempfile::tempdir().unwrap();
        let owner_fingerprint = generate_owner_access_cert(tmp.path());
        let unknown_fingerprint = "22".repeat(32);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let dir = tmp.path().to_path_buf();

        let spawn = |fingerprint: String| {
            let barrier = std::sync::Arc::clone(&barrier);
            let dir = dir.clone();
            std::thread::spawn(move || {
                barrier.wait();
                initialize_browser_mtls_root_if_needed(&dir, &fingerprint).unwrap();
            })
        };
        let owner = spawn(owner_fingerprint.clone());
        let unknown = spawn(unknown_fingerprint.clone());
        barrier.wait();
        owner.join().unwrap();
        unknown.join().unwrap();

        let state = load_state(tmp.path()).unwrap();
        let roots: Vec<_> = state
            .grants
            .iter()
            .filter(|grant| grant.role_id == "role:root")
            .collect();
        assert_eq!(roots.len(), 1);
        assert!(
            principal_for_browser_mtls_cert(&state, &owner_fingerprint, "test-owner-mtls")
                .is_some_and(|principal| principal.role_id == "role:root")
        );
        assert!(
            principal_for_browser_mtls_cert(&state, &unknown_fingerprint, "test-unknown-mtls")
                .is_none()
        );
    }

    #[test]
    fn force_regenerated_owner_cert_is_persisted_behind_existing_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let names = super::super::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        super::super::certs::ensure_certs(tmp.path(), &names, "owner", false).unwrap();
        let old = super::super::certs::read_owner_client_cert_fingerprint(tmp.path()).unwrap();
        seed_generated_browser_mtls_owner_root(tmp.path()).unwrap();

        super::super::certs::ensure_certs(tmp.path(), &names, "owner", true).unwrap();
        let new = super::super::certs::read_owner_client_cert_fingerprint(tmp.path()).unwrap();
        assert_ne!(old, new);
        assert!(browser_mtls_initialized_path(tmp.path()).exists());
        assert!(seed_generated_browser_mtls_owner_root(tmp.path()).unwrap());

        let state = load_state(tmp.path()).unwrap();
        assert!(
            principal_for_browser_mtls_cert(&state, &new, "test-force-owner")
                .is_some_and(|principal| principal.role_id == "role:root")
        );
    }

    #[test]
    fn missing_state_loads_default_foundation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = load_state_for_overview(tmp.path());

        assert_eq!(loaded.status, IamStateStatus::Missing);
        assert_eq!(loaded.state.schema_version, IAM_SCHEMA_VERSION);
        assert!(loaded.state.roles.iter().any(|r| r.id == "role:root"));
    }

    #[test]
    fn save_load_round_trips_managed_records() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:human:alice".to_string(),
            kind: "human_user".to_string(),
            label: "Alice".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: Vec::new(),
            notes: Some("not enforced yet".to_string()),
            created_at_unix_ms: Some(123),
        });
        state.grants.push(IamGrant {
            id: "grant:alice:local:scoped".to_string(),
            principal_id: "principal:human:alice".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:scoped-human".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            reason: "example".to_string(),
            created_at_unix_ms: Some(124),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });

        save_state(tmp.path(), &state).unwrap();
        let loaded = load_state(tmp.path()).unwrap();

        assert_eq!(loaded.managed_principal_count(), 1);
        assert_eq!(loaded.managed_grant_count(), 1);
        assert!(iam_state_path(tmp.path()).exists());
    }

    #[test]
    fn normalize_bounds_audit_events_to_newest_tail() {
        let mut state = LocalIamState::default();
        for index in 0..(IAM_AUDIT_EVENTS_CAP + 50) {
            state.audit_events.push(IamAuditEvent {
                id: format!("audit:{index}"),
                at_unix_ms: Some(index as u64),
                actor_principal_id: "principal:test".to_string(),
                action: "test".to_string(),
                target_id: "target".to_string(),
                summary: format!("event {index}"),
            });
        }
        let state = state.normalize();
        assert_eq!(state.audit_events.len(), IAM_AUDIT_EVENTS_CAP);
        // Oldest events dropped, newest retained, order preserved.
        assert_eq!(state.audit_events.first().unwrap().id, "audit:50");
        assert_eq!(
            state.audit_events.last().unwrap().id,
            format!("audit:{}", IAM_AUDIT_EVENTS_CAP + 49)
        );
    }

    #[test]
    fn malformed_state_reports_error_for_overview() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(iam_state_path(tmp.path()), b"{not json").unwrap();

        let loaded = load_state_for_overview(tmp.path());

        assert!(matches!(loaded.status, IamStateStatus::Error(_)));
        assert_eq!(loaded.state.managed_grant_count(), 0);
    }

    #[test]
    fn load_state_cached_matches_uncached_and_sees_external_writes() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Missing file: same default-state contract as load_state.
        assert_eq!(
            *load_state_cached_arc(tmp.path()).unwrap(),
            load_state(tmp.path()).unwrap()
        );

        // save_state → cached read returns the saved state (and matches a
        // fresh uncached parse).
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:cache-a".to_string(),
            kind: "user_client".to_string(),
            label: "Cache A".to_string(),
            status: "active".to_string(),
            source: "test".to_string(),
            account: None,
            organization: None,
            authn: Vec::new(),
            notes: None,
            created_at_unix_ms: Some(1),
        });
        save_state(tmp.path(), &state).unwrap();
        let cached = load_state_cached_arc(tmp.path()).unwrap();
        assert_eq!(*cached, load_state(tmp.path()).unwrap());
        assert!(cached
            .principals
            .iter()
            .any(|p| p.id == "principal:cache-a"));
        // Second read (a cache hit) is the same shared snapshot.
        let hit = load_state_cached_arc(tmp.path()).unwrap();
        assert!(std::sync::Arc::ptr_eq(&hit, &cached));
        assert_eq!(*hit, *cached);

        // A writer that bypasses save_state (another process, a hand
        // edit) must be picked up by the per-call fingerprint re-check —
        // never trust invalidation.
        let mut external = (*cached).clone();
        for principal in &mut external.principals {
            if principal.id == "principal:cache-a" {
                principal.label = "Cache A (externally rewritten)".to_string();
            }
        }
        let body = serde_json::to_string_pretty(&external).unwrap();
        std::fs::write(iam_state_path(tmp.path()), body).unwrap();
        let reread = load_state_cached_arc(tmp.path()).unwrap();
        assert!(reread
            .principals
            .iter()
            .any(|p| p.label == "Cache A (externally rewritten)"));
        assert_eq!(*reread, load_state(tmp.path()).unwrap());

        // Deleting the file falls back to the default state.
        std::fs::remove_file(iam_state_path(tmp.path())).unwrap();
        assert_eq!(
            *load_state_cached_arc(tmp.path()).unwrap(),
            LocalIamState::default()
        );
    }

    #[test]
    fn normalize_refreshes_stale_builtin_roles_but_not_user_roles() {
        // An on-disk state minted before credentials.manage existed: its
        // role:operator is builtin but lacks the permission.
        let mut state = LocalIamState::default();
        let operator = state
            .roles
            .iter_mut()
            .find(|role| role.id == "role:operator")
            .expect("builtin operator role");
        operator
            .permissions
            .retain(|permission| permission != "credentials.manage");
        operator.summary = "stale on-disk summary".to_string();
        state.roles.push(IamRole {
            id: "role:custom".to_string(),
            label: "Custom".to_string(),
            status: "enforced".to_string(),
            summary: "user-created".to_string(),
            permissions: vec!["stats.read".to_string()],
            source: "local_iam_state".to_string(),
        });

        let normalized = state.normalize();

        let operator = normalized
            .roles
            .iter()
            .find(|role| role.id == "role:operator")
            .expect("operator survives");
        assert!(
            operator
                .permissions
                .iter()
                .any(|permission| permission == "credentials.manage"),
            "builtin operator was not refreshed from the template"
        );
        assert_ne!(operator.summary, "stale on-disk summary");
        let custom = normalized
            .roles
            .iter()
            .find(|role| role.id == "role:custom")
            .expect("user role survives");
        assert_eq!(custom.summary, "user-created");
        assert_eq!(custom.permissions, vec!["stats.read".to_string()]);
    }

    #[test]
    fn credentials_manage_is_root_and_operator_but_no_peer_profile() {
        assert!(root_permission_ids()
            .iter()
            .any(|id| id == "credentials.manage"));
        let templates = builtin_role_templates();
        let has = |role_id: &str| {
            templates
                .iter()
                .find(|role| role.id == role_id)
                .map(|role| {
                    role.permissions
                        .iter()
                        .any(|permission| permission == "credentials.manage")
                })
                .unwrap_or(false)
        };
        assert!(
            has("role:operator"),
            "operator must hold credentials.manage"
        );
        assert!(!has("role:observer"));
        assert!(!has("role:session-reader"));
        assert!(!has("role:terminal"));
        assert!(!has("role:scoped-human"));
        // The peer lane is excluded in v1 — not even admin peers may
        // fuel or drain credentials.
        for profile in [
            "presence-only",
            "stats",
            "session-reader",
            "read-only-display",
            "file-operator",
            "terminal-operator",
            "task-runner",
            "operator",
            "admin-peer",
        ] {
            assert!(
                !crate::access::access_policy::profile_allows_operation(
                    profile,
                    crate::access::access_policy::PeerOperation::CredentialsManage,
                ),
                "peer profile {profile} must not allow credentials.manage"
            );
        }
    }

    #[test]
    fn overview_values_mark_local_iam_grants_unenforced() {
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:human:alice".to_string(),
            kind: "human_user".to_string(),
            label: "Alice".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: Vec::new(),
            notes: None,
            created_at_unix_ms: None,
        });
        state.grants.push(IamGrant {
            id: "grant:alice".to_string(),
            principal_id: "principal:human:alice".to_string(),
            target_id: String::new(),
            role_id: "role:scoped-human".to_string(),
            policy_id: String::new(),
            status: String::new(),
            source: String::new(),
            reason: String::new(),
            created_at_unix_ms: None,
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });

        let grants = grant_overview_values(&state, "local-daemon");

        assert_eq!(grants[0]["target_id"], "local-daemon");
        assert_eq!(grants[0]["status"], "draft");
        assert_eq!(grants[0]["enforced"], false);
    }

    /// `upsert_user_client_grant` stores the literal "local" sentinel when
    /// the request names no target. The overview projection must map that
    /// sentinel — exactly like the empty default — to the daemon's real
    /// target id, or the dashboard's grant rows echo the raw string
    /// ("Scoped human on local") instead of resolving this daemon's label
    /// (seen in the design-overhaul QA fleet access audit, FR-3 evidence).
    #[test]
    fn overview_values_map_local_sentinel_target_to_default_target_id() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("alice-key".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(state.grants[0].target_id, "local", "stored sentinel");

        let grants = grant_overview_values(&state, "intendant:qa-host");

        assert_eq!(grants[0]["target_id"], "intendant:qa-host");
    }

    fn active_browser_cert_state() -> LocalIamState {
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:browser-cert:ab123".to_string(),
            kind: "browser_certificate".to_string(),
            label: "Alice laptop browser".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: vec![json!({
                "kind": "browser_mtls_cert",
                "fingerprint": "ab123"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(IamGrant {
            id: "grant:browser-cert:ab123:inspect".to_string(),
            principal_id: "principal:browser-cert:ab123".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:local-user-client".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "test scoped browser certificate".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });
        state
    }

    #[test]
    fn active_browser_cert_binding_uses_local_role_permissions() {
        let state = active_browser_cert_state();
        let principal = principal_for_browser_mtls_cert(&state, "ab123", "https").unwrap();

        assert_eq!(principal.kind, "browser_certificate");
        assert_eq!(
            principal.grant_id.as_deref(),
            Some("grant:browser-cert:ab123:inspect")
        );
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn draft_browser_cert_binding_is_not_resolved() {
        let mut state = active_browser_cert_state();
        state.principals[0].status = "draft".to_string();

        assert!(principal_for_browser_mtls_cert(&state, "ab123", "https").is_none());
    }

    #[test]
    fn agent_session_grants_scope_supervised_mcp_sessions() {
        use crate::access::access_policy::PeerOperation;

        let actor = AccessPrincipal::root_dashboard_session("test", "test");
        let mut state = LocalIamState::default();
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("kid-1".to_string()),
                role_id: Some("role:session-reader".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(result.principal.kind, "agent_session");
        assert_eq!(result.principal.id, "principal:agent-session:kid-1");

        let principal = principal_for_agent_session(&state, "kid-1", "http").unwrap();
        assert_eq!(principal.role_id, "role:session-reader");
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                PeerOperation::SessionInspect
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                PeerOperation::DisplayInput
            )
            .allowed
        );

        // No binding for this session and no wildcard: the caller decides
        // the default (transport trust), not the state.
        assert!(principal_for_agent_session(&state, "other", "http").is_none());

        // A wildcard binding catches every remaining session.
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("*".to_string()),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let wildcard = principal_for_agent_session(&state, "other", "http").unwrap();
        assert_eq!(wildcard.id, "principal:agent-session:any");
        assert_eq!(wildcard.role_id, "role:operator");
        // The exact binding still wins for its own session.
        let exact = principal_for_agent_session(&state, "kid-1", "http").unwrap();
        assert_eq!(exact.id, "principal:agent-session:kid-1");
    }

    #[test]
    fn agent_session_upsert_requires_session_id() {
        let actor = AccessPrincipal::root_dashboard_session("test", "test");
        let mut state = LocalIamState::default();
        let err = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(err.to_string().contains("session_id"));
    }

    #[test]
    fn client_key_grants_can_be_updated() {
        let actor = AccessPrincipal::root_dashboard_session("test", "test");
        let mut state = LocalIamState::default();
        let created = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "client_key".to_string(),
                client_key_fingerprint: Some("fp-abc".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(created.principal.kind, "client_key");

        let updated = update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: created.grant.id.clone(),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(updated.grant.role_id, "role:operator");

        let revoked = update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: created.grant.id,
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(revoked.grant.status, "revoked");
        assert!(principal_for_client_key(&state, "fp-abc", "https").is_none());
    }

    #[test]
    fn agent_session_scoping_presence_is_sticky() {
        let actor = AccessPrincipal::root_dashboard_session("test", "test");
        let mut state = LocalIamState::default();
        assert!(!agent_session_scoping_present(&state));

        let created = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("*".to_string()),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert!(agent_session_scoping_present(&state));

        // Neither expiry nor deliberate revocation reopens anything: the
        // binding keeps counting as scoping intent. The explicit way back
        // is a local_process grant, never a lapsed timer.
        state
            .grants
            .iter_mut()
            .find(|grant| grant.id == created.grant.id)
            .unwrap()
            .expires_at_unix_ms = Some(1);
        assert!(agent_session_scoping_present(&state));

        update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: created.grant.id,
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert!(agent_session_scoping_present(&state));
    }

    #[test]
    fn lapsed_agent_session_grants_bind_and_deny_instead_of_defaulting() {
        use crate::access::access_policy::PeerOperation;

        let actor = AccessPrincipal::root_dashboard_session("test", "test");
        let mut state = LocalIamState::default();
        let created = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("kid-1".to_string()),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        // Expire the grant: the active resolver no longer matches, but the
        // any-status resolver still binds the scoped principal, and the
        // evaluator denies with the expiry as the reason.
        state
            .grants
            .iter_mut()
            .find(|grant| grant.id == created.grant.id)
            .unwrap()
            .expires_at_unix_ms = Some(1);
        assert!(principal_for_agent_session(&state, "kid-1", "http").is_none());
        let lapsed = principal_for_agent_session_any_status(&state, "kid-1", "http").unwrap();
        assert_eq!(lapsed.id, "principal:agent-session:kid-1");
        let decision =
            evaluate_principal_operation_with_state(&state, &lapsed, PeerOperation::StatsRead);
        assert!(!decision.allowed);
        assert!(decision.reason.contains("expired"), "{}", decision.reason);

        // A revoked wildcard binding catches sessions with no exact
        // binding, so they deny too instead of falling back to default
        // trust.
        let wildcard = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("*".to_string()),
                role_id: Some("role:operator".to_string()),
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(wildcard.grant.status, "revoked");
        assert!(principal_for_agent_session(&state, "other", "http").is_none());
        let lapsed_other = principal_for_agent_session_any_status(&state, "other", "http").unwrap();
        assert_eq!(lapsed_other.id, "principal:agent-session:any");
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &lapsed_other,
                PeerOperation::StatsRead
            )
            .allowed
        );
    }

    #[test]
    fn lapsed_local_process_grant_binds_and_denies() {
        use crate::access::access_policy::PeerOperation;

        let actor = AccessPrincipal::root_dashboard_session("test", "test");
        let mut state = LocalIamState::default();
        assert!(principal_for_loopback_mcp_any_status(&state, "http").is_none());

        let created = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "local_process".to_string(),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: created.grant.id,
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert!(principal_for_loopback_mcp(&state, "http").is_none());
        let lapsed = principal_for_loopback_mcp_any_status(&state, "http").unwrap();
        assert_eq!(lapsed.id, "principal:local-process:loopback");
        assert!(
            !evaluate_principal_operation_with_state(&state, &lapsed, PeerOperation::StatsRead)
                .allowed
        );
    }

    #[test]
    fn local_process_grant_scopes_loopback_mcp() {
        use crate::access::access_policy::PeerOperation;

        let actor = AccessPrincipal::root_dashboard_session("test", "test");
        let mut state = LocalIamState::default();
        assert!(principal_for_loopback_mcp(&state, "http").is_none());

        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "local_process".to_string(),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        let principal = principal_for_loopback_mcp(&state, "http").unwrap();
        assert_eq!(principal.id, "principal:local-process:loopback");
        assert_eq!(principal.kind, "local_process");
        assert!(
            evaluate_principal_operation_with_state(&state, &principal, PeerOperation::DisplayView)
                .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                PeerOperation::TerminalWrite
            )
            .allowed
        );
    }

    #[test]
    fn mcp_default_principals_are_root_compatible_with_real_identity() {
        use crate::access::access_policy::PeerOperation;

        let agent = AccessPrincipal::supervised_agent_session_default("kid-1", "http", true);
        assert_eq!(agent.kind, "root_session");
        assert_eq!(agent.id, "principal:agent-session:kid-1");
        assert_eq!(agent.source, "mcp-session-token");
        assert!(evaluate_principal_operation(&agent, PeerOperation::DisplayInput).allowed);

        let shared = AccessPrincipal::supervised_agent_session_default("kid-1", "http", false);
        assert_eq!(shared.source, "mcp-shared-token");

        let holder = AccessPrincipal::mcp_token_holder("http");
        assert_eq!(holder.id, "principal:mcp-token-holder");
        assert!(evaluate_principal_operation(&holder, PeerOperation::AccessManage).allowed);

        let local = AccessPrincipal::local_loopback_mcp_default("http");
        assert_eq!(local.id, "principal:local-process:loopback");
        assert_eq!(local.source, "mcp-loopback-cleartext");
        assert!(evaluate_principal_operation(&local, PeerOperation::AccessManage).allowed);
    }

    #[test]
    fn owner_surface_excludes_the_root_compatible_transport_defaults() {
        // Owner surfaces: the trusted dashboard and the documented
        // local-shell loopback principal.
        assert!(AccessPrincipal::root_dashboard_session("test", "dashboard").is_owner_surface());
        assert!(AccessPrincipal::local_loopback_mcp_default("http").is_owner_surface());

        // Root-COMPATIBLE is not owner: supervised external agents and MCP
        // token holders share kind "root_session" but are exactly who the
        // user-display grant must hold; peers are gated by their profile.
        assert!(
            !AccessPrincipal::supervised_agent_session_default("kid-1", "http", true)
                .is_owner_surface()
        );
        assert!(
            !AccessPrincipal::supervised_agent_session_default("kid-1", "http", false)
                .is_owner_surface()
        );
        assert!(!AccessPrincipal::mcp_token_holder("http").is_owner_surface());
        assert!(
            !AccessPrincipal::peer_daemon("fp", "dell", "peer-root", "mtls").is_owner_surface()
        );
    }

    #[test]
    fn enrolled_root_mtls_user_client_is_a_distinct_owner_anchor() {
        let mut state = active_browser_cert_state();
        state.grants[0].role_id = "role:root".to_string();
        let principal = principal_for_browser_mtls_cert(&state, "ab123", "https").unwrap();

        assert!(principal.is_enrolled_root_mtls_user_client());
        assert!(
            !principal.is_owner_surface(),
            "the global owner-surface predicate must keep excluding IAM clients"
        );

        let mut scoped = principal.clone();
        scoped.role_id = "role:operator".to_string();
        assert!(!scoped.is_enrolled_root_mtls_user_client());

        let mut ambient_key = principal.clone();
        ambient_key.authn_kind = Some("client_key".to_string());
        assert!(!ambient_key.is_enrolled_root_mtls_user_client());

        let mut wrong_principal_kind = principal.clone();
        wrong_principal_kind.kind = "root_session".to_string();
        assert!(!wrong_principal_kind.is_enrolled_root_mtls_user_client());

        let mut unenrolled = principal.clone();
        unenrolled.grant_id = None;
        assert!(!unenrolled.is_enrolled_root_mtls_user_client());

        let mut hosted = principal;
        hosted.hosted_connect = true;
        assert!(!hosted.is_enrolled_root_mtls_user_client());
    }

    #[test]
    fn upsert_browser_cert_grant_creates_active_binding() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                label: Some("Alice browser".to_string()),
                fingerprint: Some("AB:12:3".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert!(result.created_principal);
        assert!(result.created_grant);
        assert_eq!(result.principal.kind, "browser_certificate");
        assert_eq!(result.grant.status, "active");
        assert_eq!(state.audit_events.len(), 1);

        let principal = principal_for_browser_mtls_cert(&state, "ab123", "https").unwrap();
        assert_eq!(principal.label, "Alice browser");
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn expired_grants_stop_enforcing_and_report_expired() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let now = now_unix_ms();
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("temp-key".to_string()),
                role_id: Some("role:terminal".to_string()),
                expires_at_unix_ms: Some(now + 60_000),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        // Live before expiry.
        let principal = principal_for_client_key(&state, "temp-key", "test").unwrap();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::TerminalWrite,
            )
            .allowed
        );

        // Force the grant into the past: enforcement denies with an expiry
        // reason, resolution stops matching, and the overview reports
        // `expired` without touching the stored status.
        state.grants[0].expires_at_unix_ms = Some(now.saturating_sub(1));
        let decision = evaluate_principal_operation_with_state(
            &state,
            &principal,
            crate::access::access_policy::PeerOperation::TerminalWrite,
        );
        assert!(!decision.allowed);
        assert!(decision.reason.contains("expired"), "{}", decision.reason);
        assert!(principal_for_client_key(&state, "temp-key", "test").is_none());
        let overview = grant_overview_values(&state, "local");
        assert_eq!(overview[0]["status"], "expired");
        assert_eq!(overview[0]["enforced"], false);
        assert_eq!(state.grants[0].status, "active");

        // Past expiries are rejected at write time.
        let err = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("temp-key-2".to_string()),
                expires_at_unix_ms: Some(1),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(err.to_string().contains("future"));
    }

    #[test]
    fn connect_account_records_never_authenticate_even_if_granted() {
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:connect-account:user-123".to_string(),
            kind: "connect_account".to_string(),
            label: "@alice".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: Some(json!({"user_id":"user-123","account_name":"alice"})),
            organization: None,
            authn: vec![json!({
                "kind":"connect_account",
                "user_id":"user-123",
                "account_name":"alice"
            })],
            notes: None,
            created_at_unix_ms: Some(1),
        });
        state.grants.push(IamGrant {
            id: "grant:legacy-connect-root".to_string(),
            principal_id: "principal:connect-account:user-123".to_string(),
            target_id: "local".to_string(),
            role_id: "role:root".to_string(),
            policy_id: "policy:root".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "legacy record".to_string(),
            created_at_unix_ms: Some(1),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });

        let principal = principal_for_authn(
            &state,
            "connect_account",
            "user_id",
            "user-123",
            "connect".to_string(),
        )
        .unwrap();
        assert_eq!(principal.authn_kind.as_deref(), Some("connect_account"));
        // Neither the stored root grant nor a malicious legacy-state edit can
        // turn account metadata into authentication.
        state
            .role_ceilings
            .insert("connect_account".to_string(), "role:root".to_string());
        for op in crate::access::access_policy::ALL_OPERATIONS {
            let denied = evaluate_principal_operation_with_state(&state, &principal, op);
            assert!(!denied.allowed, "Connect account must deny {op:?}");
            assert!(denied.reason.contains("never authenticate"));
        }

        let principal_overview = principal_overview_values(&state);
        assert_eq!(principal_overview[0]["status"], "metadata_only");
        assert_eq!(principal_overview[0]["metadata_only"], true);
        assert_eq!(principal_overview[0]["authority"], "none");
        let grant_overview = grant_overview_values(&state, "local");
        assert_eq!(grant_overview[0]["kind"], "connect_account_metadata");
        assert_eq!(grant_overview[0]["status"], "metadata_only");
        assert_eq!(grant_overview[0]["enforced"], false);

        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let error = update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: "grant:legacy-connect-root".to_string(),
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(error.to_string().contains("metadata-only and read-only"));
    }

    #[test]
    fn hosted_sessions_are_immutably_denied_while_direct_keys_remain_authorized() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("anchor-key".to_string()),
                client_key_origin: Some("https://anchor.local:8765".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        insert_legacy_active_client_key_grant(
            &mut state,
            "hosted-key",
            "https://connect.intendant.dev",
            "role:root",
            None,
        );

        // Keys born on daemon-served origins keep their explicitly granted
        // direct authority.
        let anchor = principal_for_client_key(&state, "anchor-key", "connect").unwrap();
        assert_eq!(
            anchor.authn_origin.as_deref(),
            Some("https://anchor.local:8765")
        );
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &anchor,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );

        // Keys enrolled from a hosted origin have no authority in the
        // default build, regardless of their stored grant.
        let hosted = principal_for_client_key(&state, "hosted-key", "connect").unwrap();
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::access::access_policy::PeerOperation::ShellSpawn,
            )
            .allowed
        );

        // Current route outranks enrollment origin: this direct-born key is
        // denied when the daemon's Connect lane stamps it as hosted.
        let mut relayed_anchor = anchor.clone();
        relayed_anchor.hosted_connect = true;
        state.role_ceilings.clear();
        for op in crate::access::access_policy::ALL_OPERATIONS {
            assert!(
                !evaluate_principal_operation_with_state(&state, &relayed_anchor, op).allowed,
                "direct-born key relayed through Connect must deny {op:?}"
            );
        }
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );

        // Persisted role ceilings are not policy inputs. Even a malicious
        // root value cannot restore hosted control.
        for binding in HOSTED_CEILING_BINDINGS {
            state
                .role_ceilings
                .insert(binding.to_string(), "role:root".to_string());
        }
        for op in crate::access::access_policy::ALL_OPERATIONS {
            let denied = evaluate_principal_operation_with_state(&state, &hosted, op);
            assert!(!denied.allowed, "hosted key must deny {op:?}");
            assert!(denied.reason.contains("discovery-only"));
        }
    }

    #[test]
    fn legacy_fleet_key_sessions_are_immutably_denied_by_the_central_evaluator() {
        let mut state = LocalIamState::default();
        insert_legacy_active_client_key_grant(
            &mut state,
            "legacy-fleet-session-key",
            "https://d-legacy.fleet.intendant.dev:8765",
            "role:root",
            Some("fleet.intendant.dev"),
        );
        let fleet = principal_for_client_key(&state, "legacy-fleet-session-key", "direct").unwrap();

        for op in crate::access::access_policy::ALL_OPERATIONS {
            let denied = evaluate_principal_operation_with_state_and_fleet_zone(
                &state,
                &fleet,
                op,
                Some("fleet.intendant.dev"),
            );
            assert!(!denied.allowed, "fleet-origin key must deny {op:?}");
            assert!(denied.reason.contains("fleet-origin browser keys"));
            assert!(denied.reason.contains("discovery-only"));
        }
    }

    #[test]
    fn hosted_none_ceiling_ignores_tampered_persisted_roles() {
        let mut state = LocalIamState::default();
        insert_legacy_active_client_key_grant(
            &mut state,
            "hosted-key",
            "https://connect.intendant.dev",
            "role:root",
            None,
        );
        state
            .role_ceilings
            .insert("client_key".to_string(), "role:root".to_string());

        // Simulate an iam.json edit which also changes `source`, preventing
        // normalize() from replacing this record with the builtin template.
        let persisted_operator = state
            .roles
            .iter_mut()
            .find(|role| role.id == "role:operator")
            .expect("persisted operator role");
        persisted_operator.source = "tampered-local-copy".to_string();
        persisted_operator.permissions = root_permission_ids();

        let hosted = principal_for_client_key(&state, "hosted-key", "connect").unwrap();
        for op in crate::access::access_policy::ALL_OPERATIONS {
            let denied = evaluate_principal_operation_with_state(&state, &hosted, op);
            assert!(!denied.allowed, "tampered state must not allow {op:?}");
            assert!(denied.reason.contains("discovery-only"));
        }
    }

    #[test]
    fn origin_route_class_distinguishes_the_first_contact_rungs() {
        let hosted = default_hosted_origins();
        let zone = Some("fleet.intendant.dev");
        assert_eq!(
            origin_route_class("https://connect.intendant.dev", &hosted, zone),
            "hosted"
        );
        assert_eq!(
            origin_route_class("https://connect.intendant.dev/", &hosted, zone),
            "hosted"
        );
        assert_eq!(
            origin_route_class("https://CONNECT.INTENDANT.DEV:443/some/path", &hosted, zone),
            "hosted"
        );
        assert_eq!(
            origin_route_class("https://connect.intendant.dev.", &hosted, zone),
            "hosted"
        );
        assert_eq!(
            origin_route_class("http://connect.intendant.dev", &hosted, zone),
            "direct"
        );
        assert_eq!(
            origin_route_class("https://connect.intendant.dev", &[], zone),
            "hosted",
            "the compiled default must survive an empty/tampered state list"
        );
        assert_eq!(
            origin_route_class(
                "https://d-30a08371a38c1b.fleet.intendant.dev:8765",
                &hosted,
                zone
            ),
            "fleet"
        );
        // The zone apex itself is fleet-classed; a lookalike suffix is not.
        assert_eq!(
            origin_route_class("https://fleet.intendant.dev", &hosted, zone),
            "fleet"
        );
        assert_eq!(
            origin_route_class("https://evil-fleet.intendant.dev", &hosted, zone),
            "direct"
        );
        assert_eq!(
            origin_route_class("https://192.168.1.50:8765", &hosted, zone),
            "direct"
        );
        // No fleet zone configured: fleet-looking names are just direct.
        assert_eq!(
            origin_route_class("https://d-x.fleet.intendant.dev", &hosted, None),
            "direct"
        );
        assert_eq!(origin_route_class("", &hosted, zone), "unknown");
        assert_eq!(origin_route_class("   ", &hosted, zone), "unknown");
    }

    #[test]
    fn incomplete_fleet_provenance_refuses_unknown_dns_but_not_ip_origins() {
        let hosted = default_hosted_origins();
        assert_eq!(
            origin_route_class_with_provenance_state(
                "https://possibly-former-fleet.example.test:8765",
                &hosted,
                None,
                true,
            ),
            "fleet"
        );
        assert_eq!(
            origin_route_class_with_provenance_state(
                "https://192.168.1.50:8765",
                &hosted,
                None,
                true,
            ),
            "direct",
            "rendezvous fleet names are DNS names, never IP literals"
        );
        assert_eq!(
            origin_route_class_with_provenance_state(
                "https://connect.intendant.dev",
                &hosted,
                None,
                true,
            ),
            "hosted",
            "the more specific hosted provenance must remain visible"
        );
        assert_eq!(
            origin_route_class_with_provenance_state("", &hosted, None, true),
            "unknown"
        );
    }

    #[test]
    fn remembered_exact_fleet_name_stays_inactive_without_current_zone() {
        let fleet_name = "remembered-fleet-origin.unit.invalid";
        let fleet_origin = format!("https://{fleet_name}:8765");
        crate::web_tls::register_fleet_server_name(fleet_name);
        assert_eq!(
            origin_route_class(&fleet_origin, &default_hosted_origins(), None),
            "fleet",
            "an exact rendezvous-assigned name must survive a missing current zone"
        );

        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let mut active_state = LocalIamState::default();
        let before = active_state.clone();
        let error = upsert_user_client_grant_with_fleet_zone(
            &mut active_state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("remembered-fleet-key".to_string()),
                client_key_origin: Some(fleet_origin.clone()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("fleet origins"));
        assert_eq!(active_state, before);

        let mut legacy_state = LocalIamState::default();
        let legacy = insert_legacy_active_client_key_grant(
            &mut legacy_state,
            "legacy-remembered-fleet-key",
            &fleet_origin,
            "role:operator",
            None,
        );
        let principals = principal_overview_values_with_fleet_zone(&legacy_state, None);
        let principal = principals
            .iter()
            .find(|principal| principal["id"] == legacy.principal.id)
            .unwrap();
        assert_eq!(principal["status"], "inactive_binding");
        assert_eq!(principal["origin_class"], "fleet");
        assert_eq!(principal["authority"], "none");
        let grants = grant_overview_values_with_fleet_zone(&legacy_state, "local", None);
        let grant = grants
            .iter()
            .find(|grant| grant["id"] == legacy.grant.id)
            .unwrap();
        assert_eq!(grant["status"], "inactive_binding");
        assert_eq!(grant["enforced"], false);
    }

    #[test]
    fn active_service_controlled_pure_client_key_grants_are_refused() {
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");

        let mut hosted_state = LocalIamState::default();
        let before = hosted_state.clone();
        let error = upsert_user_client_grant(
            &mut hosted_state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("hosted-key".to_string()),
                client_key_origin: Some("https://connect.intendant.dev".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(error.to_string().contains("hosted origins"));
        assert_eq!(hosted_state, before, "a refused upsert must not mutate IAM");

        let mut fleet_state = LocalIamState::default();
        let before = fleet_state.clone();
        let error = upsert_user_client_grant_with_fleet_zone(
            &mut fleet_state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("fleet-key".to_string()),
                client_key_origin: Some("https://d-123.fleet.intendant.dev:8765".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
            Some("fleet.intendant.dev"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("fleet origins"));
        assert_eq!(fleet_state, before, "a refused upsert must not mutate IAM");

        // Renaming a pure key record to human_user cannot bypass the guard.
        let mut keyed_human_state = LocalIamState::default();
        let error = upsert_user_client_grant(
            &mut keyed_human_state,
            UserClientGrantUpsertRequest {
                kind: "human_user".to_string(),
                handle: Some("alice".to_string()),
                client_key_fingerprint: Some("hosted-human-key".to_string()),
                client_key_origin: Some("https://connect.intendant.dev".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(error.to_string().contains("active pure client_key"));

        // A staged record may be retained, but the lifecycle API cannot
        // turn it active later.
        let draft = upsert_user_client_grant(
            &mut hosted_state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("hosted-draft".to_string()),
                client_key_origin: Some("https://connect.intendant.dev".to_string()),
                status: Some("draft".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let before = hosted_state.clone();
        let error = update_user_client_grant(
            &mut hosted_state,
            IamGrantUpdateRequest {
                grant_id: draft.grant.id,
                status: Some("active".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(error.to_string().contains("hosted origins"));
        assert_eq!(
            hosted_state, before,
            "a refused activation must not mutate IAM"
        );
    }

    #[test]
    fn mtls_human_may_keep_service_controlled_key_metadata() {
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let mut state = LocalIamState::default();
        let result = upsert_user_client_grant_with_fleet_zone(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "human_user".to_string(),
                label: Some("Alice".to_string()),
                handle: Some("alice".to_string()),
                fingerprint: Some("AB:CD".to_string()),
                client_key_fingerprint: Some("fleet-metadata-key".to_string()),
                client_key_origin: Some("https://d-123.fleet.intendant.dev:8765".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
            Some("fleet.intendant.dev"),
        )
        .unwrap();
        assert!(result
            .principal
            .authn
            .iter()
            .any(|authn| authn["kind"] == "browser_mtls_cert"));
        assert!(result
            .principal
            .authn
            .iter()
            .any(|authn| authn["kind"] == "client_key"));

        let principals =
            principal_overview_values_with_fleet_zone(&state, Some("fleet.intendant.dev"));
        assert_eq!(principals[0]["status"], "active");
        assert_eq!(principals[0]["inactive_binding"], false);
        assert_eq!(principals[0]["authority"], "local_iam");
        let grants =
            grant_overview_values_with_fleet_zone(&state, "local", Some("fleet.intendant.dev"));
        assert_eq!(grants[0]["status"], "active");
        assert_eq!(grants[0]["enforced"], true);
        assert_eq!(grants[0]["authority"], "local_iam");

        // The binding is active because mTLS can authenticate it. The same
        // principal reached through its fleet-origin browser key remains
        // inert: merely carrying both bindings must not let the ambient key
        // borrow the certificate's authority.
        let via_client_key =
            principal_for_client_key(&state, "fleet-metadata-key", "direct").unwrap();
        let key_decision = evaluate_principal_operation_with_state_and_fleet_zone(
            &state,
            &via_client_key,
            crate::access::access_policy::PeerOperation::AccessInspect,
            Some("fleet.intendant.dev"),
        );
        assert!(!key_decision.allowed);
        assert!(key_decision.reason.contains("fleet-origin browser keys"));

        let via_mtls = principal_for_browser_mtls_cert(&state, "abcd", "direct").unwrap();
        assert_eq!(via_mtls.authn_kind.as_deref(), Some("browser_mtls_cert"));
        assert!(
            evaluate_principal_operation_with_state_and_fleet_zone(
                &state,
                &via_mtls,
                crate::access::access_policy::PeerOperation::AccessInspect,
                Some("fleet.intendant.dev"),
            )
            .allowed,
            "the independently verified mTLS session keeps the composite human's grant"
        );
    }

    #[test]
    fn legacy_service_controlled_key_grants_project_as_inactive() {
        let mut state = LocalIamState::default();
        let hosted = insert_legacy_active_client_key_grant(
            &mut state,
            "legacy-hosted",
            "https://connect.intendant.dev",
            "role:root",
            Some("fleet.intendant.dev"),
        );
        let fleet = insert_legacy_active_client_key_grant(
            &mut state,
            "legacy-fleet",
            "https://d-legacy.fleet.intendant.dev:8765",
            "role:operator",
            Some("fleet.intendant.dev"),
        );

        // A legacy/tampered record may contain several key entries. A direct
        // key listed first must not hide a later hosted key from either the
        // mutation guard or the overview projection.
        state
            .principals
            .iter_mut()
            .find(|principal| principal.id == hosted.principal.id)
            .unwrap()
            .authn
            .insert(
                0,
                client_key_authn_entry(
                    "legacy-direct-first",
                    None,
                    Some("https://anchor.local:8765"),
                ),
            );
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let before = state.clone();
        let error = update_user_client_grant_with_fleet_zone(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: hosted.grant.id.clone(),
                reason: Some("attempt to preserve active legacy record".to_string()),
                ..Default::default()
            },
            &actor,
            Some("fleet.intendant.dev"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("hosted origins"));
        assert_eq!(state, before);

        let principals =
            principal_overview_values_with_fleet_zone(&state, Some("fleet.intendant.dev"));
        for (principal_id, origin_class) in [
            (hosted.principal.id.as_str(), "hosted"),
            (fleet.principal.id.as_str(), "fleet"),
        ] {
            let principal = principals
                .iter()
                .find(|principal| principal["id"] == principal_id)
                .unwrap();
            assert_eq!(principal["status"], "inactive_binding");
            assert_eq!(principal["stored_status"], "active");
            assert_eq!(principal["inactive_binding"], true);
            assert_eq!(principal["origin_class"], origin_class);
            assert_eq!(principal["authority"], "none");
        }

        let grants =
            grant_overview_values_with_fleet_zone(&state, "local", Some("fleet.intendant.dev"));
        for (grant_id, origin_class) in [
            (hosted.grant.id.as_str(), "hosted"),
            (fleet.grant.id.as_str(), "fleet"),
        ] {
            let grant = grants.iter().find(|grant| grant["id"] == grant_id).unwrap();
            assert_eq!(grant["status"], "inactive_binding");
            assert_eq!(grant["stored_status"], "active");
            assert_eq!(grant["enforced"], false);
            assert_eq!(grant["inactive_binding"], true);
            assert_eq!(grant["origin_class"], origin_class);
            assert_eq!(grant["authority"], "none");
            assert!(grant["role_label"]
                .as_str()
                .unwrap()
                .contains("inactive binding"));
        }
    }

    #[test]
    fn set_daemon_tier_validates_normalizes_and_audits() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        assert_eq!(state.tier, None);

        let stored = set_daemon_tier(&mut state, Some("  Integrated "), &actor).unwrap();
        assert_eq!(stored.as_deref(), Some("integrated"));
        assert_eq!(state.tier.as_deref(), Some("integrated"));
        assert_eq!(state.audit_events.len(), 1);
        assert_eq!(state.audit_events[0].action, "set_daemon_tier");

        // Idempotent set: no state change, no audit noise.
        set_daemon_tier(&mut state, Some("integrated"), &actor).unwrap();
        assert_eq!(state.audit_events.len(), 1);

        let err = set_daemon_tier(&mut state, Some("fortress"), &actor).unwrap_err();
        assert!(err.to_string().contains("unknown tier"));
        assert_eq!(state.tier.as_deref(), Some("integrated"));

        let cleared = set_daemon_tier(&mut state, None, &actor).unwrap();
        assert_eq!(cleared, None);
        assert_eq!(state.tier, None);
        assert_eq!(state.audit_events.len(), 2);
    }

    #[test]
    fn hosted_control_is_immutable_none_and_direct_sessions_are_untouched() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        insert_legacy_active_client_key_grant(
            &mut state,
            "hosted-key",
            "https://connect.intendant.dev",
            "role:root",
            None,
        );
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("anchor-key".to_string()),
                client_key_origin: Some("https://anchor.local:8765".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        // A malicious persisted value is normalized back to the compiled
        // discovery-only policy.
        for binding in HOSTED_CEILING_BINDINGS {
            state
                .role_ceilings
                .insert(binding.to_string(), "role:root".to_string());
        }
        state = state.normalize();
        for binding in HOSTED_CEILING_BINDINGS {
            assert_eq!(
                state.role_ceilings.get(binding).map(String::as_str),
                Some("role:none")
            );
        }
        // A hosted-origin key with a root grant can do nothing.
        let hosted = principal_for_client_key(&state, "hosted-key", "connect").unwrap();
        for op in [
            crate::access::access_policy::PeerOperation::ShellSpawn,
            crate::access::access_policy::PeerOperation::DisplayView,
            crate::access::access_policy::PeerOperation::SessionInspect,
        ] {
            let denied = evaluate_principal_operation_with_state(&state, &hosted, op);
            assert!(!denied.allowed, "expected {op:?} denied under role:none");
            assert!(denied.reason.contains("discovery-only"));
        }

        // Anchor-origin keys are untouched by the hosted ceiling.
        let anchor = principal_for_client_key(&state, "anchor-key", "connect").unwrap();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &anchor,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );

        // There is no mutation helper or API. Even a post-load in-memory
        // edit is not consulted by the central evaluator.
        state
            .role_ceilings
            .insert("client_key".to_string(), "role:root".to_string());
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::access::access_policy::PeerOperation::ShellSpawn,
            )
            .allowed
        );
    }

    #[test]
    fn role_none_cannot_be_granted_to_a_user_client() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let err = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("some-key".to_string()),
                role_id: Some("role:none".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ceiling-only"));
    }

    #[test]
    fn generic_iam_mutations_cannot_assign_or_reactivate_hosted_roles() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let before = state.clone();
        let error = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:BB".to_string()),
                role_id: Some(super::super::hosted_control::HOSTED_ROLE_OPERATE.to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(error.to_string().contains("reserved"));
        assert_eq!(
            state, before,
            "root must not bypass the reserved-role writer"
        );

        let ordinary = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:CC".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let before = state.clone();
        let error = update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: ordinary.grant.id,
                role_id: Some(super::super::hosted_control::HOSTED_ROLE_TASKS.to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(error.to_string().contains("reserved"));
        assert_eq!(state, before);

        state.grants.push(IamGrant {
            id: "grant:hosted-test".to_string(),
            principal_id: "principal:hosted-test".to_string(),
            target_id: "daemon:self".to_string(),
            role_id: super::super::hosted_control::HOSTED_ROLE_VIEW.to_string(),
            policy_id: "policy:hosted-control-compiled".to_string(),
            status: "revoked".to_string(),
            source: super::super::hosted_control::HOSTED_SOURCE.to_string(),
            reason: "test fixture".to_string(),
            created_at_unix_ms: Some(now_unix_ms()),
            revoked_at_unix_ms: Some(now_unix_ms()),
            expires_at_unix_ms: Some(now_unix_ms() + 60_000),
            issued_via: None,
            fs_scope: None,
        });
        state.principals.push(IamPrincipal {
            id: "principal:hosted-test".to_string(),
            kind: super::super::hosted_control::HOSTED_PRINCIPAL_KIND.to_string(),
            label: "Hosted test".to_string(),
            status: "revoked".to_string(),
            source: super::super::hosted_control::HOSTED_SOURCE.to_string(),
            account: None,
            organization: None,
            authn: Vec::new(),
            notes: None,
            created_at_unix_ms: Some(now_unix_ms()),
        });
        let error = update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: "grant:hosted-test".to_string(),
                status: Some("active".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();
        assert!(error.to_string().contains("only local IAM"));
    }

    #[test]
    fn upsert_client_key_grant_creates_active_binding() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                // Kind is inferred from the client-key fingerprint.
                client_key_fingerprint: Some("Fp_Base64-Url".to_string()),
                client_key: Some("BPubKeyRaw".to_string()),
                client_key_origin: Some("https://anchor.local:8765".to_string()),
                role_id: Some("role:terminal".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert!(result.created_principal);
        assert_eq!(result.principal.kind, "client_key");
        // base64url fingerprints keep their case, unlike hex cert prints.
        let authn = &result.principal.authn[0];
        assert_eq!(authn["kind"], "client_key");
        assert_eq!(authn["fingerprint"], "Fp_Base64-Url");
        assert_eq!(authn["origin"], "https://anchor.local:8765");
        assert_eq!(authn["public_key"], "BPubKeyRaw");

        let principal =
            principal_for_client_key(&state, "Fp_Base64-Url", "connect-dashboard-control").unwrap();
        assert_eq!(principal.id, result.principal.id);
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::TerminalWrite,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::ShellSpawn,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::Settings,
            )
            .allowed
        );
        // Case differences must not match.
        assert!(principal_for_client_key(&state, "fp_base64-url", "x").is_none());
    }

    #[test]
    fn new_connect_account_grants_are_rejected_without_mutating_state() {
        let mut state = LocalIamState::default();
        let before = state.clone();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let error = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "connect_account".to_string(),
                user_id: Some("user-123".to_string()),
                account_name: Some("alice".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();

        assert!(error.to_string().contains("legacy discovery metadata"));
        assert_eq!(state, before);
    }

    #[test]
    fn upsert_human_user_grant_binds_browser_cert_and_metadata() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "human_user".to_string(),
                label: Some("Alice".to_string()),
                fingerprint: Some("F0:0D".to_string()),
                handle: Some("alice".to_string()),
                account_provider: Some("github".to_string()),
                verified_provider: Some("github".to_string()),
                organization_id: Some("org-1".to_string()),
                organization_name: Some("Acme".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert_eq!(result.principal.kind, "human_user");
        assert_eq!(result.principal.label, "Alice");
        assert_eq!(
            result
                .principal
                .account
                .as_ref()
                .and_then(|account| account.get("provider"))
                .and_then(Value::as_str),
            Some("github")
        );
        assert_eq!(
            result
                .principal
                .organization
                .as_ref()
                .and_then(|org| org.get("name"))
                .and_then(Value::as_str),
            Some("Acme")
        );

        let principal = principal_for_browser_mtls_cert(&state, "f00d", "https").unwrap();
        assert_eq!(principal.kind, "human_user");
        assert_eq!(principal.role_id, "role:observer");
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::SessionInspect,
            )
            .allowed
        );
    }

    #[test]
    fn human_user_account_metadata_without_a_key_or_certificate_is_rejected() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let error = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "human_user".to_string(),
                account_name: Some("alice".to_string()),
                user_id: Some("user-123".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("Connect account fields are metadata only"));
        assert!(state.principals.is_empty());
        assert!(state.grants.is_empty());
    }

    #[test]
    fn scoped_human_roles_are_enforced_by_permission_id() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                label: Some("Terminal browser".to_string()),
                fingerprint: Some("CA:FE".to_string()),
                role_id: Some("role:terminal".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert_eq!(result.grant.role_id, "role:terminal");
        let principal = principal_for_browser_mtls_cert(&state, "cafe", "https").unwrap();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::TerminalView,
            )
            .allowed
        );
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::TerminalWrite,
            )
            .allowed
        );
        // The split took spawn away from role:terminal: collaborators can
        // see and type into shared shells but cannot create new ones.
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::ShellSpawn,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::FilesystemWrite,
            )
            .allowed
        );
    }

    /// Platform-absolute fixture path: `/srv/data` is not absolute on
    /// Windows, so prefix a drive and flip separators there.
    fn abs_root(p: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", p.replace('/', "\\"))
        } else {
            p.to_string()
        }
    }

    #[test]
    fn fs_scope_is_stored_normalized_and_resolvable() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:BB".to_string()),
                role_id: Some("role:files-read".to_string()),
                fs_read_roots: vec![
                    abs_root("/srv/data"),
                    format!("  {}  ", abs_root("/srv/data")),
                    String::new(),
                ],
                fs_write_roots: vec![abs_root("/srv/data/inbox")],
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let scope = result.grant.fs_scope.as_ref().expect("scope stored");
        assert_eq!(
            scope.read_roots,
            vec![std::path::PathBuf::from(abs_root("/srv/data"))]
        );
        assert_eq!(
            scope.write_roots,
            vec![std::path::PathBuf::from(abs_root("/srv/data/inbox"))]
        );
        let principal = AccessPrincipal {
            grant_id: Some(result.grant.id.clone()),
            ..AccessPrincipal::root_dashboard_session("x", "dashboard-control")
        };
        assert!(fs_scope_for_principal(&state, &principal).is_some());
        let unbound = AccessPrincipal {
            grant_id: None,
            ..AccessPrincipal::root_dashboard_session("x", "dashboard-control")
        };
        assert!(fs_scope_for_principal(&state, &unbound).is_none());
    }

    #[test]
    fn fs_scope_rejects_relative_roots_and_clears_on_reupsert() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let err = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("CC:DD".to_string()),
                fs_read_roots: vec!["relative/path".to_string()],
                ..Default::default()
            },
            &actor,
        );
        assert!(err.is_err(), "relative roots must be rejected");

        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("CC:DD".to_string()),
                fs_read_roots: vec![abs_root("/tmp/scoped")],
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        // Re-upsert without roots clears the scope (the form always sends
        // the full desired state).
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("CC:DD".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert!(result.grant.fs_scope.is_none());
    }

    #[test]
    fn upsert_same_user_client_target_replaces_role_grant() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("FE:ED".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("FE:ED".to_string()),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert!(!result.created_grant);
        assert_eq!(state.grants.len(), 1);
        assert_eq!(state.grants[0].role_id, "role:operator");
        assert_eq!(state.grants[0].policy_id, "policy:operator");
    }

    #[test]
    fn update_user_client_grant_revokes_binding() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("DE:AD".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let updated = update_user_client_grant(
            &mut state,
            IamGrantUpdateRequest {
                grant_id: result.grant.id,
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert_eq!(updated.grant.status, "revoked");
        assert!(updated.grant.revoked_at_unix_ms.is_some());
        assert!(principal_for_browser_mtls_cert(&state, "dead", "https").is_none());
    }

    #[test]
    fn user_client_grants_reject_peer_profile_role() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let err = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("12:34".to_string()),
                role_id: Some("role:peer-profile".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap_err();

        assert!(err.to_string().contains("daemon-to-daemon role"));
    }

    #[test]
    fn root_principal_allows_every_current_operation() {
        let principal = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        for op in crate::access::access_policy::ALL_OPERATIONS {
            assert!(
                evaluate_principal_operation(&principal, op).allowed,
                "{op:?} should be allowed for root principal"
            );
        }
    }

    #[test]
    fn peer_principal_uses_peer_profile_permissions() {
        let principal =
            AccessPrincipal::peer_daemon("abc123", "peer", "peer-operator", "dashboard-control");

        assert!(
            evaluate_principal_operation(
                &principal,
                crate::access::access_policy::PeerOperation::DisplayView,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation(
                &principal,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn concurrent_local_revoke_and_grant_update_do_not_lose_either_decision() {
        use std::sync::mpsc;

        let directory = tempfile::tempdir().unwrap();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let mut initial = LocalIamState::default();
        let first = upsert_user_client_grant(
            &mut initial,
            UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("aa".repeat(32)),
                role_id: Some("role:observer".to_string()),
                reason: Some("first local decision".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        save_state(directory.path(), &initial).unwrap();

        let (revoke_has_lock_tx, revoke_has_lock_rx) = mpsc::channel();
        let (release_revoke_tx, release_revoke_rx) = mpsc::channel();
        let revoke_dir = directory.path().to_path_buf();
        let revoke_grant_id = first.grant.id.clone();
        let revoke_thread = std::thread::spawn(move || {
            let actor = AccessPrincipal::root_dashboard_session("revoke-test", "dashboard-control");
            transact_state(&revoke_dir, |state, _transaction| {
                revoke_has_lock_tx.send(()).unwrap();
                release_revoke_rx.recv().unwrap();
                update_user_client_grant(
                    state,
                    IamGrantUpdateRequest {
                        grant_id: revoke_grant_id,
                        status: Some("revoked".to_string()),
                        reason: Some("explicit local revocation".to_string()),
                        ..Default::default()
                    },
                    &actor,
                )?;
                Ok(((), true))
            })
        });
        revoke_has_lock_rx.recv().unwrap();

        let (grant_started_tx, grant_started_rx) = mpsc::channel();
        let grant_dir = directory.path().to_path_buf();
        let grant_thread = std::thread::spawn(move || {
            grant_started_tx.send(()).unwrap();
            let actor = AccessPrincipal::root_dashboard_session("grant-test", "dashboard-control");
            transact_state(&grant_dir, |state, _transaction| {
                let result = upsert_user_client_grant(
                    state,
                    UserClientGrantUpsertRequest {
                        kind: "browser_certificate".to_string(),
                        fingerprint: Some("bb".repeat(32)),
                        role_id: Some("role:session-reader".to_string()),
                        reason: Some("concurrent local grant".to_string()),
                        ..Default::default()
                    },
                    &actor,
                )?;
                Ok((result.grant.id, true))
            })
        });
        grant_started_rx.recv().unwrap();
        release_revoke_tx.send(()).unwrap();

        revoke_thread.join().unwrap().unwrap();
        let second_grant_id = grant_thread.join().unwrap().unwrap();
        let persisted = load_state(directory.path()).unwrap();
        let revoked = persisted
            .grants
            .iter()
            .find(|grant| grant.id == first.grant.id)
            .unwrap();
        assert_eq!(revoked.status, "revoked");
        assert_eq!(revoked.reason, "explicit local revocation");
        assert!(persisted
            .grants
            .iter()
            .any(|grant| grant.id == second_grant_id && grant.status == "active"));
    }
}
