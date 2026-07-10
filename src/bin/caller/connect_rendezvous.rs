//! Outbound Intendant Connect rendezvous client for dashboard-control signaling.
//!
//! Connect is the hosted transport and identity-metadata relay for
//! dashboard-control signaling. Authorization stays daemon-local: before
//! answering an offer, this client verifies any browser client key / account
//! metadata against local IAM and creates a dashboard-control session only when
//! that local grant exists. Direct mTLS/local-root dashboard access remains the
//! bootstrap path for managing those grants.

use crate::daemon_identity::DaemonIdentity;
use crate::dashboard_control::DashboardControlRegistry;
use crate::project::ConnectConfig;
use base64::Engine as _;
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const REGISTER_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Claim-proof payload protocols, mirrored by the rendezvous service.
/// v1 is account-blind. v2 additionally binds the claiming account
/// (user id + handle) into the payload this daemon signs, so "claimed by
/// @handle" is provable from the daemon's own signature instead of being
/// the service's word. The daemon signs v2 whenever the challenge names
/// an account and falls back to v1 for older services.
const CLAIM_PROTOCOL_V1: &str = "intendant-connect-claim-v1";
const CLAIM_PROTOCOL_V2: &str = "intendant-connect-claim-v2";
/// Daemon-signed release of a claim binding, mirrored by the service.
const UNCLAIM_PROTOCOL: &str = "intendant-connect-unclaim-v1";
/// First-owner bootstrap tag (mirrored by the /connect claim page): an
/// HMAC-SHA256 keyed by the daemon-minted phrase, binding the claiming
/// browser's identity key and account. Possession of the phrase is
/// box-grade proof (it exists only in this daemon's log/Access card), so
/// a valid tag is what authorizes minting the FIRST owner grant.
const BOOTSTRAP_TAG_PROTOCOL: &str = "intendant-connect-bootstrap-v1";

/// Register failures split by what retrying can fix. `Rejected` is a 4xx
/// verdict from the service — a missing/invalid daemon token or a gated
/// rendezvous — configuration, not weather: hammering once a second
/// changes nothing (observed live against a token-gated service), so
/// those retry on this slow clock instead of `retry_delay_ms`.
const REGISTER_REJECTED_RETRY: Duration = Duration::from_secs(60);

enum RegisterError {
    Rejected(String),
    Transient(String),
}

#[derive(Debug, Serialize)]
struct RegisterRequest {
    protocol: &'static str,
    daemon_id: String,
    daemon_public_key: String,
    /// First-owner bootstrap: SHA-256 (base64url) of this daemon's own
    /// normalized claim phrase, sent only while the box is fresh (empty
    /// IAM). The rendezvous stores the hash for claim routing and never
    /// sees the plaintext.
    #[serde(skip_serializing_if = "Option::is_none")]
    bootstrap_code_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    #[serde(default)]
    claimed: bool,
    /// Current owner (service-asserted; the daemon's own signed claim
    /// record is the stronger provenance when the two agree).
    #[serde(default)]
    claimed_by_user_id: Option<String>,
    #[serde(default)]
    claimed_by_handle: Option<String>,
    #[serde(default)]
    claim_code: Option<String>,
    /// The service accepted this daemon's bootstrap hash — the phrase to
    /// show is the local one, not a service-minted code.
    #[serde(default)]
    claim_code_daemon_minted: bool,
    #[serde(default)]
    claim_code_expires_unix_ms: Option<u64>,
    #[serde(default)]
    claim_url: Option<String>,
    /// This daemon's public address as the rendezvous observed it —
    /// what a cloud box behind 1:1 NAT advertises as its ICE-TCP
    /// candidate on Connect offers (reachability metadata, not
    /// authority).
    #[serde(default)]
    observed_ip: Option<String>,
    /// Fleet DNS hint: this daemon's derived name under the rendezvous's
    /// delegated zone, when it serves one (fleet certificates —
    /// `fleet_cert.rs`).
    #[serde(default)]
    fleet_dns: Option<FleetDnsHint>,
}

#[derive(Debug, Deserialize)]
struct FleetDnsHint {
    #[serde(default)]
    zone: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct RendezvousEvent {
    id: String,
    kind: String,
    #[serde(default)]
    sdp: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    candidate: Option<serde_json::Value>,
    #[serde(default)]
    session_grant: Option<String>,
    #[serde(default)]
    client_nonce: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    account_name: Option<String>,
    #[serde(default)]
    client_key: Option<String>,
    #[serde(default)]
    client_key_sig: Option<String>,
    #[serde(default)]
    client_key_ts: Option<i64>,
    /// v2 offer-signature fields (see `access::client_key`): the payload
    /// version plus the browser's own account claim, relayed verbatim.
    #[serde(default)]
    client_key_proto: Option<String>,
    #[serde(default)]
    client_key_account_user_id: Option<String>,
    #[serde(default)]
    client_key_account_name: Option<String>,
    #[serde(default)]
    org_grant: Option<serde_json::Value>,
    #[serde(default)]
    claim_id: Option<String>,
    #[serde(default)]
    challenge: Option<String>,
    /// First-owner bootstrap arm fields, relayed blind by the service —
    /// this daemon recomputes the phrase-derived tag itself.
    #[serde(default)]
    bootstrap_client_key: Option<String>,
    #[serde(default)]
    bootstrap_client_key_tag: Option<String>,
}

#[derive(Debug, Serialize)]
struct AnswerRequest {
    protocol: &'static str,
    daemon_id: String,
    request_id: String,
    session_id: String,
    sdp: String,
    binding: crate::dashboard_control::DashboardControlBinding,
}

#[derive(Debug, Serialize)]
struct ErrorRequest {
    daemon_id: String,
    request_id: String,
    /// When the error concerns a claim, its id — the service then rejects
    /// the pending claim so the claiming page shows the real reason
    /// instead of timing out. Older services ignore the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    claim_id: Option<String>,
    error: String,
}

#[derive(Debug, Serialize)]
struct ClaimProofRequest {
    protocol: &'static str,
    daemon_id: String,
    request_id: String,
    claim_id: String,
    challenge: String,
    signature: String,
}

/// The daemon's durable, self-signed record of the claim it acknowledged:
/// written the moment a v2 claim proof is accepted, cleared by a
/// daemon-initiated unclaim. Never an authority input — display
/// provenance only ("claimed by @handle", co-signed by this daemon's own
/// key) and the mismatch detector for a service that later asserts an
/// owner this daemon never acknowledged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SignedClaimRecord {
    pub claim_id: String,
    pub daemon_id: String,
    /// Rendezvous origin the acknowledgment was posted to.
    pub rendezvous: String,
    pub account_user_id: String,
    #[serde(default)]
    pub account_name: String,
    pub protocol: String,
    pub signed_at_unix_ms: i64,
}

fn signed_claim_record_path() -> PathBuf {
    crate::daemon_identity::default_identity_dir().join("connect-claim.json")
}

pub(crate) fn load_signed_claim_record() -> Option<SignedClaimRecord> {
    let path = signed_claim_record_path();
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(record) => Some(record),
        Err(e) => {
            eprintln!(
                "[connect] ignoring unreadable signed claim record {}: {e}",
                path.display()
            );
            None
        }
    }
}

fn store_signed_claim_record(record: &SignedClaimRecord) {
    let path = signed_claim_record_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("[connect] create {}: {e}", parent.display());
            return;
        }
    }
    match serde_json::to_vec_pretty(record) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, bytes) {
                eprintln!("[connect] write {}: {e}", path.display());
            }
        }
        Err(e) => eprintln!("[connect] serialize signed claim record: {e}"),
    }
}

fn clear_signed_claim_record() {
    let path = signed_claim_record_path();
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("[connect] remove {}: {e}", path.display());
        }
    }
}

/// A box is first-owner-bootstrap eligible while its local IAM holds
/// nothing at all: no principals, no grants. The auto-minted mTLS client
/// bundle does NOT count as ownership — it exists on every daemon from
/// first boot, and holding its P12 is box-grade access, exactly what the
/// bootstrap phrase proves too. The window closes the moment any
/// principal or grant is written, and unreadable IAM state fails closed.
fn first_owner_bootstrap_eligible() -> bool {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    match crate::access::iam::load_state(&cert_dir) {
        Ok(state) => state.principals.is_empty() && state.grants.is_empty(),
        Err(_) => false,
    }
}

fn bootstrap_phrase_registry() -> &'static Mutex<Option<String>> {
    static REGISTRY: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

/// The daemon-minted first-owner phrase for this process: minted on first
/// use while the box is eligible, dropped forever once an owner exists.
/// Regenerated per boot — the phrase is a bootstrap secret, not durable
/// state.
fn current_bootstrap_phrase() -> Option<String> {
    let mut slot = bootstrap_phrase_registry()
        .lock()
        .expect("bootstrap phrase poisoned");
    if !first_owner_bootstrap_eligible() {
        if slot.take().is_some() {
            eprintln!("[connect] first-owner bootstrap closed (this daemon now has an owner)");
        }
        return None;
    }
    if slot.is_none() {
        let mut entropy = [0u8; 16];
        if let Err(e) =
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut entropy)
        {
            eprintln!("[connect] bootstrap phrase entropy unavailable: {e:?}");
            return None;
        }
        match bip39::Mnemonic::from_entropy(&entropy) {
            Ok(mnemonic) => *slot = Some(mnemonic.to_string().replace(' ', "-")),
            Err(e) => {
                eprintln!("[connect] bootstrap phrase generation failed: {e}");
                return None;
            }
        }
    }
    slot.clone()
}

