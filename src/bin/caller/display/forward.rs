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
// Why PerPeerForwarder lives inside the WebRtcPeer driver now
// ---------------------------------------------------------------------------
//
// An earlier design stub had a separate `PerPeerForwarder` type with a
// `run()` method that would loop over encoder subscriptions and write
// via `str0m::Writer::write_sample`. That design can't work: the
// `Rtc` instance is owned by the `WebRtcPeer` driver task
// (`display/webrtc.rs`), and a separate forwarder task has no path to
// call str0m's writer APIs. Moving the forwarder responsibilities
// into the driver avoids the cross-task-Rtc-access problem entirely:
// each peer's driver select!-loops over its pool subscriptions
// alongside its existing command/event arms, and the pt-caching +
// keyframe-gate logic from the stub merges into `DriverState`.
//
// What stays in this module:
//
// - [`LayerSelector`] — per-peer simulcast layer choice, wired to
//   TWCC bandwidth events in phase 4.
// - [`ForwarderError`] — shared vocabulary for the handful of
//   forwarder-layer failure modes (surfaced from the driver's write
//   path).
// - [`codec_preferences_from_offer`] — SDP-offer → `PeerCodecPreferences`
//   helper used by `handle_offer` before `pool.subscribe`.
//
// What was deleted: `PerPeerForwarder` struct, `PerPeerForwarderState`
// struct, the `should_forward` keyframe-gate helper (now lives in the
// driver's `keyframe_seen` check in `write_video_frame`), and the
// placeholder `run` method that couldn't call str0m.

// ---------------------------------------------------------------------------
// Helper: derive PeerCodecPreferences from an offer SDP
// ---------------------------------------------------------------------------

