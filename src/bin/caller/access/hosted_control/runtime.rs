use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::access::iam::{
    self, AccessPrincipal, IamAuditEvent, IamGrant, IamPrincipal, LocalIamState,
};
use crate::access::{AccessError, AccessResult};
use crate::daemon_identity::{b64u, verify_b64u, DaemonIdentity};

use super::*;

/// The compiled qualifying set of signed application distributions — the
/// only ids `enroll_signed_app_anchor` will enroll and the witness/decision
/// lanes will accept. Each id names a **lane** (platform + provenance +
/// revision), never a repo, key, or filename: those parameters live once in
/// the compiled pins the enrollment verifier already consults —
/// `crate::hosted_verify::DEFAULT_RELEASE_REPO`,
/// `crate::pgp_identity::RELEASE_SIGNING_KEY_FINGERPRINT` (and the committed
/// key bytes beside it), and the per-host append-only transparency-log pin
/// under the state root (`hosted-verify/<host>.json`). Qualification is
/// evidence-based, verified per instance at enrollment (receipt
/// re-verification against the pinned log plus a keystore-key challenge) —
/// never parsed from an artifact name, and never retroactive: releases
/// published before the verified install ceremony existed stay outside.
/// `-unsigned-dev`-suffixed artifacts, source builds, and browser tabs are
/// permanently outside the set. Future lanes (an Apple-notarized lane,
/// other platforms) are new ids with their own evidence semantics.
pub(super) const ELIGIBLE_SIGNED_APP_DISTRIBUTIONS: &[&str] = &["macos-pgp-logged-v1"];

/// Mirror of the witness lane's changed-evidence posture: a decision is
/// bound to the exact daemon-signed request digest the anchor saw, and a
/// digest that no longer matches is refused rather than silently rebound.
pub const ANCHOR_DECISION_REQUEST_CHANGED_ERROR: &str =
    "lease request changed; fetch the current daemon-signed request before deciding";
/// Uniform refusal for unknown, revoked, key-mismatched, or set-nonmember
/// anchors — deliberately non-enumerating, like the witness lane's.
pub const ANCHOR_DECISION_REFUSED_ERROR: &str =
    "signed application anchor decision is not accepted";

fn ensure_eligible_distribution(distribution_id: &str) -> Result<(), String> {
    if ELIGIBLE_SIGNED_APP_DISTRIBUTIONS
        .iter()
        .any(|distribution| *distribution == distribution_id)
    {
        return Ok(());
    }
    Err(format!(
        "signed application distribution {distribution_id:?} is not in this build's qualifying set"
    ))
}

/// The one acceptance predicate for enrolled signed-application anchors:
/// an active, unrevoked record for the device, byte-equal presented key,
/// and compiled-set membership re-checked at use time. The witness and
/// decision lanes both resolve through here.
pub(super) fn find_accepted_signed_app_anchor<'a>(
    state: &'a LocalIamState,
    device_id: &str,
    presented_public_key: &str,
    refusal: &str,
) -> AccessResult<&'a SignedAppAnchor> {
    let anchor = state
        .hosted_control
        .signed_app_anchors
        .iter()
        .find(|anchor| {
            anchor.device_id == device_id && anchor.active && anchor.revoked_unix_ms.is_none()
        })
        .ok_or_else(|| AccessError(refusal.to_string()))?;
    if anchor.public_key != presented_public_key
        || ensure_eligible_distribution(&anchor.distribution_id).is_err()
    {
        return Err(AccessError(refusal.to_string()));
    }
    Ok(anchor)
}

/// Where enrollment re-verifies a receipt's release: the same rendezvous
/// ladder, GitHub API base, compiled repo, and per-host append-only pin
/// store the consumer update lane uses — one log, one pin per host.
pub(crate) struct ReleaseEvidenceEndpoints {
    pub(crate) log_base: url::Url,
    pub(crate) github_api: url::Url,
    pub(crate) repo: String,
    pub(crate) state_root: PathBuf,
}

impl ReleaseEvidenceEndpoints {
    fn resolve() -> Result<Self, String> {
        Ok(Self {
            log_base: crate::handover::update_rendezvous_url()?,
            github_api: url::Url::parse(crate::hosted_verify::GITHUB_API_BASE)
                .map_err(|error| format!("GitHub API base: {error}"))?,
            repo: crate::hosted_verify::DEFAULT_RELEASE_REPO.to_string(),
            state_root: crate::platform::intendant_home(),
        })
    }
}

fn validate_receipt_shape(receipt: &SignedAppInstallReceipt) -> Result<(), String> {
    let hex64 =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if receipt.tag.is_empty()
        || receipt.tag.len() > 64
        || !receipt
            .tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err("install receipt tag is invalid".to_string());
    }
    if !hex64(&receipt.manifest_hash) {
        return Err("install receipt manifest hash is invalid".to_string());
    }
    if !hex64(&receipt.artifact_sha256) {
        return Err("install receipt artifact digest is invalid".to_string());
    }
    // The re-sign identity fields are machine-local evidence (empty when the
    // owner declined the stable identity); bound their shape, not presence.
    let bounded = |value: &str| value.len() <= 192 && !value.chars().any(char::is_control);
    if !bounded(&receipt.resign_cert_fingerprint) || !bounded(&receipt.post_resign_cdhash) {
        return Err("install receipt re-sign identity fields are invalid".to_string());
    }
    if receipt.written_unix_ms == 0 {
        return Err("install receipt timestamp is invalid".to_string());
    }
    Ok(())
}

fn describe_verify_failure(failure: &crate::hosted_verify::VerifyFailure) -> String {
    match failure {
        crate::hosted_verify::VerifyFailure::Unavailable(detail) => {
            format!("verification unavailable: {detail}")
        }
        crate::hosted_verify::VerifyFailure::Verification { summary, mismatches } => {
            if mismatches.is_empty() {
                summary.clone()
            } else {
                format!("{summary} — {}", mismatches.join("; "))
            }
        }
    }
}

pub fn mark_session_created_by_hosted_lease(
    cert_dir: &Path,
    lease_id: &str,
    session_id: &str,
) -> AccessResult<()> {
    if !valid_id_component(lease_id) || !valid_id_component(session_id) {
        return Err(AccessError(
            "hosted lease or session identifier is invalid".to_string(),
        ));
    }
    iam::transact_state(cert_dir, |state, _| {
        if compute_current_lane_guard(state).status == HostedLaneGuardStatus::Suspended {
            return Err(AccessError(
                "hosted control is suspended by the certificate guard".to_string(),
            ));
        }
        let now = now_ms() as u64;
        let lease = state
            .hosted_control
            .leases
            .iter()
            .find(|lease| lease.document.lease_id == lease_id)
            .ok_or_else(|| AccessError("hosted lease was not found".to_string()))?;
        let document = lease.document.clone();
        if lease.status != HostedLeaseStatus::Active
            || document.expires_unix_ms <= now
            || document.preset < HostedPreset::Tasks
            || document.preset > state.hosted_control.policy.ceiling
        {
            return Err(AccessError(
                "hosted lease is inactive or cannot create sessions".to_string(),
            ));
        }
        let principal = state
            .principals
            .iter()
            .find(|principal| principal.id == document.principal_id)
            .ok_or_else(|| AccessError("hosted lease principal was not found".to_string()))?;
        let grant = state
            .grants
            .iter()
            .find(|grant| grant.id == document.grant_id)
            .ok_or_else(|| AccessError("hosted lease grant was not found".to_string()))?;
        if principal.kind != HOSTED_PRINCIPAL_KIND
            || principal.source != HOSTED_SOURCE
            || !iam::is_enforced_status(&principal.status)
            || grant.principal_id != principal.id
            || grant.source != HOSTED_SOURCE
            || HostedPreset::from_role_id(&grant.role_id) != Some(document.preset)
            || grant.expires_at_unix_ms != Some(document.expires_unix_ms)
            || !grant.is_active_at(now_ms())
        {
            return Err(AccessError(
                "hosted lease IAM binding is not current".to_string(),
            ));
        }
        let changed = !state
            .hosted_control
            .policy
            .eligible_session_ids
            .iter()
            .any(|candidate| candidate == session_id);
        if changed {
            state
                .hosted_control
                .policy
                .eligible_session_ids
                .push(session_id.to_string());
            state.hosted_control.normalize();
            push_audit(
                state,
                &document.principal_id,
                "hosted_session_create",
                session_id,
                format!("Marked session eligible from hosted lease {lease_id}"),
            );
        }
        Ok(((), changed))
    })
}

#[derive(Clone)]
pub struct HostedControlRuntime {
    pub(super) enabled: bool,
    pub(super) init_error: Option<String>,
    pub(super) cert_dir: PathBuf,
    pub(super) identity: Option<Arc<DaemonIdentity>>,
    pub(super) identity_path: Option<PathBuf>,
    pub(super) daemon_id: String,
    daemon_label: String,
    display_media_relay_configured: bool,
    pub(super) witness_rate: Arc<Mutex<WitnessRateState>>,
}

impl std::fmt::Debug for HostedControlRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedControlRuntime")
            .field("enabled", &self.enabled)
            .field("init_error", &self.init_error)
            .field("cert_dir", &self.cert_dir)
            .field("daemon_id", &self.daemon_id)
            .field("daemon_label", &self.daemon_label)
            .field(
                "display_media_relay_configured",
                &self.display_media_relay_configured,
            )
            .finish_non_exhaustive()
    }
}