fn clear_bootstrap_phrase() {
    bootstrap_phrase_registry()
        .lock()
        .expect("bootstrap phrase poisoned")
        .take();
}

/// Mirrors `normalize_claim_code` in `intendant-connect`: lowercase
/// alphanumeric runs joined by `-`.
fn normalize_claim_code(input: &str) -> String {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts.join("-")
}

/// Mirrors `claim_code_hash` in `intendant-connect` (and the /connect
/// page JS): SHA-256 of the normalized phrase, base64url unpadded.
fn claim_code_hash(code: &str) -> String {
    crate::daemon_identity::b64u(
        ring::digest::digest(
            &ring::digest::SHA256,
            normalize_claim_code(code).as_bytes(),
        )
        .as_ref(),
    )
}

/// The exact string the /connect page HMACs with the phrase-derived key.
/// Binds the claiming browser's identity key and account, so the relay
/// (which never sees the phrase) cannot substitute a key of its own.
fn bootstrap_tag_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    client_key_b64u: &str,
    account_user_id: &str,
    account_name: &str,
) -> String {
    format!(
        "{BOOTSTRAP_TAG_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{client_key_b64u}\n{account_user_id}\n{account_name}\n"
    )
}

/// Verify a bootstrap arm and enroll the claiming browser as this
/// daemon's FIRST owner (`role:root`). Authority basis: the tag proves
/// possession of the daemon-minted phrase, which only box-grade access
/// (this daemon's log, or its Access card behind AccessManage) reveals —
/// the same proof SSH access would be. Recorded with the sentinel origin
/// `connect-bootstrap`, which is not a hosted origin, so no role ceiling
/// demotes the owner it mints. Fails closed on any mismatch and on any
/// non-empty IAM.
fn bootstrap_enroll_first_owner(
    client_key_b64u: &str,
    tag_b64u: &str,
    account_user_id: &str,
    account_name: &str,
    daemon_id: &str,
    daemon_public_key: &str,
) -> Result<String, String> {
    let Some(phrase) = current_bootstrap_phrase() else {
        return Err(
            "this daemon is not offering first-owner bootstrap (it already has an owner)"
                .to_string(),
        );
    };
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let fingerprint = bootstrap_enroll_first_owner_at(
        &cert_dir,
        &phrase,
        client_key_b64u,
        tag_b64u,
        account_user_id,
        account_name,
        daemon_id,
        daemon_public_key,
    )?;
    clear_bootstrap_phrase();
    eprintln!(
        "[connect] first-owner bootstrap: enrolled client key {fingerprint} as role:root{}",
        if account_name.is_empty() {
            String::new()
        } else {
            format!(" (@{account_name})")
        }
    );
    Ok(fingerprint)
}

/// Testable core: explicit state directory and phrase.
#[allow(clippy::too_many_arguments)]
fn bootstrap_enroll_first_owner_at(
    cert_dir: &std::path::Path,
    phrase: &str,
    client_key_b64u: &str,
    tag_b64u: &str,
    account_user_id: &str,
    account_name: &str,
    daemon_id: &str,
    daemon_public_key: &str,
) -> Result<String, String> {
    let engine = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let raw_key = engine
        .decode(client_key_b64u)
        .map_err(|_| "bootstrap client key is not valid base64url".to_string())?;
    if raw_key.len() != 65 || raw_key[0] != 0x04 {
        return Err("bootstrap client key must be a 65-byte uncompressed P-256 point".to_string());
    }
    let tag = engine
        .decode(tag_b64u)
        .map_err(|_| "bootstrap tag is not valid base64url".to_string())?;
    let hmac_key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        ring::digest::digest(
            &ring::digest::SHA256,
            normalize_claim_code(phrase).as_bytes(),
        )
        .as_ref(),
    );
    let payload = bootstrap_tag_payload(
        daemon_id,
        daemon_public_key,
        client_key_b64u,
        account_user_id,
        account_name,
    );
    ring::hmac::verify(&hmac_key, payload.as_bytes(), &tag).map_err(|_| {
        "bootstrap tag verification failed — the phrase entered does not match this daemon's"
            .to_string()
    })?;

    let mut state = crate::access::iam::load_state(cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    // Re-check under the state we are about to write: eligibility may
    // have closed between the phrase snapshot and now.
    if !(state.principals.is_empty() && state.grants.is_empty()) {
        return Err("this daemon already has an owner; bootstrap enrollment is closed".to_string());
    }
    let fingerprint = crate::access::client_key::client_key_fingerprint(&raw_key);
    let request = crate::access::iam::UserClientGrantUpsertRequest {
        kind: "client_key".to_string(),
        label: Some(if account_name.is_empty() {
            "First owner (bootstrap)".to_string()
        } else {
            format!("@{account_name} (first owner)")
        }),
        client_key_fingerprint: Some(fingerprint.clone()),
        client_key: Some(client_key_b64u.to_string()),
        client_key_origin: Some("connect-bootstrap".to_string()),
        role_id: Some("role:root".to_string()),
        user_id: (!account_user_id.is_empty()).then(|| account_user_id.to_string()),
        account_name: (!account_name.is_empty()).then(|| account_name.to_string()),
        reason: Some(
            "first-owner bootstrap: phrase-holder enrolled at claim time".to_string(),
        ),
        ..Default::default()
    };
    let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
        "connect-bootstrap",
        "connect-rendezvous",
    );
    crate::access::iam::upsert_user_client_grant(&mut state, request, &actor)
        .map_err(|e| e.to_string())?;
    crate::access::iam::save_state(cert_dir, &state)
        .map_err(|e| format!("save local IAM state: {e}"))?;
    Ok(fingerprint)
}

/// How the daemon's local signed claim record relates to the owner the
/// service currently asserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ClaimBinding {
    /// The local signed acknowledgment matches the service-asserted owner.
    DaemonSigned,
    /// No local co-signed record to check against — the binding rests on
    /// the service's assertion (a v1-era claim, or one acknowledged
    /// before this daemon kept records).
    ServiceAsserted,
    /// The service asserts an owner this daemon never co-signed (or a
    /// different one than it did) — a re-bind worth the owner's eyes.
    Mismatch,
}

/// Snapshot of the Connect client for the owner-gated Access card.
/// Single writer (the client loop plus the start/stop manager); the
/// gateway only snapshots. Deliberately NOT part of the control-plane
/// state broadcast: the claim code is owner-material and must never ride
/// general frontend state snapshots.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ConnectStatus {
    pub configured: bool,
    pub env_forced: bool,
    pub rendezvous_url: Option<String>,
    pub daemon_id: Option<String>,
    pub running: bool,
    pub registered: bool,
    pub last_register_unix_ms: Option<i64>,
    pub last_error: Option<String>,
    pub claimed: Option<bool>,
    pub claimed_by_user_id: Option<String>,
    pub claimed_by_handle: Option<String>,
    pub claim_binding: Option<ClaimBinding>,
    pub signed_claim: Option<SignedClaimRecord>,
    pub claim_code: Option<String>,
    pub claim_url: Option<String>,
    pub claim_code_expires_unix_ms: Option<u64>,
    /// The current claim phrase is this daemon's own first-owner
    /// bootstrap phrase: claiming with it also enrolls the claiming
    /// browser as role:root.
    pub bootstrap: bool,
    /// Public address the rendezvous observes for this daemon.
    pub observed_ip: Option<String>,
}

fn status_registry() -> &'static Mutex<ConnectStatus> {
    static REGISTRY: OnceLock<Mutex<ConnectStatus>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(ConnectStatus::default()))
}

pub(crate) fn status_snapshot() -> ConnectStatus {
    status_registry()
        .lock()
        .expect("connect status poisoned")
        .clone()
}

fn with_status(update: impl FnOnce(&mut ConnectStatus)) {
    let mut status = status_registry().lock().expect("connect status poisoned");
    update(&mut status);
}

/// Wakes the client loop for an immediate re-register (fresh claim code /
/// claim state) instead of waiting out the refresh interval — used after
/// an unclaim so the Access card converges fast. `notify_one` stores a
/// permit, so a nudge that lands while the loop is busy is not lost.
fn register_nudge() -> &'static Notify {
    static NUDGE: OnceLock<Notify> = OnceLock::new();
    NUDGE.get_or_init(Notify::new)
}

/// The spawned client task plus the dashboard-control registry it was
/// started with, so the gateway toggle can stop/restart at runtime.
struct ClientState {
    handle: Option<JoinHandle<()>>,
    dashboard_control: Option<Arc<DashboardControlRegistry>>,
    /// The web gateway's TCP port — combined with the rendezvous-observed
    /// IP to advertise an ICE-TCP candidate on Connect offers.
    gateway_tcp_port: Option<u16>,
}

fn client_state() -> &'static Mutex<ClientState> {
    static STATE: OnceLock<Mutex<ClientState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(ClientState {
            handle: None,
            dashboard_control: None,
            gateway_tcp_port: None,
        })
    })
}

pub fn spawn_connect_rendezvous_client(
    config: ConnectConfig,
    dashboard_control: Arc<DashboardControlRegistry>,
    gateway_tcp_port: Option<u16>,
) {
    {
        let mut state = client_state().lock().expect("connect client state poisoned");
        state.dashboard_control = Some(dashboard_control.clone());
        state.gateway_tcp_port = gateway_tcp_port;
    }
    start_client(config, dashboard_control, gateway_tcp_port);
}

/// Stop the running client task, if any. All of the task's awaits are
/// cancellation-safe HTTP calls, so an abort leaves no local state
/// half-written.
pub(crate) fn stop_client() {
    let handle = client_state()
        .lock()
        .expect("connect client state poisoned")
        .handle
        .take();
    if let Some(handle) = handle {
        handle.abort();
    }
    with_status(|status| {
        status.running = false;
        status.registered = false;
    });
}

