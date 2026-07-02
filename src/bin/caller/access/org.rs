//! Organization identities and signed org-grant documents.
//!
//! Phase 6 of the trust architecture (docs/src/trust-architecture.md): an
//! organization is an Ed25519 root keypair plus a handle. The org signs
//! self-contained grant documents binding a member's browser identity key
//! to a role; any daemon that has locally chosen to trust the org key
//! verifies a presented document and materializes an ordinary local IAM
//! grant from it. There is no membership server: the document is the
//! authorization, the subject's own key is the authentication, and the
//! local daemon is — as everywhere else — the only authority.

use crate::access::iam::{
    is_enforced_status, normalize_client_key_fingerprint, IamAuditEvent, IamGrant, IamPrincipal,
    LocalIamState, TrustedOrg,
};
use crate::access::{AccessError, AccessResult};
use crate::daemon_identity::{b64u, DaemonIdentity};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const ORG_GRANT_PROTOCOL: &str = "intendant-org-grant-v1";
/// Hard cap on document lifetime; the default issuance TTL is 30 days.
pub const MAX_ORG_GRANT_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1000;
pub const DEFAULT_ORG_GRANT_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Tolerated forward clock skew on `issued_at`.
const ISSUED_AT_SKEW_MS: u64 = 5 * 60 * 1000;

/// Where an org's root key lives on its designated daemon.
pub fn org_key_path(cert_dir: &Path, handle: &str) -> PathBuf {
    cert_dir.join("org").join(handle).join("root.pk8")
}

pub fn valid_org_handle(handle: &str) -> bool {
    let handle = handle.trim();
    (2..=40).contains(&handle.len())
        && handle
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !handle.starts_with('-')
        && !handle.ends_with('-')
}

/// Load (or create on first use) the org root key. Reuses the daemon
/// identity plumbing: Ed25519 PKCS#8 on disk, 0600 on Unix.
pub fn load_or_create_org_identity(cert_dir: &Path, handle: &str) -> Result<DaemonIdentity, String> {
    if !valid_org_handle(handle) {
        return Err(format!(
            "invalid org handle {handle:?}: use 2-40 chars of a-z, 0-9, and '-'"
        ));
    }
    DaemonIdentity::load_or_create(org_key_path(cert_dir, handle))
}

pub fn load_org_identity(cert_dir: &Path, handle: &str) -> Result<Option<DaemonIdentity>, String> {
    if !valid_org_handle(handle) {
        return Ok(None);
    }
    let path = org_key_path(cert_dir, handle);
    if !path.exists() {
        return Ok(None);
    }
    DaemonIdentity::load_or_create(path).map(Some)
}

