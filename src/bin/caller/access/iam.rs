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
}

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
}

fn default_role_ceilings() -> std::collections::BTreeMap<String, String> {
    let mut ceilings = std::collections::BTreeMap::new();
    ceilings.insert("connect_account".to_string(), "role:operator".to_string());
    ceilings.insert("client_key".to_string(), "role:operator".to_string());
    ceilings
}

fn default_hosted_origins() -> Vec<String> {
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
        op: crate::peer::access_policy::PeerOperation,
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
        op: crate::peer::access_policy::PeerOperation,
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
        }
    }
}

impl LocalIamState {
    fn normalize(mut self) -> Self {
        if self.schema_version == 0 {
            self.schema_version = IAM_SCHEMA_VERSION;
        }
        for role in builtin_role_templates() {
            if !self.roles.iter().any(|existing| existing.id == role.id) {
                self.roles.push(role);
            }
        }
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
    Ok(())
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
        "browser_certificate" | "connect_account" | "human_user" | ""
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
        _ => Err(AccessError(
            "kind must be client_key, browser_certificate, connect_account, or human_user"
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
        "role:operator" => "policy:operator".to_string(),
        "role:directory-files" => "policy:directory-files".to_string(),
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
            "enforced_principal_kinds": ["root_session", "peer_daemon", "human_user", "browser_certificate", "client_key", "connect_account"],
            "reason": "The daemon enforces trusted owner/root dashboard sessions, daemon peer profiles, and active local IAM user/client grants when requests bind to browser identity keys, browser mTLS certificates, or Connect account identities."
        },
        "role_ceilings": load.state.role_ceilings.clone(),
        "hosted_origins": load.state.hosted_origins.clone(),
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
        "session.inspect" => "Session inspect",
        "session.manage" => "Session manage",
        "terminal.use" => "Terminal use",
        "settings.manage" => "Settings manage",
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
        "peer.manage" => "Create, remove, pair, and use daemon peer routes.",
        "session.inspect" => "Read session lists, logs, reports, recordings, and replay metadata.",
        "session.manage" => "Delete, rewind, prune, upload to, or otherwise mutate sessions.",
        "terminal.use" => "Open and operate dashboard shell sessions.",
        "settings.manage" => "Read or write daemon settings and API keys.",
        "runtime.control" => "Use runtime-control surfaces such as TUI, media, and recording controls.",
        "filesystem.read" => "Stat, list, and read files through dashboard APIs.",
        "filesystem.write" => "Create directories or write uploaded file content.",
        _ => "Operation permission.",
    }
}

pub fn evaluate_principal_operation(
    principal: &AccessPrincipal,
    op: crate::peer::access_policy::PeerOperation,
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
            if crate::peer::access_policy::profile_allows_operation(profile, op) {
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
    op: crate::peer::access_policy::PeerOperation,
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
    if !role
        .permissions
        .iter()
        .any(|candidate| candidate == permission)
    {
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
        if !ceiling_role
            .permissions
            .iter()
            .any(|candidate| candidate == permission)
        {
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
/// configured hosted origins (keys born on daemon-served origins are
/// anchor-grade and uncapped).
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

pub fn operation_permission_id(op: crate::peer::access_policy::PeerOperation) -> &'static str {
    use crate::peer::access_policy::PeerOperation;
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
        PeerOperation::SessionInspect => "session.inspect",
        PeerOperation::SessionManage => "session.manage",
        PeerOperation::Terminal => "terminal.use",
        PeerOperation::Settings => "settings.manage",
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
            json!({
                "id": grant.id.clone(),
                "principal_id": grant.principal_id.clone(),
                "target_id": if grant.target_id.is_empty() { default_target_id } else { grant.target_id.as_str() },
                "kind": "user_client_local_iam",
                "kind_label": "Local IAM user/client grant",
                "policy_id": if grant.policy_id.is_empty() { "policy:scoped-human" } else { grant.policy_id.as_str() },
                "role": role_id,
                "role_label": role_label(state, role_id),
                "transport_id": "transport:local-user-client-binding",
                "source": if grant.source.is_empty() { "local_iam_state" } else { grant.source.as_str() },
                "status": status,
                "enforced": grant.is_active_at(now),
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
                "session.inspect".to_string(),
                "session.manage".to_string(),
                "terminal.use".to_string(),
                "settings.manage".to_string(),
                "runtime.control".to_string(),
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
            ],
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
            summary: "Open and use shell sessions without broader dashboard mutation rights."
                .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "access.inspect".to_string(),
                "session.inspect".to_string(),
                "terminal.use".to_string(),
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
            id: "role:operator".to_string(),
            label: "Operator".to_string(),
            status: "enforced".to_string(),
            summary:
                "Operate sessions, display, shell, files, and approvals without access/settings administration."
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
                "session.inspect".to_string(),
                "session.manage".to_string(),
                "terminal.use".to_string(),
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:directory-files".to_string(),
            label: "Directory scoped files".to_string(),
            status: "planned".to_string(),
            summary: "Future file role bounded by selected roots and operations.".to_string(),
            permissions: vec!["filesystem.read".to_string()],
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
        "session.inspect",
        "session.manage",
        "terminal.use",
        "settings.manage",
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
        });

        let grants = grant_overview_values(&state, "local-daemon");

        assert_eq!(grants[0]["target_id"], "local-daemon");
        assert_eq!(grants[0]["status"], "draft");
        assert_eq!(grants[0]["enforced"], false);
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
                crate::peer::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
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
                crate::peer::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
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
                crate::peer::access_policy::PeerOperation::Terminal,
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
            crate::peer::access_policy::PeerOperation::Terminal,
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
                crate::peer::access_policy::PeerOperation::Terminal,
            )
            .allowed
        );
        let denied = evaluate_principal_operation_with_state(
            &state,
            &principal,
            crate::peer::access_policy::PeerOperation::AccessManage,
        );
        assert!(!denied.allowed);
        assert!(denied.reason.contains("role ceiling"));

        // Clearing the ceiling restores the full granted role.
        state.role_ceilings.clear();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
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

        // Anchor-origin keys are anchor-grade: no ceiling.
        let anchor = principal_for_client_key(&state, "anchor-key", "connect").unwrap();
        assert_eq!(
            anchor.authn_origin.as_deref(),
            Some("https://anchor.local:8765")
        );
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &anchor,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );

