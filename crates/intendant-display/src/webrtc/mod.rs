//! Per-peer WebRTC driver built on the sans-I/O `rtc` core.
//!
//! Architecture: each `WebRtcPeer` owns a tokio task ("driver") that holds an
//! peer connection instance and UDP/TCP sockets. The driver pumps three things in a single
//! `select!` loop:
//!
//! 1. Inbound UDP/TCP datagrams → `peer.handle_read(TaggedBytesMut)`
//! 2. Encoded video frames from the shared encoder fan-out → `writer.write(...)`
//! 3. Commands from the public `WebRtcPeer` handle (ICE candidates, clipboard
//!    sends, shutdown) → `peer.add_remote_candidate()` / data channel writes
//!
//! After every input the driver drains the peer connection's pending writes,
//! reads, and events, and uses `poll_timeout` / `handle_timeout` to drive timers.
//!
//! ## ICE-TCP multiplexing
//!
//! The web gateway creates one shared `TcpPeerRegistry` at startup and
//! hands it to every peer via `handle_offer`. Peers pre-generate their
//! local ICE ufrag (so the registry key is known before the SDP answer
//! is produced) and register it with the registry at construction time.
//!
//! The web gateway's accept loop peeks every incoming TCP connection's
//! first bytes to tell HTTP vs. WebSocket vs. STUN-framed traffic apart.
//! STUN-framed traffic is read through one RFC 4571 frame and handed to
//! the registry, which parses the STUN USERNAME attribute, extracts the
//! target-ufrag half (per RFC 8445 §7.2.2 the USERNAME is
//! `<target_ufrag>:<sender_ufrag>`), and forwards the connection to the
//! matching peer's driver. Each TCP connection becomes a bidirectional
//! channel: inbound frames flow through the same packet channel UDP
//! uses (tagged with `TransportProtocol::TCP`), and outbound writes
//! with `proto == Tcp` is written to the connection's write half keyed
//! on the destination address.
//!
//! The advertised TCP candidate's address comes from the browser's
//! `Host:` HTTP header (parsed by the gateway): whatever non-loopback
//! IP the browser is already using to reach the dashboard, we advertise
//! as our ICE-TCP host candidate. Firefox would filter a remote
//! `127.0.0.1` candidate as an anti-rebinding mitigation, so a user who
//! accesses the dashboard via `http://localhost:…` through a
//! loopback-bound port-forward gets no TCP path — they need to access
//! via the host's LAN IP (or configure their port-forward on all
//! interfaces). This is documented in the README.

use super::clipboard::ClipboardContent;
use super::encode::pool::{
    CodecKind, EncoderId, EncoderPool, EncoderSubscription, PeerCodecPreferences, PoolLease,
    SimulcastRid,
};
use super::tile::backpressure::{TileDeltaBackpressure, TileDeltaSendDecision};
use super::tile::transport as tile_transport;
use super::{EncodedFrame, IceConfig, InputEvent, PeerId};
use intendant_core::error::CallerError;
use bytes::{Bytes, BytesMut};
use rtc::data_channel::{RTCDataChannelId, RTCDataChannelMessage};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264 as RTC_MIME_TYPE_H264, MIME_TYPE_VP8 as RTC_MIME_TYPE_VP8,
};
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCDtlsRole;
use rtc::peer_connection::transport::{RTCIceCandidateInit, RTCIceProtocol};
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::packetizer::{self, Packetizer};
use rtc::rtp::sequence;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RTCRtpHeaderExtensionCapability, RtpCodecKind,
};
use rtc::rtp_transceiver::RTCRtpSenderId;
use rtc::sansio::Protocol as RtcProtocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use rtc::statistics::report::RTCStatsReportEntry;
use rtc::statistics::stats::RTCStatsType;
use rtc::statistics::StatsSelector;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

mod tcp_mux;
pub use tcp_mux::*;
mod ice;
pub(crate) use ice::*;
mod offer;
pub(crate) use offer::*;
mod driver;
pub use driver::*;
mod pool_glue;
pub(crate) use pool_glue::*;

/// Bound on the per-peer encoded-frame channel. Frames in excess are dropped
/// with backpressure registered in the display metrics.
const ENCODED_FRAME_CHANNEL: usize = 8;

