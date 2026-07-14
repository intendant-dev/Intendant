//! Direct browser-to-peer file-transfer WebRTC sessions.
//!
//! The primary daemon only coordinates signaling. The peer daemon that owns
//! the file answers the browser's WebRTC offer, enforces the primary peer
//! identity's filesystem grants, and streams bytes over a data channel.

use crate::error::CallerError;
use crate::event::AppEvent;
use crate::peer::access_policy::{
    filesystem_access_canonical_subject, FilesystemAccessKind, FilesystemAccessPolicy,
    PeerOperation,
};
use bytes::BytesMut;
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCDtlsRole, RTCIceCandidate, RTCIceCandidateInit,
    RTCIceServer,
};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Seek as _, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

const TRANSFER_CHANNEL_LABEL: &str = "intendant-peer-file-transfer";
const UDP_BUF_LEN: usize = 2000;
const COMMAND_CHANNEL: usize = 64;
const CHUNK_BYTES: usize = 16 * 1024;
const MAX_READ_BYTES: u64 = 512 * 1024 * 1024;
const PENDING_CANDIDATES_PER_SESSION: usize = 64;
const MAX_PENDING_TRANSFER_RESERVATIONS: usize = 128;
const MAX_PENDING_TRANSFER_RESERVATIONS_PER_OWNER: usize = 8;
const PENDING_TRANSFER_RESERVATION_TTL: Duration = Duration::from_secs(60);
const LIVE_TRANSFER_AUTHORITY_RECHECK_INTERVAL: Duration = Duration::from_millis(250);
const TCP_OUT_QUEUE: usize = 256;

#[derive(Clone, Debug)]
pub struct PeerFileTransferAuthorization {
    pub fingerprint: String,
    pub label: String,
    pub profile: String,
    pub filesystem: FilesystemAccessPolicy,
    /// Exact peer identity that authenticated the opening and its
    /// daemon-owned store. Production sessions must carry both; `(None,
    /// None)` is reserved for hermetic unit fixtures.
    pub identity_record: Option<crate::peer::access_policy::PeerIdentityRecord>,
    pub iam_cert_dir: Option<PathBuf>,
}

impl PeerFileTransferAuthorization {
    fn is_current(&self) -> bool {
        match (&self.identity_record, &self.iam_cert_dir) {
            // Production construction always carries the exact opening
            // identity and its store. The empty pair exists only to keep
            // focused unit fixtures independent of machine state.
            #[cfg(test)]
            (None, None) => true,
            (Some(opening), Some(cert_dir)) => {
                let now_unix = crate::access::client_key::now_unix_ms() / 1000;
                matches!(
                    crate::peer::access_policy::lookup_identity(cert_dir, &self.fingerprint),
                    Ok(Some(current)) if current == *opening && current.is_active(now_unix)
                )
            }
            _ => false,
        }
    }

    fn access_principal(&self) -> crate::access::iam::AccessPrincipal {
        crate::access::iam::AccessPrincipal::peer_daemon(
            self.fingerprint.clone(),
            self.label.clone(),
            self.profile.clone(),
            "peer-file-transfer",
        )
    }
}

