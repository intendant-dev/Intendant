//! Outbound Intendant Connect rendezvous client for dashboard-control signaling.
//!
//! Connect is the hosted account, route, and identity-metadata relay. The
//! default daemon refuses every Connect-origin dashboard-control event before
//! touching the control registry, IAM, or enrollment state — including events
//! from an older or self-hosted service that still forwards signaling. A
//! Connect account link is discovery metadata, never authentication. Direct
//! independently verified direct mTLS or local-root access is the trusted path for
//! daemon control and grant management.

use crate::daemon_identity::DaemonIdentity;
use crate::dashboard_control::DashboardControlRegistry;
use crate::project::ConnectConfig;
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const REGISTER_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Route-link codes expire and rotate locally on this clock. The service
/// stores only the signed hash and applies the same ten-minute TTL.
const CLAIM_CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// Claim-proof payload protocols, mirrored by the rendezvous service.
/// v1 is account-blind. v2 additionally binds the linked account
/// (user id + handle) into the route-association acknowledgment this
/// daemon signs. This detects later service-side re-binding; it is not
/// authentication or proof of local human approval. The daemon signs v2
/// whenever the challenge names an account and falls back to v1 for older
/// services.
const CLAIM_PROTOCOL_V1: &str = "intendant-connect-claim-v1";
const CLAIM_PROTOCOL_V2: &str = "intendant-connect-claim-v2";
/// Proof-of-possession on registration. A public daemon key identifies a
/// route but is not a credential; every open-registration request signs its
/// fresh timestamp and the locally minted route-code hash. Connect never
/// receives or returns the plaintext code.
const REGISTER_PROOF_PROTOCOL: &str = "intendant-connect-register-proof-v1";
const HOSTED_CONTROL_CAPABILITY_PROTOCOL: &str = "intendant-connect-hosted-control-capability-v1";
/// Daemon-signed release of a claim binding, mirrored by the service.
const UNCLAIM_PROTOCOL: &str = "intendant-connect-unclaim-v1";

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
    claim_code_hash: String,
    issued_at_unix_ms: u64,
    signature: String,
    hosted_control_enabled: bool,
    hosted_control_signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fleet_certificate_ledger: Option<crate::access::hosted_control::HostedCertificateLedger>,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    #[serde(default)]
    claimed: bool,
    /// Current linked account (service-asserted; wire name retained for
    /// compatibility). This is discovery metadata, never IAM ownership.
    #[serde(default)]
    claimed_by_user_id: Option<String>,
    #[serde(default)]
    claimed_by_handle: Option<String>,
    #[serde(default)]
    claim_code: Option<String>,
    #[serde(default)]
    claim_code_expires_unix_ms: Option<u64>,
    #[serde(default)]
    claim_url: Option<String>,
    /// Rotating post-registration credential. It authenticates only this
    /// daemon's poll/answer/error channel and carries no dashboard authority.
    #[serde(default)]
    daemon_session_token: Option<String>,
    #[serde(default)]
    daemon_session_expires_unix_ms: Option<u64>,
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
    user_id: Option<String>,
    #[serde(default)]
    account_name: Option<String>,
    #[serde(default)]
    claim_id: Option<String>,
    #[serde(default)]
    challenge: Option<String>,
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
/// provenance only ("linked to @handle", acknowledged by this daemon's
/// own key) and the mismatch detector for a service that later asserts an
/// association this daemon did not most recently acknowledge.
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

/// How the daemon's local signed claim record relates to the account link
/// the service currently asserts. None of these states grants authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ClaimBinding {
    /// The local signed acknowledgment matches the service-asserted link.
    DaemonSigned,
    /// No local co-signed record to check against — the binding rests on
    /// the service's assertion (a v1-era claim, or one acknowledged
    /// before this daemon kept records).
    ServiceAsserted,
    /// The service asserts a link this daemon never co-signed (or a
    /// different one than it did) — a re-bind worth the operator's eyes.
    Mismatch,
}

/// Snapshot of the Connect client for the trusted Access card.
/// Single writer (the client loop plus the start/stop manager); the
/// gateway only snapshots. Deliberately NOT part of the control-plane
/// state broadcast: the one-time route-link code must never ride general
/// frontend state snapshots.
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

