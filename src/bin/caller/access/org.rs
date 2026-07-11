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
use crate::daemon_identity::DaemonIdentity;
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
pub fn load_or_create_org_identity(
    cert_dir: &Path,
    handle: &str,
) -> Result<DaemonIdentity, String> {
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
    /// A member's browser identity key (human lane). Exactly one of this
    /// and `peer_fingerprint` must be present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_key_fingerprint: String,
    /// A member daemon's mTLS certificate fingerprint (peer lane,
    /// 64 hex chars). Materializes into the peer identity store.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub peer_fingerprint: String,
    #[serde(default)]
    pub label: String,
}

impl OrgGrantSubject {
    pub fn is_peer(&self) -> bool {
        !self.peer_fingerprint.trim().is_empty()
    }

    /// The payload line naming the subject kind. Baked into every
    /// signature, so a document can never be replayed across kinds.
    fn kind_line(&self) -> &'static str {
        if self.is_peer() {
            "peer_daemon"
        } else {
            "client_key"
        }
    }

    fn fingerprint_line(&self) -> &str {
        if self.is_peer() {
            self.peer_fingerprint.trim()
        } else {
            &self.client_key_fingerprint
        }
    }
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
    /// Issuer delegation chain (step 6b): empty when the root signed the
    /// document; exactly one root-signed issuer certificate when a
    /// delegated key did. Not part of the document payload — the chain
    /// is carried beside the signature it explains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<OrgIssuerCert>,
    pub sig: String,
}

/// A root-signed delegation certificate: day-to-day signing moves off
/// the root. One level only — an issuer cannot mint issuers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrgIssuerCert {
    pub v: u32,
    pub kind: String,
    pub org: OrgGrantOrg,
    pub issuer_key: String,
    #[serde(default)]
    pub label: String,
    /// Optional scope: `role:*` caps human documents (permission subset,
    /// checked at materialization where the role catalog lives); `peer:*`
    /// caps peer documents (profile containment). A scoped cert refuses
    /// documents of the other kind; no scope allows both.
    #[serde(default)]
    pub max_role: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub sig: String,
}

pub const ORG_ISSUER_PROTOCOL: &str = "intendant-org-issuer-v1";
/// Hard cap on delegation lifetime.
pub const MAX_ORG_ISSUER_TTL_MS: u64 = 365 * 24 * 60 * 60 * 1000;

pub fn org_issuer_signing_payload(cert: &OrgIssuerCert) -> Vec<u8> {
    format!(
        "{ORG_ISSUER_PROTOCOL}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        cert.org.handle,
        cert.org.root_key,
        cert.issuer_key,
        cert.label,
        cert.max_role,
        cert.issued_at_unix_ms,
        cert.expires_at_unix_ms,
    )
    .into_bytes()
}

/// Sign a delegation certificate with the org root key.
pub fn delegate_org_issuer(
    identity: &DaemonIdentity,
    handle: &str,
    issuer_key: &str,
    label: &str,
    max_role: &str,
    ttl_ms: Option<u64>,
    now_unix_ms: u64,
) -> Result<OrgIssuerCert, String> {
    let issuer_key = issuer_key.trim().to_string();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&issuer_key)
        .map_err(|_| "issuer key is not valid base64url".to_string())?;
    if decoded.len() != 32 {
        return Err("issuer key must be a 32-byte Ed25519 public key".to_string());
    }
    if issuer_key == identity.public_key_b64u() {
        return Err(
            "the org root key cannot be its own issuer; delegate a different key".to_string(),
        );
    }
    let ttl = ttl_ms.unwrap_or(MAX_ORG_ISSUER_TTL_MS);
    if ttl == 0 || ttl > MAX_ORG_ISSUER_TTL_MS {
        return Err(format!(
            "ttl_ms must be between 1 and {MAX_ORG_ISSUER_TTL_MS} (365 days)"
        ));
    }
    let max_role = max_role.trim();
    if !max_role.is_empty() && !max_role.starts_with("role:") && !max_role.starts_with("peer:") {
        return Err(
            "issuer scope must be a role:* or peer:* id (or empty for both lanes)".to_string(),
        );
    }
    let mut cert = OrgIssuerCert {
        v: 1,
        kind: "org-issuer".to_string(),
        org: OrgGrantOrg {
            handle: handle.trim().to_string(),
            root_key: identity.public_key_b64u(),
        },
        issuer_key,
        label: label.trim().to_string(),
        max_role: max_role.to_string(),
        issued_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl),
        sig: String::new(),
    };
    cert.sig = identity.sign_b64u(&org_issuer_signing_payload(&cert));
    Ok(cert)
}

/// Verify a certificate's own integrity and window against the org key
/// it names. Whether THAT key is trusted here is the caller's problem.
pub fn verify_org_issuer_cert(cert: &OrgIssuerCert, now_unix_ms: u64) -> Result<(), String> {
    if cert.v != 1 || cert.kind != "org-issuer" {
        return Err("unsupported org issuer certificate version or kind".to_string());
    }
    if cert.expires_at_unix_ms <= now_unix_ms {
        return Err("org issuer certificate has expired".to_string());
    }
    if cert.issued_at_unix_ms > now_unix_ms.saturating_add(ISSUED_AT_SKEW_MS) {
        return Err("org issuer certificate issued_at is in the future".to_string());
    }
    if cert
        .expires_at_unix_ms
        .saturating_sub(cert.issued_at_unix_ms)
        > MAX_ORG_ISSUER_TTL_MS
    {
        return Err("org issuer certificate lifetime exceeds the 365 day cap".to_string());
    }
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cert.org.root_key.trim())
        .map_err(|_| "org root key is not valid base64url".to_string())?;
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cert.sig.trim())
        .map_err(|_| "org issuer certificate signature is not valid base64url".to_string())?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&org_issuer_signing_payload(cert), &sig)
        .map_err(|_| "org issuer certificate signature verification failed".to_string())
}

/// The exact byte string the org root signs. Newline-joined fields, the
/// protocol style used by claim proofs and client-key offers — no JSON
/// canonicalization pitfalls.
pub fn org_grant_signing_payload(doc: &OrgGrantDocument) -> Vec<u8> {
    format!(
        "{ORG_GRANT_PROTOCOL}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        doc.org.handle,
        doc.org.root_key,
        doc.subject.kind_line(),
        doc.subject.fingerprint_line(),
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
    /// Human lane subject; exactly one of this and `peer_fingerprint`.
    pub client_key_fingerprint: &'a str,
    /// Peer lane subject (member daemon mTLS cert fingerprint).
    pub peer_fingerprint: &'a str,
    pub subject_label: &'a str,
    pub role_id: &'a str,
    pub targets: Vec<String>,
    pub ttl_ms: Option<u64>,
}

/// The `peer:` role namespace used by peer-subject documents; the rest is
/// a peer profile (`session-reader`, `operator`, …), never an IAM role —
/// a peer document cannot be confused with a human-role document even
/// outside the signed payload.
pub const PEER_ROLE_PREFIX: &str = "peer:";

pub fn peer_profile_from_role(role_id: &str) -> Option<&str> {
    role_id.trim().strip_prefix(PEER_ROLE_PREFIX)
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
    let client_fingerprint = normalize_client_key_fingerprint(request.client_key_fingerprint);
    let peer_fingerprint_raw = request.peer_fingerprint.trim();
    if client_fingerprint.is_empty() && peer_fingerprint_raw.is_empty() {
        return Err(AccessError(
            "a subject is required: client_key_fingerprint (a member's browser key) or peer_fingerprint (a member daemon's certificate)".to_string(),
        ));
    }
    if !client_fingerprint.is_empty() && !peer_fingerprint_raw.is_empty() {
        return Err(AccessError(
            "a document binds exactly one subject: client_key_fingerprint or peer_fingerprint, not both".to_string(),
        ));
    }
    let role_id = request.role_id.trim();
    let peer_fingerprint = if peer_fingerprint_raw.is_empty() {
        String::new()
    } else {
        // Peer subjects take a peer profile in the `peer:` namespace, not
        // an IAM role; the fingerprint is the mTLS cert digest.
        let normalized = crate::access::access_policy::normalize_fingerprint(peer_fingerprint_raw)
            .map_err(|e| AccessError(e.to_string()))?;
        let Some(profile) = peer_profile_from_role(role_id) else {
            return Err(AccessError(format!(
                "peer-subject documents use the peer profile vocabulary: expected peer:<profile>, got {role_id}"
            )));
        };
        crate::access::access_policy::normalize_profile(profile)
            .map_err(|e| AccessError(e.to_string()))?;
        normalized
    };
    if peer_fingerprint.is_empty() {
        let Some(role) = state.roles.iter().find(|role| role.id == role_id) else {
            return Err(AccessError(format!("unknown IAM role {role_id}")));
        };
        if role.id == "role:peer-profile" || role.status == "planned" {
            return Err(AccessError(format!(
                "role {role_id} cannot be granted to a person"
            )));
        }
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
            client_key_fingerprint: client_fingerprint,
            peer_fingerprint,
            label: request.subject_label.trim().to_string(),
        },
        role_id: role_id.to_string(),
        targets,
        grant_id: uuid::Uuid::new_v4().to_string(),
        issued_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl),
        chain: Vec::new(),
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
    let has_client = !doc.subject.client_key_fingerprint.trim().is_empty();
    let has_peer = doc.subject.is_peer();
    if !has_client && !has_peer {
        return Err("org grant subject is missing a fingerprint".to_string());
    }
    if has_client && has_peer {
        return Err(
            "org grant subject must bind exactly one of a client key or a peer daemon".to_string(),
        );
    }
    if has_peer {
        crate::access::access_policy::normalize_fingerprint(&doc.subject.peer_fingerprint)
            .map_err(|e| e.to_string())?;
        if peer_profile_from_role(&doc.role_id).is_none() {
            return Err(format!(
                "peer-subject documents use the peer profile vocabulary: expected peer:<profile>, got {}",
                doc.role_id
            ));
        }
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
    if doc.expires_at_unix_ms.saturating_sub(doc.issued_at_unix_ms) > MAX_ORG_GRANT_TTL_MS {
        return Err("org grant lifetime exceeds the 90 day cap".to_string());
    }
    // Outside-in: the trusted root validates the (at most one) issuer
    // certificate, and whichever key ends the chain validates the document.
    let signing_key_b64u = match doc.chain.len() {
        0 => doc.org.root_key.trim().to_string(),
        1 => {
            let cert = &doc.chain[0];
            if cert.org.handle.trim() != doc.org.handle.trim()
                || cert.org.root_key != doc.org.root_key
            {
                return Err("issuer certificate and document belong to different orgs".to_string());
            }
            verify_org_issuer_cert(cert, now_unix_ms)?;
            match cert.max_role.trim() {
                "" => {}
                scope if scope.starts_with("peer:") => {
                    if !doc.subject.is_peer() {
                        return Err("this issuer may only sign peer-subject documents".to_string());
                    }
                    let granted = peer_profile_from_role(&doc.role_id).unwrap_or("");
                    let cap = scope.strip_prefix("peer:").unwrap_or(scope);
                    if !crate::access::access_policy::profile_fits_under(granted, cap) {
                        return Err(format!(
                            "document profile {} exceeds the issuer's scope {scope}",
                            doc.role_id
                        ));
                    }
                }
                _scope => {
                    // role:* scope caps human documents; the permission
                    // subset needs the role catalog, so materialization
                    // enforces it — here we only refuse kind mismatches.
                    if doc.subject.is_peer() {
                        return Err("this issuer may only sign human-subject documents".to_string());
                    }
                }
            }
            cert.issuer_key.trim().to_string()
        }
        _ => return Err("issuer chains are one level: root, then issuer".to_string()),
    };
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&signing_key_b64u)
        .map_err(|_| "org signing key is not valid base64url".to_string())?;
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
    /// False when the document was already materialized identically —
    /// re-presentation (offers attach documents on every connect) must not
    /// grow the audit log or rewrite state.
    #[serde(skip)]
    pub changed: bool,
}