#[derive(Clone)]
pub struct PeerFileTransferRegistry {
    ice_config: crate::display::IceConfig,
    bus: crate::event::EventBus,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    peers: Arc<Mutex<HashMap<String, PeerFileTransferPeer>>>,
    pending_reservations: Arc<Mutex<HashMap<String, PendingTransferReservation>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerFileTransferSessionMutation {
    Applied,
    NotFound,
    Forbidden,
}

#[derive(Clone, Debug)]
struct PendingTransferReservation {
    owner_fingerprint: String,
    candidates: Vec<String>,
    created_at: Instant,
}

impl PeerFileTransferRegistry {
    pub fn new(
        ice_config: crate::display::IceConfig,
        bus: crate::event::EventBus,
        tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    ) -> Self {
        Self {
            ice_config,
            bus,
            tcp_peer_registry,
            peers: Arc::new(Mutex::new(HashMap::new())),
            pending_reservations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn answer_offer(
        &self,
        session_id: String,
        offer_sdp: String,
        authorization: PeerFileTransferAuthorization,
        advertise_tcp_via_url: Option<String>,
    ) -> Result<String, String> {
        if !authorization.is_current() {
            return Err(
                "peer file-transfer opening identity changed or is no longer active".into(),
            );
        }
        let owner_fingerprint = authorization.fingerprint.clone();
        let reservation_created_at = Instant::now();
        {
            let peers = self.peers.lock().await;
            if let Some(existing) = peers.get(&session_id) {
                return Err(if existing.belongs_to(&owner_fingerprint) {
                    "peer file-transfer session already exists".to_string()
                } else {
                    "peer file-transfer session belongs to another authenticated peer".to_string()
                });
            }
            let mut reservations = self.pending_reservations.lock().await;
            reserve_pending_transfer_session(
                &mut reservations,
                &session_id,
                &owner_fingerprint,
                reservation_created_at,
            )?;
        }
        let tcp_advertised_addr = match advertise_tcp_via_url.as_deref() {
            Some(url) if !url.is_empty() => {
                crate::web_gateway::resolve_url_to_socket_addr(url).await
            }
            _ => None,
        };
        let answer = PeerFileTransferPeer::answer_offer(
            session_id.clone(),
            offer_sdp,
            authorization,
            self.ice_config.clone(),
            self.bus.clone(),
            Arc::clone(&self.tcp_peer_registry),
            tcp_advertised_addr,
        )
        .await;
        let (peer, answer_sdp) = match answer {
            Ok(answer) => answer,
            Err(error) => {
                self.release_pending_reservation(&session_id, &owner_fingerprint)
                    .await;
                return Err(error.to_string());
            }
        };
        if !peer.opening_authority_is_current() {
            peer.close().await;
            self.release_pending_reservation(&session_id, &owner_fingerprint)
                .await;
            return Err("peer file-transfer identity changed during WebRTC setup".into());
        }
        let pending_candidates = {
            let mut peers = self.peers.lock().await;
            if let Some(existing) = peers.get(&session_id) {
                let message = if existing.belongs_to(&peer.owner_fingerprint) {
                    "peer file-transfer session already exists"
                } else {
                    "peer file-transfer session belongs to another authenticated peer"
                };
                drop(peers);
                peer.close().await;
                return Err(message.to_string());
            }
            let mut reservations = self.pending_reservations.lock().await;
            prune_expired_transfer_reservations(&mut reservations, Instant::now());
            let Some(reservation) = reservations.get(&session_id) else {
                drop(reservations);
                drop(peers);
                peer.close().await;
                return Err("peer file-transfer offer reservation expired or was closed".into());
            };
            if reservation.owner_fingerprint != owner_fingerprint
                || reservation.created_at != reservation_created_at
            {
                drop(reservations);
                drop(peers);
                peer.close().await;
                return Err(
                    "peer file-transfer offer reservation was replaced by another negotiation"
                        .into(),
                );
            }
            let pending = reservations
                .remove(&session_id)
                .expect("transfer reservation existed under the same lock")
                .candidates;
            peers.insert(session_id.clone(), peer.clone());
            pending
        };
        for candidate in pending_candidates {
            if let Err(error) = peer.add_ice_candidate(candidate).await {
                let _ = self.close_for_peer(&session_id, &owner_fingerprint).await;
                return Err(error);
            }
        }
        Ok(answer_sdp)
    }

    pub async fn add_ice_candidate_for_peer(
        &self,
        session_id: &str,
        candidate_json: &str,
        owner_fingerprint: &str,
    ) -> Result<PeerFileTransferSessionMutation, String> {
        let candidate: serde_json::Value =
            serde_json::from_str(candidate_json).map_err(|e| format!("invalid ICE JSON: {e}"))?;
        // ICE never creates state. The authenticated owner must first reserve
        // the caller-chosen id by sending an Offer; this keeps the pending map
        // bounded and prevents one peer from attaching to another's session.
        {
            let peers = self.peers.lock().await;
            if let Some(peer) = peers.get(session_id) {
                if !peer.belongs_to(owner_fingerprint) || !peer.opening_authority_is_current() {
                    return Ok(PeerFileTransferSessionMutation::Forbidden);
                }
            } else {
                let mut reservations = self.pending_reservations.lock().await;
                prune_expired_transfer_reservations(&mut reservations, Instant::now());
                match reservations.get(session_id) {
                    Some(reservation) if reservation.owner_fingerprint != owner_fingerprint => {
                        return Ok(PeerFileTransferSessionMutation::Forbidden)
                    }
                    Some(_) => {}
                    None => return Ok(PeerFileTransferSessionMutation::NotFound),
                }
            }
        }
        let candidate_str = candidate
            .get("candidate")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if candidate_str.is_empty() {
            return Ok(PeerFileTransferSessionMutation::Applied);
        }
        let resolved = match crate::display::webrtc::resolve_mdns_in_candidate(candidate_str).await
        {
            Ok(candidate) => candidate,
            Err(e) => {
                self.bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".into(),
                    source: "peer-file-transfer".into(),
                    content: format!("mDNS resolve failed for transfer ICE candidate: {e}"),
                    turn: None,
                });
                return Ok(PeerFileTransferSessionMutation::Applied);
            }
        };
        let peer = {
            let peers = self.peers.lock().await;
            if let Some(peer) = peers.get(session_id) {
                if !peer.belongs_to(owner_fingerprint) || !peer.opening_authority_is_current() {
                    return Ok(PeerFileTransferSessionMutation::Forbidden);
                }
                peer.clone()
            } else {
                let mut reservations = self.pending_reservations.lock().await;
                prune_expired_transfer_reservations(&mut reservations, Instant::now());
                let Some(reservation) = reservations.get_mut(session_id) else {
                    return Ok(PeerFileTransferSessionMutation::NotFound);
                };
                if reservation.owner_fingerprint != owner_fingerprint {
                    return Ok(PeerFileTransferSessionMutation::Forbidden);
                }
                if reservation.candidates.len() < PENDING_CANDIDATES_PER_SESSION {
                    reservation.candidates.push(resolved);
                }
                return Ok(PeerFileTransferSessionMutation::Applied);
            }
        };
        peer.add_ice_candidate(resolved).await?;
        Ok(PeerFileTransferSessionMutation::Applied)
    }

    pub async fn close_for_peer(
        &self,
        session_id: &str,
        owner_fingerprint: &str,
    ) -> PeerFileTransferSessionMutation {
        let peer = {
            let mut peers = self.peers.lock().await;
            let Some(existing) = peers.get(session_id) else {
                let mut reservations = self.pending_reservations.lock().await;
                prune_expired_transfer_reservations(&mut reservations, Instant::now());
                return match reservations.get(session_id) {
                    Some(existing) if existing.owner_fingerprint != owner_fingerprint => {
                        PeerFileTransferSessionMutation::Forbidden
                    }
                    Some(_) => {
                        reservations.remove(session_id);
                        PeerFileTransferSessionMutation::Applied
                    }
                    None => PeerFileTransferSessionMutation::NotFound,
                };
            };
            if !existing.belongs_to(owner_fingerprint) {
                return PeerFileTransferSessionMutation::Forbidden;
            }
            peers
                .remove(session_id)
                .expect("peer file-transfer session existed under the same lock")
        };
        peer.close().await;
        self.release_pending_reservation(session_id, owner_fingerprint)
            .await;
        PeerFileTransferSessionMutation::Applied
    }