/// Handles this daemon can issue for (org keys present on disk).
pub fn local_org_handles(cert_dir: &Path) -> Vec<String> {
    let mut handles = Vec::new();
    let Ok(entries) = std::fs::read_dir(cert_dir.join("org")) else {
        return handles;
    };
    for entry in entries.flatten() {
        let handle = entry.file_name().to_string_lossy().to_string();
        if valid_org_handle(&handle) && entry.path().join("root.pk8").is_file() {
            handles.push(handle);
        }
    }
    handles.sort();
    handles
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrgGrantOrg {
    pub handle: String,
    pub root_key: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrgGrantSubject {
    pub client_key_fingerprint: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrgGrantDocument {
    pub v: u32,
    pub kind: String,
    pub org: OrgGrantOrg,
    pub subject: OrgGrantSubject,
    pub role_id: String,
    pub targets: Vec<String>,
    pub grant_id: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub sig: String,
}

/// The exact byte string the org root signs. Newline-joined fields, the
/// protocol style used by claim proofs and client-key offers — no JSON
/// canonicalization pitfalls.
pub fn org_grant_signing_payload(doc: &OrgGrantDocument) -> Vec<u8> {
    format!(
        "{ORG_GRANT_PROTOCOL}\n{}\n{}\nclient_key\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        doc.org.handle,
        doc.org.root_key,
        doc.subject.client_key_fingerprint,
        doc.subject.label,
        doc.role_id,
        doc.targets.join(","),
        doc.grant_id,
        doc.issued_at_unix_ms,
        doc.expires_at_unix_ms,
    )
    .into_bytes()
}

pub struct IssueOrgGrantRequest<'a> {
    pub handle: &'a str,
    pub client_key_fingerprint: &'a str,
    pub subject_label: &'a str,
    pub role_id: &'a str,
    pub targets: Vec<String>,
    pub ttl_ms: Option<u64>,
}

/// Sign a new grant document with the org root key. Role validity against
/// the *issuing* daemon's catalog is checked here as a courtesy; the
/// receiving daemon re-validates everything against its own state.
pub fn issue_org_grant(
    identity: &DaemonIdentity,
    state: &LocalIamState,
    request: IssueOrgGrantRequest<'_>,
    now_unix_ms: u64,
) -> AccessResult<OrgGrantDocument> {
    let fingerprint = normalize_client_key_fingerprint(request.client_key_fingerprint);
    if fingerprint.is_empty() {
        return Err(AccessError(
            "client_key_fingerprint is required".to_string(),
        ));
    }
    let role_id = request.role_id.trim();
    let Some(role) = state.roles.iter().find(|role| role.id == role_id) else {
        return Err(AccessError(format!("unknown IAM role {role_id}")));
    };
    if role.id == "role:peer-profile" || role.status == "planned" {
        return Err(AccessError(format!(
            "role {role_id} cannot be granted to a person"
        )));
    }
    let ttl = request.ttl_ms.unwrap_or(DEFAULT_ORG_GRANT_TTL_MS);
    if ttl == 0 || ttl > MAX_ORG_GRANT_TTL_MS {
        return Err(AccessError(format!(
            "ttl_ms must be between 1 and {MAX_ORG_GRANT_TTL_MS} (90 days)"
        )));
    }
    let mut targets: Vec<String> = request
        .targets
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if targets.is_empty() {
        targets.push("*".to_string());
    }
    let mut doc = OrgGrantDocument {
        v: 1,
        kind: "org-grant".to_string(),
        org: OrgGrantOrg {
            handle: request.handle.trim().to_string(),
            root_key: identity.public_key_b64u(),
        },
        subject: OrgGrantSubject {
            client_key_fingerprint: fingerprint,
            label: request.subject_label.trim().to_string(),
        },
        role_id: role_id.to_string(),
        targets,
        grant_id: uuid::Uuid::new_v4().to_string(),
        issued_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl),
        sig: String::new(),
    };
    doc.sig = identity.sign_b64u(&org_grant_signing_payload(&doc));
    Ok(doc)
}

/// Verify a document's own integrity: shape, signature, and time window.
/// Trust (is this org's key accepted here, does the role fit under its
/// cap, is this daemon a target) is the materialization step's job.
pub fn verify_org_grant(doc: &OrgGrantDocument, now_unix_ms: u64) -> Result<(), String> {
    if doc.v != 1 || doc.kind != "org-grant" {
        return Err("unsupported org grant version or kind".to_string());
    }
    if doc.subject.client_key_fingerprint.trim().is_empty() {
        return Err("org grant subject is missing a client key fingerprint".to_string());
    }
    if doc.grant_id.trim().is_empty() {
        return Err("org grant is missing its grant_id".to_string());
    }
    if doc.issued_at_unix_ms > now_unix_ms.saturating_add(ISSUED_AT_SKEW_MS) {
        return Err("org grant issued_at is in the future".to_string());
    }
    if doc.expires_at_unix_ms <= now_unix_ms {
        return Err("org grant has expired".to_string());
    }
    if doc
        .expires_at_unix_ms
        .saturating_sub(doc.issued_at_unix_ms)
        > MAX_ORG_GRANT_TTL_MS
    {
        return Err("org grant lifetime exceeds the 90 day cap".to_string());
    }
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(doc.org.root_key.trim())
        .map_err(|_| "org root key is not valid base64url".to_string())?;
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(doc.sig.trim())
        .map_err(|_| "org grant signature is not valid base64url".to_string())?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&org_grant_signing_payload(doc), &sig)
        .map_err(|_| "org grant signature verification failed".to_string())
}

/// Outcome of materializing a document into local IAM.
#[derive(Clone, Debug, Serialize)]
pub struct MaterializedOrgGrant {
    pub principal: IamPrincipal,
    pub grant: IamGrant,
    pub org_handle: String,
}

