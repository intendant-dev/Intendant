//! Per-peer forwarder: translates encoder output into one peer's
//! WebRTC RTP stream, with per-peer codec / simulcast-layer selection.
//!
//! ## Why this exists
//!
//! The peer pool (see [`crate::display::encode::pool`]) produces
//! encoded frames per `(codec, rid)`. Each WebRTC peer needs those
//! frames rewritten into its own RTP track with:
//!
//! - **Its own negotiated payload type (PT)**. Different peers can land
//!   on different PTs for the same codec depending on each peer's
//!   offer SDP — str0m acknowledges this explicitly in the
//!   [`Writer::match_params`] documentation: *"a certain codec
//!   configuration might not have the same payload type (PT) for two
//!   different peers."*
//! - **Its own SSRC + sequence numbers + RTP timestamps**, managed by
//!   str0m internally per [`Rtc`] instance.
//! - **Its own simulcast layer choice**, which may shift as TWCC
//!   bandwidth estimates change. Today this is static (`Rid::full`);
//!   phase 4 wires TWCC events into layer selection.
//!
//! ## Pattern: str0m's chat.rs SFU example
//!
//! The str0m crate ships an SFU example that's structurally identical
//! to what we need. Key elements:
//!
//! 1. **One [`Rtc`] per peer.** str0m does not support per-peer codec
//!    selection inside a single `Rtc`, so each peer gets its own.
//!    (str0m example: `Rtc::builder().build(Instant::now())`.)
//! 2. **Receive `Event::MediaData` from the publisher `Rtc`, enqueue
//!    onto a shared channel.** Our publisher is not an `Rtc` — it's
//!    the encoder pool producing `EncodedFrame` directly — but the
//!    channel abstraction is the same.
//! 3. **For each subscriber `Rtc`, translate PT via
//!    [`Writer::match_params`] and call [`Writer::write`] with the
//!    codec-specific sample.** This is the core of the forwarder loop.
//! 4. **For simulcast sources, the subscriber filters by RID.** The
//!    str0m example hard-codes to one RID; we pick per-peer based on
//!    TWCC bandwidth.
//!
//! ## Keyframe-first guard
//!
//! A peer that joins mid-stream MUST receive a keyframe before any
//! P-frame or the decoder produces garbage (browser shows black or
//! corruption until the next natural keyframe, often 2-5 seconds
//! later on static content).
//!
//! Every forwarder starts with `keyframe_seen: false`. Until set, it
//! drops non-keyframe frames and requests a keyframe from the pool
//! via [`crate::display::encode::pool::EncoderPool::request_keyframe`].
//! The pool's keyframe coalescer ensures N late-joiners produce one
//! keyframe, not N.
//!
//! Once the forwarder sees its first keyframe, it sets the flag and
//! forwards all subsequent frames (keyframe and P).
//!
//! ## Layer selection
//!
//! Simulcast lets one peer pick the layer it can sustain over its
//! link:
//!
//! - Full-resolution peer on LAN: RID `f`.
//! - Browser behind a 2 Mbps shared WiFi: RID `h` (or `q` under load).
//! - Browser on a mobile hotspot: RID `q`.
//!
//! The per-peer [`LayerSelector`] holds the currently-active RID and
//! accepts feedback from str0m's `Event::EgressBitrateEstimate` (phase
//! 4). In this design stub, selection is static (`RID_FULL` default).
//!
//! ## What this module is NOT doing yet
//!
//! - Spawning the forward loop task (phase 4).
//! - Wiring str0m's `Event::KeyframeRequest` → pool (phase 4).
//! - Wiring str0m's `Event::EgressBitrateEstimate` → layer selector
//!   (phase 4).
//! - Per-peer RTP timestamp anchoring beyond what str0m does internally
//!   (phase 4).
//!
//! This stub captures the types, the forwarder state machine, and the
//! per-peer contract the pool depends on. Phase 4 fills in the runtime.
//!
//! [`Rtc`]: https://docs.rs/str0m
//! [`Writer::match_params`]: https://docs.rs/str0m/latest/str0m/media/struct.Writer.html
//! [`Writer::write`]: https://docs.rs/str0m/latest/str0m/media/struct.Writer.html

