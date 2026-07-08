//! Pool vocabulary types: tunables and RID constants, codec identity
//! ([`CodecKind`], the platform baseline), [`SimulcastRid`], layer
//! specification ([`LayerSpec`] and its factories), encoder identity and
//! handles, the I420 input frame, subscriptions, and peer codec
//! preferences.

use super::*;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum attempts [`EncoderPool::subscribe`] will make before
/// giving up on a stale-epoch race. Two attempts is enough: the
/// first races with `on_resize`, the second has fresh dimensions
/// (and a microsecond-scale construct window before the next
/// possible on_resize). A pathological case where every attempt
/// races would mean resize traffic at sub-millisecond cadence,
/// which is itself a bug worth surfacing.
pub(crate) const MAX_SUBSCRIBE_ATTEMPTS: usize = 2;

/// Outcome of one [`EncoderPool::subscribe_once`] attempt. The outer
/// `subscribe` loop continues only on [`Self::StaleEpochRetry`].
pub(crate) enum SubscribeAttemptOutcome {
    /// Attempt produced a final result — either a successful
    /// subscription or a definitive NoCompatibleCodec that doesn't
    /// stem from a resize race. Outer subscribe returns this verbatim.
    Done(Result<(Vec<EncoderSubscription>, PoolLease), SubscribeError>),
    /// Attempt detected `source_gen` advanced during off-lock
    /// construction AND the would-be result is empty (only on-demand
    /// codecs requested, all stale). Outer subscribe retries with
    /// fresh dimensions.
    StaleEpochRetry,
}

/// Conventional simulcast RID for the highest-quality layer (full
/// resolution). Matches LiveKit / mediasoup convention.
pub const RID_FULL: &str = "f";

/// Conventional simulcast RID for the medium layer (typically half
/// resolution).
pub const RID_HALF: &str = "h";

/// Conventional simulcast RID for the lowest layer (typically quarter
/// resolution).
pub const RID_QUARTER: &str = "q";

/// RID for the on-demand *federated* H.264 layer (quarter resolution +
/// capped bitrate — see [`LayerSpec::single_federated`]). Distinct from
/// [`RID_FULL`] so a federated H.264 encoder keys a different pool slot
/// (`EncoderId { H264, RID_FEDERATED }`) than a local full-resolution
/// H.264 encoder (`EncoderId { H264, RID_FULL }`) — the two never share
/// a refcounted slot, so a federated viewer can never be handed a
/// full-resolution H.264 encoder a local viewer spawned, or vice versa.
///
/// Single-encoding only: the federated path narrows to one RID, the
/// track is built with a single encoding, and rtc 0.9 emits NO
/// `a=rid` / `a=simulcast` lines for a single encoding (see
/// `add_sender_sdp`'s `is_simulcast = encodings.len() > 1` gate). So this
/// non-canonical RID is purely an internal routing/slot key and never
/// reaches the wire — no `from_str_loose` round-trip is required.
pub const RID_FEDERATED: &str = "fed";

/// PLI/FIR coalesce window. Within this duration, multiple keyframe
/// requests for the same `(codec, rid)` collapse into one request to
/// the encoder. 50 ms is short enough that perceived recovery latency
/// is unchanged for any single viewer, and long enough to absorb the
/// spike when N viewers hit the wire at once.
pub const KEYFRAME_COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// Bounded capacity for each encoder's outbound `EncodedFrame`
/// broadcast. Lossy by design — slow subscribers drop frames at this
/// queue rather than backpressuring the encoder, which would degrade
/// every other viewer.
pub const ENCODER_FRAME_BROADCAST_CAPACITY: usize = 16;

/// Bounded capacity for the pool's inbound I420 broadcast. Sized to match
/// the existing bridge → encoder sync_channel that this replaces (4
/// frames at 30fps ≈ 130ms of buffering, enough to absorb a brief
/// scheduler hiccup without wedging the bridge). Lossy: a slow encoder
/// thread sees `RecvError::Lagged` and skips ahead rather than
/// backpressuring the bridge.
pub const I420_BROADCAST_CAPACITY: usize = 4;

// ---------------------------------------------------------------------------
// Codec identity
// ---------------------------------------------------------------------------