const SHARED_TRANSIENT_FILE: &str = "hosted-control-transient.json";
const SHARED_TRANSIENT_SCHEMA_VERSION: u32 = 2;
const SHARED_TRANSIENT_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayRecord {
    authority_digest: String,
    nonce_digest: String,
    timestamp_unix_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WsTicketRecord {
    lease_id: String,
    grant_id: String,
    fleet_origin: String,
    expires_unix_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedTransientState {
    schema_version: u32,
    public_replay: Vec<ReplayRecord>,
    lease_replay: Vec<ReplayRecord>,
    tickets: HashMap<String, WsTicketRecord>,
    #[serde(default)]
    doorbell_rate: DoorbellRateState,
    #[serde(default)]
    poll_rate: PollRateState,
}

impl Default for SharedTransientState {
    fn default() -> Self {
        Self {
            schema_version: SHARED_TRANSIENT_SCHEMA_VERSION,
            public_replay: Vec::new(),
            lease_replay: Vec::new(),
            tickets: HashMap::new(),
            doorbell_rate: DoorbellRateState::default(),
            poll_rate: PollRateState::default(),
        }
    }
}

#[derive(Clone, Copy)]
enum ReplayLane {
    Public,
    Lease,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DoorbellRateState {
    global: VecDeque<i64>,
    by_key: HashMap<String, VecDeque<i64>>,
    by_source: HashMap<String, VecDeque<i64>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PollRateState {
    global: VecDeque<i64>,
    by_request: HashMap<String, VecDeque<i64>>,
}

#[derive(Default)]
pub(super) struct WitnessRateState {
    pub(super) global: VecDeque<i64>,
    pub(super) by_binding: HashMap<String, VecDeque<i64>>,
}

#[derive(Clone, Debug)]
pub struct VerifiedHostedLease {
    pub principal: AccessPrincipal,
    pub iam_state: Arc<LocalIamState>,
    pub document: HostedLeaseDocument,
}

impl HostedControlRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        enabled: bool,
        cert_dir: PathBuf,
        identity_path: Option<&Path>,
        configured_daemon_id: Option<&str>,
        daemon_label: String,
        display_media_relay_configured: bool,
    ) -> Self {
        let (identity, init_error) = if enabled {
            match identity_path
                .map(DaemonIdentity::load_or_create)
                .unwrap_or_else(DaemonIdentity::load_or_create_default)
                .map(Arc::new)
            {
                Ok(identity) => (Some(identity), None),
                Err(error) => (None, Some(error)),
            }
        } else {
            // A dark runtime must not touch the live daemon-identity store.
            (None, None)
        };
        let public_key = identity
            .as_ref()
            .map(|identity| identity.public_key_b64u())
            .unwrap_or_default();
        let daemon_id = configured_daemon_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&public_key)
            .to_string();
        Self {
            enabled,
            init_error,
            cert_dir,
            identity,
            identity_path: identity_path.map(Path::to_path_buf),
            daemon_id,
            daemon_label,
            display_media_relay_configured,
            witness_rate: Arc::new(Mutex::new(WitnessRateState::default())),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled && self.init_error.is_none()
    }

    pub fn configured(&self) -> bool {
        self.enabled
    }

    pub fn initialization_error(&self) -> Option<&str> {
        self.init_error.as_deref()
    }

    pub fn bootstrap(&self, fleet_origin: &str) -> Result<HostedControlBootstrap, String> {
        self.ensure_enabled()?;
        let fleet_origin = validate_fleet_origin(fleet_origin)?;
        let state = iam::load_state_cached_arc(&self.cert_dir)
            .map_err(|error| format!("load hosted-control policy: {error}"))?;
        let identity = self.identity()?;
        let lane_guard = HostedPublicLaneGuard {
            status: compute_current_lane_guard(&state).status,
        };
        Ok(HostedControlBootstrap {
            enabled: true,
            daemon_id: self.daemon_id.clone(),
            daemon_label: self.daemon_label.clone(),
            daemon_public_key: identity.public_key_b64u(),
            fleet_origin,
            default_preset: HostedPreset::Tasks.min(state.hosted_control.policy.ceiling),
            ceiling: state.hosted_control.policy.ceiling,
            default_ttl_secs: DEFAULT_LEASE_TTL_SECS.min(state.hosted_control.policy.max_ttl_secs),
            max_ttl_secs: state.hosted_control.policy.max_ttl_secs,
            request_ttl_ms: PENDING_REQUEST_TTL_MS,
            display_media_relay_configured: self.display_media_relay_configured,
            lane_guard,
            custom_domain: false,
            rp_id: None,
            passkey_available: false,
        })
    }

    pub fn create_request(
        &self,
        mut input: HostedLeaseRequestInput,
        fleet_origin: &str,
        source_bucket: Option<&str>,
    ) -> Result<HostedLeaseRequest, String> {
        self.ensure_enabled()?;
        self.ensure_lane_available()?;
        let identity = self.identity()?;
        let fleet_origin = validate_fleet_origin(fleet_origin)?;
        let (public_key, fingerprint) = validate_browser_public_key(&input.browser_public_key)?;
        let label = input.requester_label.trim().to_string();
        if label.is_empty() || label.len() > 96 || label.chars().any(char::is_control) {
            return Err("requester_label must contain 1 to 96 printable characters".to_string());
        }
        if !valid_id_component(&input.nonce) {
            return Err("doorbell proof nonce is invalid".to_string());
        }
        verify_timestamp(input.timestamp_unix_ms)?;
        if !(MIN_LEASE_TTL_SECS..=HARD_MAX_LEASE_TTL_SECS).contains(&input.requested_ttl_secs) {
            return Err(format!(
                "requested_ttl_secs must be between {MIN_LEASE_TTL_SECS} and {HARD_MAX_LEASE_TTL_SECS}"
            ));
        }
        input.browser_public_key = public_key.clone();
        input.requester_label = label.clone();
        let now = now_ms();
        // Account every well-shaped attempt before the comparatively
        // expensive curve verification. Invalid signatures must consume the
        // same bounded attempt budget as valid ones.
        self.check_doorbell_rate(&fingerprint, source_bucket, now)?;
        verify_p256_signature(
            &public_key,
            input
                .proof_payload(&self.daemon_id, &fleet_origin)
                .as_bytes(),
            &input.signature,
        )?;
        self.record_nonce(
            &format!("doorbell:{fingerprint}"),
            &input.nonce,
            input.timestamp_unix_ms,
        )?;
        let mut request = HostedLeaseRequest {
            protocol: DOORBELL_PROTOCOL.to_string(),
            request_id: format!("request:{}", uuid::Uuid::new_v4().simple()),
            request_nonce: random_b64u(32)?,
            browser_public_key: public_key,
            browser_key_fingerprint: fingerprint,
            requested_preset: input.requested_preset,
            requested_ttl_secs: input.requested_ttl_secs,
            requester_label: label,
            fleet_origin,
            daemon_id: self.daemon_id.clone(),
            daemon_label: self.daemon_label.clone(),
            daemon_public_key: identity.public_key_b64u(),
            created_unix_ms: now as u64,
            expires_unix_ms: (now as u64).saturating_add(PENDING_REQUEST_TTL_MS),
            status: HostedLeaseRequestStatus::Pending,
            approved_lease_id: None,
            doorbell_signature: String::new(),
        };
        request.doorbell_signature = identity.sign_b64u(request.signing_payload().as_bytes());
        iam::transact_state(&self.cert_dir, |state, _| {
            let now = now as u64;
            let expired_request_ids = state
                .hosted_control
                .requests
                .iter_mut()
                .filter_map(|stored| {
                    if stored.status == HostedLeaseRequestStatus::Pending
                        && stored.expires_unix_ms <= now
                    {
                        stored.status = HostedLeaseRequestStatus::Expired;
                        Some(stored.request_id.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            for request_id in expired_request_ids {
                push_audit(
                    state,
                    "principal:anonymous:hosted-doorbell",
                    "hosted_lease_request_expire",
                    &request_id,
                    "Observed expired hosted lease request".to_string(),
                );
            }
            let active_pending = state
                .hosted_control
                .requests
                .iter()
                .filter(|stored| {
                    stored.status == HostedLeaseRequestStatus::Pending
                        && stored.expires_unix_ms > now
                })
                .count();
            if active_pending >= HOSTED_REQUESTS_CAP {
                return Err(AccessError(
                    "hosted lease request queue is full; retry after a pending request is decided or expires"
                        .to_string(),
                ));
            }
            if request.requested_preset > state.hosted_control.policy.ceiling {
                return Err(AccessError(
                    "requested preset exceeds the daemon ceiling".to_string(),
                ));
            }
            if !(MIN_LEASE_TTL_SECS..=state.hosted_control.policy.max_ttl_secs)
                .contains(&request.requested_ttl_secs)
            {
                return Err(AccessError(format!(
                    "requested_ttl_secs must be between {MIN_LEASE_TTL_SECS} and {}",
                    state.hosted_control.policy.max_ttl_secs
                )));
            }
            state.hosted_control.requests.push(request.clone());
            state.hosted_control.normalize();
            push_audit(
                state,
                "principal:anonymous:hosted-doorbell",
                "hosted_lease_request",
                &request.request_id,
                format!(
                    "Hosted lease request for {} preset, {} seconds",
                    request.requested_preset.as_str(),
                    request.requested_ttl_secs
                ),
            );
            Ok(((), true))
        })
        .map_err(|error| format!("store hosted lease request: {error}"))?;
        Ok(request)
    }

    pub fn poll_request(
        &self,
        proof: &HostedLeasePollProof,
    ) -> Result<HostedLeasePollResult, String> {
        self.ensure_enabled()?;
        let state = iam::load_state_cached_arc(&self.cert_dir)
            .map_err(|error| format!("load hosted lease request: {error}"))?;
        let request = state
            .hosted_control
            .requests
            .iter()
            .find(|request| request.request_id == proof.request_id)
            .cloned()
            .ok_or_else(|| "hosted lease request was not found".to_string())?;
        self.verify_doorbell(&request)?;
        verify_timestamp(proof.timestamp_unix_ms)?;
        if !valid_id_component(&proof.nonce) {
            return Err("poll proof nonce is invalid".to_string());
        }
        self.check_poll_rate(&request.request_id, now_ms())?;
        let payload = format!(
            "{POLL_PROOF_PROTOCOL}\n{}\n{}\n{}\n{}",
            request.request_id,
            request.document_sha256(),
            proof.nonce,
            proof.timestamp_unix_ms
        );
        verify_p256_signature(
            &request.browser_public_key,
            payload.as_bytes(),
            &proof.signature,
        )?;
        self.record_nonce(
            &format!("poll:{}", request.request_id),
            &proof.nonce,
            proof.timestamp_unix_ms,
        )?;
        self.materialize_expirations("principal:anonymous:hosted-doorbell")
            .map_err(|error| format!("record hosted-control expiry: {error}"))?;
        let state = iam::load_state_cached_arc(&self.cert_dir)
            .map_err(|error| format!("reload hosted lease request: {error}"))?;
        let request = state
            .hosted_control
            .requests
            .iter()
            .find(|candidate| candidate.request_id == request.request_id)
            .cloned()
            .ok_or_else(|| "hosted lease request was not found".to_string())?;
        let lease = request.approved_lease_id.as_deref().and_then(|lease_id| {
            state
                .hosted_control
                .leases
                .iter()
                .find(|lease| {
                    lease.document.lease_id == lease_id && lease.status == HostedLeaseStatus::Active
                })
                .map(|lease| lease.document.clone())
        });
        Ok(HostedLeasePollResult { request, lease })
    }

    pub fn decide_request(
        &self,
        input: HostedLeaseDecisionInput,
        actor: &AccessPrincipal,
    ) -> Result<Option<HostedLeaseDocument>, String> {
        self.decide_request_as(input, &actor.id)
    }

    /// The one lease-decision core. Trusted-surface callers arrive through
    /// [`Self::decide_request`]; the anchor decision lane arrives through
    /// [`Self::apply_anchor_decision`] after content-signature verification,
    /// with the enrolled anchor as the audited actor. Nothing else decides
    /// pending lease requests.
    fn decide_request_as(
        &self,
        input: HostedLeaseDecisionInput,
        actor_id: &str,
    ) -> Result<Option<HostedLeaseDocument>, String> {
        self.ensure_enabled()?;
        let identity = self.identity()?;
        iam::transact_state(&self.cert_dir, |state, _| {
            if input.approve
                && compute_current_lane_guard(state).status == HostedLaneGuardStatus::Suspended
            {
                return Err(AccessError(
                    "hosted control is suspended by the certificate guard".to_string(),
                ));
            }
            let now = now_ms() as u64;
            let request_index = state
                .hosted_control
                .requests
                .iter()
                .position(|request| request.request_id == input.request_id)
                .ok_or_else(|| AccessError("hosted lease request was not found".to_string()))?;
            let request = state.hosted_control.requests[request_index].clone();
            self.verify_doorbell(&request).map_err(AccessError)?;
            if request.expires_unix_ms <= now {
                state.hosted_control.requests[request_index].status =
                    HostedLeaseRequestStatus::Expired;
                return Err(AccessError("hosted lease request has expired".to_string()));
            }
            if request.status == HostedLeaseRequestStatus::Approved {
                let lease_id = request
                    .approved_lease_id
                    .as_deref()
                    .ok_or_else(|| AccessError("approved request has no lease id".to_string()))?;
                let document = state
                    .hosted_control
                    .leases
                    .iter()
                    .find(|lease| lease.document.lease_id == lease_id)
                    .map(|lease| lease.document.clone())
                    .ok_or_else(|| {
                        AccessError("approved request lease record was not found".to_string())
                    })?;
                return Ok((Some(document), false));
            }
            if request.status != HostedLeaseRequestStatus::Pending {
                return Err(AccessError(
                    "hosted lease request is no longer pending".to_string(),
                ));
            }
            if !input.approve {
                state.hosted_control.requests[request_index].status =
                    HostedLeaseRequestStatus::Denied;
                push_audit(
                    state,
                    actor_id,
                    "hosted_lease_deny",
                    &request.request_id,
                    "Denied hosted lease request".to_string(),
                );
                return Ok((None, true));
            }
            let preset = input.approved_preset.unwrap_or(request.requested_preset);
            if preset > request.requested_preset || preset > state.hosted_control.policy.ceiling {
                return Err(AccessError(
                    "approved preset may not exceed the request or daemon ceiling".to_string(),
                ));
            }
            let ttl = input
                .approved_ttl_secs
                .unwrap_or(request.requested_ttl_secs);
            if ttl < MIN_LEASE_TTL_SECS
                || ttl > request.requested_ttl_secs
                || ttl > state.hosted_control.policy.max_ttl_secs
                || ttl > HARD_MAX_LEASE_TTL_SECS
            {
                return Err(AccessError(
                    "approved TTL may not exceed the request or daemon limit".to_string(),
                ));
            }
            let document = issue_lease_record(
                state,
                &request,
                preset,
                ttl,
                actor_id,
                identity,
                &self.daemon_id,
            )?;
            state.hosted_control.requests[request_index].status =
                HostedLeaseRequestStatus::Approved;
            state.hosted_control.requests[request_index].approved_lease_id =
                Some(document.lease_id.clone());
            Ok((Some(document), true))
        })
        .map_err(|error| format!("decide hosted lease request: {error}"))
    }

    /// Issue a lease after a successful custom-domain passkey ceremony.
    ///
    /// This path validates the browser-key proof and the exact same daemon
    /// ceiling/TTL/guard invariants as a doorbell approval, but deliberately
    /// does not consume anonymous doorbell rate or pending-request capacity.
    pub(crate) fn issue_passkey_lease(
        &self,
        mut input: HostedLeaseRequestInput,
        fleet_origin: &str,
        actor: &AccessPrincipal,
    ) -> Result<HostedLeaseDocument, String> {
        self.ensure_enabled()?;
        self.ensure_lane_available()?;
        let identity = self.identity()?;
        let fleet_origin = validate_fleet_origin(fleet_origin)?;
        let (public_key, fingerprint) = validate_browser_public_key(&input.browser_public_key)?;
        let label = input.requester_label.trim().to_string();
        if label.is_empty() || label.len() > 96 || label.chars().any(char::is_control) {
            return Err("requester_label must contain 1 to 96 printable characters".to_string());
        }
        if !valid_id_component(&input.nonce) {
            return Err("doorbell proof nonce is invalid".to_string());
        }
        verify_timestamp(input.timestamp_unix_ms)?;
        if !(MIN_LEASE_TTL_SECS..=HARD_MAX_LEASE_TTL_SECS).contains(&input.requested_ttl_secs) {
            return Err(format!(
                "requested_ttl_secs must be between {MIN_LEASE_TTL_SECS} and {HARD_MAX_LEASE_TTL_SECS}"
            ));
        }
        input.browser_public_key = public_key.clone();
        input.requester_label = label.clone();
        verify_p256_signature(
            &public_key,
            input
                .proof_payload(&self.daemon_id, &fleet_origin)
                .as_bytes(),
            &input.signature,
        )?;
        self.record_nonce(
            &format!("passkey:{}:{fingerprint}", actor.id),
            &input.nonce,
            input.timestamp_unix_ms,
        )?;
        let now = now_ms().max(0) as u64;
        let mut request = HostedLeaseRequest {
            protocol: DOORBELL_PROTOCOL.to_string(),
            request_id: format!("passkey-request:{}", uuid::Uuid::new_v4().simple()),
            request_nonce: random_b64u(32)?,
            browser_public_key: public_key,
            browser_key_fingerprint: fingerprint,
            requested_preset: input.requested_preset,
            requested_ttl_secs: input.requested_ttl_secs,
            requester_label: label,
            fleet_origin,
            daemon_id: self.daemon_id.clone(),
            daemon_label: self.daemon_label.clone(),
            daemon_public_key: identity.public_key_b64u(),
            created_unix_ms: now,
            expires_unix_ms: now.saturating_add(PENDING_REQUEST_TTL_MS),
            status: HostedLeaseRequestStatus::Pending,
            approved_lease_id: None,
            doorbell_signature: String::new(),
        };
        request.doorbell_signature = identity.sign_b64u(request.signing_payload().as_bytes());
        iam::transact_state(&self.cert_dir, |state, _| {
            if compute_current_lane_guard(state).status == HostedLaneGuardStatus::Suspended {
                return Err(AccessError(
                    "hosted control is suspended by the certificate guard".to_string(),
                ));
            }
            if request.requested_preset > state.hosted_control.policy.ceiling {
                return Err(AccessError(
                    "requested preset exceeds the daemon ceiling".to_string(),
                ));
            }
            if !(MIN_LEASE_TTL_SECS..=state.hosted_control.policy.max_ttl_secs)
                .contains(&request.requested_ttl_secs)
            {
                return Err(AccessError(format!(
                    "requested_ttl_secs must be between {MIN_LEASE_TTL_SECS} and {}",
                    state.hosted_control.policy.max_ttl_secs
                )));
            }
            let document = issue_lease_record(
                state,
                &request,
                request.requested_preset,
                request.requested_ttl_secs,
                &actor.id,
                identity,
                &self.daemon_id,
            )?;
            Ok((document, true))
        })
        .map_err(|error| format!("issue passkey-authorized hosted lease: {error}"))
    }

    pub fn verify_request_proof(
        &self,
        method: &str,
        raw_path_and_query: &str,
        fleet_origin: &str,
        proof: &HostedRequestProof,
        transport: &str,
    ) -> Result<VerifiedHostedLease, String> {
        self.ensure_enabled()?;
        verify_timestamp(proof.timestamp_unix_ms)?;
        if !valid_id_component(&proof.nonce) {
            return Err("hosted request proof nonce is invalid".to_string());
        }
        let fleet_origin = validate_fleet_origin(fleet_origin)?;
        let verified = self.load_verified_lease(&proof.lease_id, &fleet_origin, transport)?;
        let payload = format!(
            "{REQUEST_PROOF_PROTOCOL}\n{}\n{}\n{}\n{}\n{}\n{}",
            method.to_ascii_uppercase(),
            raw_path_and_query,
            self.daemon_id,
            verified.document.document_sha256,
            proof.nonce,
            proof.timestamp_unix_ms
        );
        verify_p256_signature(
            &verified.document.browser_public_key,
            payload.as_bytes(),
            &proof.signature,
        )?;
        self.record_lease_nonce(
            &format!("lease:{}", proof.lease_id),
            &proof.nonce,
            proof.timestamp_unix_ms,
        )?;
        Ok(verified)
    }

    /// Refresh an already-authenticated request's lease and IAM snapshot
    /// without consuming another proof nonce. Long request-body reads must
    /// call this again before dispatching side effects so expiry, revocation,
    /// ceiling changes, and the certificate guard take effect within the
    /// same HTTP exchange.
    pub fn revalidate_verified_lease(
        &self,
        verified: &VerifiedHostedLease,
    ) -> Result<VerifiedHostedLease, String> {
        self.ensure_enabled()?;
        let current = self.load_verified_lease(
            &verified.document.lease_id,
            &verified.document.fleet_origin,
            &verified.principal.transport,
        )?;
        if current.document != verified.document {
            return Err("hosted lease document changed during the request".to_string());
        }
        Ok(current)
    }

    pub fn mint_ws_ticket(&self, verified: &VerifiedHostedLease) -> Result<HostedWsTicket, String> {
        self.ensure_enabled()?;
        self.ensure_lane_available()?;
        let ticket = random_b64u(32)?;
        let expires = (now_ms() as u64).saturating_add(WS_TICKET_TTL_MS);
        let record = WsTicketRecord {
            lease_id: verified.document.lease_id.clone(),
            grant_id: verified.document.grant_id.clone(),
            fleet_origin: verified.document.fleet_origin.clone(),
            expires_unix_ms: expires,
        };
        mutate_shared_transient(&self.cert_dir, |state| {
            let now = now_ms().max(0) as u64;
            state
                .tickets
                .retain(|_, record| record.expires_unix_ms > now);
            if state.tickets.len() >= WS_TICKETS_GLOBAL_CAP {
                return Err("too many outstanding hosted WebSocket tickets".to_string());
            }
            if state.tickets.contains_key(&ticket) {
                return Err("generated a duplicate hosted WebSocket ticket".to_string());
            }
            state.tickets.insert(ticket.clone(), record);
            Ok(())
        })?;
        Ok(HostedWsTicket {
            ticket,
            expires_unix_ms: expires,
        })
    }

    pub fn consume_ws_ticket(
        &self,
        ticket: &str,
        fleet_origin: &str,
        transport: &str,
    ) -> Result<VerifiedHostedLease, String> {
        self.ensure_enabled()?;
        let fleet_origin = validate_fleet_origin(fleet_origin)?;
        if !valid_id_component(ticket) {
            return Err("hosted WebSocket ticket is invalid".to_string());
        }
        let record = mutate_shared_transient(&self.cert_dir, |state| {
            state.tickets.remove(ticket).ok_or_else(|| {
                "hosted WebSocket ticket was not found or was already used".to_string()
            })
        })?;
        if record.expires_unix_ms <= now_ms() as u64 {
            return Err("hosted WebSocket ticket has expired".to_string());
        }
        if record.fleet_origin != fleet_origin {
            return Err("hosted WebSocket ticket origin does not match".to_string());
        }
        let verified = self.load_verified_lease(&record.lease_id, &fleet_origin, transport)?;
        if verified.document.grant_id != record.grant_id {
            return Err("hosted WebSocket ticket grant changed".to_string());
        }
        Ok(verified)
    }

    pub fn management_snapshot(&self) -> AccessResult<HostedControlManagementSnapshot> {
        self.materialize_expirations("principal:local:hosted-control-observer")?;
        let state = iam::load_state_cached_arc(&self.cert_dir)?;
        let now = now_ms() as u64;
        let lane_guard = compute_current_lane_guard(&state);
        // Derived from the compiled constant; forks and future builds may
        // compile the set empty again, so the emptiness read stays.
        #[allow(clippy::const_is_empty)]
        let qualifying_signed_app_distribution_available =
            !ELIGIBLE_SIGNED_APP_DISTRIBUTIONS.is_empty();
        Ok(HostedControlManagementSnapshot {
            configured: self.configured(),
            enabled: self.enabled(),
            initialization_error: self.initialization_error().map(ToOwned::to_owned),
            display_media_relay_configured: self.display_media_relay_configured,
            anchor_decision_protocol: ANCHOR_DECISION_PROTOCOL.to_string(),
            qualifying_signed_app_distribution_available,
            eligible_signed_app_distributions: ELIGIBLE_SIGNED_APP_DISTRIBUTIONS
                .iter()
                .map(ToString::to_string)
                .collect(),
            policy: state.hosted_control.policy.clone(),
            pending_requests: state
                .hosted_control
                .requests
                .iter()
                .cloned()
                .map(|request| project_request_status(request, now))
                .filter(|request| request.status == HostedLeaseRequestStatus::Pending)
                .collect(),
            active_leases: state
                .hosted_control
                .leases
                .iter()
                .filter(|lease| {
                    lease.status == HostedLeaseStatus::Active
                        && lease.document.expires_unix_ms > now
                })
                .cloned()
                .collect(),
            signed_app_anchors: state.hosted_control.signed_app_anchors.clone(),
            certificate_ledger: self.certificate_ledger().ok(),
            lane_guard,
        })
    }

    pub fn revoke_lease(&self, lease_id: &str, actor: &AccessPrincipal) -> AccessResult<bool> {
        self.ensure_enabled().map_err(AccessError)?;
        iam::transact_state(&self.cert_dir, |state, _| {
            let now = now_ms() as u64;
            let Some(lease) = state
                .hosted_control
                .leases
                .iter_mut()
                .find(|lease| lease.document.lease_id == lease_id)
            else {
                return Ok((false, false));
            };
            if lease.status != HostedLeaseStatus::Active {
                return Ok((false, false));
            }
            lease.status = HostedLeaseStatus::Revoked;
            lease.revoked_at_unix_ms = Some(now);
            lease.revoked_by = Some(actor.id.clone());
            let grant_id = lease.document.grant_id.clone();
            if let Some(grant) = state.grants.iter_mut().find(|grant| grant.id == grant_id) {
                grant.status = "revoked".to_string();
                grant.revoked_at_unix_ms = Some(now);
            }
            push_audit(
                state,
                &actor.id,
                "hosted_lease_revoke",
                lease_id,
                "Revoked hosted lease".to_string(),
            );
            Ok((true, true))
        })
    }

    pub fn set_policy(
        &self,
        ceiling: HostedPreset,
        max_ttl_secs: u64,
        actor: &AccessPrincipal,
        operate_acknowledged: bool,
    ) -> AccessResult<HostedControlPolicy> {
        self.ensure_enabled().map_err(AccessError)?;
        if !(MIN_LEASE_TTL_SECS..=HARD_MAX_LEASE_TTL_SECS).contains(&max_ttl_secs) {
            return Err(AccessError(format!(
                "max lease TTL must be between {MIN_LEASE_TTL_SECS} and {HARD_MAX_LEASE_TTL_SECS} seconds"
            )));
        }
        iam::transact_state(&self.cert_dir, |state, _| {
            let old = state.hosted_control.policy.clone();
            if state.tier.as_deref() == Some("integrated")
                && ceiling == HostedPreset::Operate
                && old.ceiling < HostedPreset::Operate
                && !operate_acknowledged
            {
                return Err(AccessError(
                    "Operate on an integrated daemon requires hardening acknowledgement"
                        .to_string(),
                ));
            }
            state.hosted_control.policy.ceiling = ceiling;
            state.hosted_control.policy.max_ttl_secs = max_ttl_secs;
            let now = now_ms() as u64;
            let mut revoked_documents = Vec::new();
            for lease in &mut state.hosted_control.leases {
                if lease.status == HostedLeaseStatus::Active
                    && (lease.document.preset > ceiling
                        || lease.document.expires_unix_ms.saturating_sub(now)
                            > max_ttl_secs.saturating_mul(1000))
                {
                    lease.status = HostedLeaseStatus::Revoked;
                    lease.revoked_at_unix_ms = Some(now);
                    lease.revoked_by = Some(actor.id.clone());
                    revoked_documents.push(lease.document.clone());
                }
            }
            for grant in &mut state.grants {
                if revoked_documents
                    .iter()
                    .any(|document| document.grant_id == grant.id)
                {
                    grant.status = "revoked".to_string();
                    grant.revoked_at_unix_ms = Some(now);
                }
            }
            for document in &revoked_documents {
                push_audit(
                    state,
                    &actor.id,
                    "hosted_lease_revoke",
                    &document.lease_id,
                    format!(
                        "Revoked {} lease during policy update ({} second lifetime)",
                        document.preset.as_str(),
                        document
                            .expires_unix_ms
                            .saturating_sub(document.issued_unix_ms)
                            / 1000
                    ),
                );
            }
            push_audit(
                state,
                &actor.id,
                "hosted_policy_update",
                "policy:hosted-control",
                format!(
                    "Set hosted ceiling to {} and max TTL to {} seconds",
                    ceiling.as_str(),
                    max_ttl_secs
                ),
            );
            Ok((
                state.hosted_control.policy.clone(),
                old != state.hosted_control.policy || !revoked_documents.is_empty(),
            ))
        })
    }

    pub fn set_session_eligibility(
        &self,
        session_id: &str,
        eligible: bool,
        actor: &AccessPrincipal,
    ) -> AccessResult<bool> {
        self.ensure_enabled().map_err(AccessError)?;
        if !valid_id_component(session_id) {
            return Err(AccessError("session id is invalid".to_string()));
        }
        iam::transact_state(&self.cert_dir, |state, _| {
            let before = state.hosted_control.policy.eligible_session_ids.clone();
            if eligible {
                state
                    .hosted_control
                    .policy
                    .eligible_session_ids
                    .push(session_id.to_string());
            } else {
                state
                    .hosted_control
                    .policy
                    .eligible_session_ids
                    .retain(|candidate| candidate != session_id);
            }
            state.hosted_control.normalize();
            let changed = before != state.hosted_control.policy.eligible_session_ids;
            if changed {
                push_audit(
                    state,
                    &actor.id,
                    "hosted_session_eligibility",
                    session_id,
                    if eligible {
                        "Marked session hosted-eligible".to_string()
                    } else {
                        "Removed hosted session eligibility".to_string()
                    },
                );
            }
            Ok((changed, changed))
        })
    }

    /// Enroll a signed-application anchor at the unchanged local or
    /// direct-mTLS owner ceremony. The evidence chain hardens that ceremony
    /// against supply-chain drift and honest mistake; it never substitutes
    /// for it, and an enrolled anchor holds no IAM role. Verification is
    /// fail-closed and online: the daemon independently re-verifies the
    /// receipt's release against its own compiled pins through
    /// [`crate::hosted_verify::verify_hosted_release`] — no cached-evidence
    /// acceptance. The optional macOS live-codesign strength probe stages
    /// with the app-side ceremony (only the running app knows its live
    /// bundle path); the receipt's re-sign identity fields are covered by
    /// the receipt digest so that probe can compare later.
    pub async fn enroll_signed_app_anchor(
        &self,
        input: SignedAppAnchorEnrollmentInput,
        actor: &AccessPrincipal,
    ) -> AccessResult<SignedAppAnchor> {
        let endpoints = ReleaseEvidenceEndpoints::resolve().map_err(AccessError)?;
        self.enroll_signed_app_anchor_at(&endpoints, input, actor)
            .await
    }

    pub(crate) async fn enroll_signed_app_anchor_at(
        &self,
        endpoints: &ReleaseEvidenceEndpoints,
        mut input: SignedAppAnchorEnrollmentInput,
        actor: &AccessPrincipal,
    ) -> AccessResult<SignedAppAnchor> {
        self.ensure_enabled().map_err(AccessError)?;
        // Set membership refuses before any cryptography or network reach,
        // and is re-checked inside the durable transaction below.
        ensure_eligible_distribution(&input.distribution_id).map_err(AccessError)?;
        if !valid_id_component(&input.device_id) {
            return Err(AccessError("anchor device id is invalid".to_string()));
        }
        let label = input.label.trim().to_string();
        if label.is_empty() || label.len() > 96 || label.chars().any(char::is_control) {
            return Err(AccessError(
                "anchor label must contain 1 to 96 printable characters".to_string(),
            ));
        }
        let (public_key, key_fingerprint) = validate_browser_public_key(&input.public_key)
            .map_err(|_| {
                AccessError(
                    "anchor public key must be an uncompressed P-256 point (base64url)"
                        .to_string(),
                )
            })?;
        if !valid_id_component(&input.nonce) {
            return Err(AccessError("anchor enrollment nonce is invalid".to_string()));
        }
        verify_timestamp(input.timestamp_unix_ms).map_err(AccessError)?;
        validate_receipt_shape(&input.receipt).map_err(AccessError)?;
        // The keystore-key challenge: a domain-separated versioned transcript
        // binding daemon, device, key, distribution, and the exact receipt.
        input.public_key = public_key.clone();
        input.label = label.clone();
        verify_p256_signature(
            &public_key,
            input.challenge_payload(&self.daemon_id).as_bytes(),
            &input.signature,
        )
        .map_err(AccessError)?;
        self.record_nonce(
            &format!("anchor-enroll:{key_fingerprint}"),
            &input.nonce,
            input.timestamp_unix_ms,
        )
        .map_err(AccessError)?;
        // Fail closed online: any transparency-log, pin, signature-coverage,
        // or GitHub divergence — including plain unavailability — refuses
        // enrollment. Enrollment is rare and owner-driven; there is no
        // cached-evidence path.
        let report = crate::hosted_verify::verify_hosted_release(
            &endpoints.log_base,
            &endpoints.github_api,
            &endpoints.repo,
            Some(&input.receipt.tag),
            false,
            &endpoints.state_root,
        )
        .await
        .map_err(|failure| {
            AccessError(format!(
                "enrollment evidence verification failed: {}",
                describe_verify_failure(&failure)
            ))
        })?;
        if report.manifest_index != input.receipt.log_index {
            return Err(AccessError(
                "install receipt names a different transparency-log index than the verified release"
                    .to_string(),
            ));
        }
        if report.manifest_hash != input.receipt.manifest_hash {
            return Err(AccessError(
                "install receipt manifest hash does not match the verified release".to_string(),
            ));
        }
        if !report
            .artifacts
            .iter()
            .any(|artifact| artifact.sha256 == input.receipt.artifact_sha256)
        {
            return Err(AccessError(
                "install receipt artifact digest is not in the verified release's logged manifest"
                    .to_string(),
            ));
        }
        let now = now_ms().max(0) as u64;
        let anchor = SignedAppAnchor {
            device_id: input.device_id.clone(),
            label,
            public_key,
            key_fingerprint,
            distribution_id: input.distribution_id.clone(),
            active: true,
            enrolled_unix_ms: now,
            revoked_unix_ms: None,
            evidence: Some(SignedAppAnchorEvidence {
                receipt_sha256: input.receipt.document_sha256(),
                verified_tag: report.tag.clone(),
                log_index: report.manifest_index,
                artifact_sha256: input.receipt.artifact_sha256.clone(),
                pgp_fingerprint_at_enrollment: report.pgp_fingerprint.clone(),
                verified_unix_ms: now,
            }),
        };
        iam::transact_state(&self.cert_dir, |state, _| {
            ensure_eligible_distribution(&anchor.distribution_id).map_err(AccessError)?;
            // Re-enrollment under the same device id is the owner's
            // key-rotation and staleness remedy: the fresh record replaces
            // the prior one, so the old key stops witnessing immediately.
            let replaced = state
                .hosted_control
                .signed_app_anchors
                .iter()
                .any(|existing| existing.device_id == anchor.device_id);
            state
                .hosted_control
                .signed_app_anchors
                .retain(|existing| existing.device_id != anchor.device_id);
            state.hosted_control.signed_app_anchors.push(anchor.clone());
            state.hosted_control.normalize();
            push_audit(
                state,
                &actor.id,
                "hosted_anchor_enroll",
                &anchor.device_id,
                format!(
                    "Enrolled signed-app anchor under {} with verified release {}{}",
                    anchor.distribution_id,
                    report.tag,
                    if replaced {
                        ", replacing the prior enrollment"
                    } else {
                        ""
                    }
                ),
            );
            Ok(((), true))
        })?;
        Ok(anchor)
    }

    /// Verify and apply a content-signed lease decision from an enrolled
    /// signed-application anchor. The document's authority is exactly one
    /// pending lease decision — approve or deny, bound to the exact
    /// daemon-signed request digest — never a management operation: policy,
    /// revocation, eligibility, anchor CRUD, and witness confirm/override
    /// remain exclusively on the trusted confirmation surface.
    pub fn apply_anchor_decision(
        &self,
        document: HostedAnchorDecisionDocument,
    ) -> Result<HostedLeaseRequestStatus, String> {
        self.ensure_enabled()?;
        if document.protocol != ANCHOR_DECISION_PROTOCOL {
            return Err("unsupported anchor decision protocol".to_string());
        }
        if document.daemon_id != self.daemon_id {
            return Err("anchor decision names a different target daemon".to_string());
        }
        if !valid_id_component(&document.device_id)
            || !valid_id_component(&document.request_id)
            || !valid_id_component(&document.nonce)
        {
            return Err("anchor decision identifier is invalid".to_string());
        }
        verify_timestamp(document.timestamp_unix_ms)?;
        let state = iam::load_state_cached_arc(&self.cert_dir)
            .map_err(|error| format!("load signed-app anchor state: {error}"))?;
        let anchor = find_accepted_signed_app_anchor(
            &state,
            &document.device_id,
            &document.anchor_public_key,
            ANCHOR_DECISION_REFUSED_ERROR,
        )
        .map_err(|error| error.to_string())?;
        let anchor_public_key = anchor.public_key.clone();
        // Account the attempt before the curve verification, but only after
        // the anchor lookup: garbage from unenrolled devices must not drain
        // the shared observation window.
        self.record_witness_rate(&format!("decision:{}", document.device_id))
            .map_err(|_| "anchor decision rate limit exceeded".to_string())?;
        verify_p256_signature(
            &anchor_public_key,
            document.signing_payload().as_bytes(),
            &document.signature,
        )?;
        self.record_nonce(
            &format!("anchor-decision:{}", document.device_id),
            &document.nonce,
            document.timestamp_unix_ms,
        )?;
        // Bind to the exact daemon-signed request. The digest covers only
        // the request's immutable creation-time fields, so a mismatch means
        // the anchor decided a different request than this daemon holds.
        let request = state
            .hosted_control
            .requests
            .iter()
            .find(|request| request.request_id == document.request_id)
            .cloned()
            .ok_or_else(|| "hosted lease request was not found".to_string())?;
        self.verify_doorbell(&request)?;
        if request.document_sha256() != document.request_document_sha256 {
            return Err(ANCHOR_DECISION_REQUEST_CHANGED_ERROR.to_string());
        }
        let decided = self.decide_request_as(
            HostedLeaseDecisionInput {
                request_id: document.request_id.clone(),
                approve: document.approve,
                approved_preset: None,
                approved_ttl_secs: None,
            },
            &format!("principal:signed-app-anchor:{}", document.device_id),
        )?;
        // The lease document itself rides the requesting browser's poll
        // channel; the anchor learns only the decision outcome.
        Ok(if decided.is_some() {
            HostedLeaseRequestStatus::Approved
        } else {
            HostedLeaseRequestStatus::Denied
        })
    }

    fn materialize_expirations(&self, actor: &str) -> AccessResult<()> {
        iam::transact_state(&self.cert_dir, |state, _| {
            let now = now_ms() as u64;
            let mut expired_requests = Vec::new();
            for request in &mut state.hosted_control.requests {
                if request.status == HostedLeaseRequestStatus::Pending
                    && request.expires_unix_ms <= now
                {
                    request.status = HostedLeaseRequestStatus::Expired;
                    expired_requests.push(request.request_id.clone());
                }
            }
            for request_id in &expired_requests {
                push_audit(
                    state,
                    actor,
                    "hosted_lease_request_expire",
                    request_id,
                    "Observed expired hosted lease request".to_string(),
                );
            }
            let expired_leases = materialize_hosted_lease_expirations(state, now, actor);
            let changed = !expired_requests.is_empty() || expired_leases > 0;
            Ok(((), changed))
        })
    }

    pub(super) fn ensure_enabled(&self) -> Result<(), String> {
        if !self.enabled {
            return Err("hosted control is disabled".to_string());
        }
        if let Some(error) = &self.init_error {
            return Err(format!("hosted control failed to initialize: {error}"));
        }
        Ok(())
    }

    pub(super) fn identity(&self) -> Result<&DaemonIdentity, String> {
        self.identity
            .as_deref()
            .ok_or_else(|| "hosted-control daemon identity is unavailable".to_string())
    }

    fn verify_doorbell(&self, request: &HostedLeaseRequest) -> Result<(), String> {
        if request.protocol != DOORBELL_PROTOCOL
            || request.daemon_id != self.daemon_id
            || request.document_sha256().is_empty()
            || !verify_b64u(
                &request.daemon_public_key,
                request.signing_payload().as_bytes(),
                &request.doorbell_signature,
            )
        {
            return Err("hosted lease request signature is invalid".to_string());
        }
        if self
            .identity
            .as_ref()
            .is_none_or(|identity| identity.public_key_b64u() != request.daemon_public_key)
        {
            return Err("hosted lease request names a different daemon identity".to_string());
        }
        Ok(())
    }

    fn load_verified_lease(
        &self,
        lease_id: &str,
        fleet_origin: &str,
        transport: &str,
    ) -> Result<VerifiedHostedLease, String> {
        let state = iam::load_state_cached_arc(&self.cert_dir)
            .map_err(|error| format!("load hosted lease state: {error}"))?;
        if compute_current_lane_guard(&state).status == HostedLaneGuardStatus::Suspended {
            return Err("hosted control is suspended by the certificate guard".to_string());
        }
        let lease = state
            .hosted_control
            .leases
            .iter()
            .find(|lease| lease.document.lease_id == lease_id)
            .ok_or_else(|| "hosted lease was not found".to_string())?;
        if lease.status != HostedLeaseStatus::Active {
            return Err("hosted lease is not active".to_string());
        }
        let document = &lease.document;
        if document.protocol != LEASE_PROTOCOL
            || document.daemon_id != self.daemon_id
            || document.fleet_origin != fleet_origin
            || document.document_sha256 != document.expected_document_sha256()
            || !verify_b64u(
                &document.daemon_public_key,
                document.signing_payload().as_bytes(),
                &document.signature,
            )
            || self
                .identity
                .as_ref()
                .is_none_or(|identity| identity.public_key_b64u() != document.daemon_public_key)
        {
            return Err("hosted lease document is invalid".to_string());
        }
        let document = lease.document.clone();
        let principal_record = state
            .principals
            .iter()
            .find(|principal| principal.id == document.principal_id)
            .ok_or_else(|| "hosted lease principal was not found".to_string())?;
        let grant = state
            .grants
            .iter()
            .find(|grant| grant.id == document.grant_id)
            .ok_or_else(|| "hosted lease grant was not found".to_string())?;
        let principal = AccessPrincipal {
            id: principal_record.id.clone(),
            kind: principal_record.kind.clone(),
            label: principal_record.label.clone(),
            source: principal_record.source.clone(),
            role_id: grant.role_id.clone(),
            grant_id: Some(grant.id.clone()),
            transport: transport.to_string(),
            peer_profile: None,
            account: None,
            organization: None,
            authn: principal_record.authn.clone(),
            authn_kind: Some(HOSTED_AUTHN_KIND.to_string()),
            authn_binding: Some(document.browser_key_fingerprint.clone()),
            authn_origin: Some(document.fleet_origin.clone()),
            hosted_connect: true,
        };
        super::hosted_preset_for_principal(&state, &principal)?;
        Ok(VerifiedHostedLease {
            principal,
            iam_state: state,
            document,
        })
    }

    fn record_nonce(
        &self,
        authority: &str,
        nonce: &str,
        timestamp_unix_ms: i64,
    ) -> Result<(), String> {
        record_nonce_in(
            &self.cert_dir,
            ReplayLane::Public,
            authority,
            nonce,
            timestamp_unix_ms,
        )
    }

    fn record_lease_nonce(
        &self,
        authority: &str,
        nonce: &str,
        timestamp_unix_ms: i64,
    ) -> Result<(), String> {
        record_nonce_in(
            &self.cert_dir,
            ReplayLane::Lease,
            authority,
            nonce,
            timestamp_unix_ms,
        )
    }

    fn check_doorbell_rate(
        &self,
        fingerprint: &str,
        source_bucket: Option<&str>,
        now: i64,
    ) -> Result<(), String> {
        let fingerprint = stable_id_digest(fingerprint);
        let source_bucket = source_bucket
            .filter(|source| !source.trim().is_empty())
            .map(stable_id_digest);
        mutate_shared_transient(&self.cert_dir, |state| {
            let cutoff = now.saturating_sub(60_000);
            let rate = &mut state.doorbell_rate;
            retain_recent(&mut rate.global, cutoff);
            rate.by_key.retain(|_, entries| {
                retain_recent(entries, cutoff);
                !entries.is_empty()
            });
            rate.by_source.retain(|_, entries| {
                retain_recent(entries, cutoff);
                !entries.is_empty()
            });
            if rate.global.len() >= DOORBELL_GLOBAL_PER_MINUTE {
                return Err("hosted lease request rate limit reached".to_string());
            }
            if rate.by_key.get(&fingerprint).map_or(0, VecDeque::len) >= DOORBELL_PER_KEY_PER_MINUTE
            {
                return Err("hosted lease request key rate limit reached".to_string());
            }
            if let Some(source) = source_bucket.as_ref() {
                let source_entries = rate.by_source.entry(source.clone()).or_default();
                if source_entries.len() >= 30 {
                    return Err("hosted lease request source rate limit reached".to_string());
                }
                source_entries.push_back(now);
            }
            rate.global.push_back(now);
            rate.by_key.entry(fingerprint).or_default().push_back(now);
            Ok(())
        })
    }

    fn check_poll_rate(&self, request_id: &str, now: i64) -> Result<(), String> {
        let request_id = stable_id_digest(request_id);
        mutate_shared_transient(&self.cert_dir, |state| {
            let cutoff = now.saturating_sub(60_000);
            let rate = &mut state.poll_rate;
            retain_recent(&mut rate.global, cutoff);
            rate.by_request.retain(|_, entries| {
                retain_recent(entries, cutoff);
                !entries.is_empty()
            });
            if rate.global.len() >= POLL_GLOBAL_PER_MINUTE {
                return Err("hosted lease poll global rate limit reached".to_string());
            }
            if rate.by_request.get(&request_id).map_or(0, VecDeque::len)
                >= POLL_PER_REQUEST_PER_MINUTE
            {
                return Err("hosted lease poll request rate limit reached".to_string());
            }
            rate.by_request
                .entry(request_id)
                .or_default()
                .push_back(now);
            rate.global.push_back(now);
            Ok(())
        })
    }
}

fn materialize_hosted_lease_expirations(state: &mut LocalIamState, now: u64, actor: &str) -> usize {
    let mut expired_leases = Vec::new();
    for lease in &mut state.hosted_control.leases {
        if lease.status == HostedLeaseStatus::Active && lease.document.expires_unix_ms <= now {
            lease.status = HostedLeaseStatus::Expired;
            expired_leases.push((
                lease.document.lease_id.clone(),
                lease.document.grant_id.clone(),
            ));
        }
    }
    for (_, grant_id) in &expired_leases {
        if let Some(grant) = state.grants.iter_mut().find(|grant| grant.id == *grant_id) {
            grant.status = "expired".to_string();
        }
    }
    for (lease_id, _) in &expired_leases {
        push_audit(
            state,
            actor,
            "hosted_lease_expire",
            lease_id,
            "Observed expired hosted lease".to_string(),
        );
    }
    expired_leases.len()
}

fn issue_lease_record(
    state: &mut LocalIamState,
    request: &HostedLeaseRequest,
    preset: HostedPreset,
    ttl_secs: u64,
    actor_id: &str,
    identity: &DaemonIdentity,
    daemon_id: &str,
) -> AccessResult<HostedLeaseDocument> {
    let now = now_ms().max(0) as u64;
    materialize_hosted_lease_expirations(state, now, actor_id);
    iam::normalize_hosted_lease_bindings(state);
    let active_leases = state
        .hosted_control
        .leases
        .iter()
        .filter(|lease| {
            lease.status == HostedLeaseStatus::Active && lease.document.expires_unix_ms > now
        })
        .count();
    if active_leases >= HOSTED_LEASES_CAP {
        return Err(AccessError(
            "hosted lease capacity is full; retry after an active lease expires or is revoked"
                .to_string(),
        ));
    }
    let stable = stable_id_digest(&format!(
        "{}\n{}",
        request.request_id, request.browser_key_fingerprint
    ));
    let lease_id = format!("lease:{stable}");
    let principal_id = format!("principal:hosted-lease:{stable}");
    let grant_id = format!("grant:hosted-lease:{stable}");
    let expires = now.saturating_add(ttl_secs.saturating_mul(1000));
    let principal = IamPrincipal {
        id: principal_id.clone(),
        kind: HOSTED_PRINCIPAL_KIND.to_string(),
        label: format!("Hosted lease {}", &stable[..12]),
        status: "active".to_string(),
        source: HOSTED_SOURCE.to_string(),
        account: None,
        organization: None,
        authn: vec![json!({
            "kind": HOSTED_AUTHN_KIND,
            "fingerprint": request.browser_key_fingerprint,
            "public_key": request.browser_public_key,
        })],
        notes: None,
        created_at_unix_ms: Some(now),
    };
    let grant = IamGrant {
        id: grant_id.clone(),
        principal_id: principal_id.clone(),
        target_id: "daemon:self".to_string(),
        role_id: preset.role_id().to_string(),
        policy_id: "policy:hosted-control-compiled".to_string(),
        status: "active".to_string(),
        source: HOSTED_SOURCE.to_string(),
        reason: "daemon-local hosted lease approval".to_string(),
        created_at_unix_ms: Some(now),
        revoked_at_unix_ms: None,
        expires_at_unix_ms: Some(expires),
        issued_via: None,
        fs_scope: None,
    };
    let mut document = HostedLeaseDocument {
        protocol: LEASE_PROTOCOL.to_string(),
        lease_id: lease_id.clone(),
        request_id: request.request_id.clone(),
        daemon_id: daemon_id.to_string(),
        daemon_public_key: identity.public_key_b64u(),
        fleet_origin: request.fleet_origin.clone(),
        browser_public_key: request.browser_public_key.clone(),
        browser_key_fingerprint: request.browser_key_fingerprint.clone(),
        preset,
        issued_unix_ms: now,
        expires_unix_ms: expires,
        principal_id: principal_id.clone(),
        grant_id: grant_id.clone(),
        document_sha256: String::new(),
        signature: String::new(),
    };
    document.document_sha256 = document.expected_document_sha256();
    document.signature = identity.sign_b64u(document.signing_payload().as_bytes());
    state.principals.push(principal);
    state.grants.push(grant);
    state.hosted_control.leases.push(HostedLeaseRecord {
        document: document.clone(),
        status: HostedLeaseStatus::Active,
        revoked_at_unix_ms: None,
        revoked_by: None,
    });
    iam::normalize_hosted_lease_bindings(state);
    push_audit(
        state,
        actor_id,
        "hosted_lease_issue",
        &lease_id,
        format!("Issued {} lease for {} seconds", preset.as_str(), ttl_secs),
    );
    Ok(document)
}

fn record_nonce_in(
    cert_dir: &Path,
    lane: ReplayLane,
    authority: &str,
    nonce: &str,
    timestamp_unix_ms: i64,
) -> Result<(), String> {
    let authority_digest = stable_id_digest(authority);
    let nonce_digest = stable_id_digest(nonce);
    mutate_shared_transient(cert_dir, |state| {
        let replay = match lane {
            ReplayLane::Public => &mut state.public_replay,
            ReplayLane::Lease => &mut state.lease_replay,
        };
        if replay.len() >= PROOF_NONCES_GLOBAL_CAP {
            return Err("hosted proof replay window is full".to_string());
        }
        if replay.iter().any(|record| {
            record.authority_digest == authority_digest && record.nonce_digest == nonce_digest
        }) {
            return Err("hosted proof nonce was already used".to_string());
        }
        if replay
            .iter()
            .filter(|record| record.authority_digest == authority_digest)
            .count()
            >= PROOF_NONCES_PER_LEASE_CAP
        {
            return Err("hosted proof nonce window is full for this authority".to_string());
        }
        replay.push(ReplayRecord {
            authority_digest,
            nonce_digest,
            timestamp_unix_ms,
        });
        Ok(())
    })
}

fn shared_transient_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(SHARED_TRANSIENT_FILE)
}

fn valid_rate_digest(value: &str) -> bool {
    value.len() == 43 && valid_id_component(value)
}

fn doorbell_rate_state_is_valid(rate: &DoorbellRateState) -> bool {
    rate.global.len() <= DOORBELL_GLOBAL_PER_MINUTE
        && rate.by_key.len() <= DOORBELL_GLOBAL_PER_MINUTE
        && rate.by_source.len() <= DOORBELL_GLOBAL_PER_MINUTE
        && rate.by_key.iter().all(|(key, entries)| {
            valid_rate_digest(key) && entries.len() <= DOORBELL_PER_KEY_PER_MINUTE
        })
        && rate
            .by_source
            .iter()
            .all(|(source, entries)| valid_rate_digest(source) && entries.len() <= 30)
        && rate.by_key.values().map(VecDeque::len).sum::<usize>() <= rate.global.len()
        && rate.by_source.values().map(VecDeque::len).sum::<usize>() <= rate.global.len()
}

fn poll_rate_state_is_valid(rate: &PollRateState) -> bool {
    rate.global.len() <= POLL_GLOBAL_PER_MINUTE
        && rate.by_request.len() <= POLL_GLOBAL_PER_MINUTE
        && rate.by_request.iter().all(|(request, entries)| {
            valid_rate_digest(request) && entries.len() <= POLL_PER_REQUEST_PER_MINUTE
        })
        && rate.by_request.values().map(VecDeque::len).sum::<usize>() <= rate.global.len()
}

fn load_shared_transient_locked(cert_dir: &Path) -> Result<SharedTransientState, String> {
    use std::io::Read as _;

    let path = shared_transient_path(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SharedTransientState::default());
        }
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(SHARED_TRANSIENT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > SHARED_TRANSIENT_MAX_BYTES {
        return Err(format!(
            "{} exceeds the hosted transient-state size cap",
            path.display()
        ));
    }
    let mut state: SharedTransientState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if state.schema_version == 1 {
        state.schema_version = SHARED_TRANSIENT_SCHEMA_VERSION;
    }
    if state.schema_version != SHARED_TRANSIENT_SCHEMA_VERSION
        || state.public_replay.len() > PROOF_NONCES_GLOBAL_CAP
        || state.lease_replay.len() > PROOF_NONCES_GLOBAL_CAP
        || state.tickets.len() > WS_TICKETS_GLOBAL_CAP
        || !doorbell_rate_state_is_valid(&state.doorbell_rate)
        || !poll_rate_state_is_valid(&state.poll_rate)
        || state
            .public_replay
            .iter()
            .chain(&state.lease_replay)
            .any(|record| {
                record.authority_digest.len() != 43
                    || record.nonce_digest.len() != 43
                    || !valid_id_component(&record.authority_digest)
                    || !valid_id_component(&record.nonce_digest)
            })
        || state.tickets.iter().any(|(ticket, record)| {
            !valid_id_component(ticket)
                || !valid_id_component(&record.lease_id)
                || !valid_id_component(&record.grant_id)
                || validate_fleet_origin(&record.fleet_origin).is_err()
        })
    {
        return Err(format!(
            "{} contains invalid hosted transient state",
            path.display()
        ));
    }
    let replay_cutoff = now_ms().saturating_sub(REQUEST_PROOF_MAX_SKEW_MS);
    state
        .public_replay
        .retain(|record| record.timestamp_unix_ms >= replay_cutoff);
    state
        .lease_replay
        .retain(|record| record.timestamp_unix_ms >= replay_cutoff);
    let rate_cutoff = now_ms().saturating_sub(60_000);
    retain_recent(&mut state.doorbell_rate.global, rate_cutoff);
    state.doorbell_rate.by_key.retain(|_, entries| {
        retain_recent(entries, rate_cutoff);
        !entries.is_empty()
    });
    state.doorbell_rate.by_source.retain(|_, entries| {
        retain_recent(entries, rate_cutoff);
        !entries.is_empty()
    });
    retain_recent(&mut state.poll_rate.global, rate_cutoff);
    state.poll_rate.by_request.retain(|_, entries| {
        retain_recent(entries, rate_cutoff);
        !entries.is_empty()
    });
    Ok(state)
}

fn write_shared_transient_locked(
    cert_dir: &Path,
    state: &SharedTransientState,
) -> AccessResult<()> {
    let bytes = serde_json::to_vec(state)
        .map_err(|error| AccessError(format!("serialize hosted transient state: {error}")))?;
    if bytes.len() as u64 > SHARED_TRANSIENT_MAX_BYTES {
        return Err(AccessError(
            "hosted transient state exceeds its size cap".to_string(),
        ));
    }
    crate::access::authority_store::atomic_write_private_locked(
        &shared_transient_path(cert_dir),
        &bytes,
    )
}

fn mutate_shared_transient<T>(
    cert_dir: &Path,
    update: impl FnOnce(&mut SharedTransientState) -> Result<T, String>,
) -> Result<T, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut state =
            load_shared_transient_locked(cert_dir).map_err(crate::access::AccessError)?;
        let result = update(&mut state).map_err(crate::access::AccessError)?;
        write_shared_transient_locked(cert_dir, &state)?;
        Ok(result)
    })
    .map_err(|error| error.to_string())
}

fn project_request_status(mut request: HostedLeaseRequest, now_unix_ms: u64) -> HostedLeaseRequest {
    if request.status == HostedLeaseRequestStatus::Pending && request.expires_unix_ms <= now_unix_ms
    {
        request.status = HostedLeaseRequestStatus::Expired;
    }
    request
}

pub(super) fn validate_fleet_origin(origin: &str) -> Result<String, String> {
    let parsed = url::Url::parse(origin.trim())
        .map_err(|_| "fleet origin is not a valid URL".to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("fleet origin must be an HTTPS origin without path or credentials".to_string());
    }
    Ok(parsed.origin().ascii_serialization())
}

fn validate_browser_public_key(value: &str) -> Result<(String, String), String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.trim())
        .map_err(|_| "browser public key is not valid base64url".to_string())?;
    if bytes.len() != 65 || bytes.first() != Some(&0x04) {
        return Err("browser public key must be an uncompressed P-256 point".to_string());
    }
    // Ring validates that the point is on the curve during signature checks.
    // The doorbell needs a stable identity before a signature exists, so it
    // performs the exact encoded-point shape check here and binds every later
    // proof to these bytes.
    Ok((
        b64u(&bytes),
        crate::access::client_key::client_key_fingerprint(&bytes),
    ))
}

