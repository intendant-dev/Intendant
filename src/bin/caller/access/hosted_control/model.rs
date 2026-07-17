use serde::{Deserialize, Serialize};

use crate::daemon_identity::b64u;

pub const DOORBELL_PROTOCOL: &str = "intendant-hosted-control-doorbell-v1";
pub const DOORBELL_REQUEST_PROOF_PROTOCOL: &str = "intendant-hosted-control-doorbell-request-v1";
pub const LEASE_PROTOCOL: &str = "intendant-hosted-control-lease-v1";
pub const REQUEST_PROOF_PROTOCOL: &str = "intendant-hosted-control-request-v1";
pub const POLL_PROOF_PROTOCOL: &str = "intendant-hosted-control-poll-v1";
pub const ANCHOR_DECISION_PROTOCOL: &str = "intendant-hosted-control-anchor-decision-v1";

pub const DEFAULT_LEASE_TTL_SECS: u64 = 4 * 60 * 60;
pub const HARD_MAX_LEASE_TTL_SECS: u64 = 24 * 60 * 60;
pub const MIN_LEASE_TTL_SECS: u64 = 60;
pub const PENDING_REQUEST_TTL_MS: u64 = 10 * 60 * 1000;
pub const HOSTED_REQUESTS_CAP: usize = 128;
pub const HOSTED_LEASES_CAP: usize = 256;
pub const HOSTED_ELIGIBLE_SESSIONS_CAP: usize = 2048;
pub const HOSTED_ANCHORS_CAP: usize = 32;
pub const REQUEST_PROOF_MAX_SKEW_MS: i64 = 60 * 1000;
pub const WS_TICKET_TTL_MS: u64 = 15 * 1000;
pub const PROOF_NONCES_GLOBAL_CAP: usize = 4096;
pub const PROOF_NONCES_PER_LEASE_CAP: usize = 128;
pub const WS_TICKETS_GLOBAL_CAP: usize = 512;
pub const DOORBELL_GLOBAL_PER_MINUTE: usize = 120;
pub const DOORBELL_PER_KEY_PER_MINUTE: usize = 6;

pub const HOSTED_PRINCIPAL_KIND: &str = "hosted_lease";
pub const HOSTED_AUTHN_KIND: &str = "hosted_lease_key";
pub const HOSTED_SOURCE: &str = "hosted_control";
pub const HOSTED_ROLE_VIEW: &str = "role:hosted-view";
pub const HOSTED_ROLE_TASKS: &str = "role:hosted-tasks";
pub const HOSTED_ROLE_OPERATE: &str = "role:hosted-operate";
pub const HOSTED_ROLE_IDS: [&str; 3] = [HOSTED_ROLE_VIEW, HOSTED_ROLE_TASKS, HOSTED_ROLE_OPERATE];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostedPreset {
    View,
    #[default]
    Tasks,
    Operate,
}