/// Bound on the per-peer command channel.
const COMMAND_CHANNEL: usize = 32;

/// Bound on the per-peer keyframe-request channel (driver → intake).
///
/// Lossy by design — the encoder pool's coalescer dedups bursts within
/// a small window, so a request lost to a full channel is reissued by
/// the next PLI/FIR within the same coalesce window. Sized to absorb
/// brief PLI storms (e.g. all simulcast layers requesting at once
/// after a keyframe loss) without backpressure on the rtc poll loop.
const KEYFRAME_REQUEST_CHANNEL: usize = 16;

/// **Phase 4d.1**: how often the driver polls `rtc.get_stats(..)` to
/// compute the per-peer recent observed send bitrate from outbound
/// `bytes_sent` deltas across one polling window.
///
/// 1s is the smallest interval where the bytes-delta has enough
/// signal-to-noise to be a useful steady-state observation: a 30fps
/// VP8 simulcast at ~3 Mbps total produces ~375 KB/poll at 1s, vs
/// per-packet jitter of single-KB. Faster polling (e.g. 200ms)
/// would amplify per-packet jitter into the rate estimate without
/// actually catching real bandwidth shifts any sooner. Polls
/// themselves are cheap (read-only walk of the rtc-side accumulator
/// state); the tradeoff is purely the staleness of the watch-channel
/// value the layer-selection aggregator (4d.2) reads.
///
/// **Why not `available_outgoing_bitrate`**: rtc 0.9's
/// `RTCIceCandidatePairStats::available_outgoing_bitrate` is
/// initialized to 0.0 by `rtc-ice-0.9.0` and never written to —
/// rtc 0.9's `update_ice_agent_stats` only copies STUN counters and
/// RTT, no congestion-control bandwidth estimate flows through.
/// Polling that field returns 0.0 forever. Deriving from
/// `bytes_sent` deltas observes a signal rtc 0.9 actually
/// maintains.
const TWCC_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Maximum UDP datagram we'll receive on the per-peer socket.
const UDP_BUF_LEN: usize = 2000;

/// Maximum RFC 4571 frame we'll accept over ICE-TCP (one STUN/DTLS/RTP packet).
/// DTLS records and RTP packets are bounded by MTU in practice; we use a
/// generous ceiling to accommodate jumbo frames without allowing pathological
/// memory allocation from a malicious peer.
const TCP_MAX_FRAME_LEN: usize = 65535;