/// Verify trust and write the local grant. `daemon_ids` are the names this
/// daemon answers to when matching the document's `targets`.
pub fn materialize_org_grant(
    state: &mut LocalIamState,
    doc: &OrgGrantDocument,
    daemon_ids: &[String],
    now_unix_ms: u64,
) -> AccessResult<MaterializedOrgGrant> {
    verify_org_grant(doc, now_unix_ms).map_err(AccessError)?;

    let handle = doc.org.handle.trim().to_string();
    let Some(trusted) = state.trusted_orgs.iter().find(|org| {
        org.handle == handle
            && org.root_key == doc.org.root_key
            && is_enforced_status(&org.status)
    }) else {
        return Err(AccessError(format!(
            "this daemon does not trust org {handle} with that root key; a root session must trust it under Access → Advanced → Organizations first"
        )));
    };

    let targets_self = doc.targets.iter().any(|target| {
        target == "*" || daemon_ids.iter().any(|id| id == target)
    });
    if !targets_self {
        return Err(AccessError(format!(
            "org grant targets {:?} do not include this daemon",
            doc.targets
        )));
    }

    // The org's local cap: the granted role's permissions must fit inside
    // the max_role's. Reject rather than silently downgrade so issuers
    // learn the cap immediately.
    let max_role_id = if trusted.max_role.trim().is_empty() {
        "role:operator"
    } else {
        trusted.max_role.as_str()
    };
    let Some(role) = state.roles.iter().find(|role| role.id == doc.role_id) else {
        return Err(AccessError(format!(
            "org grant role {} is unknown on this daemon",
            doc.role_id
        )));
    };
    if role.id == "role:peer-profile" || role.status == "planned" {
        return Err(AccessError(format!(
            "org grant role {} cannot be granted to a person",
            doc.role_id
        )));
    }
    let Some(max_role) = state.roles.iter().find(|role| role.id == max_role_id) else {
        return Err(AccessError(format!(
            "trusted org max_role {max_role_id} is not defined; failing closed"
        )));
    };
    if let Some(excess) = role
        .permissions
        .iter()
        .find(|permission| !max_role.permissions.contains(permission))
    {
        return Err(AccessError(format!(
            "org grant role {} exceeds this daemon's cap for org {handle} (max {max_role_id}; {excess} is not allowed)",
            doc.role_id
        )));
    }

    let fingerprint = normalize_client_key_fingerprint(&doc.subject.client_key_fingerprint);
    let principal_id = format!("principal:client-key:{fingerprint}");
    let label = if doc.subject.label.trim().is_empty() {
        format!("{handle} member")
    } else {
        doc.subject.label.trim().to_string()
    };

    let principal = if let Some(existing) = state
        .principals
        .iter_mut()
        .find(|principal| principal.id == principal_id)
    {
        existing.status = "active".to_string();
        existing.clone()
    } else {
        let principal = IamPrincipal {
            id: principal_id.clone(),
            kind: "client_key".to_string(),
            label,
            status: "active".to_string(),
            source: format!("org:{handle}"),
            account: None,
            organization: Some(json!({ "handle": handle })),
            authn: vec![json!({
                "kind": "client_key",
                "label": "Browser identity key",
                "fingerprint": fingerprint,
            })],
            notes: Some(format!("Materialized from an org grant issued by {handle}")),
            created_at_unix_ms: Some(now_unix_ms),
        };
        state.principals.push(principal.clone());
        principal
    };

    let grant_id = format!("grant:org:{handle}:{}", doc.grant_id.trim());
    let grant = IamGrant {
        id: grant_id.clone(),
        principal_id: principal_id.clone(),
        target_id: "local".to_string(),
        role_id: doc.role_id.clone(),
        policy_id: crate::access::iam::policy_for_role(&doc.role_id),
        status: "active".to_string(),
        source: format!("org:{handle}"),
        reason: format!("Org grant {} issued by {handle}", doc.grant_id),
        created_at_unix_ms: Some(now_unix_ms),
        revoked_at_unix_ms: None,
        expires_at_unix_ms: Some(doc.expires_at_unix_ms),
    };
    if let Some(existing) = state.grants.iter_mut().find(|grant| grant.id == grant_id) {
        *existing = grant.clone();
    } else {
        state.grants.push(grant.clone());
    }

    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{now_unix_ms}:{}", state.audit_events.len() + 1),
        at_unix_ms: Some(now_unix_ms),
        actor_principal_id: format!("org:{handle}"),
        action: "materialize_org_grant".to_string(),
        target_id: grant_id,
        summary: format!(
            "Materialized {} for {} from org {handle} (expires {})",
            doc.role_id, principal.label, doc.expires_at_unix_ms
        ),
    });

    Ok(MaterializedOrgGrant {
        principal,
        grant,
        org_handle: handle,
    })
}

