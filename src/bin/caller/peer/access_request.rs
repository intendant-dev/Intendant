//! Peer access-request pairing.
//!
//! This is the "doorbell" flow: an unauthenticated caller may create one
//! bounded pending request containing only a requester public key and label.
//! Credentials are issued only after local approval on the target daemon.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::access;
use crate::error::CallerError;
use crate::peer::transport::pinning;
use crate::project::{PeerAccessRequestConfig, PeerConfig, Project};

use super::access_policy::unix_timestamp;
use super::pairing::{storage_slug, write_secret_file, JoinOutcome, AGENT_CARD_PATH};

pub(crate) const PUBLIC_REQUEST_PATH: &str = "/api/peer-pairing/requests";
const REQUEST_STORE_DIR: &str = "peer-access-requests";
const OUTGOING_STORE_DIR: &str = "peer-access-outgoing";
const DEFAULT_PROFILE: &str = super::access_policy::DEFAULT_PROFILE;
const REQUEST_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

static CREATE_RATE_LIMITER: OnceLock<StdMutex<CreateRateLimiter>> = OnceLock::new();

#[derive(Debug, Default)]
struct CreateRateLimiter {
    global: VecDeque<i64>,
    per_source: HashMap<String, VecDeque<i64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccessRequestCreate {
    pub version: u8,
    pub requester_label: String,
    pub public_key_pem: String,
    pub nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_card_url: Option<String>,
    /// Doorbell caller-ID (docs/src/trust-tiers.md § Two lanes): the
    /// requesting daemon proves its Ed25519 identity over this relayed,
    /// unauthenticated exchange. The signature covers the origin the
    /// requester DIALED, the enrollment key, the nonce, and a timestamp
    /// — so a captured request cannot be replayed against a different
    /// target, key, or ceremony. All-absent = a legacy requester
    /// (admitted, shown as an unverified caller). A target that predates
    /// these fields rejects them (`deny_unknown_fields`); the requester
    /// retries once without and notes the downgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_daemon_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_daemon_sig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_daemon_sig_ts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialed_origin: Option<String>,
    /// Cross-owner tier claim (docs/src/trust-tiers.md § Where fleet
    /// metadata rides): the requesting daemon's own trust tier, carried
    /// INSIDE the v2 caller-ID transcript so the claim is bound to the
    /// verified daemon identity above. Present only when the requester
    /// has a tier set; a claim without a verifying signature refuses the
    /// request — it is never admitted as a bare assertion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AccessRequestCreated {
    pub request_id: String,
    pub code: String,
    pub status: AccessRequestStatus,
    pub expires_at_unix: i64,
    pub target_label: String,
    pub target_card_url: String,
    pub server_cert_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AccessRequestStatusResponse {
    pub request_id: String,
    pub code: String,
    pub status: AccessRequestStatus,
    pub expires_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ApprovedAccessResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApprovedAccessResult {
    pub card_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub server_cert_fingerprint: String,
    pub client_cert_pem: String,
    pub approved_profile: String,
    pub approved_at_unix: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessRequestStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredAccessRequest {
    pub version: u8,
    pub request_id: String,
    pub code: String,
    pub status: AccessRequestStatus,
    pub requester_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_profile: Option<String>,
    pub public_key_pem: String,
    pub nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_card_url: Option<String>,
    /// The VERIFIED requesting daemon's Ed25519 identity (base64url
    /// public key). Set only when the caller-ID signature checked out —
    /// an absent value means a legacy/unproven caller, never a failed
    /// one (failures refuse the request outright).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_daemon_id: Option<String>,
    /// The tier the VERIFIED caller claimed for itself, from the v2
    /// doorbell transcript. Set only when the caller-ID signature over
    /// that claim checked out — an unverified tier claim is an assertion
    /// dressed as evidence and is never stored or shown
    /// (docs/src/trust-tiers.md § Where fleet metadata rides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hint: Option<String>,
    pub target_label: String,
    pub target_card_url: String,
    pub server_cert_fingerprint: String,
    pub created_at_unix: i64,
    pub expires_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_at_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_pem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutgoingAccessRequest {
    pub version: u8,
    pub request_id: String,
    pub code: String,
    pub status_url: String,
    pub target_card_url: String,
    pub server_cert_fingerprint: String,
    pub requester_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_profile: Option<String>,
    pub public_key_pem: String,
    pub client_key_path: PathBuf,
    pub created_at_unix: i64,
    pub expires_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_unix: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct InitiateAccessRequestOptions {
    pub target_url: String,
    pub requester_label: Option<String>,
    pub requested_profile: Option<String>,
    pub requester_card_url: Option<String>,
}

#[derive(Debug)]
pub(crate) struct PollAccessRequestOutcome {
    pub status: AccessRequestStatus,
    pub request_id: String,
    pub code: String,
    pub approved_profile: Option<String>,
    pub server_cert_fingerprint: Option<String>,
    pub install: Option<JoinOutcome>,
}

/// Doorbell caller-ID transcript (v1). Binds the origin the requester
/// dialed, the enrollment key being certified, the nonce, and a
/// timestamp under the requesting daemon's Ed25519 identity.
pub(crate) fn doorbell_transcript(
    dialed_origin: &str,
    public_key_pem: &str,
    nonce: &str,
    ts_unix_ms: i64,
) -> Vec<u8> {
    let key_digest = doorbell_key_digest(public_key_pem);
    format!("intendant-peer-doorbell-v1\n{dialed_origin}\n{key_digest}\n{nonce}\n{ts_unix_ms}")
        .into_bytes()
}

/// Doorbell caller-ID transcript (v2): v1's fields plus the requester's
/// own trust-tier claim as the final line (empty string when the
/// requester has no tier set). Carrying the tier INSIDE the signed
/// transcript is what turns "I'm disposable" from an assertion into a
/// claim pinned to a proven daemon key — and makes stripping or
/// rewriting the claim break the signature outright instead of quietly
/// demoting it (docs/src/trust-tiers.md § Where fleet metadata rides).
pub(crate) fn doorbell_transcript_v2(
    dialed_origin: &str,
    public_key_pem: &str,
    nonce: &str,
    ts_unix_ms: i64,
    requester_tier: &str,
) -> Vec<u8> {
    let key_digest = doorbell_key_digest(public_key_pem);
    format!(
        "intendant-peer-doorbell-v2\n{dialed_origin}\n{key_digest}\n{nonce}\n{ts_unix_ms}\n{requester_tier}"
    )
    .into_bytes()
}

fn doorbell_key_digest(public_key_pem: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, public_key_pem.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// Doorbell clock-skew tolerance. Wider than the dashboard offer bound:
/// pairing spans machines that have never met, where several minutes of
/// drift is routine; the nonce + one-shot request id carry replay
/// resistance.
const DOORBELL_MAX_SKEW_MS: i64 = 300_000;

/// A doorbell caller-ID that verified: the requesting daemon's proven
/// Ed25519 identity (base64url public key), plus the tier it claimed
/// for itself inside the v2 transcript (`None` under the v1 and
/// untiered-v2 transcripts — no claim was signed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedDoorbellCaller {
    pub daemon_id: String,
    pub tier: Option<String>,
}

/// Verify a doorbell request's caller-ID fields. Pure core:
/// - all fields absent → `Ok(None)` (legacy caller, admitted unverified);
/// - a valid signature whose dialed origin matches the origin this
///   daemon received the request on → `Ok(Some(caller))`. The transcript
///   dispatches on the tier claim: `requester_tier` present → it must
///   name a known daemon tier AND the v2 transcript carrying it must
///   verify; absent → the v1 transcript (a requester that predates the
///   tier claim) or the untiered v2 transcript (a current requester
///   with no tier set) must verify;
/// - anything else (partial fields, bad signature, origin mismatch,
///   stale timestamp, an unknown or unsigned tier claim) → `Err` — the
///   request is refused, so a captured or tampered caller-ID can never
///   demote itself to merely "unverified" and still ring the doorbell.
pub(crate) fn verify_doorbell_caller(
    request: &AccessRequestCreate,
    received_origin: &str,
    now_unix_ms: i64,
) -> Result<Option<VerifiedDoorbellCaller>, String> {
    let present = request.requester_daemon_id.is_some()
        || request.requester_daemon_sig.is_some()
        || request.requester_daemon_sig_ts.is_some()
        || request.dialed_origin.is_some()
        || request.requester_tier.is_some();
    if !present {
        return Ok(None);
    }
    let daemon_id = request
        .requester_daemon_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or("caller id is missing requester_daemon_id")?;
    if daemon_id.len() > 128 {
        return Err("requester_daemon_id is too long".into());
    }
    let sig = request
        .requester_daemon_sig
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or("caller id is missing its signature")?;
    let ts = request
        .requester_daemon_sig_ts
        .ok_or("caller id is missing its timestamp")?;
    let dialed = request
        .dialed_origin
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or("caller id is missing the dialed origin")?;
    let skew = (now_unix_ms - ts).abs();
    if skew > DOORBELL_MAX_SKEW_MS {
        return Err(format!(
            "caller id timestamp is {skew}ms from daemon time (max {DOORBELL_MAX_SKEW_MS}ms)"
        ));
    }
    if !origins_match(dialed, received_origin) {
        return Err(format!(
            "caller dialed {dialed} but this daemon received the request at {received_origin}"
        ));
    }
    // Tier claim (docs/src/trust-tiers.md § Where fleet metadata rides):
    // an unknown tier string in a signed claim is a validation error —
    // refused, never passed through. Vocabulary membership is exact: the
    // requester signs the normalized value its own IAM stores.
    let claimed_tier = match request.requester_tier.as_deref() {
        None => None,
        Some(tier) => {
            if !crate::access::iam::DAEMON_TIERS.contains(&tier) {
                return Err(format!(
                    "unknown requester tier {tier:?} (expected one of: {})",
                    crate::access::iam::DAEMON_TIERS.join(", ")
                ));
            }
            Some(tier.to_string())
        }
    };
    let signature_ok = match claimed_tier.as_deref() {
        // A stated tier is only ever accepted from the v2 transcript
        // that binds it — a v1 signature with a tier field bolted on is
        // a claim outside what was signed, and refuses.
        Some(tier) => {
            let transcript =
                doorbell_transcript_v2(dialed, &request.public_key_pem, &request.nonce, ts, tier);
            crate::daemon_identity::verify_b64u(daemon_id, &transcript, sig)
        }
        // No claim: current requesters sign the untiered v2 transcript
        // (empty tier line, field omitted); requesters that predate the
        // tier claim signed v1. Either proves the same thing — identity
        // with no tier stated — and stripping the tier from a v2-with-
        // tier request matches neither, so it refuses outright.
        None => {
            let v2 =
                doorbell_transcript_v2(dialed, &request.public_key_pem, &request.nonce, ts, "");
            crate::daemon_identity::verify_b64u(daemon_id, &v2, sig) || {
                let v1 = doorbell_transcript(dialed, &request.public_key_pem, &request.nonce, ts);
                crate::daemon_identity::verify_b64u(daemon_id, &v1, sig)
            }
        }
    };
    if !signature_ok {
        return Err("caller id signature verification failed".into());
    }
    Ok(Some(VerifiedDoorbellCaller {
        daemon_id: daemon_id.to_string(),
        tier: claimed_tier,
    }))
}

/// Origin comparison for the dialed-vs-received check: scheme + host +
/// port, case-insensitive host, default ports normalized.
fn origins_match(a: &str, b: &str) -> bool {
    fn norm(v: &str) -> Option<(String, String, u16)> {
        let url = url::Url::parse(v.trim()).ok()?;
        let scheme = url.scheme().to_ascii_lowercase();
        let host = url.host_str()?.to_ascii_lowercase();
        let port = url
            .port()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        Some((scheme, host, port))
    }
    match (norm(a), norm(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

pub(crate) fn create_pending_request(
    cert_dir: &Path,
    request: AccessRequestCreate,
    target_card_url: String,
    source_hint: Option<String>,
    config: &PeerAccessRequestConfig,
) -> Result<AccessRequestCreated, CallerError> {
    if !public_requests_enabled(config) {
        return Err(CallerError::Config(
            "peer access requests are disabled by configuration".into(),
        ));
    }
    validate_create_request(&request)?;
    // Caller-ID (docs/src/trust-tiers.md § Two lanes): the origin we
    // received the request on is the card URL's origin — the Host the
    // requester actually dialed. Invalid caller-ID refuses the request;
    // absent caller-ID is a legacy requester, admitted unverified.
    let received_origin = target_card_url
        .strip_suffix(super::pairing::AGENT_CARD_PATH)
        .unwrap_or(&target_card_url)
        .trim_end_matches('/')
        .to_string();
    let verified_caller =
        verify_doorbell_caller(&request, &received_origin, unix_timestamp() * 1000).map_err(
            |e| CallerError::Config(format!("caller identity verification failed: {e}")),
        )?;
    // The stored identity AND tier both come only from the verified
    // caller — never from the raw wire fields, so a tier that arrived
    // without a verifying v2 signature can never reach the store.
    let (verified_requester_daemon_id, verified_requester_tier) = match verified_caller {
        Some(caller) => (Some(caller.daemon_id), caller.tier),
        None => (None, None),
    };
    enforce_create_rate_limits(source_hint.as_deref(), config)?;
    prune_expired(cert_dir)?;
    enforce_pending_limits(cert_dir, source_hint.as_deref(), config)?;

    let server_cert_fingerprint = access::certs::read_server_cert_fingerprint(cert_dir)
        .ok_or_else(|| {
            CallerError::Config(format!(
                "no server.crt found in {} — run `intendant access setup` first",
                cert_dir.display()
            ))
        })?;
    crate::peer::transport::pinning::parse_fingerprint(&server_cert_fingerprint).map_err(|e| {
        CallerError::Config(format!("local server cert fingerprint is invalid: {e}"))
    })?;

    let target_label = access::resolve_host_label();
    let request_id = request_id_for(&request, &server_cert_fingerprint);
    let code = verification_code_for(
        &request.public_key_pem,
        &request.nonce,
        &server_cert_fingerprint,
        &target_card_url,
    );
    let now = unix_timestamp();
    let expires_at_unix = now + effective_ttl_secs(config);
    let path = request_path(cert_dir, &request_id);
    if let Some(existing) = read_request_path(&path)? {
        if !matches!(effective_status(&existing), AccessRequestStatus::Pending) {
            return Err(CallerError::Config(format!(
                "pairing request {} is already {:?}",
                existing.code, existing.status
            )));
        }
    }

    let stored = StoredAccessRequest {
        version: 1,
        request_id: request_id.clone(),
        code: code.clone(),
        status: AccessRequestStatus::Pending,
        requester_label: clean_label(&request.requester_label)?,
        requested_profile: request
            .requested_profile
            .as_deref()
            .map(clean_profile)
            .transpose()?,
        approved_profile: None,
        public_key_pem: request.public_key_pem,
        nonce: request.nonce,
        requester_card_url: request.requester_card_url,
        requester_daemon_id: verified_requester_daemon_id,
        requester_tier: verified_requester_tier,
        source_hint,
        target_label: target_label.clone(),
        target_card_url: target_card_url.clone(),
        server_cert_fingerprint: server_cert_fingerprint.clone(),
        created_at_unix: now,
        expires_at_unix,
        approved_at_unix: None,
        denied_at_unix: None,
        client_cert_pem: None,
    };
    write_request(cert_dir, &stored)?;
    eprintln!(
        "intendant: peer access request {} from {}{}; approve with `intendant peer approve {}`",
        stored.code,
        stored.requester_label,
        stored
            .source_hint
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default(),
        stored.code,
    );

    Ok(AccessRequestCreated {
        request_id,
        code,
        status: AccessRequestStatus::Pending,
        expires_at_unix,
        target_label,
        target_card_url,
        server_cert_fingerprint,
    })
}

pub(crate) fn request_status(
    cert_dir: &Path,
    request_id: &str,
) -> Result<AccessRequestStatusResponse, CallerError> {
    validate_request_id(request_id)?;
    let path = request_path(cert_dir, request_id);
    let mut stored = read_request_path(&path)?
        .ok_or_else(|| CallerError::Config("pairing request not found".into()))?;
    let status = effective_status(&stored);
    if status == AccessRequestStatus::Expired && stored.status != AccessRequestStatus::Expired {
        stored.status = AccessRequestStatus::Expired;
        write_request(cert_dir, &stored)?;
    }
    Ok(status_response(&stored))
}

pub(crate) fn list_requests(cert_dir: &Path) -> Result<Vec<StoredAccessRequest>, CallerError> {
    prune_expired(cert_dir)?;
    let mut requests = read_all_requests(cert_dir)?;
    requests.sort_by_key(|r| std::cmp::Reverse(r.created_at_unix));
    Ok(requests)
}

pub(crate) fn approve_request(
    cert_dir: &Path,
    code_or_id: &str,
    profile_override: Option<&str>,
) -> Result<StoredAccessRequest, CallerError> {
    prune_expired(cert_dir)?;
    let mut stored = find_request(cert_dir, code_or_id)?;
    if effective_status(&stored) != AccessRequestStatus::Pending {
        return Err(CallerError::Config(format!(
            "pairing request {} is {:?}, not pending",
            stored.code, stored.status
        )));
    }
    let profile = profile_override
        .map(clean_profile)
        .transpose()?
        .or_else(|| stored.requested_profile.clone())
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string());
    let cert_pem = access::certs::issue_client_certificate_for_public_key(
        cert_dir,
        &stored.requester_label,
        &stored.public_key_pem,
    )
    .map_err(|e| CallerError::Config(e.to_string()))?;
    let client_fingerprint = super::access_policy::fingerprint_pem(&cert_pem)?;
    super::access_policy::write_approved_identity(
        cert_dir,
        &client_fingerprint,
        &stored.requester_label,
        &profile,
        stored.requester_card_url.as_deref(),
        Some(&stored.request_id),
    )?;
    stored.status = AccessRequestStatus::Approved;
    stored.approved_profile = Some(profile);
    stored.approved_at_unix = Some(unix_timestamp());
    stored.client_cert_pem = Some(cert_pem);
    write_request(cert_dir, &stored)?;
    Ok(stored)
}

pub(crate) fn deny_request(
    cert_dir: &Path,
    code_or_id: &str,
) -> Result<StoredAccessRequest, CallerError> {
    prune_expired(cert_dir)?;
    let mut stored = find_request(cert_dir, code_or_id)?;
    if effective_status(&stored) != AccessRequestStatus::Pending {
        return Err(CallerError::Config(format!(
            "pairing request {} is {:?}, not pending",
            stored.code, stored.status
        )));
    }
    stored.status = AccessRequestStatus::Denied;
    stored.denied_at_unix = Some(unix_timestamp());
    write_request(cert_dir, &stored)?;
    Ok(stored)
}

pub(crate) async fn initiate_access_request(
    cert_dir: &Path,
    options: InitiateAccessRequestOptions,
) -> Result<OutgoingAccessRequest, CallerError> {
    let endpoint = target_request_endpoint(&options.target_url)?;
    let key = access::certs::generate_client_key_material()
        .map_err(|e| CallerError::Config(e.to_string()))?;
    let requester_label = options
        .requester_label
        .as_deref()
        .map(clean_label)
        .transpose()?
        .unwrap_or_else(access::resolve_host_label);
    let requested_profile = options
        .requested_profile
        .as_deref()
        .map(clean_profile)
        .transpose()?;
    let mut request = AccessRequestCreate {
        version: 1,
        requester_label: requester_label.clone(),
        public_key_pem: key.public_key_pem.clone(),
        nonce: uuid::Uuid::new_v4().to_string(),
        requested_profile: requested_profile.clone(),
        requester_card_url: options.requester_card_url,
        requester_daemon_id: None,
        requester_daemon_sig: None,
        requester_daemon_sig_ts: None,
        dialed_origin: None,
        requester_tier: None,
    };
    // Caller-ID: prove this daemon's identity over the doorbell. Best
    // effort — a box without a loadable identity still rings the bell,
    // it just shows as an unverified caller on the approval side.
    if let Some(origin) = request_origin(&endpoint) {
        match crate::daemon_identity::DaemonIdentity::load_or_create_default() {
            Ok(identity) => {
                let ts = unix_timestamp() * 1000;
                // Tier claim: this daemon's own tier — the same IAM
                // state the access overview reads, resolved under
                // `cert_dir` — rides INSIDE the signed v2 transcript.
                // No tier set → the transcript's tier line is empty and
                // the wire field is omitted.
                let tier = crate::access::iam::load_state_for_overview(cert_dir)
                    .state
                    .tier
                    .unwrap_or_default();
                let transcript = doorbell_transcript_v2(
                    &origin,
                    &request.public_key_pem,
                    &request.nonce,
                    ts,
                    &tier,
                );
                request.requester_daemon_id = Some(identity.public_key_b64u());
                request.requester_daemon_sig = Some(identity.sign_b64u(&transcript));
                request.requester_daemon_sig_ts = Some(ts);
                request.dialed_origin = Some(origin);
                request.requester_tier = (!tier.is_empty()).then_some(tier);
            }
            Err(e) => eprintln!("[peer-request] caller-id skipped (no daemon identity): {e}"),
        }
    }
    let (client, captured_fingerprint) = bootstrap_capturing_http_client()?;
    let resp = client
        .post(&endpoint)
        .json(&request)
        .send()
        .await
        .map_err(|e| CallerError::Config(format!("send access request: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| CallerError::Config(format!("read access request response: {e}")))?;
    // The caller-ID fields are the only supported shape. A target that
    // predates them rejects the unknown fields (`deny_unknown_fields` →
    // 400); we surface that as a hard error rather than silently
    // downgrading to an unauthenticated bare request, which an on-path
    // attacker could force by injecting a 400.
    if !status.is_success() {
        return Err(CallerError::Config(format!(
            "target rejected access request ({status}): {text}"
        )));
    }
    let created: AccessRequestCreated = serde_json::from_str(&text)?;
    // Over TLS, bind the fingerprint the target reports to the cert it
    // actually presented (pinned on first contact). This is what makes
    // the permanent peer pin the requester stores below the transport
    // identity it authenticated — not a value an on-path party chose.
    // A cleartext loopback target has no transport cert to compare.
    if endpoint.starts_with("https://") {
        verify_reported_matches_pinned(&captured_fingerprint, &created.server_cert_fingerprint)?;
    }
    let outgoing_dir = outgoing_request_dir(cert_dir, &created.request_id);
    std::fs::create_dir_all(&outgoing_dir)?;
    let client_key_path = outgoing_dir.join("client.key");
    write_secret_file(&client_key_path, &key.key_pem)?;
    let outgoing = OutgoingAccessRequest {
        version: 1,
        request_id: created.request_id.clone(),
        code: created.code.clone(),
        status_url: format!("{}/{}", endpoint.trim_end_matches('/'), created.request_id),
        target_card_url: created.target_card_url,
        server_cert_fingerprint: created.server_cert_fingerprint,
        requester_label,
        requested_profile,
        public_key_pem: key.public_key_pem,
        client_key_path,
        created_at_unix: unix_timestamp(),
        expires_at_unix: created.expires_at_unix,
        completed_at_unix: None,
    };
    write_outgoing_request(cert_dir, &outgoing)?;
    Ok(outgoing)
}

pub(crate) async fn poll_access_request(
    project: &mut Project,
    cert_dir: &Path,
    request_id: &str,
    label_override: Option<&str>,
) -> Result<PollAccessRequestOutcome, CallerError> {
    validate_request_id(request_id)?;
    let mut outgoing = read_outgoing_request(cert_dir, request_id)?
        .ok_or_else(|| CallerError::Config("outgoing access request not found".into()))?;
    // Pin the status poll to the exact server cert learned (and verified
    // against the transport) during the initial request — the whole
    // ceremony rides one authenticated identity.
    let client = bootstrap_pinned_http_client(&outgoing.server_cert_fingerprint)?;
    let resp = client
        .get(&outgoing.status_url)
        .send()
        .await
        .map_err(|e| CallerError::Config(format!("poll access request: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| CallerError::Config(format!("read access request status: {e}")))?;
    if !status.is_success() {
        return Err(CallerError::Config(format!(
            "target rejected access request status ({status}): {text}"
        )));
    }
    let remote: AccessRequestStatusResponse = serde_json::from_str(&text)?;
    if remote.status != AccessRequestStatus::Approved {
        return Ok(PollAccessRequestOutcome {
            status: remote.status,
            request_id: remote.request_id,
            code: remote.code,
            approved_profile: remote.approved_profile,
            server_cert_fingerprint: None,
            install: None,
        });
    }
    let result = remote
        .result
        .ok_or_else(|| CallerError::Config("approved access request is missing result".into()))?;
    // The approved result's server fingerprint must equal the identity
    // pinned for the whole flow, so a swapped fingerprint in the
    // approval can't redirect the permanent peer pin installed below.
    let pinned_fp = pinning::parse_fingerprint(&outgoing.server_cert_fingerprint)
        .map_err(|e| CallerError::Config(format!("stored target fingerprint is invalid: {e}")))?;
    let approved_fp = pinning::parse_fingerprint(&result.server_cert_fingerprint).map_err(|e| {
        CallerError::Config(format!(
            "approved result has invalid server fingerprint: {e}"
        ))
    })?;
    if pinned_fp != approved_fp {
        return Err(CallerError::Config(format!(
            "approved result server fingerprint {} does not match the pinned target identity {} — refusing to install",
            pinning::format_fingerprint(&approved_fp),
            pinning::format_fingerprint(&pinned_fp),
        )));
    }
    let key_pem = std::fs::read_to_string(&outgoing.client_key_path)?;
    let install = install_approved_identity(
        project,
        cert_dir,
        &result,
        &key_pem,
        label_override.or(result.label.as_deref()),
    )?;
    outgoing.completed_at_unix = Some(unix_timestamp());
    write_outgoing_request(cert_dir, &outgoing)?;
    Ok(PollAccessRequestOutcome {
        status: AccessRequestStatus::Approved,
        request_id: remote.request_id,
        code: remote.code,
        approved_profile: remote.approved_profile,
        server_cert_fingerprint: Some(result.server_cert_fingerprint.clone()),
        install: Some(install),
    })
}

pub(crate) fn install_approved_identity(
    project: &mut Project,
    cert_dir: &Path,
    result: &ApprovedAccessResult,
    client_key_pem: &str,
    label_override: Option<&str>,
) -> Result<JoinOutcome, CallerError> {
    crate::peer::transport::pinning::parse_fingerprint(&result.server_cert_fingerprint).map_err(
        |e| {
            CallerError::Config(format!(
                "approved result has invalid server fingerprint: {e}"
            ))
        },
    )?;
    let peer_dir = cert_dir.join("peers").join(storage_slug(
        label_override.or(result.label.as_deref()),
        &result.card_url,
    ));
    std::fs::create_dir_all(&peer_dir)?;

    let cert_path = peer_dir.join("client.crt");
    let key_path = peer_dir.join("client.key");
    std::fs::write(&cert_path, result.client_cert_pem.as_bytes())?;
    write_secret_file(&key_path, client_key_pem)?;

    let label = label_override
        .map(str::to_string)
        .or_else(|| result.label.clone());
    let pins = vec![result.server_cert_fingerprint.clone()];
    let existing = project
        .config
        .peers
        .iter_mut()
        .find(|peer| peer.card_url == result.card_url);
    let updated_existing = existing.is_some();
    match existing {
        Some(peer) => {
            if label.is_some() {
                peer.label = label;
            }
            peer.client_cert = Some(cert_path.to_string_lossy().into_owned());
            peer.client_key = Some(key_path.to_string_lossy().into_owned());
            peer.pinned_fingerprints = pins;
        }
        None => {
            project.config.peers.push(PeerConfig {
                card_url: result.card_url.clone(),
                label,
                bearer_token: None,
                via_urls: Vec::new(),
                client_cert: Some(cert_path.to_string_lossy().into_owned()),
                client_key: Some(key_path.to_string_lossy().into_owned()),
                pinned_fingerprints: pins,
                browser_tcp_via_url: None,
                certificate_witness_vantage: crate::peer::PeerWitnessVantage::Unknown,
            });
        }
    }

    project.save_config()?;
    Ok(JoinOutcome {
        card_url: result.card_url.clone(),
        config_path: project.root.join("intendant.toml"),
        client_cert_path: cert_path,
        client_key_path: key_path,
        updated_existing,
    })
}

fn validate_create_request(request: &AccessRequestCreate) -> Result<(), CallerError> {
    if request.version != 1 {
        return Err(CallerError::Config(format!(
            "unsupported access request version {}",
            request.version
        )));
    }
    clean_label(&request.requester_label)?;
    if let Some(profile) = &request.requested_profile {
        clean_profile(profile)?;
    }
    if request.nonce.len() < 16 || request.nonce.len() > 128 {
        return Err(CallerError::Config(
            "nonce must be between 16 and 128 characters".into(),
        ));
    }
    if request.public_key_pem.len() > 2048 {
        return Err(CallerError::Config("public key is too large".into()));
    }
    rcgen::SubjectPublicKeyInfo::from_pem(&request.public_key_pem)
        .map_err(|e| CallerError::Config(format!("invalid public key: {e}")))?;
    if let Some(url) = &request.requester_card_url {
        super::pairing::normalize_card_url(url)?;
    }
    Ok(())
}

fn clean_label(raw: &str) -> Result<String, CallerError> {
    let label = raw.trim();
    if label.is_empty() {
        return Err(CallerError::Config("label cannot be empty".into()));
    }
    if label.len() > 80 {
        return Err(CallerError::Config("label must be at most 80 bytes".into()));
    }
    Ok(label.to_string())
}

fn clean_profile(raw: &str) -> Result<String, CallerError> {
    super::access_policy::normalize_profile(raw)
}

fn status_response(stored: &StoredAccessRequest) -> AccessRequestStatusResponse {
    let status = effective_status(stored);
    let result = if status == AccessRequestStatus::Approved {
        stored
            .client_cert_pem
            .as_ref()
            .map(|cert| ApprovedAccessResult {
                card_url: stored.target_card_url.clone(),
                label: Some(stored.target_label.clone()),
                server_cert_fingerprint: stored.server_cert_fingerprint.clone(),
                client_cert_pem: cert.clone(),
                approved_profile: stored
                    .approved_profile
                    .clone()
                    .unwrap_or_else(|| DEFAULT_PROFILE.to_string()),
                approved_at_unix: stored.approved_at_unix.unwrap_or(stored.created_at_unix),
            })
    } else {
        None
    };
    AccessRequestStatusResponse {
        request_id: stored.request_id.clone(),
        code: stored.code.clone(),
        status,
        expires_at_unix: stored.expires_at_unix,
        approved_profile: stored.approved_profile.clone(),
        result,
    }
}

fn effective_status(stored: &StoredAccessRequest) -> AccessRequestStatus {
    if stored.status == AccessRequestStatus::Pending && unix_timestamp() > stored.expires_at_unix {
        AccessRequestStatus::Expired
    } else {
        stored.status
    }
}

fn request_id_for(request: &AccessRequestCreate, server_cert_fingerprint: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(request.public_key_pem.as_bytes());
    hasher.update(b"\0");
    hasher.update(request.nonce.as_bytes());
    hasher.update(b"\0");
    hasher.update(server_cert_fingerprint.as_bytes());
    let digest = hasher.finalize();
    // The 'r' prefix keeps the id from ever starting with base64url's
    // '-', which argv parsers (this CLI's `peer complete <id>` included)
    // read as a flag. Ids are opaque: minted here once, carried on the
    // wire and in store paths, never decoded — old unprefixed ids stay
    // valid, and the CLI additionally tolerates their leading dash.
    format!("r{}", URL_SAFE_NO_PAD.encode(&digest[..18]))
}

fn verification_code_for(
    public_key_pem: &str,
    nonce: &str,
    server_cert_fingerprint: &str,
    target_card_url: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key_pem.as_bytes());
    hasher.update(b"\0");
    hasher.update(nonce.as_bytes());
    hasher.update(b"\0");
    hasher.update(server_cert_fingerprint.as_bytes());
    hasher.update(b"\0");
    hasher.update(target_card_url.as_bytes());
    let digest = hasher.finalize();
    format!("{}-{}", hex_upper(&digest[..2]), hex_upper(&digest[2..4]))
}

fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

fn request_store_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.join(REQUEST_STORE_DIR)
}

fn outgoing_store_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.join(OUTGOING_STORE_DIR)
}

fn outgoing_request_dir(cert_dir: &Path, request_id: &str) -> PathBuf {
    outgoing_store_dir(cert_dir).join(request_id)
}

fn request_path(cert_dir: &Path, request_id: &str) -> PathBuf {
    request_store_dir(cert_dir).join(format!("{request_id}.json"))
}

fn outgoing_request_path(cert_dir: &Path, request_id: &str) -> PathBuf {
    outgoing_request_dir(cert_dir, request_id).join("request.json")
}

fn write_request(cert_dir: &Path, stored: &StoredAccessRequest) -> Result<(), CallerError> {
    std::fs::create_dir_all(request_store_dir(cert_dir))?;
    let body = serde_json::to_string_pretty(stored)?;
    std::fs::write(request_path(cert_dir, &stored.request_id), body)?;
    Ok(())
}

fn read_request_path(path: &Path) -> Result<Option<StoredAccessRequest>, CallerError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn read_all_requests(cert_dir: &Path) -> Result<Vec<StoredAccessRequest>, CallerError> {
    let dir = request_store_dir(cert_dir);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(request) = read_request_path(&path)? {
            out.push(request);
        }
    }
    Ok(out)
}

fn prune_expired(cert_dir: &Path) -> Result<(), CallerError> {
    for mut request in read_all_requests(cert_dir)? {
        if request.status == AccessRequestStatus::Pending
            && unix_timestamp() > request.expires_at_unix
        {
            request.status = AccessRequestStatus::Expired;
            write_request(cert_dir, &request)?;
        }
    }
    Ok(())
}

pub(crate) fn effective_body_limit_bytes(config: &PeerAccessRequestConfig) -> usize {
    config.body_limit_bytes.max(1)
}

fn effective_ttl_secs(config: &PeerAccessRequestConfig) -> i64 {
    config.ttl_secs.max(1)
}

fn effective_rate_limit_window_secs(config: &PeerAccessRequestConfig) -> i64 {
    config.rate_limit_window_secs.max(1)
}

fn enforce_pending_limits(
    cert_dir: &Path,
    source_hint: Option<&str>,
    config: &PeerAccessRequestConfig,
) -> Result<(), CallerError> {
    let requests = read_all_requests(cert_dir)?;
    let pending: Vec<&StoredAccessRequest> = requests
        .iter()
        .filter(|r| effective_status(r) == AccessRequestStatus::Pending)
        .collect();
    if pending.len() >= config.max_pending {
        return Err(CallerError::Config(
            "too many pending peer access requests; approve, deny, or wait for expiry".into(),
        ));
    }
    if let Some(source) = source_hint {
        let source_count = pending
            .iter()
            .filter(|r| r.source_hint.as_deref() == Some(source))
            .count();
        if source_count >= config.max_pending_per_source {
            return Err(CallerError::Config(format!(
                "too many pending peer access requests from {source}"
            )));
        }
    }
    Ok(())
}

fn public_requests_enabled(config: &PeerAccessRequestConfig) -> bool {
    config.enabled && public_requests_enabled_from_env()
}

fn public_requests_enabled_from_env() -> bool {
    match std::env::var("INTENDANT_PEER_ACCESS_REQUESTS") {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "0" | "false" | "off" | "no" | "disabled")
        }
        Err(_) => true,
    }
}

fn enforce_create_rate_limits(
    source_hint: Option<&str>,
    config: &PeerAccessRequestConfig,
) -> Result<(), CallerError> {
    let now = unix_timestamp();
    let limiter = CREATE_RATE_LIMITER.get_or_init(|| StdMutex::new(CreateRateLimiter::default()));
    let mut limiter = limiter
        .lock()
        .map_err(|_| CallerError::Config("peer access request rate limiter poisoned".into()))?;
    let window_secs = effective_rate_limit_window_secs(config);

    prune_rate_queue(&mut limiter.global, now, window_secs);
    if limiter.global.len() >= config.max_creates_per_window {
        return Err(CallerError::Config(
            "peer access request rate limit exceeded".into(),
        ));
    }

    let source = source_hint.unwrap_or("unknown").to_string();
    {
        let source_queue = limiter.per_source.entry(source.clone()).or_default();
        prune_rate_queue(source_queue, now, window_secs);
        if source_queue.len() >= config.max_creates_per_source_per_window {
            return Err(CallerError::Config(format!(
                "peer access request rate limit exceeded for {source}"
            )));
        }
    }

    limiter.global.push_back(now);
    limiter.per_source.entry(source).or_default().push_back(now);
    Ok(())
}

fn prune_rate_queue(queue: &mut VecDeque<i64>, now: i64, window_secs: i64) {
    while let Some(ts) = queue.front().copied() {
        if now - ts < window_secs {
            break;
        }
        queue.pop_front();
    }
}

fn find_request(cert_dir: &Path, code_or_id: &str) -> Result<StoredAccessRequest, CallerError> {
    let needle = normalize_code(code_or_id);
    for request in read_all_requests(cert_dir)? {
        if request.request_id == code_or_id || normalize_code(&request.code) == needle {
            return Ok(request);
        }
    }
    Err(CallerError::Config("pairing request not found".into()))
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

fn validate_request_id(request_id: &str) -> Result<(), CallerError> {
    let valid = !request_id.is_empty()
        && request_id.len() <= 80
        && request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(CallerError::Config("invalid pairing request id".into()))
    }
}

fn write_outgoing_request(
    cert_dir: &Path,
    outgoing: &OutgoingAccessRequest,
) -> Result<(), CallerError> {
    std::fs::create_dir_all(outgoing_request_dir(cert_dir, &outgoing.request_id))?;
    let body = serde_json::to_string_pretty(outgoing)?;
    std::fs::write(outgoing_request_path(cert_dir, &outgoing.request_id), body)?;
    Ok(())
}

fn read_outgoing_request(
    cert_dir: &Path,
    request_id: &str,
) -> Result<Option<OutgoingAccessRequest>, CallerError> {
    let path = outgoing_request_path(cert_dir, request_id);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn target_request_endpoint(raw: &str) -> Result<String, CallerError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(CallerError::Config("target URL is required".into()));
    }
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
        return Err(CallerError::Config(
            "target URL must start with https:// or http://".into(),
        ));
    }
    // Cleartext is permitted only to a loopback target (local testing).
    // A non-loopback `http://` doorbell would ring — and pin the
    // resulting peer identity — over an unauthenticated, tamperable
    // transport, exactly the MITM the pinning above closes.
    if trimmed.starts_with("http://") && !endpoint_is_loopback(trimmed) {
        return Err(CallerError::Config(
            "refusing a non-loopback http:// peer access request target; use https://".into(),
        ));
    }
    if trimmed.ends_with(PUBLIC_REQUEST_PATH) {
        return Ok(trimmed.to_string());
    }
    let base = trimmed
        .strip_suffix(AGENT_CARD_PATH)
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    Ok(format!("{base}{PUBLIC_REQUEST_PATH}"))
}

/// The origin (scheme://host:port) of the doorbell endpoint — the value
/// the caller-ID signature binds as "where I meant to ring".
fn request_origin(endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    let host = url.host_str()?;
    let scheme = url.scheme();
    match url.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// Slot holding the SHA-256 fingerprint of the first server cert the
/// capturing bootstrap client saw. Read back after the request to bind
/// the reported fingerprint to the transport identity.
type CapturedFingerprint = StdMutex<Option<pinning::Fingerprint>>;

/// Bootstrap TLS verifier for the outbound doorbell: pin-on-first-
/// contact. Peer daemons serve their own self-signed certs, so there is
/// no CA to anchor to on first contact — instead the first cert
/// presented is recorded and accepted, and every later handshake in the
/// same flow must present the identical cert (a mid-flow swap fails).
/// The recorded fingerprint is checked after the exchange against the
/// fingerprint the target reports, so the permanent peer pin can only
/// ever be the transport identity the requester actually spoke to. This
/// replaces the previous blanket `danger_accept_invalid_certs(true)`,
/// which accepted any cert and pinned a target-reported fingerprint it
/// never compared to the transport.
#[derive(Debug)]
struct CaptureOnFirstContactVerifier {
    provider: std::sync::Arc<rustls::crypto::CryptoProvider>,
    seen: std::sync::Arc<CapturedFingerprint>,
}

impl rustls::client::danger::ServerCertVerifier for CaptureOnFirstContactVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = pinning::fingerprint_of_der(end_entity.as_ref());
        let mut seen = self
            .seen
            .lock()
            .map_err(|_| rustls::Error::General("bootstrap pin slot poisoned".into()))?;
        match *seen {
            None => {
                *seen = Some(fp);
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            Some(prev) if prev == fp => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::General(
                "server presented a different certificate mid-flow".into(),
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn bootstrap_crypto_provider() -> std::sync::Arc<rustls::crypto::CryptoProvider> {
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(rustls::crypto::ring::default_provider()))
}

/// Bootstrap client for the initial doorbell POST: pins the first
/// server cert it sees and hands back the slot so the caller can bind
/// the reported fingerprint to that transport identity.
fn bootstrap_capturing_http_client(
) -> Result<(reqwest::Client, std::sync::Arc<CapturedFingerprint>), CallerError> {
    let seen: std::sync::Arc<CapturedFingerprint> = std::sync::Arc::new(StdMutex::new(None));
    let provider = bootstrap_crypto_provider();
    let verifier = CaptureOnFirstContactVerifier {
        provider: provider.clone(),
        seen: seen.clone(),
    };
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|e| CallerError::Config(format!("bootstrap TLS versions: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
        .with_no_client_auth();
    let client = reqwest::Client::builder()
        .timeout(REQUEST_HTTP_TIMEOUT)
        .use_preconfigured_tls(config)
        .build()
        .map_err(|e| CallerError::Config(format!("build bootstrap HTTP client: {e}")))?;
    Ok((client, seen))
}

/// Bootstrap client for the status poll: pins the exact server cert
/// fingerprint learned (and verified against the transport) during the
/// initial request, so the whole ceremony is bound to one identity.
fn bootstrap_pinned_http_client(
    server_cert_fingerprint: &str,
) -> Result<reqwest::Client, CallerError> {
    let verifier = pinning::PinnedFingerprintVerifier::from_strings([server_cert_fingerprint])
        .map_err(|e| CallerError::Config(format!("pin target server cert: {e}")))?;
    let config = pinning::pinned_client_config(verifier);
    reqwest::Client::builder()
        .timeout(REQUEST_HTTP_TIMEOUT)
        .use_preconfigured_tls(config)
        .build()
        .map_err(|e| CallerError::Config(format!("build bootstrap HTTP client: {e}")))
}

/// Confirm the fingerprint the target reported equals the transport
/// identity we pinned on first contact. Fails closed: a `None` slot
/// (no TLS cert captured) or a mismatch refuses the pairing.
fn verify_reported_matches_pinned(
    captured: &CapturedFingerprint,
    reported: &str,
) -> Result<(), CallerError> {
    let reported_fp = pinning::parse_fingerprint(reported).map_err(|e| {
        CallerError::Config(format!(
            "target reported an invalid server fingerprint: {e}"
        ))
    })?;
    let seen = captured
        .lock()
        .map_err(|_| CallerError::Config("bootstrap pin slot poisoned".into()))?;
    match *seen {
        Some(transport_fp) if transport_fp == reported_fp => Ok(()),
        Some(transport_fp) => Err(CallerError::Config(format!(
            "target reported server fingerprint {} but presented {} on the wire — refusing to pair",
            pinning::format_fingerprint(&reported_fp),
            pinning::format_fingerprint(&transport_fp),
        ))),
        None => Err(CallerError::Config(
            "no server certificate was captured during the request — refusing to pair".into(),
        )),
    }
}

/// True when `endpoint`'s host is a loopback address or `localhost` —
/// the only case where a cleartext `http://` doorbell is permitted.
fn endpoint_is_loopback(endpoint: &str) -> bool {
    let Some(host) = url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::certs::{ensure_certs, ServerNames};
    use crate::project::ProjectConfig;

    fn setup_certs(dir: &Path) {
        let names = ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        ensure_certs(dir, &names, "access-request-test", false).unwrap();
    }

    /// A v1-signed caller-ID request: the shape a requester that
    /// predates the tier claim sends (no tier field, v1 transcript).
    fn signed_create_request(
        identity: &crate::daemon_identity::DaemonIdentity,
        dialed_origin: &str,
        public_key_pem: &str,
        ts: i64,
    ) -> AccessRequestCreate {
        let nonce = "0123456789abcdef".to_string();
        let transcript = doorbell_transcript(dialed_origin, public_key_pem, &nonce, ts);
        AccessRequestCreate {
            version: 1,
            requester_label: "primary".into(),
            public_key_pem: public_key_pem.to_string(),
            nonce,
            requested_profile: None,
            requester_card_url: None,
            requester_daemon_id: Some(identity.public_key_b64u()),
            requester_daemon_sig: Some(identity.sign_b64u(&transcript)),
            requester_daemon_sig_ts: Some(ts),
            dialed_origin: Some(dialed_origin.to_string()),
            requester_tier: None,
        }
    }

    /// A v2-signed caller-ID request: the current requester shape.
    /// `tier: None` signs the untiered v2 transcript (empty tier line)
    /// and omits the wire field, exactly like a daemon with no tier set.
    fn signed_create_request_v2(
        identity: &crate::daemon_identity::DaemonIdentity,
        dialed_origin: &str,
        public_key_pem: &str,
        ts: i64,
        tier: Option<&str>,
    ) -> AccessRequestCreate {
        let nonce = "0123456789abcdef".to_string();
        let transcript = doorbell_transcript_v2(
            dialed_origin,
            public_key_pem,
            &nonce,
            ts,
            tier.unwrap_or(""),
        );
        AccessRequestCreate {
            version: 1,
            requester_label: "primary".into(),
            public_key_pem: public_key_pem.to_string(),
            nonce,
            requested_profile: None,
            requester_card_url: None,
            requester_daemon_id: Some(identity.public_key_b64u()),
            requester_daemon_sig: Some(identity.sign_b64u(&transcript)),
            requester_daemon_sig_ts: Some(ts),
            dialed_origin: Some(dialed_origin.to_string()),
            requester_tier: tier.map(str::to_string),
        }
    }

    fn test_identity() -> crate::daemon_identity::DaemonIdentity {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        crate::daemon_identity::DaemonIdentity::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    #[test]
    fn doorbell_caller_id_verifies_and_binds_origin_key_and_time() {
        let identity = test_identity();
        let ts = (unix_timestamp() as i64) * 1000;
        let request = signed_create_request(&identity, "https://target:8765", "PEM", ts);

        // Valid: verified id comes back (v1 signed no claim, so no tier).
        let verified = verify_doorbell_caller(&request, "https://target:8765", ts + 1_000)
            .unwrap()
            .expect("v1 caller-id must verify");
        assert_eq!(verified.daemon_id, identity.public_key_b64u());
        assert_eq!(verified.tier, None);

        // Origin mismatch (replay against a different daemon) refuses.
        assert!(verify_doorbell_caller(&request, "https://other:8765", ts).is_err());

        // Stale timestamp refuses.
        assert!(verify_doorbell_caller(
            &request,
            "https://target:8765",
            ts + DOORBELL_MAX_SKEW_MS + 1
        )
        .is_err());

        // Tampered enrollment key (splicing the attacker's key under the
        // victim's caller-ID) refuses.
        let mut tampered = request.clone();
        tampered.public_key_pem = "EVIL".into();
        assert!(verify_doorbell_caller(&tampered, "https://target:8765", ts).is_err());

        // Partial fields refuse (a relay cannot strip the signature and
        // keep the identity claim).
        let mut partial = request.clone();
        partial.requester_daemon_sig = None;
        assert!(verify_doorbell_caller(&partial, "https://target:8765", ts).is_err());

        // Absent fields = legacy caller, admitted unverified.
        let mut absent = request;
        absent.requester_daemon_id = None;
        absent.requester_daemon_sig = None;
        absent.requester_daemon_sig_ts = None;
        absent.dialed_origin = None;
        absent.requester_tier = None;
        assert!(verify_doorbell_caller(&absent, "https://target:8765", ts)
            .unwrap()
            .is_none());
    }

    #[test]
    fn doorbell_v2_tier_claim_binds_to_the_signature_and_vocabulary() {
        let identity = test_identity();
        let ts = (unix_timestamp() as i64) * 1000;
        let origin = "https://target:8765";

        // v2 with a vocabulary tier: verified, the claim comes back.
        let request = signed_create_request_v2(&identity, origin, "PEM", ts, Some("disposable"));
        let verified = verify_doorbell_caller(&request, origin, ts)
            .unwrap()
            .expect("v2 caller-id with tier must verify");
        assert_eq!(verified.daemon_id, identity.public_key_b64u());
        assert_eq!(verified.tier.as_deref(), Some("disposable"));

        // A tampered tier (signed "disposable", claims "integrated")
        // refuses — the claim is bound inside the signature.
        let mut tampered = request.clone();
        tampered.requester_tier = Some("integrated".into());
        assert!(verify_doorbell_caller(&tampered, origin, ts).is_err());

        // Stripping the tier from a v2-with-tier request refuses outright
        // (neither the v1 nor the untiered-v2 transcript matches): a relay
        // cannot demote a signed claim to "no claim".
        let mut stripped = request.clone();
        stripped.requester_tier = None;
        assert!(verify_doorbell_caller(&stripped, origin, ts).is_err());

        // The current no-tier requester shape — untiered v2 transcript,
        // field omitted — verifies with no tier claim.
        let untiered = signed_create_request_v2(&identity, origin, "PEM", ts, None);
        let verified = verify_doorbell_caller(&untiered, origin, ts)
            .unwrap()
            .expect("untiered v2 caller-id must verify");
        assert_eq!(verified.tier, None);

        // A tier field bolted onto a v1-shaped signature refuses: the
        // claim is outside what was signed.
        let mut v1_plus_tier = signed_create_request(&identity, origin, "PEM", ts);
        v1_plus_tier.requester_tier = Some("disposable".into());
        assert!(verify_doorbell_caller(&v1_plus_tier, origin, ts).is_err());

        // An unknown tier string refuses even under a valid signature —
        // vocabulary validation, never passthrough. Same for the
        // empty-string claim (the no-claim shape is an absent field).
        let unknown = signed_create_request_v2(&identity, origin, "PEM", ts, Some("fortress"));
        assert!(verify_doorbell_caller(&unknown, origin, ts).is_err());
        let empty = signed_create_request_v2(&identity, origin, "PEM", ts, Some(""));
        assert!(verify_doorbell_caller(&empty, origin, ts).is_err());

        // A tier claim with no caller-ID at all refuses — an unverifiable
        // claim never demotes itself to merely "unverified".
        let mut bare = signed_create_request(&identity, origin, "PEM", ts);
        bare.requester_daemon_id = None;
        bare.requester_daemon_sig = None;
        bare.requester_daemon_sig_ts = None;
        bare.dialed_origin = None;
        bare.requester_tier = Some("disposable".into());
        assert!(verify_doorbell_caller(&bare, origin, ts).is_err());
    }

    #[test]
    fn create_pending_request_stores_tier_only_from_verified_v2() {
        let certs = tempfile::TempDir::new().unwrap();
        setup_certs(certs.path());
        let identity = test_identity();
        let ts = (unix_timestamp() as i64) * 1000;
        // The received origin the caller-ID must bind derives from the
        // card URL the request arrived on.
        let card_url = "https://target/.well-known/agent-card.json";
        let origin = "https://target";
        let config = PeerAccessRequestConfig::default();
        let source = format!(
            "test-tier-{}-{:?}",
            unix_timestamp(),
            std::thread::current().id()
        );

        // Verified v2 with a tier claim: identity AND tier stored.
        let key = access::certs::generate_client_key_material().unwrap();
        let request = signed_create_request_v2(
            &identity,
            origin,
            &key.public_key_pem,
            ts,
            Some("disposable"),
        );
        let created = create_pending_request(
            certs.path(),
            request,
            card_url.into(),
            Some(source.clone()),
            &config,
        )
        .unwrap();
        let stored = find_request(certs.path(), &created.request_id).unwrap();
        assert_eq!(
            stored.requester_daemon_id.as_deref(),
            Some(identity.public_key_b64u().as_str())
        );
        assert_eq!(stored.requester_tier.as_deref(), Some("disposable"));

        // Verified v1 (predates the tier claim): identity stored, no tier.
        let key = access::certs::generate_client_key_material().unwrap();
        let request = signed_create_request(&identity, origin, &key.public_key_pem, ts);
        let created = create_pending_request(
            certs.path(),
            request,
            card_url.into(),
            Some(source.clone()),
            &config,
        )
        .unwrap();
        let stored = find_request(certs.path(), &created.request_id).unwrap();
        assert_eq!(
            stored.requester_daemon_id.as_deref(),
            Some(identity.public_key_b64u().as_str())
        );
        assert_eq!(stored.requester_tier, None);

        // Legacy caller (no caller-ID fields at all): admitted, nothing
        // identity- or tier-shaped stored.
        let key = access::certs::generate_client_key_material().unwrap();
        let mut request = signed_create_request(&identity, origin, &key.public_key_pem, ts);
        request.requester_daemon_id = None;
        request.requester_daemon_sig = None;
        request.requester_daemon_sig_ts = None;
        request.dialed_origin = None;
        let created = create_pending_request(
            certs.path(),
            request,
            card_url.into(),
            Some(source.clone()),
            &config,
        )
        .unwrap();
        let stored = find_request(certs.path(), &created.request_id).unwrap();
        assert_eq!(stored.requester_daemon_id, None);
        assert_eq!(stored.requester_tier, None);

        // A tampered tier refuses before anything is stored.
        let key = access::certs::generate_client_key_material().unwrap();
        let mut request = signed_create_request_v2(
            &identity,
            origin,
            &key.public_key_pem,
            ts,
            Some("disposable"),
        );
        request.requester_tier = Some("integrated".into());
        let before = read_all_requests(certs.path()).unwrap().len();
        let err = create_pending_request(
            certs.path(),
            request,
            card_url.into(),
            Some(source),
            &config,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("caller identity verification failed"),
            "err: {err}"
        );
        assert_eq!(read_all_requests(certs.path()).unwrap().len(), before);
    }

    #[test]
    fn doorbell_origin_comparison_normalizes_defaults_and_case() {
        assert!(origins_match(
            "HTTPS://Target.example",
            "https://target.example:443"
        ));
        assert!(origins_match("http://t:80", "http://t"));
        assert!(!origins_match("https://t:8765", "https://t:8766"));
        assert!(!origins_match("https://t", "http://t"));
        assert!(!origins_match("not a url", "https://t"));
    }

    #[test]
    fn disabled_public_access_request_config_rejects_before_creating() {
        let certs = tempfile::TempDir::new().unwrap();
        let request = AccessRequestCreate {
            version: 1,
            requester_label: "primary".into(),
            public_key_pem: "not checked while disabled".into(),
            nonce: "0123456789abcdef".into(),
            requested_profile: None,
            requester_card_url: None,
            requester_daemon_id: None,
            requester_daemon_sig: None,
            requester_daemon_sig_ts: None,
            dialed_origin: None,
            requester_tier: None,
        };
        let config = PeerAccessRequestConfig {
            enabled: false,
            ..Default::default()
        };

        let err = create_pending_request(
            certs.path(),
            request,
            "https://target/.well-known/agent-card.json".into(),
            Some("127.0.0.1".into()),
            &config,
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("peer access requests are disabled"));
        assert!(read_all_requests(certs.path()).unwrap().is_empty());
    }

    #[test]
    fn public_access_request_body_limit_clamps_to_one_byte() {
        let config = PeerAccessRequestConfig {
            body_limit_bytes: 0,
            ..Default::default()
        };

        assert_eq!(effective_body_limit_bytes(&config), 1);
    }

    #[test]
    fn minted_request_ids_are_flag_safe() {
        let certs = tempfile::TempDir::new().unwrap();
        setup_certs(certs.path());
        let key = access::certs::generate_client_key_material().unwrap();
        let request = AccessRequestCreate {
            version: 1,
            requester_label: "primary".into(),
            public_key_pem: key.public_key_pem,
            nonce: "0123456789abcdef".into(),
            requested_profile: None,
            requester_card_url: None,
            requester_daemon_id: None,
            requester_daemon_sig: None,
            requester_daemon_sig_ts: None,
            dialed_origin: None,
            requester_tier: None,
        };
        let created = create_pending_request(
            certs.path(),
            request,
            "https://target/.well-known/agent-card.json".into(),
            Some("127.0.0.1".into()),
            &PeerAccessRequestConfig::default(),
        )
        .unwrap();
        // The id rides argv in `peer complete <id>`: a leading '-' reads
        // as a flag. The 'r' prefix pins the invariant structurally.
        assert!(
            created.request_id.starts_with('r'),
            "id not flag-safe: {}",
            created.request_id
        );
        assert!(validate_request_id(&created.request_id).is_ok());
    }

    #[test]
    fn approve_request_signs_requester_public_key_without_private_key() {
        let certs = tempfile::TempDir::new().unwrap();
        setup_certs(certs.path());
        let key = access::certs::generate_client_key_material().unwrap();
        let request = AccessRequestCreate {
            version: 1,
            requester_label: "primary".into(),
            public_key_pem: key.public_key_pem,
            nonce: "0123456789abcdef".into(),
            requested_profile: Some("peer-daemon".into()),
            requester_card_url: None,
            requester_daemon_id: None,
            requester_daemon_sig: None,
            requester_daemon_sig_ts: None,
            dialed_origin: None,
            requester_tier: None,
        };

        let created = create_pending_request(
            certs.path(),
            request,
            "https://target/.well-known/agent-card.json".into(),
            Some("127.0.0.1".into()),
            &PeerAccessRequestConfig::default(),
        )
        .unwrap();
        let approved = approve_request(certs.path(), &created.code, None).unwrap();

        assert_eq!(approved.status, AccessRequestStatus::Approved);
        let cert = approved.client_cert_pem.unwrap();
        assert!(cert.contains("BEGIN CERTIFICATE"));
        assert!(!cert.contains("PRIVATE KEY"));
        let status = request_status(certs.path(), &created.request_id).unwrap();
        assert_eq!(status.status, AccessRequestStatus::Approved);
        assert!(status
            .result
            .unwrap()
            .client_cert_pem
            .contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn approve_request_without_profile_uses_peer_operator_default() {
        let certs = tempfile::TempDir::new().unwrap();
        setup_certs(certs.path());
        let key = access::certs::generate_client_key_material().unwrap();
        let request = AccessRequestCreate {
            version: 1,
            requester_label: "primary".into(),
            public_key_pem: key.public_key_pem,
            nonce: "0123456789abcdef".into(),
            requested_profile: None,
            requester_card_url: None,
            requester_daemon_id: None,
            requester_daemon_sig: None,
            requester_daemon_sig_ts: None,
            dialed_origin: None,
            requester_tier: None,
        };

        let created = create_pending_request(
            certs.path(),
            request,
            "https://target/.well-known/agent-card.json".into(),
            Some("127.0.0.1".into()),
            &PeerAccessRequestConfig::default(),
        )
        .unwrap();
        let approved = approve_request(certs.path(), &created.code, None).unwrap();

        assert_eq!(
            approved.approved_profile.as_deref(),
            Some(crate::peer::access_policy::DEFAULT_PROFILE)
        );
    }

    #[test]
    fn wire_requested_profiles_stay_lenient_and_degrade_fail_closed() {
        // The CLI validates --profile loudly, but the doorbell wire path
        // must keep accepting profile names this build does not know
        // (older/newer requesters): the string is stored as-is and stays
        // fail-closed — presence-only — at authorization time.
        let certs = tempfile::TempDir::new().unwrap();
        setup_certs(certs.path());
        let key = access::certs::generate_client_key_material().unwrap();
        let request = AccessRequestCreate {
            version: 1,
            requester_label: "primary".into(),
            public_key_pem: key.public_key_pem,
            nonce: "0123456789abcdef".into(),
            requested_profile: Some("future-profile".into()),
            requester_card_url: None,
            requester_daemon_id: None,
            requester_daemon_sig: None,
            requester_daemon_sig_ts: None,
            dialed_origin: None,
            requester_tier: None,
        };

        let created = create_pending_request(
            certs.path(),
            request,
            "https://target/.well-known/agent-card.json".into(),
            Some("127.0.0.1".into()),
            &PeerAccessRequestConfig::default(),
        )
        .unwrap();
        let approved = approve_request(certs.path(), &created.code, None).unwrap();

        assert_eq!(approved.approved_profile.as_deref(), Some("future-profile"));
        assert!(!crate::peer::access_policy::profile_allows_operation(
            "future-profile",
            crate::peer::access_policy::PeerOperation::StatsRead,
        ));
    }

    #[test]
    fn install_approved_identity_writes_peer_config_and_key() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("intendant.toml"), "").unwrap();
        let certs = tempfile::TempDir::new().unwrap();
        setup_certs(certs.path());
        let mut project = Project {
            root: root.path().to_path_buf(),
            config: ProjectConfig::default(),
        };
        let key = access::certs::generate_client_key_material().unwrap();
        let cert = access::certs::issue_client_certificate_for_public_key(
            certs.path(),
            "primary",
            &key.public_key_pem,
        )
        .unwrap();
        let result = ApprovedAccessResult {
            card_url: "https://target/.well-known/agent-card.json".into(),
            label: Some("target".into()),
            server_cert_fingerprint: access::certs::read_server_cert_fingerprint(certs.path())
                .unwrap(),
            client_cert_pem: cert,
            approved_profile: "peer-daemon".into(),
            approved_at_unix: unix_timestamp(),
        };

        let outcome =
            install_approved_identity(&mut project, certs.path(), &result, &key.key_pem, None)
                .unwrap();

        assert_eq!(outcome.card_url, result.card_url);
        assert!(outcome.client_cert_path.exists());
        assert!(outcome.client_key_path.exists());
        assert_eq!(project.config.peers.len(), 1);
        assert_eq!(project.config.peers[0].card_url, result.card_url);
    }

    #[test]
    fn create_rate_limit_rejects_excess_per_source() {
        let source = format!(
            "test-rate-{}-{:?}",
            unix_timestamp(),
            std::thread::current().id()
        );
        let config = PeerAccessRequestConfig {
            max_creates_per_window: 1024,
            max_creates_per_source_per_window: 2,
            ..Default::default()
        };
        for _ in 0..config.max_creates_per_source_per_window {
            enforce_create_rate_limits(Some(&source), &config).unwrap();
        }

        let err = enforce_create_rate_limits(Some(&source), &config).unwrap_err();
        assert!(
            err.to_string()
                .contains("peer access request rate limit exceeded"),
            "err: {err}"
        );
    }

    #[test]
    fn endpoint_is_loopback_matches_localhost_and_loopback_ips() {
        assert!(endpoint_is_loopback("http://127.0.0.1:8765"));
        assert!(endpoint_is_loopback("http://localhost:8765/x"));
        assert!(endpoint_is_loopback("http://[::1]:8765"));
        assert!(!endpoint_is_loopback("http://10.0.0.4:8765"));
        assert!(!endpoint_is_loopback("http://peer.example:8765"));
    }

    #[test]
    fn target_request_endpoint_rejects_non_loopback_http() {
        let err = target_request_endpoint("http://peer.example:8765").unwrap_err();
        assert!(err.to_string().contains("non-loopback http"), "err: {err}");
        // Loopback http is allowed (local testing); https always is.
        assert!(target_request_endpoint("http://127.0.0.1:8765").is_ok());
        assert!(target_request_endpoint("https://peer.example:8765").is_ok());
    }

    #[test]
    fn reported_fingerprint_must_match_pinned_transport() {
        let transport_fp = pinning::fingerprint_of_der(b"the-cert-bytes");
        let captured: CapturedFingerprint = StdMutex::new(Some(transport_fp));
        // Matching reported fingerprint passes.
        assert!(verify_reported_matches_pinned(
            &captured,
            &pinning::format_fingerprint(&transport_fp)
        )
        .is_ok());
        // A different reported fingerprint fails closed.
        let other = pinning::format_fingerprint(&pinning::fingerprint_of_der(b"other-bytes"));
        assert!(verify_reported_matches_pinned(&captured, &other).is_err());
        // No captured cert (handshake never ran) fails closed.
        let empty: CapturedFingerprint = StdMutex::new(None);
        assert!(verify_reported_matches_pinned(
            &empty,
            &pinning::format_fingerprint(&transport_fp)
        )
        .is_err());
    }
}
