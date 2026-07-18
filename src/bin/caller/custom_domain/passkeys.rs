use passkey_auth::{
    Attachment, AuthenticationResponse, AuthenticationState, CredentialId, PasskeyCredential,
    RegistrationResponse, RegistrationState, Webauthn,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::access::hosted_control::{
    HostedControlRuntime, HostedLeaseDocument, HostedLeaseRequestInput,
};
use crate::access::iam::AccessPrincipal;
use crate::project::ValidatedCustomDomain;

const LEGACY_STORE_FILE: &str = "custom-domain-passkeys.json";
const STORE_FILE_PREFIX: &str = "custom-domain-passkeys";
const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_MAX_BYTES: u64 = 1024 * 1024;
const LEGACY_CEREMONY_STORE_FILE: &str = "custom-domain-passkey-ceremonies.json";
const CEREMONY_STORE_FILE_PREFIX: &str = "custom-domain-passkey-ceremonies";
const CEREMONY_STORE_SCHEMA_VERSION: u32 = 1;
const CEREMONY_STORE_MAX_BYTES: u64 = 2 * 1024 * 1024;
const FLOW_TTL_MS: u64 = 5 * 60 * 1000;
const INVITE_TTL_MS: u64 = 10 * 60 * 1000;
const MAX_PENDING_FLOWS: usize = 64;
const MAX_PASSKEYS: usize = 32;
const AUTH_START_WINDOW_MS: u64 = 60_000;
const AUTH_STARTS_PER_WINDOW: usize = 60;
/// One network-source bucket may consume only one eighth of the durable
/// authentication-flow pool. The remaining slots stay available to other
/// sources while the separate global ceiling bounds total work.
const AUTH_STARTS_PER_SOURCE_WINDOW: usize = 8;
const AUTH_PENDING_PER_SOURCE: usize = 8;
const AUTH_NEW_SOURCE_RESERVED_SLOTS: usize = 8;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingRegistration {
    label: String,
    user_id: Uuid,
    state: RegistrationState,
    expires_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingInvitation {
    label: String,
    expires_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingAuthentication {
    user_id: Uuid,
    state: AuthenticationState,
    input: HostedLeaseRequestInput,
    source_bucket: Option<String>,
    expires_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CeremonyStore {
    schema_version: u32,
    name: String,
    rp_id: String,
    invitations: HashMap<String, PendingInvitation>,
    registrations: HashMap<String, PendingRegistration>,
    authentications: HashMap<String, PendingAuthentication>,
    authentication_starts: VecDeque<u64>,
    #[serde(default)]
    authentication_starts_by_source: HashMap<String, VecDeque<u64>>,
}

pub(crate) struct PasskeyRuntime {
    domain: ValidatedCustomDomain,
    cert_dir: PathBuf,
    webauthn: Webauthn,
    store: Mutex<PasskeyStore>,
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
        crate::access::authority_store::with_lock(&cert_dir, || {
            let store_missing = match std::fs::metadata(store_path(&cert_dir, &domain)) {
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => {
                    return Err(crate::access::AccessError(format!(
                        "inspect {}: {error}",
                        store_path(&cert_dir, &domain).display()
                    )));
                }
            };
            let store = load_or_migrate_store_locked(&cert_dir, &domain)
                .map_err(crate::access::AccessError)?;
            if store_missing {
                save_store_locked(&cert_dir, &store)?;
            }
            migrate_legacy_ceremony_store_locked(&cert_dir, &domain)
                .map_err(crate::access::AccessError)?;
            load_ceremony_store_locked(&cert_dir, &domain).map_err(crate::access::AccessError)?;
            Ok(store)
        })
        .map_err(|error| error.to_string())
        .map(|store| {
            let webauthn = Webauthn::new(&domain.rp_id, "Intendant", &domain.origin)
                .require_user_verification(true)
                .authenticator_attachment(Attachment::Any)
                .strict_base64(true);
            Self {
                domain,
                cert_dir,
                webauthn,
                store: Mutex::new(store),
                hosted,
            }
        })
    }

    pub(crate) fn views(&self) -> Result<Vec<PasskeyView>, String> {
        self.with_fresh_store(|store| {
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
        })
    }

    pub(crate) fn registration_invite(
        &self,
        input: RegistrationInviteInput,
    ) -> Result<EnrollmentInvite, String> {
        let label = normalized_label(&input.label)?;
        self.with_fresh_store(|store| {
            if store.passkeys.len() >= MAX_PASSKEYS {
                return Err("custom-domain passkey limit reached".to_string());
            }
            Ok(())
        })?;
        let token = Uuid::new_v4().simple().to_string();
        let now = now_unix_ms();
        let expires_unix_ms = now.saturating_add(INVITE_TTL_MS);
        self.mutate_ceremonies(|ceremonies| {
            if ceremonies.invitations.len() >= MAX_PENDING_FLOWS {
                return Err("too many pending custom-domain enrollment invitations".to_string());
            }
            ceremonies.invitations.insert(
                token.clone(),
                PendingInvitation {
                    label,
                    expires_unix_ms,
                },
            );
            Ok(())
        })?;
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
        let token = input.token.trim().to_string();
        let invitation = self.read_ceremonies(|ceremonies| {
            ceremonies
                .invitations
                .get(&token)
                .cloned()
                .ok_or_else(|| "passkey enrollment invitation was not found".to_string())
        })?;
        if invitation.expires_unix_ms <= now_unix_ms() {
            return Err("passkey enrollment invitation expired".to_string());
        }
        let label = invitation.label.clone();
        let (user_id, exclude) = self.with_fresh_store(|store| {
            if store.passkeys.len() >= MAX_PASSKEYS {
                return Err("custom-domain passkey limit reached".to_string());
            }
            Ok((
                store.user_id,
                store
                    .passkeys
                    .iter()
                    .map(|passkey| passkey.credential.id.clone())
                    .collect::<Vec<_>>(),
            ))
        })?;
        let (options, state) = self.webauthn.start_registration(
            user_id.as_bytes(),
            &self.domain.name,
            &label,
            &exclude,
        );
        let flow_id = Uuid::new_v4().to_string();
        let now = now_unix_ms();
        self.mutate_ceremonies(|ceremonies| {
            if ceremonies.registrations.len() >= MAX_PENDING_FLOWS {
                return Err("too many pending custom-domain registration ceremonies".to_string());
            }
            let current = ceremonies
                .invitations
                .get(&token)
                .ok_or_else(|| "passkey enrollment invitation was not found".to_string())?;
            if current.expires_unix_ms <= now {
                return Err("passkey enrollment invitation expired".to_string());
            }
            if current.label != invitation.label
                || current.expires_unix_ms != invitation.expires_unix_ms
            {
                return Err("passkey enrollment invitation changed".to_string());
            }
            ceremonies.invitations.remove(&token);
            ceremonies.registrations.insert(
                flow_id.clone(),
                PendingRegistration {
                    label,
                    user_id,
                    state,
                    expires_unix_ms: now.saturating_add(FLOW_TTL_MS),
                },
            );
            Ok(())
        })?;
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
        let pending = self.mutate_ceremonies(|ceremonies| {
            ceremonies
                .registrations
                .remove(input.flow_id.trim())
                .ok_or_else(|| "passkey registration flow was not found".to_string())
        })?;
        if pending.expires_unix_ms <= now_unix_ms() {
            return Err("passkey registration flow expired".to_string());
        }
        let credential = self
            .webauthn
            .finish_registration(&pending.state, &input.credential)
            .map_err(|error| format!("finish passkey registration: {error}"))?;
        let now = now_unix_ms();
        self.mutate_store(move |store| {
            if store.user_id != pending.user_id {
                return Err("passkey enrollment state changed; create a new invitation".to_string());
            }
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
            let view = PasskeyView {
                credential_id: credential.id.to_b64url(),
                label: pending.label.clone(),
                created_unix_ms: now,
                last_used_unix_ms: None,
            };
            store.passkeys.push(StoredPasskey {
                credential,
                label: pending.label,
                created_unix_ms: now,
                last_used_unix_ms: None,
            });
            Ok((view, true))
        })
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
        let (user_id, credentials) = self.with_fresh_store(|store| {
            if store.passkeys.is_empty() {
                return Err("no passkey is registered for this custom domain".to_string());
            }
            Ok((
                store.user_id,
                store
                    .passkeys
                    .iter()
                    .map(|passkey| passkey.credential.clone())
                    .collect::<Vec<_>>(),
            ))
        })?;
        let (options, state) = self
            .webauthn
            .start_authentication_with_creds_for_user(user_id.as_bytes(), &credentials);
        let flow_id = Uuid::new_v4().to_string();
        let source_rate_key = authentication_source_rate_key(source_bucket);
        self.mutate_ceremonies(|ceremonies| {
            if ceremonies.authentication_starts.len() >= AUTH_STARTS_PER_WINDOW {
                return Err("custom-domain passkey ceremony rate limit reached".to_string());
            }
            if ceremonies.authentications.len() >= MAX_PENDING_FLOWS {
                return Err("too many pending custom-domain authentication ceremonies".to_string());
            }
            if ceremonies
                .authentication_starts_by_source
                .get(&source_rate_key)
                .is_some_and(|starts| starts.len() >= AUTH_STARTS_PER_SOURCE_WINDOW)
            {
                return Err("custom-domain passkey ceremony source rate limit reached".to_string());
            }
            let source_pending = ceremonies
                .authentications
                .values()
                .filter(|flow| {
                    authentication_source_rate_key(flow.source_bucket.as_deref()) == source_rate_key
                })
                .count();
            if source_pending >= AUTH_PENDING_PER_SOURCE {
                return Err(
                    "too many pending custom-domain authentication ceremonies for this source"
                        .to_string(),
                );
            }
            if ceremonies.authentications.len()
                >= MAX_PENDING_FLOWS.saturating_sub(AUTH_NEW_SOURCE_RESERVED_SLOTS)
                && source_pending > 0
            {
                return Err(
                    "custom-domain passkey capacity is reserved for a new source".to_string(),
                );
            }
            ceremonies.authentication_starts.push_back(now);
            ceremonies
                .authentication_starts_by_source
                .entry(source_rate_key)
                .or_default()
                .push_back(now);
            ceremonies.authentications.insert(
                flow_id.clone(),
                PendingAuthentication {
                    user_id,
                    state,
                    input: input.request,
                    source_bucket: source_bucket.map(str::to_string),
                    expires_unix_ms: now.saturating_add(FLOW_TTL_MS),
                },
            );
            Ok(())
        })?;
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
        current_fleet_zone_observed: Option<&AtomicBool>,
    ) -> Result<PasskeyLeaseResult, String> {
        self.require_origin(origin)?;
        let mut pending = self.mutate_ceremonies(|ceremonies| {
            ceremonies
                .authentications
                .remove(input.flow_id.trim())
                .ok_or_else(|| "passkey authentication flow was not found".to_string())
        })?;
        if pending.expires_unix_ms <= now_unix_ms() {
            return Err("passkey authentication flow expired".to_string());
        }
        pending.input.nonce = input.nonce;
        pending.input.timestamp_unix_ms = input.timestamp_unix_ms;
        pending.input.signature = input.signature;
        validate_pending_request_shape(&pending.input)?;
        let credential_id = CredentialId::from_b64url(&input.credential.id)
            .map_err(|error| format!("passkey credential id: {error}"))?;
        self.with_passkey_lease_transaction(|| {
            // Fleet provenance and passkey state share this authority lock.
            // Recheck after acquiring it so a zone observation committed
            // between the HTTP precheck and this transaction cannot mint a
            // lease from a lane that has just become ineligible.
            self.require_lane_eligible_locked(current_fleet_zone_observed)?;
            self.mutate_store(|store| {
                if store.user_id != pending.user_id {
                    return Err(
                        "passkey authentication state changed; start a new ceremony".to_string()
                    );
                }
                let stored = store
                    .passkeys
                    .iter_mut()
                    .find(|passkey| passkey.credential.id == credential_id)
                    .ok_or_else(|| {
                        "passkey is not registered for this custom domain".to_string()
                    })?;
                let result = self
                    .webauthn
                    .finish_authentication(&pending.state, &input.credential, &stored.credential)
                    .map_err(|error| format!("finish passkey authentication: {error}"))?;
                stored.credential.counter = result.new_counter;
                stored.last_used_unix_ms = Some(now_unix_ms());
                Ok(((), true))
            })?;

            let actor = passkey_actor(&credential_id);
            let lease = self
                .hosted
                .issue_passkey_lease(pending.input, origin, &actor)?;
            Ok(PasskeyLeaseResult { ok: true, lease })
        })
    }

    pub(crate) fn revoke(&self, input: RevokeInput) -> Result<bool, String> {
        let credential_id = CredentialId::from_b64url(input.credential_id.trim())
            .map_err(|error| format!("passkey credential id: {error}"))?;
        self.with_passkey_lease_transaction(|| {
            self.mutate_store(move |store| {
                let before = store.passkeys.len();
                store
                    .passkeys
                    .retain(|passkey| passkey.credential.id != credential_id);
                let revoked = store.passkeys.len() != before;
                Ok((revoked, revoked))
            })
        })
    }

    fn require_origin(&self, origin: &str) -> Result<(), String> {
        if origin == self.domain.origin {
            Ok(())
        } else {
            Err("passkey ceremony origin does not match the custom domain".to_string())
        }
    }

    fn with_fresh_store<T>(
        &self,
        read: impl FnOnce(&PasskeyStore) -> Result<T, String>,
    ) -> Result<T, String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let mut cached = self.store.lock().map_err(|_| {
                crate::access::AccessError("custom-domain passkey store is unavailable".to_string())
            })?;
            let fresh = load_current_store_locked(&self.cert_dir, &self.domain)
                .map_err(crate::access::AccessError)?;
            *cached = fresh;
            read(&cached).map_err(crate::access::AccessError)
        })
        .map_err(|error| error.to_string())
    }

    fn mutate_store<T>(
        &self,
        update: impl FnOnce(&mut PasskeyStore) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let mut cached = self.store.lock().map_err(|_| {
                crate::access::AccessError("custom-domain passkey store is unavailable".to_string())
            })?;
            let mut fresh = load_current_store_locked(&self.cert_dir, &self.domain)
                .map_err(crate::access::AccessError)?;
            let (value, changed) = update(&mut fresh).map_err(crate::access::AccessError)?;
            if changed {
                save_store_locked(&self.cert_dir, &fresh)?;
            }
            *cached = fresh;
            Ok(value)
        })
        .map_err(|error| error.to_string())
    }

    fn mutate_ceremonies<T>(
        &self,
        update: impl FnOnce(&mut CeremonyStore) -> Result<T, String>,
    ) -> Result<T, String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let mut ceremonies = load_ceremony_store_locked(&self.cert_dir, &self.domain)
                .map_err(crate::access::AccessError)?;
            prune_ceremonies(&mut ceremonies, now_unix_ms());
            let before = serde_json::to_vec(&ceremonies).map_err(|error| {
                crate::access::AccessError(format!(
                    "serialize custom-domain passkey ceremonies: {error}"
                ))
            })?;
            let result = update(&mut ceremonies).map_err(crate::access::AccessError)?;
            let changed = serde_json::to_vec(&ceremonies).map_err(|error| {
                crate::access::AccessError(format!(
                    "serialize custom-domain passkey ceremonies: {error}"
                ))
            })? != before;
            if changed {
                save_ceremony_store_locked(&self.cert_dir, &ceremonies)?;
            }
            Ok(result)
        })
        .map_err(|error| error.to_string())
    }

    fn read_ceremonies<T>(
        &self,
        read: impl FnOnce(&CeremonyStore) -> Result<T, String>,
    ) -> Result<T, String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let mut ceremonies = load_ceremony_store_locked(&self.cert_dir, &self.domain)
                .map_err(crate::access::AccessError)?;
            prune_ceremonies(&mut ceremonies, now_unix_ms());
            read(&ceremonies).map_err(crate::access::AccessError)
        })
        .map_err(|error| error.to_string())
    }

    /// Order credential validation + lease issuance against revocation under
    /// the same process and cross-process authority lock. If issuance wins,
    /// revocation cannot return until that lease exists; if revocation wins,
    /// the fresh store check rejects the removed credential before any lease
    /// request is created.
    fn with_passkey_lease_transaction<T>(
        &self,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            operation().map_err(crate::access::AccessError)
        })
        .map_err(|error| error.to_string())
    }

    fn require_lane_eligible_locked(
        &self,
        current_fleet_zone_observed: Option<&AtomicBool>,
    ) -> Result<(), String> {
        match super::domain_control_error_in(
            &self.cert_dir,
            &self.domain,
            current_fleet_zone_observed,
        ) {
            None => Ok(()),
            Some(error) => Err(error),
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

fn authentication_source_rate_key(source_bucket: Option<&str>) -> String {
    let source = source_bucket
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .unwrap_or("shared-relay-source");
    let mut hasher = Sha256::new();
    hasher.update(b"intendant-custom-domain-passkey-source-v1\n");
    hasher.update(source.as_bytes());
    crate::daemon_identity::b64u(&hasher.finalize())
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

fn store_generation(name: &str, rp_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"intendant-custom-domain-passkey-generation-v1\n");
    hasher.update(name.as_bytes());
    hasher.update(b"\n");
    hasher.update(rp_id.as_bytes());
    crate::daemon_identity::b64u(&hasher.finalize())
}

fn store_path_for_identity(cert_dir: &Path, name: &str, rp_id: &str) -> PathBuf {
    cert_dir.join(format!(
        "{STORE_FILE_PREFIX}-{}.json",
        store_generation(name, rp_id)
    ))
}

fn store_path(cert_dir: &Path, domain: &ValidatedCustomDomain) -> PathBuf {
    store_path_for_identity(cert_dir, &domain.name, &domain.rp_id)
}

fn legacy_store_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(LEGACY_STORE_FILE)
}

fn ceremony_store_path_for_identity(cert_dir: &Path, name: &str, rp_id: &str) -> PathBuf {
    cert_dir.join(format!(
        "{CEREMONY_STORE_FILE_PREFIX}-{}.json",
        store_generation(name, rp_id)
    ))
}

fn ceremony_store_path(cert_dir: &Path, domain: &ValidatedCustomDomain) -> PathBuf {
    ceremony_store_path_for_identity(cert_dir, &domain.name, &domain.rp_id)
}

fn legacy_ceremony_store_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(LEGACY_CEREMONY_STORE_FILE)
}

