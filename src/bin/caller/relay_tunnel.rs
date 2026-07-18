//! Daemon-side reachability relay tunnel client.
//!
//! When the reachability relay is enabled (`[connect] relay_enabled` +
//! `relay_endpoint`, docs/src/self-hosted-rendezvous.md), the daemon holds a
//! persistent control channel to Connect so its NAT'd fleet name is reachable
//! through the relay's SNI passthrough:
//!
//!   - Long-poll `POST /api/relay/next` on the Connect HTTP API, authenticated
//!     by the daemon identity key with the same signed/freshness discipline as
//!     the fleet-DNS publishes. Each successful poll re-registers the tunnel.
//!   - On a dial-back request (a single-use nonce), open a raw TCP connection
//!     to the relay's passthrough port, announce the nonce, connect to this
//!     daemon's dedicated loopback-only relay ingress, and splice bytes
//!     between them. The gateway tags connections accepted there as
//!     reachability-relay provenance before TLS/HTTP parsing, so the local
//!     dial-back hop cannot inherit trusted-local authority. The browser's TLS
//!     still completes end-to-end against this daemon's fleet certificate; the
//!     relay only ever moves ciphertext.
//!   - Publish relay-mode fleet DNS so the fleet name resolves to the relay.
//!
//! No new authority: the dedicated ingress itself is discovery-only, and the
//! fleet SNI remains an independent second gate. The relay changes
//! reachability, not trust.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::{net::SocketAddr, time::Duration};

use futures_util::StreamExt as _;
use reqwest::{Client, Url};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinSet;

use crate::connect_rendezvous::{
    authenticated, dns_publish_addresses, dns_publish_addresses_for_config, dns_publish_via_relay,
    join_url, signed_daemon_context_for_config, RELAY_CONTROL_PROTOCOL, RELAY_CONTROL_PROTOCOL_V1,
    RELAY_DISCONNECT_PROTOCOL,
};
use crate::daemon_identity::DaemonIdentity;
use crate::project::ConnectConfig;

const RELAY_NAME_PROOF_PROTOCOL: &str = "intendant-connect-relay-name-proof-v1";

#[derive(serde::Serialize)]
struct RelayServerNameProof {
    server_name: String,
    certificate_chain_pem: String,
    signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RelayDialback {
    nonce: String,
    source_bucket: Option<String>,
}

/// The first line the daemon writes on a dial-back data connection (mirrors
/// `bin/connect/relay.rs`): this magic and the single-use nonce.
const DIALBACK_MAGIC: &str = "ITRLY1";
/// The daemon-side tunnel writes this availability-only source hint before
/// the browser's TLS bytes on the dedicated loopback relay ingress.
pub(crate) const GATEWAY_RELAY_SOURCE_MAGIC: &str = "ITGWS1";
pub(crate) const GATEWAY_RELAY_SOURCE_MAX_BYTES: usize = 64;
/// Control long-poll timeout requested of the relay.
const CONTROL_POLL_TIMEOUT_MS: u64 = 15_000;
const CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_RESPONSE_MAX_BYTES: usize = 64 * 1024;
/// Reconnect backoff bounds after control-channel errors.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How often to re-assert relay-mode fleet DNS while the tunnel runs, so the
/// fleet name keeps resolving to the relay (the DNS record TTL is short).
const DNS_REASSERT_INTERVAL: Duration = Duration::from_secs(240);
/// Idle teardown + per-direction byte cap on a spliced dial-back connection.
const SPLICE_IDLE: Duration = Duration::from_secs(120);
const SPLICE_MAX_BYTES: u64 = 512 * 1024 * 1024;
const DIALBACK_SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ACTIVE_DIALBACKS: usize = 64;

fn dialback_capacity_available(active: usize) -> bool {
    active < MAX_ACTIVE_DIALBACKS
}

/// Start the relay tunnel client when the config opts in. Whenever a
/// rendezvous destination is configured, the DNS reconciler starts even if
/// the relay is disabled or invalid: a previous process may have left
/// relay-mode DNS published, so startup must explicitly withdraw it.
/// `gateway_ingress_addr` is the gateway's dedicated loopback-only relay
/// listener. It serves the fleet-certificate handshake while preserving
/// immutable relay provenance at the accept edge.
pub fn spawn_relay_tunnel_client(
    config: ConnectConfig,
    gateway_ingress_addr: Option<SocketAddr>,
    current_fleet_zone_observed: Arc<AtomicBool>,
    lifecycle: watch::Receiver<bool>,
) {
    let (readiness_tx, readiness_rx) = watch::channel(false);
    if has_rendezvous_destination(&config) {
        let dns_config = config.clone();
        let dns_lifecycle = lifecycle.clone();
        let readiness_guard = readiness_tx.clone();
        tokio::spawn(relay_dns_reassert_loop(
            dns_config,
            dns_lifecycle,
            readiness_rx,
            readiness_guard,
        ));
    }
    if !config.relay_enabled {
        return;
    }
    let Some(endpoint) = config
        .relay_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        eprintln!("[relay] tunnel enabled but no relay_endpoint is configured");
        return;
    };
    let Some(gateway_ingress_addr) = gateway_ingress_addr else {
        eprintln!("[relay] tunnel enabled but its dedicated gateway ingress is unavailable");
        return;
    };
    let tunnel_lifecycle = lifecycle.clone();
    tokio::spawn(run_relay_tunnel(
        config,
        endpoint,
        gateway_ingress_addr,
        current_fleet_zone_observed,
        tunnel_lifecycle,
        readiness_tx,
    ));
}

fn has_rendezvous_destination(config: &ConnectConfig) -> bool {
    config
        .rendezvous_url
        .as_deref()
        .map(str::trim)
        .is_some_and(|url| !url.is_empty())
}

#[derive(Default)]
struct RelayDnsPublicationState {
    relay_active: bool,
}