/// Apply a new effective config at runtime: stop the running client,
/// start a fresh one when enabled. Returns whether a client is running
/// afterwards. Fails only if enablement is requested before boot wiring
/// provided the dashboard-control registry (the gateway calling this
/// implies it already exists).
pub(crate) fn apply_config(config: ConnectConfig) -> Result<bool, String> {
    stop_client();
    if !config.enabled {
        with_status(|status| {
            status.configured = false;
            status.env_forced = ConnectConfig::env_forced();
            status.claim_code = None;
            status.claim_url = None;
            status.claim_code_expires_unix_ms = None;
            status.last_error = None;
        });
        return Ok(false);
    }
    let (dashboard_control, gateway_tcp_port) = {
        let state = client_state().lock().expect("connect client state poisoned");
        (state.dashboard_control.clone(), state.gateway_tcp_port)
    };
    let dashboard_control = dashboard_control
        .ok_or_else(|| "connect client cannot start before the web gateway".to_string())?;
    start_client(config, dashboard_control, gateway_tcp_port);
    Ok(client_state()
        .lock()
        .expect("connect client state poisoned")
        .handle
        .is_some())
}

/// Shared by boot (`spawn_connect_rendezvous_client`) and the runtime
/// toggle (`apply_config`).
fn start_client(
    config: ConnectConfig,
    dashboard_control: Arc<DashboardControlRegistry>,
    gateway_tcp_port: Option<u16>,
) {
    with_status(|status| {
        status.configured = config.enabled;
        status.env_forced = ConnectConfig::env_forced();
        status.rendezvous_url = config.rendezvous_url.clone();
        status.daemon_id = config.daemon_id.clone();
        status.signed_claim = load_signed_claim_record();
    });
    if !config.enabled {
        // One line per gateway spawn: a daemon that silently never
        // registers is indistinguishable from a broken rendezvous — say
        // why. (This was the client's only silent path, found the hard
        // way on the first fresh-VPS E2E.) The env-visibility clause
        // distinguishes "not configured" from "configured but lost
        // between the environment and this call".
        eprintln!(
            "[connect] rendezvous client disabled (enable via INTENDANT_CONNECT_RENDEZVOUS_URL \
             or [connect] in intendant.toml; that env var {} visible to this process)",
            if ConnectConfig::env_forced() {
                "IS"
            } else {
                "is not"
            }
        );
        return;
    }
    let Some(base_url) = config
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        eprintln!("[connect] enabled but no rendezvous_url is configured");
        with_status(|status| {
            status.last_error = Some("enabled but no rendezvous_url is configured".to_string());
        });
        return;
    };
    let base_url = match Url::parse(base_url) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("[connect] invalid rendezvous_url {base_url:?}: {e}");
            with_status(|status| {
                status.last_error = Some(format!("invalid rendezvous_url {base_url:?}: {e}"));
            });
            return;
        }
    };
    let handle = tokio::spawn(async move {
        run_connect_rendezvous_client(config, base_url, dashboard_control, gateway_tcp_port).await;
        // Natural exit (identity or HTTP-client construction failure) —
        // an abort via `stop_client` never reaches this line, but that
        // path flips the flag itself.
        with_status(|status| {
            status.running = false;
        });
    });
    client_state()
        .lock()
        .expect("connect client state poisoned")
        .handle = Some(handle);
}

async fn run_connect_rendezvous_client(
    config: ConnectConfig,
    base_url: Url,
    dashboard_control: Arc<DashboardControlRegistry>,
    gateway_tcp_port: Option<u16>,
) {
    let identity = match DaemonIdentity::load_or_create_default() {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!("[connect] daemon identity unavailable: {e}");
            with_status(|status| {
                status.last_error = Some(format!("daemon identity unavailable: {e}"));
            });
            return;
        }
    };
    let daemon_public_key = identity.public_key_b64u();
    let daemon_id = config
        .daemon_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| daemon_public_key.clone());
    let client = match Client::builder()
        .timeout(Duration::from_millis(
            config.poll_timeout_ms.saturating_add(10_000).max(10_000),
        ))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            eprintln!("[connect] failed to build HTTP client: {e}");
            with_status(|status| {
                status.last_error = Some(format!("failed to build HTTP client: {e}"));
            });
            return;
        }
    };
    let retry_delay = Duration::from_millis(config.retry_delay_ms.max(100));
    eprintln!("[connect] rendezvous client enabled for daemon {daemon_id}");
    with_status(|status| {
        status.running = true;
        status.daemon_id = Some(daemon_id.clone());
    });

    loop {
        match register(&client, &base_url, &config, &daemon_id, &daemon_public_key).await {
            Ok(response) => note_register_response(&response, &base_url),
            Err(RegisterError::Rejected(e)) => {
                eprintln!(
                    "[connect] register rejected: {e} — the rendezvous refused this daemon \
                     (check INTENDANT_CONNECT_TOKEN, or whether the service gates \
                     registration); retrying every {}s",
                    REGISTER_REJECTED_RETRY.as_secs()
                );
                note_register_error(&e);
                tokio::time::sleep(REGISTER_REJECTED_RETRY).await;
                continue;
            }
            Err(RegisterError::Transient(e)) => {
                eprintln!("[connect] register failed: {e}");
                note_register_error(&e);
                tokio::time::sleep(retry_delay).await;
                continue;
            }
        }

        let mut last_register = Instant::now();
        loop {
            // A nudge (post-unclaim) may cancel an in-flight `/next` poll;
            // a popped-but-undelivered event is lost. Acceptable: nudges
            // are rare and owner-initiated, and events addressed to a
            // just-released binding are moot.
            let force_register = tokio::select! {
                result = poll_next(&client, &base_url, &config, &daemon_id) => {
                    match result {
                        Ok(Some(event)) => {
                            handle_event(
                                &client,
                                &base_url,
                                &config,
                                &daemon_id,
                                &identity,
                                &dashboard_control,
                                gateway_tcp_port,
                                event,
                            )
                            .await;
                            continue;
                        }
                        Ok(None) => {
                            report_dry_credentials(&client, &base_url, &config, &daemon_id).await;
                            false
                        }
                        Err(e) => {
                            eprintln!("[connect] poll failed: {e}");
                            with_status(|status| {
                                status.last_error = Some(format!("poll failed: {e}"));
                            });
                            tokio::time::sleep(retry_delay).await;
                            break;
                        }
                    }
                }
                _ = register_nudge().notified() => true,
            };
            if force_register || last_register.elapsed() >= REGISTER_REFRESH_INTERVAL {
                match register(&client, &base_url, &config, &daemon_id, &daemon_public_key).await {
                    Ok(response) => {
                        note_register_response(&response, &base_url);
                        last_register = Instant::now();
                    }
                    Err(RegisterError::Rejected(e)) => {
                        eprintln!(
                            "[connect] refresh register rejected: {e} — retrying \
                             every {}s",
                            REGISTER_REJECTED_RETRY.as_secs()
                        );
                        note_register_error(&e);
                        tokio::time::sleep(REGISTER_REJECTED_RETRY).await;
                        break;
                    }
                    Err(RegisterError::Transient(e)) => {
                        eprintln!("[connect] refresh register failed: {e}");
                        note_register_error(&e);
                        tokio::time::sleep(retry_delay).await;
                        break;
                    }
                }
            }
        }
    }
}

fn note_register_error(error: &str) {
    with_status(|status| {
        status.registered = false;
        status.last_error = Some(error.to_string());
    });
}