/// Verify trust and write the local grant. `daemon_ids` are the names this
/// daemon answers to when matching the document's `targets`.
/// The subject fingerprint in its kind's normalized form — what ORL
/// `revoked_subjects` entries are matched against.
fn subject_fingerprint(doc: &OrgGrantDocument) -> String {
    if doc.subject.is_peer() {
        crate::access::access_policy::normalize_fingerprint(&doc.subject.peer_fingerprint)
            .unwrap_or_else(|_| doc.subject.peer_fingerprint.trim().to_string())
    } else {
        normalize_client_key_fingerprint(&doc.subject.client_key_fingerprint)
    }
}

/// Shared pre-checks for both materialization lanes: the document is
/// intact, this daemon trusts the signing key, the org's applied
/// revocation list does not cover it, and this daemon is a target.
fn trusted_org_for_doc<'a>(
    state: &'a LocalIamState,
    doc: &OrgGrantDocument,
    daemon_ids: &[String],
    now_unix_ms: u64,
) -> AccessResult<&'a TrustedOrg> {
    verify_org_grant(doc, now_unix_ms).map_err(AccessError)?;

    let handle = doc.org.handle.trim();
    let Some(trusted) = state.trusted_orgs.iter().find(|org| {
        org.handle == handle && org.root_key == doc.org.root_key && is_enforced_status(&org.status)
    }) else {
        return Err(AccessError(format!(
            "this daemon does not trust org {handle} with that root key; a root session must trust it under Access → Advanced → Organizations first"
        )));
    };

    // The org's own revocation list, as last applied here: a listed grant
    // or subject is refused even though the document's signature is valid
    // — a still-held document must not outrun its revocation.
    let fingerprint = subject_fingerprint(doc);
    let doc_grant_id = doc.grant_id.trim();
    if trusted
        .orl_revoked_grant_ids
        .iter()
        .any(|id| id == doc_grant_id)
    {
        return Err(AccessError(format!(
            "org grant {doc_grant_id} is revoked by org {handle}'s revocation list"
        )));
    }
    if trusted
        .orl_revoked_subjects
        .iter()
        .any(|subject| subject == &fingerprint)
    {
        return Err(AccessError(format!(
            "the subject key is revoked by org {handle}'s revocation list"
        )));
    }
    if let Some(cert) = doc.chain.first() {
        if trusted
            .orl_revoked_issuer_keys
            .iter()
            .any(|key| key == cert.issuer_key.trim())
        {
            return Err(AccessError(format!(
                "the issuer key that signed this document is revoked by org {handle}'s revocation list"
            )));
        }
    }

    let targets_self = doc
        .targets
        .iter()
        .any(|target| target == "*" || daemon_ids.iter().any(|id| id == target));
    if !targets_self {
        return Err(AccessError(format!(
            "org grant targets {:?} do not include this daemon",
            doc.targets
        )));
    }
    Ok(trusted)
}

pub fn materialize_org_grant(
    state: &mut LocalIamState,
    doc: &OrgGrantDocument,
    daemon_ids: &[String],
    now_unix_ms: u64,
) -> AccessResult<MaterializedOrgGrant> {
    if doc.subject.is_peer() {
        return Err(AccessError(
            "peer-subject documents materialize into the peer identity store".to_string(),
        ));
    }
    let trusted = trusted_org_for_doc(state, doc, daemon_ids, now_unix_ms)?;
    let handle = doc.org.handle.trim().to_string();
    let fingerprint = normalize_client_key_fingerprint(&doc.subject.client_key_fingerprint);

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
    if let Some(excess) =
        crate::access::iam::permissions_excess(&role.permissions, &max_role.permissions)
    {
        return Err(AccessError(format!(
            "org grant role {} exceeds this daemon's cap for org {handle} (max {max_role_id}; {excess} is not allowed)",
            doc.role_id
        )));
    }

    // A delegated issuer's `role:*` scope caps the human roles it may sign.
    // verify_org_grant can only check subject-kind fit (it has no role
    // catalog); the permission-subset half of the scope lives here, where
    // the roles exist.
    if let Some(cert) = doc.chain.first() {
        let scope = cert.max_role.trim();
        if scope.starts_with("role:") {
            let Some(scoped_role) = state.roles.iter().find(|role| role.id == scope) else {
                return Err(AccessError(format!(
                    "issuer scope {scope} is not defined on this daemon; failing closed"
                )));
            };
            if let Some(excess) =
                crate::access::iam::permissions_excess(&role.permissions, &scoped_role.permissions)
            {
                return Err(AccessError(format!(
                    "org grant role {} exceeds the issuer's scope {scope} ({excess} is not allowed)",
                    doc.role_id
                )));
            }
        }
    }

    let principal_id = format!("principal:client-key:{fingerprint}");
    let label = if doc.subject.label.trim().is_empty() {
        format!("{handle} member")
    } else {
        doc.subject.label.trim().to_string()
    };

    // Local IAM always wins over the document: a principal or grant the
    // local owner revoked stays revoked. Without this, offers that carry
    // the document would silently undo a local revocation on the next
    // connect. The org's way out is a fresh document (new grant_id).
    let mut changed = false;
    let principal = if let Some(existing) = state
        .principals
        .iter_mut()
        .find(|principal| principal.id == principal_id)
    {
        if existing.status == "revoked" {
            return Err(AccessError(
                "the subject key's principal was revoked locally on this daemon; a root session must re-activate it under Access → People & Devices".to_string(),
            ));
        }
        if existing.status != "active" {
            existing.status = "active".to_string();
            changed = true;
        }
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
        changed = true;
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
        issued_via: doc
            .chain
            .first()
            .map(|cert| cert.issuer_key.trim().to_string()),
        fs_scope: None,
    };
    let grant = if let Some(existing) = state.grants.iter_mut().find(|grant| grant.id == grant_id) {
        if existing.status == "revoked" || existing.revoked_at_unix_ms.is_some() {
            return Err(AccessError(format!(
                "org grant {} was revoked locally on this daemon; local IAM wins — the org must issue a new grant, or a root session can re-enable this one",
                doc.grant_id
            )));
        }
        let identical = existing.role_id == grant.role_id
            && existing.expires_at_unix_ms == grant.expires_at_unix_ms
            && is_enforced_status(&existing.status);
        if identical {
            existing.clone()
        } else {
            *existing = grant.clone();
            changed = true;
            grant
        }
    } else {
        state.grants.push(grant.clone());
        changed = true;
        grant
    };

    if changed {
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
    }

    Ok(MaterializedOrgGrant {
        principal,
        grant,
        org_handle: handle,
        changed,
    })
}

