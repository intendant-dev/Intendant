//! Independent certificate observations for the dark hosted-control lane.
//!
//! A peer obtains the target's signed serial ledger through the already
//! authenticated direct peer route, then separately opens the target's
//! public fleet-name TLS endpoint with ordinary WebPKI verification. Only a
//! serial outside the signed ledger is reported, over the typed peer
//! transport. Connectivity failures remain diagnostics and never become
//! certificate evidence.
//!
//! Scheduling hygiene: a peer card can advertise candidate addresses that
//! are unreachable from this daemon's network, and dialing them every sweep
//! floods the log with an identical diagnostic while wasting timeout-bound
//! dials. [`WitnessProbeSchedule`] governs the dials — per-peer,
//! per-candidate-address exponential backoff with deterministic jitter and
//! a cap (never a cutoff: witnessing stays fail-open, every address is
//! eventually re-probed), candidate ordering that prefers addresses that
//! have ever answered, and state-change-only diagnostics (first failure,
//! backoff escalation, recovery — each carrying the backoff horizon)
//! instead of one line per failed attempt. This is scheduling and logging
//! only; witness-verdict semantics are untouched.

use std::collections::{HashMap, HashSet};
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
use crate::peer::transport::intendant::{
    TransportCredentials, PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE,
};
use crate::peer::transport::ws_url_to_http_base;
use crate::peer::PeerWitnessVantage;

const LEDGER_PATH: &str = "/api/hosted-control/certificate-ledger";
const LEDGER_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const LEDGER_RESPONSE_CAP: usize = 64 * 1024;
const CERTIFICATE_DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const WITNESS_INITIAL_DELAY: Duration = Duration::from_secs(2 * 60);
const WITNESS_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// First backoff horizon after a probe target's first consecutive failure.
const PROBE_BACKOFF_BASE: Duration = Duration::from_secs(30);
/// Ceiling for a failing probe target's backoff horizon. A cap, not a
/// cutoff — witness checks stay fail-open: even a persistently unreachable
/// address is re-probed at least this often (pre-jitter).
const PROBE_BACKOFF_CAP: Duration = Duration::from_secs(15 * 60);

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
            TransportSpec::IntendantWs { url, .. } => {
                Some(format!("{}{LEDGER_PATH}", ws_url_to_http_base(url)))
            }
            _ => None,
        })
        .collect();
    let mut seen = HashSet::new();
    endpoints.retain(|endpoint| seen.insert(endpoint.clone()));
    endpoints
}

// ---------------------------------------------------------------------------
// Probe scheduling (backoff + state-change diagnostics)
// ---------------------------------------------------------------------------

/// A probe state change worth exactly one log line. Steady states — healthy
/// attempts and repeat failures at the backoff cap — stay silent.
#[derive(Debug, PartialEq, Eq)]
enum ProbeTransition {
    /// A healthy (or never-tried) target failed: first line of this outage.
    FirstFailure { horizon: Duration },
    /// A further failure grew the backoff (it had not reached the cap yet).
    BackoffEscalated { horizon: Duration },
    /// A failing target answered again.
    Recovered,
}

/// One probe target's scheduling state (a ledger candidate address or the
/// fleet origin). Pure data driven by an injected millisecond clock — tests
/// never sleep.
#[derive(Debug)]
struct ProbeState {
    /// Consecutive failures since the last success (0 = healthy).
    consecutive_failures: u32,
    /// Whether this target has ever answered — ranks candidates that have
    /// worked ahead of persistently-failing strangers.
    ever_succeeded: bool,
    /// Nominal (pre-jitter) backoff currently in force; zero when healthy.
    backoff: Duration,
    /// Injected-clock instant (ms) from which the next attempt is allowed.
    eligible_at_ms: u64,
    /// Deterministic jitter phase (FNV-1a of the probe key) — decorrelates
    /// targets while keeping tests reproducible, the same scheme as the
    /// peer actor's reconnect backoff.
    seed: u32,
}

impl ProbeState {
    fn new(key: &str) -> Self {
        Self {
            consecutive_failures: 0,
            ever_succeeded: false,
            backoff: Duration::ZERO,
            eligible_at_ms: 0,
            seed: probe_seed(key),
        }
    }