pub(super) fn verify_p256_signature(
    public_key: &str,
    payload: &[u8],
    signature: &str,
) -> Result<(), String> {
    let engine = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let key = engine
        .decode(public_key)
        .map_err(|_| "hosted proof public key is invalid".to_string())?;
    let signature = engine
        .decode(signature)
        .map_err(|_| "hosted proof signature is invalid base64url".to_string())?;
    if key.len() != 65 || key.first() != Some(&0x04) || signature.len() != 64 {
        return Err("hosted proof key or signature has an invalid shape".to_string());
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, key)
        .verify(payload, &signature)
        .map_err(|_| "hosted request proof signature verification failed".to_string())
}

fn verify_timestamp(timestamp_unix_ms: i64) -> Result<(), String> {
    let skew = now_ms().saturating_sub(timestamp_unix_ms).abs();
    if skew > REQUEST_PROOF_MAX_SKEW_MS {
        return Err(format!(
            "hosted proof timestamp is outside the {REQUEST_PROOF_MAX_SKEW_MS}ms window"
        ));
    }
    Ok(())
}

fn random_b64u(bytes: usize) -> Result<String, String> {
    use ring::rand::SecureRandom as _;
    let mut output = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut output)
        .map_err(|_| "generate hosted-control random value".to_string())?;
    Ok(b64u(&output))
}