pub fn issuer_key_path(cert_dir: &Path, handle: &str) -> PathBuf {
    cert_dir.join("org").join(handle).join("issuer.pk8")
}

pub fn issuer_cert_path(cert_dir: &Path, handle: &str) -> PathBuf {
    cert_dir.join("org").join(handle).join("issuer-cert.json")
}

/// Deputy-side: create (or return) this daemon's issuer keypair for an
/// org. Holding a key grants nothing until the root signs a certificate
/// for it and it is installed here.
pub fn load_or_create_issuer_identity(
    cert_dir: &Path,
    handle: &str,
) -> Result<DaemonIdentity, String> {
    if !valid_org_handle(handle) {
        return Err(format!(
            "invalid org handle {handle:?}: use 2-40 chars of a-z, 0-9, and '-'"
        ));
    }
    DaemonIdentity::load_or_create(issuer_key_path(cert_dir, handle))
}

pub fn load_issuer_identity(
    cert_dir: &Path,
    handle: &str,
) -> Result<Option<DaemonIdentity>, String> {
    if !valid_org_handle(handle) {
        return Ok(None);
    }
    let path = issuer_key_path(cert_dir, handle);
    if !path.exists() {
        return Ok(None);
    }
    DaemonIdentity::load_or_create(path).map(Some)
}

pub fn load_issuer_cert(cert_dir: &Path, handle: &str) -> Result<Option<OrgIssuerCert>, String> {
    let path = issuer_cert_path(cert_dir, handle);
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Deputy-side: persist the root-signed certificate for the local issuer
/// key after verifying it actually names that key and its signature and
/// window hold.
pub fn install_issuer_cert(
    cert_dir: &Path,
    handle: &str,
    cert: &OrgIssuerCert,
    now_unix_ms: u64,
) -> Result<(), String> {
    let issuer = load_issuer_identity(cert_dir, handle)?.ok_or_else(|| {
        format!("this daemon holds no issuer key for org {handle:?}; initialize one first")
    })?;
    if cert.issuer_key.trim() != issuer.public_key_b64u() {
        return Err("the certificate names a different issuer key".to_string());
    }
    if cert.org.handle.trim() != handle.trim() {
        return Err("the certificate belongs to a different org handle".to_string());
    }
    verify_org_issuer_cert(cert, now_unix_ms)?;
    let path = issuer_cert_path(cert_dir, handle);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(cert).map_err(|e| e.to_string())?;
    std::fs::write(&path, format!("{body}\n"))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Outcome of materializing a peer-subject document into the peer
/// identity store.
#[derive(Clone, Debug, Serialize)]
pub struct MaterializedOrgPeerGrant {
    pub record: crate::access::access_policy::PeerIdentityRecord,
    pub org_handle: String,
    #[serde(skip)]
    pub changed: bool,
}

/// Materialize a peer-subject document into the peer identity store —
/// daemons are peers, never people. Same rules as the human lane:
/// fail-closed cap (empty `max_peer_profile` refuses everything),
/// idempotent-quiet re-presentation, and no resurrection of locally
/// revoked identities. The audit trail lives in the IAM state.
pub fn materialize_org_peer_grant(
    state: &mut LocalIamState,
    cert_dir: &Path,
    doc: &OrgGrantDocument,
    daemon_ids: &[String],
    now_unix_ms: u64,
) -> AccessResult<MaterializedOrgPeerGrant> {
    use crate::access::access_policy as pol;
    if !doc.subject.is_peer() {
        return Err(AccessError("not a peer-subject document".to_string()));
    }
    let trusted = trusted_org_for_doc(state, doc, daemon_ids, now_unix_ms)?;
    let handle = doc.org.handle.trim().to_string();
    let profile = peer_profile_from_role(&doc.role_id).ok_or_else(|| {
        AccessError("peer-subject documents use peer:<profile> roles".to_string())
    })?;
    let profile = pol::normalize_profile(profile).map_err(|e| AccessError(e.to_string()))?;

    // Fail closed: the human and peer lanes are separate trust decisions,
    // so trusting an org grants no daemon-to-daemon authority until the
    // owner sets a peer cap explicitly.
    let cap = trusted.max_peer_profile.trim().to_string();
    if cap.is_empty() {
        return Err(AccessError(format!(
            "this daemon grants org {handle} no peer authority; a root session must set max_peer_profile under Access → Advanced → Organizations first"
        )));
    }
    if !pol::profile_fits_under(&profile, &cap) {
        return Err(AccessError(format!(
            "org grant profile peer:{profile} exceeds this daemon's peer cap for org {handle} (max peer:{cap})"
        )));
    }

    let fingerprint = pol::normalize_fingerprint(&doc.subject.peer_fingerprint)
        .map_err(|e| AccessError(e.to_string()))?;
    let expires_at_unix = (doc.expires_at_unix_ms / 1000) as i64;
    let label = if doc.subject.label.trim().is_empty() {
        format!("{handle} member daemon")
    } else {
        doc.subject.label.trim().to_string()
    };

    let existing =
        pol::lookup_identity(cert_dir, &fingerprint).map_err(|e| AccessError(e.to_string()))?;
    if let Some(existing) = existing.as_ref() {
        if matches!(existing.status, pol::PeerIdentityStatus::Revoked) {
            return Err(AccessError(
                "the subject daemon's identity was revoked locally on this daemon; a root session must re-approve it".to_string(),
            ));
        }
        let identical = existing.profile == profile
            && existing.expires_at_unix == Some(expires_at_unix)
            && existing.source.as_deref() == Some(&format!("org:{handle}") as &str)
            && existing.org_grant_id.as_deref() == Some(doc.grant_id.trim());
        if identical {
            return Ok(MaterializedOrgPeerGrant {
                record: existing.clone(),
                org_handle: handle,
                changed: false,
            });
        }
    }

    let record = pol::PeerIdentityRecord {
        version: 1,
        fingerprint: fingerprint.clone(),
        label,
        profile,
        status: pol::PeerIdentityStatus::Approved,
        card_url: existing.as_ref().and_then(|r| r.card_url.clone()),
        request_id: existing.as_ref().and_then(|r| r.request_id.clone()),
        filesystem: existing
            .as_ref()
            .map(|r| r.filesystem.clone())
            .unwrap_or_default(),
        created_at_unix: existing
            .as_ref()
            .map(|r| r.created_at_unix)
            .unwrap_or((now_unix_ms / 1000) as i64),
        revoked_at_unix: None,
        expires_at_unix: Some(expires_at_unix),
        source: Some(format!("org:{handle}")),
        org_grant_id: Some(doc.grant_id.trim().to_string()),
        issued_via: doc
            .chain
            .first()
            .map(|cert| cert.issuer_key.trim().to_string()),
    };
    pol::write_identity_record(cert_dir, &record).map_err(|e| AccessError(e.to_string()))?;

    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{now_unix_ms}:{}", state.audit_events.len() + 1),
        at_unix_ms: Some(now_unix_ms),
        actor_principal_id: format!("org:{handle}"),
        action: "materialize_org_peer_grant".to_string(),
        target_id: format!("peer:{}", record.fingerprint),
        summary: format!(
            "Materialized peer profile {} for {} from org {handle} (expires {})",
            record.profile, record.label, doc.expires_at_unix_ms
        ),
    });

    Ok(MaterializedOrgPeerGrant {
        record,
        org_handle: handle,
        changed: true,
    })
}

/// A presented document, materialized into whichever store its subject
/// kind belongs to.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum PresentedOrgGrant {
    Human(Box<MaterializedOrgGrant>),
    Peer(MaterializedOrgPeerGrant),
}

impl PresentedOrgGrant {
    pub fn changed(&self) -> bool {
        match self {
            Self::Human(outcome) => outcome.changed,
            Self::Peer(outcome) => outcome.changed,
        }
    }

    pub fn org_handle(&self) -> &str {
        match self {
            Self::Human(outcome) => &outcome.org_handle,
            Self::Peer(outcome) => &outcome.org_handle,
        }
    }
}