/// Root-session action: trust an org key on this daemon.
pub fn trust_org(
    state: &mut LocalIamState,
    handle: &str,
    root_key: &str,
    max_role: Option<&str>,
    now_unix_ms: u64,
) -> AccessResult<TrustedOrg> {
    let handle = handle.trim().to_string();
    if !valid_org_handle(&handle) {
        return Err(AccessError(format!(
            "invalid org handle {handle:?}: use 2-40 chars of a-z, 0-9, and '-'"
        )));
    }
    let root_key = root_key.trim().to_string();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&root_key)
        .map_err(|_| AccessError("org root key is not valid base64url".to_string()))?;
    if decoded.len() != 32 {
        return Err(AccessError(
            "org root key must be a 32-byte Ed25519 public key".to_string(),
        ));
    }
    let max_role = max_role
        .map(str::trim)
        .filter(|role| !role.is_empty())
        .unwrap_or("role:operator")
        .to_string();
    if !state.roles.iter().any(|role| role.id == max_role) {
        return Err(AccessError(format!("unknown max_role {max_role}")));
    }
    let entry = TrustedOrg {
        handle: handle.clone(),
        root_key,
        max_role,
        status: "active".to_string(),
        added_at_unix_ms: Some(now_unix_ms),
    };
    if let Some(existing) = state
        .trusted_orgs
        .iter_mut()
        .find(|org| org.handle == handle)
    {
        *existing = entry.clone();
    } else {
        state.trusted_orgs.push(entry.clone());
    }
    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{now_unix_ms}:{}", state.audit_events.len() + 1),
        at_unix_ms: Some(now_unix_ms),
        actor_principal_id: "principal:root:dashboard".to_string(),
        action: "trust_org".to_string(),
        target_id: format!("org:{}", entry.handle),
        summary: format!(
            "Trusted org {} with cap {}",
            entry.handle, entry.max_role
        ),
    });
    Ok(entry)
}

/// Root-session action: revoke an org's trust and every grant it
/// materialized here. Local IAM always wins over org documents.
pub fn revoke_org(
    state: &mut LocalIamState,
    handle: &str,
    now_unix_ms: u64,
) -> AccessResult<usize> {
    let handle = handle.trim();
    let Some(entry) = state
        .trusted_orgs
        .iter_mut()
        .find(|org| org.handle == handle)
    else {
        return Err(AccessError(format!("org {handle} is not trusted here")));
    };
    entry.status = "revoked".to_string();
    let source = format!("org:{handle}");
    let mut revoked = 0;
    for grant in state
        .grants
        .iter_mut()
        .filter(|grant| grant.source == source && is_enforced_status(&grant.status))
    {
        grant.status = "revoked".to_string();
        grant.revoked_at_unix_ms = Some(now_unix_ms);
        revoked += 1;
    }
    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{now_unix_ms}:{}", state.audit_events.len() + 1),
        at_unix_ms: Some(now_unix_ms),
        actor_principal_id: "principal:root:dashboard".to_string(),
        action: "revoke_org".to_string(),
        target_id: format!("org:{handle}"),
        summary: format!("Revoked org {handle} trust and {revoked} materialized grants"),
    });
    Ok(revoked)
}