/// Build [`PeerCodecPreferences`] from a browser's offer SDP.
///
/// The returned preferences contain only codecs whose **exact**
/// payload spec the encoder pool can actually match via
/// `str0m::Writer::match_params`. This matters for H.264: an offer
/// with an rtpmap of `H264/90000` but fmtp of `profile-level-id=64001f`
/// (High) and `packetization-mode=0` would previously end up as
/// `CodecKind::H264` in prefs, get subscribed, and then every
/// encoded frame would fail match_params because the pool's encoder
/// produces Constrained Baseline / mode 1 — a silent-black-screen
/// class of bug that the whole 3c.0 contract exists to prevent.
///
/// The guard is [`crate::display::encode::has_compatible_h264_offer`],
/// which checks for a Baseline-family (profile_idc 0x42) variant
/// with packetization-mode 0 or 1 — the intersection of what our
/// VideoToolbox / VAAPI / libx264 backends produce and what str0m
/// will actually negotiate against the encoder's cached
/// [`crate::display::encode::PayloadSpec::h264_constrained_baseline`].
///
/// VP9 / AV1 don't need the guard today (no backend; pool excludes
/// them at `on_demand_spawnable`), but including them unconditionally
/// in prefs is harmless and matches the "prefs advertise what the
/// peer supports, pool decides what's serveable" split.
pub fn codec_preferences_from_offer(sdp: &str) -> PeerCodecPreferences {
    let offered = crate::display::encode::parse_offered_codecs(sdp);
    let mut supported = Vec::new();
    for name in offered {
        match name.as_str() {
            "VP8" => supported.push(CodecKind::Vp8),
            "H264" => {
                // Only include H.264 if the offer carries a variant
                // that str0m's Writer::match_params would accept
                // against our encoder's exact PayloadSpec. The older
                // `has_compatible_h264_offer` is broader than that
                // (it accepts packetization-mode 0, missing fmtp,
                // and any profile_idc = 0x42 regardless of
                // constraint_set1_flag) — str0m rejects all of
                // those, so they'd result in silent black-screen
                // frame-drop. See the detailed rules next to
                // `offer_has_poolable_h264_variant`.
                if crate::display::encode::offer_has_poolable_h264_variant(sdp) {
                    supported.push(CodecKind::H264);
                }
            }
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

    // PerPeerForwarder tests were deleted with the type — the
    // keyframe-gate regression guard moves to the driver's write path
    // (`display/webrtc.rs::write_video_frame`), where `state.keyframe_seen`
    // now lives. Driver-side coverage is via e2e webrtc tests rather
    // than in-unit tests because the relevant path requires a live
    // `Rtc` instance.

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
        // Skeleton SDP carrying the codec rtpmap lines that
        // `parse_offered_codecs` looks for. The H.264 line carries
        // a Constrained Baseline + packetization-mode 1 fmtp so the
        // strict `offer_has_poolable_h264_variant` gate admits it —
        // this test is about "do we see all three codec families,"
        // separate from the H.264-profile-specific tests below that
        // cover the edge cases of the strict gate.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97 98\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=1\r\n",
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

    /// Finding 1 in the 3c.0a review: an offer that advertises H.264
    /// with an *incompatible* profile (e.g., High `64001f` + mode 2)
    /// must NOT produce `CodecKind::H264` in prefs, because the pool
    /// only produces Constrained Baseline / mode 1 and str0m would
    /// match_params-miss every frame. The legacy path's
    /// `is_compatible_h264_profile` does the exact check.
    #[test]
    fn codec_preferences_excludes_incompatible_h264_profile() {
        // H.264 High (profile_idc=0x64 = 100), packetization-mode=2 (well
        // beyond our encoder's max). VP8 on a separate PT so the peer
        // still has a compatible codec.
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n",
            "a=rtpmap:96 VP8/90000\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=64001f;packetization-mode=2\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(prefs.supports(CodecKind::Vp8), "VP8 must remain supported");
        assert!(
            !prefs.supports(CodecKind::H264),
            "H.264 High/mode 2 must NOT be claimed — encoder produces \
             Constrained Baseline/mode 1 only, str0m would drop every frame"
        );
    }

    /// Complement of the above: Baseline + mode 1 is what our encoder
    /// produces, so it must be included.
    #[test]
    fn codec_preferences_includes_compatible_h264_baseline() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=1\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(prefs.supports(CodecKind::H264));
    }

    /// An offer with multiple H.264 variants — one compatible, one not —
    /// should still claim H.264 support. str0m picks the compatible
    /// variant for negotiation; the incompatible one is ignored.
    #[test]
    fn codec_preferences_h264_mixed_variants_keeps_codec() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97 98\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=64001f;packetization-mode=0\r\n", // High, incompatible
            "a=rtpmap:98 H264/90000\r\n",
            "a=fmtp:98 profile-level-id=42e01f;packetization-mode=1\r\n", // Baseline, compatible
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            prefs.supports(CodecKind::H264),
            "H.264 must be claimed when at least one offered variant is compatible"
        );
    }

    // -----------------------------------------------------------------------
    // Strict H.264 filter tests (findings #1 revisited in 3c.0a review)
    //
    // Each of these variants was previously accepted by the legacy
    // `has_compatible_h264_offer` helper but would be rejected by
    // str0m's `Writer::match_params` against the encoder's exact
    // `PayloadSpec::h264_constrained_baseline`. Result: silent
    // black-screen frame drops. The new `offer_has_poolable_h264_variant`
    // gate must exclude each one.
    // -----------------------------------------------------------------------

    /// `42e01f` profile (Constrained Baseline, the match) but
    /// packetization-mode 0 (encoder produces mode 1). str0m's matcher
    /// requires equality on packetization-mode.
    #[test]
    fn codec_preferences_excludes_h264_wrong_packetization_mode() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01f;packetization-mode=0\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "packetization-mode 0 must be rejected — encoder emits mode 1 only"
        );
    }

    /// `42001f` profile (Baseline, NOT Constrained Baseline) at
    /// packetization-mode 1. str0m's profile resolver maps this to
    /// `H264Profile::Baseline` while our encoder emits
    /// `ConstrainedBaseline`; the matcher requires profile equality.
    #[test]
    fn codec_preferences_excludes_h264_pure_baseline_without_cs1() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42001f;packetization-mode=1\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "Pure Baseline (cs1 unset) must be rejected — encoder emits Constrained Baseline"
        );
    }

    /// H.264 rtpmap with NO fmtp line at all. `parse_h264_fmtp` treats
    /// missing fmtp as packetization-mode 0 + empty profile-level-id,
    /// and str0m's `match_params` falls back to Baseline/Level 1 for
    /// missing profile-level-id. Both axes disagree with our encoder.
    #[test]
    fn codec_preferences_excludes_h264_with_no_fmtp() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "No fmtp implies str0m-fallback Baseline + mode 0 — must be rejected"
        );
    }

    /// Correct profile + mode but offered level 5.0 (`4d0032` —
    /// actually Main at Level 5.0; use `42e028` for ConstrainedBaseline
    /// at Level 4.0 to keep profile family). Level 4.0 (0x28) > our
    /// encoder's Level 3.1 (0x1f); str0m rejects when the offer's
    /// level exceeds ours.
    #[test]
    fn codec_preferences_excludes_h264_when_offered_level_exceeds_encoder() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e028;packetization-mode=1\r\n",
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            !prefs.supports(CodecKind::H264),
            "Level 4.0 offer exceeds encoder's Level 3.1 ceiling — must be rejected"
        );
    }

    /// Correct profile, correct mode, level LOWER than ours (3.0 vs 3.1).
    /// str0m accepts `c1_level <= c0_level`, so this matches.
    #[test]
    fn codec_preferences_includes_h264_at_lower_level() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 97\r\n",
            "a=rtpmap:97 H264/90000\r\n",
            "a=fmtp:97 profile-level-id=42e01e;packetization-mode=1\r\n", // Level 3.0
        );
        let prefs = codec_preferences_from_offer(sdp);
        assert!(
            prefs.supports(CodecKind::H264),
            "Level 3.0 is below encoder's Level 3.1 — must be accepted (str0m: c1_level <= c0_level)"
        );
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