/// The names a daemon answers to when an org grant's `targets` list is
/// matched. Callers pass their path-specific ids (the agent card id and
/// label on the HTTP gateway, the configured rendezvous daemon id on the
/// Connect client); every path shares the configured Connect daemon id,
/// the stored host label, and the host's agent-card peer-id form, so a
/// document targeting any of this daemon's names materializes no matter
/// which door it arrives through.
pub fn org_target_daemon_ids(extra_ids: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    fn push(ids: &mut Vec<String>, value: &str) {
        let value = value.trim();
        if !value.is_empty() && !ids.iter().any(|existing| existing == value) {
            ids.push(value.to_string());
        }
    }
    for id in extra_ids {
        push(&mut ids, id);
    }
    if let Ok(connect_id) = std::env::var("INTENDANT_CONNECT_DAEMON_ID") {
        push(&mut ids, &connect_id);
    }
    let host_label = crate::access::resolve_host_label();
    push(&mut ids, &host_label);
    push(
        &mut ids,
        intendant_core::peer_id::PeerId::new(
            intendant_core::peer_id::PeerKind::Intendant,
            &host_label,
        )
        .as_str(),
    );
    ids
}

/// Documents ride along on dashboard-control offers, so cap what a relay
/// can make a daemon parse. Matches the public endpoint's body cap.
pub const MAX_ORG_GRANT_DOC_BYTES: usize = 16 * 1024;

/// Parse and materialize a raw document value against the given state.
/// The IO-free core of [`present_org_grant_value`], shared so tests and
/// the offer ride-along paths exercise the same semantics.
pub fn present_org_grant_state(
    state: &mut LocalIamState,
    cert_dir: &Path,
    doc_value: &serde_json::Value,
    extra_daemon_ids: &[String],
    now_unix_ms: u64,
) -> Result<PresentedOrgGrant, String> {
    if serde_json::to_string(doc_value)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > MAX_ORG_GRANT_DOC_BYTES
    {
        return Err("org grant document is too large".to_string());
    }
    let doc: OrgGrantDocument = serde_json::from_value(doc_value.clone())
        .map_err(|e| format!("invalid org grant document: {e}"))?;
    let daemon_ids = org_target_daemon_ids(extra_daemon_ids);
    if doc.subject.is_peer() {
        materialize_org_peer_grant(state, cert_dir, &doc, &daemon_ids, now_unix_ms)
            .map(PresentedOrgGrant::Peer)
            .map_err(|e| e.to_string())
    } else {
        materialize_org_grant(state, &doc, &daemon_ids, now_unix_ms)
            .map(|outcome| PresentedOrgGrant::Human(Box::new(outcome)))
            .map_err(|e| e.to_string())
    }
}

/// Present a raw org-grant document against this daemon's IAM state on
/// disk: rate-limit, parse, verify, materialize, persist. Shared by the
/// public presentation endpoint and the offer ride-along paths — the
/// document is the authorization on all of them, and a failure changes
/// nothing.
pub fn present_org_grant_value(
    cert_dir: &std::path::Path,
    doc_value: &serde_json::Value,
    extra_daemon_ids: &[String],
    now_unix_ms: u64,
) -> Result<PresentedOrgGrant, String> {
    if !presentation_rate_ok(now_unix_ms) {
        return Err("too many org grant presentations; retry shortly".to_string());
    }
    let mut state = crate::access::iam::load_state(cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let outcome = present_org_grant_state(
        &mut state,
        cert_dir,
        doc_value,
        extra_daemon_ids,
        now_unix_ms,
    )?;
    if outcome.changed() {
        crate::access::iam::save_state(cert_dir, &state)
            .map_err(|e| format!("save local IAM state: {e}"))?;
    }
    Ok(outcome)
}

/// ── Org revocation lists (phase 6 step 5) ──
///
/// The root signs a cumulative list of revoked document grant ids and
/// subject fingerprints. The org daemon maintains it next to the root
/// key; anyone may carry it to a consuming daemon, whose signature check
/// plus monotonic `seq` make the courier irrelevant. Applying it revokes
/// matching materialized grants AND persists the lists on the trusted-org
/// entry so future materialization/renewal of listed entries is refused.
pub const ORG_ORL_PROTOCOL: &str = "intendant-org-orl-v1";
/// Body cap for carried lists (~1500 revocations); the org daemon refuses
/// to grow a list past this so consumers can always accept it.
pub const MAX_ORG_ORL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrgRevocationList {
    pub v: u32,
    pub kind: String,
    pub org: OrgGrantOrg,
    pub seq: u64,
    pub revoked_grant_ids: Vec<String>,
    pub revoked_subjects: Vec<String>,
    /// Delegated issuer keys revoked wholesale: every document they
    /// signed is refused and every grant they materialized is swept.
    #[serde(default)]
    pub revoked_issuer_keys: Vec<String>,
    pub issued_at_unix_ms: u64,
    pub sig: String,
}

/// Newline-joined signing payload, like every protocol here. Entries are
/// comma-joined inside their line: grant ids are UUIDs and subjects are
/// base64url fingerprints, so neither can contain a comma.
pub fn orl_signing_payload(orl: &OrgRevocationList) -> Vec<u8> {
    format!(
        "{ORG_ORL_PROTOCOL}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        orl.org.handle,
        orl.org.root_key,
        orl.seq,
        orl.revoked_grant_ids.join(","),
        orl.revoked_subjects.join(","),
        orl.revoked_issuer_keys.join(","),
        orl.issued_at_unix_ms,
    )
    .into_bytes()
}

/// Integrity only (shape + signature). Trust — is this org's key trusted
/// here, is the seq fresh — is [`apply_orl`]'s job. Lists do not expire;
/// `seq` is the staleness control.
pub fn verify_orl(orl: &OrgRevocationList) -> Result<(), String> {
    if orl.v != 1 || orl.kind != "org-revocations" {
        return Err("unsupported org revocation list version or kind".to_string());
    }
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(orl.org.root_key.trim())
        .map_err(|_| "org root key is not valid base64url".to_string())?;
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(orl.sig.trim())
        .map_err(|_| "org revocation list signature is not valid base64url".to_string())?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&orl_signing_payload(orl), &sig)
        .map_err(|_| "org revocation list signature verification failed".to_string())
}

pub fn orl_path(cert_dir: &Path, handle: &str) -> PathBuf {
    cert_dir.join("org").join(handle).join("orl.json")
}

/// The org daemon's current list, signing an empty seq-0 list lazily when
/// none exists yet. A corrupt on-disk list is an error, not a reset — a
/// re-signed lower `seq` would be refused by every consumer.
pub fn load_or_init_orl(
    identity: &DaemonIdentity,
    cert_dir: &Path,
    handle: &str,
    now_unix_ms: u64,
) -> Result<OrgRevocationList, String> {
    let path = orl_path(cert_dir, handle);
    if path.exists() {
        let raw =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let orl: OrgRevocationList =
            serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        verify_orl(&orl)?;
        if orl.org.root_key != identity.public_key_b64u() {
            return Err(format!(
                "{} was signed by a different org root key",
                path.display()
            ));
        }
        return Ok(orl);
    }
    let mut orl = OrgRevocationList {
        v: 1,
        kind: "org-revocations".to_string(),
        org: OrgGrantOrg {
            handle: handle.trim().to_string(),
            root_key: identity.public_key_b64u(),
        },
        seq: 0,
        revoked_grant_ids: Vec::new(),
        revoked_subjects: Vec::new(),
        revoked_issuer_keys: Vec::new(),
        issued_at_unix_ms: now_unix_ms,
        sig: String::new(),
    };
    orl.sig = identity.sign_b64u(&orl_signing_payload(&orl));
    Ok(orl)
}

/// Org-daemon action: add entries, bump `seq`, re-sign, persist.
pub fn orl_revoke(
    identity: &DaemonIdentity,
    cert_dir: &Path,
    handle: &str,
    grant_ids: &[String],
    subjects: &[String],
    issuer_keys: &[String],
    now_unix_ms: u64,
) -> Result<OrgRevocationList, String> {
    let mut orl = load_or_init_orl(identity, cert_dir, handle, now_unix_ms)?;
    let mut added = false;
    for id in grant_ids {
        let id = id.trim();
        if id.contains(',') {
            return Err(format!("invalid grant id {id:?}"));
        }
        if !id.is_empty() && !orl.revoked_grant_ids.iter().any(|existing| existing == id) {
            orl.revoked_grant_ids.push(id.to_string());
            added = true;
        }
    }
    for subject in subjects {
        let subject = normalize_client_key_fingerprint(subject);
        if subject.contains(',') {
            return Err(format!("invalid subject fingerprint {subject:?}"));
        }
        if !subject.is_empty()
            && !orl
                .revoked_subjects
                .iter()
                .any(|existing| existing == &subject)
        {
            orl.revoked_subjects.push(subject);
            added = true;
        }
    }
    for key in issuer_keys {
        let key = key.trim();
        if key.contains(',') {
            return Err(format!("invalid issuer key {key:?}"));
        }
        if !key.is_empty()
            && !orl
                .revoked_issuer_keys
                .iter()
                .any(|existing| existing == key)
        {
            orl.revoked_issuer_keys.push(key.to_string());
            added = true;
        }
    }
    if !added {
        return Err("nothing new to revoke: pass a document grant_id, a subject key fingerprint, or an issuer key".to_string());
    }
    orl.seq += 1;
    orl.issued_at_unix_ms = now_unix_ms;
    orl.sig = identity.sign_b64u(&orl_signing_payload(&orl));
    let serialized = serde_json::to_string_pretty(&orl).map_err(|e| e.to_string())?;
    if serialized.len() > MAX_ORG_ORL_BYTES {
        return Err(format!(
            "revocation list would exceed {MAX_ORG_ORL_BYTES} bytes; expire old grants instead of growing it further"
        ));
    }
    let path = orl_path(cert_dir, handle);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, format!("{serialized}\n"))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(orl)
}