/// Public handle to a single WebRTC peer.
///
/// All operations route to the driver task via channels; the driver owns the
/// RTC peer connection and UDP/TCP sockets exclusively.
pub struct WebRtcPeer {
    #[allow(dead_code)]
    pub peer_id: PeerId,
    command_tx: mpsc::Sender<Command>,
    /// **Phase 4d.1**: per-peer recent observed send bitrate in
    /// bits/sec, computed by the driver every `TWCC_POLL_INTERVAL`
    /// from outbound `bytes_sent` deltas across one polling window.
    /// `None` on the first poll (seeds the per-SSRC `prev` map) and
    /// any time the most recent window had zero usable deltas (no
    /// outbound traffic, counter wraparound, etc.); `Some(bps)` once
    /// a delta can be computed.
    ///
    /// **This is local egress, not available capacity.** It tells
    /// you "how many bits we just sent," not "how many bits the
    /// link could carry." Treating it as capacity creates a ratchet:
    /// pausing a layer drops observed egress, which then keeps the
    /// layer paused permanently. Capacity-driven layer adaptation
    /// needs a remote signal — RTCP RR `fraction_lost` per SSRC,
    /// TWCC arrival feedback, browser-side `getStats` — see 4d.3.
    observed_send_bitrate_rx: watch::Receiver<Option<u64>>,
    /// **Phase 4d.3a**: per-peer per-RID receiver-feedback health,
    /// derived from inbound RTCP RR via rtc 0.9's
    /// `RTCRemoteInboundRtpStreamStats` (the only RR-derived signal
    /// rtc 0.9 actually populates — see
    /// [`Self::observed_send_bitrate_rx`] for why local egress is
    /// the wrong proxy for capacity). Refreshed by the driver every
    /// `TWCC_POLL_INTERVAL` from the same `get_stats` call that
    /// drives `observed_send_bitrate`.
    ///
    /// Initial value is the empty map ("no RR has arrived for any
    /// SSRC yet"); per-RID entries appear as RRs arrive. A RID
    /// missing from the map means no signal yet for that layer —
    /// the 4d.3b/c policy treats missing as "stay conservative,
    /// don't act on absence."
    ///
    /// **Phase 4d.3a is observation only.** No layer decisions are
    /// made from this signal. 4d.3b adds the pure policy
    /// (per-(peer, RID) wanted-set + hysteresis); 4d.3c wires the
    /// aggregator to react.
    remote_inbound_health_rx: watch::Receiver<HashMap<SimulcastRid, PeerLayerHealth>>,
    /// **Phase 4d.3b**: per-peer aggregate TWCC health, published
    /// once per second by [`crate::twcc_tap::spawn_twcc_health_aggregator`].
    /// `None` initially (no window has fired yet), and `None` for
    /// any window in which no TWCC events arrived (silence is not
    /// recovery — see the aggregator's module docs). The channel
    /// transitions `None → Some(_) → None → Some(_)` as feedback
    /// arrives and goes silent across windows.
    ///
    /// This is the actionable capacity signal on this stack —
    /// rtc 0.9's `RTCRemoteInboundRtpStreamStats` (above) stays
    /// at all-zero defaults regardless of received RTCP because
    /// the rtc-interceptor chain consumes RTCP without
    /// surfacing it. The TWCC tap fills that gap by parsing
    /// `TransportLayerCc` packets directly at the chain.
    ///
    /// WKWebView's TWCC reporting is aggregate (single sender-SSRC
    /// across all RIDs in a simulcast send), not per-layer — the
    /// 4d.3b policy treats the signal as peer-wide and gates upper
    /// simulcast layers in cascade (full → half → floor-only) under
    /// sustained loss. Per-layer adaptation is a 4d.3c concern,
    /// dependent on receivers that emit per-RID TLC.
    twcc_health_rx: watch::Receiver<Option<crate::twcc_tap::TwccHealth>>,
    /// **#57**: the negotiated active RID set for this peer, frozen at
    /// construction time. The layer-policy coordinator
    /// ([`crate::aggregator::spawn_layer_policy_coordinator`])
    /// reads this each tick to compute the per-display "pinned"
    /// layer set: a peer with `active_rids.len() == 1` MUST keep its
    /// only RID active or it gets no frames at all (its WebRTC track
    /// only declares one encoding; pausing that layer in the encoder
    /// pool starves the peer rather than degrading it). Multi-RID
    /// peers (`len() > 1`) don't pin — the policy is free to pause
    /// upper layers because they have the floor as fallback.
    ///
    /// Stable for the peer's lifetime: WebRTC re-negotiation (mid-call
    /// SDP renegotiate) would change this, but the pool's
    /// peer-rebuild path drops + recreates the WebRtcPeer, so a fresh
    /// `active_rids` snapshot is always in lockstep with the
    /// negotiated answer SDP.
    active_rids: Vec<SimulcastRid>,
    shutdown: CancellationToken,
}

/// **Phase 4d.3a**: per-RID receiver-feedback health, derived from a
/// single `RTCRemoteInboundRtpStreamStats` entry (one outbound SSRC's
/// RR-reported state). Surfaced to the layer-selection aggregator via
/// [`WebRtcPeer::subscribe_remote_inbound_health`].
///
/// All fields come straight from rtc 0.9's RR accumulator (no delta
/// computation in 4d.3a — 4d.3b decides which signals to use and how).
#[derive(Clone, Debug, PartialEq)]
pub struct PeerLayerHealth {
    /// Fraction of packets lost on this layer in the most recent RR
    /// window, 0.0-1.0. RR-derived: instantaneous, not cumulative.
    /// The most actionable signal for "this layer's link can't
    /// sustain it right now."
    pub fraction_lost: f64,
    /// Cumulative packets lost on this layer since the connection
    /// started, as reported by the most recent RR. Signed because
    /// the upstream field is `i64` (negative values shouldn't occur
    /// in practice; surfaced as-is so callers can defend or assert
    /// per their needs).
    pub packets_lost_total: i64,
    /// Most recent round-trip time on this layer in seconds, from
    /// RTCP SR/RR exchange. `0.0` until the first RTT measurement
    /// lands.
    pub round_trip_time_seconds: f64,
    /// Number of RTT measurements ever recorded on this layer
    /// (monotonically non-decreasing). The freshness discriminator:
    /// rtc 0.9 keeps surfacing the same RR-derived field values
    /// every poll until the next RR arrives, so a `fraction_lost`
    /// reading repeated tick after tick may reflect a single RR
    /// from minutes ago — not fresh signal. The 4d.3c aggregator
    /// compares this count against its per-(peer, RID) prev-count
    /// snapshot; if the count didn't advance since last tick, the
    /// reading is stale and the policy receives `None` instead.
    /// This prevents stale loss readings from completing a 5s
    /// drop debounce all on their own.
    pub round_trip_time_measurements: u64,
}

