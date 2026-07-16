//! Encoder worker threads: spawn (with the always-on construction
//! contract), the silent-encoder watchdog, the H.264 software-fallback
//! probe, and the per-layer encode loop.

use super::*;

// ---------------------------------------------------------------------------
// Encoder thread spawn
// ---------------------------------------------------------------------------

/// Spawn one encoder thread for the given layer, returning its
/// [`EncoderHandle`]. The thread:
///
/// 1. Constructs the codec's encoder backend via
///    [`crate::encode::select_codec_for_mime`] — **on the encoder thread
///    itself** (see below).
/// 2. Subscribes to the pool's I420 broadcast.
/// 3. In a `blocking_recv` loop: pulls the next I420 frame, swaps the
///    `force_keyframe` flag, calls `encoder.encode(...)`, and
///    broadcasts each produced packet (wrapped in
///    `Arc<EncodedFrame>`) to the per-encoder frames channel.
/// 4. Exits when `shutdown` is cancelled OR the I420 broadcast closes
///    (sender dropped at pool drop).
///
/// **Construct-on-the-driver-thread.** The encoder is built inside the
/// spawned thread and then *used and dropped* on that same thread for
/// its entire life. This is load-bearing for the Windows Media
/// Foundation backend ([`crate::encode::h264_windows`]), whose `new()` calls
/// `CoInitializeEx` + `MFStartup` and whose `Drop` calls `MFShutdown` +
/// `CoUninitialize` — COM init/teardown is **per-thread**, so the same
/// thread that initializes COM must be the one that touches and releases
/// the COM objects. The other backends (VP8/libvpx, VideoToolbox,
/// ffmpeg) have no per-thread state and are unaffected; their
/// construction code is unchanged — only the thread on which
/// `select_codec_for_mime` runs has moved.
///
/// **No ghost handles.** Construction can still fail (a host without a
/// usable H.264 MFT, libvpx ABI mismatch, …) and the contract is that a
/// failed construct must *not* publish an [`EncoderHandle`] — callers
/// (the on-demand subscribe path) rely on the error to exclude the codec
/// from the subscription set rather than hand back a subscription to an
/// encoder that will never emit a frame. To keep that contract while
/// moving construction onto the thread, the thread reports the
/// construction outcome back over a one-shot startup channel and this
/// function blocks until it arrives: on `Err` we return the error (the
/// thread has already exited, nothing was published); on `Ok` we return
/// the `EncoderHandle`. This replaces the original design where the
/// caller constructed synchronously and moved the boxed encoder into the
/// thread — which worked for libvpx/VideoToolbox/ffmpeg but constructed
/// the Windows MF encoder on a Tokio worker only to use and drop it on
/// the encoder thread, a latent cross-thread-COM hazard.
pub(crate) fn try_spawn_encoder_thread(
    id: EncoderId,
    layer: LayerSpec,
    source_w: u32,
    source_h: u32,
    i420_tx: &broadcast::Sender<I420Frame>,
    duration_ms: u64,
    counters: &Arc<crate::DisplayMetricsCounters>,
) -> Result<EncoderHandle, String> {
    // The construction parameters captured for the driver thread. The
    // thread runs `select_codec_for_mime` so any per-thread codec state
    // (Windows COM/MF) is initialized on the thread that will use it.
    let mime = id.codec.mime();
    let (cw, ch, cbr) = (layer.width, layer.height, layer.target_bitrate_kbps);
    let construct =
        move || crate::encode::select_codec_for_mime(mime, cw, ch, cbr).map(|(enc, _)| enc);
    spawn_encoder_thread_with(
        id,
        layer,
        source_w,
        source_h,
        construct,
        i420_tx,
        duration_ms,
        counters,
    )
}

/// 3c.3b.4f: per-encoder silent-output watchdog.
///
/// Detects encoders that accept input but produce no encoded output
/// (the canonical failure mode is `h264_vaapi` on hosts where
/// virtio-gpu video acceleration is half-broken: `vaInitialize`
/// "succeeds" but no NAL units ever come out of stdout). After
/// [`ENCODER_SILENT_FRAMES_THRESHOLD`] consecutive silent encodes
/// the watchdog asks the caller to attempt a fallback once; after
/// the swap (or after the swap-attempt fails) the watchdog stops
/// firing for the lifetime of this encoder thread.
///
/// Catches `h264_vaapi` silent-failure (vaInitialize succeeds, no
/// NALs ever emitted) — without this the stream would stay black
/// indefinitely on hosts where VAAPI claims to work but doesn't.
pub(crate) struct WatchdogState {
    frames_since_last_output: u64,
    swap_done: bool,
}