fn empty_ceremony_store(domain: &ValidatedCustomDomain) -> CeremonyStore {
    CeremonyStore {
        schema_version: CEREMONY_STORE_SCHEMA_VERSION,
        name: domain.name.clone(),
        rp_id: domain.rp_id.clone(),
        invitations: HashMap::new(),
        registrations: HashMap::new(),
        authentications: HashMap::new(),
        authentication_starts: VecDeque::new(),
        authentication_starts_by_source: HashMap::new(),
    }
}

fn prune_ceremonies(store: &mut CeremonyStore, now: u64) {
    store
        .invitations
        .retain(|_, invite| invite.expires_unix_ms > now);
    store
        .registrations
        .retain(|_, flow| flow.expires_unix_ms > now);
    store
        .authentications
        .retain(|_, flow| flow.expires_unix_ms > now);
    while store
        .authentication_starts
        .front()
        .is_some_and(|started| now.saturating_sub(*started) >= AUTH_START_WINDOW_MS)
    {
        store.authentication_starts.pop_front();
    }
    store.authentication_starts_by_source.retain(|_, starts| {
        while starts
            .front()
            .is_some_and(|started| now.saturating_sub(*started) >= AUTH_START_WINDOW_MS)
        {
            starts.pop_front();
        }
        !starts.is_empty()
    });
}