fn relay_dns_publication_state() -> &'static Mutex<RelayDnsPublicationState> {
    static STATE: OnceLock<Mutex<RelayDnsPublicationState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(RelayDnsPublicationState::default()))
}

/// Publish direct fleet addresses unless this process has successfully
/// established relay-mode DNS. The same coordinator serializes relay and
/// direct writes, so certificate renewal cannot overwrite a live relay route.
pub(crate) async fn publish_fleet_dns_addresses(
    addresses: &[String],
) -> Result<Vec<String>, String> {
    publish_fleet_dns_addresses_with(relay_dns_publication_state(), addresses, || async {
        dns_publish_addresses(addresses).await
    })
    .await
}

async fn publish_fleet_dns_addresses_with<D, DirectFuture>(
    publication: &Mutex<RelayDnsPublicationState>,
    addresses: &[String],
    direct_publish: D,
) -> Result<Vec<String>, String>
where
    D: FnOnce() -> DirectFuture,
    DirectFuture: std::future::Future<Output = Result<Vec<String>, String>>,
{
    let state = publication.lock().await;
    if state.relay_active {
        return Ok(addresses.to_vec());
    }
    direct_publish().await
}

async fn relay_dns_reassert_loop(
    config: ConnectConfig,
    lifecycle: watch::Receiver<bool>,
    readiness: watch::Receiver<bool>,
    readiness_guard: watch::Sender<bool>,
) {
    let publish_config = config.clone();
    relay_dns_reassert_loop_with(
        config.relay_enabled,
        lifecycle,
        readiness,
        readiness_guard,
        move |enable| {
            let config = publish_config.clone();
            let direct_config = config.clone();
            async move {
                publish_relay_dns_state_coordinated_with(
                    relay_dns_publication_state(),
                    enable,
                    move |enable| async move { dns_publish_via_relay(&config, enable).await },
                    move || async move {
                        let status = crate::fleet_cert::status_snapshot();
                        let addresses = if status.addresses.is_empty() {
                            crate::fleet_cert::default_publish_addresses()
                        } else {
                            status.addresses
                        };
                        dns_publish_addresses_for_config(&direct_config, &addresses)
                            .await
                            .map(|_| ())
                    },
                )
                .await
            }
        },
    )
    .await;
}

async fn publish_relay_dns_state_coordinated_with<R, RelayFuture, D, DirectFuture>(
    publication: &Mutex<RelayDnsPublicationState>,
    enable: bool,
    relay_publish: R,
    direct_publish: D,
) -> Result<(), String>
where
    R: FnOnce(bool) -> RelayFuture,
    RelayFuture: std::future::Future<Output = Result<(), String>>,
    D: FnOnce() -> DirectFuture,
    DirectFuture: std::future::Future<Output = Result<(), String>>,
{
    let mut state = publication.lock().await;
    if !enable {
        // This is the desired state even when the remote withdrawal result
        // is ambiguous. A later direct publication may safely reinforce it,
        // and the reconciler keeps retrying the full withdrawal transition.
        state.relay_active = false;
    }
    let result = publish_relay_dns_state_with(enable, relay_publish, direct_publish).await;
    if enable && result.is_ok() {
        state.relay_active = true;
    }
    result
}

async fn publish_relay_dns_state_with<R, RelayFuture, D, DirectFuture>(
    enable: bool,
    relay_publish: R,
    direct_publish: D,
) -> Result<(), String>
where
    R: FnOnce(bool) -> RelayFuture,
    RelayFuture: std::future::Future<Output = Result<(), String>>,
    D: FnOnce() -> DirectFuture,
    DirectFuture: std::future::Future<Output = Result<(), String>>,
{
    relay_publish(enable).await?;
    if !enable {
        direct_publish().await?;
    }
    Ok(())
}

/// Best-effort DNS state machine. Relay-mode publication is allowed only after
/// a successful control poll established this process's tunnel registration.
/// All other states converge toward an explicit withdrawal followed by direct
/// address publication.
async fn relay_dns_reassert_loop_with<F, Fut>(
    relay_configured: bool,
    mut lifecycle: watch::Receiver<bool>,
    mut readiness: watch::Receiver<bool>,
    _readiness_guard: watch::Sender<bool>,
    mut publish: F,
) where
    F: FnMut(bool) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    // A prior process may have exited after publishing relay mode. Begin
    // conservatively so disabled startup also sends one explicit withdrawal.
    let mut relay_dns_may_be_published = true;
    let mut withdrawal_backoff = BACKOFF_MIN;
    loop {
        let should_publish =
            relay_configured && *lifecycle.borrow_and_update() && *readiness.borrow_and_update();
        if !should_publish {
            if relay_dns_may_be_published {
                match publish(false).await {
                    Ok(()) => {
                        relay_dns_may_be_published = false;
                        withdrawal_backoff = BACKOFF_MIN;
                    }
                    Err(error) => {
                        eprintln!("[relay] relay-mode dns withdrawal (best-effort): {error}");
                        tokio::select! {
                            biased;
                            changed = lifecycle.changed() => {
                                if changed.is_err() {
                                    return;
                                }
                            }
                            changed = readiness.changed() => {
                                if changed.is_err() {
                                    return;
                                }
                            }
                            _ = tokio::time::sleep(withdrawal_backoff) => {
                                withdrawal_backoff =
                                    (withdrawal_backoff * 2).min(BACKOFF_MAX);
                            }
                        }
                    }
                }
                continue;
            }
            tokio::select! {
                changed = lifecycle.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                changed = readiness.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
            continue;
        }
        withdrawal_backoff = BACKOFF_MIN;
        // Once the signed request begins its remote effect is ambiguous until
        // an explicit withdrawal succeeds, even if local cancellation wins.
        relay_dns_may_be_published = true;
        let publish = tokio::select! {
            biased;
            changed = lifecycle.changed() => {
                if changed.is_err() {
                    return;
                }
                None
            }
            changed = readiness.changed() => {
                if changed.is_err() {
                    return;
                }
                None
            }
            result = publish(true) => Some(result),
        };
        let Some(publish) = publish else {
            continue;
        };
        if let Err(error) = publish {
            eprintln!("[relay] relay-mode dns publish (best-effort): {error}");
        }
        if !relay_configured || !*lifecycle.borrow_and_update() || !*readiness.borrow_and_update() {
            continue;
        }
        tokio::select! {
            biased;
            changed = lifecycle.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            changed = readiness.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = tokio::time::sleep(DNS_REASSERT_INTERVAL) => {}
        }
    }
}