impl WebRtcPeer {
    /// **Phase 4d.1**: subscribe to this peer's recent observed send
    /// bitrate signal. **Local egress only, not available capacity**
    /// — see the field docstring on [`Self::observed_send_bitrate_rx`]
    /// for the semantic distinction and why this can't drive
    /// capacity-based layer adaptation on its own. Returns a fresh
    /// `watch::Receiver` that always carries the latest published
    /// value (initial value `None` until the driver computes a
    /// `bytes_sent` delta).
    ///
    /// Receivers are independent — multiple subscribers (e.g. the
    /// per-display layer-selection aggregator AND a metrics
    /// dashboard) can each `subscribe_observed_send_bitrate` and read
    /// independently; calling `borrow_and_update` on one doesn't
    /// affect another.
    pub fn subscribe_observed_send_bitrate(&self) -> watch::Receiver<Option<u64>> {
        self.observed_send_bitrate_rx.clone()
    }

    /// **Phase 4d.1**: read the current observed send bitrate
    /// without subscribing. Useful for one-shot reads (debug /
    /// metrics snapshot). For change-driven consumers, prefer
    /// [`Self::subscribe_observed_send_bitrate`].
    pub fn current_observed_send_bitrate(&self) -> Option<u64> {
        *self.observed_send_bitrate_rx.borrow()
    }

    /// **Phase 4d.3a**: subscribe to this peer's per-RID receiver-
    /// feedback health signal. RR-derived (RTCP receiver reports
    /// the remote sends to us about our outbound streams) — unlike
    /// `observed_send_bitrate`, this IS a remote signal and CAN
    /// drive capacity decisions in 4d.3b/c.
    ///
    /// Returns a fresh `watch::Receiver` that always carries the
    /// latest published map (initial value is the empty map until
    /// the driver completes its first poll AND the first RR has
    /// arrived for at least one outbound SSRC).
    ///
    /// Receivers are independent — multiple subscribers (e.g. the
    /// layer-selection aggregator AND a metrics dashboard) can each
    /// `subscribe_remote_inbound_health` and read independently.
    pub fn subscribe_remote_inbound_health(
        &self,
    ) -> watch::Receiver<HashMap<SimulcastRid, PeerLayerHealth>> {
        self.remote_inbound_health_rx.clone()
    }

    /// **Phase 4d.3a**: read the current per-RID receiver-feedback
    /// health snapshot without subscribing. Returns the empty map
    /// until the first RR has arrived. For change-driven consumers,
    /// prefer [`Self::subscribe_remote_inbound_health`].
    pub fn current_remote_inbound_health(&self) -> HashMap<SimulcastRid, PeerLayerHealth> {
        self.remote_inbound_health_rx.borrow().clone()
    }

    /// **#57**: this peer's negotiated active RID set, frozen at
    /// construction. The layer-policy coordinator
    /// ([`crate::aggregator::spawn_layer_policy_coordinator`])
    /// reads this each tick to compute the per-display "pinned" layer
    /// set: a peer with `active_rids().len() == 1` MUST keep its only
    /// RID active or it gets no frames at all (its WebRTC track only
    /// declares one encoding; pausing that layer in the encoder pool
    /// starves the peer rather than degrading it). See the
    /// `active_rids` field doc on [`Self`] for the full rationale.
    pub fn active_rids(&self) -> &[SimulcastRid] {
        &self.active_rids
    }