/// Fold a register response into the status snapshot, cross-checking the
/// service-asserted owner against the daemon's own signed claim record,
/// and print the claim line when the code actually changed (the old
/// every-60s repeat was log noise; the current code is always visible in
/// the Access card).
fn note_register_response(response: &RegisterResponse, base_url: &Url) {
    // Daemon-minted bootstrap: the service echoes no code (it only holds
    // the hash) — the phrase to show, in the log and the Access card, is
    // the local one. Build the claim URL against the rendezvous origin
    // exactly like service-minted claim URLs.
    let bootstrap = if !response.claimed && response.claim_code_daemon_minted {
        current_bootstrap_phrase().map(|phrase| {
            let url = format!("{}/connect?claim_code={phrase}", base_origin(base_url));
            (phrase, url)
        })
    } else {
        None
    };
    let now = crate::access::client_key::now_unix_ms();
    let mut print_claim: Option<String> = None;
    // Fleet DNS: hand the name to the certificate machinery (it installs
    // any stored certificate the first time the name is learned).
    crate::fleet_cert::note_fleet_dns(
        response
            .fleet_dns
            .as_ref()
            .map(|hint| hint.zone.clone())
            .filter(|zone| !zone.is_empty()),
        response
            .fleet_dns
            .as_ref()
            .map(|hint| hint.name.clone())
            .filter(|name| !name.is_empty()),
    );
    with_status(|status| {
        status.registered = true;
        status.last_register_unix_ms = Some(now);
        status.last_error = None;
        status.observed_ip = response.observed_ip.clone();
        status.claimed = Some(response.claimed);
        if response.claimed {
            status.claimed_by_user_id = response.claimed_by_user_id.clone();
            status.claimed_by_handle = response.claimed_by_handle.clone();
            status.claim_code = None;
            status.claim_url = None;
            status.claim_code_expires_unix_ms = None;
            status.bootstrap = false;
            status.claim_binding =
                Some(match (&status.signed_claim, &response.claimed_by_user_id) {
                    (Some(record), Some(asserted)) if record.account_user_id == *asserted => {
                        ClaimBinding::DaemonSigned
                    }
                    (Some(_), Some(_)) => ClaimBinding::Mismatch,
                    // An older service asserts no owner id — nothing to
                    // cross-check against, even with a local record.
                    (Some(_), None) | (None, _) => ClaimBinding::ServiceAsserted,
                });
            if status.claim_binding == Some(ClaimBinding::Mismatch) {
                eprintln!(
                    "[connect] WARNING: the rendezvous asserts this daemon is claimed by \
                     account {} but this daemon co-signed a claim by account {} — a re-bind \
                     this daemon never acknowledged",
                    response
                        .claimed_by_user_id
                        .as_deref()
                        .unwrap_or("<unknown>"),
                    status
                        .signed_claim
                        .as_ref()
                        .map(|record| record.account_user_id.as_str())
                        .unwrap_or("<none>"),
                );
            }
        } else {
            status.claimed_by_user_id = None;
            status.claimed_by_handle = None;
            status.claim_binding = None;
            let (effective_code, effective_url) = match &bootstrap {
                Some((phrase, url)) => (Some(phrase.clone()), Some(url.clone())),
                None => (response.claim_code.clone(), response.claim_url.clone()),
            };
            if status.claim_code != effective_code {
                print_claim = match (&bootstrap, &effective_url, &effective_code) {
                    (Some(_), Some(url), _) => Some(format!(
                        "first-owner bootstrap: claim this daemon at {url} — entering the \
                         phrase also enrolls the claiming browser as this daemon's first owner"
                    )),
                    (None, Some(url), _) if !url.is_empty() => {
                        Some(format!("claim this daemon at {url}"))
                    }
                    (None, _, Some(code)) if !code.is_empty() => {
                        Some(format!("claim this daemon with code {code}"))
                    }
                    _ => None,
                };
            }
            status.claim_code = effective_code;
            status.claim_url = effective_url;
            status.claim_code_expires_unix_ms = response.claim_code_expires_unix_ms;
            status.bootstrap = bootstrap.is_some();
        }
    });
    if let Some(line) = print_claim {
        eprintln!("[connect] {line}");
    }
}

/// Report leases that expired without an .env fallback (credential
/// custody): the service turns this into a Web Push telling the owner
/// which daemon went dry. Best-effort — a failed report is dropped, the
/// dashboard lease status still shows the expired note.
async fn report_dry_credentials(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
) {
    let notices = crate::credential_leases::take_dry_notices();
    if notices.is_empty() {
        return;
    }
    let credentials: Vec<serde_json::Value> = notices
        .iter()
        .map(|notice| serde_json::json!({ "kind": notice.kind, "label": notice.label }))
        .collect();
    let url = match join_url(base_url, "api/daemon/dry") {
        Ok(url) => url,
        Err(e) => {
            eprintln!("[connect] dry-credential report skipped: {e}");
            return;
        }
    };
    let result = authenticated(config, client.post(url))
        .json(&serde_json::json!({
            "daemon_id": daemon_id,
            "credentials": credentials,
        }))
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => eprintln!(
            "[connect] dry-credential report failed: HTTP {}",
            resp.status()
        ),
        Err(e) => eprintln!("[connect] dry-credential report failed: {e}"),
    }
}

async fn register(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    daemon_public_key: &str,
) -> Result<RegisterResponse, RegisterError> {
    let request = RegisterRequest {
        protocol: "intendant-connect-rendezvous-v1",
        daemon_id: daemon_id.to_string(),
        daemon_public_key: daemon_public_key.to_string(),
        // Fresh boxes offer first-owner bootstrap: mint (or keep) the
        // local phrase and register only its hash. `None` the moment an
        // owner exists — the service then reverts to minting its own
        // display-only codes.
        bootstrap_code_hash: current_bootstrap_phrase()
            .as_deref()
            .map(claim_code_hash),
    };
    authenticated(
        config,
        client.post(join_url(base_url, "api/daemon/register").map_err(RegisterError::Transient)?),
    )
    .json(&request)
    .send()
    .await
    .map_err(|e| RegisterError::Transient(e.to_string()))?
    .error_for_status()
    .map_err(|e| {
        // 429 is a client error by status class but pure weather by
        // meaning — keep it on the fast retry clock.
        let rejected = e
            .status()
            .map(|s| s.is_client_error() && s != reqwest::StatusCode::TOO_MANY_REQUESTS)
            .unwrap_or(false);
        if rejected {
            RegisterError::Rejected(e.to_string())
        } else {
            RegisterError::Transient(e.to_string())
        }
    })?
    .json::<RegisterResponse>()
    .await
    .map_err(|e| RegisterError::Transient(e.to_string()))
}