/// The always-on / baseline codec for this platform — the codec the pool
/// guarantees is producing frames the instant any peer subscribes, and the one
/// `EncoderPool::new` / `on_resize` spawn for every always-on layer.
///
/// VP8 everywhere it's available (universal browser support, no licensing
/// complications). On Windows the VP8/libvpx backend is gated off (Tier-0
/// deferral — see `vp8.rs` / `Cargo.toml`), so the baseline is **H.264** via
/// the Media Foundation software encoder ([`crate::encode::h264_windows`]); H.264 is
/// universally decodable by WebRTC browsers too, so it is a sound baseline.
/// This keeps the Windows streaming path supplied with a working always-on
/// encoder while leaving the macOS/Linux VP8 baseline unchanged.
#[cfg(not(target_os = "windows"))]
pub const BASELINE_CODEC: CodecKind = CodecKind::Vp8;
/// See the non-Windows definition above; Windows has no VP8 backend so the
/// baseline is H.264.
#[cfg(target_os = "windows")]
pub const BASELINE_CODEC: CodecKind = CodecKind::H264;

/// Codec kinds the pool can produce. Closed enum because adding a codec is a
/// coordinated change (new encoder backend + RTC codec registration + browser
/// compat survey).
///
/// Distinct from [`crate::encode::CodecChoice`] (which is the existing
/// "what did we pick for this session" enum). Pool-level identity
/// includes codecs we plan to support but haven't wired backends for
/// yet (Av1, Vp9), so these are kept separate to avoid leaking
/// pool-internal vocabulary into the older single-encoder API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecKind {
    Vp8,
    H264,
    Vp9,
    Av1,
}

impl CodecKind {
    /// Wire / SDP MIME type for this codec, e.g. `"video/VP8"`.
    pub fn mime(&self) -> &'static str {
        match self {
            Self::Vp8 => crate::encode::MIME_TYPE_VP8,
            Self::H264 => crate::encode::MIME_TYPE_H264,
            Self::Vp9 => "video/VP9",
            Self::Av1 => "video/AV1",
        }
    }

    /// Inverse of [`Self::mime`]. Returns `None` for unrecognised wire
    /// strings — callers that need to fail loud on unknown codecs must
    /// handle the `None` case explicitly rather than matching on the
    /// MIME string themselves (keeps the codec vocabulary in one place).
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            m if m == crate::encode::MIME_TYPE_VP8 => Some(Self::Vp8),
            m if m == crate::encode::MIME_TYPE_H264 => Some(Self::H264),
            "video/VP9" => Some(Self::Vp9),
            "video/AV1" => Some(Self::Av1),
            _ => None,
        }
    }

    /// Short string for logs. Distinct from `mime()` because logs read
    /// better with `vp8` / `h264` than `video/VP8` / `video/H264`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vp8 => "vp8",
            Self::H264 => "h264",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
        }
    }

    /// Whether this codec is in the always-on bank by default. The
    /// [`BASELINE_CODEC`] is always-on (VP8 on macOS/Linux for universal
    /// compatibility; H.264 on Windows where VP8 is unavailable); everything
    /// else spins up on demand.
    pub fn is_always_on_default(&self) -> bool {
        *self == BASELINE_CODEC
    }
}

impl fmt::Display for CodecKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Simulcast layer ID, RFC 8853. Newtype around String so we don't
/// confuse it with arbitrary identifiers. Maps to RTP RID at the
/// forwarding layer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimulcastRid(pub String);

impl SimulcastRid {
    pub fn new(rid: impl Into<String>) -> Self {
        Self(rid.into())
    }

    /// `RID_FULL` — convention for the top simulcast layer.
    pub fn full() -> Self {
        Self(RID_FULL.to_string())
    }

    /// `RID_HALF` — convention for the middle simulcast layer.
    pub fn half() -> Self {
        Self(RID_HALF.to_string())
    }

    /// `RID_QUARTER` — convention for the bottom simulcast layer.
    pub fn quarter() -> Self {
        Self(RID_QUARTER.to_string())
    }