#[derive(Clone, Debug, Serialize)]
pub struct AppliedOrl {
    pub org_handle: String,
    pub seq: u64,
    pub revoked_grants: usize,
    pub revoked_peer_identities: usize,
    /// False when this seq (or a newer one) was already applied.
    pub changed: bool,
}

/// Consumer action: verify against the locally trusted key, enforce
/// monotonic `seq`, persist the lists, and revoke matching materialized
/// grants. Idempotent for an already-applied seq; stale lists are refused
/// loudly so couriers learn they carried an old copy.
pub fn apply_orl(
    state: &mut LocalIamState,
    cert_dir: &Path,
    orl: &OrgRevocationList,
    now_unix_ms: u64,
) -> AccessResult<AppliedOrl> {
    verify_orl(orl).map_err(AccessError)?;
    let handle = orl.org.handle.trim().to_string();
    let Some(index) = state.trusted_orgs.iter().position(|org| {
        org.handle == handle && org.root_key == orl.org.root_key && is_enforced_status(&org.status)
    }) else {
        return Err(AccessError(format!(
            "this daemon does not trust org {handle} with that root key, so its revocation list does not apply here"
        )));
    };
    if orl.seq < state.trusted_orgs[index].last_orl_seq {
        return Err(AccessError(format!(
            "stale revocation list: seq {} was already superseded by {} here",
            orl.seq, state.trusted_orgs[index].last_orl_seq
        )));
    }
    if orl.seq == state.trusted_orgs[index].last_orl_seq {
        return Ok(AppliedOrl {
            org_handle: handle,
            seq: orl.seq,
            revoked_grants: 0,
            revoked_peer_identities: 0,
            changed: false,
        });
    }

    let revoked_grant_ids: Vec<String> = orl
        .revoked_grant_ids
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    let revoked_subjects: Vec<String> = orl
        .revoked_subjects
        .iter()
        .map(|subject| normalize_client_key_fingerprint(subject))
        .filter(|subject| !subject.is_empty())
        .collect();
    let revoked_issuer_keys: Vec<String> = orl
        .revoked_issuer_keys
        .iter()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .collect();

    // Subjects resolve to principals via their client-key authn entries.
    let revoked_principals: Vec<String> = state
        .principals
        .iter()
        .filter(|principal| {
            principal.authn.iter().any(|authn| {
                authn.get("kind").and_then(|v| v.as_str()) == Some("client_key")
                    && authn
                        .get("fingerprint")
                        .and_then(|v| v.as_str())
                        .map(normalize_client_key_fingerprint)
                        .map(|fingerprint| revoked_subjects.contains(&fingerprint))
                        .unwrap_or(false)
            })
        })
        .map(|principal| principal.id.clone())
        .collect();

    // Peer identities materialized from this org are swept by the same
    // lists: subject fingerprints and document grant ids. This runs BEFORE
    // any state mutation and fails the whole apply on a write error —
    // advancing last_orl_seq past an unrecorded revocation would make the
    // idempotent re-apply a no-op and leave the peer approved forever.
    let mut revoked_peer_identities = 0;
    let identities = crate::access::access_policy::list_identities(cert_dir).map_err(|e| {
        AccessError(format!(
            "cannot apply org {handle} revocation list: peer identity store unreadable: {e}"
        ))
    })?;
    let now_unix = (now_unix_ms / 1000) as i64;
    let mut failed_peer_writes: Vec<String> = Vec::new();
    for mut record in identities {
        let from_org = record.source.as_deref() == Some(&format!("org:{handle}") as &str);
        let listed = revoked_subjects.iter().any(|s| s == &record.fingerprint)
            || record
                .org_grant_id
                .as_deref()
                .map(|id| revoked_grant_ids.iter().any(|g| g == id))
                .unwrap_or(false)
            || record
                .issued_via
                .as_deref()
                .map(|key| revoked_issuer_keys.iter().any(|k| k == key))
                .unwrap_or(false);
        if from_org
            && listed
            && matches!(
                record.status,
                crate::access::access_policy::PeerIdentityStatus::Approved
            )
        {
            let fingerprint = record.fingerprint.clone();
            record.status = crate::access::access_policy::PeerIdentityStatus::Revoked;
            record.revoked_at_unix = Some(now_unix);
            match crate::access::access_policy::write_identity_record(cert_dir, &record) {
                Ok(()) => revoked_peer_identities += 1,
                Err(e) => failed_peer_writes.push(format!("{fingerprint}: {e}")),
            }
        }
    }
    if !failed_peer_writes.is_empty() {
        return Err(AccessError(format!(
            "org {handle} revocation list seq {} was NOT recorded: {} peer identity record(s) could not be revoked ({}); fix the store and re-apply the list",
            orl.seq,
            failed_peer_writes.len(),
            failed_peer_writes.join(", ")
        )));
    }

    let source = format!("org:{handle}");
    let local_grant_ids: Vec<String> = revoked_grant_ids
        .iter()
        .map(|id| format!("grant:org:{handle}:{id}"))
        .collect();
    let mut revoked = 0;
    for grant in state.grants.iter_mut().filter(|grant| {
        grant.source == source
            && is_enforced_status(&grant.status)
            && (local_grant_ids.iter().any(|id| id == &grant.id)
                || revoked_principals
                    .iter()
                    .any(|id| id == &grant.principal_id)
                || grant
                    .issued_via
                    .as_deref()
                    .map(|key| revoked_issuer_keys.iter().any(|k| k == key))
                    .unwrap_or(false))
    }) {
        grant.status = "revoked".to_string();
        grant.revoked_at_unix_ms = Some(now_unix_ms);
        revoked += 1;
    }

    let entry = &mut state.trusted_orgs[index];
    entry.last_orl_seq = orl.seq;
    entry.orl_revoked_grant_ids = revoked_grant_ids;
    entry.orl_revoked_subjects = revoked_subjects;
    entry.orl_revoked_issuer_keys = revoked_issuer_keys;

    state.audit_events.push(IamAuditEvent {
        id: format!("audit:{now_unix_ms}:{}", state.audit_events.len() + 1),
        at_unix_ms: Some(now_unix_ms),
        actor_principal_id: format!("org:{handle}"),
        action: "apply_org_revocations".to_string(),
        target_id: format!("org:{handle}"),
        summary: format!(
            "Applied org {handle} revocation list seq {} ({} grants, {} peer identities revoked here)",
            orl.seq, revoked, revoked_peer_identities
        ),
    });

    Ok(AppliedOrl {
        org_handle: handle,
        seq: orl.seq,
        revoked_grants: revoked,
        revoked_peer_identities,
        changed: true,
    })
}

/// Org-daemon action: re-sign a still-valid document with a fresh window.
/// The `grant_id` deliberately stays stable so ORL revocation by grant_id
/// survives renewal; the original lifetime span is preserved (90d cap).
/// The org's own list gates renewal — a revoked grant or subject must age
/// out via expiry instead.
pub fn renew_org_grant(
    identity: &DaemonIdentity,
    orl: &OrgRevocationList,
    doc: &OrgGrantDocument,
    now_unix_ms: u64,
) -> Result<OrgGrantDocument, String> {
    verify_org_grant(doc, now_unix_ms)?;
    if doc.org.root_key != identity.public_key_b64u() {
        return Err(
            "this daemon does not hold the org root key that signed the document".to_string(),
        );
    }
    if doc.org.handle.trim() != orl.org.handle {
        return Err("document and revocation list belong to different orgs".to_string());
    }
    let doc_grant_id = doc.grant_id.trim();
    if orl.revoked_grant_ids.iter().any(|id| id == doc_grant_id) {
        return Err(format!(
            "org grant {doc_grant_id} is revoked; it cannot be renewed"
        ));
    }
    let fingerprint = subject_fingerprint(doc);
    if orl
        .revoked_subjects
        .iter()
        .any(|subject| subject == &fingerprint)
    {
        return Err("the subject key is revoked; the document cannot be renewed".to_string());
    }
    if let Some(cert) = doc.chain.first() {
        if orl
            .revoked_issuer_keys
            .iter()
            .any(|key| key == cert.issuer_key.trim())
        {
            return Err(
                "the issuer key that signed this document is revoked; it cannot be renewed"
                    .to_string(),
            );
        }
    }
    let span = doc
        .expires_at_unix_ms
        .saturating_sub(doc.issued_at_unix_ms)
        .min(MAX_ORG_GRANT_TTL_MS);
    let mut renewed = doc.clone();
    renewed.issued_at_unix_ms = now_unix_ms;
    renewed.expires_at_unix_ms = now_unix_ms.saturating_add(span);
    renewed.chain = Vec::new();
    renewed.sig = identity.sign_b64u(&org_grant_signing_payload(&renewed));
    Ok(renewed)
}