/// Fixed-window limiter for the public presentation endpoint.
pub fn presentation_rate_ok(now_unix_ms: u64) -> bool {
    use std::sync::{Mutex, OnceLock};
    const WINDOW_MS: u64 = 60_000;
    const MAX_PER_WINDOW: u32 = 30;
    static WINDOW: OnceLock<Mutex<(u64, u32)>> = OnceLock::new();
    let mut window = WINDOW
        .get_or_init(|| Mutex::new((0, 0)))
        .lock()
        .expect("org presentation limiter poisoned");
    if now_unix_ms.saturating_sub(window.0) >= WINDOW_MS {
        *window = (now_unix_ms, 0);
    }
    if window.1 >= MAX_PER_WINDOW {
        return false;
    }
    window.1 += 1;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org_identity() -> DaemonIdentity {
        let dir = tempfile::tempdir().unwrap();
        let identity = load_or_create_org_identity(dir.path(), "acme").unwrap();
        // Keep the tempdir alive long enough by leaking it; tests are short.
        std::mem::forget(dir);
        identity
    }

    // Anchor test time to the real clock: grant resolution checks expiry
    // against the daemon's actual now, so synthetic epochs would read as
    // long-expired.
    fn test_now() -> u64 {
        crate::access::client_key::now_unix_ms() as u64
    }

    fn issue(identity: &DaemonIdentity, state: &LocalIamState, role: &str) -> OrgGrantDocument {
        issue_org_grant(
            identity,
            state,
            IssueOrgGrantRequest {
                handle: "acme",
                client_key_fingerprint: "member-key",
                subject_label: "Alice",
                role_id: role,
                targets: vec!["*".to_string()],
                ttl_ms: None,
            },
            test_now(),
        )
        .unwrap()
    }

    #[test]
    fn org_grant_signs_verifies_and_materializes() {
        let identity = org_identity();
        let mut state = LocalIamState::default();
        trust_org(&mut state, "acme", &identity.public_key_b64u(), None, test_now()).unwrap();
        let doc = issue(&identity, &state, "role:session-reader");
        verify_org_grant(&doc, test_now()).unwrap();

        let outcome = materialize_org_grant(
            &mut state,
            &doc,
            &["intendant:host-a".to_string()],
            test_now(),
        )
        .unwrap();
        assert_eq!(outcome.principal.kind, "client_key");
        assert_eq!(outcome.grant.source, "org:acme");
        assert_eq!(
            outcome.grant.expires_at_unix_ms,
            Some(doc.expires_at_unix_ms)
        );

        // The member's key now resolves with the granted role.
        let principal =
            crate::access::iam::principal_for_client_key(&state, "member-key", "test").unwrap();
        assert!(crate::access::iam::evaluate_principal_operation_with_state(
            &state,
            &principal,
            crate::peer::access_policy::PeerOperation::SessionInspect,
        )
        .allowed);
        assert!(!crate::access::iam::evaluate_principal_operation_with_state(
            &state,
            &principal,
            crate::peer::access_policy::PeerOperation::AccessManage,
        )
        .allowed);

        // Re-presentation is idempotent (same grant id, refreshed record).
        let again =
            materialize_org_grant(&mut state, &doc, &["intendant:host-a".to_string()], test_now())
                .unwrap();
        assert_eq!(again.grant.id, outcome.grant.id);
        assert_eq!(
            state
                .grants
                .iter()
                .filter(|grant| grant.id == outcome.grant.id)
                .count(),
            1
        );
    }

    #[test]
    fn org_grants_fail_closed_on_trust_tamper_cap_targets_and_expiry() {
        let identity = org_identity();
        let mut state = LocalIamState::default();
        let ids = ["intendant:host-a".to_string()];

        // Untrusted org: refused with guidance.
        let doc = issue(&identity, &state, "role:session-reader");
        let err = materialize_org_grant(&mut state, &doc, &ids, test_now()).unwrap_err();
        assert!(err.to_string().contains("does not trust org acme"));

        trust_org(&mut state, "acme", &identity.public_key_b64u(), None, test_now()).unwrap();

        // Tampered role: signature fails.
        let mut tampered = doc.clone();
        tampered.role_id = "role:root".to_string();
        assert!(verify_org_grant(&tampered, test_now())
            .unwrap_err()
            .contains("signature"));

        // Role above the org cap (default operator): rejected, not downgraded.
        let root_doc = issue(&identity, &state, "role:root");
        let err = materialize_org_grant(&mut state, &root_doc, &ids, test_now()).unwrap_err();
        assert!(err.to_string().contains("exceeds this daemon's cap"), "{err}");

        // Targets that do not include this daemon: refused.
        let mut elsewhere = issue(&identity, &state, "role:observer");
        elsewhere.targets = vec!["intendant:host-b".to_string()];
        elsewhere.sig = identity.sign_b64u(&org_grant_signing_payload(&elsewhere));
        let err = materialize_org_grant(&mut state, &elsewhere, &ids, test_now()).unwrap_err();
        assert!(err.to_string().contains("do not include this daemon"));

        // Expired document: refused at verify time.
        let doc2 = issue(&identity, &state, "role:observer");
        assert!(verify_org_grant(&doc2, doc2.expires_at_unix_ms + 1)
            .unwrap_err()
            .contains("expired"));

        // Wrong key for a trusted handle: refused.
        let other = {
            let dir = tempfile::tempdir().unwrap();
            let identity = load_or_create_org_identity(dir.path(), "acme").unwrap();
            std::mem::forget(dir);
            identity
        };
        let forged = issue(&other, &state, "role:observer");
        let err = materialize_org_grant(&mut state, &forged, &ids, test_now()).unwrap_err();
        assert!(err.to_string().contains("does not trust org acme"));
    }

    #[test]
    fn revoking_org_trust_revokes_its_materialized_grants() {
        let identity = org_identity();
        let mut state = LocalIamState::default();
        trust_org(&mut state, "acme", &identity.public_key_b64u(), None, test_now()).unwrap();
        let doc = issue(&identity, &state, "role:observer");
        materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap();
        assert!(
            crate::access::iam::principal_for_client_key(&state, "member-key", "test").is_some()
        );

        let revoked = revoke_org(&mut state, "acme", test_now()).unwrap();
        assert_eq!(revoked, 1);
        assert!(
            crate::access::iam::principal_for_client_key(&state, "member-key", "test").is_none()
        );
        // And new presentations are refused.
        let err = materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now())
            .unwrap_err();
        assert!(err.to_string().contains("does not trust org acme"));
    }
}