    /// `RID_FEDERATED` — the on-demand federated H.264 layer (quarter
    /// resolution + capped bitrate). Keeps the federated H.264 slot
    /// distinct from a local full-resolution H.264 slot. See
    /// [`RID_FEDERATED`].
    pub fn federated() -> Self {
        Self(RID_FEDERATED.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a token as a known simulcast RID. Recognizes the three
    /// canonical names ([`RID_FULL`] / [`RID_HALF`] / [`RID_QUARTER`])
    /// and returns `Some` for them; returns `None` for any other token.
    ///
    /// Forward-compat: callers parsing offerer-supplied RID lists
    /// (notably `parse_offer_simulcast_recv_rids` in
    /// [`crate::webrtc`]) `filter_map` through this so unknown
    /// future RID names silently drop while known ones pass through.
    /// Strict variants that need to surface unknowns can match on the
    /// raw `&str` directly before calling this.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s {
            RID_FULL => Some(Self::full()),
            RID_HALF => Some(Self::half()),
            RID_QUARTER => Some(Self::quarter()),
            _ => None,
        }
    }
}

impl fmt::Display for SimulcastRid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Encoder spec & layer
// ---------------------------------------------------------------------------

/// Resolution + bitrate spec for one simulcast layer. A non-simulcast
/// codec is represented as a single layer (typically [`SimulcastRid::full`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerSpec {
    pub rid: SimulcastRid,
    pub width: u32,
    pub height: u32,
    pub target_bitrate_kbps: u32,
    pub framerate: u32,
}

/// Minimum encoder dimension. Layers smaller than this are dropped
/// from the simulcast set rather than included — VP8 requires
/// even dims at minimum 16x16 (libvpx; smaller sizes work in some
/// builds but aren't portable), and at quarter-of-source the
/// quarter layer hits this floor for source widths/heights below
/// ~64 px (rare but possible during a live resize transient).
///
/// Set to 16 to match the lowest libvpx contract dim. A layer
/// dropped here means the simulcast set returns 1 or 2 layers
/// instead of 3 — peers that subscribed to the dropped RID see
/// a normal `RecvError::Closed` and resubscribe via the
/// pool-frame-intake reconnect path.
pub const MIN_LAYER_DIM: u32 = 16;

/// Target bitrate (kbps) for the on-demand federated H.264 layer
/// ([`LayerSpec::single_federated`]). Roughly double the VP8 quarter
/// floor's 125 kbps — H.264 carries more per-frame overhead (SPS/PPS
/// repeated before every periodic IDR, slice headers from
/// `slice-max-size=1200`) than VP8 at the same resolution, so a slightly
/// higher cap holds equivalent quality at quarter resolution while
/// keeping each IDR small enough to reassemble under relay loss. Well
/// below the 2500 kbps full-resolution [`LayerSpec::single`] cap that
/// produces the un-reassemblable seed IDR this layer exists to avoid.
pub const FEDERATED_H264_BITRATE_KBPS: u32 = 250;

/// Normalize a `(width, height)` pair to the constraints both
/// VP8 encoder construction and [`crate::encode::downscale_i420`] require:
///
///   1. Round to nearest even (`& !1`).
///   2. Reject if either dim falls below [`MIN_LAYER_DIM`].
///
/// Returns `Some((even_w, even_h))` for valid layer dims,
/// `None` for dims that should drop the layer entirely.
///
/// **Used by [`LayerSpec::vp8_simulcast`]** to filter the layers
/// it returns. Since 4a-fix-#3 the resize path doesn't rescale
/// old layers — it re-invokes the pool's stored
/// [`LayerFactory`] with the new source dims, so the same
/// `vp8_simulcast` call (and its `normalize_layer_dims` filter)
/// runs on resize too. That guarantees a 64×64 → 60×48 resize
/// drops the quarter layer the same way a fresh
/// `vp8_simulcast(60, 48, ...)` would, AND the next 60×48 →
/// 64×64 resize restores the dropped quarter (because the
/// factory regenerates from the canonical layout, not from the
/// previous epoch's surviving handles).
pub(crate) fn normalize_layer_dims(w: u32, h: u32) -> Option<(u32, u32)> {
    let w = w & !1;
    let h = h & !1;
    if w < MIN_LAYER_DIM || h < MIN_LAYER_DIM {
        None
    } else {
        Some((w, h))
    }
}