    async fn release_pending_reservation(&self, session_id: &str, owner_fingerprint: &str) {
        let mut reservations = self.pending_reservations.lock().await;
        if reservations
            .get(session_id)
            .is_some_and(|reservation| reservation.owner_fingerprint == owner_fingerprint)
        {
            reservations.remove(session_id);
        }
    }
}

fn prune_expired_transfer_reservations(
    reservations: &mut HashMap<String, PendingTransferReservation>,
    now: Instant,
) {
    reservations.retain(|_, reservation| {
        now.saturating_duration_since(reservation.created_at) < PENDING_TRANSFER_RESERVATION_TTL
    });
}

fn reserve_pending_transfer_session(
    reservations: &mut HashMap<String, PendingTransferReservation>,
    session_id: &str,
    owner_fingerprint: &str,
    now: Instant,
) -> Result<(), String> {
    prune_expired_transfer_reservations(reservations, now);
    if let Some(existing) = reservations.get(session_id) {
        return Err(if existing.owner_fingerprint == owner_fingerprint {
            "peer file-transfer session is already being negotiated".to_string()
        } else {
            "peer file-transfer session belongs to another authenticated peer".to_string()
        });
    }
    if reservations.len() >= MAX_PENDING_TRANSFER_RESERVATIONS {
        return Err("too many pending peer file-transfer offers".to_string());
    }
    let owner_count = reservations
        .values()
        .filter(|reservation| reservation.owner_fingerprint == owner_fingerprint)
        .count();
    if owner_count >= MAX_PENDING_TRANSFER_RESERVATIONS_PER_OWNER {
        return Err("authenticated peer has too many pending file-transfer offers".to_string());
    }
    reservations.insert(
        session_id.to_string(),
        PendingTransferReservation {
            owner_fingerprint: owner_fingerprint.to_string(),
            candidates: Vec::new(),
            created_at: now,
        },
    );
    Ok(())
}

#[derive(Clone)]
struct PeerFileTransferPeer {
    command_tx: mpsc::Sender<TransferCommand>,
    shutdown: CancellationToken,
    owner_fingerprint: String,
    opening_authorization: PeerFileTransferAuthorization,
}

impl PeerFileTransferPeer {
    async fn answer_offer(
        session_id: String,
        offer_sdp: String,
        authorization: PeerFileTransferAuthorization,
        ice_config: crate::display::IceConfig,
        bus: crate::event::EventBus,
        tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
        tcp_advertised_addr: Option<SocketAddr>,
    ) -> Result<(Self, String), CallerError> {
        let mut setting_engine = SettingEngine::default();
        setting_engine
            .set_answering_dtls_role(RTCDtlsRole::Server)
            .map_err(|e| CallerError::WebRtc(format!("set transfer DTLS role: {e}")))?;

        let rtc_config = RTCConfigurationBuilder::new()
            .with_ice_servers(to_rtc_ice_servers(&ice_config.ice_servers))
            .build();
        let mut rtc = RTCPeerConnectionBuilder::new()
            .with_configuration(rtc_config)
            .with_setting_engine(setting_engine)
            .build()
            .map_err(|e| CallerError::WebRtc(format!("build transfer rtc peer: {e}")))?;

        let tcp_advertised = tcp_advertised_addr
            .filter(|addr| !addr.ip().is_loopback() && !addr.ip().is_unspecified());
        let all_local_addrs = crate::access::routable_local_addrs(true);
        let local_addrs: Vec<std::net::IpAddr> = match tcp_advertised.map(|addr| addr.ip()) {
            Some(preferred) if all_local_addrs.contains(&preferred) => vec![preferred],
            _ => all_local_addrs,
        };
        let mut sockets = Vec::new();
        for ip in local_addrs {
            let socket = match UdpSocket::bind(SocketAddr::new(ip, 0)).await {
                Ok(socket) => socket,
                Err(e) => {
                    eprintln!("[peer-file-transfer] skipping UDP bind on {ip}: {e}");
                    continue;
                }
            };
            let local = match socket.local_addr() {
                Ok(local) => local,
                Err(e) => {
                    eprintln!("[peer-file-transfer] skipping UDP socket on {ip}: {e}");
                    continue;
                }
            };
            let candidate = udp_host_candidate_init(local)?;
            match rtc.add_local_candidate(candidate) {
                Ok(()) => sockets.push(Arc::new(socket)),
                Err(e) => {
                    eprintln!("[peer-file-transfer] skipping UDP host candidate {local}: {e}")
                }
            }
        }
        if sockets.is_empty() {
            return Err(CallerError::WebRtc(
                "no usable local UDP sockets bound for peer file transfer".into(),
            ));
        }

        if let Some(addr) = tcp_advertised {
            match rtc.add_local_candidate(tcp_host_candidate_init(addr)) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("[peer-file-transfer] failed to add ICE-TCP candidate {addr}: {e}")
                }
            }
        } else if tcp_advertised_addr.is_some() {
            eprintln!(
                "[peer-file-transfer] not advertising ICE-TCP candidate from unsuitable address {tcp_advertised_addr:?}"
            );
        }

        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| CallerError::WebRtc(format!("parse transfer offer: {e}")))?;
        rtc.set_remote_description(offer)
            .map_err(|e| CallerError::WebRtc(format!("set transfer remote offer: {e}")))?;
        let answer = rtc
            .create_answer(None)
            .map_err(|e| CallerError::WebRtc(format!("create transfer answer: {e}")))?;
        rtc.set_local_description(answer.clone())
            .map_err(|e| CallerError::WebRtc(format!("set transfer local answer: {e}")))?;

        let mut tcp_registration = None;
        let mut tcp_conn_rx = None;
        if tcp_advertised.is_some() {
            match crate::display::webrtc::parse_sdp_ice_ufrag(&answer.sdp) {
                Some(ufrag) => {
                    let (registration, rx) = tcp_peer_registry.register(ufrag);
                    tcp_registration = Some(registration);
                    tcp_conn_rx = Some(rx);
                }
                None => {
                    eprintln!("[peer-file-transfer] answer SDP had no ice-ufrag; ICE-TCP disabled")
                }
            }
        }

        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL);
        let shutdown = CancellationToken::new();
        let owner_fingerprint = authorization.fingerprint.clone();
        let opening_authorization = authorization.clone();
        tokio::spawn(transfer_driver(
            session_id,
            rtc,
            sockets,
            authorization,
            bus,
            command_tx.clone(),
            command_rx,
            shutdown.clone(),
            tcp_conn_rx,
            tcp_advertised,
            tcp_registration,
        ));
        Ok((
            Self {
                command_tx,
                shutdown,
                owner_fingerprint,
                opening_authorization,
            },
            answer.sdp,
        ))
    }

    async fn add_ice_candidate(&self, candidate: String) -> Result<(), String> {
        self.command_tx
            .send(TransferCommand::AddIceCandidate(candidate))
            .await
            .map_err(|_| "peer file-transfer driver gone".to_string())
    }

    fn belongs_to(&self, owner_fingerprint: &str) -> bool {
        self.owner_fingerprint == owner_fingerprint
    }

    fn opening_authority_is_current(&self) -> bool {
        self.opening_authorization.is_current()
    }

    async fn close(self) {
        self.shutdown.cancel();
    }
}