impl HostedPreset {
    #[cfg(test)]
    pub const ALL: [Self; 3] = [Self::View, Self::Tasks, Self::Operate];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Tasks => "tasks",
            Self::Operate => "operate",
        }
    }

    pub fn role_id(self) -> &'static str {
        match self {
            Self::View => HOSTED_ROLE_VIEW,
            Self::Tasks => HOSTED_ROLE_TASKS,
            Self::Operate => HOSTED_ROLE_OPERATE,
        }
    }

    pub fn from_role_id(role_id: &str) -> Option<Self> {
        match role_id {
            HOSTED_ROLE_VIEW => Some(Self::View),
            HOSTED_ROLE_TASKS => Some(Self::Tasks),
            HOSTED_ROLE_OPERATE => Some(Self::Operate),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedControlPolicy {
    #[serde(default)]
    pub ceiling: HostedPreset,
    #[serde(default = "default_max_ttl_secs")]
    pub max_ttl_secs: u64,
    #[serde(default)]
    pub eligible_session_ids: Vec<String>,
}

fn default_max_ttl_secs() -> u64 {
    DEFAULT_LEASE_TTL_SECS
}

impl Default for HostedControlPolicy {
    fn default() -> Self {
        Self {
            ceiling: HostedPreset::Tasks,
            max_ttl_secs: DEFAULT_LEASE_TTL_SECS,
            eligible_session_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostedLeaseRequestStatus {
    #[default]
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedLeaseRequest {
    pub protocol: String,
    pub request_id: String,
    pub request_nonce: String,
    pub browser_public_key: String,
    pub browser_key_fingerprint: String,
    pub requested_preset: HostedPreset,
    pub requested_ttl_secs: u64,
    pub requester_label: String,
    pub fleet_origin: String,
    pub daemon_id: String,
    pub daemon_label: String,
    pub daemon_public_key: String,
    pub created_unix_ms: u64,
    pub expires_unix_ms: u64,
    #[serde(default)]
    pub status: HostedLeaseRequestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_lease_id: Option<String>,
    pub doorbell_signature: String,
}

impl HostedLeaseRequest {
    pub fn signing_payload(&self) -> String {
        format!(
            "{protocol}\n{request_id}\n{request_nonce}\n{browser_public_key}\n{browser_key_fingerprint}\n{preset}\n{ttl}\n{requester_label}\n{fleet_origin}\n{daemon_id}\n{daemon_label}\n{daemon_public_key}\n{created}\n{expires}",
            protocol = self.protocol,
            request_id = self.request_id,
            request_nonce = self.request_nonce,
            browser_public_key = self.browser_public_key,
            browser_key_fingerprint = self.browser_key_fingerprint,
            preset = self.requested_preset.as_str(),
            ttl = self.requested_ttl_secs,
            requester_label = self.requester_label,
            fleet_origin = self.fleet_origin,
            daemon_id = self.daemon_id,
            daemon_label = self.daemon_label,
            daemon_public_key = self.daemon_public_key,
            created = self.created_unix_ms,
            expires = self.expires_unix_ms,
        )
    }

    pub fn document_sha256(&self) -> String {
        b64u(
            ring::digest::digest(&ring::digest::SHA256, self.signing_payload().as_bytes()).as_ref(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedLeaseDocument {
    pub protocol: String,
    pub lease_id: String,
    pub request_id: String,
    pub daemon_id: String,
    pub daemon_public_key: String,
    pub fleet_origin: String,
    pub browser_public_key: String,
    pub browser_key_fingerprint: String,
    pub preset: HostedPreset,
    pub issued_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub principal_id: String,
    pub grant_id: String,
    pub document_sha256: String,
    pub signature: String,
}

impl HostedLeaseDocument {
    pub fn unsigned_payload(&self) -> String {
        format!(
            "{protocol}\n{lease_id}\n{request_id}\n{daemon_id}\n{daemon_public_key}\n{fleet_origin}\n{browser_public_key}\n{browser_key_fingerprint}\n{preset}\n{issued}\n{expires}\n{principal_id}\n{grant_id}",
            protocol = self.protocol,
            lease_id = self.lease_id,
            request_id = self.request_id,
            daemon_id = self.daemon_id,
            daemon_public_key = self.daemon_public_key,
            fleet_origin = self.fleet_origin,
            browser_public_key = self.browser_public_key,
            browser_key_fingerprint = self.browser_key_fingerprint,
            preset = self.preset.as_str(),
            issued = self.issued_unix_ms,
            expires = self.expires_unix_ms,
            principal_id = self.principal_id,
            grant_id = self.grant_id,
        )
    }

    pub fn expected_document_sha256(&self) -> String {
        b64u(
            ring::digest::digest(&ring::digest::SHA256, self.unsigned_payload().as_bytes())
                .as_ref(),
        )
    }

    pub fn signing_payload(&self) -> String {
        format!("{}\n{}", self.unsigned_payload(), self.document_sha256)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostedLeaseStatus {
    #[default]
    Active,
    Revoked,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedLeaseRecord {
    pub document: HostedLeaseDocument,
    #[serde(default)]
    pub status: HostedLeaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedAppAnchor {
    pub device_id: String,
    pub label: String,
    pub public_key: String,
    pub key_fingerprint: String,
    pub distribution_id: String,
    #[serde(default)]
    pub active: bool,
    pub enrolled_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedControlState {
    #[serde(default)]
    pub policy: HostedControlPolicy,
    #[serde(default)]
    pub requests: Vec<HostedLeaseRequest>,
    #[serde(default)]
    pub leases: Vec<HostedLeaseRecord>,
    #[serde(default)]
    pub signed_app_anchors: Vec<SignedAppAnchor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedControlBootstrap {
    pub enabled: bool,
    pub daemon_id: String,
    pub daemon_label: String,
    pub daemon_public_key: String,
    pub fleet_origin: String,
    pub default_preset: HostedPreset,
    pub ceiling: HostedPreset,
    pub default_ttl_secs: u64,
    pub max_ttl_secs: u64,
    pub request_ttl_ms: u64,
    pub display_media_relay_configured: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedLeaseRequestInput {
    pub browser_public_key: String,
    #[serde(default)]
    pub requested_preset: HostedPreset,
    #[serde(default = "default_max_ttl_secs")]
    pub requested_ttl_secs: u64,
    #[serde(default)]
    pub requester_label: String,
    pub nonce: String,
    pub timestamp_unix_ms: i64,
    pub signature: String,
}

impl HostedLeaseRequestInput {
    pub fn proof_payload(&self, daemon_id: &str, fleet_origin: &str) -> String {
        format!(
            "{DOORBELL_REQUEST_PROOF_PROTOCOL}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.browser_public_key,
            self.requested_preset.as_str(),
            self.requested_ttl_secs,
            self.requester_label,
            fleet_origin,
            daemon_id,
            self.nonce,
            self.timestamp_unix_ms,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedLeaseDecisionInput {
    pub request_id: String,
    pub approve: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_preset: Option<HostedPreset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_ttl_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedLeasePollProof {
    pub request_id: String,
    pub nonce: String,
    pub timestamp_unix_ms: i64,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedLeasePollResult {
    pub request: HostedLeaseRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<HostedLeaseDocument>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedRequestProof {
    pub lease_id: String,
    pub nonce: String,
    pub timestamp_unix_ms: i64,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedWsTicket {
    pub ticket: String,
    pub expires_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostedControlManagementSnapshot {
    pub configured: bool,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_error: Option<String>,
    pub display_media_relay_configured: bool,
    pub anchor_decision_protocol: String,
    pub qualifying_signed_app_distribution_available: bool,
    pub policy: HostedControlPolicy,
    pub pending_requests: Vec<HostedLeaseRequest>,
    pub active_leases: Vec<HostedLeaseRecord>,
    pub signed_app_anchors: Vec<SignedAppAnchor>,
}

impl HostedControlState {
    pub fn normalize(&mut self) {
        self.policy.max_ttl_secs = self
            .policy
            .max_ttl_secs
            .clamp(MIN_LEASE_TTL_SECS, HARD_MAX_LEASE_TTL_SECS);
        self.policy
            .eligible_session_ids
            .retain(|value| valid_id_component(value));
        self.policy.eligible_session_ids.sort();
        self.policy.eligible_session_ids.dedup();
        retain_tail(
            &mut self.policy.eligible_session_ids,
            HOSTED_ELIGIBLE_SESSIONS_CAP,
        );

        self.requests
            .retain(|request| valid_id_component(&request.request_id));
        retain_tail(&mut self.requests, HOSTED_REQUESTS_CAP);
        self.leases
            .retain(|lease| valid_id_component(&lease.document.lease_id));
        retain_tail(&mut self.leases, HOSTED_LEASES_CAP);
        self.signed_app_anchors
            .retain(|anchor| valid_id_component(&anchor.device_id));
        retain_tail(&mut self.signed_app_anchors, HOSTED_ANCHORS_CAP);
    }
}

fn retain_tail<T>(values: &mut Vec<T>, cap: usize) {
    if values.len() > cap {
        values.drain(..values.len() - cap);
    }
}

pub fn valid_id_component(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
}