impl LayerSpec {
    /// Reference VP8 simulcast layout — up to three layers at full /
    /// half / quarter resolution from a source resolution. Bitrates
    /// roughly follow LiveKit's defaults (2.5 Mbps / 400 kbps /
    /// 125 kbps for 720p source).
    ///
    /// Each layer's dimensions are rounded down to the nearest even
    /// number — VP8 requires even dims (per [`vp8::Vp8Encoder::new`]
    /// and the same constraint enforced by [`crate::encode::downscale_i420`]),
    /// and naked integer division produces odd dims for common
    /// display sizes. 1366×768 is the canonical case: full 1366×768
    /// (already even), half 683×384 (683 odd → encoder reject), quarter
    /// 341×192 (341 odd → encoder reject). With the round-down those
    /// become 682×384 and 340×192 — losing one column on each odd
    /// layer, which is invisible at the encode-then-display stage.
    ///
    /// Layers below [`MIN_LAYER_DIM`] are dropped from the returned
    /// vec — at small source dims (e.g. 60×48) the quarter would
    /// be 14×10, below libvpx's portable minimum. Returning 1 or 2
    /// layers instead of 3 is the safe degradation; the caller still
    /// gets at least the full layer for any source ≥ MIN_LAYER_DIM.
    /// If the source itself is below MIN_LAYER_DIM in either dim,
    /// returns an empty vec — at that point the display pipeline
    /// can't encode at all and the caller should fail loud at pool
    /// construction.
    pub fn vp8_simulcast(source_w: u32, source_h: u32, framerate: u32) -> Vec<LayerSpec> {
        let mut out = Vec::with_capacity(3);
        for (rid, divisor, target_bitrate_kbps) in [
            (SimulcastRid::full(), 1, 2500),
            (SimulcastRid::half(), 2, 400),
            (SimulcastRid::quarter(), 4, 125),
        ] {
            let Some((w, h)) = normalize_layer_dims(source_w / divisor, source_h / divisor) else {
                continue;
            };
            out.push(LayerSpec {
                rid,
                width: w,
                height: h,
                target_bitrate_kbps,
                framerate,
            });
        }
        out
    }

    /// Single-layer spec for codecs we don't simulcast (H.264 today —
    /// libx264 + ffmpeg's broken-pipe model makes parallel encoders
    /// fragile). Single full-resolution stream, no RID-based switching.
    pub fn single(codec: CodecKind, width: u32, height: u32, framerate: u32) -> LayerSpec {
        let bitrate = match codec {
            CodecKind::H264 | CodecKind::Vp9 | CodecKind::Av1 => 2500,
            CodecKind::Vp8 => 2500,
        };
        LayerSpec {
            rid: SimulcastRid::full(),
            width,
            height,
            target_bitrate_kbps: bitrate,
            framerate,
        }
    }