async fn poll_next(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
) -> Result<Option<RendezvousEvent>, String> {
    let mut url = join_url(base_url, "api/daemon/next")?;
    url.query_pairs_mut()
        .append_pair("daemon_id", daemon_id)
        .append_pair("timeout_ms", &config.poll_timeout_ms.to_string());
    let response = authenticated(config, client.get(url))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    let response = response.error_for_status().map_err(|e| e.to_string())?;
    response
        .json::<RendezvousEvent>()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

async fn handle_event(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    identity: &DaemonIdentity,
    dashboard_control: &Arc<DashboardControlRegistry>,
    gateway_tcp_port: Option<u16>,
    event: RendezvousEvent,
) {
    match event.kind.as_str() {
        "offer" => {
            let Some(sdp) = event.sdp.as_deref().filter(|s| !s.trim().is_empty()) else {
                let _ = post_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    "missing sdp",
                )
                .await;
                return;
            };
            let session_grant = event
                .session_grant
                .as_deref()
                .map(str::trim)
                .filter(|grant| !grant.is_empty())
                .map(str::to_string);
            let client_nonce = event
                .client_nonce
                .as_deref()
                .map(str::trim)
                .filter(|nonce| !nonce.is_empty())
                .map(str::to_string);
            // A signed browser identity key authenticates end-to-end: the
            // rendezvous only relays it, and a bad signature fails closed so
            // a malicious relay cannot strip or corrupt the binding.
            let client_key_fields = crate::access::client_key::ClientKeyOfferFields {
                client_key: event.client_key.clone(),
                client_key_sig: event.client_key_sig.clone(),
                client_key_ts: event.client_key_ts,
                client_key_proto: event.client_key_proto.clone(),
                client_key_account_user_id: event.client_key_account_user_id.clone(),
                client_key_account_name: event.client_key_account_name.clone(),
            };
            let verified_client_key = match client_key_fields.verify(
                daemon_id,
                client_nonce.as_deref().unwrap_or(""),
                sdp,
                crate::access::client_key::now_unix_ms(),
            ) {
                Ok(verified) => verified,
                Err(e) => {
                    let _ = post_error(
                        client,
                        base_url,
                        config,
                        daemon_id,
                        &event.id,
                        &format!("client key verification failed: {e}"),
                    )
                    .await;
                    return;
                }
            };
            // When the device key attests an account (v2 signature) AND the
            // service stamped one from its session, the two must agree — a
            // disagreement means the relay and the device are telling
            // different stories about who is knocking, and nothing
            // downstream should silently pick one.
            if let (Some((attested_user, _)), Some(stamped_user)) = (
                verified_client_key
                    .as_ref()
                    .and_then(|key| key.attested_account.as_ref()),
                event
                    .user_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty()),
            ) {
                if attested_user != stamped_user {
                    let _ = post_error(
                        client,
                        base_url,
                        config,
                        daemon_id,
                        &event.id,
                        &format!(
                            "client key attests account {attested_user:?} but the rendezvous \
                             asserts {stamped_user:?} — refusing the mismatched offer"
                        ),
                    )
                    .await;
                    return;
                }
            }
            // Org-grant ride-along (phase 6 step 4): a member's offer may
            // carry its signed org grant document so first contact with a
            // daemon that trusts the org is one round trip. Materialize it
            // before grant resolution — the freshly written grant then
            // resolves for this very offer. A failure is non-fatal: if
            // another identity resolves the session proceeds, otherwise
            // the error rides back inside the refusal.
            let org_grant_error = event
                .org_grant
                .as_ref()
                .filter(|doc| !doc.is_null())
                .and_then(|doc| {
                    crate::access::org::present_org_grant_value(
                        &crate::access::backend::select_backend().cert_dir(),
                        doc,
                        &[daemon_id.to_string()],
                        crate::access::client_key::now_unix_ms() as u64,
                    )
                    .err()
                });
            if let Some(org_error) = org_grant_error.as_deref() {
                eprintln!("[connect] offer org grant not accepted: {org_error}");
            }
            let grant = match connect_dashboard_grant(
                event.user_id.as_deref(),
                event.account_name.as_deref(),
                verified_client_key.as_ref(),
            ) {
                Ok(grant) => grant,
                Err(e) => {
                    let e = match org_grant_error {
                        Some(org_error) => {
                            format!("{e} The offer's org grant was not accepted: {org_error}")
                        }
                        None => e,
                    };
                    // A verified key without a grant is an enrollment
                    // candidate: queue it so the owner can approve from an
                    // already-trusted Access session instead of copying the
                    // fingerprint out of this error by hand.
                    if let Some(key) = verified_client_key.as_ref() {
                        let origin = base_origin(base_url);
                        // Prefer the account the device key itself attested
                        // (v2 signature); the relay-asserted identity is
                        // only the fallback hint.
                        let (account_hint, account_attested) = match key.attested_account.as_ref()
                        {
                            Some((_, name)) if !name.is_empty() => (format!("@{name}"), true),
                            Some((user_id, _)) => {
                                (user_id.chars().take(12).collect::<String>(), true)
                            }
                            None => (
                                match (
                                    event
                                        .account_name
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|v| !v.is_empty()),
                                    event
                                        .user_id
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|v| !v.is_empty()),
                                ) {
                                    (Some(name), _) => format!("@{name}"),
                                    (None, Some(id)) => id.chars().take(12).collect(),
                                    (None, None) => String::new(),
                                },
                                false,
                            ),
                        };
                        crate::access::enrollment::record_refused_client_key(
                            &key.fingerprint,
                            &key.public_key_b64u,
                            &origin,
                            "connect-dashboard-control",
                            &account_hint,
                            account_attested,
                            crate::access::client_key::now_unix_ms(),
                        );
                    }
                    let _ = post_error(client, base_url, config, daemon_id, &event.id, &e).await;
                    return;
                }
            };
            // A cloud box's interface addresses are private (the public IP
            // lives on the provider's 1:1 NAT), and this engine gathers no
            // server-reflexive candidates — so hosted offers advertise an
            // ICE-TCP candidate at the rendezvous-observed public address
            // on the gateway port, the one address the world can reach.
            let tcp_advertised_addr = status_snapshot()
                .observed_ip
                .as_deref()
                .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
                .zip(gateway_tcp_port)
                .map(|(ip, port)| std::net::SocketAddr::new(ip, port));
            match dashboard_control
                .answer_offer_with_session_id_grant_and_tcp(
                    uuid::Uuid::new_v4().to_string(),
                    sdp.to_string(),
                    session_grant,
                    client_nonce,
                    grant,
                    tcp_advertised_addr,
                )
                .await
            {
                Ok(answer) => {
                    let body = AnswerRequest {
                        protocol: "intendant-connect-rendezvous-v1",
                        daemon_id: daemon_id.to_string(),
                        request_id: event.id,
                        session_id: answer.session_id,
                        sdp: answer.sdp,
                        binding: answer.binding,
                    };
                    if let Err(e) = authenticated(
                        config,
                        client.post(match join_url(base_url, "api/daemon/answer") {
                            Ok(url) => url,
                            Err(e) => {
                                eprintln!("[connect] answer URL failed: {e}");
                                return;
                            }
                        }),
                    )
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|resp| {
                        resp.error_for_status()
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }) {
                        eprintln!("[connect] post answer failed: {e}");
                    }
                }
                Err(e) => {
                    let _ = post_error(client, base_url, config, daemon_id, &event.id, &e).await;
                }
            }
        }
        "ice" => {
            let applied = match (event.session_id.as_deref(), event.candidate.as_ref()) {
                (Some(session_id), Some(candidate)) => dashboard_control
                    .add_ice_candidate(session_id, candidate)
                    .await
                    .unwrap_or(false),
                _ => false,
            };
            if !applied {
                eprintln!("[connect] dropped ICE candidate for event {}", event.id);
            }
        }
        "close" => {
            if let Some(session_id) = event.session_id.as_deref() {
                dashboard_control.close(session_id).await;
            }
        }
        "claim_challenge" => {
            let Some(claim_id) = event.claim_id.as_deref().filter(|s| !s.trim().is_empty()) else {
                let _ = post_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    "missing claim_id",
                )
                .await;
                return;
            };
            let Some(challenge) = event.challenge.as_deref().filter(|s| !s.trim().is_empty())
            else {
                let _ = post_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    "missing claim challenge",
                )
                .await;
                return;
            };
            // Sign v2 (account-bound) whenever the challenge names the
            // claiming account: the daemon then co-signs *who* claimed it,
            // and keeps its own record of that acknowledgment. v1 remains
            // for older services whose challenges are account-blind.
            let account_user_id = event
                .user_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let account_name = event
                .account_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_string();
            let daemon_public_key = identity.public_key_b64u();
            // First-owner bootstrap: an armed claim carries the browser's
            // identity key plus a tag derived from the daemon-minted
            // phrase. Verify and enroll BEFORE acknowledging the claim —
            // a claim that promised ownership must never half-complete as
            // a metadata-only binding.
            if let (Some(bootstrap_key), Some(bootstrap_tag)) = (
                event
                    .bootstrap_client_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                event
                    .bootstrap_client_key_tag
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
            ) {
                if let Err(e) = bootstrap_enroll_first_owner(
                    bootstrap_key,
                    bootstrap_tag,
                    account_user_id.as_deref().unwrap_or(""),
                    &account_name,
                    daemon_id,
                    &daemon_public_key,
                ) {
                    eprintln!("[connect] first-owner bootstrap refused: {e}");
                    let _ = post_claim_error(
                        client,
                        base_url,
                        config,
                        daemon_id,
                        &event.id,
                        claim_id,
                        &format!("first-owner bootstrap refused: {e}"),
                    )
                    .await;
                    return;
                }
            }
            let (protocol, payload) = match account_user_id.as_deref() {
                Some(user_id) => (
                    CLAIM_PROTOCOL_V2,
                    claim_signing_payload_v2(
                        claim_id,
                        daemon_id,
                        &daemon_public_key,
                        challenge,
                        user_id,
                        &account_name,
                    ),
                ),
                None => (
                    CLAIM_PROTOCOL_V1,
                    claim_signing_payload(claim_id, daemon_id, &daemon_public_key, challenge),
                ),
            };
            let body = ClaimProofRequest {
                protocol,
                daemon_id: daemon_id.to_string(),
                request_id: event.id,
                claim_id: claim_id.to_string(),
                challenge: challenge.to_string(),
                signature: identity.sign_b64u(payload.as_bytes()),
            };
            match authenticated(
                config,
                client.post(match join_url(base_url, "api/daemon/claim-proof") {
                    Ok(url) => url,
                    Err(e) => {
                        eprintln!("[connect] claim-proof URL failed: {e}");
                        return;
                    }
                }),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())
            .and_then(|resp| {
                resp.error_for_status()
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }) {
                Ok(()) => {
                    // 2xx means the service verified the proof and bound
                    // the claim — this is the moment the daemon's own
                    // acknowledgment becomes durable truth.
                    if let Some(user_id) = account_user_id {
                        let record = SignedClaimRecord {
                            claim_id: claim_id.to_string(),
                            daemon_id: daemon_id.to_string(),
                            rendezvous: base_origin(base_url),
                            account_user_id: user_id,
                            account_name: account_name.clone(),
                            protocol: protocol.to_string(),
                            signed_at_unix_ms: crate::access::client_key::now_unix_ms(),
                        };
                        store_signed_claim_record(&record);
                        eprintln!(
                            "[connect] claim acknowledged — this daemon co-signed being \
                             claimed by {}",
                            if record.account_name.is_empty() {
                                record.account_user_id.clone()
                            } else {
                                format!("@{}", record.account_name)
                            }
                        );
                        with_status(|status| {
                            status.claimed = Some(true);
                            status.claimed_by_user_id = Some(record.account_user_id.clone());
                            status.claimed_by_handle = if record.account_name.is_empty() {
                                None
                            } else {
                                Some(record.account_name.clone())
                            };
                            status.claim_binding = Some(ClaimBinding::DaemonSigned);
                            status.claim_code = None;
                            status.claim_url = None;
                            status.claim_code_expires_unix_ms = None;
                            status.signed_claim = Some(record);
                        });
                    } else {
                        with_status(|status| {
                            status.claimed = Some(true);
                            status.claim_binding = Some(ClaimBinding::ServiceAsserted);
                            status.claim_code = None;
                            status.claim_url = None;
                            status.claim_code_expires_unix_ms = None;
                        });
                    }
                }
                Err(e) => eprintln!("[connect] post claim proof failed: {e}"),
            }
        }
        other => {
            let _ = post_error(
                client,
                base_url,
                config,
                daemon_id,
                &event.id,
                &format!("unknown event kind: {other}"),
            )
            .await;
        }
    }
}

fn connect_dashboard_grant(
    user_id: Option<&str>,
    account_name: Option<&str>,
    client_key: Option<&crate::access::client_key::VerifiedClientKey>,
) -> Result<crate::dashboard_control::DashboardControlGrant, String> {
    let user_id = user_id.map(str::trim).filter(|value| !value.is_empty());
    let account_name = account_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if user_id.is_none() && account_name.is_none() && client_key.is_none() {
        return Err(connect_account_not_authorized_message(
            None,
            None,
            None,
            Some("the Connect offer did not include account identity or a client key"),
        ));
    }

    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let path = crate::access::iam::iam_state_path(&cert_dir);
    if !path.exists() {
        return Err(connect_account_not_authorized_message(
            user_id,
            account_name,
            client_key,
            Some("no daemon-local IAM state exists"),
        ));
    }
    let state = crate::access::iam::load_state(&cert_dir)
        .map_err(|e| format!("local IAM state is invalid: {e}"))?;
    connect_dashboard_grant_from_state(state, user_id, account_name, client_key)
}

