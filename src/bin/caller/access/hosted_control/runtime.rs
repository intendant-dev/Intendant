use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::json;

use crate::access::iam::{
    self, AccessPrincipal, IamAuditEvent, IamGrant, IamPrincipal, LocalIamState,
};
use crate::access::{AccessError, AccessResult};
use crate::daemon_identity::{b64u, verify_b64u, DaemonIdentity};

use super::*;

pub(super) const ELIGIBLE_SIGNED_APP_DISTRIBUTIONS: &[&str] = &[];

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
    /// Replay window for authority-free doorbell creation and polling.
    replay: Arc<Mutex<ReplayState>>,
    /// Independent replay window for active lease proofs. Public polling must
    /// never consume the capacity that protects authenticated control.
    lease_replay: Arc<Mutex<ReplayState>>,
    tickets: Arc<Mutex<HashMap<String, WsTicketRecord>>>,
    doorbell_rate: Arc<Mutex<DoorbellRateState>>,
    poll_rate: Arc<Mutex<PollRateState>>,
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

#[derive(Default)]
struct ReplayState {
    by_authority: HashMap<String, VecDeque<(String, i64)>>,
}

#[derive(Clone)]
struct WsTicketRecord {
    lease_id: String,
    grant_id: String,
    fleet_origin: String,
    expires_unix_ms: u64,
}

#[derive(Default)]
struct DoorbellRateState {
    global: VecDeque<i64>,
    by_key: HashMap<String, VecDeque<i64>>,
    by_source: HashMap<String, VecDeque<i64>>,
}