    /// Single-layer spec for the on-demand **federated** H.264 layer:
    /// quarter resolution + a capped bitrate, mirroring the VP8 federated
    /// floor that already works under loss.
    ///
    /// **Why this exists.** A federated viewer reaches the daemon over a
    /// `browser → TURN relay → remote peer` path where moderate sustained
    /// packet loss (~5-20 %) is the operational baseline. A full-resolution
    /// 2500 kbps H.264 stream ([`Self::single`]) produces a seed IDR of
    /// hundreds of RTP packets — at ~17 % loss the probability of
    /// reassembling every packet is effectively zero, so the stream never
    /// bootstraps (`framesDecoded == 0`). The working VP8 federated floor
    /// is quarter-resolution / 125 kbps (~17-packet IDR, ~24 % intact
    /// arrival); this gives federated H.264 the same shape: quarter the
    /// source dimensions (`source / 4`, rounded to the same even /
    /// [`MIN_LAYER_DIM`] constraints as [`Self::vp8_simulcast`]'s quarter
    /// layer via [`normalize_layer_dims`]) and a bitrate capped at
    /// [`FEDERATED_H264_BITRATE_KBPS`]. Combined with the finite-GOP
    /// periodic IDR (`encode/h264_linux.rs`) and same-SSRC NACK
    /// retransmission (`display/webrtc.rs`), the small IDR both survives
    /// the relay and is recoverable.
    ///
    /// **Distinct RID.** The layer carries [`SimulcastRid::federated`], not
    /// [`SimulcastRid::full`], so its pool slot
    /// (`EncoderId { H264, RID_FEDERATED }`) is never shared with a local
    /// full-resolution H.264 slot — see [`RID_FEDERATED`].
    ///
    /// **Single-RID, not simulcast.** H.264 stays single-encoding (the
    /// pool's `try_spawn_encoder_thread` / libx264 broken-pipe constraint
    /// makes parallel H.264 encoders fragile); this is one capped layer,
    /// not an added simulcast tier. H.264 is the only codec that takes
    /// this federated path (VP8 federation uses its own quarter tier from
    /// [`Self::vp8_simulcast`]), so this constructor is codec-implicit —
    /// no `codec` argument.
    ///
    /// Degrades safely: if `source / 4` falls below [`MIN_LAYER_DIM`]
    /// (tiny source / mid-resize transient), falls back to the largest
    /// encodable dims at or below source rather than dropping the layer —
    /// a federated peer that negotiated H.264 must get *a* stream, never
    /// an empty layer set.
    pub fn single_federated(source_w: u32, source_h: u32, framerate: u32) -> LayerSpec {
        // Quarter resolution, same even / MIN_LAYER_DIM normalization as
        // the VP8 quarter floor. Fall back to normalized source dims if
        // the quarter is too small to encode, and finally to the raw
        // source so we always return an encodable layer.
        let (width, height) = normalize_layer_dims(source_w / 4, source_h / 4)
            .or_else(|| normalize_layer_dims(source_w, source_h))
            .unwrap_or((source_w, source_h));
        LayerSpec {
            rid: SimulcastRid::federated(),
            width,
            height,
            target_bitrate_kbps: FEDERATED_H264_BITRATE_KBPS,
            framerate,
        }
    }
}

/// Identity of one encoder instance the pool can spawn. The pool keys
/// its slots on `(codec, rid)` so simulcast layers of the same codec
/// are independently spawnable / addressable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EncoderId {
    pub codec: CodecKind,
    pub rid: SimulcastRid,
}

impl EncoderId {
    pub fn new(codec: CodecKind, rid: SimulcastRid) -> Self {
        Self { codec, rid }
    }
}

impl fmt::Display for EncoderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.codec, self.rid)
    }
}

// ---------------------------------------------------------------------------
// Encoder handle (one running encoder)
// ---------------------------------------------------------------------------

/// Handle to one running encoder inside the pool.
///
/// Holding a clone of `frames` does **not** keep the encoder alive — the
/// encoder thread holds its own clone of the underlying state and exits
/// only when (a) the pool's I420 input broadcast closes (last sender
/// drops, typically at pool drop), or (b) the pool fires its
/// [`shutdown`](Self::shutdown) cancellation token (per-encoder, used
/// by on-demand teardown so other encoders keep running). Both paths
/// are cooperative; the thread checks `shutdown.is_cancelled()` between
/// frames so a cancellation lands within at most one `blocking_recv`
/// wakeup (~one frame interval).
#[derive(Clone)]
pub struct EncoderHandle {
    pub id: EncoderId,
    pub layer: LayerSpec,
    /// Broadcast of encoded frames produced by this encoder. Each
    /// peer's forwarder calls `frames.subscribe()` once when it joins.
    /// The broadcast is lossy (slow subscribers see `Lagged` and skip)
    /// — intentional, because backpressuring the encoder degrades
    /// every other peer using this layer.
    pub frames: broadcast::Sender<Arc<EncodedFrame>>,
    /// Per-encoder force-keyframe flag. [`EncoderPool::request_keyframe`]
    /// stores `true` here; the encoder thread `swap`s it back to false
    /// when consumed on the next frame and passes the bool to
    /// [`crate::encode::Encoder::encode`]. AtomicBool keeps the
    /// signaling lock-free between the async pool API and the std::thread
    /// encoder loop.
    pub force_keyframe: Arc<AtomicBool>,
    /// **Phase 4d.0**: per-encoder pause flag. When set, the encoder
    /// thread drains its `i420_rx` broadcast subscription as usual
    /// (so the channel doesn't lag) but skips the downscale + encode
    /// + broadcast step entirely. Used by the layer-selection policy
    ///   (4d.2) to throttle layers no peer is consuming under current
    ///   bandwidth conditions, without tearing down the encoder slot
    ///   itself — resume is just a flag flip and the next captured frame
    ///   gets encoded.
    ///
    /// Behavior preserved across pause:
    /// - `force_keyframe`: NOT consumed while paused, so a keyframe
    ///   request that arrives during pause is honored on the first
    ///   frame after resume — exactly the right thing for "viewer just
    ///   subscribed to this layer, give them a fresh keyframe."
    /// - Watchdog: not advanced while paused (otherwise a long pause
    ///   would trip the silent-output threshold and trigger an
    ///   unnecessary H.264 fallback on resume).
    /// - Metrics: encoder doesn't count `encode_frames` /
    ///   `encode_freshness_us_sum` while paused (no work was done).
    ///   `encode_drops` from broadcast lag still counts (lag reflects
    ///   subscriber slowness, not pause state).
    pub paused: Arc<AtomicBool>,
    /// Per-encoder shutdown signal. Cancelled by [`EncoderPool`] on
    /// release/drop. Encoder thread checks between frames and breaks
    /// cleanly on next iter. Distinct from "i420 broadcast closed" so
    /// individual on-demand encoders can be torn down without dropping
    /// the shared input channel.
    pub shutdown: CancellationToken,
}

