//! Independent certificate observations for the dark hosted-control lane.
//!
//! A peer obtains the target's signed serial ledger through the already
//! authenticated direct peer route, then separately opens the target's
//! public fleet-name TLS endpoint with ordinary WebPKI verification. Only a
//! serial outside the signed ledger is reported, over the typed peer
//! transport. Connectivity failures remain diagnostics and never become
//! certificate evidence.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;

use crate::access::hosted_control::{
    verify_certificate_ledger, HostedCertificateLedger, HostedControlRuntime, HostedWitnessVantage,
};
use crate::peer::card::{AgentCard, TransportSpec};
use crate::peer::handle::PeerHandle;
use crate::peer::registry::PeerRegistry;
use crate::peer::transport::intendant::{PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE};
use crate::peer::transport::ws_url_to_http_base;
use crate::peer::PeerWitnessVantage;

const LEDGER_PATH: &str = "/api/hosted-control/certificate-ledger";
const LEDGER_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const LEDGER_RESPONSE_CAP: usize = 64 * 1024;
const CERTIFICATE_DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const WITNESS_INITIAL_DELAY: Duration = Duration::from_secs(2 * 60);
const WITNESS_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetCertificateObservation {
    pub serial_hex: String,
    pub vantage: HostedWitnessVantage,
}