#[derive(Debug)]
struct InboundPacket {
    proto: TransportProtocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    received_at: Instant,
}

#[derive(Debug)]
enum TransferCommand {
    AddIceCandidate(String),
    SendText(String),
    SendBinary(Vec<u8>),
    ReadFinished(String),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum TransferRequest {
    Read {
        id: String,
        path: String,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        length: Option<u64>,
    },
    Cancel {
        id: String,
    },
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
async fn transfer_driver<I: rtc::interceptor::Interceptor + Send + Sync + 'static>(
    session_id: String,
    mut rtc: RTCPeerConnection<I>,
    sockets: Vec<Arc<UdpSocket>>,
    authorization: PeerFileTransferAuthorization,
    bus: crate::event::EventBus,
    command_tx: mpsc::Sender<TransferCommand>,
    mut command_rx: mpsc::Receiver<TransferCommand>,
    shutdown: CancellationToken,
    mut tcp_conn_rx: Option<mpsc::Receiver<crate::display::webrtc::AcceptedTcpConnection>>,
    tcp_advertised: Option<SocketAddr>,
    _tcp_registration: Option<crate::display::webrtc::PeerRegistration>,
) {
    let mut sockets_by_addr: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundPacket>(64);
    let mut tcp_senders: HashMap<SocketAddr, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut forwarder_handles = Vec::new();
    for sock in sockets {
        let local = match sock.local_addr() {
            Ok(local) => local,
            Err(_) => continue,
        };
        sockets_by_addr.insert(local, Arc::clone(&sock));
        let tx = inbound_tx.clone();
        let shutdown = shutdown.clone();
        forwarder_handles.push(tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_BUF_LEN];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    recv = sock.recv_from(&mut buf) => match recv {
                        Ok((n, source)) => {
                            let pkt = InboundPacket {
                                proto: TransportProtocol::UDP,
                                source,
                                destination: local,
                                bytes: buf[..n].to_vec(),
                                received_at: Instant::now(),
                            };
                            if tx.send(pkt).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("[peer-file-transfer] UDP recv failed on {local}: {e}");
                            break;
                        }
                    }
                }
            }
        }));
    }

    let mut channels: HashMap<String, rtc::data_channel::RTCDataChannelId> = HashMap::new();
    let mut active_reads: HashMap<String, CancellationToken> = HashMap::new();
    let mut authority_tick = tokio::time::interval(LIVE_TRANSFER_AUTHORITY_RECHECK_INTERVAL);
    authority_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    'driver: loop {
        if !authorization.is_current() {
            shutdown.cancel();
            break;
        }
        let timeout_at = match drain_transfer_outputs(
            &mut rtc,
            &sockets_by_addr,
            &mut tcp_senders,
            &mut channels,
        )
        .await
        {
            Ok(timeout_at) => timeout_at,
            Err(()) => {
                shutdown.cancel();
                break;
            }
        };
        let timeout_dur = timeout_at
            .saturating_duration_since(Instant::now())
            .max(Duration::from_micros(1));

        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = authority_tick.tick() => {
                if !authorization.is_current() {
                    shutdown.cancel();
                    break;
                }
            }
            Some(pkt) = inbound_rx.recv() => {
                let input = TaggedBytesMut {
                    now: pkt.received_at,
                    transport: TransportContext {
                        local_addr: pkt.destination,
                        peer_addr: pkt.source,
                        transport_protocol: pkt.proto,
                        ecn: None,
                    },
                    message: BytesMut::from(pkt.bytes.as_slice()),
                };
                if let Err(e) = rtc.handle_read(input) {
                    eprintln!("[peer-file-transfer] handle_read failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
            Some(accepted) = async {
                match tcp_conn_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(fake_local) = tcp_advertised else {
                    eprintln!("[peer-file-transfer] TCP connection received without advertised local address");
                    continue;
                };
                let crate::display::webrtc::AcceptedTcpConnection {
                    remote_addr,
                    local_addr: real_local,
                    first_frame,
                    stream,
                } = accepted;
                eprintln!(
                    "[peer-file-transfer] ICE-TCP connection from {remote_addr} -> {real_local} (rtc sees {fake_local})"
                );
                let (read_half, write_half) = stream.into_split();

                let (tcp_out_tx, mut tcp_out_rx) = mpsc::channel::<Vec<u8>>(TCP_OUT_QUEUE);
                tcp_senders.insert(remote_addr, tcp_out_tx);
                let writer_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut write_half = write_half;
                    loop {
                        tokio::select! {
                            _ = writer_shutdown.cancelled() => break,
                            frame = tcp_out_rx.recv() => match frame {
                                Some(contents) => {
                                    if let Err(e) =
                                        crate::display::webrtc::write_rfc4571_frame(&mut write_half, &contents).await
                                    {
                                        eprintln!(
                                            "[peer-file-transfer] ICE-TCP writer for {remote_addr} failed: {e}"
                                        );
                                        writer_shutdown.cancel();
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                    let _ = write_half.shutdown().await;
                });

                let reader_tx = inbound_tx.clone();
                let reader_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let mut read_half = read_half;
                    loop {
                        tokio::select! {
                            _ = reader_shutdown.cancelled() => break,
                            frame = crate::display::webrtc::read_rfc4571_frame(&mut read_half) => match frame {
                                Ok(bytes) => {
                                    let pkt = InboundPacket {
                                        proto: TransportProtocol::TCP,
                                        source: remote_addr,
                                        destination: fake_local,
                                        bytes,
                                        received_at: Instant::now(),
                                    };
                                    if reader_tx.send(pkt).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[peer-file-transfer] ICE-TCP reader for {remote_addr} exiting: {e}");
                                    break;
                                }
                            }
                        }
                    }
                });

                let input = TaggedBytesMut {
                    now: Instant::now(),
                    transport: TransportContext {
                        local_addr: fake_local,
                        peer_addr: remote_addr,
                        transport_protocol: TransportProtocol::TCP,
                        ecn: None,
                    },
                    message: BytesMut::from(first_frame.as_slice()),
                };
                if let Err(e) = rtc.handle_read(input) {
                    eprintln!("[peer-file-transfer] handle_read(first TCP frame) failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
            Some(cmd) = command_rx.recv() => {
                if !authorization.is_current() {
                    shutdown.cancel();
                    break;
                }
                match cmd {
                    TransferCommand::AddIceCandidate(candidate) => {
                        let init = RTCIceCandidateInit {
                            candidate,
                            sdp_mid: None,
                            sdp_mline_index: None,
                            username_fragment: None,
                            url: None,
                        };
                        if let Err(e) = rtc.add_remote_candidate(init) {
                            eprintln!("[peer-file-transfer] parse remote candidate failed: {e}");
                        }
                    }
                    TransferCommand::SendText(text) => {
                        send_transfer_text(&mut rtc, &channels, text);
                    }
                    TransferCommand::SendBinary(bytes) => {
                        send_transfer_binary(&mut rtc, &channels, bytes);
                    }
                    TransferCommand::ReadFinished(id) => {
                        active_reads.remove(&id);
                    }
                }
                let _ = rtc.handle_timeout(Instant::now());
            }
            _ = tokio::time::sleep(timeout_dur) => {
                if let Err(e) = rtc.handle_timeout(Instant::now()) {
                    eprintln!("[peer-file-transfer] handle_timeout failed: {e:?}");
                    shutdown.cancel();
                    break;
                }
            }
        }

        while let Some(message) = rtc.poll_read() {
            let RTCMessage::DataChannelMessage(cid, msg) = message else {
                continue;
            };
            let label = channels
                .iter()
                .find_map(|(label, id)| (*id == cid).then(|| label.clone()));
            if label.as_deref() != Some(TRANSFER_CHANNEL_LABEL) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&msg.data) else {
                continue;
            };
            if !authorization.is_current() {
                shutdown.cancel();
                break 'driver;
            }
            handle_transfer_request(
                &session_id,
                text,
                &authorization,
                &bus,
                command_tx.clone(),
                &mut active_reads,
            );
        }
    }

    for (_, token) in active_reads {
        token.cancel();
    }
    for handle in forwarder_handles {
        let _ = handle.await;
    }
}

async fn drain_transfer_outputs<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    tcp_senders: &mut HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>,
    channels: &mut HashMap<String, rtc::data_channel::RTCDataChannelId>,
) -> Result<Instant, ()> {
    while let Some(t) = rtc.poll_write() {
        // Route by connection first, engine stamp second: rtc < 0.9.1
        // stamped DTLS/SCTP transmits `TransportProtocol::UDP` even on a
        // TCP pair, misrouting every post-ICE packet (webrtc-rs/rtc#109,
        // fixed by our upstream PR #110, released as 0.9.1 — which we
        // run). Tuple-first routing stays regardless: the tuple is the
        // engine's own connection key (rtc-shared `FiveTuple`), and it
        // keeps any future stamping regression from presenting as a
        // silent DTLS timeout again.
        if let Some(sender) = tcp_senders.get(&t.transport.peer_addr) {
            match sender.try_send(t.message.to_vec()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tcp_senders.remove(&t.transport.peer_addr);
                }
            }
            continue;
        }
        if t.transport.transport_protocol == TransportProtocol::TCP {
            // TCP-stamped transmit with no live stream for the tuple: the
            // connection is gone and there is nothing to write to.
            continue;
        }
        if t.transport.local_addr.is_ipv4() != t.transport.peer_addr.is_ipv4() {
            continue;
        }
        if t.transport.local_addr.ip().is_loopback() != t.transport.peer_addr.ip().is_loopback() {
            continue;
        }
        let Some(sock) = sockets_by_addr.get(&t.transport.local_addr) else {
            continue;
        };
        if let Err(e) = sock.send_to(&t.message, t.transport.peer_addr).await {
            eprintln!(
                "[peer-file-transfer] udp send {} -> {} failed: {e}",
                t.transport.local_addr, t.transport.peer_addr
            );
        }
    }

    while let Some(event) = rtc.poll_event() {
        match event {
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(cid)) => {
                let label = rtc
                    .data_channel(cid)
                    .map(|channel| channel.label().to_string())
                    .unwrap_or_else(|| format!("channel-{cid}"));
                channels.insert(label, cid);
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(cid)) => {
                channels.retain(|_, id| *id != cid);
            }
            RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                if matches!(
                    state,
                    rtc::peer_connection::state::RTCPeerConnectionState::Failed
                        | rtc::peer_connection::state::RTCPeerConnectionState::Closed
                ) {
                    return Err(());
                }
            }
            _ => {}
        }
    }

    Ok(rtc
        .poll_timeout()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400)))
}