impl EncoderHandle {
    /// Subscribe a new consumer (peer forwarder) to this encoder's
    /// frame stream. Subscriber starts receiving from the next emitted
    /// frame; previously emitted frames are not replayed.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<EncodedFrame>> {
        self.frames.subscribe()
    }
}

// ---------------------------------------------------------------------------
// I420 input frame
// ---------------------------------------------------------------------------

/// One I420-converted capture frame, fed into the pool's input broadcast
/// by the bridge. `data` is `Arc`-wrapped so multiple encoder threads
/// each get a cheap clone (the bytes themselves aren't copied per
/// subscriber).
#[derive(Clone, Debug)]
pub struct I420Frame {
    pub data: Arc<Vec<u8>>,
    pub arrived: Instant,
    pub visual_marker_value: Option<u32>,
}

// ---------------------------------------------------------------------------
// Subscription returned to peer forwarders
// ---------------------------------------------------------------------------

/// Subscription package handed back to one peer's forwarder by
/// [`EncoderPool::subscribe`]. Carries everything the forwarder needs
/// to consume one encoder's output:
///
/// - the [`EncoderId`] (so the forwarder knows which codec/layer this is)
/// - the [`LayerSpec`] (resolution / bitrate / framerate, useful for
///   layer-selection policy)
/// - the live broadcast receiver
///
/// A peer that supports multiple codecs receives multiple
/// `EncoderSubscription`s — one per codec the peer can decode. The
/// forwarder picks which to actually consume based on the peer's
/// negotiated codec set; unconsumed subscriptions are dropped at peer
/// teardown which decrements the encoder's refcount via
/// [`EncoderPool::release`].
pub struct EncoderSubscription {
    pub id: EncoderId,
    pub layer: LayerSpec,
    pub frames: broadcast::Receiver<Arc<EncodedFrame>>,
}

// ---------------------------------------------------------------------------
// Peer codec preferences (input to subscribe)
// ---------------------------------------------------------------------------

/// What a peer can decode. The forwarder builds this from the peer's
/// SDP offer using [`crate::encode::parse_offered_codecs`] (existing function).
///
/// Order matters only as a preference hint for the forwarder when
/// multiple codecs would work; the pool subscribes the peer to **all**
/// codecs it supports and lets the forwarder choose at frame time
/// (cheap; subscribing is just a `broadcast::Receiver` per codec).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerCodecPreferences {
    pub supported: Vec<CodecKind>,
    /// True when this peer is a **federated** viewer (its offer has no
    /// `a=simulcast:recv` directive — the signature of a
    /// `PeerDisplayConnection` reaching us over the TURN relay). The pool
    /// uses this to spawn the on-demand H.264 layer at the loss-resilient
    /// quarter-resolution / capped-bitrate shape
    /// ([`LayerSpec::single_federated`]) instead of the full-resolution
    /// [`LayerSpec::single`]. Carried through every resubscribe so the
    /// federated peer keeps getting a federated-shaped encoder across
    /// resize / Closed-recovery epochs.
    pub federated: bool,
}