    fn eligible(&self, now_ms: u64) -> bool {
        now_ms >= self.eligible_at_ms
    }

    fn record_success(&mut self) -> Option<ProbeTransition> {
        let recovered = self.consecutive_failures > 0;
        self.consecutive_failures = 0;
        self.ever_succeeded = true;
        self.backoff = Duration::ZERO;
        self.eligible_at_ms = 0;
        recovered.then_some(ProbeTransition::Recovered)
    }

    /// Advance the backoff ladder and re-arm the horizon. Returns the
    /// state change to log, if any: the first failure and every escalation
    /// get one line; repeat failures at the cap are silent (the horizon
    /// still re-arms, so the target keeps being re-probed).
    fn record_failure(&mut self, now_ms: u64) -> Option<ProbeTransition> {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let previous = self.backoff;
        self.backoff = if self.consecutive_failures == 1 {
            PROBE_BACKOFF_BASE
        } else {
            (previous * 2).min(PROBE_BACKOFF_CAP)
        };
        // ±20% deterministic jitter, stepping through 40 positions by the
        // consecutive-failure count, phase-shifted per target.
        let jitter_bps =
            ((i64::from(self.consecutive_failures) * 137) + i64::from(self.seed)) % 40 - 20;
        let horizon_ms =
            ((self.backoff.as_millis() as i64) * (100 + jitter_bps) / 100).max(0) as u64;
        let horizon = Duration::from_millis(horizon_ms);
        self.eligible_at_ms = now_ms.saturating_add(horizon_ms);
        if self.consecutive_failures == 1 {
            Some(ProbeTransition::FirstFailure { horizon })
        } else if self.backoff > previous {
            Some(ProbeTransition::BackoffEscalated { horizon })
        } else {
            None
        }
    }
}