async fn run_relay_tunnel(
    config: ConnectConfig,
    relay_endpoint: String,
    gateway_ingress_addr: SocketAddr,
    current_fleet_zone_observed: Arc<AtomicBool>,
    mut lifecycle: watch::Receiver<bool>,
    readiness: watch::Sender<bool>,
) {
    let client = match Client::builder()
        .timeout(Duration::from_millis(CONTROL_POLL_TIMEOUT_MS + 10_000))
        .connect_timeout(CONTROL_CONNECT_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[relay] failed to build tunnel HTTP client: {error}");
            return;
        }
    };
    eprintln!("[relay] tunnel client enabled (dial-back via {relay_endpoint})");
    loop {
        while !*lifecycle.borrow_and_update() {
            readiness.send_replace(false);
            if lifecycle.changed().await.is_err() {
                return;
            }
        }
        let poller_id = uuid::Uuid::new_v4().simple().to_string();
        run_enabled_relay_tunnel(
            &config,
            &relay_endpoint,
            gateway_ingress_addr,
            &current_fleet_zone_observed,
            &client,
            &poller_id,
            &mut lifecycle,
            &readiness,
        )
        .await;
        readiness.send_replace(false);
        if let Err(error) = withdraw_relay_name_registration(&config, &client, &poller_id).await {
            eprintln!("[relay] exact-name withdrawal (best-effort): {error}");
        }
        if lifecycle.has_changed().is_err() {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_enabled_relay_tunnel(
    config: &ConnectConfig,
    relay_endpoint: &str,
    gateway_ingress_addr: SocketAddr,
    current_fleet_zone_observed: &AtomicBool,
    client: &Client,
    poller_id: &str,
    lifecycle: &mut watch::Receiver<bool>,
    readiness: &watch::Sender<bool>,
) {
    let mut backoff = BACKOFF_MIN;
    let mut last_custom_error: Option<String> = None;
    let mut dialbacks = JoinSet::new();
    loop {
        if !*lifecycle.borrow_and_update() {
            readiness.send_replace(false);
            return;
        }
        while dialbacks.try_join_next().is_some() {}
        // Another daemon process may have renewed the shared fleet pair.
        // Relay readiness is process-local, so reload it before registering
        // this poller as a possible dialback recipient.
        crate::fleet_cert::refresh_installed_state();
        // Resolve identity material fresh each cycle, but keep the complete
        // relay configuration generation fixed. A live Connect destination
        // change must not pair a nonce from a new control plane with the
        // boot-captured raw dial-back endpoint.
        let (control_config, base_url, identity, daemon_id) =
            match signed_daemon_context_for_config(config.clone()) {
                Ok(context) => context,
                Err(error) => {
                    readiness.send_replace(false);
                    eprintln!("[relay] tunnel context unavailable: {error}");
                    backoff = match sleep_backoff_or_disabled(backoff, lifecycle).await {
                        Some(next) => next,
                        None => return,
                    };
                    continue;
                }
            };
        let name_materials = if !current_fleet_zone_observed.load(Ordering::SeqCst) {
            Vec::new()
        } else {
            match crate::custom_domain::relay_certificate_material(&control_config.custom_domain) {
                Ok(Some(material)) => {
                    last_custom_error = None;
                    vec![material]
                }
                Ok(None) => {
                    last_custom_error = None;
                    Vec::new()
                }
                Err(error) => {
                    if last_custom_error.as_deref() != Some(error.as_str()) {
                        eprintln!(
                            "[relay] custom-domain registration unavailable; fleet relay remains active: {error}"
                        );
                        last_custom_error = Some(error);
                    }
                    Vec::new()
                }
            }
        };
        let poll = RelayPollContext {
            client,
            config: &control_config,
            base_url: &base_url,
            identity: &identity,
            daemon_id: &daemon_id,
            poller_id,
        };
        let poll_result = tokio::select! {
            biased;
            changed = lifecycle.changed() => {
                if changed.is_err() || !*lifecycle.borrow_and_update() {
                    None
                } else {
                    continue;
                }
            }
            result = poll_relay_next(&poll, &name_materials) => Some(result),
        };
        let Some(poll_result) = poll_result else {
            readiness.send_replace(false);
            return;
        };
        if !*lifecycle.borrow_and_update() {
            readiness.send_replace(false);
            return;
        }
        match poll_result {
            Ok(Some(dialback)) => {
                readiness.send_replace(true);
                backoff = BACKOFF_MIN;
                if !dialback_capacity_available(dialbacks.len()) {
                    // Keep polling so the server-side tunnel registration
                    // stays live; the relay closes this unclaimed nonce on
                    // its own bounded dialback deadline. Waiting here would
                    // let the 45-second registration lapse behind a
                    // 120-second active splice.
                    eprintln!("[relay] dial-back capacity reached; refusing one excess connection");
                } else {
                    let endpoint = relay_endpoint.to_string();
                    dialbacks.spawn(async move {
                        if let Err(error) =
                            handle_dialback(&endpoint, gateway_ingress_addr, &dialback).await
                        {
                            eprintln!("[relay] dial-back failed: {error}");
                        }
                    });
                }
            }
            Ok(None) => {
                readiness.send_replace(true);
                backoff = BACKOFF_MIN;
            }
            Err(error) => {
                readiness.send_replace(false);
                eprintln!("[relay] control poll failed: {error}");
                backoff = match sleep_backoff_or_disabled(backoff, lifecycle).await {
                    Some(next) => next,
                    None => return,
                };
            }
        }
    }
}

async fn sleep_backoff_or_disabled(
    current: Duration,
    lifecycle: &mut watch::Receiver<bool>,
) -> Option<Duration> {
    tokio::select! {
        _ = tokio::time::sleep(current) => Some((current * 2).min(BACKOFF_MAX)),
        changed = lifecycle.changed() => {
            if changed.is_err() || !*lifecycle.borrow_and_update() {
                None
            } else {
                Some(current)
            }
        }
    }
}

async fn withdraw_relay_name_registration(
    config: &ConnectConfig,
    client: &Client,
    poller_id: &str,
) -> Result<(), String> {
    let (control_config, base_url, identity, daemon_id) =
        signed_daemon_context_for_config(config.clone())?;
    let poll = RelayPollContext {
        client,
        config: &control_config,
        base_url: &base_url,
        identity: &identity,
        daemon_id: &daemon_id,
        poller_id,
    };
    let body = build_relay_disconnect_body(&poll);
    decode_relay_poll_response(send_relay_poll(&poll, &body).await?)
        .await
        .map(|_| ())
}

/// One control-channel long-poll. Returns the dial-back nonce if the relay
/// asked this daemon to dial back, `None` on an empty poll.
struct RelayPollContext<'a> {
    client: &'a Client,
    config: &'a ConnectConfig,
    base_url: &'a Url,
    identity: &'a DaemonIdentity,
    daemon_id: &'a str,
    poller_id: &'a str,
}

async fn poll_relay_next(
    poll: &RelayPollContext<'_>,
    name_materials: &[crate::custom_domain::RelayCertificateMaterial],
) -> Result<Option<RelayDialback>, String> {
    let v2_body = match build_relay_poll_body(poll, RELAY_CONTROL_PROTOCOL, name_materials) {
        Ok(body) => body,
        Err(error) if !name_materials.is_empty() => {
            eprintln!(
                "[relay] exact-name proof unavailable ({error}); retaining fleet-label compatibility via control v1"
            );
            let v1_body = build_relay_poll_body(poll, RELAY_CONTROL_PROTOCOL_V1, &[])?;
            return decode_relay_poll_response(send_relay_poll(poll, &v1_body).await?).await;
        }
        Err(error) => return Err(error),
    };
    let mut response = send_relay_poll(poll, &v2_body).await?;
    if !response.status().is_success() {
        let status = response.status();
        let text = relay_response_text_capped(response, "control rejection").await?;
        if exact_name_registration_rejected(status, &text) {
            eprintln!(
                "[relay] exact-name registration rejected ({status} {text}); retaining fleet-label compatibility via control v1"
            );
            let v1_body = build_relay_poll_body(poll, RELAY_CONTROL_PROTOCOL_V1, &[])?;
            response = send_relay_poll(poll, &v1_body).await?;
        } else {
            return Err(format!("relay control poll rejected: HTTP {status} {text}"));
        }
    }
    decode_relay_poll_response(response).await
}

fn exact_name_registration_rejected(status: reqwest::StatusCode, body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    match status {
        reqwest::StatusCode::CONFLICT => true,
        reqwest::StatusCode::BAD_REQUEST => {
            body.contains("unsupported protocol")
                || body.contains("relay server name")
                || body.contains("exact relay")
                || body.contains("server names")
        }
        reqwest::StatusCode::FORBIDDEN => {
            body.contains("relay certificate")
                || body.contains("certificate ownership proof")
                || body.contains("each exact relay name")
        }
        _ => false,
    }
}

fn build_relay_poll_body(
    poll: &RelayPollContext<'_>,
    protocol: &str,
    name_materials: &[crate::custom_domain::RelayCertificateMaterial],
) -> Result<serde_json::Value, String> {
    let daemon_public_key = poll.identity.public_key_b64u();
    let issued_at_unix_ms = crate::access::client_key::now_unix_ms().max(0) as u64;
    let server_names: Vec<String> = name_materials
        .iter()
        .map(|material| material.server_name.clone())
        .collect();
    let payload = relay_control_signing_payload(
        protocol,
        poll.daemon_id,
        &daemon_public_key,
        issued_at_unix_ms,
        (protocol == RELAY_CONTROL_PROTOCOL).then_some(poll.poller_id),
        &server_names,
    );
    let signature = poll.identity.sign_b64u(&payload);
    let mut body = serde_json::json!({
        "protocol": protocol,
        "daemon_id": poll.daemon_id,
        "daemon_public_key": daemon_public_key,
        "issued_at_unix_ms": issued_at_unix_ms,
        "signature": signature,
        "timeout_ms": CONTROL_POLL_TIMEOUT_MS,
    });
    if protocol == RELAY_CONTROL_PROTOCOL {
        body["poller_id"] = serde_json::Value::String(poll.poller_id.to_string());
        let proofs = name_materials
            .iter()
            .map(|material| {
                relay_server_name_proof(
                    material,
                    poll.daemon_id,
                    &daemon_public_key,
                    issued_at_unix_ms,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        body["server_names"] = serde_json::to_value(&server_names)
            .map_err(|error| format!("serialize relay server names: {error}"))?;
        body["server_name_proofs"] = serde_json::to_value(proofs)
            .map_err(|error| format!("serialize relay server-name proofs: {error}"))?;
    }
    Ok(body)
}

fn build_relay_disconnect_body(poll: &RelayPollContext<'_>) -> serde_json::Value {
    let daemon_public_key = poll.identity.public_key_b64u();
    let issued_at_unix_ms = crate::access::client_key::now_unix_ms().max(0) as u64;
    let payload = relay_control_signing_payload(
        RELAY_DISCONNECT_PROTOCOL,
        poll.daemon_id,
        &daemon_public_key,
        issued_at_unix_ms,
        Some(poll.poller_id),
        &[],
    );
    serde_json::json!({
        "protocol": RELAY_DISCONNECT_PROTOCOL,
        "daemon_id": poll.daemon_id,
        "daemon_public_key": daemon_public_key,
        "issued_at_unix_ms": issued_at_unix_ms,
        "signature": poll.identity.sign_b64u(&payload),
        "poller_id": poll.poller_id,
    })
}

async fn send_relay_poll(
    poll: &RelayPollContext<'_>,
    body: &serde_json::Value,
) -> Result<reqwest::Response, String> {
    authenticated(
        poll.config,
        poll.client.post(join_url(poll.base_url, "api/relay/next")?),
    )
    .json(body)
    .send()
    .await
    .map_err(|e| e.to_string())
}

fn relay_server_name_proof(
    material: &crate::custom_domain::RelayCertificateMaterial,
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> Result<RelayServerNameProof, String> {
    use rustls::pki_types::pem::PemObject as _;

    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(material.private_key_pem.as_bytes())
        .map_err(|error| format!("parse custom-domain relay proof key: {error}"))?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|error| format!("load custom-domain relay proof key: {error}"))?;
    let signer = signing_key
        .choose_scheme(&[rustls::SignatureScheme::ECDSA_NISTP256_SHA256])
        .ok_or_else(|| {
            "custom-domain relay proof requires an ECDSA P-256 certificate key".to_string()
        })?;
    let payload = relay_name_proof_signing_payload(
        daemon_id,
        daemon_public_key,
        issued_at_unix_ms,
        &material.server_name,
    );
    let signature = signer
        .sign(&payload)
        .map_err(|error| format!("sign custom-domain relay proof: {error}"))?;
    Ok(RelayServerNameProof {
        server_name: material.server_name.clone(),
        certificate_chain_pem: material.certificate_chain_pem.clone(),
        signature: crate::daemon_identity::b64u(&signature),
    })
}

fn relay_name_proof_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    server_name: &str,
) -> Vec<u8> {
    format!(
        "{RELAY_NAME_PROOF_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{}\n{server_name}\n",
        server_name.len(),
    )
    .into_bytes()
}

async fn decode_relay_poll_response(
    response: reqwest::Response,
) -> Result<Option<RelayDialback>, String> {
    let status = response.status();
    if status.as_u16() == 204 {
        return Ok(None);
    }
    if !status.is_success() {
        let text = relay_response_text_capped(response, "control rejection").await?;
        return Err(format!("relay control poll rejected: HTTP {status} {text}"));
    }
    let body = relay_response_body_capped(response, "control response").await?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|error| format!("parse relay response: {error}"))?;
    let Some(nonce) = value.pointer("/dialback/nonce").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    if nonce.is_empty() || nonce.len() > 64 {
        return Err("relay returned an invalid dial-back nonce".to_string());
    }
    let source_bucket = value
        .pointer("/dialback/source_bucket")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    if source_bucket
        .as_deref()
        .is_some_and(|bucket| !valid_source_bucket(bucket))
    {
        return Err("relay returned an invalid source bucket".to_string());
    }
    Ok(Some(RelayDialback {
        nonce: nonce.to_string(),
        source_bucket,
    }))
}

async fn relay_response_text_capped(
    response: reqwest::Response,
    label: &str,
) -> Result<String, String> {
    relay_response_body_capped(response, label)
        .await
        .map(|body| String::from_utf8_lossy(&body).into_owned())
}

async fn relay_response_body_capped(
    response: reqwest::Response,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| format!("read relay {label}: {error}"))?;
        if body.len().saturating_add(chunk.len()) > CONTROL_RESPONSE_MAX_BYTES {
            return Err(format!(
                "relay {label} exceeds the {CONTROL_RESPONSE_MAX_BYTES}-byte cap"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn relay_control_signing_payload(
    protocol: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    poller_id: Option<&str>,
    server_names: &[String],
) -> Vec<u8> {
    let mut payload =
        format!("{protocol}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n").into_bytes();
    if matches!(protocol, RELAY_CONTROL_PROTOCOL | RELAY_DISCONNECT_PROTOCOL) {
        let poller_id = poller_id.unwrap_or_default();
        payload.extend_from_slice(poller_id.len().to_string().as_bytes());
        payload.push(b'\n');
        payload.extend_from_slice(poller_id.as_bytes());
        payload.push(b'\n');
    }
    for name in server_names {
        payload.extend_from_slice(name.len().to_string().as_bytes());
        payload.push(b'\n');
        payload.extend_from_slice(name.as_bytes());
        payload.push(b'\n');
    }
    payload
}

/// Dial back a browser connection: connect the relay's passthrough port,
/// announce the nonce, connect the daemon's dedicated loopback relay ingress,
/// and splice. The browser's ClientHello (fleet SNI and all) flows verbatim to
/// the gateway, whose fleet certificate completes the handshake. The
/// dedicated accept edge, not any mutable byte in that ClientHello, records
/// that the connection came through the relay.
async fn handle_dialback(
    relay_endpoint: &str,
    gateway_ingress_addr: SocketAddr,
    dialback: &RelayDialback,
) -> Result<(), String> {
    let (data, gateway) = dialback_setup_with_timeout(
        relay_endpoint,
        gateway_ingress_addr,
        dialback,
        DIALBACK_SETUP_TIMEOUT,
    )
    .await?;
    splice(data, gateway).await;
    Ok(())
}

async fn dialback_setup_with_timeout(
    relay_endpoint: &str,
    gateway_ingress_addr: SocketAddr,
    dialback: &RelayDialback,
    timeout: Duration,
) -> Result<(TcpStream, TcpStream), String> {
    run_with_dialback_setup_timeout(timeout, async {
        let mut data = TcpStream::connect(relay_endpoint)
            .await
            .map_err(|error| format!("connect relay {relay_endpoint}: {error}"))?;
        data.write_all(format!("{DIALBACK_MAGIC} {}\n", dialback.nonce).as_bytes())
            .await
            .map_err(|error| format!("write dial-back hello: {error}"))?;
        let mut gateway = TcpStream::connect(gateway_ingress_addr)
            .await
            .map_err(|error| {
                format!("connect dedicated gateway ingress {gateway_ingress_addr}: {error}")
            })?;
        let source_bucket = dialback
            .source_bucket
            .clone()
            .filter(|bucket| valid_source_bucket(bucket))
            .unwrap_or_else(shared_relay_source_bucket);
        gateway
            .write_all(format!("{GATEWAY_RELAY_SOURCE_MAGIC} {source_bucket}\n").as_bytes())
            .await
            .map_err(|error| format!("write gateway relay-source preamble: {error}"))?;
        Ok((data, gateway))
    })
    .await
}

async fn run_with_dialback_setup_timeout<T>(
    timeout: Duration,
    setup: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(timeout, setup)
        .await
        .map_err(|_| "relay dial-back setup timed out".to_string())?
}

fn valid_source_bucket(bucket: &str) -> bool {
    bucket.len() == 43
        && bucket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn shared_relay_source_bucket() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"intendant-relay-source-fallback-v1");
    crate::daemon_identity::b64u(&hasher.finalize())
}

/// Bidirectional byte splice with a per-direction byte cap and idle teardown.
async fn splice(relay: TcpStream, gateway: TcpStream) {
    let (relay_r, relay_w) = relay.into_split();
    let (gateway_r, gateway_w) = gateway.into_split();
    let to_gateway = copy_half(relay_r, gateway_w);
    let to_relay = copy_half(gateway_r, relay_w);
    tokio::select! {
        _ = to_gateway => {}
        _ = to_relay => {}
    }
}

async fn copy_half<R, W>(mut reader: R, mut writer: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = match tokio::time::timeout(SPLICE_IDLE, reader.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => n,
        };
        total = total.saturating_add(n as u64);
        if total > SPLICE_MAX_BYTES {
            break;
        }
        if writer.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn dns_reconciliation_requires_a_configured_rendezvous_not_relay_enablement() {
        assert!(!has_rendezvous_destination(&ConnectConfig::default()));
        let configured = ConnectConfig {
            rendezvous_url: Some("https://connect.example.test".to_string()),
            relay_enabled: false,
            ..ConnectConfig::default()
        };
        assert!(has_rendezvous_destination(&configured));
    }

    #[test]
    fn dialback_capacity_refuses_excess_without_an_unbounded_task_set() {
        assert!(dialback_capacity_available(MAX_ACTIVE_DIALBACKS - 1));
        assert!(!dialback_capacity_available(MAX_ACTIVE_DIALBACKS));
        assert!(!dialback_capacity_available(MAX_ACTIVE_DIALBACKS + 1));
    }

    #[tokio::test]
    async fn dns_reconciler_withdraws_when_disabled_and_waits_for_tunnel_readiness() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (lifecycle_tx, lifecycle_rx) = watch::channel(false);
        let (readiness_tx, readiness_rx) = watch::channel(false);
        let calls_for_disabled = Arc::clone(&calls);
        let disabled = tokio::spawn(relay_dns_reassert_loop_with(
            false,
            lifecycle_rx,
            readiness_rx,
            readiness_tx.clone(),
            move |enable| {
                let calls = Arc::clone(&calls_for_disabled);
                async move {
                    calls.lock().unwrap().push(enable);
                    Ok(())
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), [false]);
        disabled.abort();
        drop(lifecycle_tx);

        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (lifecycle_tx, lifecycle_rx) = watch::channel(true);
        let (readiness_tx, readiness_rx) = watch::channel(false);
        let calls_for_waiting = Arc::clone(&calls);
        let waiting = tokio::spawn(relay_dns_reassert_loop_with(
            true,
            lifecycle_rx,
            readiness_rx,
            readiness_tx.clone(),
            move |enable| {
                let calls = Arc::clone(&calls_for_waiting);
                async move {
                    calls.lock().unwrap().push(enable);
                    Ok(())
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), [false]);
        readiness_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !calls.lock().unwrap().contains(&true) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            calls.lock().unwrap().iter().position(|enable| *enable),
            Some(1),
            "relay DNS must not publish before a control poll established readiness"
        );
        waiting.abort();
        drop(lifecycle_tx);
    }

    #[tokio::test]
    async fn relay_dns_withdrawal_restores_direct_addresses_before_success() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let relay_calls = Arc::clone(&calls);
        let direct_calls = Arc::clone(&calls);
        publish_relay_dns_state_with(
            false,
            move |enable| {
                let calls = Arc::clone(&relay_calls);
                async move {
                    calls.lock().unwrap().push(if enable {
                        "relay-enable"
                    } else {
                        "relay-disable"
                    });
                    Ok(())
                }
            },
            move || {
                let calls = Arc::clone(&direct_calls);
                async move {
                    calls.lock().unwrap().push("direct");
                    Ok(())
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["relay-disable", "direct"]
        );

        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let relay_calls = Arc::clone(&calls);
        let direct_calls = Arc::clone(&calls);
        assert!(publish_relay_dns_state_with(
            true,
            move |enable| {
                let calls = Arc::clone(&relay_calls);
                async move {
                    calls.lock().unwrap().push(if enable {
                        "relay-enable"
                    } else {
                        "relay-disable"
                    });
                    Ok(())
                }
            },
            move || {
                let calls = Arc::clone(&direct_calls);
                async move {
                    calls.lock().unwrap().push("direct");
                    Ok(())
                }
            },
        )
        .await
        .is_ok());
        assert_eq!(calls.lock().unwrap().as_slice(), ["relay-enable"]);
    }

    #[tokio::test]
    async fn relay_ready_certificate_address_refresh_preserves_relay_dns() {
        let publication = Mutex::new(RelayDnsPublicationState::default());
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let relay_calls = Arc::clone(&calls);
        let unexpected_direct_calls = Arc::clone(&calls);
        publish_relay_dns_state_coordinated_with(
            &publication,
            true,
            move |enable| {
                let calls = Arc::clone(&relay_calls);
                async move {
                    calls.lock().unwrap().push(if enable {
                        "relay-enable"
                    } else {
                        "relay-disable"
                    });
                    Ok(())
                }
            },
            || async { Ok(()) },
        )
        .await
        .unwrap();

        let addresses = vec!["192.0.2.10".to_string()];
        let accepted = publish_fleet_dns_addresses_with(&publication, &addresses, move || {
            let calls = Arc::clone(&unexpected_direct_calls);
            async move {
                calls.lock().unwrap().push("direct-refresh");
                Ok(Vec::new())
            }
        })
        .await
        .unwrap();
        assert_eq!(accepted, addresses);
        assert_eq!(calls.lock().unwrap().as_slice(), ["relay-enable"]);

        let relay_calls = Arc::clone(&calls);
        let withdrawal_calls = Arc::clone(&calls);
        publish_relay_dns_state_coordinated_with(
            &publication,
            false,
            move |enable| {
                let calls = Arc::clone(&relay_calls);
                async move {
                    calls.lock().unwrap().push(if enable {
                        "relay-enable"
                    } else {
                        "relay-disable"
                    });
                    Ok(())
                }
            },
            move || {
                let calls = Arc::clone(&withdrawal_calls);
                async move {
                    calls.lock().unwrap().push("direct-restore");
                    Ok(())
                }
            },
        )
        .await
        .unwrap();
        let direct_calls = Arc::clone(&calls);
        let direct_addresses = addresses.clone();
        let accepted = publish_fleet_dns_addresses_with(&publication, &addresses, move || {
            let calls = Arc::clone(&direct_calls);
            let addresses = direct_addresses;
            async move {
                calls.lock().unwrap().push("direct-refresh");
                Ok(addresses)
            }
        })
        .await
        .unwrap();
        assert_eq!(accepted, vec!["192.0.2.10".to_string()]);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                "relay-enable",
                "relay-disable",
                "direct-restore",
                "direct-refresh"
            ]
        );
    }

    #[tokio::test]
    async fn incomplete_relay_withdrawal_remains_retryable() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let relay_calls = Arc::clone(&calls);
        let direct_calls = Arc::clone(&calls);
        let error = publish_relay_dns_state_with(
            false,
            move |_| {
                let calls = Arc::clone(&relay_calls);
                async move {
                    calls.lock().unwrap().push("relay-disable");
                    Ok(())
                }
            },
            move || {
                let calls = Arc::clone(&direct_calls);
                async move {
                    calls.lock().unwrap().push("direct");
                    Err("direct publish unavailable".to_string())
                }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error, "direct publish unavailable");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["relay-disable", "direct"]
        );
    }

    #[test]
    fn v2_signature_payload_binds_length_prefixed_exact_names() {
        let payload = relay_control_signing_payload(
            RELAY_CONTROL_PROTOCOL,
            "daemon",
            "public",
            42,
            Some("11111111111111111111111111111111"),
            &["box.example.test".to_string()],
        );
        assert_eq!(
            String::from_utf8(payload).unwrap(),
            concat!(
                "intendant-connect-relay-control-v2\n",
                "daemon\n",
                "public\n",
                "42\n",
                "32\n",
                "11111111111111111111111111111111\n",
                "16\n",
                "box.example.test\n",
            )
        );
    }

    #[test]
    fn disconnect_signature_payload_binds_the_poller_identity() {
        let payload = relay_control_signing_payload(
            RELAY_DISCONNECT_PROTOCOL,
            "daemon",
            "public",
            42,
            Some("11111111111111111111111111111111"),
            &[],
        );
        assert_eq!(
            String::from_utf8(payload).unwrap(),
            concat!(
                "intendant-connect-relay-disconnect-v1\n",
                "daemon\n",
                "public\n",
                "42\n",
                "32\n",
                "11111111111111111111111111111111\n",
            )
        );
    }

    #[test]
    fn name_proof_payload_binds_the_exact_name_and_daemon_identity() {
        assert_eq!(
            String::from_utf8(relay_name_proof_signing_payload(
                "daemon",
                "public",
                42,
                "box.example.test",
            ))
            .unwrap(),
            concat!(
                "intendant-connect-relay-name-proof-v1\n",
                "daemon\n",
                "public\n",
                "42\n",
                "16\n",
                "box.example.test\n",
            )
        );
    }

    #[test]
    fn exact_name_rejections_fall_back_without_disabling_fleet_routing() {
        assert!(exact_name_registration_rejected(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"unsupported protocol"}"#,
        ));
        assert!(exact_name_registration_rejected(
            reqwest::StatusCode::CONFLICT,
            "",
        ));
        assert!(exact_name_registration_rejected(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":"relay certificate ownership proof is invalid"}"#,
        ));
        assert!(!exact_name_registration_rejected(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":"missing bearer token"}"#,
        ));
        assert!(!exact_name_registration_rejected(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":"daemon authentication failed"}"#,
        ));
        assert!(!exact_name_registration_rejected(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"relay control signature invalid"}"#,
        ));
    }

    fn relay_poll_test_context() -> (
        tempfile::TempDir,
        Client,
        ConnectConfig,
        DaemonIdentity,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::builder().build().unwrap();
        let config = ConnectConfig {
            enabled: true,
            ..ConnectConfig::default()
        };
        let identity = DaemonIdentity::load_or_create(&dir.path().join("identity.pk8")).unwrap();
        let daemon_id = "daemon-test".to_string();
        (dir, client, config, identity, daemon_id)
    }

    #[tokio::test]
    async fn local_exact_name_proof_failure_falls_back_to_v1() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_route = Arc::clone(&seen);
        let router = axum::Router::new().route(
            "/api/relay/next",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let seen = Arc::clone(&seen_for_route);
                async move {
                    seen.lock().unwrap().push(
                        body.get("protocol")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        let (dir, client, config, identity, daemon_id) = relay_poll_test_context();
        let poll = RelayPollContext {
            client: &client,
            config: &config,
            base_url: &base_url,
            identity: &identity,
            daemon_id: &daemon_id,
            poller_id: "11111111111111111111111111111111",
        };
        let material = crate::custom_domain::RelayCertificateMaterial {
            server_name: "box.example.test".to_string(),
            certificate_chain_pem: "not-used".to_string(),
            private_key_pem: "not-a-private-key".to_string(),
        };
        assert_eq!(poll_relay_next(&poll, &[material]).await.unwrap(), None);
        assert_eq!(seen.lock().unwrap().as_slice(), [RELAY_CONTROL_PROTOCOL_V1]);
        drop(dir);
        server.abort();
    }

    #[tokio::test]
    async fn proof_specific_forbidden_response_falls_back_to_v1() {
        use axum::response::IntoResponse as _;

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_route = Arc::clone(&seen);
        let router = axum::Router::new().route(
            "/api/relay/next",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let seen = Arc::clone(&seen_for_route);
                async move {
                    let protocol = body
                        .get("protocol")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string();
                    seen.lock().unwrap().push(protocol.clone());
                    if protocol == RELAY_CONTROL_PROTOCOL {
                        (
                            axum::http::StatusCode::FORBIDDEN,
                            axum::Json(serde_json::json!({
                                "ok": false,
                                "error": "relay certificate ownership proof is invalid",
                            })),
                        )
                            .into_response()
                    } else {
                        axum::http::StatusCode::NO_CONTENT.into_response()
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        let (dir, client, config, identity, daemon_id) = relay_poll_test_context();
        let poll = RelayPollContext {
            client: &client,
            config: &config,
            base_url: &base_url,
            identity: &identity,
            daemon_id: &daemon_id,
            poller_id: "11111111111111111111111111111111",
        };
        let certificate =
            rcgen::generate_simple_self_signed(vec!["box.example.test".to_string()]).unwrap();
        let material = crate::custom_domain::RelayCertificateMaterial {
            server_name: "box.example.test".to_string(),
            certificate_chain_pem: certificate.cert.pem(),
            private_key_pem: certificate.signing_key.serialize_pem(),
        };
        assert_eq!(poll_relay_next(&poll, &[material]).await.unwrap(), None);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [RELAY_CONTROL_PROTOCOL, RELAY_CONTROL_PROTOCOL_V1]
        );
        drop(dir);
        server.abort();
    }

    #[tokio::test]
    async fn oversized_chunked_control_response_is_rejected_incrementally() {
        let router = axum::Router::new().route(
            "/oversized",
            axum::routing::get(|| async {
                let chunks = futures_util::stream::iter((0..5).map(|_| {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(vec![
                        b'x';
                        CONTROL_RESPONSE_MAX_BYTES
                            / 4
                    ]))
                }));
                axum::response::Response::new(axum::body::Body::from_stream(chunks))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oversized", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        let response = reqwest::get(url).await.unwrap();
        let error = relay_response_body_capped(response, "test response")
            .await
            .unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn blackholed_dialback_setup_has_an_explicit_deadline() {
        let error = run_with_dialback_setup_timeout(
            Duration::from_millis(20),
            std::future::pending::<Result<(), String>>(),
        )
        .await
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
    }

    /// The dial-back path splices the relay data connection to the dedicated
    /// loopback gateway ingress: bytes written by a fake "browser" at the
    /// relay end arrive at the "gateway" end and vice versa, after the nonce
    /// hello is consumed by the relay side.
    #[tokio::test]
    async fn dialback_splices_relay_to_loopback_gateway() {
        // Stand in for the relay's passthrough port: accept one connection,
        // read the dial-back hello line, then echo-splice remains to a channel.
        let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay_listener.local_addr().unwrap();

        // Stand in for this daemon's gateway: echo server that upper-cases.
        let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_port = gateway_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = gateway_listener.accept().await.unwrap();
            let mut preamble = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream.read_exact(&mut byte).await.unwrap();
                if byte[0] == b'\n' {
                    break;
                }
                preamble.push(byte[0]);
            }
            assert!(String::from_utf8(preamble).unwrap().starts_with("ITGWS1 "));
            let mut buf = vec![0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            let upper: Vec<u8> = buf[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
            stream.write_all(&upper).await.unwrap();
            let _ = stream.shutdown().await;
        });

        // Drive the daemon dial-back side.
        let dial = tokio::spawn(async move {
            handle_dialback(
                &relay_addr.to_string(),
                std::net::SocketAddr::from(([127, 0, 0, 1], gateway_port)),
                &RelayDialback {
                    nonce: "the-nonce".to_string(),
                    source_bucket: Some("a".repeat(43)),
                },
            )
            .await
            .unwrap();
        });

        // The relay end: accept, verify the hello, then act as the browser.
        let (mut relay_side, _) = relay_listener.accept().await.unwrap();
        let mut hello = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            relay_side.read_exact(&mut byte).await.unwrap();
            if byte[0] == b'\n' {
                break;
            }
            hello.push(byte[0]);
        }
        assert_eq!(String::from_utf8(hello).unwrap(), "ITRLY1 the-nonce");
        // "Browser" ciphertext round-trips through the daemon into the gateway.
        relay_side.write_all(b"hello-daemon").await.unwrap();
        let mut echoed = Vec::new();
        relay_side.read_to_end(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"HELLO-DAEMON");
        dial.await.unwrap();
    }
}
