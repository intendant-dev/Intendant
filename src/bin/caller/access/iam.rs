//! Local Access/IAM state.
//!
//! This is deliberately a local daemon-owned access model. The daemon can
//! distinguish trusted owner/root dashboard sessions, daemon peer identities, and
//! active user/client records bound to stable browser mTLS or Connect account
//! metadata.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{AccessError, AccessResult};

pub const IAM_STATE_FILE: &str = "iam.json";
pub const IAM_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    IAM_SCHEMA_VERSION
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
    /// Effective-permission ceilings for low-provenance authn bindings,
    /// keyed by binding kind (`connect_account`, `client_key`). A session
    /// authenticated by a capped binding never exceeds the ceiling role's
    /// permissions, no matter what its grant says. `connect_account`
    /// sessions are always subject to their ceiling; `client_key` sessions
    /// only when the key's recorded enrollment origin is in
    /// `hosted_origins`. Owners who accept hosted-root risk can raise or
    /// clear a ceiling by editing this map (an explicit empty map disables
    /// ceilings entirely).
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
    ceilings.insert("connect_account".to_string(), "role:operator".to_string());
    ceilings.insert("client_key".to_string(), "role:operator".to_string());
    ceilings
}

pub fn default_hosted_origins() -> Vec<String> {
    vec!["https://connect.intendant.dev".to_string()]
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
    /// Origin the key was enrolled from, recorded by the trusted session
    /// that creates the grant. Role ceilings use this to distinguish
    /// anchor-origin keys from hosted-origin keys.
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
    /// The authn binding kind that actually authenticated this session
    /// (e.g. `client_key`, `connect_account`, `browser_mtls_cert`). Role
    /// ceilings key off this, not the principal kind, because one principal
    /// (a `human_user`) can carry several bindings of different provenance.
    #[serde(default)]
    pub authn_kind: Option<String>,
    /// The origin recorded on the matched binding at grant time, when the
    /// binding carries one (client keys do).
    #[serde(default)]
    pub authn_origin: Option<String>,
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
    /// dashboard and enrolled root user clients (both keep the root
    /// dashboard grant id, minted nowhere else) plus the documented
    /// anything-with-a-shell local loopback principal. The derived
    /// transport defaults — supervised agent sessions and MCP token
    /// holders — share the `root_session` *kind* but drop the grant id,
    /// so this predicate must never be widened to the kind alone: those
    /// callers are root-compatible for IAM yet are exactly who the
    /// user-display grant exists to hold.
    pub fn is_owner_surface(&self) -> bool {
        self.grant_id.as_deref() == Some("grant:root:dashboard")
            || self.id == "principal:local-process:loopback"
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
            authn_origin: None,
        }
    }

    pub fn root_user_client(
        source: impl Into<String>,
        transport: impl Into<String>,
        label: impl Into<String>,
        account: Option<Value>,
        organization: Option<Value>,
        authn: Vec<Value>,
    ) -> Self {
        let mut principal = Self::root_dashboard_session(source, transport);
        principal.label = label.into();
        principal.account = account;
        principal.organization = organization;
        principal.authn = authn;
        principal
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

    /// A trusted-local root session whose offer carried a verified browser
    /// identity key that has no local grant yet. The session keeps its
    /// root-compatible authority (the transport is trusted), but the key is
    /// surfaced in `authn` so the UI can offer to enroll it.
    pub fn root_dashboard_session_with_client_key(
        source: impl Into<String>,
        transport: impl Into<String>,
        client_key_fingerprint: &str,
        client_key_public_b64u: &str,
    ) -> Self {
        let mut principal = Self::root_dashboard_session(source, transport);
        principal.authn.push(serde_json::json!({
            "kind": "client_key",
            "label": "Browser identity key",
            "fingerprint": client_key_fingerprint,
            "public_key": client_key_public_b64u,
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
            authn_origin: None,
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
            authn_origin: None,
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
        }
    }
}

impl LocalIamState {
    fn normalize(mut self) -> Self {
        if self.schema_version == 0 {
            self.schema_version = IAM_SCHEMA_VERSION;
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

pub fn load_state(cert_dir: &Path) -> AccessResult<LocalIamState> {
    let path = iam_state_path(cert_dir);
    if !path.exists() {
        return Ok(LocalIamState::default());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| AccessError(format!("read {}: {e}", path.display())))?;
    let state: LocalIamState = serde_json::from_str(&contents)
        .map_err(|e| AccessError(format!("parse {}: {e}", path.display())))?;
    Ok(state.normalize())
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

#[allow(dead_code)]
pub fn save_state(cert_dir: &Path, state: &LocalIamState) -> AccessResult<()> {
    std::fs::create_dir_all(cert_dir)?;
    let path = iam_state_path(cert_dir);
    let tmp = path.with_extension("json.tmp");
    let normalized = state.clone().normalize();
    let mut contents = serde_json::to_vec_pretty(&normalized)
        .map_err(|e| AccessError(format!("serialize {}: {e}", path.display())))?;
    contents.push(b'\n');
    std::fs::write(&tmp, contents)?;
    set_private_perms(&tmp)?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        AccessError(format!(
            "rename {} to {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    // Refresh the read cache with the state just persisted, keyed by the
    // renamed file's fresh fingerprint. Best-effort: the per-read
    // fingerprint re-check below is the correctness backbone, so a lost
    // refresh only costs one re-parse.
    if let Some(fingerprint) = iam_state_fingerprint(&path) {
        store_cached_iam_state(&path, fingerprint, normalized);
    }
    Ok(())
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

fn store_cached_iam_state(path: &Path, fingerprint: IamStateFingerprint, state: LocalIamState) {
    let mut cache = iam_state_cache().lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= IAM_STATE_CACHE_MAX_DIRS && !cache.contains_key(path) {
        cache.clear();
    }
    cache.insert(
        path.to_path_buf(),
        IamStateCacheEntry {
            fingerprint,
            state: std::sync::Arc::new(state),
        },
    );
}

/// [`load_state`] behind a stat-fingerprint cache: the per-request read
/// path (mTLS request authorization stats + parses `iam.json` on every
/// HTTP request, including statics) pays one `stat` instead of a full
/// read + parse + normalize when the file is unchanged. Never trusts
/// invalidation alone — every call re-checks the fingerprint, so writers
/// that bypass [`save_state`] (other processes, hand edits) are picked up
/// on the next request. Parse errors are never cached.
pub fn load_state_cached(cert_dir: &Path) -> AccessResult<LocalIamState> {
    let path = iam_state_path(cert_dir);
    let Some(fingerprint) = iam_state_fingerprint(&path) else {
        // Missing file: same contract as load_state. Nothing to cache —
        // the default is cheap relative to a request.
        return Ok(LocalIamState::default());
    };
    {
        let cache = iam_state_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache
            .get(&path)
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            return Ok((*entry.state).clone());
        }
    }
    let state = load_state(cert_dir)?;
    // Re-stat AFTER the read: if the file changed between the stat and the
    // read, caching the read under the pre-read fingerprint could pin a
    // torn view. Matching fingerprints prove read and stat saw one file.
    match iam_state_fingerprint(&path) {
        Some(after) if after == fingerprint => {
            store_cached_iam_state(&path, fingerprint, state.clone());
        }
        _ => {}
    }
    Ok(state)
}

struct UserClientBinding {
    principal_id: String,
    principal_kind: String,
    label: String,
    account: Option<Value>,
    organization: Option<Value>,
    authn: Vec<Value>,
}

/// `--owner <client-key-fingerprint>` bootstrap (credential custody):
/// seed a root grant pinned to the given browser identity key so a fresh
/// install is owned from first boot — authority minted locally, nothing
/// secret on the wire (the fingerprint is public). Idempotent: an
/// existing root binding for the key is left untouched, so restarting
/// with the same flag neither duplicates grants nor grows the audit log.
pub fn seed_owner_bootstrap_grant(cert_dir: &Path, fingerprint: &str) -> AccessResult<bool> {
    let fingerprint = normalize_client_key_fingerprint(fingerprint);
    if fingerprint.is_empty() {
        return Err(AccessError(
            "--owner requires a client-key fingerprint (shown in the Access drawer)".to_string(),
        ));
    }
    let mut state = load_state(cert_dir)?;
    if let Some(existing) = principal_for_client_key(&state, &fingerprint, "owner-bootstrap") {
        if existing.role_id == "role:root" {
            return Ok(false);
        }
    }
    let actor = AccessPrincipal::root_dashboard_session("owner-bootstrap", "cli");
    upsert_user_client_grant(
        &mut state,
        UserClientGrantUpsertRequest {
            client_key_fingerprint: Some(fingerprint),
            label: Some("Owner (bootstrap)".to_string()),
            role_id: Some("role:root".to_string()),
            status: Some("active".to_string()),
            reason: Some(
                "--owner bootstrap: root authority pinned to this browser key at install time"
                    .to_string(),
            ),
            ..Default::default()
        },
        &actor,
    )?;
    save_state(cert_dir, &state)?;
    Ok(true)
}

pub fn upsert_user_client_grant(
    state: &mut LocalIamState,
    request: UserClientGrantUpsertRequest,
    actor: &AccessPrincipal,
) -> AccessResult<UserClientGrantUpsertResult> {
    for role in builtin_role_templates() {
        if !state.roles.iter().any(|existing| existing.id == role.id) {
            state.roles.push(role);
        }
    }

    let kind = normalize_user_client_kind(&request)?;
    let role_id = request
        .role_id
        .as_deref()
        .and_then(trimmed_nonempty)
        .unwrap_or("role:scoped-human")
        .to_string();
    validate_user_client_role(state, &role_id)?;
    let status = normalize_user_client_status(request.status.as_deref())?;
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
    for role in builtin_role_templates() {
        if !state.roles.iter().any(|existing| existing.id == role.id) {
            state.roles.push(role);
        }
    }

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
    if !matches!(
        state.principals[principal_index].kind.as_str(),
        "browser_certificate"
            | "connect_account"
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
    if let Some(role_id) = role_id.as_deref() {
        validate_user_client_role(state, role_id)?;
    }
    let status = match request.status.as_deref() {
        Some(_) => Some(normalize_user_client_status(request.status.as_deref())?),
        None => None,
    };
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

/// The binding kinds the hosted-control ceiling governs: Connect-account
/// sessions and browser identity keys enrolled from a hosted origin.
pub const HOSTED_CEILING_BINDINGS: [&str; 2] = ["connect_account", "client_key"];

/// Set the hosted-control ceiling — the `role_ceilings` entries for both
/// hosted-provenance binding kinds — to `role_id`, which must name a
/// defined, enforced role (`role:none` refuses hosted control entirely).
/// Pure state mutation + audit event; the caller persists. Divergent
/// per-binding ceilings remain possible by editing `iam.json`; this
/// function is the one-knob path the dashboard exposes.
pub fn set_hosted_control_ceiling(
    state: &mut LocalIamState,
    role_id: &str,
    actor: &AccessPrincipal,
) -> AccessResult<()> {
    for role in builtin_role_templates() {
        if !state.roles.iter().any(|existing| existing.id == role.id) {
            state.roles.push(role);
        }
    }
    let role_id = role_id.trim();
    let Some(role) = state.roles.iter().find(|role| role.id == role_id) else {
        return Err(AccessError(format!("unknown IAM role {role_id}")));
    };
    if role.status == "planned" {
        return Err(AccessError(format!(
            "IAM role {role_id} is planned but not enforced"
        )));
    }
    let role_id = role.id.clone();
    let previous: Vec<String> = HOSTED_CEILING_BINDINGS
        .iter()
        .map(|binding| {
            state
                .role_ceilings
                .get(*binding)
                .cloned()
                .unwrap_or_else(|| "uncapped".to_string())
        })
        .collect();
    if previous.iter().all(|existing| *existing == role_id) {
        return Ok(());
    }
    for binding in HOSTED_CEILING_BINDINGS {
        state
            .role_ceilings
            .insert(binding.to_string(), role_id.clone());
    }
    let now = now_unix_ms();
    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{}:{}", now, state.audit_events.len() + 1),
        at_unix_ms: Some(now),
        actor_principal_id: actor.id.clone(),
        action: "set_hosted_control_ceiling".to_string(),
        target_id: "role_ceilings".to_string(),
        summary: format!(
            "Hosted control ceiling {} -> {role_id}",
            if previous[0] == previous[1] {
                previous[0].clone()
            } else {
                previous.join(" / ")
            }
        ),
    });
    Ok(())
}

fn validate_user_client_role(state: &LocalIamState, role_id: &str) -> AccessResult<()> {
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
        "connect_account" | "connect-account" | "passkey_account" | "passkey-account" => {
            Ok("connect_account".to_string())
        }
        "human_user" | "human-user" | "human" | "human_mtls" | "human-mtls" => {
            Ok("human_user".to_string())
        }
        "agent_session" | "agent-session" => Ok("agent_session".to_string()),
        "local_process" | "local-process" | "loopback_mcp" | "loopback-mcp" => {
            Ok("local_process".to_string())
        }
        _ => Err(AccessError(
            "kind must be client_key, browser_certificate, connect_account, human_user, agent_session, or local_process"
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
    if fingerprint.is_none()
        && client_key_fingerprint.is_none()
        && user_id.is_none()
        && handle.is_none()
    {
        return Err(AccessError(
            "human_user requires a fingerprint, client key, user_id, or handle".to_string(),
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
            "enforced_principal_kinds": ["root_session", "peer_daemon", "human_user", "browser_certificate", "client_key", "connect_account", "agent_session", "local_process"],
            "reason": "The daemon enforces trusted owner/root dashboard sessions, daemon peer profiles, and active local IAM user/client grants when requests bind to browser identity keys, browser mTLS certificates, or Connect account identities. /mcp requests bind to supervised agent sessions (session_id + token), the MCP token holder, or the local loopback principal, and every tool call is evaluated per-operation."
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

    // Role ceilings: the effective permission set of a low-provenance
    // session is the intersection of its granted role and the ceiling role
    // for the binding that authenticated it. The grant stays intact; only
    // this session's authority is bounded.
    if let Some(ceiling_role_id) = role_ceiling_for_session(state, principal) {
        let Some(ceiling_role) = state.roles.iter().find(|role| role.id == ceiling_role_id) else {
            return AccessDecision::denied(
                principal,
                op,
                format!(
                    "role ceiling {ceiling_role_id} is configured but not defined; failing closed"
                ),
            );
        };
        if !permissions_allow(&ceiling_role.permissions, permission) {
            let binding = principal.authn_kind.as_deref().unwrap_or("session");
            return AccessDecision::denied(
                principal,
                op,
                format!(
                    "role ceiling {ceiling_role_id} for {binding} bindings does not allow {permission}"
                ),
            );
        }
    }

    AccessDecision::allowed(
        principal,
        op,
        format!("local IAM role {role_id} allows {permission}"),
    )
}

/// The ceiling role applying to this session, if any. `connect_account`
/// bindings are always subject to their configured ceiling; `client_key`
/// bindings only when the key's recorded enrollment origin is one of the
/// configured hosted origins. Keys born on daemon-served origins are
/// uncapped — including fleet-name origins, whose code is daemon-served
/// but whose ROUTE the rendezvous names (first-contact rung two,
/// docs/src/trust-tiers.md); owners who want fleet-name sessions capped
/// add the fleet zone's origins to `hosted_origins`.
pub fn role_ceiling_for_session(
    state: &LocalIamState,
    principal: &AccessPrincipal,
) -> Option<String> {
    let binding = principal.authn_kind.as_deref()?;
    let ceiling = state.role_ceilings.get(binding)?;
    if binding == "client_key" {
        let origin = principal.authn_origin.as_deref().unwrap_or("");
        let hosted = !origin.is_empty()
            && state
                .hosted_origins
                .iter()
                .any(|candidate| candidate.trim_end_matches('/') == origin.trim_end_matches('/'));
        if !hosted {
            return None;
        }
    }
    Some(ceiling.clone())
}

/// Origin class of a session for the custody trail
/// (docs/src/trust-tiers.md): `hosted` (Connect account or a browser key
/// enrolled from one of `hosted_origins`), `direct` (a key or mTLS
/// certificate on a daemon-served origin — fleet-name origins included:
/// daemon-served for code, rendezvous-named for routing; the finer
/// routing distinction is [`origin_route_class`]), `local` (the owner's
/// own dashboard / loopback), or `peer` (a federated daemon).
/// Classification mirrors [`role_ceiling_for_session`]'s hosted test.
pub fn session_origin_class(
    hosted_origins: &[String],
    principal: &AccessPrincipal,
) -> &'static str {
    if principal.kind == "peer_daemon" {
        return "peer";
    }
    match principal.authn_kind.as_deref() {
        Some("connect_account") => "hosted",
        Some("client_key") => {
            let origin = principal.authn_origin.as_deref().unwrap_or("");
            let hosted = !origin.is_empty()
                && hosted_origins.iter().any(|candidate| {
                    candidate.trim_end_matches('/') == origin.trim_end_matches('/')
                });
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
/// - `hosted`: one of `hosted_origins` — the rendezvous serves the code.
/// - `fleet`: a name under the rendezvous's delegated fleet zone — the
///   daemon serves the code, but the rendezvous names the route (it could
///   hijack DNS and mint a certificate; active-only, CT-logged).
/// - `direct`: any other explicit origin (typed IP, mDNS, own domain).
/// - `unknown`: no origin recorded (pre-origin enrollments).
///
/// Distinct from [`session_origin_class`]: that classifies for custody
/// (code provenance), this classifies for approval decisions (route
/// provenance). A fleet origin is `direct`-grade there and `fleet` here.
pub fn origin_route_class(
    origin: &str,
    hosted_origins: &[String],
    fleet_zone: Option<&str>,
) -> &'static str {
    let origin = origin.trim();
    if origin.is_empty() {
        return "unknown";
    }
    if hosted_origins
        .iter()
        .any(|candidate| candidate.trim_end_matches('/') == origin.trim_end_matches('/'))
    {
        return "hosted";
    }
    if let Some(zone) = fleet_zone.map(str::trim).filter(|zone| !zone.is_empty()) {
        let zone = zone.trim_end_matches('.').to_ascii_lowercase();
        let host = url::Url::parse(origin)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
        if let Some(host) = host {
            let host = host.trim_end_matches('.');
            if host == zone || host.ends_with(&format!(".{zone}")) {
                return "fleet";
            }
        }
    }
    "direct"
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
    }
}

pub fn principal_overview_values(state: &LocalIamState) -> Vec<Value> {
    state
        .principals
        .iter()
        .map(|principal| {
            json!({
                "id": principal.id.clone(),
                "kind": if principal.kind.is_empty() { "human_user" } else { principal.kind.as_str() },
                "kind_label": principal_kind_label(&principal.kind),
                "label": if principal.label.is_empty() { principal.id.as_str() } else { principal.label.as_str() },
                "source": if principal.source.is_empty() { "local_iam_state" } else { principal.source.as_str() },
                "status": if principal.status.is_empty() { "draft" } else { principal.status.as_str() },
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
    let now = crate::access::client_key::now_unix_ms();
    state
        .grants
        .iter()
        .map(|grant| {
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
            json!({
                "id": grant.id.clone(),
                "principal_id": grant.principal_id.clone(),
                "target_id": target_id,
                "kind": "user_client_local_iam",
                "kind_label": "Local IAM user/client grant",
                "policy_id": if grant.policy_id.is_empty() { "policy:scoped-human" } else { grant.policy_id.as_str() },
                "role": role_id,
                "role_label": role_label(state, role_id),
                "transport_id": "transport:local-user-client-binding",
                "source": if grant.source.is_empty() { "local_iam_state" } else { grant.source.as_str() },
                "status": status,
                "enforced": grant.is_active_at(now),
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
                // Fueling from a hosted session is the core custody flow and
                // the hosted ceiling defaults to operator — without this the
                // vault bootstrap story dies at its last step. Scoped guest
                // roles deliberately do not get it.
                "credentials.manage".to_string(),
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
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
    ]
    .iter()
    .map(|permission| (*permission).to_string())
    .collect()
}

fn principal_kind_label(kind: &str) -> &'static str {
    match kind {
        "browser_certificate" => "Browser certificate",
        "client_key" => "Browser key",
        "connect_account" => "Connect account",
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

pub fn principal_for_client_key_any_status(
    state: &LocalIamState,
    fingerprint: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let fingerprint = normalize_client_key_fingerprint(fingerprint);
    principal_for_authn_any_status(state, "client_key", "fingerprint", &fingerprint, transport)
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

pub fn principal_for_connect_account(
    state: &LocalIamState,
    user_id: &str,
    account_name: Option<&str>,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let transport = transport.into();
    principal_for_authn(
        state,
        "connect_account",
        "user_id",
        user_id,
        transport.clone(),
    )
    .or_else(|| {
        account_name.and_then(|name| {
            principal_for_authn(state, "connect_account", "account_name", name, transport)
        })
    })
}

pub fn principal_for_connect_account_any_status(
    state: &LocalIamState,
    user_id: &str,
    account_name: Option<&str>,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let transport = transport.into();
    principal_for_authn_any_status(
        state,
        "connect_account",
        "user_id",
        user_id,
        transport.clone(),
    )
    .or_else(|| {
        account_name.and_then(|name| {
            principal_for_authn_any_status(
                state,
                "connect_account",
                "account_name",
                name,
                transport,
            )
        })
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
    fn dashboard_hosted_ceiling_choices_name_builtin_roles() {
        let app = app_html();
        let builtin: std::collections::BTreeSet<String> = builtin_role_templates()
            .into_iter()
            .map(|role| role.id)
            .collect();
        let choices = slice_between(app, "const ACCESS_HOSTED_CEILING_CHOICES = [", "\n];");
        let mirrored = quoted_role_ids(choices);
        assert!(
            !mirrored.is_empty(),
            "ACCESS_HOSTED_CEILING_CHOICES (static/app.html) lists no role ids"
        );
        for role_id in &mirrored {
            assert!(
                builtin.contains(role_id),
                "ACCESS_HOSTED_CEILING_CHOICES entry {role_id} is not a builtin role"
            );
        }
        assert!(
            mirrored.contains("role:none"),
            "the hosted-ceiling knob must offer the refuse-entirely position (role:none)"
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
            load_state_cached(tmp.path()).unwrap(),
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
        let cached = load_state_cached(tmp.path()).unwrap();
        assert_eq!(cached, load_state(tmp.path()).unwrap());
        assert!(cached
            .principals
            .iter()
            .any(|p| p.id == "principal:cache-a"));
        // Second read (a cache hit) is identical.
        assert_eq!(load_state_cached(tmp.path()).unwrap(), cached);

        // A writer that bypasses save_state (another process, a hand
        // edit) must be picked up by the per-call fingerprint re-check —
        // never trust invalidation.
        let mut external = cached.clone();
        for principal in &mut external.principals {
            if principal.id == "principal:cache-a" {
                principal.label = "Cache A (externally rewritten)".to_string();
            }
        }
        let body = serde_json::to_string_pretty(&external).unwrap();
        std::fs::write(iam_state_path(tmp.path()), body).unwrap();
        let reread = load_state_cached(tmp.path()).unwrap();
        assert!(reread
            .principals
            .iter()
            .any(|p| p.label == "Cache A (externally rewritten)"));
        assert_eq!(reread, load_state(tmp.path()).unwrap());

        // Deleting the file falls back to the default state.
        std::fs::remove_file(iam_state_path(tmp.path())).unwrap();
        assert_eq!(
            load_state_cached(tmp.path()).unwrap(),
            LocalIamState::default()
        );
    }

    #[test]
    fn owner_bootstrap_seeds_root_once_and_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();

        assert!(seed_owner_bootstrap_grant(tmp.path(), "Owner_Key-Fp").unwrap());
        let state = load_state(tmp.path()).unwrap();
        let principal =
            principal_for_client_key(&state, "Owner_Key-Fp", "test").expect("owner principal");
        assert_eq!(principal.kind, "client_key");
        assert_eq!(principal.role_id, "role:root");
        let audit_after_first = state.audit_events.len();

        // Restarting with the same flag must not duplicate anything.
        assert!(!seed_owner_bootstrap_grant(tmp.path(), "Owner_Key-Fp").unwrap());
        let state = load_state(tmp.path()).unwrap();
        assert_eq!(state.audit_events.len(), audit_after_first);
        assert_eq!(
            state
                .grants
                .iter()
                .filter(|grant| grant.principal_id == principal.id)
                .count(),
            1
        );

        // Whitespace-only fingerprints are refused.
        assert!(seed_owner_bootstrap_grant(tmp.path(), "   ").is_err());
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
        // Owner surfaces: the trusted dashboard / enrolled root user
        // clients, and the documented local-shell loopback principal.
        assert!(AccessPrincipal::root_dashboard_session("test", "dashboard").is_owner_surface());
        assert!(AccessPrincipal::root_user_client(
            "test",
            "dashboard",
            "Alice",
            None,
            None,
            Vec::new()
        )
        .is_owner_surface());
        assert!(AccessPrincipal::root_dashboard_session_with_client_key(
            "test",
            "dashboard",
            "fp",
            "pk"
        )
        .is_owner_surface());
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
    fn role_ceiling_caps_connect_account_sessions() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "connect_account".to_string(),
                user_id: Some("user-123".to_string()),
                account_name: Some("alice".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        let principal =
            principal_for_connect_account(&state, "user-123", Some("alice"), "connect").unwrap();
        assert_eq!(principal.authn_kind.as_deref(), Some("connect_account"));
        // The grant says root, but the default connect_account ceiling is
        // operator: operating permissions pass, admin permissions do not.
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::ShellSpawn,
            )
            .allowed
        );
        let denied = evaluate_principal_operation_with_state(
            &state,
            &principal,
            crate::access::access_policy::PeerOperation::AccessManage,
        );
        assert!(!denied.allowed);
        assert!(denied.reason.contains("role ceiling"));

        // Clearing the ceiling restores the full granted role.
        state.role_ceilings.clear();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn role_ceiling_caps_only_hosted_origin_client_keys() {
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
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("hosted-key".to_string()),
                client_key_origin: Some("https://connect.intendant.dev".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        // Keys born on daemon-served origins are uncapped: no ceiling.
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

        // Keys enrolled from a hosted origin are capped.
        let hosted = principal_for_client_key(&state, "hosted-key", "connect").unwrap();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::access::access_policy::PeerOperation::ShellSpawn,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
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
    fn hosted_control_ceiling_role_none_refuses_hosted_sessions_entirely() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                client_key_fingerprint: Some("hosted-key".to_string()),
                client_key_origin: Some("https://connect.intendant.dev".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
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

        set_hosted_control_ceiling(&mut state, "role:none", &actor).unwrap();
        for binding in HOSTED_CEILING_BINDINGS {
            assert_eq!(
                state.role_ceilings.get(binding).map(String::as_str),
                Some("role:none")
            );
        }
        assert!(state
            .audit_events
            .iter()
            .any(|event| event.action == "set_hosted_control_ceiling"));

        // A hosted-origin key with a root grant can no longer do anything —
        // not even the observer-grade operations operator allowed.
        let hosted = principal_for_client_key(&state, "hosted-key", "connect").unwrap();
        for op in [
            crate::access::access_policy::PeerOperation::ShellSpawn,
            crate::access::access_policy::PeerOperation::DisplayView,
            crate::access::access_policy::PeerOperation::SessionInspect,
        ] {
            let denied = evaluate_principal_operation_with_state(&state, &hosted, op);
            assert!(!denied.allowed, "expected {op:?} denied under role:none");
            assert!(denied.reason.contains("role ceiling"));
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

        // Idempotent set: no extra audit event.
        let audit_count = state.audit_events.len();
        set_hosted_control_ceiling(&mut state, "role:none", &actor).unwrap();
        assert_eq!(state.audit_events.len(), audit_count);

        // And the knob moves back up.
        set_hosted_control_ceiling(&mut state, "role:operator", &actor).unwrap();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::access::access_policy::PeerOperation::ShellSpawn,
            )
            .allowed
        );
    }

    #[test]
    fn set_hosted_control_ceiling_requires_a_defined_role() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let err = set_hosted_control_ceiling(&mut state, "role:fortress", &actor).unwrap_err();
        assert!(err.to_string().contains("unknown IAM role"));
        assert_eq!(state.role_ceilings, default_role_ceilings());
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
    fn upsert_connect_account_grant_creates_active_binding() {
        let mut state = LocalIamState::default();
        let actor = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        let result = upsert_user_client_grant(
            &mut state,
            UserClientGrantUpsertRequest {
                kind: "connect_account".to_string(),
                user_id: Some("user-123".to_string()),
                account_name: Some("alice".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();

        assert_eq!(result.principal.kind, "connect_account");
        assert_eq!(result.principal.label, "@alice");
        assert_eq!(result.grant.role_id, "role:scoped-human");

        let principal =
            principal_for_connect_account(&state, "user-123", Some("alice"), "dashboard-control")
                .unwrap();
        assert_eq!(principal.id, result.principal.id);
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
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
}