fn handle_transfer_request(
    session_id: &str,
    text: &str,
    authorization: &PeerFileTransferAuthorization,
    bus: &crate::event::EventBus,
    command_tx: mpsc::Sender<TransferCommand>,
    active_reads: &mut HashMap<String, CancellationToken>,
) {
    let request = match serde_json::from_str::<TransferRequest>(text) {
        Ok(request) => request,
        Err(e) => {
            let _ = command_tx.try_send(TransferCommand::SendText(
                serde_json::json!({"t": "error", "id": null, "error": format!("invalid request: {e}")})
                    .to_string(),
            ));
            return;
        }
    };

    match request {
        TransferRequest::Read {
            id,
            path,
            offset,
            length,
        } => {
            if let Some(old) = active_reads.remove(&id) {
                old.cancel();
            }
            let cancel = CancellationToken::new();
            active_reads.insert(id.clone(), cancel.clone());
            let authorization = authorization.clone();
            let bus = bus.clone();
            let session_id = session_id.to_string();
            tokio::spawn(async move {
                stream_read_request(
                    session_id,
                    id,
                    path,
                    offset,
                    length,
                    authorization,
                    command_tx,
                    cancel,
                    bus,
                )
                .await;
            });
        }
        TransferRequest::Cancel { id } => {
            if let Some(token) = active_reads.remove(&id) {
                token.cancel();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
async fn stream_read_request(
    session_id: String,
    id: String,
    raw_path: String,
    offset: u64,
    length: Option<u64>,
    authorization: PeerFileTransferAuthorization,
    command_tx: mpsc::Sender<TransferCommand>,
    cancel: CancellationToken,
    bus: crate::event::EventBus,
) {
    let result = async {
        let (canonical, file) = open_authorized_read_file(&authorization, &raw_path)?;
        let metadata = file
            .metadata()
            .map_err(|e| format!("stat {}: {e}", canonical.display()))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a file", canonical.display()));
        }
        let total_size = metadata.len();
        if offset > total_size {
            return Err(format!("offset {offset} exceeds file size {total_size}"));
        }
        let available = total_size.saturating_sub(offset);
        let read_len = length
            .unwrap_or(available)
            .min(available)
            .min(MAX_READ_BYTES);
        let filename = canonical
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download")
            .to_string();
        let content_type = crate::web_gateway::dashboard_fs_content_type(&canonical);
        if !authorization.is_current() {
            return Err("peer file-transfer identity changed before response".to_string());
        }
        command_tx
            .send(TransferCommand::SendText(
                serde_json::json!({
                    "t": "start",
                    "id": id,
                    "path": canonical.to_string_lossy(),
                    "filename": filename,
                    "content_type": content_type,
                    "offset": offset,
                    "length": read_len,
                    "total_size": total_size,
                })
                .to_string(),
            ))
            .await
            .map_err(|_| "transfer driver gone".to_string())?;

        stream_file_range(
            file,
            &canonical,
            offset,
            read_len,
            &authorization,
            &command_tx,
            &cancel,
        )
        .await?;
        if !authorization.is_current() {
            return Err("peer file-transfer identity changed before completion".to_string());
        }
        command_tx
            .send(TransferCommand::SendText(
                serde_json::json!({
                    "t": "end",
                    "id": id,
                    "bytes": read_len,
                    "offset": offset,
                    "total_size": total_size,
                })
                .to_string(),
            ))
            .await
            .map_err(|_| "transfer driver gone".to_string())?;
        Ok::<(), String>(())
    }
    .await;

    match result {
        Ok(()) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "info".into(),
                source: "peer-file-transfer".into(),
                content: format!(
                    "completed read session={} peer={} fingerprint={} path={} offset={} length={:?}",
                    session_id, authorization.label, authorization.fingerprint, raw_path, offset, length
                ),
                turn: None,
            });
        }
        Err(error) => {
            if authorization.is_current() {
                let _ = command_tx
                    .send(TransferCommand::SendText(
                        serde_json::json!({"t": "error", "id": id, "error": error}).to_string(),
                    ))
                    .await;
            }
        }
    }
    let _ = command_tx.send(TransferCommand::ReadFinished(id)).await;
}

