use passkey_auth::{
    Attachment, AuthenticationResponse, AuthenticationState, CredentialId, PasskeyCredential,
    RegistrationResponse, RegistrationState, Webauthn,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::access::hosted_control::{
    HostedControlRuntime, HostedLeaseDecisionInput, HostedLeaseDocument, HostedLeaseRequestInput,
};
use crate::access::iam::AccessPrincipal;
use crate::project::ValidatedCustomDomain;

const STORE_FILE: &str = "custom-domain-passkeys.json";
const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_MAX_BYTES: u64 = 1024 * 1024;
const FLOW_TTL_MS: u64 = 5 * 60 * 1000;
const INVITE_TTL_MS: u64 = 10 * 60 * 1000;
const MAX_PENDING_FLOWS: usize = 64;
const MAX_PASSKEYS: usize = 32;
const AUTH_START_WINDOW_MS: u64 = 60_000;
const AUTH_STARTS_PER_WINDOW: usize = 60;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PasskeyView {
    pub credential_id: String,
    pub label: String,
    pub created_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_unix_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CeremonyStart {
    pub ok: bool,
    pub flow_id: String,
    pub options: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct EnrollmentInvite {
    pub ok: bool,
    pub expires_unix_ms: u64,
    pub enrollment_url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct PasskeyLeaseResult {
    pub ok: bool,
    pub lease: HostedLeaseDocument,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistrationInviteInput {
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistrationStartInput {
    pub token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistrationFinishInput {
    pub flow_id: String,
    pub credential: RegistrationResponse,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthenticationStartInput {
    pub request: HostedLeaseRequestInput,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthenticationFinishInput {
    pub flow_id: String,
    pub credential: AuthenticationResponse,
    pub nonce: String,
    pub timestamp_unix_ms: i64,
    pub signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RevokeInput {
    pub credential_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredPasskey {
    credential: PasskeyCredential,
    label: String,
    created_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PasskeyStore {
    schema_version: u32,
    name: String,
    rp_id: String,
    user_id: Uuid,
    passkeys: Vec<StoredPasskey>,
}

struct PendingRegistration {
    label: String,
    state: RegistrationState,
    expires_unix_ms: u64,
}

struct PendingInvitation {
    label: String,
    expires_unix_ms: u64,
}

struct PendingAuthentication {
    state: AuthenticationState,
    input: HostedLeaseRequestInput,
    source_bucket: Option<String>,
    expires_unix_ms: u64,
}

pub(crate) struct PasskeyRuntime {
    domain: ValidatedCustomDomain,
    cert_dir: PathBuf,
    webauthn: Webauthn,
    store: Mutex<PasskeyStore>,
    invitations: Mutex<HashMap<String, PendingInvitation>>,
    registrations: Mutex<HashMap<String, PendingRegistration>>,
    authentications: Mutex<HashMap<String, PendingAuthentication>>,
    authentication_starts: Mutex<VecDeque<u64>>,
    hosted: Arc<HostedControlRuntime>,
}

impl std::fmt::Debug for PasskeyRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PasskeyRuntime")
            .field("name", &self.domain.name)
            .field("rp_id", &self.domain.rp_id)
            .field("cert_dir", &self.cert_dir)
            .finish_non_exhaustive()
    }
}

impl PasskeyRuntime {
    pub(crate) fn new(
        domain: ValidatedCustomDomain,
        cert_dir: PathBuf,
        hosted: Arc<HostedControlRuntime>,
    ) -> Result<Self, String> {
        let store = load_store(&cert_dir, &domain)?;
        let webauthn = Webauthn::new(&domain.rp_id, "Intendant", &domain.origin)
            .require_user_verification(true)
            .authenticator_attachment(Attachment::Any)
            .strict_base64(true);
        Ok(Self {
            domain,
            cert_dir,
            webauthn,
            store: Mutex::new(store),
            invitations: Mutex::new(HashMap::new()),
            registrations: Mutex::new(HashMap::new()),
            authentications: Mutex::new(HashMap::new()),
            authentication_starts: Mutex::new(VecDeque::new()),
            hosted,
        })
    }

    pub(crate) fn views(&self) -> Result<Vec<PasskeyView>, String> {
        let store = self
            .store
            .lock()
            .map_err(|_| "custom-domain passkey store is unavailable".to_string())?;
        Ok(store
            .passkeys
            .iter()
            .map(|passkey| PasskeyView {
                credential_id: passkey.credential.id.to_b64url(),
                label: passkey.label.clone(),
                created_unix_ms: passkey.created_unix_ms,
                last_used_unix_ms: passkey.last_used_unix_ms,
            })
            .collect())
    }

    pub(crate) fn registration_invite(
        &self,
        input: RegistrationInviteInput,
    ) -> Result<EnrollmentInvite, String> {
        let label = normalized_label(&input.label)?;
        {
            let store = self
                .store
                .lock()
                .map_err(|_| "custom-domain passkey store is unavailable".to_string())?;
            if store.passkeys.len() >= MAX_PASSKEYS {
                return Err("custom-domain passkey limit reached".to_string());
            }
        }
        let token = Uuid::new_v4().simple().to_string();
        let now = now_unix_ms();
        let expires_unix_ms = now.saturating_add(INVITE_TTL_MS);
        let mut invitations = self
            .invitations
            .lock()
            .map_err(|_| "custom-domain enrollment invitation state is unavailable".to_string())?;
        retain_live(&mut invitations, now, |invite| invite.expires_unix_ms);
        if invitations.len() >= MAX_PENDING_FLOWS {
            return Err("too many pending custom-domain enrollment invitations".to_string());
        }
        invitations.insert(
            token.clone(),
            PendingInvitation {
                label,
                expires_unix_ms,
            },
        );
        Ok(EnrollmentInvite {
            ok: true,
            enrollment_url: format!("{}#passkey_enroll={token}", self.domain.origin),
            expires_unix_ms,
        })
    }

    pub(crate) fn registration_start(
        &self,
        input: RegistrationStartInput,
        origin: &str,
    ) -> Result<CeremonyStart, String> {
        self.require_origin(origin)?;
        if input.token.len() > 64 {
            return Err("passkey enrollment invitation is invalid".to_string());
        }
        let invitation = self
            .invitations
            .lock()
            .map_err(|_| "custom-domain enrollment invitation state is unavailable".to_string())?
            .remove(input.token.trim())
            .ok_or_else(|| "passkey enrollment invitation was not found".to_string())?;
        if invitation.expires_unix_ms <= now_unix_ms() {
            return Err("passkey enrollment invitation expired".to_string());
        }
        let label = invitation.label;
        let (user_id, exclude) = {
            let store = self
                .store
                .lock()
                .map_err(|_| "custom-domain passkey store is unavailable".to_string())?;
            if store.passkeys.len() >= MAX_PASSKEYS {
                return Err("custom-domain passkey limit reached".to_string());
            }
            (
                store.user_id,
                store
                    .passkeys
                    .iter()
                    .map(|passkey| passkey.credential.id.clone())
                    .collect::<Vec<_>>(),
            )
        };
        let (options, state) = self.webauthn.start_registration(
            user_id.as_bytes(),
            &self.domain.name,
            &label,
            &exclude,
        );
        let flow_id = Uuid::new_v4().to_string();
        let now = now_unix_ms();
        let mut pending = self
            .registrations
            .lock()
            .map_err(|_| "custom-domain registration state is unavailable".to_string())?;
        retain_live(&mut pending, now, |flow| flow.expires_unix_ms);
        if pending.len() >= MAX_PENDING_FLOWS {
            return Err("too many pending custom-domain registration ceremonies".to_string());
        }
        pending.insert(
            flow_id.clone(),
            PendingRegistration {
                label,
                state,
                expires_unix_ms: now.saturating_add(FLOW_TTL_MS),
            },
        );
        Ok(CeremonyStart {
            ok: true,
            flow_id,
            options: serde_json::to_value(options)
                .map_err(|error| format!("serialize passkey registration options: {error}"))?,
        })
    }

    pub(crate) fn registration_finish(
        &self,
        input: RegistrationFinishInput,
    ) -> Result<PasskeyView, String> {
        let pending = self
            .registrations
            .lock()
            .map_err(|_| "custom-domain registration state is unavailable".to_string())?
            .remove(input.flow_id.trim())
            .ok_or_else(|| "passkey registration flow was not found".to_string())?;
        if pending.expires_unix_ms <= now_unix_ms() {
            return Err("passkey registration flow expired".to_string());
        }
        let credential = self
            .webauthn
            .finish_registration(&pending.state, &input.credential)
            .map_err(|error| format!("finish passkey registration: {error}"))?;
        let now = now_unix_ms();
        let view = PasskeyView {
            credential_id: credential.id.to_b64url(),
            label: pending.label.clone(),
            created_unix_ms: now,
            last_used_unix_ms: None,
        };
        let mut store = self
            .store
            .lock()
            .map_err(|_| "custom-domain passkey store is unavailable".to_string())?;
        if store
            .passkeys
            .iter()
            .any(|stored| stored.credential.id == credential.id)
        {
            return Err("passkey is already registered".to_string());
        }
        if store.passkeys.len() >= MAX_PASSKEYS {
            return Err("custom-domain passkey limit reached".to_string());
        }
        let mut next = store.clone();
        next.passkeys.push(StoredPasskey {
            credential,
            label: pending.label,
            created_unix_ms: now,
            last_used_unix_ms: None,
        });
        persist_store(&self.cert_dir, &next)?;
        *store = next;
        Ok(view)
    }

    pub(crate) fn authentication_start(
        &self,
        input: AuthenticationStartInput,
        origin: &str,
        source_bucket: Option<&str>,
    ) -> Result<CeremonyStart, String> {
        self.require_origin(origin)?;
        validate_pending_request_shape(&input.request)?;
        let now = now_unix_ms();
        {
            let mut starts = self.authentication_starts.lock().map_err(|_| {
                "custom-domain authentication rate state is unavailable".to_string()
            })?;
            while starts
                .front()
                .is_some_and(|started| now.saturating_sub(*started) >= AUTH_START_WINDOW_MS)
            {
                starts.pop_front();
            }
            if starts.len() >= AUTH_STARTS_PER_WINDOW {
                return Err("custom-domain passkey ceremony rate limit reached".to_string());
            }
            starts.push_back(now);
        }
        let (user_id, credentials) = {
            let store = self
                .store
                .lock()
                .map_err(|_| "custom-domain passkey store is unavailable".to_string())?;
            if store.passkeys.is_empty() {
                return Err("no passkey is registered for this custom domain".to_string());
            }
            (
                store.user_id,
                store
                    .passkeys
                    .iter()
                    .map(|passkey| passkey.credential.clone())
                    .collect::<Vec<_>>(),
            )
        };
        let (options, state) = self
            .webauthn
            .start_authentication_with_creds_for_user(user_id.as_bytes(), &credentials);
        let flow_id = Uuid::new_v4().to_string();
        let mut pending = self
            .authentications
            .lock()
            .map_err(|_| "custom-domain authentication state is unavailable".to_string())?;
        retain_live(&mut pending, now, |flow| flow.expires_unix_ms);
        if pending.len() >= MAX_PENDING_FLOWS {
            return Err("too many pending custom-domain authentication ceremonies".to_string());
        }
        pending.insert(
            flow_id.clone(),
            PendingAuthentication {
                state,
                input: input.request,
                source_bucket: source_bucket.map(str::to_string),
                expires_unix_ms: now.saturating_add(FLOW_TTL_MS),
            },
        );
        Ok(CeremonyStart {
            ok: true,
            flow_id,
            options: serde_json::to_value(options)
                .map_err(|error| format!("serialize passkey authentication options: {error}"))?,
        })
    }

    pub(crate) fn authentication_finish(
        &self,
        input: AuthenticationFinishInput,
        origin: &str,
    ) -> Result<PasskeyLeaseResult, String> {
        self.require_origin(origin)?;
        let mut pending = self
            .authentications
            .lock()
            .map_err(|_| "custom-domain authentication state is unavailable".to_string())?
            .remove(input.flow_id.trim())
            .ok_or_else(|| "passkey authentication flow was not found".to_string())?;
        if pending.expires_unix_ms <= now_unix_ms() {
            return Err("passkey authentication flow expired".to_string());
        }
        pending.input.nonce = input.nonce;
        pending.input.timestamp_unix_ms = input.timestamp_unix_ms;
        pending.input.signature = input.signature;
        validate_pending_request_shape(&pending.input)?;
        let credential_id = CredentialId::from_b64url(&input.credential.id)
            .map_err(|error| format!("passkey credential id: {error}"))?;
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| "custom-domain passkey store is unavailable".to_string())?;
            let mut next = store.clone();
            let stored = next
                .passkeys
                .iter_mut()
                .find(|passkey| passkey.credential.id == credential_id)
                .ok_or_else(|| "passkey is not registered for this custom domain".to_string())?;
            let result = self
                .webauthn
                .finish_authentication(&pending.state, &input.credential, &stored.credential)
                .map_err(|error| format!("finish passkey authentication: {error}"))?;
            stored.credential.counter = result.new_counter;
            stored.last_used_unix_ms = Some(now_unix_ms());
            persist_store(&self.cert_dir, &next)?;
            *store = next;
        }

        let request =
            self.hosted
                .create_request(pending.input, origin, pending.source_bucket.as_deref())?;
        let actor = passkey_actor(&credential_id);
        let lease = self
            .hosted
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: request.request_id,
                    approve: true,
                    approved_preset: Some(request.requested_preset),
                    approved_ttl_secs: Some(request.requested_ttl_secs),
                },
                &actor,
            )?
            .ok_or_else(|| "passkey-approved lease was not issued".to_string())?;
        Ok(PasskeyLeaseResult { ok: true, lease })
    }

    pub(crate) fn revoke(&self, input: RevokeInput) -> Result<bool, String> {
        let credential_id = CredentialId::from_b64url(input.credential_id.trim())
            .map_err(|error| format!("passkey credential id: {error}"))?;
        let mut store = self
            .store
            .lock()
            .map_err(|_| "custom-domain passkey store is unavailable".to_string())?;
        let mut next = store.clone();
        let before = next.passkeys.len();
        next.passkeys
            .retain(|passkey| passkey.credential.id != credential_id);
        if next.passkeys.len() == before {
            return Ok(false);
        }
        persist_store(&self.cert_dir, &next)?;
        *store = next;
        Ok(true)
    }

    fn require_origin(&self, origin: &str) -> Result<(), String> {
        if origin == self.domain.origin {
            Ok(())
        } else {
            Err("passkey ceremony origin does not match the custom domain".to_string())
        }
    }
}

fn normalized_label(value: &str) -> Result<String, String> {
    let label = value.trim();
    let label = if label.is_empty() {
        "Owner passkey"
    } else {
        label
    };
    if label.len() > 96 || label.chars().any(char::is_control) {
        return Err("passkey label must contain at most 96 printable characters".to_string());
    }
    Ok(label.to_string())
}

fn validate_pending_request_shape(input: &HostedLeaseRequestInput) -> Result<(), String> {
    if input.browser_public_key.len() > 256
        || input.signature.len() > 512
        || input.nonce.len() > 128
        || input.requester_label.len() > 96
        || input.requester_label.chars().any(char::is_control)
    {
        return Err("custom-domain lease request fields exceed their bounds".to_string());
    }
    Ok(())
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

fn retain_live<T>(pending: &mut HashMap<String, T>, now: u64, expires: impl Fn(&T) -> u64) {
    pending.retain(|_, flow| expires(flow) > now);
}

fn store_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(STORE_FILE)
}

fn load_store(cert_dir: &Path, domain: &ValidatedCustomDomain) -> Result<PasskeyStore, String> {
    let path = store_path(cert_dir);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PasskeyStore {
                schema_version: STORE_SCHEMA_VERSION,
                name: domain.name.clone(),
                rp_id: domain.rp_id.clone(),
                user_id: Uuid::new_v4(),
                passkeys: Vec::new(),
            });
        }
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if metadata.len() > STORE_MAX_BYTES {
        return Err(format!(
            "{} exceeds the passkey-store size cap",
            path.display()
        ));
    }
    let bytes =
        std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let store: PasskeyStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if store.schema_version != STORE_SCHEMA_VERSION {
        return Err(format!(
            "{} uses unsupported schema version {}",
            path.display(),
            store.schema_version
        ));
    }
    if store.name != domain.name || store.rp_id != domain.rp_id {
        return Err(
            "stored passkeys belong to a different custom-domain name or rp_id".to_string(),
        );
    }
    if store.passkeys.len() > MAX_PASSKEYS {
        return Err("stored custom-domain passkey count exceeds the limit".to_string());
    }
    Ok(store)
}

fn persist_store(cert_dir: &Path, store: &PasskeyStore) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("serialize custom-domain passkeys: {error}"))?;
    bytes.push(b'\n');
    crate::access::authority_store::with_lock(cert_dir, || {
        crate::access::authority_store::atomic_write_private_locked(&store_path(cert_dir), &bytes)
    })
    .map_err(|error| error.to_string())
}

fn passkey_actor(credential_id: &CredentialId) -> AccessPrincipal {
    let digest = Sha256::digest(&credential_id.0);
    let suffix: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    AccessPrincipal {
        id: format!("principal:custom-domain-passkey:{suffix}"),
        kind: "passkey_account".to_string(),
        label: "Custom-domain passkey".to_string(),
        source: "custom-domain-webauthn".to_string(),
        role_id: "role:none".to_string(),
        grant_id: None,
        transport: "custom-domain-https".to_string(),
        peer_profile: None,
        account: None,
        organization: None,
        authn: vec![serde_json::json!({
            "kind": "custom_domain_passkey",
            "credential_sha256": suffix,
        })],
        authn_kind: Some("custom_domain_passkey".to_string()),
        authn_binding: Some(suffix),
        authn_origin: None,
        hosted_connect: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain() -> ValidatedCustomDomain {
        ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        }
    }

    #[test]
    fn a_store_cannot_be_reused_under_a_different_rp_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = PasskeyStore {
            schema_version: STORE_SCHEMA_VERSION,
            name: "old.example.test".to_string(),
            rp_id: "old.example.test".to_string(),
            user_id: Uuid::new_v4(),
            passkeys: Vec::new(),
        };
        persist_store(dir.path(), &store).unwrap();
        assert!(load_store(dir.path(), &domain())
            .unwrap_err()
            .contains("different custom-domain"));
    }

    #[test]
    fn passkey_actor_carries_no_ambient_role() {
        let actor = passkey_actor(&CredentialId(vec![1, 2, 3]));
        assert_eq!(actor.kind, "passkey_account");
        assert_eq!(actor.role_id, "role:none");
        assert!(!actor.hosted_connect);
    }
}