struct RouteClaimCode {
    phrase: String,
    created_at: Instant,
}

fn route_claim_code_registry() -> &'static Mutex<Option<RouteClaimCode>> {
    static REGISTRY: OnceLock<Mutex<Option<RouteClaimCode>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

/// Mint the route-link code on the daemon, outside hosted-origin JavaScript.
/// It is display-only metadata: knowing it can associate this route with a
/// Connect account, but cannot create a daemon principal or grant.
fn current_route_claim_code() -> Result<String, String> {
    let mut slot = route_claim_code_registry()
        .lock()
        .expect("route claim code poisoned");
    if slot
        .as_ref()
        .is_some_and(|code| code.created_at.elapsed() >= CLAIM_CODE_TTL)
    {
        *slot = None;
    }
    if slot.is_none() {
        let phrase = generate_route_claim_code()?;
        *slot = Some(RouteClaimCode {
            phrase,
            created_at: Instant::now(),
        });
    }
    Ok(slot
        .as_ref()
        .expect("route claim code initialized")
        .phrase
        .clone())
}

fn generate_route_claim_code() -> Result<String, String> {
    let mut entropy = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut entropy)
        .map_err(|error| format!("route claim code entropy unavailable: {error:?}"))?;
    bip39::Mnemonic::from_entropy(&entropy)
        .map_err(|error| format!("route claim code generation failed: {error}"))
        .map(|mnemonic| mnemonic.to_string().replace(' ', "-"))
}

fn peek_route_claim_code() -> Option<String> {
    route_claim_code_registry()
        .lock()
        .expect("route claim code poisoned")
        .as_ref()
        .map(|code| code.phrase.clone())
}

fn clear_route_claim_code() {
    route_claim_code_registry()
        .lock()
        .expect("route claim code poisoned")
        .take();
}

fn daemon_session_registry() -> &'static Mutex<Option<String>> {
    static REGISTRY: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

fn set_daemon_session(token: Option<String>) {
    *daemon_session_registry()
        .lock()
        .expect("daemon session credential poisoned") = token;
}

fn daemon_session_snapshot() -> Option<String> {
    daemon_session_registry()
        .lock()
        .expect("daemon session credential poisoned")
        .clone()
}

/// Mirrors `normalize_claim_code` in `intendant-connect` and the hosted claim
/// page: lowercase alphanumeric runs joined by `-`.
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

fn claim_code_hash(code: &str) -> String {
    crate::daemon_identity::b64u(
        ring::digest::digest(&ring::digest::SHA256, normalize_claim_code(code).as_bytes()).as_ref(),
    )
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
    hosted_control: Option<Arc<crate::access::hosted_control::HostedControlRuntime>>,
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
            hosted_control: None,
            gateway_tcp_port: None,
        })
    })
}