impl PeerCodecPreferences {
    /// Local (non-federated) peer preferences — the on-demand H.264 layer
    /// is built at full resolution ([`LayerSpec::single`]).
    pub fn new(supported: Vec<CodecKind>) -> Self {
        Self {
            supported,
            federated: false,
        }
    }

    /// Federated peer preferences — the on-demand H.264 layer is built at
    /// the loss-resilient quarter-resolution / capped-bitrate shape
    /// ([`LayerSpec::single_federated`]). See the `federated` field doc.
    pub fn new_federated(supported: Vec<CodecKind>) -> Self {
        Self {
            supported,
            federated: true,
        }
    }

    pub fn supports(&self, codec: CodecKind) -> bool {
        self.supported.contains(&codec)
    }

    pub fn is_empty(&self) -> bool {
        self.supported.is_empty()
    }
}

/// Codec-agnostic, allocation-free pool logic tests that run on **every**
/// platform (no `EncoderPool::new`, so no encoder backend is constructed).
/// Kept separate from [`tests`] so the Windows target — where the heavier
/// pool-construction tests are gated off (see that module's note) — still
/// verifies codec identity, the platform baseline, and the pure helper math.
#[cfg(test)]
mod logic_tests {
    use super::*;

    #[test]
    fn codec_kind_mime_round_trip() {
        assert_eq!(CodecKind::Vp8.mime(), crate::encode::MIME_TYPE_VP8);
        assert_eq!(CodecKind::H264.mime(), crate::encode::MIME_TYPE_H264);
        assert_eq!(CodecKind::Vp9.mime(), "video/VP9");
        assert_eq!(CodecKind::Av1.mime(), "video/AV1");
    }

    #[test]
    fn codec_kind_from_mime_round_trips_every_kind() {
        for k in [
            CodecKind::Vp8,
            CodecKind::H264,
            CodecKind::Vp9,
            CodecKind::Av1,
        ] {
            assert_eq!(CodecKind::from_mime(k.mime()), Some(k));
        }
        assert_eq!(CodecKind::from_mime(""), None);
    }

    #[test]
    fn codec_kind_only_baseline_is_always_on_default() {
        // Exactly the platform BASELINE_CODEC is always-on (VP8 on
        // macOS/Linux, H.264 on Windows where VP8 is gated off); every other
        // codec spins up on demand. This is the load-bearing cross-platform
        // assertion for the Windows H.264-baseline wiring.
        for k in [
            CodecKind::Vp8,
            CodecKind::H264,
            CodecKind::Vp9,
            CodecKind::Av1,
        ] {
            assert_eq!(
                k.is_always_on_default(),
                k == BASELINE_CODEC,
                "{k:?} always-on should equal (k == BASELINE_CODEC)"
            );
        }
        assert!(BASELINE_CODEC.is_always_on_default());
    }

    #[test]
    fn baseline_codec_is_h264_on_windows_vp8_elsewhere() {
        #[cfg(target_os = "windows")]
        assert_eq!(BASELINE_CODEC, CodecKind::H264);
        #[cfg(not(target_os = "windows"))]
        assert_eq!(BASELINE_CODEC, CodecKind::Vp8);
    }

    #[test]
    fn simulcast_rid_constants_match_constructors() {
        assert_eq!(SimulcastRid::full().as_str(), RID_FULL);
        assert_eq!(SimulcastRid::half().as_str(), RID_HALF);
        assert_eq!(SimulcastRid::quarter().as_str(), RID_QUARTER);
    }