    /// Test-only: construct a `WebRtcPeer` with just `active_rids`
    /// populated and dummy values for everything else. The dummy
    /// channels are constructed but their senders are dropped so
    /// any production caller that tries to use them will see closed-
    /// channel errors — only the layer-policy coordinator (which
    /// reads `active_rids()` and the watch channels' initial values)
    /// is intended to interact with these stubs.
    ///
    /// Used by `display::tests::pool_feed_bridge_*` to register a
    /// fake peer whose negotiated demand keeps all VP8 simulcast
    /// layers active across the layer-policy's per-tick demanded-
    /// bound check (#48). Without a registered peer, the policy
    /// computes `demanded = empty` and pauses every encoder
    /// immediately, which is correct production behavior but
    /// breaks tests that exercise the bridge → encoder → consumer
    /// pipeline directly.
    #[cfg(any(test, feature = "test-util"))]
    pub fn new_for_test(peer_id: PeerId, active_rids: Vec<SimulcastRid>) -> Self {
        use std::collections::HashMap;
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (_obs_tx, observed_send_bitrate_rx) = watch::channel(None);
        let (_ri_tx, remote_inbound_health_rx) =
            watch::channel(HashMap::<SimulcastRid, PeerLayerHealth>::new());
        let (_twcc_tx, twcc_health_rx) =
            watch::channel::<Option<crate::twcc_tap::TwccHealth>>(None);
        Self {
            peer_id,
            command_tx,
            observed_send_bitrate_rx,
            remote_inbound_health_rx,
            twcc_health_rx,
            active_rids,
            shutdown: CancellationToken::new(),
        }
    }

    /// **Phase 4d.3b**: subscribe to this peer's aggregate TWCC
    /// health signal. Published once per second by the
    /// [`crate::twcc_tap::spawn_twcc_health_aggregator`]
    /// task that drains the [`crate::twcc_tap::TwccTapInterceptor`]
    /// event stream.
    ///
    /// `None` means either "no window has fired yet" or "the most
    /// recent window had zero TWCC events." Silence is not
    /// recovery — see [`crate::twcc_tap`] module docs.
    /// The channel transitions `None → Some(_) → None → Some(_)`
    /// as feedback arrives and goes silent across windows. The
    /// capacity policy in [`crate::aggregator`] treats
    /// `None` and `Some(empty_health)` alike via its short-circuit
    /// arms and gates upper simulcast layers based only on
    /// sustained, non-empty loss readings.
    ///
    /// Receivers are independent — multiple subscribers (capacity
    /// aggregator + a metrics dashboard, say) can each
    /// `subscribe_twcc_health` and read independently.
    pub fn subscribe_twcc_health(
        &self,
    ) -> watch::Receiver<Option<crate::twcc_tap::TwccHealth>> {
        self.twcc_health_rx.clone()
    }

    /// **Phase 4d.3b**: read the current TWCC health snapshot
    /// without subscribing. Returns `None` if no window has fired
    /// yet OR if the most recent window had zero TWCC events
    /// (silence is not recovery — see the module docs at
    /// [`crate::twcc_tap`]). For change-driven consumers,
    /// prefer [`Self::subscribe_twcc_health`].
    pub fn current_twcc_health(&self) -> Option<crate::twcc_tap::TwccHealth> {
        *self.twcc_health_rx.borrow()
    }
}

/// Personalized display-input authority state for one viewer.
///
/// Wire vocabulary matches the local 5c data-channel protocol exactly
/// (see `web_gateway.rs::compute_bootstrap_authority_snapshots`). Used
/// by [`WebRtcPeer::send_authority_state`] for the federated path's
/// `display_input_authority` data channel — peer broadcasts a
/// personalized value to each subscribed federated browser.
///
/// Modelled as an enum (rather than passing `&str` through the API)
/// so the wire vocabulary lives in exactly one place; adding a future
/// state value is an explicit ABI change rather than a stringly-typed
/// caller mistake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayInputAuthorityState {
    You,
    Other,
    Unclaimed,
}

impl DisplayInputAuthorityState {
    /// Wire string for the `state` field of
    /// `display_input_authority_state` data-channel messages.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::You => "you",
            Self::Other => "other",
            Self::Unclaimed => "unclaimed",
        }
    }
}