use crate::display::encode::pool::{
    CodecKind, EncoderSubscription, PeerCodecPreferences, SimulcastRid,
};
use crate::display::EncodedFrame;
use crate::display::PeerId;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{broadcast, RwLock};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Forwarder-layer errors. These are in-process errors; wire-layer
/// WebRTC errors stay in [`crate::display::webrtc`].
#[derive(Debug)]
pub enum ForwarderError {
    /// Peer advertised codecs but the pool returned no subscriptions —
    /// the peer's codec set doesn't overlap with any encoder the pool
    /// is producing (or willing to spawn). Surfaces to the WebRTC
    /// handler as "offer rejected: no compatible codec."
    NoCompatibleCodec,
    /// str0m's [`Writer::match_params`] returned `None` — encoder PT
    /// doesn't have a peer-negotiated equivalent. Should be impossible
    /// if the pool subscription set matches the peer's negotiated
    /// codec set, so this represents a bug: fail loud.
    PayloadTypeTranslationFailed {
        codec: CodecKind,
        rid: SimulcastRid,
    },
    /// Subscriber channel lagged past recovery. str0m handles SFU-side
    /// losses with NACK + PLI, so the forwarder recovers naturally;
    /// this variant exists for logging / metrics not for failure
    /// semantics.
    SubscriptionLagged {
        codec: CodecKind,
        rid: SimulcastRid,
        skipped: u64,
    },
}

impl fmt::Display for ForwarderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCompatibleCodec => {
                write!(f, "peer's codec set does not overlap with pool output")
            }
            Self::PayloadTypeTranslationFailed { codec, rid } => write!(
                f,
                "str0m match_params returned None for {}:{} (pool/peer codec set mismatch)",
                codec, rid
            ),
            Self::SubscriptionLagged {
                codec,
                rid,
                skipped,
            } => write!(
                f,
                "forwarder skipped {} frames on {}:{} (slow subscriber, self-healing via NACK/PLI)",
                skipped, codec, rid
            ),
        }
    }
}

impl std::error::Error for ForwarderError {}

// ---------------------------------------------------------------------------
// Layer selector
// ---------------------------------------------------------------------------

/// Per-peer simulcast layer selection. Holds the currently-active RID;
/// the forwarder reads it on each frame to decide whether to forward
/// or drop.
///
/// Layer changes come from str0m's `Event::EgressBitrateEstimate` in
/// phase 4. For now: static default `RID_FULL`, updates via
/// [`LayerSelector::prefer`] which is called from the forwarder's
/// keyframe-request path (a new layer needs a fresh keyframe to be
/// decodable, so layer-switch and keyframe-request are paired).
pub struct LayerSelector {
    active: RwLock<SimulcastRid>,
}

impl LayerSelector {
    pub fn new() -> Self {
        Self {
            active: RwLock::new(SimulcastRid::full()),
        }
    }

    pub fn with_initial(rid: SimulcastRid) -> Self {
        Self {
            active: RwLock::new(rid),
        }
    }

    /// Currently-active RID. Cheap (read lock on a small value).
    pub async fn active(&self) -> SimulcastRid {
        self.active.read().await.clone()
    }

    /// Switch to a new layer. The caller should also request a
    /// keyframe — P-frames against the new layer's keyframe chain
    /// don't decode against the old layer's keyframe.
    pub async fn prefer(&self, rid: SimulcastRid) {
        *self.active.write().await = rid;
    }
}

impl Default for LayerSelector {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Per-peer forwarder state
// ---------------------------------------------------------------------------

/// State owned by one peer's forwarder. Created at
/// [`PerPeerForwarder::new`] time, mutated by the forwarder loop.
///
/// The fields that change over the peer's lifetime are behind
/// synchronization primitives because (a) the forwarder loop
/// mutates them, (b) str0m event handlers read them (e.g. the PLI
/// handler reads `keyframe_seen` to decide whether to force a
/// keyframe vs just forward one).
pub struct PerPeerForwarderState {
    pub peer_id: PeerId,
    pub prefs: PeerCodecPreferences,
    pub layer: LayerSelector,
    /// Has this peer received ≥1 keyframe? False blocks P-frame
    /// forwarding; true admits all frames.
    pub keyframe_seen: AtomicBool,
}

impl PerPeerForwarderState {
    pub fn new(peer_id: PeerId, prefs: PeerCodecPreferences) -> Self {
        Self {
            peer_id,
            prefs,
            layer: LayerSelector::new(),
            keyframe_seen: AtomicBool::new(false),
        }
    }