pub fn spawn_connect_rendezvous_client(
    config: ConnectConfig,
    dashboard_control: Arc<DashboardControlRegistry>,
    gateway_tcp_port: Option<u16>,
    hosted_control: Arc<crate::access::hosted_control::HostedControlRuntime>,
) {
    {
        let mut state = client_state()
            .lock()
            .expect("connect client state poisoned");
        state.dashboard_control = Some(dashboard_control.clone());
        state.hosted_control = Some(Arc::clone(&hosted_control));
        state.gateway_tcp_port = gateway_tcp_port;
    }
    start_client(config, dashboard_control, gateway_tcp_port, hosted_control);
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
    set_daemon_session(None);
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
    let (dashboard_control, gateway_tcp_port, hosted_control) = {
        let state = client_state()
            .lock()
            .expect("connect client state poisoned");
        (
            state.dashboard_control.clone(),
            state.gateway_tcp_port,
            state.hosted_control.clone(),
        )
    };
    let dashboard_control = dashboard_control
        .ok_or_else(|| "connect client cannot start before the web gateway".to_string())?;
    let hosted_control = hosted_control.ok_or_else(|| {
        "connect client cannot start before hosted control is initialized".to_string()
    })?;
    start_client(config, dashboard_control, gateway_tcp_port, hosted_control);
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
    hosted_control: Arc<crate::access::hosted_control::HostedControlRuntime>,
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
        run_connect_rendezvous_client(
            config,
            base_url,
            dashboard_control,
            gateway_tcp_port,
            hosted_control,
        )
        .await;
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
    hosted_control: Arc<crate::access::hosted_control::HostedControlRuntime>,
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
        match register(
            &client,
            &base_url,
            &config,
            &daemon_id,
            &identity,
            &hosted_control,
        )
        .await
        {
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
                            // Fall through to the refresh check below: a
                            // continuous event stream must not starve the
                            // periodic re-register (daemon_session_token
                            // rotation + claim-code refresh), which used to
                            // be reachable only after an empty poll.
                            false
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
                match register(
                    &client,
                    &base_url,
                    &config,
                    &daemon_id,
                    &identity,
                    &hosted_control,
                )
                .await
                {
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
/// service-asserted account link against the daemon's own signed record,
/// and print the claim line when the code actually changed (the old
/// every-60s repeat was log noise; the current code is always visible in
/// the Access card).
fn note_register_response(response: &RegisterResponse, base_url: &Url) {
    let now = crate::access::client_key::now_unix_ms();
    let mut print_claim: Option<String> = None;
    let daemon_session_token = response.daemon_session_token.clone().filter(|_| {
        response
            .daemon_session_expires_unix_ms
            .is_none_or(|expires| expires > now.max(0) as u64)
    });
    set_daemon_session(daemon_session_token);
    if response.claimed {
        clear_route_claim_code();
    }
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
            status.claim_binding =
                Some(match (&status.signed_claim, &response.claimed_by_user_id) {
                    (Some(record), Some(asserted)) if record.account_user_id == *asserted => {
                        ClaimBinding::DaemonSigned
                    }
                    (Some(_), Some(_)) => ClaimBinding::Mismatch,
                    // An older service asserts no linked account id — nothing to
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
            // New services retain only the daemon-minted hash and return
            // null plaintext fields. Older services ignore that hash, mint
            // their own plaintext code, and return the code/URL pair that
            // they can actually redeem. Treat any legacy fields as a
            // coherent pair; mixing their URL or code with the local phrase
            // would display an unclaimable credential during a rolling
            // service upgrade.
            let local_code = peek_route_claim_code();
            let (effective_code, effective_url) =
                if response.claim_code.is_some() || response.claim_url.is_some() {
                    (response.claim_code.clone(), response.claim_url.clone())
                } else {
                    let local_url = local_code
                        .as_deref()
                        .map(|code| route_claim_url(base_url, code));
                    (local_code, local_url)
                };
            if status.claim_code != effective_code {
                print_claim = match (&effective_url, &effective_code) {
                    (Some(url), _) if !url.is_empty() => Some(format!(
                        "one-time claim code: link this daemon at {url}. Linking changes no \
                             IAM and grants no access"
                    )),
                    (_, Some(code)) if !code.is_empty() => Some(format!(
                        "one-time claim code {code}. Linking changes no IAM and grants no access"
                    )),
                    _ => None,
                };
            }
            status.claim_code = effective_code;
            status.claim_url = effective_url;
            status.claim_code_expires_unix_ms = response.claim_code_expires_unix_ms;
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
    let result = daemon_authenticated(config, client.post(url))
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
    identity: &DaemonIdentity,
    hosted_control: &crate::access::hosted_control::HostedControlRuntime,
) -> Result<RegisterResponse, RegisterError> {
    let daemon_public_key = identity.public_key_b64u();
    let claim_code = current_route_claim_code().map_err(RegisterError::Transient)?;
    let claim_code_hash = claim_code_hash(&claim_code);
    let issued_at_unix_ms = crate::access::client_key::now_unix_ms().max(0) as u64;
    let payload = registration_signing_payload(
        daemon_id,
        &daemon_public_key,
        &claim_code_hash,
        issued_at_unix_ms,
    );
    let hosted_control_payload = hosted_control_capability_signing_payload(
        daemon_id,
        &daemon_public_key,
        &claim_code_hash,
        issued_at_unix_ms,
        config.hosted_control_enabled,
    );
    let fleet_certificate_ledger = if config.hosted_control_enabled {
        hosted_control.certificate_ledger().ok().filter(|ledger| {
            ledger.daemon_id == daemon_id && ledger.daemon_public_key == daemon_public_key
        })
    } else {
        None
    };
    let request = RegisterRequest {
        protocol: "intendant-connect-rendezvous-v1",
        daemon_id: daemon_id.to_string(),
        daemon_public_key,
        claim_code_hash,
        issued_at_unix_ms,
        signature: identity.sign_b64u(payload.as_bytes()),
        hosted_control_enabled: config.hosted_control_enabled,
        hosted_control_signature: identity.sign_b64u(hosted_control_payload.as_bytes()),
        fleet_certificate_ledger,
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
    let response = daemon_authenticated(config, client.get(url))
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

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
async fn handle_event(
    client: &Client,
    base_url: &Url,
    config: &ConnectConfig,
    daemon_id: &str,
    identity: &DaemonIdentity,
    _dashboard_control: &Arc<DashboardControlRegistry>,
    _gateway_tcp_port: Option<u16>,
    event: RendezvousEvent,
) {
    // Defense in depth for mixed-version and self-hosted deployments: the
    // current service never enqueues these events, but an older service can.
    // Refuse all three control verbs before consulting the registry. In
    // particular, `close` must not be allowed to tear down a local/direct-mTLS
    // session whose id a hosted relay guessed or replayed, and `ice` must not
    // mutate an existing peer's candidate state.
    if let Some(error) = hosted_control_event_refusal(&event) {
        let _ = post_error(client, base_url, config, daemon_id, &event.id, &error).await;
        return;
    }

    match event.kind.as_str() {
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
                let _ = post_claim_error(
                    client,
                    base_url,
                    config,
                    daemon_id,
                    &event.id,
                    claim_id,
                    "missing claim challenge",
                )
                .await;
                return;
            };
            // Sign v2 (account-bound) whenever the challenge names the
            // linked account, and keep a local record of that route-only
            // acknowledgment. This is automatic service metadata, not
            // local human confirmation. v1 remains for older services.
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
            match daemon_authenticated(
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
                            "[connect] route link acknowledged for {} — no IAM authority changed",
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

fn hosted_control_event_refusal(event: &RendezvousEvent) -> Option<String> {
    match event.kind.as_str() {
        "offer" => Some(match connect_dashboard_grant(
            event.user_id.as_deref(),
            event.account_name.as_deref(),
            None,
        ) {
            Err(error) => error,
            Ok(_) => "hosted Connect control is unavailable in the default build".to_string(),
        }),
        "ice" | "close" => Some(format!(
            "hosted Connect {} events are unavailable in the default build; route metadata grants no daemon-control authority",
            event.kind
        )),
        _ => None,
    }
}

fn connect_dashboard_grant(
    user_id: Option<&str>,
    account_name: Option<&str>,
    client_key: Option<&crate::access::client_key::VerifiedClientKey>,
) -> Result<crate::dashboard_control::DashboardControlGrant, String> {
    connect_dashboard_grant_refusal(user_id, account_name, client_key)
}

fn connect_dashboard_grant_refusal(
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
    Err(connect_account_not_authorized_message(
        user_id,
        account_name,
        client_key,
        Some(
            "the default build uses hosted Connect for discovery and route metadata only; no hosted account, browser key, or IAM grant can authorize daemon control",
        ),
    ))
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
        "{identity} is not authorized for daemon control through hosted Connect. The default build uses Connect for discovery and route metadata only; open this daemon through local access or independently verified direct mTLS. No signed/notarized native release exists for this alpha."
    );
    if let Some(key) = client_key {
        message.push_str(&format!(
            " The presented browser key fingerprint is {} (informational only; hosted key grants are never exercised).",
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

/// Mirrors `registration_signing_payload` in `intendant-connect`.
fn registration_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    claim_code_hash: &str,
    issued_at_unix_ms: u64,
) -> String {
    format!(
        "{REGISTER_PROOF_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{claim_code_hash}\n{issued_at_unix_ms}\n"
    )
}

/// Optional discovery hint for the Connect directory. The daemon still
/// validates the flag and every lease at its own gateway; this signature binds
/// the displayed capability state to the exact registration fields.
fn hosted_control_capability_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    claim_code_hash: &str,
    issued_at_unix_ms: u64,
    enabled: bool,
) -> String {
    format!(
        "{HOSTED_CONTROL_CAPABILITY_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{claim_code_hash}\n{issued_at_unix_ms}\n{enabled}\n"
    )
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

/// A fragment is interpreted only by the browser and is never included in
/// the HTTP request, proxy logs, or referrer. The hosted page hashes the code
/// locally before calling the claim API.
fn route_claim_url(base_url: &Url, code: &str) -> String {
    format!("{}/connect#claim_code={code}", base_origin(base_url))
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
/// Relay-mode DNS publish: "answer my fleet label with the relay's address"
/// (mirrors `bin/connect/relay.rs`; same signing discipline as the fleet-DNS
/// publishes).
pub(crate) const DNS_RELAY_PROTOCOL: &str = "intendant-connect-dns-relay-v1";
/// Persistent relay control-channel long-poll protocol (mirrors
/// `bin/connect/relay.rs`).
pub(crate) const RELAY_CONTROL_PROTOCOL: &str = "intendant-connect-relay-control-v1";

#[cfg(test)] // golden-test twin of the payload `dns_signed_post` builds inline
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

#[cfg(test)] // golden-test twin of the payload `dns_signed_post` builds inline
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
/// DNS, reachability relay, attention nudges): effective config, rendezvous
/// URL, identity, and the effective daemon id.
pub(crate) fn signed_daemon_context() -> Result<(ConnectConfig, Url, DaemonIdentity, String), String>
{
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

/// Relay-mode DNS publish: ask the rendezvous to answer this daemon's fleet
/// label with the reachability relay's address (`enable = true`) so the name
/// resolves to the relay, or revert to direct address publishing
/// (`enable = false`). Best-effort: only meaningful when the rendezvous runs
/// both fleet DNS and the relay.
pub(crate) async fn dns_publish_via_relay(enable: bool) -> Result<(), String> {
    dns_signed_post(
        "api/dns/relay",
        DNS_RELAY_PROTOCOL,
        if enable { "1" } else { "0" },
        serde_json::json!({ "enable": enable }),
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

/// Tell the rendezvous an owner-attention request (closed `kind`
/// vocabulary) has gone unseen, so it can Web-Push the linked account's
/// opted-in browsers. Errors are expected weather for unlinked / offline daemons —
/// the caller degrades silently.
pub(crate) async fn notify_attention(kind: &str, session_label: &str) -> Result<(), String> {
    // Only a route-linked daemon has an account to notify; the service would
    // refuse an unlinked nudge anyway, so skip the round-trip.
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
    daemon_authenticated(config, client.post(join_url(base_url, "api/daemon/error")?))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub(crate) fn authenticated(config: &ConnectConfig, builder: RequestBuilder) -> RequestBuilder {
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

fn daemon_authenticated(config: &ConnectConfig, builder: RequestBuilder) -> RequestBuilder {
    let builder = authenticated(config, builder);
    match daemon_session_snapshot() {
        Some(token) => builder.header("x-intendant-daemon-session", token),
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

    fn hosted_event_test_registry(
        root: &std::path::Path,
    ) -> (
        Arc<DashboardControlRegistry>,
        crate::web_gateway::DashboardTabsRegistry,
    ) {
        let (broadcast_tx, _) = tokio::sync::broadcast::channel::<String>(16);
        let tabs = crate::web_gateway::DashboardTabsRegistry::new(
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(crate::web_gateway::DisplayInputAuthority::default()),
        );
        let registry = Arc::new(DashboardControlRegistry::new(
            crate::web_gateway::WebGatewayConfig::default(),
            broadcast_tx,
            crate::event::EventBus::new(),
            None,
            None,
            crate::web_gateway::ActiveSessionState::empty(),
            Some(root.to_path_buf()),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(crate::terminal::TerminalRegistry::new(root.to_path_buf())),
            None,
            serde_json::json!({"id": "intendant:test-hosted-refusal"}),
            crate::dashboard_control::DashboardBootstrapCaches::default(),
            None,
            None,
            crate::display::IceConfig::default(),
            crate::display::webrtc::TcpPeerRegistry::new(),
            tabs.clone(),
        ));
        (registry, tabs)
    }

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

    #[tokio::test]
    async fn hosted_offer_ice_and_close_leave_control_iam_and_enrollment_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, tabs) = hosted_event_test_registry(temp.path());
        let session_id = "trusted-direct-session";
        tabs.register(
            session_id,
            crate::web_gateway::DashboardTabConnection {
                lane: crate::web_gateway::DashboardTabLane::ControlTunnel,
                kind: "local",
                label: "trusted-local".to_string(),
                tab_id: Some("trusted-tab-1234".to_string()),
                remote: Some("127.0.0.1".to_string()),
                user_agent: Some("test".to_string()),
                connected_at_unix_ms: 1,
            },
        );
        let tabs_before = tabs.snapshot();

        // Pin deliberately unnormalized hostile legacy bytes in an isolated
        // IAM store. Using `save_state` here would normalize the ceiling to
        // role:none before the test starts and make this adversarial fixture
        // vacuous. Connect event handling has no path to this store (or any
        // IAM store), so the exact hostile bytes must survive every refused
        // control verb.
        let iam_dir = temp.path().join("access");
        std::fs::create_dir_all(&iam_dir).unwrap();
        let iam_path = crate::access::iam::iam_state_path(&iam_dir);
        let iam_before = br#"{"schema_version":1,"role_ceilings":{"connect_account":"role:root","client_key":"role:root"},"grants":[{"id":"grant:legacy-connect-root","principal_id":"principal:legacy","target_id":"local","role_id":"role:root","policy_id":"policy:root","status":"active","source":"connect-bootstrap","reason":"hostile fixture"}]}"#.to_vec();
        std::fs::write(&iam_path, &iam_before).unwrap();

        // Use a unique sentinel instead of clearing the process-global queue,
        // so this regression remains hermetic under parallel unit tests.
        let enrollment_fingerprint = "fp-hosted-control-refusal-sentinel";
        let now = crate::access::client_key::now_unix_ms();
        crate::access::enrollment::record_refused_client_key(
            enrollment_fingerprint,
            "sentinel-public-key",
            "https://trusted.example",
            "direct",
            "sentinel",
            false,
            now,
        );
        let enrollment_before = crate::access::enrollment::pending_enrollments(now)
            .into_iter()
            .find(|entry| entry.fingerprint == enrollment_fingerprint)
            .unwrap();

        // Bind and release a port to get a deterministic local connection
        // refusal for the best-effort error report; no network leaves the
        // test process and event refusal itself does not depend on the reply.
        let dead = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base_url = Url::parse(&format!(
            "http://127.0.0.1:{}/",
            dead.local_addr().unwrap().port()
        ))
        .unwrap();
        drop(dead);
        let client = Client::builder()
            .timeout(Duration::from_millis(250))
            .build()
            .unwrap();
        let config = ConnectConfig::default();
        let identity = DaemonIdentity::load_or_create(temp.path().join("identity.pk8")).unwrap();

        // Unknown legacy fields intentionally deserialize away. Their mere
        // presence must not revive the retired authorization paths.
        let events = [
            serde_json::from_value::<RendezvousEvent>(serde_json::json!({
                "id": "offer-1",
                "kind": "offer",
                "session_id": session_id,
                "sdp": "legacy-offer",
                "user_id": "user-1",
                "account_name": "mallory",
                "client_key": "legacy-hosted-key",
                "org_grant": {"role_id": "role:root"}
            }))
            .unwrap(),
            serde_json::from_value::<RendezvousEvent>(serde_json::json!({
                "id": "ice-1",
                "kind": "ice",
                "session_id": session_id,
                "candidate": { "candidate": "candidate:legacy-hosted" }
            }))
            .unwrap(),
            serde_json::from_value::<RendezvousEvent>(serde_json::json!({
                "id": "close-1",
                "kind": "close",
                "session_id": session_id
            }))
            .unwrap(),
        ];

        for event in events {
            assert!(hosted_control_event_refusal(&event).is_some());
            handle_event(
                &client, &base_url, &config, "daemon-1", &identity, &registry, None, event,
            )
            .await;
            assert_eq!(tabs.snapshot(), tabs_before);
            assert_eq!(std::fs::read(&iam_path).unwrap(), iam_before);
            let enrollment_after = crate::access::enrollment::pending_enrollments(now)
                .into_iter()
                .find(|entry| entry.fingerprint == enrollment_fingerprint)
                .unwrap();
            assert_eq!(enrollment_after, enrollment_before);
        }

        let _ = crate::access::enrollment::take_enrollment(enrollment_fingerprint);
    }

    #[test]
    fn connect_account_metadata_is_always_discovery_only() {
        let error = connect_dashboard_grant(Some("user-123"), Some("alice"), None).unwrap_err();
        assert!(
            error.contains("discovery and route metadata only"),
            "{error}"
        );
        assert!(error.contains("no hosted account"), "{error}");
    }

    #[test]
    fn unmatched_connect_account_metadata_is_discovery_only() {
        let error = connect_dashboard_grant(Some("user-123"), Some("alice"), None).unwrap_err();
        assert!(error.contains("@alice"));
        assert!(error.contains("discovery and route metadata only"));
        assert!(error.contains("direct mTLS"));
    }

    #[test]
    fn connect_offer_without_account_identity_is_rejected() {
        let error = connect_dashboard_grant(None, None, None).unwrap_err();
        assert!(error.contains("not authorized"));
        assert!(error.contains("did not include account identity or a client key"));
    }

    #[test]
    fn verified_client_key_cannot_authorize_a_hosted_offer() {
        let key = crate::access::client_key::VerifiedClientKey {
            fingerprint: "fp-abc".to_string(),
            public_key_b64u: "unused".to_string(),
            attested_account: None,
        };
        let error =
            connect_dashboard_grant(Some("unknown-user"), Some("unknown"), Some(&key)).unwrap_err();
        assert!(
            error.contains("discovery and route metadata only"),
            "{error}"
        );
        assert!(
            error.contains("hosted key grants are never exercised"),
            "{error}"
        );
    }

    #[test]
    fn hosted_key_identity_never_selects_an_authority_resolver() {
        let key = crate::access::client_key::VerifiedClientKey {
            fingerprint: "fp-scoped".to_string(),
            public_key_b64u: "unused".to_string(),
            attested_account: None,
        };
        let error = connect_dashboard_grant(None, None, Some(&key)).unwrap_err();
        assert!(
            error.contains("discovery and route metadata only"),
            "{error}"
        );
        assert!(error.contains("no hosted account, browser key, or IAM grant"));
    }

    #[test]
    fn hosted_offer_refuses_root_and_legacy_bootstrap_keys() {
        for fingerprint in ["fp-root", "fp-legacy-connect-bootstrap"] {
            let key = crate::access::client_key::VerifiedClientKey {
                fingerprint: fingerprint.to_string(),
                public_key_b64u: "unused".to_string(),
                attested_account: None,
            };
            let error = connect_dashboard_grant(None, None, Some(&key)).unwrap_err();
            assert!(
                error.contains("discovery and route metadata only"),
                "{error}"
            );
        }
    }

    #[test]
    fn unmatched_client_key_reports_its_fingerprint() {
        let key = crate::access::client_key::VerifiedClientKey {
            fingerprint: "fp-unenrolled".to_string(),
            public_key_b64u: "unused".to_string(),
            attested_account: None,
        };
        let error = connect_dashboard_grant(None, None, Some(&key)).unwrap_err();
        assert!(error.contains("fp-unenrolled"));
        assert!(error.contains("informational only"));
    }

    /// Twin of `claim_and_unclaim_payloads_pin_the_wire_format` in
    /// `intendant-connect` — the two binaries replicate these formats
    /// rather than share code, so each side pins the same golden
    /// literals and drift fails a test instead of shipping as an
    /// unverifiable signature.
    #[test]
    fn claim_and_unclaim_payloads_pin_the_wire_format() {
        assert_eq!(
            registration_signing_payload("daemon-1", "PubKey", "ClaimHash", 1_700_000_000_000),
            "intendant-connect-register-proof-v1\ndaemon-1\nPubKey\nClaimHash\n1700000000000\n"
        );
        assert_eq!(
            hosted_control_capability_signing_payload(
                "daemon-1",
                "PubKey",
                "ClaimHash",
                1_700_000_000_000,
                true,
            ),
            "intendant-connect-hosted-control-capability-v1\ndaemon-1\nPubKey\nClaimHash\n1700000000000\ntrue\n"
        );
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

    #[test]
    fn route_claim_codes_are_local_twelve_word_bip39_values() {
        let code = generate_route_claim_code().unwrap();
        let words: Vec<_> = code.split('-').collect();
        assert_eq!(words.len(), 12);
        bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &words.join(" "))
            .expect("route claim code must be a valid BIP39 mnemonic");
        let hash = claim_code_hash(&code);
        assert_eq!(hash.len(), 43);
        assert!(hash
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
    }

    #[test]
    fn route_claim_url_keeps_the_phrase_out_of_the_request_target() {
        let base = Url::parse("https://connect.example/base?ignored=1").unwrap();
        let url = route_claim_url(&base, "word-word-word");
        assert_eq!(
            url,
            "https://connect.example/connect#claim_code=word-word-word"
        );
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.path(), "/connect");
        assert_eq!(parsed.query(), None);
        assert_eq!(parsed.fragment(), Some("claim_code=word-word-word"));
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

    /// A register response asserting a different account link than the daemon's
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
                claim_code_expires_unix_ms: None,
                claim_url: None,
                daemon_session_token: None,
                daemon_session_expires_unix_ms: None,
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
                claim_code_expires_unix_ms: None,
                claim_url: None,
                daemon_session_token: None,
                daemon_session_expires_unix_ms: None,
                observed_ip: None,
                fleet_dns: None,
            },
            &base_url,
        );
        assert_eq!(
            status_snapshot().claim_binding,
            Some(ClaimBinding::Mismatch)
        );

        // During a mixed-version rollout an older service ignores the
        // daemon-minted hash and returns the different plaintext phrase it
        // can redeem. Its coherent response pair must outrank the local
        // phrase; otherwise the dashboard displays an unclaimable code.
        let local_code = current_route_claim_code().unwrap();
        assert_ne!(local_code, "word-word-word");
        note_register_response(
            &RegisterResponse {
                claimed: false,
                claimed_by_user_id: None,
                claimed_by_handle: None,
                claim_code: Some("word-word-word".to_string()),
                claim_code_expires_unix_ms: Some(1_700_000_600_000),
                claim_url: Some(
                    "https://connect.example/connect?claim_code=word-word-word".to_string(),
                ),
                daemon_session_token: None,
                daemon_session_expires_unix_ms: None,
                observed_ip: None,
                fleet_dns: None,
            },
            &base_url,
        );
        let status = status_snapshot();
        assert_eq!(status.claimed, Some(false));
        assert_eq!(status.claim_binding, None);
        assert_eq!(status.claim_code.as_deref(), Some("word-word-word"));
        assert_eq!(
            status.claim_url.as_deref(),
            Some("https://connect.example/connect?claim_code=word-word-word")
        );
        assert_eq!(status.claim_code_expires_unix_ms, Some(1_700_000_600_000));

        // A new service returns null plaintext fields because it retained the
        // signed local hash. In that case the same local phrase and its
        // fragment URL are the claim surface.
        note_register_response(
            &RegisterResponse {
                claimed: false,
                claimed_by_user_id: None,
                claimed_by_handle: None,
                claim_code: None,
                claim_code_expires_unix_ms: Some(1_700_001_200_000),
                claim_url: None,
                daemon_session_token: None,
                daemon_session_expires_unix_ms: None,
                observed_ip: None,
                fleet_dns: None,
            },
            &base_url,
        );
        let status = status_snapshot();
        assert_eq!(status.claim_code.as_deref(), Some(local_code.as_str()));
        let local_url = route_claim_url(&base_url, &local_code);
        assert_eq!(status.claim_url.as_deref(), Some(local_url.as_str()));
        assert_eq!(status.claim_code_expires_unix_ms, Some(1_700_001_200_000));

        // Leave no residue for other tests sharing the process-global
        // registry.
        clear_route_claim_code();
        with_status(|status| *status = ConnectStatus::default());
    }
}