/// Deputy-side issuance: sign with a delegated issuer key and attach its
/// certificate as the one-link chain. The certificate's scope and window
/// are enforced by every verifier; a courtesy check here fails early.
pub fn issue_org_grant_via(
    issuer: &DaemonIdentity,
    cert: &OrgIssuerCert,
    state: &LocalIamState,
    request: IssueOrgGrantRequest<'_>,
    now_unix_ms: u64,
) -> AccessResult<OrgGrantDocument> {
    if cert.issuer_key.trim() != issuer.public_key_b64u() {
        return Err(AccessError(
            "the installed issuer certificate names a different key".to_string(),
        ));
    }
    verify_org_issuer_cert(cert, now_unix_ms).map_err(AccessError)?;
    // Sign a root-style document first (issue_org_grant validates the
    // request shape), then re-sign with the issuer key and the chain.
    let mut doc = issue_org_grant(issuer, state, request, now_unix_ms)?;
    doc.org = cert.org.clone();
    doc.chain = vec![cert.clone()];
    doc.sig = issuer.sign_b64u(&org_grant_signing_payload(&doc));
    verify_org_grant(&doc, now_unix_ms).map_err(AccessError)?;
    Ok(doc)
}

/// Root-session action: trust an org key on this daemon.
pub fn trust_org(
    state: &mut LocalIamState,
    handle: &str,
    root_key: &str,
    max_role: Option<&str>,
    max_peer_profile: Option<&str>,
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
    // Fail-closed default: no peer authority until the owner raises it.
    let max_peer_profile = max_peer_profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(|profile| {
            crate::access::access_policy::normalize_profile(
                profile.strip_prefix("peer:").unwrap_or(profile),
            )
            .map_err(|e| AccessError(e.to_string()))
        })
        .transpose()?
        .unwrap_or_default();
    let mut entry = TrustedOrg {
        handle: handle.clone(),
        root_key,
        max_role,
        max_peer_profile,
        status: "active".to_string(),
        added_at_unix_ms: Some(now_unix_ms),
        last_orl_seq: 0,
        orl_revoked_grant_ids: Vec::new(),
        orl_revoked_subjects: Vec::new(),
        orl_revoked_issuer_keys: Vec::new(),
    };
    if let Some(existing) = state
        .trusted_orgs
        .iter_mut()
        .find(|org| org.handle == handle)
    {
        // Re-trusting (e.g. to change the cap) keeps the applied
        // revocation state — it belongs to the key, so it only resets
        // when the key actually changes.
        if existing.root_key == entry.root_key {
            entry.last_orl_seq = existing.last_orl_seq;
            entry.orl_revoked_grant_ids = existing.orl_revoked_grant_ids.clone();
            entry.orl_revoked_subjects = existing.orl_revoked_subjects.clone();
            entry.orl_revoked_issuer_keys = existing.orl_revoked_issuer_keys.clone();
        }
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
        summary: format!("Trusted org {} with cap {}", entry.handle, entry.max_role),
    });
    Ok(entry)
}