    /// Whether the forwarder should forward this frame given the
    /// current `keyframe_seen` state and the frame's keyframe flag.
    /// Used by the run loop; exposed here for testing.
    pub fn should_forward(&self, frame: &EncodedFrame) -> bool {
        if self.keyframe_seen.load(Ordering::Acquire) {
            return true;
        }
        if frame.is_keyframe {
            self.keyframe_seen.store(true, Ordering::Release);
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Per-peer forwarder
// ---------------------------------------------------------------------------

/// Runs one peer's forwarding loop. Owns the state and the set of
/// encoder subscriptions; on `run`, consumes both and drives frames
/// into the peer's str0m [`Rtc`] instance.
///
/// Constructed from [`EncoderPool::subscribe`]'s result, which is why
/// the pool owns the codec-negotiation and subscription logic and the
/// forwarder is pure plumbing: codec fit has already been checked.
///
/// **Stub:** `new` is wired; `run` has the loop shape in the docstring
/// but the body is a phase-4 placeholder (no str0m calls yet).
pub struct PerPeerForwarder {
    state: Arc<PerPeerForwarderState>,
    subscriptions: Vec<EncoderSubscription>,
}

impl PerPeerForwarder {
    /// Construct a forwarder for one peer.
    ///
    /// Returns [`ForwarderError::NoCompatibleCodec`] if the
    /// subscription set is empty — the peer's codec preferences
    /// don't overlap with any encoder the pool can produce. This
    /// should normally be caught earlier in the handshake
    /// (`handle_offer` returns an error SDP), but the check here is
    /// the backstop.
    pub fn new(
        peer_id: PeerId,
        prefs: PeerCodecPreferences,
        subscriptions: Vec<EncoderSubscription>,
    ) -> Result<Self, ForwarderError> {
        if subscriptions.is_empty() {
            return Err(ForwarderError::NoCompatibleCodec);
        }
        Ok(Self {
            state: Arc::new(PerPeerForwarderState::new(peer_id, prefs)),
            subscriptions,
        })
    }

    /// Handle to the forwarder's state, for PLI / TWCC callbacks from
    /// str0m's event loop that need to mutate the forwarder
    /// (e.g. `layer.prefer()` on bandwidth change, or reading
    /// `keyframe_seen` to coalesce keyframe requests).
    pub fn state(&self) -> Arc<PerPeerForwarderState> {
        Arc::clone(&self.state)
    }

    /// Run the forward loop. Phase 4 fills in the body; the intended
    /// structure is documented here so reviewers can evaluate the
    /// design without reading every phase's PR:
    ///
    /// ```text
    ///     loop {
    ///         tokio::select! {
    ///             // For each subscription, pull the next frame.
    ///             // (Multiple subscriptions happen when a peer
    ///             // supports multiple codecs; we pick one based on
    ///             // the peer's negotiated codec in str0m's Rtc and
    ///             // drop frames from the others.)
    ///             Ok(frame) = sub_vp8_full.recv() if self.peer_wants(Vp8, Full) => {
    ///                 if !self.state.should_forward(&frame) {
    ///                     // Pre-keyframe P-frame; drop + ask pool for KF.
    ///                     self.pool.request_keyframe(Vp8, Some(Full)).await;
    ///                     continue;
    ///                 }
    ///                 let pt = writer.match_params(encoder_params)
    ///                     .ok_or(ForwarderError::PayloadTypeTranslationFailed)?;
    ///                 writer.write(pt, rtp_time, &frame.data, Some(rid))?;
    ///             }
    ///             // ...similar arms for other (codec, rid) subscriptions.
    ///             _ = shutdown.cancelled() => break,
    ///         }
    ///     }
    /// ```
    ///
    /// **Stub:** returns `Ok(())` immediately. Consumers of this type
    /// can exercise the constructor and `state()` without pulling in
    /// the str0m runtime.
    pub async fn run(self) -> Result<(), ForwarderError> {
        // Phase 4.
        let _ = self.subscriptions;
        let _ = self.state;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper: derive PeerCodecPreferences from an offer SDP
// ---------------------------------------------------------------------------

/// Build [`PeerCodecPreferences`] from a browser's offer SDP.
///
/// Uses the existing [`crate::display::encode::parse_offered_codecs`]
/// parser so we share vocabulary with the legacy codec-selection
/// path — there's one source of truth for "what did this SDP
/// advertise."
pub fn codec_preferences_from_offer(sdp: &str) -> PeerCodecPreferences {
    let offered = crate::display::encode::parse_offered_codecs(sdp);
    let mut supported = Vec::new();
    for name in offered {
        match name.as_str() {
            "VP8" => supported.push(CodecKind::Vp8),
            "H264" => supported.push(CodecKind::H264),
            "VP9" => supported.push(CodecKind::Vp9),
            "AV1" => supported.push(CodecKind::Av1),
            _ => {
                // Non-video or RTX / RED / ULPFEC — ignored; these
                // aren't codecs the encoder produces. The existing
                // parser returns these too.
            }
        }
    }
    PeerCodecPreferences::new(supported)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::encode::pool::{
        EncoderId, EncoderSubscription, LayerSpec, SimulcastRid,
    };
    use tokio::sync::broadcast;

    fn make_subscription(codec: CodecKind, rid: SimulcastRid) -> EncoderSubscription {
        let (_tx, rx) = broadcast::channel(16);
        EncoderSubscription {
            id: EncoderId::new(codec, rid.clone()),
            layer: LayerSpec::single(codec, 640, 480, 30),
            frames: rx,
        }
    }

    fn peer_id(n: u64) -> PeerId {
        n
    }

    #[tokio::test]
    async fn forwarder_rejects_empty_subscription_set() {
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let result = PerPeerForwarder::new(peer_id(1), prefs, vec![]);
        assert!(matches!(result, Err(ForwarderError::NoCompatibleCodec)));
    }

    #[tokio::test]
    async fn forwarder_accepts_one_subscription() {
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let sub = make_subscription(CodecKind::Vp8, SimulcastRid::full());
        let fwd = PerPeerForwarder::new(peer_id(1), prefs, vec![sub]).unwrap();
        let state = fwd.state();
        assert_eq!(state.peer_id, 1);
        assert!(!state.keyframe_seen.load(Ordering::Acquire));
    }

    #[test]
    fn keyframe_gate_blocks_pframes_until_first_keyframe() {
        let state = PerPeerForwarderState::new(
            peer_id(1),
            PeerCodecPreferences::new(vec![CodecKind::Vp8]),
        );

        let pframe = EncodedFrame {
            data: vec![0],
            pts_ms: 0,
            duration_ms: 33,
            is_keyframe: false,
        };
        let keyframe = EncodedFrame {
            data: vec![0],
            pts_ms: 33,
            duration_ms: 33,
            is_keyframe: true,
        };

        // Pre-keyframe P-frame is rejected.
        assert!(!state.should_forward(&pframe));
        // Keyframe flips the gate open and is itself forwarded.
        assert!(state.should_forward(&keyframe));
        // All subsequent frames, keyframe or not, are forwarded.
        assert!(state.should_forward(&pframe));
        assert!(state.should_forward(&keyframe));
    }

    #[tokio::test]
    async fn layer_selector_starts_at_full() {
        let sel = LayerSelector::new();
        assert_eq!(sel.active().await, SimulcastRid::full());
    }

    #[tokio::test]
    async fn layer_selector_switches_on_prefer() {
        let sel = LayerSelector::new();
        sel.prefer(SimulcastRid::quarter()).await;
        assert_eq!(sel.active().await, SimulcastRid::quarter());
    }

    #[test]
    fn codec_preferences_from_offer_parses_known_codecs() {
        // Skeleton SDP carrying the codec rtpmap lines
        // parse_offered_codecs looks for.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=rtpmap:98 AV1/90000\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(prefs.supports(CodecKind::Vp8));
        assert!(prefs.supports(CodecKind::H264));
        assert!(prefs.supports(CodecKind::Av1));
        assert!(!prefs.supports(CodecKind::Vp9));
    }

    #[test]
    fn codec_preferences_from_offer_ignores_non_codec_lines() {
        // RTX, ULPFEC, RED are not primary codecs; the pool doesn't
        // produce them directly and the forwarder shouldn't claim the
        // peer "supports" them as if they were decodable stand-alone.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 RTX/90000\r\n",
            "a=rtpmap:98 ulpfec/90000\r\n",
            "a=rtpmap:99 red/90000\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert_eq!(prefs.supported, vec![CodecKind::Vp8]);
    }

    #[test]
    fn forwarder_error_display_includes_codec_id() {
        let e = ForwarderError::PayloadTypeTranslationFailed {
            codec: CodecKind::H264,
            rid: SimulcastRid::full(),
        };
        let s = format!("{}", e);
        assert!(s.contains("h264"));
        assert!(s.contains("f"));
    }
}