/// F-1.3b2: Browser-originated authority message on the
/// `display_input_authority` data channel.
///
/// Wire format from the federated authority design (see
/// `docs/design-federated-input-authority.md` §Wire):
///
/// ```text
/// { "t": "display_input_authority_request", "display_id": 0 }
/// { "t": "display_input_authority_release", "display_id": 0 }
/// ```
///
/// `display/webrtc.rs` parses these frames off the wire and hands
/// them to an opaque [`AuthorityChannelHandler`] without applying any
/// policy. The handler — built outside the transport in
/// `web_gateway.rs` by the slice that wires the registry — consults
/// the federated authority registry and decides whether to grant /
/// release / no-op. Same separation as the existing
/// `input_handler`: webrtc.rs parses the wire shape, the gate lives
/// outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityChannelMessage {
    Request { display_id: u32 },
    Release { display_id: u32 },
}

/// F-1.3b2: opaque handler invoked on every parsed
/// [`AuthorityChannelMessage`] received on the
/// `display_input_authority` data channel.
///
/// Sibling to the existing `input_handler` constructor argument —
/// same `Arc<dyn Fn(...) + Send + Sync>` shape, same no-op default
/// for callers that don't gate authority. The closure runs on the
/// driver task, so it must not block; production handlers (added by
/// the federated wiring slice) push work to the federated authority
/// registry via non-blocking channels or atomic ops.
///
/// Local DisplaySlot's `WebRtcPeer` passes a no-op (see
/// [`noop_authority_handler`]) because the local browser doesn't
/// create the `display_input_authority` channel (5a/5c uses the WS
/// path); the federated `PeerDisplayConnection` does create it, and
/// the federated wiring slice plugs the real registry-driven handler
/// in there.
pub type AuthorityChannelHandler = Arc<dyn Fn(AuthorityChannelMessage) + Send + Sync>;

/// F-1.3b2: no-op [`AuthorityChannelHandler`] for callers that do not
/// gate authority on this peer. Used by the local DisplaySlot path
/// (browser doesn't create the channel) and as the placeholder on the
/// federated path until the federated wiring slice replaces it. Kept
/// as a single canonical source so future F-1.3b3 diffs against the
/// federated callsite are isolated to one line.
pub fn noop_authority_handler() -> AuthorityChannelHandler {
    Arc::new(|_| {})
}

/// D-4d2: Browser-originated recovery/control messages on the
/// `tile-control` data channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileControlMessage {
    Subscribe {
        client_id: u32,
    },
    SnapshotRequest {
        epoch: u32,
        reason: tile_transport::SnapshotRequestReason,
    },
    GapReport {
        epoch: u32,
        last_seen_seq: u32,
        expected_seq: u32,
    },
}

/// Opaque transport callback for parsed tile-control frames.
///
/// The driver task invokes this synchronously; production handlers
/// must spawn any async recovery work rather than blocking the RTC
/// pump.
pub type TileControlHandler = Arc<dyn Fn(TileControlMessage) + Send + Sync>;

#[allow(dead_code)]
pub fn noop_tile_control_handler() -> TileControlHandler {
    Arc::new(|_| {})
}

/// D-3b: Tile-stream data-channel labels.
///
/// Browser-side `PeerDisplayConnection` creates these channels before
/// `createOffer()`. The peer passively observes them through
/// `OnDataChannel(OnOpen)` and writes binary tile frames by label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileDataChannel {
    Control,
    Snapshot,
    Deltas,
}

impl TileDataChannel {
    fn label(self) -> &'static str {
        match self {
            Self::Control => TILE_CONTROL_CHANNEL_LABEL,
            Self::Snapshot => TILE_SNAPSHOT_CHANNEL_LABEL,
            Self::Deltas => TILE_DELTAS_CHANNEL_LABEL,
        }
    }

    fn queues_before_open(self) -> bool {
        matches!(self, Self::Control | Self::Snapshot)
    }
}