#[derive(Default)]
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
            replay: Arc::new(Mutex::new(ReplayState::default())),
            lease_replay: Arc::new(Mutex::new(ReplayState::default())),
            tickets: Arc::new(Mutex::new(HashMap::new())),
            doorbell_rate: Arc::new(Mutex::new(DoorbellRateState::default())),
            poll_rate: Arc::new(Mutex::new(PollRateState::default())),
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
                    &actor.id,
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
            let stable = stable_id_digest(&format!(
                "{}\n{}",
                request.request_id, request.browser_key_fingerprint
            ));
            let lease_id = format!("lease:{stable}");
            let principal_id = format!("principal:hosted-lease:{stable}");
            let grant_id = format!("grant:hosted-lease:{stable}");
            let expires = now.saturating_add(ttl.saturating_mul(1000));
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
                daemon_id: self.daemon_id.clone(),
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
            state.hosted_control.requests[request_index].status =
                HostedLeaseRequestStatus::Approved;
            state.hosted_control.requests[request_index].approved_lease_id = Some(lease_id.clone());
            push_audit(
                state,
                &actor.id,
                "hosted_lease_issue",
                &lease_id,
                format!("Issued {} lease for {} seconds", preset.as_str(), ttl),
            );
            Ok((Some(document), true))
        })
        .map_err(|error| format!("decide hosted lease request: {error}"))
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
        let mut tickets = self
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let now = now_ms() as u64;
        tickets.retain(|_, record| record.expires_unix_ms > now);
        if tickets.len() >= WS_TICKETS_GLOBAL_CAP {
            return Err("too many outstanding hosted WebSocket tickets".to_string());
        }
        tickets.insert(ticket.clone(), record);
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
        let record = self
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(ticket)
            .ok_or_else(|| {
                "hosted WebSocket ticket was not found or was already used".to_string()
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
        Ok(HostedControlManagementSnapshot {
            configured: self.configured(),
            enabled: self.enabled(),
            initialization_error: self.initialization_error().map(ToOwned::to_owned),
            display_media_relay_configured: self.display_media_relay_configured,
            anchor_decision_protocol: ANCHOR_DECISION_PROTOCOL.to_string(),
            qualifying_signed_app_distribution_available: !ELIGIBLE_SIGNED_APP_DISTRIBUTIONS
                .is_empty(),
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

    pub fn enroll_signed_app_anchor(
        &self,
        _anchor: SignedAppAnchor,
        _actor: &AccessPrincipal,
    ) -> AccessResult<()> {
        self.ensure_enabled().map_err(AccessError)?;
        let _ = ELIGIBLE_SIGNED_APP_DISTRIBUTIONS;
        Err(AccessError(
            "no qualifying signed application distribution is enabled in this build".to_string(),
        ))
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
            let mut expired_leases = Vec::new();
            for lease in &mut state.hosted_control.leases {
                if lease.status == HostedLeaseStatus::Active
                    && lease.document.expires_unix_ms <= now
                {
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
            for request_id in &expired_requests {
                push_audit(
                    state,
                    actor,
                    "hosted_lease_request_expire",
                    request_id,
                    "Observed expired hosted lease request".to_string(),
                );
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
            let changed = !expired_requests.is_empty() || !expired_leases.is_empty();
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
        record_nonce_in(&self.replay, authority, nonce, timestamp_unix_ms)
    }

    fn record_lease_nonce(
        &self,
        authority: &str,
        nonce: &str,
        timestamp_unix_ms: i64,
    ) -> Result<(), String> {
        record_nonce_in(&self.lease_replay, authority, nonce, timestamp_unix_ms)
    }

    fn check_doorbell_rate(
        &self,
        fingerprint: &str,
        source_bucket: Option<&str>,
        now: i64,
    ) -> Result<(), String> {
        let cutoff = now.saturating_sub(60_000);
        let mut rate = self
            .doorbell_rate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
        let key_entries = rate.by_key.entry(fingerprint.to_string()).or_default();
        if key_entries.len() >= DOORBELL_PER_KEY_PER_MINUTE {
            return Err("hosted lease request key rate limit reached".to_string());
        }
        if let Some(source) = source_bucket.filter(|source| !source.trim().is_empty()) {
            let source_entries = rate.by_source.entry(source.to_string()).or_default();
            if source_entries.len() >= 30 {
                return Err("hosted lease request source rate limit reached".to_string());
            }
            source_entries.push_back(now);
        }
        rate.global.push_back(now);
        rate.by_key
            .entry(fingerprint.to_string())
            .or_default()
            .push_back(now);
        Ok(())
    }

    fn check_poll_rate(&self, request_id: &str, now: i64) -> Result<(), String> {
        let cutoff = now.saturating_sub(60_000);
        let mut rate = self
            .poll_rate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        retain_recent(&mut rate.global, cutoff);
        rate.by_request.retain(|_, entries| {
            retain_recent(entries, cutoff);
            !entries.is_empty()
        });
        if rate.global.len() >= POLL_GLOBAL_PER_MINUTE {
            return Err("hosted lease poll global rate limit reached".to_string());
        }
        let request_entries = rate.by_request.entry(request_id.to_string()).or_default();
        if request_entries.len() >= POLL_PER_REQUEST_PER_MINUTE {
            return Err("hosted lease poll request rate limit reached".to_string());
        }
        request_entries.push_back(now);
        rate.global.push_back(now);
        Ok(())
    }
}

fn record_nonce_in(
    replay: &Arc<Mutex<ReplayState>>,
    authority: &str,
    nonce: &str,
    timestamp_unix_ms: i64,
) -> Result<(), String> {
    let cutoff = now_ms().saturating_sub(REQUEST_PROOF_MAX_SKEW_MS);
    let mut replay = replay.lock().unwrap_or_else(|error| error.into_inner());
    replay.by_authority.retain(|_, entries| {
        entries.retain(|(_, timestamp)| *timestamp >= cutoff);
        !entries.is_empty()
    });
    let total: usize = replay.by_authority.values().map(VecDeque::len).sum();
    if total >= PROOF_NONCES_GLOBAL_CAP {
        return Err("hosted proof replay window is full".to_string());
    }
    let entries = replay
        .by_authority
        .entry(authority.to_string())
        .or_default();
    if entries.iter().any(|(candidate, _)| candidate == nonce) {
        return Err("hosted proof nonce was already used".to_string());
    }
    if entries.len() >= PROOF_NONCES_PER_LEASE_CAP {
        return Err("hosted proof nonce window is full for this authority".to_string());
    }
    entries.push_back((nonce.to_string(), timestamp_unix_ms));
    Ok(())
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
        runtime
            .doorbell_rate
            .lock()
            .unwrap()
            .global
            .extend(std::iter::repeat_n(now, DOORBELL_GLOBAL_PER_MINUTE));
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
            runtime.replay.lock().unwrap().by_authority.is_empty(),
            "rate-limited doorbells must not consume the shared proof nonce window"
        );
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
            runtime.replay.lock().unwrap().by_authority.is_empty(),
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
        let mut public_replay = runtime.replay.lock().unwrap();
        public_replay.by_authority.clear();
        public_replay.by_authority.insert(
            "poll:public-capacity".to_string(),
            (0..PROOF_NONCES_GLOBAL_CAP)
                .map(|index| (format!("public-{index}"), now))
                .collect(),
        );
        drop(public_replay);

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
        assert_eq!(
            runtime
                .lease_replay
                .lock()
                .unwrap()
                .by_authority
                .values()
                .map(VecDeque::len)
                .sum::<usize>(),
            1
        );
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
        runtime
            .poll_rate
            .lock()
            .unwrap()
            .global
            .extend(std::iter::repeat_n(now, POLL_GLOBAL_PER_MINUTE));
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
            runtime
                .replay
                .lock()
                .unwrap()
                .by_authority
                .values()
                .map(VecDeque::len)
                .sum::<usize>(),
            1,
            "the rejected poll must not consume replay capacity"
        );
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
        assert!(runtime.revoke_lease(&revoked.lease_id, &owner()).unwrap());
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
    fn unsigned_development_artifacts_cannot_enroll_as_anchors() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let anchor = SignedAppAnchor {
            device_id: "device-test".to_string(),
            label: "Unsigned development app".to_string(),
            public_key: "not-used".to_string(),
            key_fingerprint: "not-used".to_string(),
            distribution_id: "macos-unsigned-dev".to_string(),
            active: true,
            enrolled_unix_ms: now_ms() as u64,
            revoked_unix_ms: None,
        };
        assert!(runtime
            .enroll_signed_app_anchor(anchor, &owner())
            .unwrap_err()
            .to_string()
            .contains("no qualifying signed application distribution"));
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
        runtime
            .tickets
            .lock()
            .unwrap()
            .get_mut(&expired.ticket)
            .unwrap()
            .expires_unix_ms = now_ms().saturating_sub(1) as u64;
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