fn authorize_path(
    authorization: &PeerFileTransferAuthorization,
    raw_path: &str,
) -> Result<PathBuf, String> {
    if !authorization.is_current() {
        return Err("peer file-transfer identity changed or is no longer active".to_string());
    }
    crate::access::iam::evaluate_principal_operation(
        &authorization.access_principal(),
        PeerOperation::FilesystemRead,
    )
    .ensure_allowed()?;
    let path = crate::web_gateway::expand_dashboard_fs_path(raw_path)?;
    filesystem_access_canonical_subject(
        &authorization.filesystem,
        FilesystemAccessKind::Read,
        &path,
    )
}

/// Authorize one canonical path, open it once, then prove the opened handle
/// still names that path. Streaming owns this handle and never reopens the
/// caller-controlled path, narrowing symlink/path replacement races to the
/// platform's path-open operation itself.
fn open_authorized_read_file(
    authorization: &PeerFileTransferAuthorization,
    raw_path: &str,
) -> Result<(PathBuf, std::fs::File), String> {
    let canonical = authorize_path(authorization, raw_path)?;
    let file = std::fs::File::open(&canonical)
        .map_err(|e| format!("open {}: {e}", canonical.display()))?;

    // Detect a parent-component or final-component replacement that raced
    // authorization/open. The open handle is the object we will stream;
    // both the path's fresh canonical form and stable file identity must
    // still agree with it before any metadata or bytes leave the daemon.
    let current_canonical = std::fs::canonicalize(&canonical)
        .map_err(|e| format!("re-resolve {} after open: {e}", canonical.display()))?;
    if current_canonical != canonical {
        return Err(format!(
            "{} changed while the peer file read was opening",
            canonical.display()
        ));
    }
    let opened_identity = crate::platform::FileIdentity::from_file(&file)
        .map_err(|e| format!("identify opened {}: {e}", canonical.display()))?;
    let path_identity = crate::platform::FileIdentity::from_path(&canonical)
        .map_err(|e| format!("identify path {} after open: {e}", canonical.display()))?;
    if !opened_identity.is_reliable()
        || !path_identity.is_reliable()
        || opened_identity != path_identity
    {
        return Err(format!(
            "{} changed while the peer file read was opening",
            canonical.display()
        ));
    }
    if !authorization.is_current() {
        return Err("peer file-transfer identity changed while opening the file".to_string());
    }
    Ok((canonical, file))
}