fn connect_dashboard_grant_from_state(
    state: crate::access::iam::LocalIamState,
    user_id: Option<&str>,
    account_name: Option<&str>,
    client_key: Option<&crate::access::client_key::VerifiedClientKey>,
) -> Result<crate::dashboard_control::DashboardControlGrant, String> {
    let user_id = user_id.map(str::trim).filter(|value| !value.is_empty());
    let account_name = account_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if user_id.is_none() && account_name.is_none() && client_key.is_none() {
        return Err(connect_account_not_authorized_message(
            None,
            None,
            None,
            Some("the Connect offer did not include account identity or a client key"),
        ));
    }

    // A verified browser identity key is the strongest binding: it
    // authenticated end-to-end regardless of what the rendezvous claims.
    if let Some(key) = client_key {
        if let Some(principal) = crate::access::iam::principal_for_client_key(
            &state,
            &key.fingerprint,
            "connect-dashboard-control",
        )
        .or_else(|| {
            crate::access::iam::principal_for_client_key_any_status(
                &state,
                &key.fingerprint,
                "connect-dashboard-control",
            )
        }) {
            return Ok(
                crate::dashboard_control::DashboardControlGrant::UserClient {
                    principal,
                    iam_state: state,
                },
            );
        }
    }

    match crate::access::iam::principal_for_connect_account(
        &state,
        user_id.unwrap_or_default(),
        account_name,
        "connect-dashboard-control",
    ) {
        Some(principal) => Ok(
            crate::dashboard_control::DashboardControlGrant::UserClient {
                principal,
                iam_state: state,
            },
        ),
        None => match crate::access::iam::principal_for_connect_account_any_status(
            &state,
            user_id.unwrap_or_default(),
            account_name,
            "connect-dashboard-control",
        ) {
            Some(principal) => Ok(
                crate::dashboard_control::DashboardControlGrant::UserClient {
                    principal,
                    iam_state: state,
                },
            ),
            None => Err(connect_account_not_authorized_message(
                user_id,
                account_name,
                client_key,
                Some("no matching daemon-local grant exists for the client key or Connect account"),
            )),
        },
    }
}

fn connect_account_not_authorized_message(
    user_id: Option<&str>,
    account_name: Option<&str>,
    client_key: Option<&crate::access::client_key::VerifiedClientKey>,
    detail: Option<&str>,
) -> String {
    let user_id = user_id.map(str::trim).filter(|value| !value.is_empty());
    let account_name = account_name
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let identity = match (account_name, user_id) {
        (Some(name), Some(id)) => format!("@{name} ({})", id.chars().take(12).collect::<String>()),
        (Some(name), None) => format!("@{name}"),
        (None, Some(id)) => format!(
            "Connect account {}",
            id.chars().take(12).collect::<String>()
        ),
        (None, None) => "This client".to_string(),
    };
    let mut message = format!(
        "{identity} is not authorized by this daemon. Open this daemon's Access page through direct mTLS/local root access and add a local IAM grant before using hosted Connect."
    );
    if let Some(key) = client_key {
        message.push_str(&format!(
            " The verified browser key fingerprint is {} — grant it under Access → People & Devices.",
            key.fingerprint
        ));
    }
    if let Some(detail) = detail.and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }) {
        message.push_str(" Detail: ");
        message.push_str(detail);
        message.push('.');
    }
    message
}

fn claim_signing_payload(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
) -> String {
    format!("{CLAIM_PROTOCOL_V1}\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n")
}

/// Mirrors `claim_signing_payload_v2` in `intendant-connect` — stable
/// protocol, replicated rather than shared. The account fields come from
/// the claim challenge verbatim; the service verifies against its own
/// claim-time snapshot, so a relay that alters them fails the signature.
fn claim_signing_payload_v2(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
    account_user_id: &str,
    account_name: &str,
) -> String {
    format!(
        "{CLAIM_PROTOCOL_V2}\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n{account_user_id}\n{account_name}\n"
    )
}

/// Mirrors `unclaim_signing_payload` in `intendant-connect`.
fn unclaim_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> String {
    format!("{UNCLAIM_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n")
}

/// The rendezvous base URL reduced to its origin (scheme://host[:port]).
fn base_origin(base_url: &Url) -> String {
    let mut origin = base_url.clone();
    origin.set_path("");
    origin.set_query(None);
    origin.set_fragment(None);
    origin.to_string().trim_end_matches('/').to_string()
}

/// Daemon-initiated release of the claim binding at the rendezvous — the
/// recovery verb for a squatted or mis-claimed box, and the paved way to
/// move a daemon between accounts. Returns whether the service changed
/// anything (`false` = it was already unclaimed). Independent of the
/// polling client: it signs a fresh timestamped payload with the daemon
/// identity key, so it works whether or not the client loop is running.
pub(crate) async fn request_unclaim(config: &ConnectConfig) -> Result<bool, String> {
    let base_url = config
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "no rendezvous_url configured".to_string())?;
    let base_url = Url::parse(base_url).map_err(|e| format!("invalid rendezvous_url: {e}"))?;
    let identity = DaemonIdentity::load_or_create_default()?;
    let daemon_public_key = identity.public_key_b64u();
    let daemon_id = config
        .daemon_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| daemon_public_key.clone());
    let issued_at_unix_ms = crate::access::client_key::now_unix_ms().max(0) as u64;
    let payload = unclaim_signing_payload(&daemon_id, &daemon_public_key, issued_at_unix_ms);
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = authenticated(
        config,
        client.post(join_url(&base_url, "api/daemon/unclaim")?),
    )
    .json(&serde_json::json!({
        "protocol": UNCLAIM_PROTOCOL,
        "daemon_id": daemon_id,
        "daemon_public_key": daemon_public_key,
        "issued_at_unix_ms": issued_at_unix_ms,
        "signature": identity.sign_b64u(payload.as_bytes()),
    }))
    .send()
    .await
    .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("unclaim rejected: HTTP {status} {body}"));
    }
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    let changed = body
        .get("changed")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    clear_signed_claim_record();
    with_status(|status| {
        status.claimed = Some(false);
        status.claimed_by_user_id = None;
        status.claimed_by_handle = None;
        status.claim_binding = None;
        status.signed_claim = None;
        status.claim_code = None;
        status.claim_url = None;
        status.claim_code_expires_unix_ms = None;
    });
    // Wake the client loop so a fresh claim code arrives now, not at the
    // next refresh tick.
    register_nudge().notify_one();
    Ok(changed)
}

/* ── Fleet DNS: daemon-signed record publishes (fleet_cert.rs) ──
Payloads REPLICATE bin/connect/rendezvous.rs (twin golden test below);
same resolution + signing discipline as request_unclaim. */

const DNS_PUBLISH_PROTOCOL: &str = "intendant-connect-dns-publish-v1";
const DNS_ACME_PROTOCOL: &str = "intendant-connect-dns-acme-v1";

fn dns_publish_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    addresses_csv: &str,
) -> String {
    format!(
        "{DNS_PUBLISH_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{addresses_csv}\n"
    )
}

fn dns_acme_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    txt_value: &str,
) -> String {
    format!(
        "{DNS_ACME_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{txt_value}\n"
    )
}

/// The signing context every daemon-signed rendezvous call needs (fleet
/// DNS, attention nudges): effective config, rendezvous URL, identity, and
/// the effective daemon id.
fn signed_daemon_context() -> Result<(ConnectConfig, Url, DaemonIdentity, String), String> {
    let mut config = crate::project::ConnectConfig::default().effective_with_env();
    if config.rendezvous_url.is_none() {
        config.rendezvous_url = status_snapshot().rendezvous_url;
    }
    if config.daemon_id.is_none() {
        config.daemon_id = status_snapshot().daemon_id;
    }
    let base_url = config
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "no rendezvous configured".to_string())?;
    let base_url = Url::parse(base_url).map_err(|e| format!("invalid rendezvous_url: {e}"))?;
    let identity = DaemonIdentity::load_or_create_default()?;
    let daemon_id = config
        .daemon_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| identity.public_key_b64u());
    Ok((config, base_url, identity, daemon_id))
}

async fn dns_signed_post(
    path: &str,
    protocol: &str,
    payload_tail: &str,
    extra: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (config, base_url, identity, daemon_id) = signed_daemon_context()?;
    let daemon_public_key = identity.public_key_b64u();
    let issued_at_unix_ms = crate::access::client_key::now_unix_ms().max(0) as u64;
    let payload = format!(
        "{protocol}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{payload_tail}\n"
    );
    let mut body = serde_json::json!({
        "protocol": protocol,
        "daemon_id": daemon_id,
        "daemon_public_key": daemon_public_key,
        "issued_at_unix_ms": issued_at_unix_ms,
        "signature": identity.sign_b64u(payload.as_bytes()),
    });
    if let (Some(map), Some(extra_map)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in extra_map {
            map.insert(key.clone(), value.clone());
        }
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = authenticated(&config, client.post(join_url(&base_url, path)?))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("dns publish rejected: HTTP {status} {text}"));
    }
    response.json().await.map_err(|e| e.to_string())
}