/// Stable jitter seed from the probe key (FNV-1a over the bytes).
fn probe_seed(key: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in key.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// One peer's probe targets plus its residual-diagnostic dedupe state.
#[derive(Debug, Default)]
struct PeerProbes {
    /// Ledger-candidate probes, keyed by candidate endpoint URL.
    candidates: HashMap<String, ProbeState>,
    /// The fleet-origin dial probe; replaced when the signed ledger names
    /// a different origin (fresh targets start eligible).
    fleet: Option<(String, ProbeState)>,
    /// Last non-dial diagnostic (missing routes, report build/submit
    /// errors), logged only when its text changes.
    residual: Option<String>,
}

/// Per-peer, per-address dial scheduler for the witness loop. Pure state:
/// callers inject the clock (`now_ms`, any monotonic millisecond source)
/// and emit the log lines the returned [`ProbeTransition`]s call for.
/// Backoff here is connectivity scheduling only — it never contributes to,
/// or suppresses, a certificate verdict.
#[derive(Debug, Default)]
struct WitnessProbeSchedule {
    peers: HashMap<String, PeerProbes>,
}

impl WitnessProbeSchedule {
    /// Filter and order one peer's advertised candidate addresses for this
    /// sweep: drop candidates whose backoff horizon has not elapsed, rank
    /// addresses that have ever answered ahead of ones that never have
    /// (stable within each group, preserving the card's fallback order),
    /// and prune state for addresses the card no longer advertises.
    fn plan_candidates(&mut self, peer: &str, advertised: &[String], now_ms: u64) -> Vec<String> {
        let probes = self.peers.entry(peer.to_string()).or_default();
        probes
            .candidates
            .retain(|endpoint, _| advertised.iter().any(|advert| advert == endpoint));
        let mut planned: Vec<&String> = advertised
            .iter()
            .filter(|endpoint| {
                probes
                    .candidates
                    .get(*endpoint)
                    .is_none_or(|probe| probe.eligible(now_ms))
            })
            .collect();
        planned.sort_by_key(|endpoint| {
            probes
                .candidates
                .get(*endpoint)
                .is_none_or(|probe| !probe.ever_succeeded)
        });
        planned.into_iter().cloned().collect()
    }

    fn record_candidate_success(&mut self, peer: &str, endpoint: &str) -> Option<ProbeTransition> {
        self.candidate_probe(peer, endpoint).record_success()
    }

    fn record_candidate_failure(
        &mut self,
        peer: &str,
        endpoint: &str,
        now_ms: u64,
    ) -> Option<ProbeTransition> {
        self.candidate_probe(peer, endpoint).record_failure(now_ms)
    }

    fn candidate_probe(&mut self, peer: &str, endpoint: &str) -> &mut ProbeState {
        self.peers
            .entry(peer.to_string())
            .or_default()
            .candidates
            .entry(endpoint.to_string())
            .or_insert_with(|| ProbeState::new(&format!("{peer}|{endpoint}")))
    }

    /// Whether the fleet-origin dial may run now.
    fn fleet_origin_eligible(&mut self, peer: &str, origin: &str, now_ms: u64) -> bool {
        self.fleet_probe(peer, origin).eligible(now_ms)
    }

    fn record_fleet_success(&mut self, peer: &str, origin: &str) -> Option<ProbeTransition> {
        self.fleet_probe(peer, origin).record_success()
    }

    fn record_fleet_failure(
        &mut self,
        peer: &str,
        origin: &str,
        now_ms: u64,
    ) -> Option<ProbeTransition> {
        self.fleet_probe(peer, origin).record_failure(now_ms)
    }

    fn fleet_probe(&mut self, peer: &str, origin: &str) -> &mut ProbeState {
        let probes = self.peers.entry(peer.to_string()).or_default();
        let stale = probes
            .fleet
            .as_ref()
            .is_none_or(|(current, _)| current != origin);
        if stale {
            probes.fleet = Some((
                origin.to_string(),
                ProbeState::new(&format!("{peer}|fleet|{origin}")),
            ));
        }
        &mut probes
            .fleet
            .as_mut()
            .expect("fleet probe installed above")
            .1
    }

    /// Record a non-dial diagnostic; true when it should be logged (first
    /// occurrence, or its text changed).
    fn record_residual(&mut self, peer: &str, message: &str) -> bool {
        let probes = self.peers.entry(peer.to_string()).or_default();
        if probes.residual.as_deref() == Some(message) {
            return false;
        }
        probes.residual = Some(message.to_string());
        true
    }

    /// Clear the residual diagnostic after a fully successful pass; true
    /// when one was standing (worth a recovery line).
    fn clear_residual(&mut self, peer: &str) -> bool {
        self.peers
            .get_mut(peer)
            .is_some_and(|probes| probes.residual.take().is_some())
    }

    /// Drop scheduling state for peers no longer in the registry.
    fn retain_peers(&mut self, live: &HashSet<String>) {
        self.peers.retain(|peer, _| live.contains(peer));
    }
}

/// Human horizon for the state-change lines: seconds under two minutes,
/// rounded-up minutes beyond.
fn format_backoff_horizon(horizon: Duration) -> String {
    let secs = horizon.as_secs();
    if secs < 120 {
        format!("{secs}s")
    } else {
        format!("{}m", secs.div_ceil(60))
    }
}

/// One line per probe state change; `error` carries the failing attempt's
/// text (absent for recoveries). The horizon tells operators when the next
/// attempt is due, so a quiet log still reads as alive.
fn log_probe_transition(
    peer: &str,
    target: &str,
    error: Option<&str>,
    transition: &ProbeTransition,
) {
    match transition {
        ProbeTransition::FirstFailure { horizon } => eprintln!(
            "[hosted-control] certificate witness probe for peer {peer} failing at {target}: {} \
             (next attempt in ~{}; repeats logged only on backoff change)",
            error.unwrap_or("unknown error"),
            format_backoff_horizon(*horizon)
        ),
        ProbeTransition::BackoffEscalated { horizon } => eprintln!(
            "[hosted-control] certificate witness probe for peer {peer} still failing at {target}: \
             {} (backing off, next attempt in ~{})",
            error.unwrap_or("unknown error"),
            format_backoff_horizon(*horizon)
        ),
        ProbeTransition::Recovered => eprintln!(
            "[hosted-control] certificate witness probe for peer {peer} recovered at {target}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Ledger fetch
// ---------------------------------------------------------------------------

/// Build the direct-route HTTPS client from the peer's transport
/// credentials (same pinned/mTLS policy as the peer transport).
fn build_ledger_client(credentials: &TransportCredentials) -> Result<reqwest::Client, String> {
    let tls_config = credentials
        .tls
        .client_config_for_policy(
            &credentials.effective_tls_policy(),
            credentials.client_identity.as_ref(),
        )
        .map_err(|error| format!("build peer ledger TLS policy: {error}"))?;
    let mut client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(tls_config) = tls_config {
        client = client.use_preconfigured_tls(rustls::ClientConfig::clone(&tls_config));
    }
    client
        .build()
        .map_err(|error| format!("build peer ledger client: {error}"))
}

/// The result of walking one planned candidate list: the first ledger that
/// answered (with the address that produced it), plus every failed attempt
/// before it in attempt order — so the caller can record each address's
/// outcome in the schedule.
struct CandidateSweep {
    ledger: Option<(String, HostedCertificateLedger)>,
    failures: Vec<(String, String)>,
}

async fn fetch_certificate_ledger_from_candidates(
    client: &reqwest::Client,
    endpoints: &[String],
    bearer_token: Option<&str>,
) -> CandidateSweep {
    let mut failures = Vec::new();
    for endpoint in endpoints {
        match fetch_certificate_ledger_from_endpoint(client, endpoint, bearer_token).await {
            Ok(ledger) => {
                return CandidateSweep {
                    ledger: Some((endpoint.clone(), ledger)),
                    failures,
                }
            }
            Err(error) => failures.push((endpoint.clone(), error)),
        }
    }
    CandidateSweep {
        ledger: None,
        failures,
    }
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

/// One peer's sweep outcome. Probe connectivity state changes are logged
/// inline where they are recorded; the caller only maintains the residual
/// diagnostic from this.
enum ObserveOutcome {
    /// Peer disconnected or witnessing unsupported; nothing attempted.
    Idle,
    /// Every would-be dial is inside its backoff horizon; nothing dialed.
    Skipped,
    /// A dial failed; its state change (if any) is already logged.
    ProbeFailed,
    /// The full pass ran (serial in ledger, or a witness report submitted).
    Complete,
}

/// Milliseconds since the loop's epoch — the injected monotonic clock the
/// probe schedule runs on.
fn probe_now_ms(epoch: std::time::Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

async fn observe_peer_once(
    runtime: &HostedControlRuntime,
    handle: &PeerHandle,
    schedule: &tokio::sync::Mutex<WitnessProbeSchedule>,
    epoch: std::time::Instant,
) -> Result<ObserveOutcome, String> {
    if !handle.is_connected() || !handle.features().certificate_witness {
        return Ok(ObserveOutcome::Idle);
    }
    let peer_id = handle.id().to_string();
    let advertised = certificate_ledger_endpoints(&handle.card_snapshot());
    if advertised.is_empty() {
        return Err(format!(
            "peer {peer_id} advertises no direct route for a certificate ledger"
        ));
    }
    let candidates =
        schedule
            .lock()
            .await
            .plan_candidates(&peer_id, &advertised, probe_now_ms(epoch));
    if candidates.is_empty() {
        return Ok(ObserveOutcome::Skipped);
    }
    let credentials = handle.transport_credentials();
    let client = build_ledger_client(credentials)?;
    let sweep = fetch_certificate_ledger_from_candidates(
        &client,
        &candidates,
        credentials.bearer_token.as_deref(),
    )
    .await;
    {
        let mut schedule = schedule.lock().await;
        let now_ms = probe_now_ms(epoch);
        for (endpoint, error) in &sweep.failures {
            if let Some(transition) = schedule.record_candidate_failure(&peer_id, endpoint, now_ms)
            {
                log_probe_transition(&peer_id, endpoint, Some(error.as_str()), &transition);
            }
        }
        if let Some((endpoint, _)) = &sweep.ledger {
            if let Some(transition) = schedule.record_candidate_success(&peer_id, endpoint) {
                log_probe_transition(&peer_id, endpoint, None, &transition);
            }
        }
    }
    let Some((_, ledger)) = sweep.ledger else {
        return Ok(ObserveOutcome::ProbeFailed);
    };
    let origin = ledger.fleet_origin.clone();
    if !schedule
        .lock()
        .await
        .fleet_origin_eligible(&peer_id, &origin, probe_now_ms(epoch))
    {
        return Ok(ObserveOutcome::Skipped);
    }
    let observation = match observe_fleet_certificate(&ledger).await {
        Ok(observation) => {
            if let Some(transition) = schedule
                .lock()
                .await
                .record_fleet_success(&peer_id, &origin)
            {
                log_probe_transition(&peer_id, &origin, None, &transition);
            }
            observation
        }
        Err(error) => {
            if let Some(transition) =
                schedule
                    .lock()
                    .await
                    .record_fleet_failure(&peer_id, &origin, probe_now_ms(epoch))
            {
                log_probe_transition(&peer_id, &origin, Some(error.as_str()), &transition);
            }
            return Ok(ObserveOutcome::ProbeFailed);
        }
    };
    if ledger.serials.contains(&observation.serial_hex) {
        return Ok(ObserveOutcome::Complete);
    }
    let vantage = effective_peer_vantage(observation.vantage, handle.certificate_witness_vantage());
    let report = runtime.build_peer_witness_report(&ledger, &observation.serial_hex, vantage)?;
    handle
        .submit_certificate_witness(report)
        .await
        .map_err(|error| format!("submit peer certificate witness: {error}"))?;
    Ok(ObserveOutcome::Complete)
}

async fn observe_all_peers(
    runtime: Arc<HostedControlRuntime>,
    registry: PeerRegistry,
    schedule: Arc<tokio::sync::Mutex<WitnessProbeSchedule>>,
    epoch: std::time::Instant,
) {
    let handles = registry.list();
    {
        let live: HashSet<String> = handles
            .iter()
            .map(|handle| handle.id().to_string())
            .collect();
        schedule.lock().await.retain_peers(&live);
    }
    let mut tasks = tokio::task::JoinSet::new();
    for handle in handles {
        let runtime = Arc::clone(&runtime);
        let schedule = Arc::clone(&schedule);
        tasks.spawn(async move {
            let peer_id = handle.id().to_string();
            let outcome = observe_peer_once(&runtime, &handle, &schedule, epoch).await;
            (peer_id, outcome)
        });
    }
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((peer_id, Ok(outcome))) => {
                if matches!(outcome, ObserveOutcome::Complete)
                    && schedule.lock().await.clear_residual(&peer_id)
                {
                    eprintln!(
                        "[hosted-control] certificate witness diagnostic cleared for peer {peer_id}"
                    );
                }
            }
            Ok((peer_id, Err(error))) => {
                if schedule.lock().await.record_residual(&peer_id, &error) {
                    eprintln!(
                        "[hosted-control] certificate witness diagnostic for peer {peer_id}: \
                         {error} (repeats suppressed until the diagnostic changes)"
                    );
                }
            }
            Err(error) => {
                eprintln!("[hosted-control] certificate witness task failed: {error}");
            }
        }
    }
}

pub fn spawn_certificate_witness_loop(runtime: Arc<HostedControlRuntime>, registry: PeerRegistry) {
    tokio::spawn(async move {
        let schedule = Arc::new(tokio::sync::Mutex::new(WitnessProbeSchedule::default()));
        let epoch = std::time::Instant::now();
        tokio::time::sleep(WITNESS_INITIAL_DELAY).await;
        let mut interval = tokio::time::interval(WITNESS_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            observe_all_peers(
                Arc::clone(&runtime),
                registry.clone(),
                Arc::clone(&schedule),
                epoch,
            )
            .await;
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
            identity_attestation: None,
        }
    }

    #[test]
    fn ledger_endpoints_preserve_direct_peer_route_fallback_order() {
        assert_eq!(
            certificate_ledger_endpoints(&card(vec![
                TransportSpec::IntendantWs {
                    url: "wss://dead.example.test:9443/ws".to_string(),
                    relay: false,
                },
                TransportSpec::IntendantWs {
                    url: "wss://peer.example.test:9443/ws".to_string(),
                    relay: false,
                },
                TransportSpec::IntendantWs {
                    url: "wss://dead.example.test:9443/ws".to_string(),
                    relay: false,
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

        let sweep = fetch_certificate_ledger_from_candidates(
            &client,
            &[first.clone(), second.clone()],
            None,
        )
        .await;

        let (endpoint, fetched) = sweep.ledger.unwrap();
        assert_eq!(endpoint, second);
        assert_eq!(fetched, expected);
        assert_eq!(sweep.failures.len(), 1);
        assert_eq!(sweep.failures[0].0, first);
        assert!(sweep.failures[0].1.contains("503"));
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

    // -----------------------------------------------------------------
    // Probe schedule: hermetic state-machine tests. Time is injected as
    // plain milliseconds — no sleeps, no network, no tokio clock.
    // -----------------------------------------------------------------

    #[test]
    fn probe_backoff_escalates_to_the_cap_and_never_retires() {
        let mut schedule = WitnessProbeSchedule::default();
        let advertised = vec!["https://a.example.test:9443/x".to_string()];
        let mut now_ms = 0_u64;
        assert_eq!(
            schedule.plan_candidates("peer", &advertised, now_ms),
            advertised
        );
        let Some(ProbeTransition::FirstFailure { horizon }) =
            schedule.record_candidate_failure("peer", &advertised[0], now_ms)
        else {
            panic!("first failure must produce a state change");
        };
        // Base 30s, ±20% jitter.
        assert!(
            (24..=36).contains(&horizon.as_secs()),
            "first horizon {horizon:?}"
        );
        // Inside the horizon the candidate is not planned; at it, it is —
        // failing addresses are deferred, never retired.
        assert!(schedule
            .plan_candidates("peer", &advertised, now_ms + 1)
            .is_empty());
        now_ms += horizon.as_millis() as u64;
        assert_eq!(
            schedule.plan_candidates("peer", &advertised, now_ms),
            advertised
        );
        // Escalations double the nominal backoff up to the cap, one
        // logged transition per step; at the cap further failures are
        // silent. Ladder: 30s, 60s, 120s, 240s, 480s, 900s.
        let mut horizons = vec![horizon];
        loop {
            match schedule.record_candidate_failure("peer", &advertised[0], now_ms) {
                Some(ProbeTransition::BackoffEscalated { horizon }) => {
                    horizons.push(horizon);
                    now_ms += horizon.as_millis() as u64;
                }
                None => break,
                other => panic!("unexpected transition {other:?}"),
            }
        }
        assert_eq!(horizons.len(), 6, "six logged steps then silence");
        for (step, horizon) in horizons.iter().enumerate() {
            let nominal = std::cmp::min(30 * (1_u64 << step), PROBE_BACKOFF_CAP.as_secs());
            assert!(
                (nominal * 4 / 5..=nominal * 6 / 5).contains(&horizon.as_secs()),
                "step {step}: horizon {horizon:?} outside jitter window of {nominal}s"
            );
        }
        // The silent at-cap failure still re-armed the horizon…
        assert!(schedule
            .plan_candidates("peer", &advertised, now_ms + 1)
            .is_empty());
        // …and the cap bounds it: after at most a jittered cap the address
        // is probed again. Fail-open — never permanent.
        now_ms += PROBE_BACKOFF_CAP.as_millis() as u64 * 6 / 5;
        assert_eq!(
            schedule.plan_candidates("peer", &advertised, now_ms),
            advertised
        );
    }

    #[test]
    fn candidates_that_ever_answered_rank_ahead_of_strangers() {
        let mut schedule = WitnessProbeSchedule::default();
        let first = "https://a.example.test:9443/x".to_string();
        let second = "https://b.example.test:9443/x".to_string();
        let advertised = vec![first.clone(), second.clone()];
        // Card order wins while nothing has answered.
        assert_eq!(schedule.plan_candidates("peer", &advertised, 0), advertised);
        // Once the second answers it leads, though both stay planned.
        assert!(schedule.record_candidate_success("peer", &second).is_none());
        assert_eq!(
            schedule.plan_candidates("peer", &advertised, 0),
            vec![second.clone(), first.clone()]
        );
        // Two proven addresses fall back to the card's order.
        assert!(schedule.record_candidate_success("peer", &first).is_none());
        assert_eq!(schedule.plan_candidates("peer", &advertised, 0), advertised);
    }

    #[test]
    fn recovery_logs_once_and_resets_the_backoff_ladder() {
        let mut schedule = WitnessProbeSchedule::default();
        let endpoint = "https://a.example.test:9443/x".to_string();
        schedule.record_candidate_failure("peer", &endpoint, 0);
        schedule.record_candidate_failure("peer", &endpoint, 0);
        assert_eq!(
            schedule.record_candidate_success("peer", &endpoint),
            Some(ProbeTransition::Recovered)
        );
        // Healthy repeat successes stay silent.
        assert!(schedule
            .record_candidate_success("peer", &endpoint)
            .is_none());
        // The ladder restarts at the base after a recovery.
        let Some(ProbeTransition::FirstFailure { horizon }) =
            schedule.record_candidate_failure("peer", &endpoint, 0)
        else {
            panic!("post-recovery failure must be a fresh first failure");
        };
        assert!((24..=36).contains(&horizon.as_secs()));
    }

    #[test]
    fn candidate_state_is_pruned_when_the_card_stops_advertising_it() {
        let mut schedule = WitnessProbeSchedule::default();
        let dropped = "https://a.example.test:9443/x".to_string();
        let kept = "https://b.example.test:9443/x".to_string();
        schedule.record_candidate_failure("peer", &dropped, 0);
        assert_eq!(
            schedule.plan_candidates("peer", &[dropped.clone(), kept.clone()], 0),
            vec![kept.clone()]
        );
        // The card stops advertising the failing address; its state goes
        // with it, so a later re-advertisement starts fresh and eligible.
        schedule.plan_candidates("peer", &[kept.clone()], 0);
        assert_eq!(
            schedule.plan_candidates("peer", &[dropped.clone(), kept.clone()], 0),
            vec![dropped, kept]
        );
    }

    #[test]
    fn fleet_origin_probe_backs_off_and_resets_on_origin_change() {
        let mut schedule = WitnessProbeSchedule::default();
        let origin = "https://fleet.example.test";
        assert!(schedule.fleet_origin_eligible("peer", origin, 0));
        let Some(ProbeTransition::FirstFailure { horizon }) =
            schedule.record_fleet_failure("peer", origin, 0)
        else {
            panic!("first fleet failure must produce a state change");
        };
        assert!(!schedule.fleet_origin_eligible("peer", origin, 1));
        assert!(schedule.fleet_origin_eligible("peer", origin, horizon.as_millis() as u64));
        // A signed ledger naming a different origin is a fresh probe,
        // eligible immediately even while the old horizon stands.
        schedule.record_fleet_failure("peer", origin, 0);
        assert!(schedule.fleet_origin_eligible("peer", "https://other.example.test", 0));
    }

    #[test]
    fn residual_diagnostics_log_only_on_change() {
        let mut schedule = WitnessProbeSchedule::default();
        assert!(schedule.record_residual("peer", "submit failed: x"));
        assert!(!schedule.record_residual("peer", "submit failed: x"));
        assert!(schedule.record_residual("peer", "submit failed: y"));
        assert!(schedule.clear_residual("peer"));
        assert!(!schedule.clear_residual("peer"));
        assert!(schedule.record_residual("peer", "submit failed: x"));
    }

    #[test]
    fn departed_peers_scheduling_state_is_dropped() {
        let mut schedule = WitnessProbeSchedule::default();
        let endpoint = "https://a.example.test:9443/x".to_string();
        schedule.record_candidate_failure("gone", &endpoint, 0);
        schedule.record_candidate_failure("stays", &endpoint, 0);
        schedule.retain_peers(&HashSet::from(["stays".to_string()]));
        // The departed peer's backoff is forgotten; the survivor's stands.
        assert_eq!(
            schedule.plan_candidates("gone", &[endpoint.clone()], 0),
            vec![endpoint.clone()]
        );
        assert!(schedule.plan_candidates("stays", &[endpoint], 0).is_empty());
    }

    #[test]
    fn jitter_is_deterministic_per_target() {
        let mut first = ProbeState::new("peer|https://a.example.test:9443/x");
        let mut second = ProbeState::new("peer|https://a.example.test:9443/x");
        assert_eq!(first.record_failure(0), second.record_failure(0));
        assert_eq!(first.record_failure(0), second.record_failure(0));
    }

    #[test]
    fn backoff_horizons_format_for_operators() {
        assert_eq!(format_backoff_horizon(Duration::from_secs(30)), "30s");
        assert_eq!(format_backoff_horizon(Duration::from_secs(96)), "96s");
        assert_eq!(format_backoff_horizon(Duration::from_secs(480)), "8m");
        assert_eq!(format_backoff_horizon(Duration::from_secs(1071)), "18m");
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