        // Keys enrolled from a hosted origin are capped.
        let hosted = principal_for_client_key(&state, "hosted-key", "connect").unwrap();
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::peer::access_policy::PeerOperation::Terminal,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &hosted,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
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
                crate::peer::access_policy::PeerOperation::Terminal,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::peer::access_policy::PeerOperation::Settings,
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
                crate::peer::access_policy::PeerOperation::AccessInspect,
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
                crate::peer::access_policy::PeerOperation::SessionInspect,
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
                crate::peer::access_policy::PeerOperation::Terminal,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::peer::access_policy::PeerOperation::FilesystemWrite,
            )
            .allowed
        );
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
        for op in [
            crate::peer::access_policy::PeerOperation::PresenceRead,
            crate::peer::access_policy::PeerOperation::StatsRead,
            crate::peer::access_policy::PeerOperation::DisplayView,
            crate::peer::access_policy::PeerOperation::DisplayInput,
            crate::peer::access_policy::PeerOperation::Message,
            crate::peer::access_policy::PeerOperation::Task,
            crate::peer::access_policy::PeerOperation::Approval,
            crate::peer::access_policy::PeerOperation::AccessInspect,
            crate::peer::access_policy::PeerOperation::AccessManage,
            crate::peer::access_policy::PeerOperation::PeerInspect,
            crate::peer::access_policy::PeerOperation::PeerManage,
            crate::peer::access_policy::PeerOperation::SessionInspect,
            crate::peer::access_policy::PeerOperation::SessionManage,
            crate::peer::access_policy::PeerOperation::Terminal,
            crate::peer::access_policy::PeerOperation::Settings,
            crate::peer::access_policy::PeerOperation::RuntimeControl,
            crate::peer::access_policy::PeerOperation::FilesystemRead,
            crate::peer::access_policy::PeerOperation::FilesystemWrite,
        ] {
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
                crate::peer::access_policy::PeerOperation::DisplayView,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation(
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }
}