fn load_ceremony_store_locked(
    cert_dir: &Path,
    domain: &ValidatedCustomDomain,
) -> Result<CeremonyStore, String> {
    let path = ceremony_store_path(cert_dir, domain);
    load_ceremony_store_from_path(&path, domain)
}

fn load_ceremony_store_from_path(
    path: &Path,
    domain: &ValidatedCustomDomain,
) -> Result<CeremonyStore, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty_ceremony_store(domain));
        }
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if metadata.len() > CEREMONY_STORE_MAX_BYTES {
        return Err(format!(
            "{} exceeds the passkey-ceremony store size cap",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let store: CeremonyStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if store.schema_version != CEREMONY_STORE_SCHEMA_VERSION {
        return Err(format!(
            "{} uses unsupported schema version {}",
            path.display(),
            store.schema_version
        ));
    }
    if store.name != domain.name || store.rp_id != domain.rp_id {
        return Err(
            "stored passkey ceremonies belong to a different custom-domain name or rp_id"
                .to_string(),
        );
    }
    if store.invitations.len() > MAX_PENDING_FLOWS
        || store.registrations.len() > MAX_PENDING_FLOWS
        || store.authentications.len() > MAX_PENDING_FLOWS
        || store.authentication_starts.len() > AUTH_STARTS_PER_WINDOW
        || store.authentication_starts_by_source.len() > AUTH_STARTS_PER_WINDOW
        || store
            .authentication_starts_by_source
            .iter()
            .any(|(source, starts)| {
                source.len() != 43
                    || !source
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                    || starts.len() > AUTH_STARTS_PER_SOURCE_WINDOW
            })
        || store
            .invitations
            .keys()
            .chain(store.registrations.keys())
            .chain(store.authentications.keys())
            .any(|key| key.is_empty() || key.len() > 64)
        || store.authentications.values().any(|flow| {
            flow.source_bucket
                .as_ref()
                .is_some_and(|bucket| bucket.len() > 256)
        })
    {
        return Err("stored custom-domain passkey ceremonies exceed their bounds".to_string());
    }
    Ok(store)
}

fn migrate_legacy_ceremony_store_locked(
    cert_dir: &Path,
    domain: &ValidatedCustomDomain,
) -> Result<(), String> {
    let generation_path = ceremony_store_path(cert_dir, domain);
    if generation_path.exists() {
        return Ok(());
    }
    let legacy_path = legacy_ceremony_store_path(cert_dir);
    if !legacy_path.exists() {
        return Ok(());
    }
    let legacy: CeremonyStore = {
        let metadata = std::fs::metadata(&legacy_path)
            .map_err(|error| format!("inspect {}: {error}", legacy_path.display()))?;
        if metadata.len() > CEREMONY_STORE_MAX_BYTES {
            return Err(format!(
                "{} exceeds the passkey-ceremony store size cap",
                legacy_path.display()
            ));
        }
        let bytes = std::fs::read(&legacy_path)
            .map_err(|error| format!("read {}: {error}", legacy_path.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse {}: {error}", legacy_path.display()))?
    };
    if legacy.name != domain.name || legacy.rp_id != domain.rp_id {
        return Ok(());
    }
    // Reuse the complete validator before copying a legacy generation.
    let validated = load_ceremony_store_from_path(&legacy_path, domain)?;
    save_ceremony_store_locked(cert_dir, &validated).map_err(|error| error.to_string())
}

fn serialized_ceremony_store(store: &CeremonyStore) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("serialize custom-domain passkey ceremonies: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > CEREMONY_STORE_MAX_BYTES {
        return Err("custom-domain passkey-ceremony store exceeds the size cap".to_string());
    }
    Ok(bytes)
}

fn save_ceremony_store_locked(
    cert_dir: &Path,
    store: &CeremonyStore,
) -> crate::access::AccessResult<()> {
    let bytes = serialized_ceremony_store(store).map_err(crate::access::AccessError)?;
    crate::access::authority_store::atomic_write_private_locked(
        &ceremony_store_path_for_identity(cert_dir, &store.name, &store.rp_id),
        &bytes,
    )
}

fn empty_passkey_store(domain: &ValidatedCustomDomain) -> PasskeyStore {
    PasskeyStore {
        schema_version: STORE_SCHEMA_VERSION,
        name: domain.name.clone(),
        rp_id: domain.rp_id.clone(),
        user_id: Uuid::new_v4(),
        passkeys: Vec::new(),
    }
}

fn load_store_from_path(
    path: &Path,
    domain: &ValidatedCustomDomain,
) -> Result<PasskeyStore, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty_passkey_store(domain));
        }
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if metadata.len() > STORE_MAX_BYTES {
        return Err(format!(
            "{} exceeds the passkey-store size cap",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
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

fn load_store(cert_dir: &Path, domain: &ValidatedCustomDomain) -> Result<PasskeyStore, String> {
    load_store_from_path(&store_path(cert_dir, domain), domain)
}

fn load_or_migrate_store_locked(
    cert_dir: &Path,
    domain: &ValidatedCustomDomain,
) -> Result<PasskeyStore, String> {
    let generation_path = store_path(cert_dir, domain);
    if generation_path.exists() {
        return load_store_from_path(&generation_path, domain);
    }
    let legacy_path = legacy_store_path(cert_dir);
    if !legacy_path.exists() {
        return Ok(empty_passkey_store(domain));
    }
    let metadata = std::fs::metadata(&legacy_path)
        .map_err(|error| format!("inspect {}: {error}", legacy_path.display()))?;
    if metadata.len() > STORE_MAX_BYTES {
        return Err(format!(
            "{} exceeds the passkey-store size cap",
            legacy_path.display()
        ));
    }
    let bytes = std::fs::read(&legacy_path)
        .map_err(|error| format!("read {}: {error}", legacy_path.display()))?;
    let legacy: PasskeyStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", legacy_path.display()))?;
    if legacy.name != domain.name || legacy.rp_id != domain.rp_id {
        return Ok(empty_passkey_store(domain));
    }
    load_store_from_path(&legacy_path, domain)
}

fn load_current_store_locked(
    cert_dir: &Path,
    domain: &ValidatedCustomDomain,
) -> Result<PasskeyStore, String> {
    match std::fs::metadata(store_path(cert_dir, domain)) {
        Ok(_) => load_store(cert_dir, domain),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "{} disappeared after passkey initialization",
            store_path(cert_dir, domain).display()
        )),
        Err(error) => Err(format!(
            "inspect {}: {error}",
            store_path(cert_dir, domain).display()
        )),
    }
}

fn serialized_store(store: &PasskeyStore) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("serialize custom-domain passkeys: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > STORE_MAX_BYTES {
        return Err("custom-domain passkey store exceeds the size cap".to_string());
    }
    Ok(bytes)
}

fn save_store_locked(cert_dir: &Path, store: &PasskeyStore) -> crate::access::AccessResult<()> {
    let bytes = serialized_store(store).map_err(crate::access::AccessError)?;
    crate::access::authority_store::atomic_write_private_locked(
        &store_path_for_identity(cert_dir, &store.name, &store.rp_id),
        &bytes,
    )
}

#[cfg(test)]
fn persist_store(cert_dir: &Path, store: &PasskeyStore) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || save_store_locked(cert_dir, store))
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

    fn stored_passkey(id: u8, label: &str) -> StoredPasskey {
        StoredPasskey {
            credential: PasskeyCredential {
                id: CredentialId(vec![id]),
                public_key_cose: passkey_auth::CosePublicKey(vec![0xa0]),
                counter: 0,
                transports: Vec::new(),
                aaguid: [0; 16],
            },
            label: label.to_string(),
            created_unix_ms: 1,
            last_used_unix_ms: None,
        }
    }

    fn hosted_runtime(cert_dir: &Path) -> Arc<HostedControlRuntime> {
        Arc::new(HostedControlRuntime::new(
            false,
            cert_dir.to_path_buf(),
            None,
            None,
            String::new(),
            false,
        ))
    }

    #[test]
    fn lease_transaction_rechecks_current_fleet_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = PasskeyRuntime::new(
            domain(),
            dir.path().to_path_buf(),
            hosted_runtime(dir.path()),
        )
        .unwrap();
        let observed = AtomicBool::new(true);

        runtime
            .with_passkey_lease_transaction(|| {
                runtime.require_lane_eligible_locked(Some(&observed))
            })
            .unwrap();

        observed.store(false, std::sync::atomic::Ordering::SeqCst);
        let error = runtime
            .with_passkey_lease_transaction(|| {
                runtime.require_lane_eligible_locked(Some(&observed))
            })
            .unwrap_err();
        assert!(error.contains("waiting for the current Connect fleet-zone observation"));

        observed.store(true, std::sync::atomic::Ordering::SeqCst);
        crate::fleet_cert::remember_fleet_origin_for_test(
            dir.path(),
            Some("example.test"),
            "d-1234567890abcdef1234.example.test",
        )
        .unwrap();
        let error = runtime
            .with_passkey_lease_transaction(|| {
                runtime.require_lane_eligible_locked(Some(&observed))
            })
            .unwrap_err();
        assert!(error.contains("overlaps a service-controlled fleet name or zone"));
    }

    #[test]
    fn empty_store_identity_is_persisted_before_ceremonies_are_exposed() {
        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        let hosted = hosted_runtime(dir.path());
        let first = PasskeyRuntime::new(
            domain.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&hosted),
        )
        .unwrap();
        let first_user_id = first.store.lock().unwrap().user_id;
        assert!(store_path(dir.path(), &domain).is_file());

        let second = PasskeyRuntime::new(domain, dir.path().to_path_buf(), hosted).unwrap();
        assert_eq!(second.store.lock().unwrap().user_id, first_user_id);
    }

    #[test]
    fn passkey_generations_are_namespaced_by_domain_and_rp_id() {
        let dir = tempfile::tempdir().unwrap();
        let domain_a = domain();
        let hosted = hosted_runtime(dir.path());
        let first = PasskeyRuntime::new(
            domain_a.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&hosted),
        )
        .unwrap();
        first
            .mutate_store(|store| {
                store.passkeys.push(stored_passkey(7, "Domain A key"));
                Ok(((), true))
            })
            .unwrap();
        let domain_b = ValidatedCustomDomain {
            name: "other.example.test".to_string(),
            rp_id: "other.example.test".to_string(),
            origin: "https://other.example.test".to_string(),
        };
        assert_ne!(
            store_path(dir.path(), &domain_a),
            store_path(dir.path(), &domain_b)
        );

        let second =
            PasskeyRuntime::new(domain_b, dir.path().to_path_buf(), Arc::clone(&hosted)).unwrap();
        assert!(second.views().unwrap().is_empty());
        assert!(!second
            .revoke(RevokeInput {
                credential_id: CredentialId(vec![7]).to_b64url(),
            })
            .unwrap());

        let restored = PasskeyRuntime::new(domain_a, dir.path().to_path_buf(), hosted).unwrap();
        assert_eq!(restored.views().unwrap().len(), 1);
        assert_eq!(restored.views().unwrap()[0].label, "Domain A key");
    }

    #[test]
    fn legacy_singleton_migrates_only_to_its_matching_generation() {
        let dir = tempfile::tempdir().unwrap();
        let domain_a = domain();
        let legacy = PasskeyStore {
            schema_version: STORE_SCHEMA_VERSION,
            name: domain_a.name.clone(),
            rp_id: domain_a.rp_id.clone(),
            user_id: Uuid::new_v4(),
            passkeys: vec![stored_passkey(9, "Legacy key")],
        };
        std::fs::write(
            legacy_store_path(dir.path()),
            serialized_store(&legacy).unwrap(),
        )
        .unwrap();
        let hosted = hosted_runtime(dir.path());
        let migrated = PasskeyRuntime::new(
            domain_a.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&hosted),
        )
        .unwrap();
        assert_eq!(migrated.views().unwrap().len(), 1);
        assert!(store_path(dir.path(), &domain_a).is_file());

        let domain_b = ValidatedCustomDomain {
            name: "replacement.example.test".to_string(),
            rp_id: "replacement.example.test".to_string(),
            origin: "https://replacement.example.test".to_string(),
        };
        let replacement = PasskeyRuntime::new(domain_b, dir.path().to_path_buf(), hosted).unwrap();
        assert!(replacement.views().unwrap().is_empty());
    }

    #[test]
    fn registration_invites_and_flows_cross_process_boundaries_once() {
        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        let hosted = hosted_runtime(dir.path());
        let first = PasskeyRuntime::new(
            domain.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&hosted),
        )
        .unwrap();
        let second = PasskeyRuntime::new(domain.clone(), dir.path().to_path_buf(), hosted).unwrap();

        let invite = first
            .registration_invite(RegistrationInviteInput {
                label: "Phone".to_string(),
            })
            .unwrap();
        let token = invite
            .enrollment_url
            .split_once("#passkey_enroll=")
            .unwrap()
            .1
            .to_string();
        let start = second
            .registration_start(
                RegistrationStartInput {
                    token: token.clone(),
                },
                &domain.origin,
            )
            .unwrap();
        assert!(
            first
                .registration_start(RegistrationStartInput { token }, &domain.origin)
                .unwrap_err()
                .contains("not found"),
            "the durable invitation is consumed exactly once across processes"
        );
        let ceremonies = crate::access::authority_store::with_lock(dir.path(), || {
            load_ceremony_store_locked(dir.path(), &domain).map_err(crate::access::AccessError)
        })
        .unwrap();
        assert!(
            ceremonies.registrations.contains_key(&start.flow_id),
            "the finish request may land on any process"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_registration_flow_commit_does_not_consume_the_invitation() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        let runtime = PasskeyRuntime::new(
            domain.clone(),
            dir.path().to_path_buf(),
            hosted_runtime(dir.path()),
        )
        .unwrap();
        let invite = runtime
            .registration_invite(RegistrationInviteInput {
                label: "Phone".to_string(),
            })
            .unwrap();
        let token = invite
            .enrollment_url
            .split_once("#passkey_enroll=")
            .unwrap()
            .1
            .to_string();

        let original = std::fs::metadata(dir.path()).unwrap().permissions();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = runtime.registration_start(
            RegistrationStartInput {
                token: token.clone(),
            },
            &domain.origin,
        );
        std::fs::set_permissions(dir.path(), original).unwrap();
        assert!(failed.is_err(), "the durable commit was expected to fail");

        runtime
            .registration_start(RegistrationStartInput { token }, &domain.origin)
            .expect("the invitation survives a failed atomic flow commit");
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_or_rejected_ceremony_transactions_do_not_write() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let runtime = PasskeyRuntime::new(
            domain(),
            dir.path().to_path_buf(),
            hosted_runtime(dir.path()),
        )
        .unwrap();
        runtime
            .registration_invite(RegistrationInviteInput {
                label: "Phone".to_string(),
            })
            .unwrap();

        let original = std::fs::metadata(dir.path()).unwrap().permissions();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let unchanged = runtime.mutate_ceremonies(|_| Ok(()));
        let rejected =
            runtime.mutate_ceremonies::<()>(|_| Err("ceremony request rejected".to_string()));
        std::fs::set_permissions(dir.path(), original).unwrap();

        unchanged.expect("an unchanged transaction performs no durable write");
        assert_eq!(
            rejected.unwrap_err(),
            "ceremony request rejected",
            "a rejected transaction returns its decision without attempting a write"
        );
    }

    #[test]
    fn one_authentication_source_cannot_starve_another() {
        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        persist_store(
            dir.path(),
            &PasskeyStore {
                schema_version: STORE_SCHEMA_VERSION,
                name: domain.name.clone(),
                rp_id: domain.rp_id.clone(),
                user_id: Uuid::new_v4(),
                passkeys: vec![stored_passkey(1, "one")],
            },
        )
        .unwrap();
        let runtime = PasskeyRuntime::new(
            domain.clone(),
            dir.path().to_path_buf(),
            hosted_runtime(dir.path()),
        )
        .unwrap();
        let input = || AuthenticationStartInput {
            request: HostedLeaseRequestInput {
                browser_public_key: "browser-key".to_string(),
                requested_preset: Default::default(),
                requested_ttl_secs: 60,
                requester_label: "Browser".to_string(),
                nonce: String::new(),
                timestamp_unix_ms: 0,
                signature: String::new(),
            },
        };
        for _ in 0..AUTH_PENDING_PER_SOURCE {
            runtime
                .authentication_start(input(), &domain.origin, None)
                .unwrap();
        }
        assert!(runtime
            .authentication_start(input(), &domain.origin, None)
            .unwrap_err()
            .contains("source"));
        runtime
            .authentication_start(input(), &domain.origin, Some("198.51.100.7"))
            .expect("a distinct source retains admission below the global safety ceiling");
        for index in 0..47 {
            let source = format!("203.0.113.{index}:443");
            runtime
                .authentication_start(input(), &domain.origin, Some(&source))
                .unwrap();
        }
        assert!(runtime
            .authentication_start(input(), &domain.origin, Some("198.51.100.7"))
            .unwrap_err()
            .contains("reserved"));
        runtime
            .authentication_start(input(), &domain.origin, Some("198.51.100.8"))
            .expect("the global tail remains reserved for a previously unseen source");
    }

    #[test]
    fn authentication_flow_can_be_consumed_by_a_sibling_process() {
        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        persist_store(
            dir.path(),
            &PasskeyStore {
                schema_version: STORE_SCHEMA_VERSION,
                name: domain.name.clone(),
                rp_id: domain.rp_id.clone(),
                user_id: Uuid::new_v4(),
                passkeys: vec![stored_passkey(1, "one")],
            },
        )
        .unwrap();
        let hosted = hosted_runtime(dir.path());
        let first = PasskeyRuntime::new(
            domain.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&hosted),
        )
        .unwrap();
        let second = PasskeyRuntime::new(domain.clone(), dir.path().to_path_buf(), hosted).unwrap();
        let start = first
            .authentication_start(
                AuthenticationStartInput {
                    request: HostedLeaseRequestInput {
                        browser_public_key: "browser-key".to_string(),
                        requested_preset: Default::default(),
                        requested_ttl_secs: 60,
                        requester_label: "Browser".to_string(),
                        nonce: String::new(),
                        timestamp_unix_ms: 0,
                        signature: String::new(),
                    },
                },
                &domain.origin,
                Some("test-source"),
            )
            .unwrap();
        let consumed = second
            .mutate_ceremonies(|ceremonies| {
                ceremonies
                    .authentications
                    .remove(&start.flow_id)
                    .ok_or_else(|| "missing durable authentication flow".to_string())
            })
            .unwrap();
        assert_eq!(consumed.source_bucket.as_deref(), Some("test-source"));
        assert!(first
            .mutate_ceremonies(|ceremonies| {
                ceremonies
                    .authentications
                    .remove(&start.flow_id)
                    .ok_or_else(|| "already consumed".to_string())
            })
            .unwrap_err()
            .contains("already consumed"));
    }

    #[test]
    fn a_store_is_not_reused_under_a_different_rp_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = PasskeyStore {
            schema_version: STORE_SCHEMA_VERSION,
            name: "old.example.test".to_string(),
            rp_id: "old.example.test".to_string(),
            user_id: Uuid::new_v4(),
            passkeys: Vec::new(),
        };
        persist_store(dir.path(), &store).unwrap();
        let current = domain();
        let loaded = load_store(dir.path(), &current).unwrap();
        assert_eq!(loaded.name, current.name);
        assert_eq!(loaded.rp_id, current.rp_id);
        assert!(loaded.passkeys.is_empty());
        assert_ne!(
            store_path_for_identity(dir.path(), &store.name, &store.rp_id),
            store_path(dir.path(), &current)
        );
    }

    #[test]
    fn stale_daemon_cannot_restore_a_revoked_passkey() {
        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        persist_store(
            dir.path(),
            &PasskeyStore {
                schema_version: STORE_SCHEMA_VERSION,
                name: domain.name.clone(),
                rp_id: domain.rp_id.clone(),
                user_id: Uuid::new_v4(),
                passkeys: vec![stored_passkey(1, "one"), stored_passkey(2, "two")],
            },
        )
        .unwrap();
        let hosted = Arc::new(HostedControlRuntime::new(
            false,
            dir.path().to_path_buf(),
            None,
            None,
            String::new(),
            false,
        ));
        let first = PasskeyRuntime::new(
            domain.clone(),
            dir.path().to_path_buf(),
            Arc::clone(&hosted),
        )
        .unwrap();
        let stale = PasskeyRuntime::new(domain.clone(), dir.path().to_path_buf(), hosted).unwrap();

        assert!(first
            .revoke(RevokeInput {
                credential_id: CredentialId(vec![1]).to_b64url(),
            })
            .unwrap());
        assert!(stale
            .revoke(RevokeInput {
                credential_id: CredentialId(vec![2]).to_b64url(),
            })
            .unwrap());
        assert!(
            load_store(dir.path(), &domain).unwrap().passkeys.is_empty(),
            "the stale process must reload under the interprocess lock"
        );
    }

    #[test]
    fn successful_revocation_waits_for_an_inflight_passkey_authority_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        let credential_id = CredentialId(vec![1]).to_b64url();
        persist_store(
            dir.path(),
            &PasskeyStore {
                schema_version: STORE_SCHEMA_VERSION,
                name: domain.name.clone(),
                rp_id: domain.rp_id.clone(),
                user_id: Uuid::new_v4(),
                passkeys: vec![stored_passkey(1, "one")],
            },
        )
        .unwrap();
        let hosted = Arc::new(HostedControlRuntime::new(
            false,
            dir.path().to_path_buf(),
            None,
            None,
            String::new(),
            false,
        ));
        let runtime =
            Arc::new(PasskeyRuntime::new(domain, dir.path().to_path_buf(), hosted).unwrap());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut worker = None;

        crate::access::authority_store::with_lock(dir.path(), || {
            let runtime = Arc::clone(&runtime);
            worker = Some(std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let result = runtime.revoke(RevokeInput { credential_id });
                done_tx.send(result).unwrap();
            }));
            started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            assert!(
                done_rx
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .is_err(),
                "revocation must not return while issuance can still hold the authority transaction"
            );
            Ok(())
        })
        .unwrap();

        assert!(done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap());
        worker.unwrap().join().unwrap();
    }

    #[test]
    fn passkey_store_cap_is_enforced_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let domain = domain();
        let path = store_path(dir.path(), &domain);
        let error = persist_store(
            dir.path(),
            &PasskeyStore {
                schema_version: STORE_SCHEMA_VERSION,
                name: domain.name,
                rp_id: domain.rp_id,
                user_id: Uuid::new_v4(),
                passkeys: vec![stored_passkey(1, &"x".repeat(STORE_MAX_BYTES as usize))],
            },
        )
        .unwrap_err();
        assert!(error.contains("size cap"), "{error}");
        assert!(!path.exists());
    }

    #[test]
    fn passkey_actor_carries_no_ambient_role() {
        let actor = passkey_actor(&CredentialId(vec![1, 2, 3]));
        assert_eq!(actor.kind, "passkey_account");
        assert_eq!(actor.role_id, "role:none");
        assert!(!actor.hosted_connect);
    }
}