async fn stream_file_range(
    mut file: std::fs::File,
    path: &Path,
    offset: u64,
    length: u64,
    authorization: &PeerFileTransferAuthorization,
    command_tx: &mpsc::Sender<TransferCommand>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    let mut file = tokio::fs::File::from_std(file);
    let mut remaining = length;
    let mut buf = vec![0u8; CHUNK_BYTES];
    while remaining > 0 {
        if cancel.is_cancelled() {
            return Err("transfer cancelled".to_string());
        }
        if !authorization.is_current() {
            return Err("peer file-transfer identity changed during read".to_string());
        }
        let want = (remaining as usize).min(buf.len());
        let n = file
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        if !authorization.is_current() {
            return Err("peer file-transfer identity changed during read".to_string());
        }
        remaining = remaining.saturating_sub(n as u64);
        command_tx
            .send(TransferCommand::SendBinary(buf[..n].to_vec()))
            .await
            .map_err(|_| "transfer driver gone".to_string())?;
    }
    Ok(())
}

fn send_transfer_text<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    text: String,
) {
    let Some(cid) = channels.get(TRANSFER_CHANNEL_LABEL).copied() else {
        return;
    };
    if let Some(mut channel) = rtc.data_channel(cid) {
        if let Err(e) = channel.send_text(text) {
            eprintln!("[peer-file-transfer] data channel text write failed: {e:?}");
        }
    }
}

fn send_transfer_binary<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    channels: &HashMap<String, rtc::data_channel::RTCDataChannelId>,
    bytes: Vec<u8>,
) {
    let Some(cid) = channels.get(TRANSFER_CHANNEL_LABEL).copied() else {
        return;
    };
    if let Some(mut channel) = rtc.data_channel(cid) {
        if let Err(e) = channel.send(BytesMut::from(&bytes[..])) {
            eprintln!("[peer-file-transfer] data channel binary write failed: {e:?}");
        }
    }
}

