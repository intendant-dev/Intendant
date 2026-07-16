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
/// How long a positive [`PeerFileTransferAuthorization::is_current`] verdict
/// may be reused by the hot paths (driver loop, per-datachannel message,
/// per-chunk stream checks) before the identity store is consulted again.
/// Each fresh consult costs a fingerprint normalize, a `fs::metadata` stat,
/// and a process-global cache lock — at chunk rate that was thousands of
/// syscalls per second per transfer. The driver's authority tick always
/// re-verifies fresh, so the revocation bound stays
/// `LIVE_TRANSFER_AUTHORITY_RECHECK_INTERVAL` for the driver and
/// `LIVE_TRANSFER_AUTHORITY_MEMO_TTL` for in-flight read streams; a test
/// pins their sum under the 500 ms revocation budget.
const LIVE_TRANSFER_AUTHORITY_MEMO_TTL: Duration = Duration::from_millis(100);
const TCP_OUT_QUEUE: usize = 256;
/// Pause pulling new transfer commands (file chunks included) once the SCTP
/// stream buffers this much; resume when it drains to the low watermark.
/// Without this gate the reader pumps the unbounded SCTP pending queue at
/// disk speed — a 512 MB read to a slow WAN peer parked hundreds of MB of
/// RSS. 4 MiB comfortably covers a 100 Mbit/s × 300 ms RTT pipe.
const TRANSFER_BUFFERED_HIGH_WATERMARK_BYTES: usize = 4 * 1024 * 1024;
/// Low watermark paired with [`TRANSFER_BUFFERED_HIGH_WATERMARK_BYTES`].
const TRANSFER_BUFFERED_LOW_WATERMARK_BYTES: usize = 1024 * 1024;
/// In-flight read streams per transfer session. Each distinct read spawns a
/// task and holds an open file descriptor; without a cap an authorized peer
/// could hold hundreds of parallel streams, multiplying the send-queue
/// memory the watermarks above bound per stream admission. The dashboard
/// client reads ranges sequentially, so a small cap is generous.
const MAX_CONCURRENT_READS_PER_SESSION: usize = 4;

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
                    crate::peer::access_policy::lookup_identity_cached_arc(
                        cert_dir,
                        &self.fingerprint,
                    ),
                    Ok(Some(current)) if current.as_ref() == opening && current.is_active(now_unix)
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

/// A transfer session's authorization plus a shared positive-verdict memo.
///
/// [`PeerFileTransferAuthorization::is_current`] stats the identity store
/// and takes a process-global cache lock on every call; the transfer paths
/// call it per driver wakeup, per datachannel message, and twice per 16 KiB
/// chunk — thousands of times per second during a bulk read, all contending
/// the same mutex across every live transfer. This wrapper memoizes only
/// **positive** verdicts for [`LIVE_TRANSFER_AUTHORITY_MEMO_TTL`]; clones
/// (the driver plus each spawned read stream) share the memo cell.
///
/// Trust invariants (do not weaken):
/// - The fresh check and the memo update are ATOMIC: the memo lock is
///   held across the identity-store consult, so a preempted positive
///   check can never stamp the memo after a newer negative observed the
///   revocation.
/// - A fresh negative CLEARS the memo — no stale positive survives a
///   fresh failure, and negative verdicts are never memoized.
/// - The request-authorization path ([`authorize_path`],
///   [`open_authorized_read_file`]) and the driver's authority tick call
///   [`Self::is_current_fresh`], so authorization decisions and the
///   periodic revocation probe never ride the memo.
/// - `revocation bound = tick interval` for the driver (fresh at tick) and
///   `≤ memo TTL` extra staleness for in-flight chunk streams; a test pins
///   `tick + TTL ≤ 500 ms`.
#[derive(Clone)]
struct LiveTransferAuthority {
    authorization: PeerFileTransferAuthorization,
    /// Instant of the last fresh **positive** verification, shared across
    /// clones. `None` until first verified and after any fresh negative.
    /// Fresh checks run UNDER this lock (see the atomicity invariant in
    /// the type docs); they are low-frequency (250 ms tick + per-request
    /// authorization), so serializing them here costs nothing.
    verified_at: Arc<std::sync::Mutex<Option<Instant>>>,
}