/// Root-session action: revoke an org's trust and every grant it
/// materialized here. Local IAM always wins over org documents.
pub fn revoke_org(
    state: &mut LocalIamState,
    cert_dir: &Path,
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
    // Peer identities this org materialized go with it.
    if let Ok(identities) = crate::access::access_policy::list_identities(cert_dir) {
        let now_unix = (now_unix_ms / 1000) as i64;
        for mut record in identities {
            if record.source.as_deref() == Some(&source as &str)
                && matches!(
                    record.status,
                    crate::access::access_policy::PeerIdentityStatus::Approved
                )
            {
                record.status = crate::access::access_policy::PeerIdentityStatus::Revoked;
                record.revoked_at_unix = Some(now_unix);
                if crate::access::access_policy::write_identity_record(cert_dir, &record).is_ok() {
                    revoked += 1;
                }
            }
        }
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

    fn org_identity_with_dir() -> (tempfile::TempDir, DaemonIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let identity = load_or_create_org_identity(dir.path(), "acme").unwrap();
        (dir, identity)
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
                peer_fingerprint: "",
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
        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();
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
        assert!(
            crate::access::iam::evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::SessionInspect,
            )
            .allowed
        );
        assert!(
            !crate::access::iam::evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::access::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );

        // Re-presentation is idempotent (same grant id, refreshed record)
        // and quiet: offers attach documents on every connect, so an
        // unchanged presentation must not grow state or the audit log.
        assert!(outcome.changed);
        let audit_len = state.audit_events.len();
        let again = materialize_org_grant(
            &mut state,
            &doc,
            &["intendant:host-a".to_string()],
            test_now(),
        )
        .unwrap();
        assert_eq!(again.grant.id, outcome.grant.id);
        assert!(!again.changed);
        assert_eq!(state.audit_events.len(), audit_len);
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
    fn re_presentation_cannot_resurrect_local_revocations() {
        let identity = org_identity();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();
        let doc = issue(&identity, &state, "role:session-reader");
        let outcome =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap();

        // Local owner revokes the materialized grant: the same document
        // must not re-activate it (offers re-present automatically, so a
        // resurrecting upsert would undo the revocation within seconds).
        let grant = state
            .grants
            .iter_mut()
            .find(|grant| grant.id == outcome.grant.id)
            .unwrap();
        grant.status = "revoked".to_string();
        grant.revoked_at_unix_ms = Some(test_now());
        let err =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap_err();
        assert!(err.to_string().contains("revoked locally"), "{err}");

        // A locally revoked principal is refused the same way.
        let grant = state
            .grants
            .iter_mut()
            .find(|grant| grant.id == outcome.grant.id)
            .unwrap();
        grant.status = "active".to_string();
        grant.revoked_at_unix_ms = None;
        state
            .principals
            .iter_mut()
            .find(|principal| principal.id == outcome.principal.id)
            .unwrap()
            .status = "revoked".to_string();
        let err =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap_err();
        assert!(
            err.to_string().contains("principal was revoked locally"),
            "{err}"
        );
    }

    #[test]
    fn present_org_grant_state_parses_verifies_and_caps_size() {
        let identity = org_identity();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();
        let doc = issue(&identity, &state, "role:session-reader");
        let value = serde_json::to_value(&doc).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let outcome = present_org_grant_state(
            &mut state,
            dir.path(),
            &value,
            &["ignored".to_string()],
            test_now(),
        )
        .unwrap();
        assert_eq!(outcome.org_handle(), "acme");
        assert!(outcome.changed());

        // Malformed shape fails as a parse error, not a panic.
        let err = present_org_grant_state(
            &mut state,
            dir.path(),
            &serde_json::json!({"kind": "org-grant"}),
            &[],
            test_now(),
        )
        .unwrap_err();
        assert!(err.contains("invalid org grant document"), "{err}");

        // Oversized documents are refused before parsing.
        let mut huge = value.clone();
        huge["sig"] = serde_json::Value::String("x".repeat(MAX_ORG_GRANT_DOC_BYTES + 1));
        let err =
            present_org_grant_state(&mut state, dir.path(), &huge, &[], test_now()).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn org_target_daemon_ids_merges_extras_with_host_forms() {
        let host_label = crate::access::resolve_host_label();
        let extras = vec!["connect-id-1".to_string(), host_label.clone()];
        let ids = org_target_daemon_ids(&extras);
        assert_eq!(ids[0], "connect-id-1");
        assert!(ids.contains(&host_label));
        assert!(ids.contains(&format!("intendant:{host_label}")));
        // Dedup: the host label passed as an extra appears once.
        assert_eq!(ids.iter().filter(|id| **id == host_label).count(), 1);
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

        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();

        // Tampered role: signature fails.
        let mut tampered = doc.clone();
        tampered.role_id = "role:root".to_string();
        assert!(verify_org_grant(&tampered, test_now())
            .unwrap_err()
            .contains("signature"));

        // Role above the org cap (default operator): rejected, not downgraded.
        let root_doc = issue(&identity, &state, "role:root");
        let err = materialize_org_grant(&mut state, &root_doc, &ids, test_now()).unwrap_err();
        assert!(
            err.to_string().contains("exceeds this daemon's cap"),
            "{err}"
        );

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
    fn orl_applies_monotonically_and_blocks_rematerialization() {
        let (dir, identity) = org_identity_with_dir();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();
        let doc = issue(&identity, &state, "role:session-reader");
        let outcome =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap();
        assert_eq!(outcome.grant.status, "active");

        // Fresh org daemons serve a signed empty seq-0 list.
        let empty = load_or_init_orl(&identity, dir.path(), "acme", test_now()).unwrap();
        assert_eq!(empty.seq, 0);
        verify_orl(&empty).unwrap();

        // Revoke the document by grant_id: seq bumps, list persists.
        let orl = orl_revoke(
            &identity,
            dir.path(),
            "acme",
            &[doc.grant_id.clone()],
            &[],
            &[],
            test_now(),
        )
        .unwrap();
        assert_eq!(orl.seq, 1);
        verify_orl(&orl).unwrap();
        let reloaded = load_or_init_orl(&identity, dir.path(), "acme", test_now()).unwrap();
        assert_eq!(reloaded, orl);

        // Tampering is caught by the signature.
        let mut tampered = orl.clone();
        tampered.revoked_grant_ids.pop();
        assert!(verify_orl(&tampered).unwrap_err().contains("signature"));

        // Applying revokes the materialized grant and persists the lists.
        let applied = apply_orl(&mut state, dir.path(), &orl, test_now()).unwrap();
        assert!(applied.changed);
        assert_eq!(applied.revoked_grants, 1);
        let grant = state
            .grants
            .iter()
            .find(|grant| grant.id == outcome.grant.id)
            .unwrap();
        assert_eq!(grant.status, "revoked");
        let trusted = &state.trusted_orgs[0];
        assert_eq!(trusted.last_orl_seq, 1);
        assert_eq!(trusted.orl_revoked_grant_ids, vec![doc.grant_id.clone()]);

        // Same seq again: idempotent no-op. A member re-presenting the
        // still-signed document is refused by the persisted list.
        let again = apply_orl(&mut state, dir.path(), &orl, test_now()).unwrap();
        assert!(!again.changed);
        let err =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap_err();
        assert!(err.to_string().contains("revocation list"), "{err}");

        // Subject revocation sweeps by fingerprint and blocks new docs for
        // that key even with fresh grant_ids.
        let doc2 = issue(&identity, &state, "role:observer");
        // (issue() binds subject "member-key"; doc2 has a new grant_id.)
        let orl2 = orl_revoke(
            &identity,
            dir.path(),
            "acme",
            &[],
            &["member-key".to_string()],
            &[],
            test_now(),
        )
        .unwrap();
        assert_eq!(orl2.seq, 2);
        apply_orl(&mut state, dir.path(), &orl2, test_now()).unwrap();
        let err =
            materialize_org_grant(&mut state, &doc2, &["*".to_string()], test_now()).unwrap_err();
        assert!(err.to_string().contains("subject key is revoked"), "{err}");

        // A stale (superseded) list is refused loudly.
        let err = apply_orl(&mut state, dir.path(), &orl, test_now()).unwrap_err();
        assert!(err.to_string().contains("stale"), "{err}");

        // Untrusting states refuse the list entirely.
        let mut fresh = LocalIamState::default();
        let err = apply_orl(&mut fresh, dir.path(), &orl2, test_now()).unwrap_err();
        assert!(err.to_string().contains("does not trust org"), "{err}");

        // Re-trusting with the same key preserves the applied ORL state;
        // a changed key resets it.
        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            Some("role:terminal"),
            None,
            test_now(),
        )
        .unwrap();
        assert_eq!(state.trusted_orgs[0].last_orl_seq, 2);
        assert!(!state.trusted_orgs[0].orl_revoked_subjects.is_empty());
        let other = org_identity();
        trust_org(
            &mut state,
            "acme",
            &other.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();
        assert_eq!(state.trusted_orgs[0].last_orl_seq, 0);
        assert!(state.trusted_orgs[0].orl_revoked_grant_ids.is_empty());
    }

    #[test]
    fn renewal_keeps_grant_id_and_respects_the_orl() {
        let (dir, identity) = org_identity_with_dir();
        let state = LocalIamState::default();
        let doc = issue(&identity, &state, "role:session-reader");
        let orl = load_or_init_orl(&identity, dir.path(), "acme", test_now()).unwrap();

        let later = test_now() + 1000;
        let renewed = renew_org_grant(&identity, &orl, &doc, later).unwrap();
        assert_eq!(renewed.grant_id, doc.grant_id, "grant_id must stay stable");
        assert_eq!(renewed.subject, doc.subject);
        assert_eq!(renewed.role_id, doc.role_id);
        assert_eq!(renewed.issued_at_unix_ms, later);
        assert_eq!(
            renewed.expires_at_unix_ms - renewed.issued_at_unix_ms,
            doc.expires_at_unix_ms - doc.issued_at_unix_ms,
            "original lifetime span is preserved"
        );
        verify_org_grant(&renewed, later).unwrap();

        // The org's own list gates renewal, by grant_id and by subject.
        let orl = orl_revoke(
            &identity,
            dir.path(),
            "acme",
            &[doc.grant_id.clone()],
            &[],
            &[],
            test_now(),
        )
        .unwrap();
        let err = renew_org_grant(&identity, &orl, &doc, later).unwrap_err();
        assert!(err.contains("revoked"), "{err}");
        let doc2 = issue(&identity, &state, "role:observer");
        let orl = orl_revoke(
            &identity,
            dir.path(),
            "acme",
            &[],
            &["member-key".to_string()],
            &[],
            test_now(),
        )
        .unwrap();
        let err = renew_org_grant(&identity, &orl, &doc2, later).unwrap_err();
        assert!(err.contains("subject key is revoked"), "{err}");

        // Only the signing key's daemon can renew.
        let stranger = org_identity();
        let strangers_orl = OrgRevocationList {
            org: OrgGrantOrg {
                handle: "acme".to_string(),
                root_key: stranger.public_key_b64u(),
            },
            ..orl.clone()
        };
        let err = renew_org_grant(&stranger, &strangers_orl, &doc2, later).unwrap_err();
        assert!(err.contains("does not hold the org root key"), "{err}");
    }

    #[test]
    fn peer_subject_documents_fail_closed_cap_and_materialize_into_peer_store() {
        let (dir, identity) = org_identity_with_dir();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();
        let peer_fp = "aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66";
        let issue_peer = |state: &LocalIamState, role: &str| {
            issue_org_grant(
                &identity,
                state,
                IssueOrgGrantRequest {
                    handle: "acme",
                    client_key_fingerprint: "",
                    peer_fingerprint: peer_fp,
                    subject_label: "Build daemon",
                    role_id: role,
                    targets: vec!["*".to_string()],
                    ttl_ms: None,
                },
                test_now(),
            )
        };

        // Peer docs must use the peer:<profile> namespace; the payload kind
        // line binds the subject kind into the signature.
        assert!(issue_peer(&state, "role:observer").is_err());
        let doc = issue_peer(&state, "peer:session-reader").unwrap();
        verify_org_grant(&doc, test_now()).unwrap();
        let mut cross = doc.clone();
        cross.subject.client_key_fingerprint = cross.subject.peer_fingerprint.clone();
        cross.subject.peer_fingerprint = String::new();
        assert!(verify_org_grant(&cross, test_now())
            .unwrap_err()
            .contains("signature"));

        // Fail closed: no peer cap, no peer authority.
        let ids = ["*".to_string()];
        let err =
            materialize_org_peer_grant(&mut state, dir.path(), &doc, &ids, test_now()).unwrap_err();
        assert!(err.to_string().contains("no peer authority"), "{err}");

        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            Some("session-reader"),
            test_now(),
        )
        .unwrap();

        // Over-cap profile is rejected, not downgraded.
        let op_doc = issue_peer(&state, "peer:operator").unwrap();
        let err = materialize_org_peer_grant(&mut state, dir.path(), &op_doc, &ids, test_now())
            .unwrap_err();
        assert!(
            err.to_string().contains("exceeds this daemon's peer cap"),
            "{err}"
        );

        // In-cap materializes into the peer identity store with expiry,
        // provenance, and grant id; re-presentation is a quiet no-op.
        let outcome =
            materialize_org_peer_grant(&mut state, dir.path(), &doc, &ids, test_now()).unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.record.profile, "session-reader");
        assert_eq!(outcome.record.source.as_deref(), Some("org:acme"));
        assert_eq!(
            outcome.record.org_grant_id.as_deref(),
            Some(doc.grant_id.as_str())
        );
        assert!(outcome.record.is_active((test_now() / 1000) as i64));
        let audit_len = state.audit_events.len();
        let again =
            materialize_org_peer_grant(&mut state, dir.path(), &doc, &ids, test_now()).unwrap();
        assert!(!again.changed);
        assert_eq!(state.audit_events.len(), audit_len);

        // ORL subject revocation sweeps the record and blocks re-presentation.
        let orl = orl_revoke(
            &identity,
            dir.path(),
            "acme",
            &[],
            &[peer_fp.to_string()],
            &[],
            test_now(),
        )
        .unwrap();
        let applied = apply_orl(&mut state, dir.path(), &orl, test_now()).unwrap();
        assert_eq!(applied.revoked_peer_identities, 1);
        let record = crate::access::access_policy::lookup_identity(dir.path(), peer_fp)
            .unwrap()
            .unwrap();
        assert!(!record.is_active((test_now() / 1000) as i64));
        let err =
            materialize_org_peer_grant(&mut state, dir.path(), &doc, &ids, test_now()).unwrap_err();
        assert!(err.to_string().contains("revocation list"), "{err}");
    }

    #[test]
    fn issuer_delegation_signs_verifies_scopes_and_revokes() {
        let (dir, root) = org_identity_with_dir();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &root.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();

        let issuer_dir = tempfile::tempdir().unwrap();
        let issuer = DaemonIdentity::load_or_create(issuer_dir.path().join("issuer.pk8")).unwrap();
        let cert = delegate_org_issuer(
            &root,
            "acme",
            &issuer.public_key_b64u(),
            "CI signer",
            "",
            None,
            test_now(),
        )
        .unwrap();
        verify_org_issuer_cert(&cert, test_now()).unwrap();
        // Tamper: scope change breaks the root signature.
        let mut tampered = cert.clone();
        tampered.max_role = "role:root".to_string();
        assert!(verify_org_issuer_cert(&tampered, test_now())
            .unwrap_err()
            .contains("signature"));
        // The root cannot delegate to itself.
        assert!(delegate_org_issuer(
            &root,
            "acme",
            &root.public_key_b64u(),
            "",
            "",
            None,
            test_now()
        )
        .is_err());

        // Issuer-signed document verifies via the chain and materializes
        // with the issuer recorded.
        let doc = issue_org_grant_via(
            &issuer,
            &cert,
            &state,
            IssueOrgGrantRequest {
                handle: "acme",
                client_key_fingerprint: "member-key",
                peer_fingerprint: "",
                subject_label: "Alice",
                role_id: "role:session-reader",
                targets: vec!["*".to_string()],
                ttl_ms: None,
            },
            test_now(),
        )
        .unwrap();
        assert_eq!(doc.chain.len(), 1);
        verify_org_grant(&doc, test_now()).unwrap();
        let outcome =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap();
        assert_eq!(
            outcome.grant.issued_via.as_deref(),
            Some(issuer.public_key_b64u().as_str())
        );
        // A stranger key with the same cert is refused (sig mismatch).
        let stranger = org_identity();
        let mut forged = doc.clone();
        forged.sig = stranger.sign_b64u(&org_grant_signing_payload(&forged));
        assert!(verify_org_grant(&forged, test_now())
            .unwrap_err()
            .contains("signature"));

        // Peer-scoped issuers cannot sign human documents.
        let peer_scoped = delegate_org_issuer(
            &root,
            "acme",
            &issuer.public_key_b64u(),
            "peer signer",
            "peer:session-reader",
            None,
            test_now(),
        )
        .unwrap();
        let mut wrong_kind = doc.clone();
        wrong_kind.chain = vec![peer_scoped];
        wrong_kind.sig = issuer.sign_b64u(&org_grant_signing_payload(&wrong_kind));
        assert!(verify_org_grant(&wrong_kind, test_now())
            .unwrap_err()
            .contains("peer-subject documents"));

        // Revoking the issuer sweeps its grants and blocks new documents
        // and renewals wholesale.
        let orl = orl_revoke(
            &identity_ref(&root, dir.path()),
            dir.path(),
            "acme",
            &[],
            &[],
            &[issuer.public_key_b64u()],
            test_now(),
        )
        .unwrap();
        let applied = apply_orl(&mut state, dir.path(), &orl, test_now()).unwrap();
        assert_eq!(applied.revoked_grants, 1);
        let err =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap_err();
        assert!(err.to_string().contains("issuer key"), "{err}");
        let err = renew_org_grant(&root, &orl, &doc, test_now() + 1000).unwrap_err();
        assert!(err.contains("issuer key"), "{err}");
    }

    fn identity_ref<'a>(identity: &'a DaemonIdentity, _dir: &Path) -> &'a DaemonIdentity {
        identity
    }

    #[test]
    fn issuer_role_scope_caps_materialization() {
        let root = org_identity();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &root.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();

        let issuer_dir = tempfile::tempdir().unwrap();
        let issuer = DaemonIdentity::load_or_create(issuer_dir.path().join("issuer.pk8")).unwrap();
        let cert = delegate_org_issuer(
            &root,
            "acme",
            &issuer.public_key_b64u(),
            "readers only",
            "role:session-reader",
            None,
            test_now(),
        )
        .unwrap();

        // A document within the issuer's scope materializes.
        let within = issue_org_grant_via(
            &issuer,
            &cert,
            &state,
            IssueOrgGrantRequest {
                handle: "acme",
                client_key_fingerprint: "member-key",
                peer_fingerprint: "",
                subject_label: "Alice",
                role_id: "role:session-reader",
                targets: vec!["*".to_string()],
                ttl_ms: None,
            },
            test_now(),
        )
        .unwrap();
        materialize_org_grant(&mut state, &within, &["*".to_string()], test_now()).unwrap();

        // A document for a broader role verifies (the payload is honestly
        // signed) but must be refused at materialization: the issuer's
        // scope caps what it may sign even under a permissive org cap.
        let above = issue_org_grant_via(
            &issuer,
            &cert,
            &state,
            IssueOrgGrantRequest {
                handle: "acme",
                client_key_fingerprint: "other-key",
                peer_fingerprint: "",
                subject_label: "Mallory",
                role_id: "role:operator",
                targets: vec!["*".to_string()],
                ttl_ms: None,
            },
            test_now(),
        )
        .unwrap();
        verify_org_grant(&above, test_now()).unwrap();
        let err =
            materialize_org_grant(&mut state, &above, &["*".to_string()], test_now()).unwrap_err();
        assert!(
            err.to_string().contains("exceeds the issuer's scope"),
            "{err}"
        );
        // An unknown scope fails closed rather than silently uncapping.
        let ghost_cert = delegate_org_issuer(
            &root,
            "acme",
            &issuer.public_key_b64u(),
            "ghost scope",
            "role:does-not-exist",
            None,
            test_now(),
        )
        .unwrap();
        let mut ghost = within.clone();
        ghost.chain = vec![ghost_cert];
        ghost.sig = issuer.sign_b64u(&org_grant_signing_payload(&ghost));
        let err =
            materialize_org_grant(&mut state, &ghost, &["*".to_string()], test_now()).unwrap_err();
        assert!(err.to_string().contains("failing closed"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn orl_apply_does_not_advance_seq_past_failed_peer_revocations() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, root) = org_identity_with_dir();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &root.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();

        // An approved org-materialized peer identity that the list revokes.
        let record = crate::access::access_policy::PeerIdentityRecord {
            version: 1,
            fingerprint: "aabbccdd".to_string(),
            label: "peer".to_string(),
            profile: "session-reader".to_string(),
            status: crate::access::access_policy::PeerIdentityStatus::Approved,
            card_url: None,
            request_id: None,
            filesystem: Default::default(),
            created_at_unix: 0,
            revoked_at_unix: None,
            expires_at_unix: None,
            source: Some("org:acme".to_string()),
            org_grant_id: Some("g-1".to_string()),
            issued_via: None,
        };
        crate::access::access_policy::write_identity_record(dir.path(), &record).unwrap();

        let orl = orl_revoke(
            &root,
            dir.path(),
            "acme",
            &["g-1".to_string()],
            &[],
            &[],
            test_now(),
        )
        .unwrap();

        // Make the identity record unwritable (the file, not the directory —
        // truncating an existing file never consults directory permissions):
        // the apply must fail loudly and must NOT advance the org's
        // revocation sequence, so a later re-apply still performs the sweep.
        let record_path = dir
            .path()
            .join("peer-access-identities")
            .join("aabbccdd.json");
        let writable = std::fs::metadata(&record_path).unwrap().permissions();
        let mut readonly = writable.clone();
        readonly.set_mode(0o444);
        std::fs::set_permissions(&record_path, readonly).unwrap();
        let err = apply_orl(&mut state, dir.path(), &orl, test_now()).unwrap_err();
        std::fs::set_permissions(&record_path, writable).unwrap();
        assert!(err.to_string().contains("NOT recorded"), "{err}");
        assert_eq!(state.trusted_orgs[0].last_orl_seq, 0);

        // Once the store is writable the same list applies cleanly.
        let applied = apply_orl(&mut state, dir.path(), &orl, test_now()).unwrap();
        assert_eq!(applied.revoked_peer_identities, 1);
        assert_eq!(state.trusted_orgs[0].last_orl_seq, orl.seq);
    }

    #[test]
    fn revoking_org_trust_revokes_its_materialized_grants() {
        let identity = org_identity();
        let mut state = LocalIamState::default();
        trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            test_now(),
        )
        .unwrap();
        let doc = issue(&identity, &state, "role:observer");
        materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap();
        assert!(
            crate::access::iam::principal_for_client_key(&state, "member-key", "test").is_some()
        );

        let revoked = revoke_org(
            &mut state,
            tempfile::tempdir().unwrap().path(),
            "acme",
            test_now(),
        )
        .unwrap();
        assert_eq!(revoked, 1);
        assert!(
            crate::access::iam::principal_for_client_key(&state, "member-key", "test").is_none()
        );
        // And new presentations are refused.
        let err =
            materialize_org_grant(&mut state, &doc, &["*".to_string()], test_now()).unwrap_err();
        assert!(err.to_string().contains("does not trust org acme"));
    }
}