fn to_rtc_ice_servers(servers: &[crate::display::IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .map(|server| RTCIceServer {
            urls: server.urls.clone(),
            username: server.username.clone().unwrap_or_default(),
            credential: server.credential.clone().unwrap_or_default(),
        })
        .collect()
}

fn udp_host_candidate_init(addr: SocketAddr) -> Result<RTCIceCandidateInit, CallerError> {
    let candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: addr.ip().to_string(),
            port: addr.port(),
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
    .map_err(|e| CallerError::WebRtc(format!("build UDP host candidate: {e}")))?;
    RTCIceCandidate::from(&candidate)
        .to_json()
        .map_err(|e| CallerError::WebRtc(format!("serialize UDP host candidate: {e}")))
}

fn tcp_host_candidate_init(addr: SocketAddr) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:9001 1 tcp 1677721855 {} {} typ host tcptype passive generation 0",
            addr.ip(),
            addr.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_session_owner_match_is_exact() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let opening_authorization = PeerFileTransferAuthorization {
            fingerprint: "peer-a".to_string(),
            label: "Peer A".to_string(),
            profile: "file-reader".to_string(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
        };
        let peer = PeerFileTransferPeer {
            command_tx,
            shutdown: CancellationToken::new(),
            owner_fingerprint: "peer-a".to_string(),
            opening_authorization,
        };
        assert!(peer.belongs_to("peer-a"));
        assert!(!peer.belongs_to("peer-b"));
    }

    #[test]
    fn live_peer_identity_change_invalidates_file_transfer_authority() {
        assert!(LIVE_TRANSFER_AUTHORITY_RECHECK_INTERVAL <= Duration::from_millis(500));
        let tmp = tempfile::TempDir::new().unwrap();
        let fingerprint = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let record = crate::peer::access_policy::write_approved_identity(
            tmp.path(),
            fingerprint,
            "peer-b",
            "file-reader",
            None,
            None,
        )
        .unwrap();
        let authorization = PeerFileTransferAuthorization {
            fingerprint: record.fingerprint.clone(),
            label: record.label.clone(),
            profile: record.profile.clone(),
            filesystem: record.filesystem.clone(),
            identity_record: Some(record),
            iam_cert_dir: Some(tmp.path().to_path_buf()),
        };
        assert!(authorization.is_current());
        crate::peer::access_policy::revoke_identity(tmp.path(), fingerprint).unwrap();
        assert!(!authorization.is_current());
    }

    #[test]
    fn pending_transfer_reservations_are_owner_bound_bounded_and_expiring() {
        let now = Instant::now();
        let mut reservations = HashMap::new();
        reserve_pending_transfer_session(&mut reservations, "chosen", "peer-a", now).unwrap();
        assert!(
            reserve_pending_transfer_session(&mut reservations, "chosen", "peer-a", now)
                .unwrap_err()
                .contains("already being negotiated")
        );
        assert!(
            reserve_pending_transfer_session(&mut reservations, "chosen", "peer-b", now)
                .unwrap_err()
                .contains("another authenticated peer")
        );

        for index in 1..MAX_PENDING_TRANSFER_RESERVATIONS_PER_OWNER {
            reserve_pending_transfer_session(
                &mut reservations,
                &format!("peer-a-{index}"),
                "peer-a",
                now,
            )
            .unwrap();
        }
        assert!(reserve_pending_transfer_session(
            &mut reservations,
            "peer-a-over-limit",
            "peer-a",
            now,
        )
        .unwrap_err()
        .contains("too many pending"));

        let expired_at = now
            .checked_sub(PENDING_TRANSFER_RESERVATION_TTL + Duration::from_secs(1))
            .unwrap();
        reservations.insert(
            "expired".to_string(),
            PendingTransferReservation {
                owner_fingerprint: "peer-expired".to_string(),
                candidates: vec!["candidate".to_string()],
                created_at: expired_at,
            },
        );
        prune_expired_transfer_reservations(&mut reservations, now);
        assert!(!reservations.contains_key("expired"));

        let mut global = HashMap::new();
        for index in 0..MAX_PENDING_TRANSFER_RESERVATIONS {
            reserve_pending_transfer_session(
                &mut global,
                &format!("session-{index}"),
                &format!("peer-{index}"),
                now,
            )
            .unwrap();
        }
        assert!(reserve_pending_transfer_session(
            &mut global,
            "global-over-limit",
            "peer-over-limit",
            now,
        )
        .unwrap_err()
        .contains("too many pending"));
    }

    #[test]
    fn transfer_read_request_parses_range() {
        let req: TransferRequest =
            serde_json::from_str(r#"{"t":"read","id":"r1","path":"/tmp/a","offset":4,"length":8}"#)
                .unwrap();
        match req {
            TransferRequest::Read {
                id,
                path,
                offset,
                length,
            } => {
                assert_eq!(id, "r1");
                assert_eq!(path, "/tmp/a");
                assert_eq!(offset, 4);
                assert_eq!(length, Some(8));
            }
            _ => panic!("expected read"),
        }
    }

    #[test]
    fn authorize_path_requires_file_profile() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"ok").unwrap();
        let auth = PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "operator".into(),
            filesystem: FilesystemAccessPolicy {
                read_roots: vec![tmp.path().to_path_buf()],
                write_roots: Vec::new(),
            },
            identity_record: None,
            iam_cert_dir: None,
        };
        let err = authorize_path(&auth, file.to_str().unwrap()).unwrap_err();
        assert!(err.contains("does not allow filesystem.read"));
    }

    #[test]
    fn authorize_path_accepts_file_reader_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"ok").unwrap();
        let auth = PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "file-reader".into(),
            filesystem: FilesystemAccessPolicy {
                read_roots: vec![tmp.path().to_path_buf()],
                write_roots: Vec::new(),
            },
            identity_record: None,
            iam_cert_dir: None,
        };
        assert_eq!(
            authorize_path(&auth, file.to_str().unwrap()).unwrap(),
            std::fs::canonicalize(file).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn authorized_transfer_streams_the_opened_file_not_a_replaced_path() {
        use std::io::Read as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        let moved = tmp.path().join("opened.txt");
        std::fs::write(&file, b"authorized object").unwrap();
        let auth = PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "file-reader".into(),
            filesystem: FilesystemAccessPolicy {
                read_roots: vec![tmp.path().to_path_buf()],
                write_roots: Vec::new(),
            },
            identity_record: None,
            iam_cert_dir: None,
        };

        let (_canonical, mut opened) =
            open_authorized_read_file(&auth, file.to_str().unwrap()).unwrap();
        std::fs::rename(&file, &moved).unwrap();
        std::fs::write(&file, b"replacement object").unwrap();

        let mut contents = String::new();
        opened.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "authorized object");
    }
}