/// Publish this daemon's A/AAAA addresses for its fleet name. Returns
/// the address list the service accepted.
pub(crate) async fn dns_publish_addresses(addresses: &[String]) -> Result<Vec<String>, String> {
    let addresses_csv = addresses.join(",");
    let body = dns_signed_post(
        "api/dns/publish",
        DNS_PUBLISH_PROTOCOL,
        &addresses_csv,
        serde_json::json!({ "addresses": addresses }),
    )
    .await?;
    Ok(body
        .get("addresses")
        .and_then(|value| value.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// Publish an ACME DNS-01 TXT value for this daemon's name.
pub(crate) async fn dns_acme_set(txt_value: &str) -> Result<(), String> {
    dns_signed_post(
        "api/dns/acme-challenge",
        DNS_ACME_PROTOCOL,
        txt_value,
        serde_json::json!({ "txt_value": txt_value }),
    )
    .await
    .map(|_| ())
}

/// Clear this daemon's ACME TXT records (order finished either way).
pub(crate) async fn dns_acme_clear() -> Result<(), String> {
    dns_signed_post(
        "api/dns/acme-challenge",
        DNS_ACME_PROTOCOL,
        "",
        serde_json::json!({ "clear": true }),
    )
    .await
    .map(|_| ())
}

/* ── Pending-request attention nudge (attention_nudge.rs) ──
Payload REPLICATES bin/connect/push.rs (twin golden test below); same
resolution + signing discipline as the fleet-DNS publishes. PRIVACY: the
body carries only a request KIND and a session display LABEL — never
command text, question text, or paths; the service composes the push from
those plus the daemon label it already stores. */

const NOTIFY_PROTOCOL: &str = "intendant-connect-daemon-notify-v1";

/// Mirrors `notify_signing_payload` in `intendant-connect`.
fn notify_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    kind: &str,
    session_label: &str,
) -> String {
    format!(
        "{NOTIFY_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{kind}\n{session_label}\n"
    )
}

/// Tell the rendezvous an agent→user request (`kind`: "approval" |
/// "question") has gone unseen, so it can Web-Push the owner's opted-in
/// browsers. Errors are expected weather for unclaimed / offline daemons —
/// the caller degrades silently.
pub(crate) async fn notify_attention(kind: &str, session_label: &str) -> Result<(), String> {
    // Only a claimed daemon has an owner to notify; the service would
    // refuse an unclaimed nudge anyway, so skip the round-trip.
    if status_snapshot().claimed != Some(true) {
        return Err("daemon is not claimed on a rendezvous".to_string());
    }
    let (config, base_url, identity, daemon_id) = signed_daemon_context()?;
    let daemon_public_key = identity.public_key_b64u();
    let issued_at_unix_ms = crate::access::client_key::now_unix_ms().max(0) as u64;
    let payload = notify_signing_payload(
        &daemon_id,
        &daemon_public_key,
        issued_at_unix_ms,
        kind,
        session_label,
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = authenticated(
        &config,
        client.post(join_url(&base_url, "api/daemon/notify")?),
    )
    .json(&serde_json::json!({
        "protocol": NOTIFY_PROTOCOL,
        "daemon_id": daemon_id,
        "daemon_public_key": daemon_public_key,
        "issued_at_unix_ms": issued_at_unix_ms,
        "signature": identity.sign_b64u(payload.as_bytes()),
        "kind": kind,
        "session_label": session_label,
    }))
    .send()
    .await
    .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("notify rejected: HTTP {status} {text}"));
    }
    Ok(())
}

async fn post_error(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    request_id: &str,
    error: &str,
) -> Result<(), String> {
    post_error_inner(client, base_url, config, daemon_id, request_id, None, error).await
}

/// Claim-scoped error: also names the claim so the service rejects it and
/// the claiming page surfaces the reason instead of timing out.
async fn post_claim_error(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    request_id: &str,
    claim_id: &str,
    error: &str,
) -> Result<(), String> {
    post_error_inner(
        client,
        base_url,
        config,
        daemon_id,
        request_id,
        Some(claim_id),
        error,
    )
    .await
}

async fn post_error_inner(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    request_id: &str,
    claim_id: Option<&str>,
    error: &str,
) -> Result<(), String> {
    let body = ErrorRequest {
        daemon_id: daemon_id.to_string(),
        request_id: request_id.to_string(),
        claim_id: claim_id.map(str::to_string),
        error: error.to_string(),
    };
    authenticated(config, client.post(join_url(base_url, "api/daemon/error")?))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn authenticated(config: &ConnectConfig, builder: RequestBuilder) -> RequestBuilder {
    match config
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(token) => builder.bearer_auth(token),
        None => builder,
    }
}

pub(crate) fn join_url(base_url: &Url, path: &str) -> Result<Url, String> {
    let mut url = base_url.clone();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "rendezvous_url cannot be a base URL".to_string())?;
        let base_segments: Vec<String> = base_url
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        segments.clear();
        for segment in base_segments {
            segments.push(&segment);
        }
        for segment in path.split('/').filter(|segment| !segment.is_empty()) {
            segments.push(segment);
        }
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_url_appends_under_base() {
        let base = Url::parse("https://connect.example/root/").unwrap();
        assert_eq!(
            join_url(&base, "api/daemon/next").unwrap().as_str(),
            "https://connect.example/root/api/daemon/next"
        );
    }

    #[test]
    fn join_url_treats_base_path_without_slash_as_directory() {
        let base = Url::parse("https://connect.example/root?ignored=true#frag").unwrap();
        assert_eq!(
            join_url(&base, "/api/daemon/next").unwrap().as_str(),
            "https://connect.example/root/api/daemon/next"
        );
    }

    #[test]
    fn connect_account_metadata_can_bind_to_scoped_local_grant() {
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:connect:alice".to_string(),
            kind: "connect_account".to_string(),
            label: "alice".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: Some(serde_json::json!({
                "provider": "intendant.dev",
                "account_name": "alice"
            })),
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "connect_account",
                "user_id": "user-123",
                "account_name": "alice"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:connect:alice:inspect".to_string(),
            principal_id: "principal:connect:alice".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:local-user-client".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "test Connect account grant".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });

        let grant =
            connect_dashboard_grant_from_state(state, Some("user-123"), Some("alice"), None)
                .unwrap();
        let crate::dashboard_control::DashboardControlGrant::UserClient {
            principal,
            iam_state,
        } = grant
        else {
            panic!("expected scoped user-client grant");
        };
        assert_eq!(principal.kind, "connect_account");
        assert!(
            crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
        assert!(
            !crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn unmatched_connect_account_metadata_requires_local_iam_grant() {
        let state = crate::access::iam::LocalIamState::default();
        let error =
            connect_dashboard_grant_from_state(state, Some("user-123"), Some("alice"), None)
                .unwrap_err();
        assert!(error.contains("@alice"));
        assert!(error.contains("local IAM grant"));
        assert!(error.contains("direct mTLS"));
    }

    #[test]
    fn connect_offer_without_account_identity_is_rejected() {
        let state = crate::access::iam::LocalIamState::default();
        let error = connect_dashboard_grant_from_state(state, None, None, None).unwrap_err();
        assert!(error.contains("not authorized"));
        assert!(error.contains("did not include account identity or a client key"));
    }

    #[test]
    fn verified_client_key_binds_before_account_metadata() {
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:client-key:fp-abc".to_string(),
            kind: "client_key".to_string(),
            label: "Anchor browser key".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "client_key",
                "fingerprint": "fp-abc",
                "origin": "https://anchor.local:8765"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:client-key:fp-abc:terminal".to_string(),
            principal_id: "principal:client-key:fp-abc".to_string(),
            target_id: "local".to_string(),
            role_id: "role:terminal".to_string(),
            policy_id: "policy:terminal".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "test client key grant".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });

        let key = crate::access::client_key::VerifiedClientKey {
            fingerprint: "fp-abc".to_string(),
            public_key_b64u: "unused".to_string(),
            attested_account: None,
        };
        // Account metadata matches nothing, but the verified key must bind.
        let grant = connect_dashboard_grant_from_state(
            state,
            Some("unknown-user"),
            Some("unknown"),
            Some(&key),
        )
        .unwrap();
        let crate::dashboard_control::DashboardControlGrant::UserClient {
            principal,
            iam_state,
        } = grant
        else {
            panic!("expected key-bound user-client grant");
        };
        assert_eq!(principal.kind, "client_key");
        assert!(
            crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::TerminalWrite,
            )
            .allowed
        );
        assert!(
            !crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn unmatched_client_key_reports_its_fingerprint() {
        let state = crate::access::iam::LocalIamState::default();
        let key = crate::access::client_key::VerifiedClientKey {
            fingerprint: "fp-unenrolled".to_string(),
            public_key_b64u: "unused".to_string(),
            attested_account: None,
        };
        let error = connect_dashboard_grant_from_state(state, None, None, Some(&key)).unwrap_err();
        assert!(error.contains("fp-unenrolled"));
        assert!(error.contains("People & Devices"));
    }

    #[test]
    fn revoked_connect_account_binding_does_not_fall_back_to_root() {
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:connect:alice".to_string(),
            kind: "connect_account".to_string(),
            label: "alice".to_string(),
            status: "revoked".to_string(),
            source: "local_iam_state".to_string(),
            account: Some(serde_json::json!({
                "provider": "intendant.dev",
                "account_name": "alice"
            })),
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "connect_account",
                "user_id": "user-123",
                "account_name": "alice"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:connect:alice:inspect".to_string(),
            principal_id: "principal:connect:alice".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:scoped-human".to_string(),
            status: "revoked".to_string(),
            source: "local_iam_state".to_string(),
            reason: "revoked Connect account grant".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: Some(102),
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });

        let grant =
            connect_dashboard_grant_from_state(state, Some("user-123"), Some("alice"), None)
                .unwrap();
        let crate::dashboard_control::DashboardControlGrant::UserClient {
            principal,
            iam_state,
        } = grant
        else {
            panic!("expected inactive user-client grant, not root fallback");
        };
        assert_eq!(principal.kind, "connect_account");
        assert!(
            !crate::access::iam::evaluate_principal_operation_with_state(
                &iam_state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
    }

    /// Twin of `claim_and_unclaim_payloads_pin_the_wire_format` in
    /// `intendant-connect` — the two binaries replicate these formats
    /// rather than share code, so each side pins the same golden
    /// literals and drift fails a test instead of shipping as an
    /// unverifiable signature.
    #[test]
    fn claim_and_unclaim_payloads_pin_the_wire_format() {
        assert_eq!(
            claim_signing_payload("claim-1", "daemon-1", "PubKey", "challenge-1"),
            "intendant-connect-claim-v1\nclaim-1\ndaemon-1\nPubKey\nchallenge-1\n"
        );
        assert_eq!(
            claim_signing_payload_v2(
                "claim-1",
                "daemon-1",
                "PubKey",
                "challenge-1",
                "user-uuid-1",
                "lenny"
            ),
            "intendant-connect-claim-v2\nclaim-1\ndaemon-1\nPubKey\nchallenge-1\nuser-uuid-1\nlenny\n"
        );
        assert_eq!(
            unclaim_signing_payload("daemon-1", "PubKey", 1_700_000_000_000),
            "intendant-connect-unclaim-v1\ndaemon-1\nPubKey\n1700000000000\n"
        );
        assert_eq!(
            dns_publish_signing_payload(
                "daemon-1",
                "PubKey",
                1_700_000_000_000,
                "192.168.1.50,2001:db8::7"
            ),
            "intendant-connect-dns-publish-v1\ndaemon-1\nPubKey\n1700000000000\n192.168.1.50,2001:db8::7\n"
        );
        assert_eq!(
            dns_acme_signing_payload("daemon-1", "PubKey", 1_700_000_000_000, "tok-value"),
            "intendant-connect-dns-acme-v1\ndaemon-1\nPubKey\n1700000000000\ntok-value\n"
        );
        assert_eq!(
            notify_signing_payload(
                "daemon-1",
                "PubKey",
                1_700_000_000_000,
                "approval",
                "deploy review"
            ),
            "intendant-connect-daemon-notify-v1\ndaemon-1\nPubKey\n1700000000000\napproval\ndeploy review\n"
        );
    }

    /// Twin of the service's normalize/hash tests — one shared literal
    /// pins the cross-binary (and /connect-page JS) hash construction.
    #[test]
    fn claim_code_hash_matches_the_service_construction() {
        assert_eq!(
            normalize_claim_code("  Abandon ABILITY__able "),
            "abandon-ability-able"
        );
        assert_eq!(
            claim_code_hash("  Abandon ABILITY__able "),
            "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0"
        );
    }

    #[test]
    fn bootstrap_enrolls_first_owner_only_with_a_valid_tag_on_a_fresh_box() {
        let dir = tempfile::tempdir().unwrap();
        let cert_dir = dir.path();
        let phrase =
            "legal-winner-thank-year-wave-sausage-worth-useful-legal-winner-thank-yellow";
        let mut raw = vec![0x04u8];
        raw.extend_from_slice(&[7u8; 64]);
        let client_key_b64u = crate::daemon_identity::b64u(&raw);
        let tag = {
            let hmac_key = ring::hmac::Key::new(
                ring::hmac::HMAC_SHA256,
                ring::digest::digest(
                    &ring::digest::SHA256,
                    normalize_claim_code(phrase).as_bytes(),
                )
                .as_ref(),
            );
            let payload = bootstrap_tag_payload(
                "daemon-1",
                "DaemonPub",
                &client_key_b64u,
                "user-1",
                "alice",
            );
            crate::daemon_identity::b64u(ring::hmac::sign(&hmac_key, payload.as_bytes()).as_ref())
        };

        // Wrong phrase → refused, nothing written.
        let err = bootstrap_enroll_first_owner_at(
            cert_dir,
            "wrong-phrase",
            &client_key_b64u,
            &tag,
            "user-1",
            "alice",
            "daemon-1",
            "DaemonPub",
        )
        .unwrap_err();
        assert!(err.contains("tag verification failed"), "{err}");
        assert!(crate::access::iam::load_state(cert_dir)
            .unwrap()
            .principals
            .is_empty());

        // Tag bound to a different key → refused (the relay cannot swap
        // the enrolled key).
        let mut other_raw = vec![0x04u8];
        other_raw.extend_from_slice(&[9u8; 64]);
        let other_key_b64u = crate::daemon_identity::b64u(&other_raw);
        let err = bootstrap_enroll_first_owner_at(
            cert_dir,
            phrase,
            &other_key_b64u,
            &tag,
            "user-1",
            "alice",
            "daemon-1",
            "DaemonPub",
        )
        .unwrap_err();
        assert!(err.contains("tag verification failed"), "{err}");

        // Right phrase, right key → enrolls role:root with the bootstrap
        // sentinel origin (not a hosted origin, so no ceiling demotes it).
        let fingerprint = bootstrap_enroll_first_owner_at(
            cert_dir,
            phrase,
            &client_key_b64u,
            &tag,
            "user-1",
            "alice",
            "daemon-1",
            "DaemonPub",
        )
        .unwrap();
        let state = crate::access::iam::load_state(cert_dir).unwrap();
        assert_eq!(state.grants.len(), 1);
        assert_eq!(state.grants[0].role_id, "role:root");
        assert_eq!(state.principals.len(), 1);
        let authn = &state.principals[0].authn;
        assert!(authn.iter().any(|entry| {
            entry.get("fingerprint").and_then(|v| v.as_str()) == Some(fingerprint.as_str())
                && entry.get("origin").and_then(|v| v.as_str()) == Some("connect-bootstrap")
        }));
        let principal = crate::access::iam::principal_for_client_key(
            &state,
            &fingerprint,
            "connect-dashboard-control",
        )
        .expect("bootstrap principal resolves");
        assert_eq!(
            crate::access::iam::role_ceiling_for_session(&state, &principal),
            None,
            "the bootstrap sentinel origin must not be ceiling-capped"
        );

        // A second bootstrap of any kind is refused: the box has an owner.
        let err = bootstrap_enroll_first_owner_at(
            cert_dir,
            phrase,
            &client_key_b64u,
            &tag,
            "user-1",
            "alice",
            "daemon-1",
            "DaemonPub",
        )
        .unwrap_err();
        assert!(err.contains("already has an owner"), "{err}");
    }

    #[test]
    fn register_response_reads_claimed_by_and_expiry_fields() {
        let response: RegisterResponse = serde_json::from_str(
            r#"{
                "ok": true,
                "claimed": true,
                "claimed_by_user_id": "user-1",
                "claimed_by_handle": "lenny",
                "claim_code": null,
                "claim_code_expires_unix_ms": null,
                "claim_url": null
            }"#,
        )
        .unwrap();
        assert!(response.claimed);
        assert_eq!(response.claimed_by_user_id.as_deref(), Some("user-1"));
        assert_eq!(response.claimed_by_handle.as_deref(), Some("lenny"));

        // Old-service responses (no new fields) still parse.
        let legacy: RegisterResponse =
            serde_json::from_str(r#"{"ok": true, "claimed": false, "claim_code": "a-b-c"}"#)
                .unwrap();
        assert!(!legacy.claimed);
        assert_eq!(legacy.claim_code.as_deref(), Some("a-b-c"));
        assert_eq!(legacy.claimed_by_user_id, None);
        assert_eq!(legacy.claim_code_expires_unix_ms, None);
    }

    /// A register response asserting a different owner than the daemon's
    /// own signed acknowledgment must surface as a mismatch, not be
    /// silently displayed as truth.
    #[test]
    fn register_response_folding_cross_checks_the_signed_claim_record() {
        let record = SignedClaimRecord {
            claim_id: "claim-1".to_string(),
            daemon_id: "daemon-1".to_string(),
            rendezvous: "https://connect.example".to_string(),
            account_user_id: "alice-user-id".to_string(),
            account_name: "alice".to_string(),
            protocol: CLAIM_PROTOCOL_V2.to_string(),
            signed_at_unix_ms: 1_700_000_000_000,
        };
        with_status(|status| {
            *status = ConnectStatus::default();
            status.signed_claim = Some(record);
        });

        let base_url = Url::parse("https://connect.example").unwrap();
        note_register_response(
            &RegisterResponse {
                claimed: true,
                claimed_by_user_id: Some("alice-user-id".to_string()),
                claimed_by_handle: Some("alice".to_string()),
                claim_code: None,
                claim_code_daemon_minted: false,
                claim_code_expires_unix_ms: None,
                claim_url: None,
                observed_ip: None,
                fleet_dns: None,
            },
            &base_url,
        );
        assert_eq!(
            status_snapshot().claim_binding,
            Some(ClaimBinding::DaemonSigned)
        );

        note_register_response(
            &RegisterResponse {
                claimed: true,
                claimed_by_user_id: Some("mallory-user-id".to_string()),
                claimed_by_handle: Some("mallory".to_string()),
                claim_code: None,
                claim_code_daemon_minted: false,
                claim_code_expires_unix_ms: None,
                claim_url: None,
                observed_ip: None,
                fleet_dns: None,
            },
            &base_url,
        );
        assert_eq!(
            status_snapshot().claim_binding,
            Some(ClaimBinding::Mismatch)
        );

        // Unclaimed responses clear the claim view and surface the code.
        note_register_response(
            &RegisterResponse {
                claimed: false,
                claimed_by_user_id: None,
                claimed_by_handle: None,
                claim_code: Some("word-word-word".to_string()),
                claim_code_daemon_minted: false,
                claim_code_expires_unix_ms: Some(1_700_000_600_000),
                claim_url: Some(
                    "https://connect.example/connect?claim_code=word-word-word".to_string(),
                ),
                observed_ip: None,
                fleet_dns: None,
            },
            &base_url,
        );
        let status = status_snapshot();
        assert_eq!(status.claimed, Some(false));
        assert_eq!(status.claim_binding, None);
        assert_eq!(status.claim_code.as_deref(), Some("word-word-word"));
        assert_eq!(status.claim_code_expires_unix_ms, Some(1_700_000_600_000));

        // Leave no residue for other tests sharing the process-global
        // registry.
        with_status(|status| *status = ConnectStatus::default());
    }
}
