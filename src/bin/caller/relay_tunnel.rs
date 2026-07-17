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

use std::{net::SocketAddr, time::Duration};

use reqwest::{Client, Url};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::connect_rendezvous::{
    authenticated, dns_publish_via_relay, join_url, signed_daemon_context, RELAY_CONTROL_PROTOCOL,
};
use crate::daemon_identity::DaemonIdentity;
use crate::project::ConnectConfig;

/// The first line the daemon writes on a dial-back data connection (mirrors
/// `bin/connect/relay.rs`): this magic and the single-use nonce.
const DIALBACK_MAGIC: &str = "ITRLY1";
/// Control long-poll timeout requested of the relay.
const CONTROL_POLL_TIMEOUT_MS: u64 = 15_000;
/// Reconnect backoff bounds after control-channel errors.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How often to re-assert relay-mode fleet DNS while the tunnel runs, so the
/// fleet name keeps resolving to the relay (the DNS record TTL is short).
const DNS_REASSERT_INTERVAL: Duration = Duration::from_secs(240);
/// Idle teardown + per-direction byte cap on a spliced dial-back connection.
const SPLICE_IDLE: Duration = Duration::from_secs(120);
const SPLICE_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Start the relay tunnel client when the config opts in. No-op otherwise.
/// `gateway_ingress_addr` is the gateway's dedicated loopback-only relay
/// listener. It serves the fleet-certificate handshake while preserving
/// immutable relay provenance at the accept edge.
pub fn spawn_relay_tunnel_client(config: ConnectConfig, gateway_ingress_addr: Option<SocketAddr>) {
    if !(config.enabled && config.relay_enabled) {
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
    tokio::spawn(run_relay_tunnel(endpoint, gateway_ingress_addr));
    tokio::spawn(relay_dns_reassert_loop());
}

/// Best-effort: keep the fleet name pointed at the relay while the tunnel is
/// up. Only meaningful when the rendezvous runs both fleet DNS and the relay;
/// failures are expected weather for other configurations and logged quietly.
async fn relay_dns_reassert_loop() {
    loop {
        if let Err(error) = dns_publish_via_relay(true).await {
            eprintln!("[relay] relay-mode dns publish (best-effort): {error}");
        }
        tokio::time::sleep(DNS_REASSERT_INTERVAL).await;
    }
}

async fn run_relay_tunnel(relay_endpoint: String, gateway_ingress_addr: SocketAddr) {
    let client = match Client::builder()
        .timeout(Duration::from_millis(CONTROL_POLL_TIMEOUT_MS + 10_000))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[relay] failed to build tunnel HTTP client: {error}");
            return;
        }
    };
    eprintln!("[relay] tunnel client enabled (dial-back via {relay_endpoint})");
    let mut backoff = BACKOFF_MIN;
    loop {
        // Resolve the signing context fresh each cycle so a rendezvous URL or
        // identity that only becomes available after boot is picked up.
        let (config, base_url, identity, daemon_id) = match signed_daemon_context() {
            Ok(context) => context,
            Err(error) => {
                eprintln!("[relay] tunnel context unavailable: {error}");
                backoff = sleep_backoff(backoff).await;
                continue;
            }
        };
        match poll_relay_next(&client, &config, &base_url, &identity, &daemon_id).await {
            Ok(Some(nonce)) => {
                backoff = BACKOFF_MIN;
                let endpoint = relay_endpoint.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        handle_dialback(&endpoint, gateway_ingress_addr, &nonce).await
                    {
                        eprintln!("[relay] dial-back failed: {error}");
                    }
                });
            }
            Ok(None) => {
                backoff = BACKOFF_MIN;
            }
            Err(error) => {
                eprintln!("[relay] control poll failed: {error}");
                backoff = sleep_backoff(backoff).await;
            }
        }
    }
}

async fn sleep_backoff(current: Duration) -> Duration {
    tokio::time::sleep(current).await;
    (current * 2).min(BACKOFF_MAX)
}

/// One control-channel long-poll. Returns the dial-back nonce if the relay
/// asked this daemon to dial back, `None` on an empty poll.
async fn poll_relay_next(
    client: &Client,
    config: &ConnectConfig,
    base_url: &Url,
    identity: &DaemonIdentity,
    daemon_id: &str,
) -> Result<Option<String>, String> {
    let daemon_public_key = identity.public_key_b64u();
    let issued_at_unix_ms = crate::access::client_key::now_unix_ms().max(0) as u64;
    let payload = format!(
        "{RELAY_CONTROL_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n"
    );
    let signature = identity.sign_b64u(payload.as_bytes());
    let response = authenticated(config, client.post(join_url(base_url, "api/relay/next")?))
        .json(&serde_json::json!({
            "protocol": RELAY_CONTROL_PROTOCOL,
            "daemon_id": daemon_id,
            "daemon_public_key": daemon_public_key,
            "issued_at_unix_ms": issued_at_unix_ms,
            "signature": signature,
            "timeout_ms": CONTROL_POLL_TIMEOUT_MS,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status().as_u16() == 204 {
        return Ok(None);
    }
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("relay control poll rejected: HTTP {status} {text}"));
    }
    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    Ok(value
        .pointer("/dialback/nonce")
        .and_then(|v| v.as_str())
        .map(str::to_string))
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
    nonce: &str,
) -> Result<(), String> {
    let mut data = TcpStream::connect(relay_endpoint)
        .await
        .map_err(|e| format!("connect relay {relay_endpoint}: {e}"))?;
    data.write_all(format!("{DIALBACK_MAGIC} {nonce}\n").as_bytes())
        .await
        .map_err(|e| format!("write dial-back hello: {e}"))?;
    let gateway = TcpStream::connect(gateway_ingress_addr)
        .await
        .map_err(|e| format!("connect dedicated gateway ingress {gateway_ingress_addr}: {e}"))?;
    splice(data, gateway).await;
    Ok(())
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

    fn relay_disabled() -> ConnectConfig {
        ConnectConfig {
            enabled: true,
            relay_enabled: false,
            ..ConnectConfig::default()
        }
    }

    #[test]
    fn spawn_is_a_noop_when_relay_is_disabled() {
        // No panic, no task: disabled config returns immediately. (A running
        // tokio runtime is not required because we never spawn.)
        let ingress = Some(std::net::SocketAddr::from(([127, 0, 0, 1], 8765)));
        spawn_relay_tunnel_client(relay_disabled(), ingress);
        spawn_relay_tunnel_client(ConnectConfig::default(), ingress);
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
                "the-nonce",
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