pub fn certificate_ledger_endpoints(card: &AgentCard) -> Vec<String> {
    let mut endpoints: Vec<String> = card
        .transports
        .iter()
        .filter_map(|transport| match transport {
            TransportSpec::IntendantWs { url } => {
                Some(format!("{}{LEDGER_PATH}", ws_url_to_http_base(url)))
            }
            _ => None,
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    endpoints.retain(|endpoint| seen.insert(endpoint.clone()));
    endpoints
}

pub async fn fetch_certificate_ledger(
    handle: &PeerHandle,
) -> Result<HostedCertificateLedger, String> {
    let endpoints = certificate_ledger_endpoints(&handle.card_snapshot());
    if endpoints.is_empty() {
        return Err(format!(
            "peer {} advertises no direct route for a certificate ledger",
            handle.id()
        ));
    }
    let credentials = handle.transport_credentials();
    let tls_config = credentials
        .tls
        .client_config(
            &credentials.pinned_fingerprints,
            credentials.client_identity.as_ref(),
        )
        .map_err(|error| format!("build peer ledger TLS policy: {error}"))?;
    let mut client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(tls_config) = tls_config {
        client = client.use_preconfigured_tls(rustls::ClientConfig::clone(&tls_config));
    }
    let client = client
        .build()
        .map_err(|error| format!("build peer ledger client: {error}"))?;
    fetch_certificate_ledger_from_candidates(
        &client,
        &endpoints,
        credentials.bearer_token.as_deref(),
    )
    .await
}

async fn fetch_certificate_ledger_from_candidates(
    client: &reqwest::Client,
    endpoints: &[String],
    bearer_token: Option<&str>,
) -> Result<HostedCertificateLedger, String> {
    let mut last_error = None;
    for endpoint in endpoints {
        match fetch_certificate_ledger_from_endpoint(client, endpoint, bearer_token).await {
            Ok(ledger) => return Ok(ledger),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "no peer certificate ledger route was attempted".to_string()))
}

async fn fetch_certificate_ledger_from_endpoint(
    client: &reqwest::Client,
    endpoint: &str,
    bearer_token: Option<&str>,
) -> Result<HostedCertificateLedger, String> {
    let mut request = client
        .get(endpoint)
        .timeout(LEDGER_FETCH_TIMEOUT)
        .header(PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE);
    if let Some(token) = bearer_token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("fetch peer certificate ledger: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "peer certificate ledger returned HTTP {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > LEDGER_RESPONSE_CAP as u64)
    {
        return Err("peer certificate ledger response exceeds its size limit".to_string());
    }
    let mut bytes = Vec::new();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| format!("read peer certificate ledger: {error}"))?;
        if bytes.len().saturating_add(chunk.len()) > LEDGER_RESPONSE_CAP {
            return Err("peer certificate ledger response exceeds its size limit".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    let ledger: HostedCertificateLedger = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode peer certificate ledger: {error}"))?;
    verify_certificate_ledger(&ledger)?;
    ensure_independent_ledger_source(endpoint, &ledger.fleet_origin)?;
    Ok(ledger)
}

fn ensure_independent_ledger_source(endpoint: &str, fleet_origin: &str) -> Result<(), String> {
    let source = url::Url::parse(endpoint)
        .map_err(|error| format!("parse peer certificate ledger route: {error}"))?;
    let fleet = url::Url::parse(fleet_origin)
        .map_err(|error| format!("parse fleet certificate origin: {error}"))?;
    if source.origin() == fleet.origin() {
        return Err(
            "peer certificate ledger route must be independent of the observed fleet origin"
                .to_string(),
        );
    }
    Ok(())
}

pub async fn observe_fleet_certificate(
    ledger: &HostedCertificateLedger,
) -> Result<FleetCertificateObservation, String> {
    let roots = crate::web_tls::load_native_root_store()
        .map_err(|error| format!("load native certificate roots: {error}"))?;
    let origin = url::Url::parse(&ledger.fleet_origin)
        .map_err(|error| format!("parse fleet origin: {error}"))?;
    let host = origin
        .host_str()
        .ok_or_else(|| "fleet origin has no host".to_string())?
        .to_string();
    let port = origin
        .port_or_known_default()
        .ok_or_else(|| "fleet origin has no TLS port".to_string())?;
    let tcp = tokio::time::timeout(
        CERTIFICATE_DIAL_TIMEOUT,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| "fleet certificate dial timed out".to_string())?
    .map_err(|error| format!("fleet certificate dial failed: {error}"))?;
    observe_fleet_certificate_on_stream(&host, tcp, roots).await
}

async fn observe_fleet_certificate_on_stream(
    host: &str,
    tcp: tokio::net::TcpStream,
    roots: rustls::RootCertStore,
) -> Result<FleetCertificateObservation, String> {
    let peer_addr = tcp
        .peer_addr()
        .map_err(|error| format!("read fleet certificate peer address: {error}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|error| format!("configure fleet certificate TLS: {error}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| "fleet origin host is not a valid TLS server name".to_string())?;
    let tls = tokio::time::timeout(
        CERTIFICATE_DIAL_TIMEOUT,
        connector.connect(server_name, tcp),
    )
    .await
    .map_err(|_| "fleet certificate TLS handshake timed out".to_string())?
    .map_err(|error| format!("fleet certificate TLS handshake failed: {error}"))?;
    let certificates = tls
        .get_ref()
        .1
        .peer_certificates()
        .ok_or_else(|| "fleet certificate endpoint presented no certificate".to_string())?;
    let leaf = certificates
        .first()
        .ok_or_else(|| "fleet certificate endpoint presented an empty chain".to_string())?;
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|error| format!("parse fleet certificate: {error}"))?;
    let serial_hex = parsed
        .raw_serial()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(FleetCertificateObservation {
        serial_hex: crate::fleet_cert::normalize_serial_hex(&serial_hex),
        vantage: classify_destination_vantage(peer_addr),
    })
}

fn classify_destination_vantage(peer_addr: SocketAddr) -> HostedWitnessVantage {
    match peer_addr.ip() {
        IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() || ip.is_link_local() => {
            HostedWitnessVantage::SameLan
        }
        IpAddr::V6(ip)
            if ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local() =>
        {
            HostedWitnessVantage::SameLan
        }
        IpAddr::V4(ip) if ip.is_unspecified() || ip.is_multicast() => HostedWitnessVantage::Unknown,
        IpAddr::V6(ip) if ip.is_unspecified() || ip.is_multicast() => HostedWitnessVantage::Unknown,
        // A public destination can still be a hairpinned route from a
        // co-located observer. It becomes strong only through explicit local
        // operator configuration for this peer relationship.
        _ => HostedWitnessVantage::Unknown,
    }
}

fn effective_peer_vantage(
    destination: HostedWitnessVantage,
    configured: PeerWitnessVantage,
) -> HostedWitnessVantage {
    if destination == HostedWitnessVantage::SameLan {
        return HostedWitnessVantage::SameLan;
    }
    match configured {
        PeerWitnessVantage::SameLan => HostedWitnessVantage::SameLan,
        PeerWitnessVantage::Remote => HostedWitnessVantage::Remote,
        PeerWitnessVantage::Unknown => HostedWitnessVantage::Unknown,
    }
}

async fn observe_peer_once(
    runtime: &HostedControlRuntime,
    handle: &PeerHandle,
) -> Result<(), String> {
    if !handle.is_connected() || !handle.features().certificate_witness {
        return Ok(());
    }
    let ledger = fetch_certificate_ledger(handle).await?;
    let observation = observe_fleet_certificate(&ledger).await?;
    if ledger.serials.contains(&observation.serial_hex) {
        return Ok(());
    }
    let vantage = effective_peer_vantage(observation.vantage, handle.certificate_witness_vantage());
    let report = runtime.build_peer_witness_report(&ledger, &observation.serial_hex, vantage)?;
    handle
        .submit_certificate_witness(report)
        .await
        .map_err(|error| format!("submit peer certificate witness: {error}"))
}

async fn observe_all_peers(runtime: Arc<HostedControlRuntime>, registry: PeerRegistry) {
    let mut tasks = tokio::task::JoinSet::new();
    for handle in registry.list() {
        let runtime = Arc::clone(&runtime);
        tasks.spawn(async move {
            let peer_id = handle.id().to_string();
            (peer_id, observe_peer_once(&runtime, &handle).await)
        });
    }
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((_peer_id, Ok(()))) => {}
            Ok((peer_id, Err(error))) => {
                eprintln!(
                    "[hosted-control] certificate witness diagnostic for peer {peer_id}: {error}"
                );
            }
            Err(error) => {
                eprintln!("[hosted-control] certificate witness task failed: {error}");
            }
        }
    }
}

pub fn spawn_certificate_witness_loop(runtime: Arc<HostedControlRuntime>, registry: PeerRegistry) {
    tokio::spawn(async move {
        tokio::time::sleep(WITNESS_INITIAL_DELAY).await;
        let mut interval = tokio::time::interval(WITNESS_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            observe_all_peers(Arc::clone(&runtime), registry.clone()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::card::AuthRequirements;
    use crate::peer::id::{PeerId, PeerKind};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn card(transports: Vec<TransportSpec>) -> AgentCard {
        AgentCard {
            id: PeerId::new(PeerKind::Intendant, "test"),
            label: "test".to_string(),
            version: "test".to_string(),
            git_sha: None,
            transports,
            capabilities: Vec::new(),
            auth: AuthRequirements::none(),
        }
    }

    #[test]
    fn ledger_endpoints_preserve_direct_peer_route_fallback_order() {
        assert_eq!(
            certificate_ledger_endpoints(&card(vec![
                TransportSpec::IntendantWs {
                    url: "wss://dead.example.test:9443/ws".to_string(),
                },
                TransportSpec::IntendantWs {
                    url: "wss://peer.example.test:9443/ws".to_string(),
                },
                TransportSpec::IntendantWs {
                    url: "wss://dead.example.test:9443/ws".to_string(),
                },
            ])),
            vec![
                "https://dead.example.test:9443/api/hosted-control/certificate-ledger",
                "https://peer.example.test:9443/api/hosted-control/certificate-ledger",
            ]
        );
    }

    async fn spawn_ledger_response(
        status: &str,
        body: String,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}{LEDGER_PATH}", listener.local_addr().unwrap());
        let status = status.to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        (endpoint, task)
    }

    #[tokio::test]
    async fn ledger_fetch_falls_back_to_the_next_direct_route() {
        let temp = tempfile::tempdir().unwrap();
        let identity =
            crate::daemon_identity::DaemonIdentity::load_or_create(temp.path().join("id.pk8"))
                .unwrap();
        let mut expected = HostedCertificateLedger {
            protocol: crate::access::hosted_control::CERTIFICATE_LEDGER_PROTOCOL.to_string(),
            daemon_id: "daemon-test".to_string(),
            daemon_public_key: identity.public_key_b64u(),
            fleet_origin: "https://fleet.example.test".to_string(),
            serials: vec!["a1b2".to_string()],
            issued_unix_ms: 1_700_000_000_000,
            signature: String::new(),
        };
        expected.signature = identity.sign_b64u(expected.unsigned_payload().as_bytes());
        let (first, first_task) =
            spawn_ledger_response("503 Service Unavailable", "{}".to_string()).await;
        let (second, second_task) =
            spawn_ledger_response("200 OK", serde_json::to_string(&expected).unwrap()).await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let fetched = fetch_certificate_ledger_from_candidates(&client, &[first, second], None)
            .await
            .unwrap();

        assert_eq!(fetched, expected);
        first_task.await.unwrap();
        second_task.await.unwrap();
    }

    #[test]
    fn destination_address_never_claims_an_outside_network_vantage() {
        assert_eq!(
            classify_destination_vantage("192.168.1.20:443".parse().unwrap()),
            HostedWitnessVantage::SameLan
        );
        assert_eq!(
            classify_destination_vantage("[fd00::20]:443".parse().unwrap()),
            HostedWitnessVantage::SameLan
        );
        assert_eq!(
            classify_destination_vantage("203.0.113.20:443".parse().unwrap()),
            HostedWitnessVantage::Unknown
        );
        assert_eq!(
            effective_peer_vantage(HostedWitnessVantage::Unknown, PeerWitnessVantage::Remote),
            HostedWitnessVantage::Remote
        );
        assert_eq!(
            effective_peer_vantage(HostedWitnessVantage::SameLan, PeerWitnessVantage::Remote),
            HostedWitnessVantage::SameLan
        );
    }

    #[test]
    fn ledger_source_must_not_be_the_observed_fleet_origin() {
        assert!(ensure_independent_ledger_source(
            "https://fleet.example.test/api/hosted-control/certificate-ledger",
            "https://fleet.example.test",
        )
        .unwrap_err()
        .contains("independent"));
        ensure_independent_ledger_source(
            "https://peer-direct.example.test:9443/api/hosted-control/certificate-ledger",
            "https://fleet.example.test",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn observation_verifies_webpki_name_and_reads_the_leaf_serial() {
        let temp = tempfile::tempdir().unwrap();
        let mut params =
            rcgen::CertificateParams::new(vec!["fleet.example.test".to_string()]).unwrap();
        params.serial_number = Some(rcgen::SerialNumber::from(vec![0x00, 0x12, 0xab]));
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let cert_path = temp.path().join("server.crt");
        let key_path = temp.path().join("server.key");
        std::fs::write(&cert_path, certificate.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        let acceptor =
            crate::web_tls::build_single_cert_acceptor(&crate::web_tls::TlsCertSource::Files {
                cert_path,
                key_path,
            })
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            acceptor.accept(stream).await.unwrap();
        });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate.der().clone()).unwrap();
        let tcp = tokio::net::TcpStream::connect(address).await.unwrap();

        let observation = observe_fleet_certificate_on_stream("fleet.example.test", tcp, roots)
            .await
            .unwrap();

        assert_eq!(observation.serial_hex, "12ab");
        assert_eq!(observation.vantage, HostedWitnessVantage::SameLan);
        server.await.unwrap();
    }
}