impl LiveTransferAuthority {
    fn new(authorization: PeerFileTransferAuthorization) -> Self {
        Self {
            authorization,
            verified_at: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Memoized liveness: reuse a positive verdict younger than
    /// [`LIVE_TRANSFER_AUTHORITY_MEMO_TTL`], else verify fresh (under the
    /// memo lock, like [`Self::is_current_fresh`]).
    fn is_current(&self) -> bool {
        let mut verified_at = self.verified_at.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = *verified_at {
            if at.elapsed() < LIVE_TRANSFER_AUTHORITY_MEMO_TTL {
                return true;
            }
        }
        Self::refresh_locked(&self.authorization, &mut verified_at)
    }

    /// Uncached liveness against the identity store. Check and stamp are
    /// one critical section: a positive primes the memo, a negative
    /// clears it — a preempted stale positive can never re-prime the memo
    /// after a newer negative.
    fn is_current_fresh(&self) -> bool {
        let mut verified_at = self.verified_at.lock().unwrap_or_else(|e| e.into_inner());
        Self::refresh_locked(&self.authorization, &mut verified_at)
    }

    /// The one fresh-check-and-stamp critical section (callers hold the
    /// memo lock). Lock order is memo → identity-cache global lock; no
    /// reverse path exists.
    fn refresh_locked(
        authorization: &PeerFileTransferAuthorization,
        verified_at: &mut Option<Instant>,
    ) -> bool {
        let ok = authorization.is_current();
        *verified_at = if ok { Some(Instant::now()) } else { None };
        ok
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
    opening_authorization: LiveTransferAuthority,
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
        // One memo shared by the signaling handle, the driver, and every
        // read stream the driver spawns.
        let authorization = LiveTransferAuthority::new(authorization);
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
    SendBinary(BytesMut),
    /// Natural completion of the read stream spawned as `generation` —
    /// the driver deregisters the entry only when the generation still
    /// matches, so a finished superseded task can never evict its
    /// replacement (which would leave an uncounted, uncancellable
    /// stream defeating [`MAX_CONCURRENT_READS_PER_SESSION`]).
    ReadFinished {
        id: String,
        generation: u64,
    },
}

/// One tracked read stream: its cancel token plus the spawn generation
/// that keys [`TransferCommand::ReadFinished`] deregistration.
struct ActiveRead {
    cancel: CancellationToken,
    generation: u64,
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
    authorization: LiveTransferAuthority,
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
    let mut tcp_senders: HashMap<SocketAddr, mpsc::Sender<BytesMut>> = HashMap::new();
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
    let mut active_reads: HashMap<String, ActiveRead> = HashMap::new();
    let mut next_read_generation: u64 = 0;
    // True while the transfer channel's SCTP send buffer sits above the
    // high watermark: the command lane stops admitting work (chunks, text,
    // trickled candidates alike — FIFO order must hold anyway), the bounded
    // command channel fills, and the disk readers' `send().await` parks.
    // Cleared by the OnBufferedAmountLow event in `drain_transfer_outputs`.
    let mut send_paused = false;
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
            &mut send_paused,
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
                // The periodic revocation probe never rides the memo.
                if !authorization.is_current_fresh() {
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

                let (tcp_out_tx, mut tcp_out_rx) = mpsc::channel::<BytesMut>(TCP_OUT_QUEUE);
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
            // Gated on the SCTP send-buffer watermark: while paused, queued
            // commands wait in the bounded channel and the readers park.
            // AddIceCandidate rides the same lane, so trickled candidates
            // queue behind the pause too — incidental coupling, harmless
            // today (candidates matter during setup, before bulk data can
            // congest the channel; an established pair needs no new ones).
            Some(cmd) = command_rx.recv(), if !send_paused => {
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
                    TransferCommand::ReadFinished { id, generation } => {
                        deregister_finished_read(&mut active_reads, &id, generation);
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
            if let Some(reply) = handle_transfer_request(
                &session_id,
                text,
                &authorization,
                &bus,
                command_tx.clone(),
                &mut active_reads,
                &mut next_read_generation,
            ) {
                // Rejections bypass the (possibly watermark-paused) command
                // lane: the driver owns the rtc right here, and a dropped
                // rejection would leave the client waiting forever.
                send_transfer_text(&mut rtc, &channels, reply);
            }
        }
    }

    for (_, read) in active_reads {
        read.cancel.cancel();
    }
    for handle in forwarder_handles {
        let _ = handle.await;
    }
}

async fn drain_transfer_outputs<I: rtc::interceptor::Interceptor>(
    rtc: &mut RTCPeerConnection<I>,
    sockets_by_addr: &HashMap<SocketAddr, Arc<UdpSocket>>,
    tcp_senders: &mut HashMap<SocketAddr, mpsc::Sender<BytesMut>>,
    channels: &mut HashMap<String, rtc::data_channel::RTCDataChannelId>,
    send_paused: &mut bool,
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
            // Move the transmit into the writer lane without copying
            // (~1.2 KB per packet — ~450k allocations per 512 MB read
            // before this). A full lane drops the packet exactly as the
            // to_vec path did; SCTP retransmission recovers it.
            match sender.try_send(t.message) {
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
                if label == TRANSFER_CHANNEL_LABEL {
                    // Arm the SCTP buffered-amount watermarks (same
                    // event-driven pattern as the display pipeline's
                    // tile-delta backpressure): the High event pauses the
                    // driver's command lane, Low resumes it.
                    if let Some(mut channel) = rtc.data_channel(cid) {
                        channel.set_buffered_amount_high_threshold(
                            crate::dashboard_control::watermark_to_u32(
                                TRANSFER_BUFFERED_HIGH_WATERMARK_BYTES,
                            ),
                        );
                        channel.set_buffered_amount_low_threshold(
                            crate::dashboard_control::watermark_to_u32(
                                TRANSFER_BUFFERED_LOW_WATERMARK_BYTES,
                            ),
                        );
                    }
                    *send_paused = false;
                }
                channels.insert(label, cid);
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(cid)) => {
                if channels.get(TRANSFER_CHANNEL_LABEL).copied() == Some(cid) {
                    // Never leave the command lane parked behind a channel
                    // that can no longer drain.
                    *send_paused = false;
                }
                channels.retain(|_, id| *id != cid);
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountHigh(
                cid,
            )) => {
                if channels.get(TRANSFER_CHANNEL_LABEL).copied() == Some(cid) {
                    *send_paused = true;
                }
            }
            RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnBufferedAmountLow(
                cid,
            )) => {
                if channels.get(TRANSFER_CHANNEL_LABEL).copied() == Some(cid) {
                    *send_paused = false;
                }
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

/// Handle one inbound transfer request. Rejections come back as
/// `Some(<error frame text>)` for the DRIVER to send directly over the
/// datachannel — the command lane may be watermark-paused or full, and a
/// `try_send` there dropped rejections exactly when the wire was busy.
#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
fn handle_transfer_request(
    session_id: &str,
    text: &str,
    authorization: &LiveTransferAuthority,
    bus: &crate::event::EventBus,
    command_tx: mpsc::Sender<TransferCommand>,
    active_reads: &mut HashMap<String, ActiveRead>,
    next_read_generation: &mut u64,
) -> Option<String> {
    let request = match serde_json::from_str::<TransferRequest>(text) {
        Ok(request) => request,
        Err(e) => {
            return Some(
                serde_json::json!({"t": "error", "id": null, "error": format!("invalid request: {e}")})
                    .to_string(),
            );
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
                old.cancel.cancel();
            }
            if active_reads.len() >= MAX_CONCURRENT_READS_PER_SESSION {
                return Some(
                    serde_json::json!({
                        "t": "error",
                        "id": id,
                        "error": format!(
                            "too many concurrent reads on this transfer session (max {MAX_CONCURRENT_READS_PER_SESSION})"
                        ),
                    })
                    .to_string(),
                );
            }
            *next_read_generation = next_read_generation.wrapping_add(1);
            let generation = *next_read_generation;
            let cancel = CancellationToken::new();
            active_reads.insert(
                id.clone(),
                ActiveRead {
                    cancel: cancel.clone(),
                    generation,
                },
            );
            let authorization = authorization.clone();
            let bus = bus.clone();
            let session_id = session_id.to_string();
            tokio::spawn(async move {
                stream_read_request(
                    session_id,
                    id,
                    generation,
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
            None
        }
        TransferRequest::Cancel { id } => {
            if let Some(read) = active_reads.remove(&id) {
                read.cancel.cancel();
            }
            None
        }
    }
}

/// The driver's `ReadFinished` handling (extracted for tests): deregister
/// only when the finishing generation still owns the entry, so a
/// superseded task's natural completion cannot evict its replacement.
fn deregister_finished_read(
    active_reads: &mut HashMap<String, ActiveRead>,
    id: &str,
    generation: u64,
) {
    if active_reads
        .get(id)
        .is_some_and(|read| read.generation == generation)
    {
        active_reads.remove(id);
    }
}

/// Send a transfer command, aborting when the read is cancelled — a
/// watermark-paused command lane must never pin a cancelled read task
/// inside an un-cancellable `send().await`.
async fn send_command_or_cancelled(
    command_tx: &mpsc::Sender<TransferCommand>,
    cancel: &CancellationToken,
    command: TransferCommand,
) -> Result<(), String> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("transfer cancelled".to_string()),
        sent = command_tx.send(command) => sent.map_err(|_| "transfer driver gone".to_string()),
    }
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
async fn stream_read_request(
    session_id: String,
    id: String,
    generation: u64,
    raw_path: String,
    offset: u64,
    length: Option<u64>,
    authorization: LiveTransferAuthority,
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
        send_command_or_cancelled(
            &command_tx,
            &cancel,
            TransferCommand::SendText(
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
            ),
        )
        .await?;

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
        send_command_or_cancelled(
            &command_tx,
            &cancel,
            TransferCommand::SendText(
                serde_json::json!({
                    "t": "end",
                    "id": id,
                    "bytes": read_len,
                    "offset": offset,
                    "total_size": total_size,
                })
                .to_string(),
            ),
        )
        .await?;
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
                    session_id,
                    authorization.authorization.label,
                    authorization.authorization.fingerprint,
                    raw_path,
                    offset,
                    length
                ),
                turn: None,
            });
        }
        Err(error) => {
            if !cancel.is_cancelled() && authorization.is_current() {
                let _ = send_command_or_cancelled(
                    &command_tx,
                    &cancel,
                    TransferCommand::SendText(
                        serde_json::json!({"t": "error", "id": id, "error": error}).to_string(),
                    ),
                )
                .await;
            }
        }
    }
    // Deregister on NATURAL completion only: whoever cancels a read
    // (replacement, Cancel request, driver teardown) removes the registry
    // entry itself, and this task must never sit in an un-cancellable
    // send. The generation keys the removal so a superseded task cannot
    // evict its replacement.
    if !cancel.is_cancelled() {
        let _ = send_command_or_cancelled(
            &command_tx,
            &cancel,
            TransferCommand::ReadFinished { id, generation },
        )
        .await;
    }
}

fn authorize_path(
    authorization: &LiveTransferAuthority,
    raw_path: &str,
) -> Result<PathBuf, String> {
    // Authorization decisions never ride the memo: verify fresh.
    if !authorization.is_current_fresh() {
        return Err("peer file-transfer identity changed or is no longer active".to_string());
    }
    crate::access::iam::evaluate_principal_operation(
        &authorization.authorization.access_principal(),
        PeerOperation::FilesystemRead,
    )
    .ensure_allowed()?;
    let path = crate::web_gateway::expand_dashboard_fs_path(raw_path)?;
    filesystem_access_canonical_subject(
        &authorization.authorization.filesystem,
        FilesystemAccessKind::Read,
        &path,
    )
}

/// Authorize one canonical path, open it once, then prove the opened handle
/// still names that path. Streaming owns this handle and never reopens the
/// caller-controlled path, narrowing symlink/path replacement races to the
/// platform's path-open operation itself.
fn open_authorized_read_file(
    authorization: &LiveTransferAuthority,
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
    if !authorization.is_current_fresh() {
        return Err("peer file-transfer identity changed while opening the file".to_string());
    }
    Ok((canonical, file))
}

async fn stream_file_range(
    mut file: std::fs::File,
    path: &Path,
    offset: u64,
    length: u64,
    authorization: &LiveTransferAuthority,
    command_tx: &mpsc::Sender<TransferCommand>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    use bytes::BufMut as _;

    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    let mut file = tokio::fs::File::from_std(file);
    let mut remaining = length;
    // One BytesMut reused across chunks: `read_buf` fills fresh capacity
    // and `split_to` hands the filled prefix to the driver without a copy
    // (the old path copied each chunk into a new Vec, and the driver
    // copied it again into a BytesMut for the datachannel).
    let mut buf = BytesMut::with_capacity(CHUNK_BYTES);
    while remaining > 0 {
        if cancel.is_cancelled() {
            return Err("transfer cancelled".to_string());
        }
        if !authorization.is_current() {
            return Err("peer file-transfer identity changed during read".to_string());
        }
        let want = (remaining as usize).min(CHUNK_BYTES);
        buf.reserve(want);
        let n = file
            .read_buf(&mut (&mut buf).limit(want))
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        if !authorization.is_current() {
            return Err("peer file-transfer identity changed during read".to_string());
        }
        remaining = remaining.saturating_sub(n as u64);
        send_command_or_cancelled(
            command_tx,
            cancel,
            TransferCommand::SendBinary(buf.split_to(n)),
        )
        .await?;
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
    bytes: BytesMut,
) {
    let Some(cid) = channels.get(TRANSFER_CHANNEL_LABEL).copied() else {
        return;
    };
    if let Some(mut channel) = rtc.data_channel(cid) {
        if let Err(e) = channel.send(bytes) {
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

    fn approved_authorization(
        tmp: &tempfile::TempDir,
        fingerprint: &str,
    ) -> PeerFileTransferAuthorization {
        let record = crate::peer::access_policy::write_approved_identity(
            tmp.path(),
            fingerprint,
            "peer-b",
            "file-reader",
            None,
            None,
        )
        .unwrap();
        PeerFileTransferAuthorization {
            fingerprint: record.fingerprint.clone(),
            label: record.label.clone(),
            profile: record.profile.clone(),
            filesystem: record.filesystem.clone(),
            identity_record: Some(record),
            iam_cert_dir: Some(tmp.path().to_path_buf()),
        }
    }

    #[test]
    fn transfer_session_owner_match_is_exact() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let opening_authorization = LiveTransferAuthority::new(PeerFileTransferAuthorization {
            fingerprint: "peer-a".to_string(),
            label: "Peer A".to_string(),
            profile: "file-reader".to_string(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
        });
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
        // The end-to-end revocation budget: a fresh driver tick plus the
        // worst-case memo staleness of an in-flight read stream.
        assert!(
            LIVE_TRANSFER_AUTHORITY_RECHECK_INTERVAL + LIVE_TRANSFER_AUTHORITY_MEMO_TTL
                <= Duration::from_millis(500)
        );
        let tmp = tempfile::TempDir::new().unwrap();
        let fingerprint = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let authorization = approved_authorization(&tmp, fingerprint);
        assert!(authorization.is_current());
        crate::peer::access_policy::revoke_identity(tmp.path(), fingerprint).unwrap();
        assert!(!authorization.is_current());
    }

    /// Trust pin for the memoized wrapper: `is_current_fresh` (the driver
    /// tick and the request-authorization path) consults the identity
    /// store on every call — a primed memo never masks a revocation from
    /// the fresh path.
    #[test]
    fn transfer_authority_fresh_check_ignores_memo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fingerprint = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let authority = LiveTransferAuthority::new(approved_authorization(&tmp, fingerprint));
        assert!(authority.is_current());
        assert!(authority.verified_at.lock().unwrap().is_some());
        crate::peer::access_policy::revoke_identity(tmp.path(), fingerprint).unwrap();
        assert!(!authority.is_current_fresh());
        // A fresh negative clears the memo, so the memoized path observes
        // the revocation immediately — no TTL ride-out on a verdict the
        // fresh path already invalidated.
        assert!(
            authority.verified_at.lock().unwrap().is_none(),
            "a fresh negative must clear the memo"
        );
        assert!(!authority.is_current());
    }

    /// Read-id reuse must never orphan the replacement stream: the old
    /// generation's completion may not deregister the new generation's
    /// entry (which would leave an uncounted, uncancellable stream
    /// defeating the concurrency cap), and replacement cancels the old
    /// token.
    #[tokio::test]
    async fn read_id_reuse_keeps_the_replacement_tracked() {
        let (command_tx, _command_rx) = mpsc::channel(COMMAND_CHANNEL);
        let authority = LiveTransferAuthority::new(PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "file-reader".into(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
        });
        let bus = crate::event::EventBus::new();
        let mut active_reads: HashMap<String, ActiveRead> = HashMap::new();
        let mut next_generation = 0u64;
        let read_request =
            r#"{"t":"read","id":"r1","path":"/nonexistent/fixture","offset":0}"#.to_string();

        assert!(handle_transfer_request(
            "session",
            &read_request,
            &authority,
            &bus,
            command_tx.clone(),
            &mut active_reads,
            &mut next_generation,
        )
        .is_none());
        let first_generation = active_reads["r1"].generation;
        let first_token = active_reads["r1"].cancel.clone();

        // Reuse the id: the old stream is cancelled, a new generation is
        // registered.
        assert!(handle_transfer_request(
            "session",
            &read_request,
            &authority,
            &bus,
            command_tx.clone(),
            &mut active_reads,
            &mut next_generation,
        )
        .is_none());
        assert!(first_token.is_cancelled());
        let second_generation = active_reads["r1"].generation;
        assert_ne!(first_generation, second_generation);

        // The superseded generation's completion is a no-op…
        deregister_finished_read(&mut active_reads, "r1", first_generation);
        assert!(
            active_reads.contains_key("r1"),
            "a superseded completion must not evict the replacement"
        );
        // …while the live generation deregisters normally.
        deregister_finished_read(&mut active_reads, "r1", second_generation);
        assert!(!active_reads.contains_key("r1"));
    }

    /// The concurrency-cap rejection is returned to the driver for a
    /// direct datachannel send — never dropped into a possibly paused
    /// command lane.
    #[tokio::test]
    async fn read_cap_rejection_is_returned_for_direct_send() {
        let (command_tx, _command_rx) = mpsc::channel(COMMAND_CHANNEL);
        let authority = LiveTransferAuthority::new(PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "file-reader".into(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
        });
        let bus = crate::event::EventBus::new();
        let mut active_reads: HashMap<String, ActiveRead> = HashMap::new();
        let mut next_generation = 0u64;
        for index in 0..MAX_CONCURRENT_READS_PER_SESSION {
            let request = format!(
                r#"{{"t":"read","id":"r{index}","path":"/nonexistent/fixture","offset":0}}"#
            );
            assert!(handle_transfer_request(
                "session",
                &request,
                &authority,
                &bus,
                command_tx.clone(),
                &mut active_reads,
                &mut next_generation,
            )
            .is_none());
        }
        let rejection = handle_transfer_request(
            "session",
            r#"{"t":"read","id":"over","path":"/nonexistent/fixture","offset":0}"#,
            &authority,
            &bus,
            command_tx.clone(),
            &mut active_reads,
            &mut next_generation,
        )
        .expect("over-cap read must return a rejection for the driver to send");
        assert!(rejection.contains("too many concurrent reads"));
        assert_eq!(active_reads.len(), MAX_CONCURRENT_READS_PER_SESSION);
        for (_, read) in active_reads {
            read.cancel.cancel();
        }
    }

    /// Trust pin for the memoized wrapper: a positive verdict is reusable
    /// for at most [`LIVE_TRANSFER_AUTHORITY_MEMO_TTL`] — after the TTL a
    /// revocation is observed by the memoized path too, and the failed
    /// check must not re-prime the memo.
    #[test]
    fn transfer_authority_memo_detects_revocation_after_ttl() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fingerprint = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let authority = LiveTransferAuthority::new(approved_authorization(&tmp, fingerprint));
        assert!(authority.is_current());
        crate::peer::access_policy::revoke_identity(tmp.path(), fingerprint).unwrap();
        std::thread::sleep(LIVE_TRANSFER_AUTHORITY_MEMO_TTL + Duration::from_millis(50));
        assert!(!authority.is_current());
        // Negative verdicts are never memoized.
        assert!(!authority.is_current());
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
        let auth = LiveTransferAuthority::new(PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "operator".into(),
            filesystem: FilesystemAccessPolicy {
                read_roots: vec![tmp.path().to_path_buf()],
                write_roots: Vec::new(),
            },
            identity_record: None,
            iam_cert_dir: None,
        });
        let err = authorize_path(&auth, file.to_str().unwrap()).unwrap_err();
        assert!(err.contains("does not allow filesystem.read"));
    }

    #[test]
    fn authorize_path_accepts_file_reader_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"ok").unwrap();
        let auth = LiveTransferAuthority::new(PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "file-reader".into(),
            filesystem: FilesystemAccessPolicy {
                read_roots: vec![tmp.path().to_path_buf()],
                write_roots: Vec::new(),
            },
            identity_record: None,
            iam_cert_dir: None,
        });
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
        let auth = LiveTransferAuthority::new(PeerFileTransferAuthorization {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "file-reader".into(),
            filesystem: FilesystemAccessPolicy {
                read_roots: vec![tmp.path().to_path_buf()],
                write_roots: Vec::new(),
            },
            identity_record: None,
            iam_cert_dir: None,
        });

        let (_canonical, mut opened) =
            open_authorized_read_file(&auth, file.to_str().unwrap()).unwrap();
        std::fs::rename(&file, &moved).unwrap();
        std::fs::write(&file, b"replacement object").unwrap();

        let mut contents = String::new();
        opened.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "authorized object");
    }
}