/// 30 frames ≈ 1s at 30fps, well above the normal 1–2 frame
/// pipeline depth for any healthy encoder.
pub(crate) const ENCODER_SILENT_FRAMES_THRESHOLD: u64 = 30;

impl WatchdogState {
    pub(crate) fn new() -> Self {
        Self {
            frames_since_last_output: 0,
            swap_done: false,
        }
    }

    /// Record the result of one `encoder.encode` call. `produced` is
    /// the number of encoded packets emitted (zero on silent-success
    /// AND on encode-error — both are "no output reached the wire").
    /// Returns `true` if the caller should attempt a fallback swap
    /// AFTER this call. The watchdog never returns `true` more than
    /// once per encoder lifetime — a failed fallback swap doesn't
    /// re-arm.
    pub(crate) fn record(&mut self, produced: usize) -> bool {
        if produced > 0 {
            self.frames_since_last_output = 0;
            return false;
        }
        if self.swap_done {
            return false;
        }
        self.frames_since_last_output += 1;
        if self.frames_since_last_output >= ENCODER_SILENT_FRAMES_THRESHOLD {
            self.swap_done = true;
            self.frames_since_last_output = 0;
            return true;
        }
        false
    }
}

/// 3c.3b.4f: pool-path counterpart to `mod.rs::try_h264_fallback`.
///
/// Invariants:
///   - only fires for H.264 (no fallback for VP8 — libvpx doesn't
///     exhibit the silent-failure pattern)
///   - the new encoder constructs cleanly
///
/// **3c.3b.4g:** the previous version ALSO early-returned when
/// `is_vaapi_banned()` was already true, on the assumption "we're
/// already on libx264 so there's nothing to swap to." That
/// assumption holds for an encoder constructed AFTER a ban, but
/// fails for encoders constructed BEFORE a sibling watchdog set
/// the ban: the second watchdog would see the ban, return None,
/// and leave a pre-ban VAAPI encoder stranded on the broken path
/// forever. Multi-H.264-pool-slot and mixed pool/legacy sessions
/// can both reach this state. Fix: drop the early-return, treat
/// `ban_vaapi()` as the idempotent no-op it is, and always attempt
/// construction. At worst an already-libx264 encoder respawns
/// libx264 once (a one-time waste; the watchdog latches and won't
/// fire again on this thread); at best a pre-ban VAAPI encoder
/// gets the libx264 it would otherwise miss.
///
/// Layer-aware: takes the existing [`LayerSpec`] so the replacement
/// encoder is configured for the same width / height / bitrate /
/// framerate as the original. The legacy mod.rs version takes raw
/// `(width, height, bitrate=2000)` because legacy has only one
/// shared encoder; pool has one per layer, each with its own spec.
///
/// On non-Linux targets there's no VA-API path to ban, so this is
/// a no-op — same as the legacy `#[cfg(not(target_os = "linux"))]`
/// arm.
#[cfg(target_os = "linux")]
pub(crate) fn try_h264_fallback_for_layer(
    codec: CodecKind,
    layer: &LayerSpec,
) -> Option<Box<dyn crate::encode::Encoder>> {
    if codec != CodecKind::H264 {
        return None;
    }
    // Ban BOTH GPU encoders before reconstructing. The watchdog fires on a
    // silent-failure (encoder accepts input, emits nothing) but can't tell
    // which backend the dead encoder used — VA-API or NVENC — so banning
    // both forces the rebuilt encoder past the GPU arms straight to
    // software libx264, which is the watchdog's whole intent. Idempotent —
    // see VAAPI_BANNED / NVENC_BANNED in h264_linux.rs (one-way AtomicBools
    // that are never cleared). Calling when already banned is a no-op store.
    crate::encode::h264_linux::ban_nvenc();
    crate::encode::h264_linux::ban_vaapi();
    match crate::encode::select_codec_for_mime(
        codec.mime(),
        layer.width,
        layer.height,
        layer.target_bitrate_kbps,
    ) {
        Ok((enc, _)) => Some(enc),
        Err(e) => {
            eprintln!(
                "[encoder/pool] watchdog: libx264 fallback creation failed: {}",
                e,
            );
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn try_h264_fallback_for_layer(
    _codec: CodecKind,
    _layer: &LayerSpec,
) -> Option<Box<dyn crate::encode::Encoder>> {
    None
}

/// Spawn the encoder driver thread, constructing the [`crate::encode::Encoder`]
/// **inside that thread** via the `construct` closure.
///
/// Blocks until the thread reports its construction outcome over a
/// one-shot startup channel: returns `Err` (and publishes no handle) if
/// `construct` failed, or the running [`EncoderHandle`] once the encoder
/// is built. Constructing on the thread that will use and drop the
/// encoder is what makes the Windows MF backend's per-thread COM
/// init/teardown correct; see [`try_spawn_encoder_thread`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_encoder_thread_with(
    id: EncoderId,
    layer: LayerSpec,
    source_w: u32,
    source_h: u32,
    construct: impl FnOnce() -> Result<Box<dyn crate::encode::Encoder>, String> + Send + 'static,
    i420_tx: &broadcast::Sender<I420Frame>,
    duration_ms: u64,
    counters: &Arc<crate::DisplayMetricsCounters>,
) -> Result<EncoderHandle, String> {
    let (frames_tx, _) = broadcast::channel::<Arc<EncodedFrame>>(ENCODER_FRAME_BROADCAST_CAPACITY);
    let force_keyframe = Arc::new(AtomicBool::new(false));
    // Phase 4d.0: paused defaults to false. Layer-selection policy
    // flips this via [`EncoderPool::pause_layer`] /
    // [`EncoderPool::resume_layer`]: 4d.2 pauses all simulcast
    // layers after a debounce at zero peers (CPU saver during
    // idle); 4d.3 will pause individual over-budget layers when
    // receiver feedback (RTCP RR fraction_lost et al.) shows a
    // peer's link can't sustain them.
    let paused = Arc::new(AtomicBool::new(false));
    let shutdown = CancellationToken::new();

    let mut i420_rx = i420_tx.subscribe();
    let frames_tx_for_thread = frames_tx.clone();
    let force_kf_for_thread = Arc::clone(&force_keyframe);
    let paused_for_thread = Arc::clone(&paused);
    let shutdown_for_thread = shutdown.clone();
    let id_for_log = id.clone();
    // 3c.3b.4f: watchdog needs the codec + layer to attempt a
    // fallback if the encoder goes silent. CodecKind is Copy;
    // LayerSpec clones cheaply (no Arc, no Vec — just primitives
    // + a SimulcastRid String).
    let codec_for_thread = id.codec;
    let layer_for_thread = layer.clone();
    // Phase 4a: per-layer downscale. The bridge pushes I420 at the
    // source dims; this encoder is constructed for `layer.width` ×
    // `layer.height`. When they differ (simulcast: half/quarter
    // layers), each frame must be downscaled before encode or the
    // encoder will reject (size mismatch) or mis-encode.
    let needs_downscale = (layer.width, layer.height) != (source_w, source_h);
    let downscale_src_w = source_w;
    let downscale_src_h = source_h;
    let downscale_dst_w = layer.width;
    let downscale_dst_h = layer.height;
    // 3c.3b.4h: per-encoder metrics. Bumped per encoded packet
    // (encode_frames + encode_freshness_us_sum) and on broadcast lag
    // (encode_drops). Counter is shared with DisplaySession via Arc.
    let counters_for_thread = Arc::clone(counters);

    // One-shot startup channel: the thread constructs the encoder and
    // reports `Ok(())` / `Err(reason)` back here before entering its
    // loop. Sized 1 — exactly one message is ever sent. This lets us
    // construct on the encoder thread (correct for Windows per-thread
    // COM/MF) yet still propagate a construction failure to the caller
    // synchronously, so no handle is published for an encoder that
    // could not be built.
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    std::thread::spawn(move || {
        // Construct on THIS thread so any per-thread codec state
        // (Windows COM apartment + Media Foundation) is initialized,
        // used, and torn down all on one thread. On failure, report the
        // error and exit without ever touching the i420 broadcast.
        let mut encoder = match construct() {
            Ok(enc) => {
                // Report success first; if the receiver is already gone
                // (caller dropped), there's nothing to drive, so exit.
                if startup_tx.send(Ok(())).is_err() {
                    return;
                }
                enc
            }
            Err(e) => {
                let _ = startup_tx.send(Err(e));
                return;
            }
        };
        // Drop the startup sender now that the outcome is delivered; the
        // encoder's lifetime is owned entirely by this thread from here.
        drop(startup_tx);
        let mut watchdog = WatchdogState::new();
        // Per-thread downscale scratch, reused across frames — the
        // half/quarter layers otherwise allocate a fresh multi-hundred-
        // KB buffer per frame at capture rate.
        let mut scaled_buf: Vec<u8> = Vec::new();
        // Windows black-frame diagnostic: count encode calls so the
        // hop-by-hop avg-byte logging below self-limits to the first few
        // frames (then stays off the hot path).
        #[cfg(target_os = "windows")]
        let mut diag_frame_count: u64 = 0;

        loop {
            if shutdown_for_thread.is_cancelled() {
                break;
            }
            let frame = match i420_rx.blocking_recv() {
                Ok(f) => f,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Slow encoder fell behind by `n` frames; skip
                    // ahead. Codec keyframe machinery will recover
                    // (next force_keyframe or the encoder's natural
                    // GOP cadence). 3c.3b.4h: count the skipped
                    // frames as encode_drops so the metric reflects
                    // backpressure pressure even when the encoder
                    // itself isn't logging individual drops.
                    // 3c.3b.4i: gate on receiver_count > 0 — an
                    // encoder with zero consumers (e.g. always-on
                    // VP8 during a legacy-only session, or unused
                    // always-on VP8 in an H.264-only pool session)
                    // is producing into a void; counting its lag
                    // would inflate `encode_drops` against work no
                    // peer is waiting for.
                    if frames_tx_for_thread.receiver_count() > 0 {
                        counters_for_thread
                            .encode_drops
                            .fetch_add(n, Ordering::Relaxed);
                    }
                    eprintln!(
                        "[encoder/pool] {} lagged by {} frames, skipping ahead",
                        id_for_log, n
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            // Re-check shutdown after waking from blocking_recv.
            // Between the top-of-loop check and this point, another
            // task may have cancelled our shutdown — typically
            // `EncoderPool::on_resize` for an old-generation handle,
            // or `PoolLease::release_impl` for an on-demand slot
            // whose refcount hit zero. Without this second check the
            // thread would run encode on `frame`, which for the
            // on_resize case is already post-resize data at new
            // dimensions that this encoder (configured for old
            // dimensions) would misinterpret — feeding a stale frame
            // to any still-live subscriber before finally exiting on
            // the next top-of-loop check.
            //
            // Dropping the frame and exiting here is cheaper than
            // restructuring the `blocking_recv` into a tokio select
            // on shutdown (which would require the encoder loop to
            // become async), and matches the semantics every other
            // shutdown-aware Rust loop uses: "if shutdown fires, do
            // not produce another unit of output."
            if shutdown_for_thread.is_cancelled() {
                break;
            }

            // Phase 4d.0: pause check. Done AFTER the shutdown
            // re-check (so a paused encoder that's also shutting
            // down exits cleanly) but BEFORE the force_keyframe
            // swap (so a keyframe request that arrives during pause
            // is preserved across pause→resume — the resume's first
            // encode honors it). Watchdog is also skipped while
            // paused: a long pause should not trip the silent-
            // output threshold and trigger an unnecessary H.264
            // fallback when we resume.
            //
            // Frame is consumed (we already `blocking_recv`'d above)
            // and dropped — we don't try to keep it around for
            // resume because i420 frames are pushed at capture rate
            // (typically 30fps), so the next post-resume blocking_recv
            // returns a fresh frame in ≤33ms. Buffering would just
            // surface a stale frame.
            if paused_for_thread.load(Ordering::SeqCst) {
                continue;
            }

            // No peer is currently reading this encoder's output. Keep draining
            // the shared I420 broadcast so the receiver stays current, but skip
            // all expensive per-layer work (downscale + codec encode). The next
            // subscribed frame will still honor any pending force-keyframe flag
            // because we intentionally check demand before swapping it below.
            if frames_tx_for_thread.receiver_count() == 0 {
                continue;
            }

            let force_kf = force_kf_for_thread.swap(false, Ordering::SeqCst);

            // Phase 4a: per-layer downscale. The bridge pushes I420
            // at source dims; for simulcast layers (half/quarter) the
            // encoder is sized differently and would mis-encode or
            // reject without resizing first. The `needs_downscale`
            // check is computed once at thread spawn, so the hot path
            // for the source-dim layer (always-on full + every
            // on-demand on-source-dim layer) pays nothing.
            let mut stamped_buf;
            let i420_for_encode: &[u8] = if needs_downscale {
                crate::encode::downscale_i420_into(
                    &frame.data,
                    downscale_src_w,
                    downscale_src_h,
                    downscale_dst_w,
                    downscale_dst_h,
                    &mut scaled_buf,
                );
                if let Some(value) = frame.visual_marker_value {
                    let y_len = (downscale_dst_w as usize) * (downscale_dst_h as usize);
                    if let Some(y) = scaled_buf.get_mut(0..y_len) {
                        visual_marker::stamp_y_plane(
                            y,
                            downscale_dst_w as usize,
                            downscale_dst_h as usize,
                            value,
                        );
                    }
                }
                &scaled_buf
            } else if let Some(value) = frame.visual_marker_value {
                stamped_buf = frame.data.as_ref().clone();
                let y_len = (downscale_dst_w as usize) * (downscale_dst_h as usize);
                if let Some(y) = stamped_buf.get_mut(0..y_len) {
                    visual_marker::stamp_y_plane(
                        y,
                        downscale_dst_w as usize,
                        downscale_dst_h as usize,
                        value,
                    );
                }
                &stamped_buf
            } else {
                &frame.data
            };

            // Windows black-frame diagnostic (hop B): log the average byte of
            // the exact I420 slice handed to the encoder for the first few
            // frames. This is the buffer the codec actually sees — if the
            // bridge logged a bright I420 (hop A) but this reads ~0, the frame
            // was lost/zeroed in the pool broadcast or the
            // downscale/visual-marker selection above; if both are bright, the
            // black is inside the encoder (hop C, in h264_windows.rs).
            #[cfg(target_os = "windows")]
            {
                if diag_frame_count < 5 {
                    eprintln!(
                        "[encoder/pool] {} encode-input frame #{} i420 avg={} \
                         (len={}, downscale={})",
                        id_for_log,
                        diag_frame_count + 1,
                        crate::encode::sampled_avg_byte(i420_for_encode),
                        i420_for_encode.len(),
                        needs_downscale,
                    );
                }
                diag_frame_count += 1;
            }

            let produced = match encoder.encode(i420_for_encode, duration_ms, force_kf) {
                Ok(packets) => {
                    let n = packets.len();
                    // 3c.3b.4h: latency from capture-arrival to
                    // encoded-packet-emission. Mirrors the legacy
                    // mod.rs::start_encoder_pipeline arithmetic
                    // (one freshness value computed per encode call,
                    // summed in once per packet — multi-packet
                    // outputs accumulate the same value per packet,
                    // matching legacy semantics so average rates
                    // compose cleanly across codecs.
                    // A zero-consumer encoder never reaches this
                    // block; it drains I420 and skips the encode
                    // earlier in the loop.
                    let freshness_us = frame.arrived.elapsed().as_micros() as u64;
                    for pkt in packets {
                        counters_for_thread
                            .encode_frames
                            .fetch_add(1, Ordering::Relaxed);
                        counters_for_thread
                            .encode_freshness_us_sum
                            .fetch_add(freshness_us, Ordering::Relaxed);
                        let ef = Arc::new(pkt.into_encoded_frame());
                        // Lossy broadcast: returns Err only if there
                        // are zero subscribers, which is fine.
                        let _ = frames_tx_for_thread.send(ef);
                    }
                    n
                }
                Err(e) => {
                    eprintln!("[encoder/pool] {} encode error: {}", id_for_log, e);
                    0
                }
            };

            // Silent-output watchdog. After 30 consecutive input
            // frames produced no output, attempt a one-shot fallback
            // swap (Linux H.264 → libx264). Prevents h264_vaapi
            // silent-failure (vaInitialize succeeds, no NALs ever
            // emitted) from black-screening the stream forever.
            if watchdog.record(produced) {
                eprintln!(
                    "[encoder/pool] {} watchdog: {} consecutive input \
                     frames produced no output",
                    id_for_log, ENCODER_SILENT_FRAMES_THRESHOLD,
                );
                if let Some(new_enc) =
                    try_h264_fallback_for_layer(codec_for_thread, &layer_for_thread)
                {
                    eprintln!(
                        "[encoder/pool] {} watchdog: swapped encoder to libx264 fallback",
                        id_for_log,
                    );
                    encoder = new_enc;
                } else {
                    eprintln!(
                        "[encoder/pool] {} watchdog: no fallback available, encoder stays",
                        id_for_log,
                    );
                }
            }
        }
    });

    // Block until the thread reports its construction outcome. A
    // `RecvError` here means the thread panicked or exited before
    // sending — treat that as a construction failure rather than
    // publishing a handle to a dead thread.
    match startup_rx.recv() {
        Ok(Ok(())) => Ok(EncoderHandle {
            id,
            layer,
            frames: frames_tx,
            force_keyframe,
            paused,
            shutdown,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "encoder {id} thread exited before reporting construction outcome"
        )),
    }
}