/// Commands sent from the public `WebRtcPeer` handle to the driver task.
pub(crate) enum Command {
    AddIceCandidate(String),
    SendClipboard(ClipboardContent),
    /// F-1.2: federated authority state push to the
    /// `display_input_authority` data channel. If the channel is not
    /// yet open, the driver queues the message in
    /// [`DriverState::pending_authority_state`] and flushes on
    /// `OnDataChannel(OnOpen)` for that label. Without queueing, an
    /// authority state computed before the browser's data channel
    /// finishes negotiating would land on the floor and the browser's
    /// chip would stall at `unknown` until the next state change.
    SendAuthorityState {
        display_id: u32,
        state: DisplayInputAuthorityState,
    },
    /// D-3b: binary tile-stream frame. Control/snapshot frames queue
    /// until their reliable data channel opens; delta frames are
    /// latest-wins and are dropped when the channel is unavailable.
    SendTileFrame {
        channel: TileDataChannel,
        data: Vec<u8>,
    },
}

pub(crate) struct RtpSendConfig {
    sender_id: RTCRtpSenderId,
    mid: String,
    codec: RTCRtpCodec,
    /// One entry per simulcast layer (or one entry for non-simulcast
    /// codecs like H.264). Each pair is the layer's `(SimulcastRid,
    /// SSRC)` — the SSRC matches the value passed into the
    /// [`MediaStreamTrack`]'s `RTCRtpEncodingParameters` for this RID
    /// at construction, so [`rtc`]'s `RTCRtpSender::write_rtp` (which
    /// routes to encodings by `packet.header.ssrc`) finds the right
    /// encoding when the driver writes a packet.
    ///
    /// Phase 4c (post-this-commit) populates this with N entries for
    /// VP8 simulcast. This commit (the refactor that prepares for it)
    /// always populates with exactly ONE entry — single-encoding
    /// behavior is preserved bit-for-bit until commit 2 lights up
    /// multi-encoding.
    encodings: Vec<(SimulcastRid, u32)>,
}

/// Encoded frame paired with the simulcast RID it came from. Carried
/// over the per-peer mpsc channel between [`pool_frame_intake`]
/// (producer) and [`driver`] (consumer).
///
/// The RID does NOT live on [`EncodedFrame`] itself — that struct is
/// the encoder pool's output, shared across all subscribers of a given
/// `(codec, rid)` slot, and an encoder doesn't know which subscriber's
/// RID it's serving (it just knows its own slot's rid). The pool
/// forwarder reads the rid off its [`EncoderSubscription`] (which
/// carries the [`crate::encode::pool::EncoderId`] containing
/// `(codec, rid)`) and wraps each frame here at hand-off.
///
/// The driver uses the rid to look up the matching encoding's SSRC +
/// per-`(spec, rid)` keyframe gate — see
/// [`DriverState::video_specs`] and [`RtpSendState::by_rid`] for the
/// keying decisions.
pub(crate) struct OutboundEncodedFrame {
    rid: SimulcastRid,
    frame: Arc<EncodedFrame>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // F-1.2: federated authority state — passive server-side support
    // for the `display_input_authority` data channel.
    // -----------------------------------------------------------------

    /// Wire vocabulary pin: `as_wire_str` matches the local 5c
    /// data-channel state strings exactly. If anyone changes one
    /// without the other, federated browsers' chip rendering desyncs
    /// from local browsers' chip rendering — this test fires.
    #[test]
    fn authority_state_wire_strings_match_local_5c() {
        assert_eq!(DisplayInputAuthorityState::You.as_wire_str(), "you");
        assert_eq!(DisplayInputAuthorityState::Other.as_wire_str(), "other");
        assert_eq!(
            DisplayInputAuthorityState::Unclaimed.as_wire_str(),
            "unclaimed"
        );
    }

    /// D-3b: tile data-channel labels and queue policy are part of
    /// the browser<->peer contract. Control/snapshot are reliable
    /// bootstrap channels and may queue before open; deltas are
    /// supersedable and must not queue stale frames.
    #[test]
    fn tile_data_channel_labels_and_queue_policy_match_wire_contract() {
        assert_eq!(TileDataChannel::Control.label(), "tile-control");
        assert_eq!(TileDataChannel::Snapshot.label(), "tile-snapshot");
        assert_eq!(TileDataChannel::Deltas.label(), "tile-deltas");

        assert!(TileDataChannel::Control.queues_before_open());
        assert!(TileDataChannel::Snapshot.queues_before_open());
        assert!(!TileDataChannel::Deltas.queues_before_open());
    }
}