fn stable_id_digest(value: &str) -> String {
    b64u(ring::digest::digest(&ring::digest::SHA256, value.as_bytes()).as_ref())
}

pub(super) fn now_ms() -> i64 {
    crate::access::client_key::now_unix_ms()
}

pub(super) fn retain_recent(entries: &mut VecDeque<i64>, cutoff: i64) {
    while entries.front().is_some_and(|timestamp| *timestamp < cutoff) {
        entries.pop_front();
    }
}

pub(super) fn push_audit(
    state: &mut LocalIamState,
    actor: &str,
    action: &str,
    target: &str,
    summary: String,
) {
    state.audit_events.push(IamAuditEvent {
        id: format!("audit:hosted:{}", uuid::Uuid::new_v4().simple()),
        at_unix_ms: Some(now_ms() as u64),
        actor_principal_id: actor.to_string(),
        action: action.to_string(),
        target_id: target.to_string(),
        summary,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

    struct BrowserKey {
        pair: EcdsaKeyPair,
        public_key: String,
    }

    fn browser_key() -> BrowserKey {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        BrowserKey {
            public_key: b64u(pair.public_key().as_ref()),
            pair,
        }
    }

    fn sign(key: &BrowserKey, payload: &str) -> String {
        let rng = ring::rand::SystemRandom::new();
        b64u(key.pair.sign(&rng, payload.as_bytes()).unwrap().as_ref())
    }

    fn doorbell_input(
        key: &BrowserKey,
        preset: HostedPreset,
        ttl_secs: u64,
    ) -> HostedLeaseRequestInput {
        let mut input = HostedLeaseRequestInput {
            browser_public_key: key.public_key.clone(),
            requested_preset: preset,
            requested_ttl_secs: ttl_secs,
            requester_label: "Test browser".to_string(),
            nonce: format!("nonce-{}", uuid::Uuid::new_v4().simple()),
            timestamp_unix_ms: now_ms(),
            signature: String::new(),
        };
        input.signature = sign(
            key,
            &input.proof_payload("daemon-test", "https://laptop.example.test"),
        );
        input
    }

    fn runtime(temp: &tempfile::TempDir) -> HostedControlRuntime {
        HostedControlRuntime::new(
            true,
            temp.path().join("access"),
            Some(&temp.path().join("identity.pk8")),
            Some("daemon-test"),
            "Test daemon".to_string(),
            false,
        )
    }

    fn shared_transient(runtime: &HostedControlRuntime) -> SharedTransientState {
        crate::access::authority_store::with_lock(&runtime.cert_dir, || {
            load_shared_transient_locked(&runtime.cert_dir).map_err(crate::access::AccessError)
        })
        .unwrap()
    }

    fn replace_shared_transient(
        runtime: &HostedControlRuntime,
        update: impl FnOnce(&mut SharedTransientState),
    ) {
        crate::access::authority_store::with_lock(&runtime.cert_dir, || {
            let mut state = load_shared_transient_locked(&runtime.cert_dir)
                .map_err(crate::access::AccessError)?;
            update(&mut state);
            write_shared_transient_locked(&runtime.cert_dir, &state)
        })
        .unwrap();
    }

    fn owner() -> AccessPrincipal {
        AccessPrincipal::root_dashboard_session("test", "test")
    }

    fn issue_lease(
        runtime: &HostedControlRuntime,
        key: &BrowserKey,
        preset: HostedPreset,
        ttl_secs: u64,
    ) -> (HostedLeaseRequest, HostedLeaseDocument) {
        let request = runtime
            .create_request(
                doorbell_input(key, preset, ttl_secs),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let document = runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: request.request_id.clone(),
                    approve: true,
                    approved_preset: None,
                    approved_ttl_secs: None,
                },
                &owner(),
            )
            .unwrap()
            .unwrap();
        (request, document)
    }

    fn request_proof(
        key: &BrowserKey,
        document: &HostedLeaseDocument,
        method: &str,
        path: &str,
        nonce: &str,
        timestamp_unix_ms: i64,
    ) -> HostedRequestProof {
        let payload = format!(
            "{REQUEST_PROOF_PROTOCOL}\n{}\n{path}\ndaemon-test\n{}\n{nonce}\n{timestamp_unix_ms}",
            method.to_ascii_uppercase(),
            document.document_sha256,
        );
        HostedRequestProof {
            lease_id: document.lease_id.clone(),
            nonce: nonce.to_string(),
            timestamp_unix_ms,
            signature: sign(key, &payload),
        }
    }

    #[test]
    fn dark_runtime_does_not_touch_identity_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("must-not-exist.pk8");
        let runtime = HostedControlRuntime::new(
            false,
            temp.path().join("access"),
            Some(&path),
            Some("daemon-test"),
            "Test".to_string(),
            false,
        );
        assert!(!runtime.enabled());
        assert!(!path.exists());
        assert!(runtime
            .set_policy(HostedPreset::Operate, 3600, &owner(), true)
            .unwrap_err()
            .to_string()
            .contains("disabled"));
        assert!(!path.exists());
    }

    #[test]
    fn approval_is_idempotent_and_proofs_are_non_replayable() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let decision = HostedLeaseDecisionInput {
            request_id: request.request_id,
            approve: true,
            approved_preset: None,
            approved_ttl_secs: None,
        };
        let first = runtime
            .decide_request(decision.clone(), &owner())
            .unwrap()
            .unwrap();
        let second = runtime.decide_request(decision, &owner()).unwrap().unwrap();
        assert_eq!(first, second);

        let timestamp = now_ms();
        let nonce = "nonce-1";
        let path = "/api/sessions?limit=20";
        let payload = format!(
            "{REQUEST_PROOF_PROTOCOL}\nGET\n{path}\ndaemon-test\n{}\n{nonce}\n{timestamp}",
            first.document_sha256
        );
        let proof = HostedRequestProof {
            lease_id: first.lease_id,
            nonce: nonce.to_string(),
            timestamp_unix_ms: timestamp,
            signature: sign(&key, &payload),
        };
        assert!(runtime
            .verify_request_proof("GET", path, "https://laptop.example.test", &proof, "relay")
            .is_ok());
        assert!(runtime
            .verify_request_proof("GET", path, "https://laptop.example.test", &proof, "relay")
            .unwrap_err()
            .contains("already used"));
    }

    #[test]
    fn hosted_request_proof_replay_is_shared_across_daemon_processes() {
        let temp = tempfile::tempdir().unwrap();
        let first = runtime(&temp);
        let second = runtime(&temp);
        let key = browser_key();
        let (_, document) = issue_lease(&first, &key, HostedPreset::Tasks, 3600);
        let proof = request_proof(
            &key,
            &document,
            "POST",
            "/api/sessions",
            "cross-process-replay",
            now_ms(),
        );
        first
            .verify_request_proof(
                "POST",
                "/api/sessions",
                "https://laptop.example.test",
                &proof,
                "relay",
            )
            .unwrap();
        assert!(second
            .verify_request_proof(
                "POST",
                "/api/sessions",
                "https://laptop.example.test",
                &proof,
                "relay",
            )
            .unwrap_err()
            .contains("already used"));
    }

    #[test]
    fn suspended_certificate_guard_stops_every_lease_admission_path() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let (_, document) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);
        let verified = runtime
            .verify_request_proof(
                "GET",
                "/api/sessions",
                "https://laptop.example.test",
                &request_proof(
                    &key,
                    &document,
                    "GET",
                    "/api/sessions",
                    "before-suspension",
                    now_ms(),
                ),
                "relay",
            )
            .unwrap();
        let ticket = runtime.mint_ws_ticket(&verified).unwrap();
        let pending_key = browser_key();
        let pending = runtime
            .create_request(
                doorbell_input(&pending_key, HostedPreset::View, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        iam::transact_state(&runtime.cert_dir, |state, _| {
            state
                .hosted_control
                .witnesses
                .owner_confirmed_serials
                .push("abc".to_string());
            Ok(((), true))
        })
        .unwrap();

        let public_guard = serde_json::to_value(
            runtime
                .bootstrap("https://laptop.example.test")
                .unwrap()
                .lane_guard,
        )
        .unwrap();
        assert_eq!(public_guard, serde_json::json!({"status": "suspended"}));
        assert!(runtime
            .mint_ws_ticket(&verified)
            .unwrap_err()
            .contains("suspended"));
        assert!(runtime
            .consume_ws_ticket(&ticket.ticket, "https://laptop.example.test", "relay")
            .unwrap_err()
            .contains("suspended"));
        assert!(runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: pending.request_id.clone(),
                    approve: true,
                    approved_preset: None,
                    approved_ttl_secs: None,
                },
                &owner(),
            )
            .unwrap_err()
            .contains("suspended"));
        assert!(runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: pending.request_id,
                    approve: false,
                    approved_preset: None,
                    approved_ttl_secs: None,
                },
                &owner(),
            )
            .unwrap()
            .is_none());
        assert!(runtime
            .verify_request_proof(
                "GET",
                "/api/sessions",
                "https://laptop.example.test",
                &request_proof(
                    &key,
                    &document,
                    "GET",
                    "/api/sessions",
                    "after-suspension",
                    now_ms(),
                ),
                "relay",
            )
            .unwrap_err()
            .contains("suspended"));
        let state = iam::load_state_cached_arc(&runtime.cert_dir).unwrap();
        assert!(
            crate::access::hosted_control::hosted_preset_for_principal(
                &state,
                &verified.principal,
            )
            .unwrap_err()
            .contains("suspended"),
            "a live hosted socket must fail its next authority recheck"
        );
        assert!(mark_session_created_by_hosted_lease(
            &runtime.cert_dir,
            &document.lease_id,
            "session-after-suspension",
        )
        .unwrap_err()
        .to_string()
        .contains("suspended"));
        let new_key = browser_key();
        assert!(runtime
            .create_request(
                doorbell_input(&new_key, HostedPreset::View, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap_err()
            .contains("suspended"));
    }

    #[test]
    fn doorbell_creation_requires_exact_key_proof_and_closed_input() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let input = doorbell_input(&key, HostedPreset::Tasks, 3600);
        verify_p256_signature(
            &input.browser_public_key,
            input
                .proof_payload("daemon-test", "https://laptop.example.test")
                .as_bytes(),
            &input.signature,
        )
        .unwrap();

        let mut mutations = Vec::new();
        let mut altered = input.clone();
        altered.browser_public_key = browser_key().public_key;
        mutations.push(("browser key", altered));
        let mut altered = input.clone();
        altered.requested_preset = HostedPreset::View;
        mutations.push(("preset", altered));
        let mut altered = input.clone();
        altered.requested_ttl_secs -= 1;
        mutations.push(("ttl", altered));
        let mut altered = input.clone();
        altered.requester_label.push('!');
        mutations.push(("label", altered));
        let mut altered = input.clone();
        altered.nonce.push('x');
        mutations.push(("nonce", altered));
        let mut altered = input.clone();
        altered.timestamp_unix_ms += 1;
        mutations.push(("timestamp", altered));
        for (field, altered) in mutations {
            assert!(
                verify_p256_signature(
                    &input.browser_public_key,
                    altered
                        .proof_payload("daemon-test", "https://laptop.example.test")
                        .as_bytes(),
                    &input.signature,
                )
                .is_err(),
                "doorbell proof did not bind {field}",
            );
        }
        assert!(verify_p256_signature(
            &input.browser_public_key,
            input
                .proof_payload("other-daemon", "https://laptop.example.test")
                .as_bytes(),
            &input.signature,
        )
        .is_err());
        assert!(verify_p256_signature(
            &input.browser_public_key,
            input
                .proof_payload("daemon-test", "https://other.example.test")
                .as_bytes(),
            &input.signature,
        )
        .is_err());

        runtime
            .create_request(
                input.clone(),
                "https://laptop.example.test",
                Some("198.51.100.1"),
            )
            .unwrap();
        assert!(runtime
            .create_request(input, "https://laptop.example.test", None)
            .unwrap_err()
            .contains("already used"));

        let mut json =
            serde_json::to_value(doorbell_input(&browser_key(), HostedPreset::Tasks, 3600))
                .unwrap();
        json["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<HostedLeaseRequestInput>(json).is_err());
    }

    #[test]
    fn rejected_doorbell_rate_limit_does_not_fill_the_proof_replay_window() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let now = now_ms();
        replace_shared_transient(&runtime, |state| {
            state
                .doorbell_rate
                .global
                .extend(std::iter::repeat_n(now, DOORBELL_GLOBAL_PER_MINUTE));
        });
        let key = browser_key();
        let error = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap_err();
        assert!(error.contains("rate limit"));
        assert!(
            shared_transient(&runtime).public_replay.is_empty(),
            "rate-limited doorbells must not consume the shared proof nonce window"
        );
    }

    #[test]
    fn passkey_authorized_issuance_ignores_anonymous_queue_and_rate_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let seed_key = browser_key();
        let seed = runtime
            .create_request(
                doorbell_input(&seed_key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        iam::transact_state(&runtime.cert_dir, |state, _| {
            state.hosted_control.requests = (0..HOSTED_REQUESTS_CAP)
                .map(|index| {
                    let mut request = seed.clone();
                    request.request_id = format!("request:anonymous-capacity-{index}");
                    request.request_nonce = format!("pending-{index}");
                    request.created_unix_ms = now_ms().max(0) as u64;
                    request.expires_unix_ms = request
                        .created_unix_ms
                        .saturating_add(PENDING_REQUEST_TTL_MS);
                    request.status = HostedLeaseRequestStatus::Pending;
                    request.approved_lease_id = None;
                    request.doorbell_signature = runtime
                        .identity()
                        .unwrap()
                        .sign_b64u(request.signing_payload().as_bytes());
                    request
                })
                .collect();
            Ok(((), true))
        })
        .unwrap();
        replace_shared_transient(&runtime, |state| {
            state.doorbell_rate.global =
                std::iter::repeat_n(now_ms(), DOORBELL_GLOBAL_PER_MINUTE).collect();
            state.doorbell_rate.by_key.clear();
            state.doorbell_rate.by_source.clear();
        });

        let passkey_browser = browser_key();
        let lease = runtime
            .issue_passkey_lease(
                doorbell_input(&passkey_browser, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                &owner(),
            )
            .unwrap();
        assert_eq!(lease.preset, HostedPreset::Tasks);
        let state = iam::load_state_cached_arc(&runtime.cert_dir).unwrap();
        assert_eq!(
            state.hosted_control.requests.len(),
            HOSTED_REQUESTS_CAP,
            "passkey issuance must not enqueue an anonymous request"
        );
        assert!(state
            .hosted_control
            .leases
            .iter()
            .any(|record| record.document.lease_id == lease.lease_id));
    }

    #[test]
    fn full_active_lease_capacity_refuses_new_issuance_without_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let seed = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let actor = owner();
        let identity = runtime.identity().unwrap();
        let mut state = LocalIamState::default();
        let mut first_lease_id = String::new();
        for index in 0..HOSTED_LEASES_CAP {
            let mut request = seed.clone();
            request.request_id = format!("request:active-capacity-{index}");
            request.browser_key_fingerprint = format!("fingerprint-{index}");
            let document = issue_lease_record(
                &mut state,
                &request,
                HostedPreset::Tasks,
                3600,
                &actor,
                identity,
                "daemon-test",
            )
            .unwrap();
            if index == 0 {
                first_lease_id = document.lease_id;
            }
        }

        let mut overflow = seed;
        overflow.request_id = "request:active-capacity-overflow".to_string();
        overflow.browser_key_fingerprint = "fingerprint-overflow".to_string();
        let error = issue_lease_record(
            &mut state,
            &overflow,
            HostedPreset::Tasks,
            3600,
            &actor,
            identity,
            "daemon-test",
        )
        .unwrap_err();
        assert!(error.to_string().contains("capacity is full"));
        assert_eq!(state.hosted_control.leases.len(), HOSTED_LEASES_CAP);
        assert!(state
            .hosted_control
            .leases
            .iter()
            .any(|lease| lease.document.lease_id == first_lease_id));
        assert_eq!(
            state
                .principals
                .iter()
                .filter(|principal| principal.source == HOSTED_SOURCE)
                .count(),
            HOSTED_LEASES_CAP
        );
        assert_eq!(
            state
                .grants
                .iter()
                .filter(|grant| grant.source == HOSTED_SOURCE)
                .count(),
            HOSTED_LEASES_CAP
        );
    }

    #[test]
    fn sustained_lease_turnover_keeps_records_and_iam_bindings_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let seed = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let actor = owner();
        let identity = runtime.identity().unwrap();
        let mut state = LocalIamState::default();
        let mut newest_lease_id = String::new();

        for index in 0..(HOSTED_LEASES_CAP + 32) {
            for lease in &mut state.hosted_control.leases {
                if lease.status == HostedLeaseStatus::Active {
                    lease.document.expires_unix_ms = 0;
                }
            }
            let mut request = seed.clone();
            request.request_id = format!("request:turnover-{index}");
            request.browser_key_fingerprint = format!("turnover-fingerprint-{index}");
            newest_lease_id = issue_lease_record(
                &mut state,
                &request,
                HostedPreset::Tasks,
                3600,
                &actor,
                identity,
                "daemon-test",
            )
            .unwrap()
            .lease_id;

            assert!(state.hosted_control.leases.len() <= HOSTED_LEASES_CAP);
            assert_eq!(
                state
                    .principals
                    .iter()
                    .filter(|principal| principal.source == HOSTED_SOURCE)
                    .count(),
                1
            );
            assert_eq!(
                state
                    .grants
                    .iter()
                    .filter(|grant| grant.source == HOSTED_SOURCE)
                    .count(),
                1
            );
        }

        assert_eq!(state.hosted_control.leases.len(), HOSTED_LEASES_CAP);
        assert!(state.hosted_control.leases.iter().any(|lease| {
            lease.document.lease_id == newest_lease_id && lease.status == HostedLeaseStatus::Active
        }));
    }

    #[test]
    fn invalid_doorbell_signatures_consume_the_preverification_rate_budget() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        for _ in 0..DOORBELL_PER_KEY_PER_MINUTE {
            let mut input = doorbell_input(&key, HostedPreset::Tasks, 3600);
            input.signature = b64u(&[0; 64]);
            assert!(runtime
                .create_request(input, "https://laptop.example.test", None)
                .unwrap_err()
                .contains("signature verification"));
        }
        let mut limited = doorbell_input(&key, HostedPreset::Tasks, 3600);
        limited.signature = b64u(&[0; 64]);
        assert!(runtime
            .create_request(limited, "https://laptop.example.test", None)
            .unwrap_err()
            .contains("key rate limit"));
        assert!(
            shared_transient(&runtime).public_replay.is_empty(),
            "invalid signatures must not enter the replay cache"
        );
    }

    #[test]
    fn public_replay_capacity_cannot_starve_an_active_lease() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let (_, document) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);
        let now = now_ms();
        replace_shared_transient(&runtime, |state| {
            state.public_replay = (0..PROOF_NONCES_GLOBAL_CAP)
                .map(|index| ReplayRecord {
                    authority_digest: stable_id_digest(&format!("public-authority-{index}")),
                    nonce_digest: stable_id_digest(&format!("public-nonce-{index}")),
                    timestamp_unix_ms: now,
                })
                .collect();
        });

        let proof = request_proof(
            &key,
            &document,
            "GET",
            "/api/sessions",
            "lease-independent",
            now,
        );
        assert!(runtime
            .verify_request_proof(
                "GET",
                "/api/sessions",
                "https://laptop.example.test",
                &proof,
                "relay",
            )
            .is_ok());
        assert_eq!(shared_transient(&runtime).lease_replay.len(), 1);
    }

    #[test]
    fn public_polling_is_globally_rate_limited_before_signature_verification() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let now = now_ms();
        replace_shared_transient(&runtime, |state| {
            state
                .poll_rate
                .global
                .extend(std::iter::repeat_n(now, POLL_GLOBAL_PER_MINUTE));
        });
        let proof = HostedLeasePollProof {
            request_id: request.request_id,
            nonce: "poll-rate-limit".to_string(),
            timestamp_unix_ms: now,
            signature: b64u(&[0; 64]),
        };
        assert!(runtime
            .poll_request(&proof)
            .unwrap_err()
            .contains("global rate limit"));
        assert_eq!(
            shared_transient(&runtime).public_replay.len(),
            1,
            "the rejected poll must not consume replay capacity"
        );
    }

    #[test]
    fn public_admission_rate_windows_are_shared_between_runtime_instances() {
        let temp = tempfile::tempdir().unwrap();
        let first = runtime(&temp);
        let sibling = runtime(&temp);
        let now = now_ms();
        replace_shared_transient(&first, |state| {
            state
                .doorbell_rate
                .global
                .extend(std::iter::repeat_n(now, DOORBELL_GLOBAL_PER_MINUTE));
            state
                .poll_rate
                .global
                .extend(std::iter::repeat_n(now, POLL_GLOBAL_PER_MINUTE));
        });

        assert!(sibling
            .check_doorbell_rate("sibling-key", Some("sibling-source"), now)
            .unwrap_err()
            .contains("rate limit"));
        assert!(sibling
            .check_poll_rate("request:sibling", now)
            .unwrap_err()
            .contains("global rate limit"));
    }

    #[test]
    fn legacy_shared_transient_state_migrates_with_empty_rate_windows() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        std::fs::create_dir_all(&runtime.cert_dir).unwrap();
        std::fs::write(
            shared_transient_path(&runtime.cert_dir),
            br#"{"schema_version":1,"public_replay":[],"lease_replay":[],"tickets":{}}"#,
        )
        .unwrap();

        let migrated = shared_transient(&runtime);
        assert_eq!(migrated.schema_version, SHARED_TRANSIENT_SCHEMA_VERSION);
        assert!(migrated.doorbell_rate.global.is_empty());
        assert!(migrated.poll_rate.global.is_empty());
    }

    #[test]
    fn request_retention_preserves_pending_owner_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let pending = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        iam::transact_state(&runtime.cert_dir, |state, _| {
            for index in 0..HOSTED_REQUESTS_CAP {
                let mut completed = pending.clone();
                completed.request_id = format!("request:completed-{index}");
                completed.status = HostedLeaseRequestStatus::Denied;
                state.hosted_control.requests.push(completed);
            }
            state.hosted_control.normalize();
            Ok(((), true))
        })
        .unwrap();
        let state = iam::load_state_cached_arc(&runtime.cert_dir).unwrap();
        assert_eq!(state.hosted_control.requests.len(), HOSTED_REQUESTS_CAP);
        assert!(state
            .hosted_control
            .requests
            .iter()
            .any(|request| request.request_id == pending.request_id
                && request.status == HostedLeaseRequestStatus::Pending));
    }

    #[test]
    fn a_full_pending_queue_refuses_new_requests_without_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let first = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        iam::transact_state(&runtime.cert_dir, |state, _| {
            for index in 1..HOSTED_REQUESTS_CAP {
                let mut pending = first.clone();
                pending.request_id = format!("request:pending-{index}");
                state.hosted_control.requests.push(pending);
            }
            Ok(((), true))
        })
        .unwrap();
        assert!(runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap_err()
            .contains("queue is full"));
        let state = iam::load_state_cached_arc(&runtime.cert_dir).unwrap();
        assert_eq!(state.hosted_control.requests.len(), HOSTED_REQUESTS_CAP);
        assert!(state
            .hosted_control
            .requests
            .iter()
            .any(|request| request.request_id == first.request_id));
    }

    #[test]
    fn doorbell_poll_requires_the_request_key_and_a_fresh_nonce() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let nonce = "poll-proof";
        let timestamp = now_ms();
        let payload = format!(
            "{POLL_PROOF_PROTOCOL}\n{}\n{}\n{nonce}\n{timestamp}",
            request.request_id,
            request.document_sha256(),
        );
        let proof = HostedLeasePollProof {
            request_id: request.request_id.clone(),
            nonce: nonce.to_string(),
            timestamp_unix_ms: timestamp,
            signature: sign(&key, &payload),
        };
        assert!(runtime.poll_request(&proof).is_ok());
        assert!(runtime
            .poll_request(&proof)
            .unwrap_err()
            .contains("already used"));

        let other_key = browser_key();
        let wrong_nonce = "poll-wrong-key";
        let wrong_payload = format!(
            "{POLL_PROOF_PROTOCOL}\n{}\n{}\n{wrong_nonce}\n{timestamp}",
            request.request_id,
            request.document_sha256(),
        );
        let wrong = HostedLeasePollProof {
            request_id: request.request_id,
            nonce: wrong_nonce.to_string(),
            timestamp_unix_ms: timestamp,
            signature: sign(&other_key, &wrong_payload),
        };
        assert!(runtime
            .poll_request(&wrong)
            .unwrap_err()
            .contains("signature"));
    }

    #[test]
    fn daemon_signatures_bind_every_doorbell_and_lease_document_field() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let (request, document) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);

        let mut request_mutations = Vec::new();
        macro_rules! mutate_request {
            ($name:literal, $body:expr) => {{
                let mut altered = request.clone();
                $body(&mut altered);
                request_mutations.push(($name, altered));
            }};
        }
        mutate_request!("protocol", |value: &mut HostedLeaseRequest| value
            .protocol
            .push('x'));
        mutate_request!("request id", |value: &mut HostedLeaseRequest| value
            .request_id
            .push('x'));
        mutate_request!("request nonce", |value: &mut HostedLeaseRequest| value
            .request_nonce
            .push('x'));
        mutate_request!("browser key", |value: &mut HostedLeaseRequest| value
            .browser_public_key
            .push('x'));
        mutate_request!("browser fingerprint", |value: &mut HostedLeaseRequest| {
            value.browser_key_fingerprint.push('x')
        });
        mutate_request!("preset", |value: &mut HostedLeaseRequest| value
            .requested_preset =
            HostedPreset::View);
        mutate_request!("ttl", |value: &mut HostedLeaseRequest| value
            .requested_ttl_secs +=
            1);
        mutate_request!("requester label", |value: &mut HostedLeaseRequest| value
            .requester_label
            .push('x'));
        mutate_request!("fleet origin", |value: &mut HostedLeaseRequest| value
            .fleet_origin =
            "https://other.example.test".to_string());
        mutate_request!("daemon id", |value: &mut HostedLeaseRequest| value
            .daemon_id
            .push('x'));
        mutate_request!("daemon label", |value: &mut HostedLeaseRequest| value
            .daemon_label
            .push('x'));
        mutate_request!("daemon key", |value: &mut HostedLeaseRequest| value
            .daemon_public_key
            .push('x'));
        mutate_request!("created time", |value: &mut HostedLeaseRequest| value
            .created_unix_ms +=
            1);
        mutate_request!("expiry", |value: &mut HostedLeaseRequest| value
            .expires_unix_ms +=
            1);
        for (field, altered) in request_mutations {
            assert!(
                runtime.verify_doorbell(&altered).is_err(),
                "doorbell signature did not bind {field}",
            );
        }

        let mut document_mutations = Vec::new();
        macro_rules! mutate_document {
            ($name:literal, $body:expr) => {{
                let mut altered = document.clone();
                $body(&mut altered);
                document_mutations.push(($name, altered));
            }};
        }
        mutate_document!("protocol", |value: &mut HostedLeaseDocument| value
            .protocol
            .push('x'));
        mutate_document!("lease id", |value: &mut HostedLeaseDocument| value
            .lease_id
            .push('x'));
        mutate_document!("request id", |value: &mut HostedLeaseDocument| value
            .request_id
            .push('x'));
        mutate_document!("daemon id", |value: &mut HostedLeaseDocument| value
            .daemon_id
            .push('x'));
        mutate_document!("daemon key", |value: &mut HostedLeaseDocument| value
            .daemon_public_key
            .push('x'));
        mutate_document!("fleet origin", |value: &mut HostedLeaseDocument| value
            .fleet_origin =
            "https://other.example.test".to_string());
        mutate_document!("browser key", |value: &mut HostedLeaseDocument| value
            .browser_public_key
            .push('x'));
        mutate_document!("browser fingerprint", |value: &mut HostedLeaseDocument| {
            value.browser_key_fingerprint.push('x')
        });
        mutate_document!("preset", |value: &mut HostedLeaseDocument| value.preset =
            HostedPreset::View);
        mutate_document!("issued time", |value: &mut HostedLeaseDocument| value
            .issued_unix_ms +=
            1);
        mutate_document!("expiry", |value: &mut HostedLeaseDocument| value
            .expires_unix_ms +=
            1);
        mutate_document!("principal", |value: &mut HostedLeaseDocument| value
            .principal_id
            .push('x'));
        mutate_document!("grant", |value: &mut HostedLeaseDocument| value
            .grant_id
            .push('x'));
        mutate_document!("document hash", |value: &mut HostedLeaseDocument| value
            .document_sha256
            .push('x'));
        for (field, altered) in document_mutations {
            assert!(
                !verify_b64u(
                    &document.daemon_public_key,
                    altered.signing_payload().as_bytes(),
                    &document.signature,
                ),
                "lease signature did not bind {field}",
            );
        }
    }

    #[test]
    fn request_proofs_bind_request_target_audience_key_and_freshness() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let (_, document) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);
        let now = now_ms();
        let proof = request_proof(
            &key,
            &document,
            "GET",
            "/api/sessions?limit=20",
            "proof-valid",
            now,
        );
        assert!(runtime
            .verify_request_proof(
                "GET",
                "/api/sessions?limit=20",
                "https://laptop.example.test",
                &proof,
                "relay",
            )
            .is_ok());

        for (label, method, path, origin, proof) in [
            (
                "method",
                "POST",
                "/api/sessions?limit=20",
                "https://laptop.example.test",
                request_proof(
                    &key,
                    &document,
                    "GET",
                    "/api/sessions?limit=20",
                    "proof-method",
                    now,
                ),
            ),
            (
                "raw target",
                "GET",
                "/api/sessions?limit=21",
                "https://laptop.example.test",
                request_proof(
                    &key,
                    &document,
                    "GET",
                    "/api/sessions?limit=20",
                    "proof-path",
                    now,
                ),
            ),
            (
                "origin",
                "GET",
                "/api/sessions?limit=20",
                "https://other.example.test",
                request_proof(
                    &key,
                    &document,
                    "GET",
                    "/api/sessions?limit=20",
                    "proof-origin",
                    now,
                ),
            ),
        ] {
            assert!(
                runtime
                    .verify_request_proof(method, path, origin, &proof, "relay")
                    .is_err(),
                "request proof did not bind {label}",
            );
        }

        let wrong_key = browser_key();
        let wrong_key_proof = request_proof(
            &wrong_key,
            &document,
            "GET",
            "/api/sessions",
            "proof-wrong-key",
            now,
        );
        assert!(runtime
            .verify_request_proof(
                "GET",
                "/api/sessions",
                "https://laptop.example.test",
                &wrong_key_proof,
                "relay",
            )
            .is_err());
        let stale = now.saturating_sub(REQUEST_PROOF_MAX_SKEW_MS + 1);
        let stale_proof = request_proof(
            &key,
            &document,
            "GET",
            "/api/sessions",
            "proof-stale",
            stale,
        );
        assert!(runtime
            .verify_request_proof(
                "GET",
                "/api/sessions",
                "https://laptop.example.test",
                &stale_proof,
                "relay",
            )
            .unwrap_err()
            .contains("outside"));
    }

    #[test]
    fn decisions_can_only_reduce_and_integrated_operate_needs_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        assert!(runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: request.request_id.clone(),
                    approve: true,
                    approved_preset: Some(HostedPreset::Operate),
                    approved_ttl_secs: None,
                },
                &owner(),
            )
            .unwrap_err()
            .contains("may not exceed"));
        assert!(runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: request.request_id.clone(),
                    approve: true,
                    approved_preset: None,
                    approved_ttl_secs: Some(3601),
                },
                &owner(),
            )
            .unwrap_err()
            .contains("may not exceed"));
        let reduced = runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: request.request_id,
                    approve: true,
                    approved_preset: Some(HostedPreset::View),
                    approved_ttl_secs: Some(600),
                },
                &owner(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(reduced.preset, HostedPreset::View);
        assert!(reduced.expires_unix_ms - reduced.issued_unix_ms <= 600_000);

        iam::transact_state(&runtime.cert_dir, |state, _| {
            iam::set_daemon_tier(state, Some("integrated"), &owner())?;
            Ok(((), true))
        })
        .unwrap();
        assert!(runtime
            .set_policy(HostedPreset::Operate, 3600, &owner(), false)
            .unwrap_err()
            .to_string()
            .contains("hardening acknowledgement"));
        assert_eq!(
            runtime
                .set_policy(HostedPreset::Operate, 3600, &owner(), true)
                .unwrap()
                .ceiling,
            HostedPreset::Operate
        );
    }

    #[test]
    fn revocation_expiry_and_exact_iam_mutation_end_authority() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let (_, revoked) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);
        let opening = runtime
            .load_verified_lease(&revoked.lease_id, "https://laptop.example.test", "relay")
            .unwrap();
        assert!(runtime.revalidate_verified_lease(&opening).is_ok());
        assert!(runtime.revoke_lease(&revoked.lease_id, &owner()).unwrap());
        assert!(runtime.revalidate_verified_lease(&opening).is_err());
        assert!(runtime
            .load_verified_lease(&revoked.lease_id, "https://laptop.example.test", "relay")
            .is_err());

        let key = browser_key();
        let (_, expired) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);
        iam::transact_state(&runtime.cert_dir, |state, _| {
            let now = now_ms() as u64;
            let lease = state
                .hosted_control
                .leases
                .iter_mut()
                .find(|lease| lease.document.lease_id == expired.lease_id)
                .unwrap();
            lease.document.expires_unix_ms = now.saturating_sub(1);
            lease.document.document_sha256 = lease.document.expected_document_sha256();
            lease.document.signature = runtime
                .identity()
                .unwrap()
                .sign_b64u(lease.document.signing_payload().as_bytes());
            let grant = state
                .grants
                .iter_mut()
                .find(|grant| grant.id == lease.document.grant_id)
                .unwrap();
            grant.expires_at_unix_ms = Some(lease.document.expires_unix_ms);
            Ok(((), true))
        })
        .unwrap();
        runtime.materialize_expirations("principal:test").unwrap();
        assert!(runtime
            .load_verified_lease(&expired.lease_id, "https://laptop.example.test", "relay")
            .is_err());

        let key = browser_key();
        let (_, altered) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);
        iam::transact_state(&runtime.cert_dir, |state, _| {
            state
                .grants
                .iter_mut()
                .find(|grant| grant.id == altered.grant_id)
                .unwrap()
                .role_id = HOSTED_ROLE_VIEW.to_string();
            Ok(((), true))
        })
        .unwrap();
        assert!(runtime
            .load_verified_lease(&altered.lease_id, "https://laptop.example.test", "relay")
            .is_err());
    }

    #[test]
    fn raising_ceiling_does_not_upgrade_an_existing_lease() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        runtime
            .set_policy(HostedPreset::View, 3600, &owner(), true)
            .unwrap();
        let key = browser_key();
        let (_, document) = issue_lease(&runtime, &key, HostedPreset::View, 3600);
        runtime
            .set_policy(HostedPreset::Operate, 3600, &owner(), true)
            .unwrap();
        let verified = runtime
            .load_verified_lease(&document.lease_id, "https://laptop.example.test", "relay")
            .unwrap();
        assert_eq!(verified.document.preset, HostedPreset::View);
        assert_eq!(
            super::super::hosted_preset_for_principal(&verified.iam_state, &verified.principal)
                .unwrap(),
            HostedPreset::View
        );
    }

    #[test]
    fn hosted_session_eligibility_is_stamped_only_from_a_live_lease() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let (_, document) = issue_lease(&runtime, &key, HostedPreset::Tasks, 3600);
        mark_session_created_by_hosted_lease(
            &runtime.cert_dir,
            &document.lease_id,
            "session-created",
        )
        .unwrap();
        let state = iam::load_state_cached_arc(&runtime.cert_dir).unwrap();
        assert!(state
            .hosted_control
            .policy
            .eligible_session_ids
            .contains(&"session-created".to_string()));
        runtime.revoke_lease(&document.lease_id, &owner()).unwrap();
        assert!(mark_session_created_by_hosted_lease(
            &runtime.cert_dir,
            &document.lease_id,
            "session-after-revoke",
        )
        .is_err());
    }

    #[test]
    fn qualifying_set_holds_exactly_the_macos_pgp_logged_lane() {
        assert_eq!(ELIGIBLE_SIGNED_APP_DISTRIBUTIONS, &["macos-pgp-logged-v1"]);
    }

    /// Hermetic enrollment-evidence rig: the hosted_verify fixture log with
    /// one qualifying release, plus injected endpoints so the enrollment
    /// verifier's fail-closed online re-verification runs against loopback.
    struct EvidenceRig {
        endpoints: ReleaseEvidenceEndpoints,
        tag: String,
        artifact_sha256: String,
        manifest_hash: String,
        log_index: u64,
        _server: tokio::task::JoinHandle<()>,
        _state_root: tempfile::TempDir,
    }

    async fn evidence_rig(tag: &str) -> EvidenceRig {
        use crate::hosted_verify::test_fixtures::{
            release_leaf_fixture, signed_release_artifacts, spawn_fixture_server, Fixture,
            FixtureLog,
        };
        let bytes = b"qualifying app bytes".to_vec();
        let artifact = crate::hosted_verify::ReleaseArtifact {
            name: format!("Intendant-{}-macos-arm64.zip", tag.trim_start_matches('v')),
            sha256: crate::hosted_verify::sha256_hex(&bytes),
            size: bytes.len() as u64,
        };
        let artifacts = signed_release_artifacts(&artifact, b"detached signature bytes");
        let leaf = release_leaf_fixture(tag, &artifacts);
        let manifest_hash = serde_json::from_str::<serde_json::Value>(&leaf).unwrap()
            ["manifest_hash"]
            .as_str()
            .unwrap()
            .to_string();
        let leaves = vec![
            serde_json::json!({ "kind": "daemon_claimed", "daemon_id": "d1" }).to_string(),
            leaf,
        ];
        let fixture = std::sync::Arc::new(std::sync::Mutex::new(Fixture {
            log: FixtureLog::new(leaves),
            manifest_index: 1,
            release_status: 200,
            release_body: serde_json::Value::Null,
            downloads: std::collections::HashMap::new(),
        }));
        let (base, server) = spawn_fixture_server(std::sync::Arc::clone(&fixture)).await;
        fixture.lock().unwrap().release_body = serde_json::json!({
            "tag_name": tag,
            "assets": artifacts
                .iter()
                .map(|artifact| serde_json::json!({
                    "name": artifact.name,
                    "size": artifact.size,
                    "digest": format!("sha256:{}", artifact.sha256),
                    "browser_download_url": format!("{base}dl/{}", artifact.name),
                }))
                .collect::<Vec<_>>(),
        });
        let state_root = tempfile::tempdir().unwrap();
        EvidenceRig {
            endpoints: ReleaseEvidenceEndpoints {
                log_base: base.clone(),
                github_api: base,
                repo: "test/repo".to_string(),
                state_root: state_root.path().to_path_buf(),
            },
            tag: tag.to_string(),
            artifact_sha256: artifact.sha256,
            manifest_hash,
            log_index: 1,
            _server: server,
            _state_root: state_root,
        }
    }

    fn rig_receipt(rig: &EvidenceRig) -> SignedAppInstallReceipt {
        SignedAppInstallReceipt {
            tag: rig.tag.clone(),
            log_index: rig.log_index,
            manifest_hash: rig.manifest_hash.clone(),
            artifact_sha256: rig.artifact_sha256.clone(),
            resign_cert_fingerprint: "intendant-dev-test-dr".to_string(),
            post_resign_cdhash: "cdhash-test".to_string(),
            written_unix_ms: now_ms().max(0) as u64,
        }
    }

    fn enrollment_input(
        key: &BrowserKey,
        device_id: &str,
        distribution_id: &str,
        receipt: SignedAppInstallReceipt,
    ) -> SignedAppAnchorEnrollmentInput {
        let mut input = SignedAppAnchorEnrollmentInput {
            device_id: device_id.to_string(),
            label: "Test signed app".to_string(),
            public_key: key.public_key.clone(),
            distribution_id: distribution_id.to_string(),
            receipt,
            nonce: format!("nonce-{}", uuid::Uuid::new_v4().simple()),
            timestamp_unix_ms: now_ms(),
            signature: String::new(),
        };
        input.signature = sign(key, &input.challenge_payload("daemon-test"));
        input
    }

    #[tokio::test]
    async fn enrollment_verifies_evidence_online_and_persists_the_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let rig = evidence_rig("v1.2.3").await;
        let key = browser_key();
        let receipt = rig_receipt(&rig);
        let receipt_sha256 = receipt.document_sha256();
        let anchor = runtime
            .enroll_signed_app_anchor_at(
                &rig.endpoints,
                enrollment_input(&key, "device-1", "macos-pgp-logged-v1", receipt),
                &owner(),
            )
            .await
            .expect("qualifying enrollment passes");
        assert!(anchor.active);
        assert_eq!(anchor.distribution_id, "macos-pgp-logged-v1");
        let evidence = anchor.evidence.expect("evidence snapshot persisted");
        assert_eq!(evidence.verified_tag, "v1.2.3");
        assert_eq!(evidence.log_index, 1);
        assert_eq!(evidence.receipt_sha256, receipt_sha256);
        assert_eq!(evidence.artifact_sha256, rig.artifact_sha256);
        assert_eq!(
            evidence.pgp_fingerprint_at_enrollment,
            crate::pgp_identity::RELEASE_SIGNING_KEY_FINGERPRINT
        );
        let state = iam::load_state(&runtime.cert_dir).unwrap();
        assert_eq!(state.hosted_control.signed_app_anchors.len(), 1);
        assert!(state.hosted_control.signed_app_anchors[0].evidence.is_some());
        assert!(state
            .audit_events
            .iter()
            .any(|event| event.action == "hosted_anchor_enroll"));

        // Re-enrollment under the same device id replaces the record —
        // the owner's key-rotation remedy; the old key stops witnessing.
        let rotated = browser_key();
        let replacement = runtime
            .enroll_signed_app_anchor_at(
                &rig.endpoints,
                enrollment_input(&rotated, "device-1", "macos-pgp-logged-v1", rig_receipt(&rig)),
                &owner(),
            )
            .await
            .unwrap();
        let state = iam::load_state(&runtime.cert_dir).unwrap();
        assert_eq!(state.hosted_control.signed_app_anchors.len(), 1);
        assert_eq!(
            state.hosted_control.signed_app_anchors[0].public_key,
            replacement.public_key
        );
        assert_ne!(replacement.public_key, anchor.public_key);
    }

    #[tokio::test]
    async fn enrollment_refuses_unlisted_and_development_distribution_ids() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        // Membership refuses before any network reach: these endpoints are
        // never dialed.
        let endpoints = ReleaseEvidenceEndpoints {
            log_base: url::Url::parse("http://127.0.0.1:1/").unwrap(),
            github_api: url::Url::parse("http://127.0.0.1:1/").unwrap(),
            repo: "test/repo".to_string(),
            state_root: temp.path().to_path_buf(),
        };
        let key = browser_key();
        for ineligible in ["macos-unsigned-dev", "some-other-lane", ""] {
            let receipt = SignedAppInstallReceipt {
                tag: "v1.2.3".to_string(),
                log_index: 1,
                manifest_hash: "0".repeat(64),
                artifact_sha256: "0".repeat(64),
                resign_cert_fingerprint: String::new(),
                post_resign_cdhash: String::new(),
                written_unix_ms: 1,
            };
            let error = runtime
                .enroll_signed_app_anchor_at(
                    &endpoints,
                    enrollment_input(&key, "device-1", ineligible, receipt),
                    &owner(),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("not in this build's qualifying set"),
                "{ineligible:?} was not refused by membership: {error}"
            );
        }
        assert!(iam::load_state(&runtime.cert_dir)
            .unwrap()
            .hosted_control
            .signed_app_anchors
            .is_empty());
    }

    #[tokio::test]
    async fn enrollment_refuses_forged_receipts_daemon_side() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();

        // An unlogged tag: the log has no such release committed.
        let rig = evidence_rig("v1.2.3").await;
        let mut receipt = rig_receipt(&rig);
        receipt.tag = "v9.9.9".to_string();
        let error = runtime
            .enroll_signed_app_anchor_at(
                &rig.endpoints,
                enrollment_input(&key, "device-1", "macos-pgp-logged-v1", receipt),
                &owner(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("enrollment evidence verification failed"),
            "unlogged tag must fail the online re-verification: {error}"
        );

        // An artifact digest absent from the release's logged manifest.
        let rig = evidence_rig("v1.2.3").await;
        let mut receipt = rig_receipt(&rig);
        receipt.artifact_sha256 = crate::hosted_verify::sha256_hex(b"a swapped artifact");
        let error = runtime
            .enroll_signed_app_anchor_at(
                &rig.endpoints,
                enrollment_input(&key, "device-1", "macos-pgp-logged-v1", receipt),
                &owner(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not in the verified release's logged manifest"),
            "mismatched artifact digest must be refused: {error}"
        );

        // A receipt naming a different log position or manifest hash than
        // the verified release.
        let rig = evidence_rig("v1.2.3").await;
        let mut receipt = rig_receipt(&rig);
        receipt.log_index = 7;
        let error = runtime
            .enroll_signed_app_anchor_at(
                &rig.endpoints,
                enrollment_input(&key, "device-1", "macos-pgp-logged-v1", receipt),
                &owner(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("transparency-log index"), "{error}");
        let rig = evidence_rig("v1.2.3").await;
        let mut receipt = rig_receipt(&rig);
        receipt.manifest_hash = crate::hosted_verify::sha256_hex(b"other manifest");
        let error = runtime
            .enroll_signed_app_anchor_at(
                &rig.endpoints,
                enrollment_input(&key, "device-1", "macos-pgp-logged-v1", receipt),
                &owner(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("manifest hash"), "{error}");
        assert!(iam::load_state(&runtime.cert_dir)
            .unwrap()
            .hosted_control
            .signed_app_anchors
            .is_empty());
    }

    #[tokio::test]
    async fn enrollment_challenge_is_fresh_keyed_and_replay_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let rig = evidence_rig("v1.2.3").await;
        let key = browser_key();

        // A challenge signed by a different key than the presented one.
        let mut input =
            enrollment_input(&key, "device-1", "macos-pgp-logged-v1", rig_receipt(&rig));
        input.signature = sign(&browser_key(), &input.challenge_payload("daemon-test"));
        assert!(runtime
            .enroll_signed_app_anchor_at(&rig.endpoints, input, &owner())
            .await
            .unwrap_err()
            .to_string()
            .contains("signature verification failed"));

        // A stale challenge timestamp.
        let mut input =
            enrollment_input(&key, "device-1", "macos-pgp-logged-v1", rig_receipt(&rig));
        input.timestamp_unix_ms = now_ms() - REQUEST_PROOF_MAX_SKEW_MS - 1_000;
        input.signature = sign(&key, &input.challenge_payload("daemon-test"));
        assert!(runtime
            .enroll_signed_app_anchor_at(&rig.endpoints, input, &owner())
            .await
            .unwrap_err()
            .to_string()
            .contains("timestamp"));

        // A replayed nonce after a successful enrollment.
        let input = enrollment_input(&key, "device-1", "macos-pgp-logged-v1", rig_receipt(&rig));
        let replay = input.clone();
        runtime
            .enroll_signed_app_anchor_at(&rig.endpoints, input, &owner())
            .await
            .unwrap();
        assert!(runtime
            .enroll_signed_app_anchor_at(&rig.endpoints, replay, &owner())
            .await
            .unwrap_err()
            .to_string()
            .contains("already used"));
    }

    /// Unit-level anchor injection for the decision-lane tests; enrollment
    /// evidence has its own coverage above.
    fn inject_anchor(
        runtime: &HostedControlRuntime,
        key: &BrowserKey,
        device_id: &str,
        distribution_id: &str,
    ) {
        iam::transact_state(&runtime.cert_dir, |state, _| {
            state
                .hosted_control
                .signed_app_anchors
                .retain(|anchor| anchor.device_id != device_id);
            state.hosted_control.signed_app_anchors.push(SignedAppAnchor {
                device_id: device_id.to_string(),
                label: "Injected test anchor".to_string(),
                public_key: key.public_key.clone(),
                key_fingerprint: "fp-test".to_string(),
                distribution_id: distribution_id.to_string(),
                active: true,
                enrolled_unix_ms: now_ms().max(0) as u64,
                revoked_unix_ms: None,
                evidence: None,
            });
            Ok(((), true))
        })
        .unwrap();
    }

    fn decision_document(
        key: &BrowserKey,
        device_id: &str,
        request: &HostedLeaseRequest,
        approve: bool,
    ) -> HostedAnchorDecisionDocument {
        let mut document = HostedAnchorDecisionDocument {
            protocol: ANCHOR_DECISION_PROTOCOL.to_string(),
            device_id: device_id.to_string(),
            anchor_public_key: key.public_key.clone(),
            daemon_id: "daemon-test".to_string(),
            request_id: request.request_id.clone(),
            request_document_sha256: request.document_sha256(),
            approve,
            nonce: format!("nonce-{}", uuid::Uuid::new_v4().simple()),
            timestamp_unix_ms: now_ms(),
            signature: String::new(),
        };
        document.signature = sign(key, &document.signing_payload());
        document
    }

    #[test]
    fn anchor_decision_approves_and_denies_bound_pending_requests() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let anchor_key = browser_key();
        inject_anchor(&runtime, &anchor_key, "device-1", "macos-pgp-logged-v1");

        let browser = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&browser, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let status = runtime
            .apply_anchor_decision(decision_document(&anchor_key, "device-1", &request, true))
            .unwrap();
        assert_eq!(status, HostedLeaseRequestStatus::Approved);
        let state = iam::load_state(&runtime.cert_dir).unwrap();
        let stored = state
            .hosted_control
            .requests
            .iter()
            .find(|stored| stored.request_id == request.request_id)
            .unwrap();
        assert_eq!(stored.status, HostedLeaseRequestStatus::Approved);
        let lease_id = stored.approved_lease_id.clone().unwrap();
        assert!(runtime
            .load_verified_lease(&lease_id, "https://laptop.example.test", "relay")
            .is_ok());
        assert!(state.audit_events.iter().any(|event| {
            event.action == "hosted_lease_issue"
                && event.actor_principal_id == "principal:signed-app-anchor:device-1"
        }));

        let second = runtime
            .create_request(
                doorbell_input(&browser_key(), HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let status = runtime
            .apply_anchor_decision(decision_document(&anchor_key, "device-1", &second, false))
            .unwrap();
        assert_eq!(status, HostedLeaseRequestStatus::Denied);
    }

    #[test]
    fn anchor_decision_negative_family() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let anchor_key = browser_key();
        inject_anchor(&runtime, &anchor_key, "device-1", "macos-pgp-logged-v1");
        let browser = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&browser, HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();

        // Wrong-key decision: signed by a key other than the enrolled one.
        let mut document = decision_document(&anchor_key, "device-1", &request, true);
        document.signature = sign(&browser_key(), &document.signing_payload());
        assert!(runtime
            .apply_anchor_decision(document)
            .unwrap_err()
            .contains("signature verification failed"));

        // A decision presenting a key that is not the enrolled record's.
        let foreign = browser_key();
        let document = decision_document(&foreign, "device-1", &request, true);
        assert_eq!(
            runtime.apply_anchor_decision(document).unwrap_err(),
            ANCHOR_DECISION_REFUSED_ERROR
        );

        // Stale decision: outside the freshness window.
        let mut document = decision_document(&anchor_key, "device-1", &request, true);
        document.timestamp_unix_ms = now_ms() - REQUEST_PROOF_MAX_SKEW_MS - 1_000;
        document.signature = sign(&anchor_key, &document.signing_payload());
        assert!(runtime
            .apply_anchor_decision(document)
            .unwrap_err()
            .contains("timestamp"));

        // Unknown device.
        let document = decision_document(&anchor_key, "device-unknown", &request, true);
        assert_eq!(
            runtime.apply_anchor_decision(document).unwrap_err(),
            ANCHOR_DECISION_REFUSED_ERROR
        );

        // A different target daemon.
        let mut document = decision_document(&anchor_key, "device-1", &request, true);
        document.daemon_id = "daemon-other".to_string();
        document.signature = sign(&anchor_key, &document.signing_payload());
        assert!(runtime
            .apply_anchor_decision(document)
            .unwrap_err()
            .contains("different target daemon"));

        // Changed digest: the decision names a different request document
        // than the one this daemon holds under that id.
        let mut document = decision_document(&anchor_key, "device-1", &request, true);
        let mut other = request.clone();
        other.requested_ttl_secs = 900;
        document.request_document_sha256 = other.document_sha256();
        document.signature = sign(&anchor_key, &document.signing_payload());
        assert_eq!(
            runtime.apply_anchor_decision(document).unwrap_err(),
            ANCHOR_DECISION_REQUEST_CHANGED_ERROR
        );

        // Revoked anchor.
        let revoked_key = browser_key();
        inject_anchor(&runtime, &revoked_key, "device-revoked", "macos-pgp-logged-v1");
        iam::transact_state(&runtime.cert_dir, |state, _| {
            let anchor = state
                .hosted_control
                .signed_app_anchors
                .iter_mut()
                .find(|anchor| anchor.device_id == "device-revoked")
                .unwrap();
            anchor.active = false;
            anchor.revoked_unix_ms = Some(now_ms().max(0) as u64);
            Ok(((), true))
        })
        .unwrap();
        let document = decision_document(&revoked_key, "device-revoked", &request, true);
        assert_eq!(
            runtime.apply_anchor_decision(document).unwrap_err(),
            ANCHOR_DECISION_REFUSED_ERROR
        );

        // Set-nonmember anchor: enrolled record whose distribution id is
        // outside the compiled qualifying set.
        let nonmember_key = browser_key();
        inject_anchor(&runtime, &nonmember_key, "device-dev", "macos-unsigned-dev");
        let document = decision_document(&nonmember_key, "device-dev", &request, true);
        assert_eq!(
            runtime.apply_anchor_decision(document).unwrap_err(),
            ANCHOR_DECISION_REFUSED_ERROR
        );

        // Replay: a fresh nonce decides, the identical document replayed is
        // refused, and the request stays decided exactly once.
        let replay_key = browser_key();
        inject_anchor(&runtime, &replay_key, "device-replay", "macos-pgp-logged-v1");
        let document = decision_document(&replay_key, "device-replay", &request, false);
        let replayed = document.clone();
        runtime.apply_anchor_decision(document).unwrap();
        assert!(runtime
            .apply_anchor_decision(replayed)
            .unwrap_err()
            .contains("already used"));

        // The request was denied above and is no longer pending.
        let document = decision_document(&replay_key, "device-replay", &request, true);
        assert!(runtime
            .apply_anchor_decision(document)
            .unwrap_err()
            .contains("no longer pending"));
    }

    #[test]
    fn anchor_decision_scope_is_lease_decisions_only() {
        // The document shape is closed: anything beyond the decision fields
        // is refused at parse time, so the lane cannot grow management
        // operations by riding extra fields.
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let anchor_key = browser_key();
        inject_anchor(&runtime, &anchor_key, "device-1", "macos-pgp-logged-v1");
        let request = runtime
            .create_request(
                doorbell_input(&browser_key(), HostedPreset::Tasks, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let document = decision_document(&anchor_key, "device-1", &request, true);
        let mut widened = serde_json::to_value(&document).unwrap();
        widened
            .as_object_mut()
            .unwrap()
            .insert("op".to_string(), serde_json::json!("set_policy"));
        assert!(
            serde_json::from_value::<HostedAnchorDecisionDocument>(widened).is_err(),
            "unknown fields must refuse at parse time"
        );

        // A valid decision changes lease-request state only: policy,
        // anchors, witnesses, and session eligibility stay byte-identical.
        let before = iam::load_state(&runtime.cert_dir).unwrap();
        runtime.apply_anchor_decision(document).unwrap();
        let after = iam::load_state(&runtime.cert_dir).unwrap();
        assert_eq!(before.hosted_control.policy, after.hosted_control.policy);
        assert_eq!(
            before.hosted_control.signed_app_anchors,
            after.hosted_control.signed_app_anchors
        );
        assert_eq!(
            before.hosted_control.witnesses,
            after.hosted_control.witnesses
        );
        let decided = after
            .hosted_control
            .requests
            .iter()
            .find(|stored| stored.request_id == request.request_id)
            .unwrap();
        assert_eq!(decided.status, HostedLeaseRequestStatus::Approved);
    }

    #[test]
    fn public_bootstrap_never_carries_the_qualifying_set() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let bootstrap = runtime.bootstrap("https://laptop.example.test").unwrap();
        let value = serde_json::to_value(&bootstrap).unwrap();
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("qualifying_signed_app_distribution_available"));
        assert!(!object.contains_key("eligible_signed_app_distributions"));

        // The trusted management surface does say what the set holds.
        let snapshot = runtime.management_snapshot().unwrap();
        assert!(snapshot.qualifying_signed_app_distribution_available);
        assert_eq!(
            snapshot.eligible_signed_app_distributions,
            vec!["macos-pgp-logged-v1".to_string()]
        );
    }

    #[test]
    fn websocket_ticket_is_one_use() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let key = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::View, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let document = runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: request.request_id,
                    approve: true,
                    approved_preset: None,
                    approved_ttl_secs: None,
                },
                &owner(),
            )
            .unwrap()
            .unwrap();
        let verified = runtime
            .load_verified_lease(&document.lease_id, "https://laptop.example.test", "relay")
            .unwrap();
        let ticket = runtime.mint_ws_ticket(&verified).unwrap();
        assert!(runtime
            .consume_ws_ticket(&ticket.ticket, "https://laptop.example.test", "relay")
            .is_ok());
        assert!(runtime
            .consume_ws_ticket(&ticket.ticket, "https://laptop.example.test", "relay")
            .is_err());

        let wrong_origin = runtime.mint_ws_ticket(&verified).unwrap();
        assert!(runtime
            .consume_ws_ticket(&wrong_origin.ticket, "https://other.example.test", "relay")
            .unwrap_err()
            .contains("origin"));
        assert!(
            runtime
                .consume_ws_ticket(&wrong_origin.ticket, "https://laptop.example.test", "relay")
                .is_err(),
            "an audience-mismatched attempt must consume the one-use ticket"
        );

        let expired = runtime.mint_ws_ticket(&verified).unwrap();
        replace_shared_transient(&runtime, |state| {
            state
                .tickets
                .get_mut(&expired.ticket)
                .unwrap()
                .expires_unix_ms = now_ms().saturating_sub(1) as u64;
        });
        assert!(runtime
            .consume_ws_ticket(&expired.ticket, "https://laptop.example.test", "relay")
            .unwrap_err()
            .contains("expired"));

        let revoked = runtime.mint_ws_ticket(&verified).unwrap();
        runtime.revoke_lease(&document.lease_id, &owner()).unwrap();
        assert!(
            runtime
                .consume_ws_ticket(&revoked.ticket, "https://laptop.example.test", "relay")
                .is_err(),
            "ticket consumption must recheck the live lease and grant"
        );
    }

    #[test]
    fn websocket_ticket_can_be_consumed_once_by_a_sibling_process() {
        let temp = tempfile::tempdir().unwrap();
        let first = runtime(&temp);
        let second = runtime(&temp);
        let key = browser_key();
        let (_, document) = issue_lease(&first, &key, HostedPreset::View, 3600);
        let verified = first
            .load_verified_lease(&document.lease_id, "https://laptop.example.test", "relay")
            .unwrap();
        let ticket = first.mint_ws_ticket(&verified).unwrap();
        second
            .consume_ws_ticket(&ticket.ticket, "https://laptop.example.test", "relay")
            .unwrap();
        assert!(first
            .consume_ws_ticket(&ticket.ticket, "https://laptop.example.test", "relay")
            .unwrap_err()
            .contains("already used"));
    }

    #[test]
    fn lowering_ceiling_revokes_higher_lease() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        runtime
            .set_policy(HostedPreset::Operate, 7200, &owner(), true)
            .unwrap();
        let key = browser_key();
        let request = runtime
            .create_request(
                doorbell_input(&key, HostedPreset::Operate, 3600),
                "https://laptop.example.test",
                None,
            )
            .unwrap();
        let document = runtime
            .decide_request(
                HostedLeaseDecisionInput {
                    request_id: request.request_id,
                    approve: true,
                    approved_preset: None,
                    approved_ttl_secs: None,
                },
                &owner(),
            )
            .unwrap()
            .unwrap();
        runtime
            .set_policy(HostedPreset::Tasks, 7200, &owner(), true)
            .unwrap();
        assert!(runtime
            .load_verified_lease(&document.lease_id, "https://laptop.example.test", "relay")
            .is_err());
        let state = iam::load_state_cached_arc(&runtime.cert_dir).unwrap();
        let audit = state
            .audit_events
            .iter()
            .find(|event| {
                event.action == "hosted_lease_revoke" && event.target_id == document.lease_id
            })
            .expect("policy revocation must emit a per-lease audit record");
        assert!(audit.summary.contains("operate"));
        assert!(audit.summary.contains("3600 second lifetime"));
    }
}