    #[test]
    fn vp8_simulcast_layout_is_three_descending_layers() {
        let layers = LayerSpec::vp8_simulcast(1920, 1080, 30);
        assert_eq!(layers.len(), 3);
        // Order: full, half, quarter, with exact even-rounded dims.
        assert_eq!(layers[0].rid, SimulcastRid::full());
        assert_eq!((layers[0].width, layers[0].height), (1920, 1080));
        assert_eq!(layers[1].rid, SimulcastRid::half());
        assert_eq!((layers[1].width, layers[1].height), (960, 540));
        assert_eq!(layers[2].rid, SimulcastRid::quarter());
        assert_eq!((layers[2].width, layers[2].height), (480, 270));
        // Bitrate strictly descending — smaller layers are cheap.
        assert!(layers[0].target_bitrate_kbps > layers[1].target_bitrate_kbps);
        assert!(layers[1].target_bitrate_kbps > layers[2].target_bitrate_kbps);
    }

    #[test]
    fn single_full_layer_is_source_res_and_full_rid() {
        // The local (non-federated) on-demand H.264 layer: source
        // resolution, the full 2.5 Mbps cap, on the `full` RID.
        let layer = LayerSpec::single(CodecKind::H264, 1920, 1080, 30);
        assert_eq!((layer.width, layer.height), (1920, 1080));
        assert_eq!(layer.target_bitrate_kbps, 2500);
        assert_eq!(layer.rid, SimulcastRid::full());
    }

    #[test]
    fn single_federated_layer_is_quarter_res_capped_bitrate_federated_rid() {
        // The federated on-demand H.264 layer mirrors the VP8 quarter
        // floor: quarter the source dims (even-rounded) + the capped
        // bitrate, on the distinct `fed` RID.
        let layer = LayerSpec::single_federated(1920, 1080, 30);
        assert_eq!(
            (layer.width, layer.height),
            (480, 270),
            "federated H.264 must be quarter of 1920x1080"
        );
        assert_eq!(layer.target_bitrate_kbps, FEDERATED_H264_BITRATE_KBPS);
        assert!(
            layer.target_bitrate_kbps < 2500,
            "federated bitrate must be well below the full-res 2500 kbps cap"
        );
        assert_eq!(
            layer.rid,
            SimulcastRid::federated(),
            "federated layer must use RID_FEDERATED, not RID_FULL, so its \
             pool slot never aliases a local full-res H.264 slot"
        );
        // The federated RID is distinct from the full RID — the property
        // that keeps the two EncoderId slots separate.
        assert_ne!(SimulcastRid::federated(), SimulcastRid::full());
        assert_eq!(SimulcastRid::federated().as_str(), RID_FEDERATED);
    }

    #[test]
    fn single_federated_odd_source_dims_round_to_even() {
        // 1366x768 quarter = 341x192; 341 is odd → must round down to 340
        // (the same even-dim constraint vp8_simulcast enforces, so the
        // downscale + encoder accept the dims).
        let layer = LayerSpec::single_federated(1366, 768, 30);
        assert_eq!((layer.width, layer.height), (340, 192));
        assert_eq!(layer.width % 2, 0, "width must be even");
        assert_eq!(layer.height % 2, 0, "height must be even");
    }

    #[test]
    fn single_federated_tiny_source_falls_back_to_encodable_dims() {
        // A source whose quarter would fall below MIN_LAYER_DIM must still
        // yield an encodable layer (a federated H.264 peer needs *a*
        // stream), not panic or produce sub-minimum dims.
        let layer = LayerSpec::single_federated(40, 40, 30);
        // Quarter (10x10) is below MIN_LAYER_DIM=16 → falls back to the
        // normalized source dims (40x40, both even and >= 16).
        assert!(layer.width >= MIN_LAYER_DIM && layer.height >= MIN_LAYER_DIM);
        assert_eq!((layer.width, layer.height), (40, 40));
    }

    #[test]
    fn peer_codec_preferences_federated_flag() {
        // `new` is non-federated; `new_federated` sets the flag. Both keep
        // the supported-codec list intact.
        let local = PeerCodecPreferences::new(vec![CodecKind::H264, CodecKind::Vp8]);
        assert!(!local.federated);
        assert!(local.supports(CodecKind::H264));

        let fed = PeerCodecPreferences::new_federated(vec![CodecKind::H264, CodecKind::Vp8]);
        assert!(fed.federated);
        assert!(fed.supports(CodecKind::H264));
        assert_eq!(fed.supported, local.supported);
    }
}
