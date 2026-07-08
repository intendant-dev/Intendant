//! Pool-mode glue between the shared encoder pool and a peer's driver:
//! codec selection over live subscriptions, preference narrowing, codec
//! partitioning, and the `pool_frame_intake` forwarder task that fans
//! encoded frames into the driver.

use super::*;

// ---------------------------------------------------------------------------
// Pool-mode helpers (3c.3b.2)
// ---------------------------------------------------------------------------

/// Distinct codecs covered by `subscriptions`, deduplicated. Used by
/// tests to pin the actually-served codec set rather than the original
/// peer offer prefs.
///
/// Order is preserved as encountered (CodecKind isn't `Ord`, and
/// dedup avoids counting the same codec twice in a multi-layer
/// simulcast set.
#[cfg(test)]
pub(crate) fn codec_set_from_subscriptions(subscriptions: &[EncoderSubscription]) -> Vec<CodecKind> {
    let mut seen: std::collections::HashSet<CodecKind> = std::collections::HashSet::new();
    let mut codecs: Vec<CodecKind> = Vec::new();
    for sub in subscriptions {
        if seen.insert(sub.id.codec) {
            codecs.push(sub.id.codec);
        }
    }
    codecs
}

pub(crate) fn active_codec_from_subscriptions(
    subscriptions: &[EncoderSubscription],
    prefs: &PeerCodecPreferences,
) -> Option<CodecKind> {
    prefs
        .supported
        .iter()
        .find(|&&codec| subscriptions.iter().any(|s| s.id.codec == codec))
        .copied()
}

/// Build the **negotiated** codec preferences the intake uses for
/// every `pool.subscribe` call (initial AND every resubscribe).
///
/// Filters `original_prefs` against the codec set actually returned
/// by the initial subscribe (`actual_codecs`). Preserves
/// `original_prefs` ordering — the intake's
/// [`active_codec_from_subscriptions`] uses prefs order as the
/// preference signal, and re-ordering would silently change the
/// peer's chosen codec.
///
/// **Why this matters (3c.3b.2b finding 1):** the peer's SDP answer is built
/// from the codec we can actually serve, not from `original_prefs`. If
/// `original_prefs = [VP8, H.264]` but initial subscribe returned only `[VP8]`
/// (because H.264 encoder construction failed at that moment — VAAPI
/// exhaustion, ffmpeg missing, etc.), the answer enables only VP8.
/// Resubscribing with `original_prefs` after a later resize could pick H.264 if
/// it became available, and the driver would reject every frame because the
/// sender was never negotiated for H.264. Locking the resubscribe prefs to
/// `actual_codecs` makes that reachability bug impossible.
///
/// Returns an empty `PeerCodecPreferences` only if the intersection
/// is empty, which the caller (`new`) prevents by erroring
/// upstream when `subscriptions` is empty (codec_set non-empty →
/// intersection non-empty when `original_prefs` is non-empty). The
/// upstream contract is asserted by the early-return at the top of
/// `new`.
pub(crate) fn filter_prefs_to_negotiated(
    original_prefs: &PeerCodecPreferences,
    actual_codecs: &[CodecKind],
) -> PeerCodecPreferences {
    let supported: Vec<CodecKind> = original_prefs
        .supported
        .iter()
        .copied()
        .filter(|c| actual_codecs.contains(c))
        .collect();
    // Preserve the federated flag so every resubscribe the intake makes
    // (resize / Closed-recovery) keeps spawning the federated-shaped
    // (quarter-res / capped-bitrate) on-demand H.264 layer rather than
    // silently reverting to the full-resolution `LayerSpec::single`.
    if original_prefs.federated {
        PeerCodecPreferences::new_federated(supported)
    } else {
        PeerCodecPreferences::new(supported)
    }
}

/// Partition pool subscriptions by active codec, dropping the inactive
/// subscriptions immediately and returning their ids so the caller can
/// release the lease's on-demand claims for codecs the active codec
/// doesn't use.
///
/// Active subscriptions are kept and returned for forwarder spawning
/// (one forwarder per `(codec, rid)` slot — that's how
/// browser-visible simulcast happens). Inactive subscriptions get
/// dropped here so their `broadcast::Receiver` clones release
/// immediately rather than lingering until end-of-scope; only the
/// ids escape so the caller can call
/// [`PoolLease::release_on_demand_subset`] on them.
///
/// Always-on slots have no `on_demand_refs` entry; passing their
/// ids to `release_on_demand_subset` is a silent no-op via the
/// skip-unknown-ids contract on that side. This helper doesn't
/// distinguish always-on from on-demand — it just emits every
/// inactive id and lets the lease side decide what to release. That
/// keeps the wasted-CPU regression caught in the 3c.3b.2a review
/// (multi-codec pool with a VP8-preferring peer keeping the H.264
/// encoder spinning into a no-receiver broadcast) closed.
///
/// Pure function for unit testability — no side effects on the
/// lease, no side effects on the pool. The release call lives at
/// the caller in `pool_frame_intake`.
pub(crate) fn partition_subscriptions_by_codec(
    subscriptions: Vec<EncoderSubscription>,
    active_codec: CodecKind,
) -> (Vec<EncoderSubscription>, Vec<EncoderId>) {
    let (active_subs, inactive_subs): (Vec<_>, Vec<_>) = subscriptions
        .into_iter()
        .partition(|s| s.id.codec == active_codec);
    let inactive_ids: Vec<EncoderId> = inactive_subs.iter().map(|s| s.id.clone()).collect();
    drop(inactive_subs);
    (active_subs, inactive_ids)
}

/// Why the intake exits a forwarder loop. The intake's outer select
/// branches on this to decide between resubscribe (encoder epoch
/// rolled over) and clean shutdown (driver gone, intake should exit).
#[derive(Debug)]
pub(crate) enum ForwarderExit {
    /// `broadcast::RecvError::Closed` — the encoder slot's `Sender`
    /// was dropped. Typically [`EncoderPool::on_resize`] or
    /// last-leaseholder exit. Resubscribe to recover.
    EncoderClosed,
    /// `mpsc::TrySendError::Closed` — the driver's encoded-frame
    /// receiver was dropped. The peer is gone (or going). Don't
    /// resubscribe; just exit.
    DriverClosed,
    /// Forwarder cancellation token fired. The intake cancels this
    /// when it's tearing down for `shutdown` propagation.
    Cancelled,
}

/// Per-peer task that bridges the [`EncoderPool`]'s per-subscription
/// `broadcast::Receiver<Arc<EncodedFrame>>` channels to the
/// [`WebRtcPeer`] driver's encoded-frame mpsc, and re-subscribes
/// transparently when an encoder slot is torn down (typically by
/// [`EncoderPool::on_resize`] or an on-demand slot's last-leaseholder
/// exit).
///
/// ## Multi-forwarder per active codec — the phase 4c contract
///
/// `pool.subscribe(prefs)` may return multiple subscriptions: one per
/// `(codec × layer)` the peer's prefs overlap with. For a peer that
/// supports both VP8 and H.264, that's two subscriptions; for a peer
/// supporting VP8 against a simulcast pool, that's one per layer.
///
/// **Codec selection stays single-codec.** Per epoch the intake picks
/// the active codec via [`active_codec_from_subscriptions`] from
/// `negotiated_prefs`'s ordering, then partitions the subscriptions
/// into:
///
/// 1. **Active partition** — every subscription whose codec matches
///    the active codec. For VP8 simulcast that's all three layer
///    subscriptions ([full, half, quarter]); for H.264 it's the single
///    layer. Each subscription in this partition gets its own
///    forwarder task; the per-peer mpsc receives [`OutboundEncodedFrame`]s
///    tagged with each forwarder's RID. **This is what makes
///    browser-visible simulcast possible** — the answer SDP advertises
///    N rids, and the multi-RID driver write path needs frames for
///    each rid to actually produce wire packets per encoding.
///
/// 2. **Inactive partition** — every subscription whose codec is
///    NOT the active codec (e.g. H.264 subscriptions when VP8 wins).
///    These IDs are passed to [`PoolLease::release_on_demand_subset`]
///    so on-demand encoders for the inactive codec(s) drop their
///    refcount immediately and (when refcount → 0) tear down rather
///    than spinning into a broadcast nobody reads. Always-on slots
///    are silently skipped (no refcount entry).
///
/// Codec mixing across the per-peer mpsc is forbidden — feeding two
/// codecs into one WebRTC sender produces codec-interleaved bytes the
/// browser cannot decode and renders the stream black. Per-RID
/// streams of the same codec ARE intentionally interleaved on the
/// mpsc; the driver's `state.video_specs[(spec, rid)]` keying keeps
/// keyframe gates independent per-rid so a P-frame on rid `h` doesn't
/// prematurely open the gate for rid `q`.
///
/// ## Multi-forwarder lifecycle
///
/// All forwarders for one epoch share a single
/// [`CancellationToken`] and report exit reasons via a bounded mpsc
/// sized to the forwarder count. **First exit wins**: whichever
/// forwarder reports first determines the epoch's exit reason
/// ([`ForwarderExit::EncoderClosed`] → resubscribe;
/// [`ForwarderExit::DriverClosed`] / [`ForwarderExit::Cancelled`] →
/// shut down). The intake then cancels the sibling forwarders and
/// reaps them via the exit channel, keeping the (codec, rid) set the
/// driver sees aligned with what the answer SDP advertised — a
/// straggler forwarder still trying to forward stale-epoch frames
/// would write packets the driver's video_specs map no longer
/// recognizes.
///
/// ## `negotiated_prefs` — the 3c.3b.2b finding-1 contract
///
/// `negotiated_prefs` is the **caller-filtered** subset of the peer's
/// original SDP-offer prefs that contains the active codec the pool's
/// initial subscribe can actually serve. This is the codec the peer's SDP
/// answer enabled (`new` derives both the answer and `negotiated_prefs` from
/// the same active subscription source).
///
/// The intake passes `negotiated_prefs` to every `pool.subscribe` —
/// resubscribe-after-Closed included. If we passed the original
/// unfiltered prefs, the resubscribe could return a codec the peer
/// never negotiated (e.g. H.264 construction failed initially but
/// succeeds after a later resize that respawns the on-demand slot).
/// `active_codec_from_subscriptions` would then pick that codec, the
/// driver would reject it as `Unsupported`, and every frame would
/// silently drop -> black stream. Locking the prefs to the negotiated
/// set at construction time and using that on every resubscribe is
/// the structural fix.
///
/// ## Lossy forwarding — the 3c.3b.2a contract (continued)
///
/// The forwarder uses [`mpsc::Sender::try_send`], not
/// `send().await`. When the driver's bounded encoded-frame mpsc is
/// full (slow peer, network stall, encoder burst), [`try_send`]
/// returns [`mpsc::error::TrySendError::Full`] and the forwarder
/// drops the frame and increments `drops_counter`.
///
/// Why lossy: `send().await` parks the forwarder inside the mpsc
/// when full. The forwarder's cancellation `select!` only fires
/// before `rx.recv()` — a parked send is uncancellable. That breaks
/// shutdown propagation: a peer whose driver is dying might never
/// signal exit because its forwarder is parked behind the dying
/// driver's full-and-then-closed mpsc. Lossy `try_send` keeps the
/// forwarder responsive to cancellation in milliseconds.
///
/// Codec keyframe machinery (the encoder's GOP cadence, plus
/// [`EncoderPool::request_keyframe_*`] when wired in 3c.4) recovers
/// the visual stream after a drop burst — exactly as the legacy
/// fan-out does today.
///
/// ## Closed handling
///
/// `on_resize` advances `source_state` BEFORE swapping/cancelling
/// encoder handles. A subscribe that hands us a brand-new
/// subscription can therefore deliver a `Receiver` whose underlying
/// `Sender` has already been dropped — the receiver returns
/// `RecvError::Closed` on the very first `recv()`. The forwarder
/// returns [`ForwarderExit::EncoderClosed`]; the intake treats that
/// as a normal "encoder epoch transitioned" signal, drops the lease
/// (which decrements refcounts under the generation gate, so stale
/// claims don't decrement replacement slots), calls
/// `pool.subscribe(&negotiated_prefs)` again, and continues with
/// fresh handles. The peer never sees the transition; no offer
/// rejection, no peer teardown.
///
/// The escalation path: if `pool.subscribe(&negotiated_prefs)` itself
/// returns `NoCompatibleCodec` (typically: a resize wiped every
/// negotiated codec and re-spawn failed) — or if
/// `active_codec_from_subscriptions` returns `None` against a
/// non-empty subscription set (a contract violation indicating
/// pool/peer divergence) — the intake signals `shutdown.cancel()` so
/// the driver tears the peer down cleanly rather than leaving a
/// never-decoding stream behind.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pool_frame_intake(
    pool: Arc<EncoderPool>,
    negotiated_prefs: PeerCodecPreferences,
    initial_subs: Vec<EncoderSubscription>,
    initial_lease: PoolLease,
    encoded_frame_tx: mpsc::Sender<OutboundEncodedFrame>,
    drops_counter: Arc<AtomicU64>,
    mut keyframe_request_rx: mpsc::Receiver<SimulcastRid>,
    shutdown: CancellationToken,
) {
    let mut current_lease = Some(initial_lease);
    let mut current_subs = initial_subs;

    'epoch: loop {
        // Phase 4c: pick the active codec, then partition subscriptions
        // into "active codec" (forward all of them — this is what
        // makes simulcast work) and "everything else" (release any
        // on-demand claims so abandoned codecs' encoders shut down).
        //
        // Per the user's correction #3: keep codec selection
        // single-codec — if VP8 wins, forward all VP8 subscriptions
        // (the simulcast layers); if H.264 wins, forward the single
        // H.264 subscription. NEVER mix codecs into one peer's
        // sender.
        let subs_now = std::mem::take(&mut current_subs);
        let active_codec = match active_codec_from_subscriptions(&subs_now, &negotiated_prefs) {
            Some(c) => c,
            None => {
                // Strict-by-construction `codec_set_from_subscriptions`
                // upstream means this should be unreachable: the SDP
                // we negotiated enables exactly the codecs the pool
                // committed to. Reaching here indicates a contract
                // divergence (pool changed shape since the original
                // subscribe). Escalate to peer teardown — leaving a
                // never-decoding stream is the worst possible outcome.
                eprintln!(
                    "[display/webrtc/pool-intake] no subscription matched \
                     negotiated_prefs (supported={:?}) from {} returned subs; \
                     signalling peer shutdown",
                    negotiated_prefs.supported,
                    subs_now.len(),
                );
                shutdown.cancel();
                return;
            }
        };
        // Partition by codec, dropping inactive subs immediately and
        // collecting their ids for release. See
        // [`partition_subscriptions_by_codec`] for the contract.
        let (active_subs, inactive_ids) = partition_subscriptions_by_codec(subs_now, active_codec);
        // Release the inactive on-demand claims on the active lease.
        // For a peer with prefs [VP8, H264] against a pool that has
        // VP8 always-on + H264 on-demand, this is what tears down
        // the never-consumed H264 encoder when the active codec is
        // VP8 — without it, H264 keeps encoding into a broadcast
        // channel with no receivers until peer disconnect (the
        // wasted-CPU regression caught in the 3c.3b.2a review).
        // After `filter_prefs_to_negotiated` locks resubscribes to
        // a single codec, this releases on the FIRST iteration only
        // (when initial_subs may include other codecs); subsequent
        // resubscribes return only active-codec subs, so
        // inactive_ids is empty.
        if !inactive_ids.is_empty() {
            if let Some(lease) = current_lease.as_mut() {
                lease.release_on_demand_subset(&inactive_ids);
            }
        }

        let active_ids: Vec<EncoderId> = active_subs.iter().map(|s| s.id.clone()).collect();
        let active_rids_summary: Vec<String> = active_subs
            .iter()
            .map(|s| s.id.rid.as_str().to_string())
            .collect();
        if active_subs.is_empty() {
            // Defensive: `active_codec_from_subscriptions` returned
            // Some, so at least one subscription matched. If the
            // partition produced an empty active set, something went
            // very wrong (subscriptions changed under us between
            // the two reads, which shouldn't be possible). Escalate.
            eprintln!(
                "[display/webrtc/pool-intake] active_codec={active_codec:?} \
                 resolved but partition produced 0 active subs; \
                 signalling peer shutdown"
            );
            shutdown.cancel();
            return;
        }

        // Spawn one forwarder task per active subscription. Each
        // forwarder reads encoded frames off ITS subscription's
        // broadcast (one per `(codec, rid)` slot) and pushes them to
        // the peer's mpsc as `OutboundEncodedFrame { rid, frame }`.
        // The driver looks up each frame's rid in `state.rtp.by_rid`
        // to pick the matching SSRC + packetizer at write time.
        //
        // Forwarder lifecycle: cancellation token shared across all
        // forwarders so an exit on one (encoder Closed, driver
        // Closed) cancels the others uniformly. Exit channel
        // capacity = number of active subs so the first-to-exit
        // forwarder's reason is preserved even if others race to
        // exit before we can drain.
        let fwd_shutdown = CancellationToken::new();
        let (exit_tx, mut exit_rx) = mpsc::channel::<ForwarderExit>(active_subs.len().max(1));
        let mut forwarders = Vec::with_capacity(active_subs.len());
        for sub in active_subs {
            let rid = sub.id.rid.clone();
            let mut rx = sub.frames;
            let frame_tx = encoded_frame_tx.clone();
            let counter = Arc::clone(&drops_counter);
            let fwd_shutdown_inner = fwd_shutdown.clone();
            let exit_tx_inner = exit_tx.clone();
            forwarders.push(tokio::spawn(async move {
                let exit = loop {
                    tokio::select! {
                        _ = fwd_shutdown_inner.cancelled() => break ForwarderExit::Cancelled,
                        res = rx.recv() => match res {
                            Ok(frame) => {
                                let outbound = OutboundEncodedFrame {
                                    rid: rid.clone(),
                                    frame,
                                };
                                match frame_tx.try_send(outbound) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        // Driver's mpsc is full. Drop
                                        // the frame; the codec's
                                        // keyframe cadence will recover
                                        // the visual stream. Lossy
                                        // forwarding is the 3c.3b.2a
                                        // contract — `send().await`
                                        // would park inside the mpsc
                                        // and break shutdown
                                        // propagation. Per-RID
                                        // forwarders inherit this:
                                        // a slow consumer on one RID
                                        // doesn't backpressure the
                                        // others (each has its own
                                        // forwarder task and the
                                        // try_send is per-task).
                                        counter.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        // Driver receiver dropped.
                                        // Peer is gone; nothing to
                                        // forward to.
                                        break ForwarderExit::DriverClosed;
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // Encoder for THIS rid torn down.
                                // Intake escalates to a unified
                                // resubscribe (cancels all sibling
                                // forwarders + drops lease). The
                                // sibling forwarders are still
                                // delivering to active encoders, but
                                // a Closed on any one rid likely
                                // means an `on_resize` epoch
                                // transition that affects ALL
                                // layers; resubscribe-as-a-unit
                                // keeps the multi-RID encodings
                                // coherent.
                                break ForwarderExit::EncoderClosed;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // Slow consumer; broadcast skipped
                                // ahead. Codec keyframe machinery
                                // (GOP / request_keyframe) recovers.
                                continue;
                            }
                        }
                    }
                };
                // Send is best-effort: if the intake is already
                // tearing down (shutdown branch fired and the
                // receiver was dropped), we just exit.
                let _ = exit_tx_inner.send(exit).await;
            }));
        }
        // Drop our `exit_tx` so when ALL forwarders' clones go away,
        // `exit_rx.recv()` returns None — gives a "all forwarders
        // gone" signal even if some forwarders' send-on-exit raced
        // teardown.
        drop(exit_tx);

        // Inner loop: stay here as long as keyframe requests come in
        // (route them to the pool and keep listening). Break out only
        // when shutdown fires or a forwarder exits — those drive the
        // outer 'epoch loop's resubscribe-or-return decisions.
        //
        // **Why an inner loop**: the keyframe-request branch must NOT
        // re-enter the 'epoch loop body. Re-entering would tear down
        // every forwarder we just spawned and respawn them — a PLI
        // burst would interrupt streaming on every layer. The inner
        // loop keeps forwarders running and just routes the request
        // to the pool's coalescer.
        enum InnerExit {
            Shutdown,
            ForwarderExited(Option<ForwarderExit>),
        }
        let inner_exit = 'inner: loop {
            tokio::select! {
                _ = shutdown.cancelled() => break 'inner InnerExit::Shutdown,
                // Phase 4e: drain keyframe-request RIDs from the
                // driver (one per inbound PLI/FIR for one of our
                // SSRCs). Route to the pool with the active codec —
                // the pool's coalescer dedups bursts within
                // KEYFRAME_COALESCE_WINDOW.
                Some(rid) = keyframe_request_rx.recv() => {
                    pool.request_keyframe(active_codec, Some(rid));
                    // Stay in inner loop; forwarders keep running.
                }
                recv = exit_rx.recv() => break 'inner InnerExit::ForwarderExited(recv),
            }
        };
        match inner_exit {
            InnerExit::Shutdown => {
                // Peer is going away. Cancel all forwarders, await
                // them, drop the lease, exit.
                fwd_shutdown.cancel();
                for f in forwarders {
                    let _ = f.await;
                }
                drop(current_lease.take());
                return;
            }
            InnerExit::ForwarderExited(recv) => {
                // First forwarder to exit reports its reason. Cancel
                // all sibling forwarders so the (codec, rid) set
                // doesn't drift (e.g. one rid resubscribing while
                // another keeps streaming the old epoch).
                fwd_shutdown.cancel();
                for f in forwarders {
                    let _ = f.await;
                }
                let exit = recv.unwrap_or(ForwarderExit::DriverClosed);
                match exit {
                    ForwarderExit::EncoderClosed => {
                        // Drop the old lease BEFORE resubscribing so
                        // its generation-gated release runs before
                        // subscribe potentially observes the slot
                        // map. The generation gate makes the order
                        // strictly safe (stale leases can't decrement
                        // replacement slots), but dropping first
                        // keeps the refcount accounting easier to
                        // reason about.
                        drop(current_lease.take());

                        // Use `negotiated_prefs`, not the original
                        // peer prefs. Resubscribing with original
                        // prefs would let the pool return codecs the
                        // peer's SDP answer never enabled (e.g. if
                        // initial subscribe excluded H.264 because
                        // construction failed, but a later resize +
                        // resubscribe finds H.264 working). The
                        // intake would then select a codec the peer
                        // never negotiated and the driver's per-spec
                        // gate marks `Unsupported`, dropping every
                        // frame -> silent black stream.
                        // This is the high-priority finding from the
                        // 3c.3b.2a review. The narrowed prefs locks
                        // resubscribe to exactly the codecs the
                        // peer's answer enabled.
                        match pool.subscribe(&negotiated_prefs) {
                            Ok((subs, lease)) => {
                                current_subs = subs;
                                current_lease = Some(lease);
                                eprintln!(
                                    "[display/webrtc/pool-intake] resubscribed \
                                     after encoder Closed (was forwarding \
                                     codec={active_codec:?} rids={:?})",
                                    active_rids_summary,
                                );
                                continue 'epoch;
                            }
                            Err(e) => {
                                eprintln!(
                                    "[display/webrtc/pool-intake] resubscribe \
                                     after Closed failed ({e:?}): no compatible \
                                     codec; signalling peer shutdown (was \
                                     forwarding {active_ids:?})"
                                );
                                shutdown.cancel();
                                return;
                            }
                        }
                    }
                    ForwarderExit::DriverClosed | ForwarderExit::Cancelled => {
                        // Driver is gone or forwarder was externally
                        // cancelled. Either way, no resubscribe — the
                        // peer's path is closing. Drop the lease and
                        // exit.
                        drop(current_lease.take());
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Phase 3c.3b.2: pool-mode intake
    // -----------------------------------------------------------------------

    /// `codec_set_from_subscriptions` dedups codecs across multi-layer
    /// (simulcast-style) subscription sets.
    #[test]
    fn codec_set_from_subscriptions_dedups_multi_layer() {
        use crate::encode::pool::{EncoderId, LayerSpec, SimulcastRid};
        let (s, _r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        let make = |codec: CodecKind, rid: SimulcastRid| EncoderSubscription {
            id: EncoderId::new(codec, rid),
            layer: LayerSpec::single(codec, 64, 64, 30),
            frames: s.subscribe(),
        };
        let subs = vec![
            make(CodecKind::Vp8, SimulcastRid::full()),
            // Same codec, different RID — must dedup (one
            // enable_vp8 call covers both layers).
            make(CodecKind::Vp8, SimulcastRid::half()),
            make(CodecKind::H264, SimulcastRid::full()),
        ];
        let codecs = codec_set_from_subscriptions(&subs);
        assert_eq!(codecs.len(), 2);
        assert!(codecs.contains(&CodecKind::Vp8));
        assert!(codecs.contains(&CodecKind::H264));
    }

    #[test]
    fn active_codec_from_subscriptions_respects_peer_pref_order() {
        use crate::encode::pool::{EncoderId, LayerSpec, SimulcastRid};
        let (s, _r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        let make = |codec: CodecKind| EncoderSubscription {
            id: EncoderId::new(codec, SimulcastRid::full()),
            layer: LayerSpec::single(codec, 64, 64, 30),
            frames: s.subscribe(),
        };
        let subs = vec![make(CodecKind::Vp8), make(CodecKind::H264)];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::H264, CodecKind::Vp8]);
        assert_eq!(
            active_codec_from_subscriptions(&subs, &prefs),
            Some(CodecKind::H264)
        );
    }

    #[test]
    fn active_codec_from_subscriptions_returns_none_on_no_pref_overlap() {
        use crate::encode::pool::{EncoderId, LayerSpec, SimulcastRid};
        let (s, _r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        let subs = vec![EncoderSubscription {
            id: EncoderId::new(CodecKind::Vp8, SimulcastRid::full()),
            layer: LayerSpec::single(CodecKind::Vp8, 64, 64, 30),
            frames: s.subscribe(),
        }];
        let prefs = PeerCodecPreferences::new(vec![CodecKind::H264]);
        assert_eq!(active_codec_from_subscriptions(&subs, &prefs), None);
    }

    // -----------------------------------------------------------------------
    // Phase 3c.3b.2b: filter_prefs_to_negotiated unit tests
    // -----------------------------------------------------------------------

    /// **3c.3b.2b finding 1 contract.** Filters original prefs against
    /// the codec set actually returned by initial subscribe, preserving
    /// the original ordering. Order matters because
    /// `active_codec_from_subscriptions` uses prefs order as the codec
    /// preference signal — re-ordering would change which codec the
    /// peer actually receives.
    #[test]
    fn filter_prefs_to_negotiated_preserves_original_order() {
        let original =
            PeerCodecPreferences::new(vec![CodecKind::H264, CodecKind::Vp8, CodecKind::Vp9]);
        // Pool returned VP8 + Vp9 only (no H.264 backend at the moment).
        let actual = vec![CodecKind::Vp8, CodecKind::Vp9];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert_eq!(filtered.supported, vec![CodecKind::Vp8, CodecKind::Vp9]);
        // Different order in `actual` must NOT re-rank the result —
        // the result follows `original`'s ordering.
        let actual_reversed = vec![CodecKind::Vp9, CodecKind::Vp8];
        let filtered2 = filter_prefs_to_negotiated(&original, &actual_reversed);
        assert_eq!(filtered2.supported, vec![CodecKind::Vp8, CodecKind::Vp9]);
    }

    /// Identity case: when actual ⊇ original, the filter is a no-op
    /// (everything in original survives).
    #[test]
    fn filter_prefs_to_negotiated_identity_when_actual_covers_original() {
        let original = PeerCodecPreferences::new(vec![CodecKind::Vp8, CodecKind::H264]);
        let actual = vec![CodecKind::Vp8, CodecKind::H264, CodecKind::Vp9];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert_eq!(filtered.supported, vec![CodecKind::Vp8, CodecKind::H264]);
    }

    /// No overlap → empty result. Caller must reject this case
    /// upstream (see the `is_empty()` guard in `new`); the
    /// filter itself doesn't error.
    #[test]
    fn filter_prefs_to_negotiated_returns_empty_when_no_overlap() {
        let original = PeerCodecPreferences::new(vec![CodecKind::H264]);
        let actual = vec![CodecKind::Vp8];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert!(filtered.is_empty());
    }

    /// Empty original → empty result. Belt-and-suspenders.
    #[test]
    fn filter_prefs_to_negotiated_returns_empty_for_empty_original() {
        let original = PeerCodecPreferences::new(vec![]);
        let actual = vec![CodecKind::Vp8, CodecKind::H264];
        let filtered = filter_prefs_to_negotiated(&original, &actual);
        assert!(filtered.is_empty());
    }

    /// Empty actual → empty result. (The pool returned no codecs;
    /// negotiation would be impossible; upstream rejects via the
    /// "subscriptions is_empty" guard before reaching the filter.)
    #[test]
    fn filter_prefs_to_negotiated_returns_empty_for_empty_actual() {
        let original = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let filtered = filter_prefs_to_negotiated(&original, &[]);
        assert!(filtered.is_empty());
    }

    /// The federated flag must survive the filter — every resubscribe the
    /// intake makes carries `negotiated_prefs`, and if the flag were
    /// dropped the on-demand H.264 layer would silently revert from the
    /// quarter-res / capped federated shape to full resolution on the
    /// first resize / Closed-recovery.
    #[test]
    fn filter_prefs_to_negotiated_preserves_federated_flag() {
        let federated = PeerCodecPreferences::new_federated(vec![CodecKind::H264, CodecKind::Vp8]);
        let filtered = filter_prefs_to_negotiated(&federated, &[CodecKind::H264, CodecKind::Vp8]);
        assert!(filtered.federated, "federated flag must be preserved");
        assert_eq!(filtered.supported, vec![CodecKind::H264, CodecKind::Vp8]);

        let local = PeerCodecPreferences::new(vec![CodecKind::H264]);
        let filtered_local = filter_prefs_to_negotiated(&local, &[CodecKind::H264]);
        assert!(
            !filtered_local.federated,
            "non-federated flag must be preserved too"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 4c follow-up (d): partition_subscriptions_by_codec unit tests
    // -----------------------------------------------------------------------

    /// Build a synthetic [`EncoderSubscription`] with a fresh
    /// `broadcast::Receiver`. The Sender is `mem::forget`ed so the
    /// channel stays open for the lifetime of the test (we never
    /// `recv()` — these tests inspect ids only).
    ///
    /// Synthetic: lets us construct H.264 subscriptions without
    /// spawning a real H.264 encoder backend (VAAPI / VideoToolbox
    /// / ffmpeg), so the partition test can exercise the
    /// VP8 + H.264 mix without the encoder backend dependency.
    fn make_partition_test_subscription(
        codec: CodecKind,
        rid: SimulcastRid,
    ) -> EncoderSubscription {
        use crate::encode::pool::LayerSpec;
        let (s, r) = broadcast::channel::<Arc<EncodedFrame>>(4);
        std::mem::forget(s);
        EncoderSubscription {
            id: EncoderId::new(codec, rid),
            layer: LayerSpec::single(codec, 64, 64, 30),
            frames: r,
        }
    }

    /// **Phase 4c follow-up (d) contract: mixed-codec partition.**
    ///
    /// When `pool.subscribe(prefs=[VP8, H.264])` returns subscriptions
    /// for both codecs (e.g. VP8 simulcast 3 layers + H.264 single
    /// layer = 4 subs total), `partition_subscriptions_by_codec` with
    /// `active_codec=Vp8` must:
    ///
    /// - Return all 3 VP8 subscriptions in the active partition (each
    ///   gets its own forwarder; per-RID frames feed the multi-RID
    ///   driver write path).
    /// - Return only the H.264 id in the inactive_ids vec (caller
    ///   passes to `lease.release_on_demand_subset` so the H.264
    ///   on-demand encoder tears down rather than spinning into a
    ///   no-receiver broadcast — the wasted-CPU regression caught
    ///   in the 3c.3b.2a review).
    ///
    /// The end-to-end chain is pinned by composition with the
    /// existing `release_on_demand_subset_decrements_only_specified_ids`
    /// + `release_on_demand_subset_silently_skips_unknown_ids` tests
    /// in `display/encode/pool.rs` — they pin the lease side, this
    /// pins the partition side, and `pool_frame_intake` passes the
    /// returned `inactive_ids` verbatim to `release_on_demand_subset`.
    #[test]
    fn partition_subscriptions_by_codec_mixed_codec_separates_active_keeps_inactive_ids() {
        let vp8_full = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::full());
        let vp8_half = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::half());
        let vp8_quarter = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::quarter());
        let h264_full = make_partition_test_subscription(CodecKind::H264, SimulcastRid::full());

        let (active, inactive_ids) = partition_subscriptions_by_codec(
            vec![vp8_full, h264_full, vp8_half, vp8_quarter],
            CodecKind::Vp8,
        );

        // Active partition: all 3 VP8 subs (forwarder spawns for each).
        assert_eq!(
            active.len(),
            3,
            "VP8 simulcast active partition must keep all 3 layer subs"
        );
        let active_ids: std::collections::HashSet<EncoderId> =
            active.iter().map(|s| s.id.clone()).collect();
        assert!(active_ids.contains(&EncoderId::new(CodecKind::Vp8, SimulcastRid::full())));
        assert!(active_ids.contains(&EncoderId::new(CodecKind::Vp8, SimulcastRid::half())));
        assert!(active_ids.contains(&EncoderId::new(CodecKind::Vp8, SimulcastRid::quarter())));

        // Inactive ids: ONLY the H.264 id. pool_frame_intake passes
        // this verbatim to lease.release_on_demand_subset, which is
        // what tears down the never-consumed H.264 on-demand encoder.
        assert_eq!(
            inactive_ids,
            vec![EncoderId::new(CodecKind::H264, SimulcastRid::full())],
            "inactive_ids must contain exactly the H.264 id (and only \
             the H.264 id) so release_on_demand_subset drops the \
             unused on-demand claim"
        );
    }

    /// Single-codec subscription set → empty inactive_ids → caller
    /// skips the release call (the `if !inactive_ids.is_empty()` guard
    /// in `pool_frame_intake`). This is the steady-state case for
    /// resubscribe-after-Closed: `filter_prefs_to_negotiated` locks
    /// resubscribe prefs to the active codec only, so subsequent
    /// epochs always have inactive_ids empty.
    #[test]
    fn partition_subscriptions_by_codec_single_codec_returns_empty_inactive_ids() {
        let vp8_full = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::full());
        let vp8_half = make_partition_test_subscription(CodecKind::Vp8, SimulcastRid::half());

        let (active, inactive_ids) =
            partition_subscriptions_by_codec(vec![vp8_full, vp8_half], CodecKind::Vp8);

        assert_eq!(active.len(), 2, "both VP8 subs end up in active");
        assert!(
            inactive_ids.is_empty(),
            "single-codec subscription set must produce empty inactive_ids"
        );
    }

    /// All subs are inactive — active partition empty, inactive_ids
    /// has every id. `pool_frame_intake` defends against this by
    /// calling `active_codec_from_subscriptions` first and escalating
    /// to peer shutdown if the active codec resolves but the partition
    /// still produces zero active subs (a "shouldn't happen" contract
    /// violation). This test pins the helper's behavior at that
    /// boundary so the defensive check upstream has well-defined
    /// inputs.
    #[test]
    fn partition_subscriptions_by_codec_no_active_match_keeps_all_inactive_ids() {
        let h264_full = make_partition_test_subscription(CodecKind::H264, SimulcastRid::full());
        let h264_half = make_partition_test_subscription(CodecKind::H264, SimulcastRid::half());

        let (active, inactive_ids) =
            partition_subscriptions_by_codec(vec![h264_full, h264_half], CodecKind::Vp8);

        assert!(active.is_empty(), "no VP8 in subs → active partition empty");
        assert_eq!(
            inactive_ids.len(),
            2,
            "both H.264 ids must surface as inactive when active codec is VP8"
        );
    }

    /// **3c.3b.2 first explicit test, per the 3c.3b.1a review.**
    ///
    /// `subscribe()` racing with `on_resize()` can briefly hand back
    /// `EncoderSubscription`s whose underlying `broadcast::Sender`
    /// has already been dropped — the receiver returns
    /// `RecvError::Closed` on its very first `recv()`. The pool
    /// intake must treat this as a normal "encoder epoch
    /// transitioned" signal: drop the lease, resubscribe, continue
    /// forwarding from the fresh handles. Critically: do NOT
    /// shut the peer down.
    ///
    /// Setup pins the contract by deliberately constructing the
    /// scenario:
    ///   1. Pool with VP8 always-on at 64x64.
    ///   2. `pool.subscribe(VP8)` → `initial_subs` whose Receiver
    ///      points at the original handle.
    ///   3. `pool.on_resize(128, 96)` — drops the original handle
    ///      (its broadcast Sender goes away with it), spawns a
    ///      replacement at 128x96. `initial_subs` is now stale.
    ///   4. Hand `initial_subs` to a freshly-spawned intake task.
    ///   5. Push frames at the new dimensions; the new always-on
    ///      encoder produces output that the intake — after
    ///      resubscribing — forwards into `frame_rx`.
    ///
    /// Pre-fix behavior would either time out (intake stuck waiting
    /// on a closed Receiver) or shut the peer down (treating Closed
    /// as fatal). Either fires this test's assertion.
    ///
    /// VP8-specific (gated off Windows): like the other `pool_intake_*`
    /// tests below, it drives a VP8 always-on/on-demand pool and
    /// subscribes with a VP8 preference. Windows has no VP8 backend
    /// (`Vp8Encoder::new` always `Err`s and VP8 is not on-demand
    /// spawnable), so `pool.subscribe(VP8)` cannot succeed there. The
    /// `pool_frame_intake` resubscribe/forward/lossy-drop semantics are
    /// codec-agnostic and fully exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_resubscribes_when_initial_subs_already_closed() {
        use crate::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        // Subscribe AGAINST the original handle.
        let (initial_subs, initial_lease) = pool.subscribe(&prefs).expect("initial subscribe");

        // Resize: original handle dropped, new one spawned.
        // initial_subs's Receivers will return Closed on first recv.
        pool.on_resize(128, 96);

        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(16);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        // Push several frames so the resubscribe-window race has time
        // to settle and we definitely hit a frame after resubscribe.
        // 5 frames over 200ms is generous; in practice the intake
        // detects Closed within one tick.
        let frame = Arc::new(vec![0u8; 128 * 96 * 3 / 2]);
        for _ in 0..5 {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let result = tokio::time::timeout(Duration::from_secs(2), frame_rx.recv())
            .await
            .expect(
                "frame_rx must produce within 2s — timeout indicates the \
             intake task either deadlocked on a closed Receiver or \
             escalated to peer shutdown instead of treating Closed \
             as a normal epoch transition",
            );
        assert!(
            result.is_some(),
            "intake must forward a frame from the post-resize encoder; \
             got None which means the channel closed — likely intake \
             tore down rather than resubscribed"
        );

        assert!(
            !shutdown.is_cancelled(),
            "intake must not shut down the peer on a normal \
             Closed → resubscribe path; this assertion catches a \
             regression where Closed escalates to peer teardown"
        );

        shutdown.cancel();
        let _ = intake_handle.await;
    }

    /// Escalation: when resubscribe genuinely cannot find a
    /// compatible codec (e.g. the pool no longer serves anything
    /// the peer wants), the intake signals peer shutdown rather
    /// than leaving the stream black. Mirror image of the
    /// happy-path test above — Closed should not always escalate,
    /// but it MUST escalate when there's no recovery available.
    ///
    /// VP8-specific (gated off Windows): seeds the intake from a VP8
    /// on-demand subscription (no VP8 backend on Windows). The
    /// Closed → resubscribe → NoCompatibleCodec → shutdown escalation it
    /// pins is codec-agnostic and exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_shuts_down_peer_when_resubscribe_finds_no_codec() {
        use crate::encode::pool::EncoderPool;

        // Pool with NO always-on encoders; on-demand only. Subscribe
        // for VP8 (spawns on-demand VP8 slot) to get initial_subs.
        let pool = Arc::new(EncoderPool::new(64, 64, 30, |_, _| vec![], None));
        let prefs_vp8 = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs_vp8).expect("initial on-demand VP8");

        // Drop the on-demand slot via resize. initial_subs's Receivers
        // will see Closed.
        pool.on_resize(128, 96);

        // Hand the intake a prefs the pool CANNOT serve (VP9 has no
        // backend wired). When intake sees Closed and resubscribes
        // with these prefs, pool.subscribe returns NoCompatibleCodec.
        // Intake must then shutdown.cancel() to terminate the peer
        // cleanly.
        let prefs_unservable = PeerCodecPreferences::new(vec![CodecKind::Vp9]);
        let (frame_tx, _frame_rx) = mpsc::channel::<OutboundEncodedFrame>(16);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs_unservable,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        // Wake the orphaned encoder thread so it sees its cancelled
        // shutdown and exits, dropping its frames-Sender clone. With
        // both senders gone (handle.frames already dropped by
        // on_resize, thread's clone dropped by exit) the broadcast
        // channel closes — only then does the intake's forwarder
        // see Closed and trigger the resubscribe → NoCompatibleCodec
        // → shutdown.cancel() escalation. In production the bridge
        // pushes constantly so this is automatic; the test simulates
        // by pushing a few wake-frames.
        let frame = Arc::new(vec![0u8; 128 * 96 * 3 / 2]);
        for _ in 0..3 {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let exited = tokio::time::timeout(Duration::from_secs(2), async {
            while !shutdown.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            exited.is_ok(),
            "intake must escalate to shutdown within 2s when \
             resubscribe returns NoCompatibleCodec; otherwise the \
             peer would sit forever with a black stream"
        );

        let _ = intake_handle.await;
    }

    // -----------------------------------------------------------------------
    // Phase 4c: pool_frame_intake multi-forwarder contract tests
    // -----------------------------------------------------------------------

    /// **Phase 4c contract: forward all active-codec layers.**
    ///
    /// With VP8 simulcast (3 always-on layers), `pool.subscribe(VP8)`
    /// returns three subscriptions. The intake spawns one forwarder
    /// per active-codec subscription and the per-peer mpsc receives
    /// frames from every rid concurrently. This is what makes
    /// browser-visible simulcast possible — the answer SDP advertises
    /// N rids, and the multi-RID driver write path needs frames for
    /// each rid to actually produce wire packets per encoding.
    ///
    /// Test pins:
    ///   1. With VP8 simulcast (3 layers) active, every rid appears
    ///      among forwarded frames over a fixed window.
    ///   2. Frame count is proportional to layers × inputs (NOT 1×
    ///      inputs as pre-4c). Pre-4c behavior would land at ~1×
    ///      (one layer forwarded), so we assert ≥ 2× to leave clear
    ///      daylight even with encoder warm-up irregularities.
    ///
    /// Replaces the pre-4c
    /// `pool_intake_forwards_only_one_layer_with_simulcast_set`
    /// (deleted with this commit) — the inverse contract is now in
    /// effect.
    ///
    /// VP8-specific (gated off Windows): the multi-forwarder contract is
    /// inherently about VP8 simulcast (3 always-on layers); Windows runs
    /// a single full-res H.264 layer with no simulcast and no VP8
    /// backend. Exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_forwards_all_active_codec_layers() {
        use crate::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            |w, h| LayerSpec::vp8_simulcast(w, h, 30),
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);

        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs).expect("VP8 simulcast subscribe");
        // Pre-condition: pool returned multiple subscriptions. If this
        // assertion fires the simulcast set was dropped to a single
        // layer somewhere upstream and this test no longer exercises
        // the multi-sub case it claims to.
        let n_layers = initial_subs.len();
        assert!(
            n_layers >= 2,
            "test setup expects multiple simulcast layers from \
             vp8_simulcast(); got {n_layers}",
        );
        let expected_rids: std::collections::HashSet<SimulcastRid> =
            initial_subs.iter().map(|s| s.id.rid.clone()).collect();
        let input_count = 12u64;

        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(1024);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        // Push frames at the source dimensions. Each i420 buffer
        // arrives at every always-on encoder via the bridge's
        // broadcast; with N forwarders, expect ~N×inputs encoded
        // frames at the per-peer mpsc.
        let frame = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..input_count {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut seen_rids: std::collections::HashSet<SimulcastRid> =
            std::collections::HashSet::new();
        let mut received: u64 = 0;
        while let Ok(outbound) = frame_rx.try_recv() {
            seen_rids.insert(outbound.rid);
            received += 1;
        }

        // Every expected rid must appear in the received stream.
        // Missing any rid means a forwarder failed to spawn for that
        // subscription OR the per-RID rid wrap was dropped.
        for rid in &expected_rids {
            assert!(
                seen_rids.contains(rid),
                "rid {} missing from forwarded frames; got rids {:?}, \
                 expected {:?} — multi-forwarder spawn or rid wrap broke",
                rid.as_str(),
                seen_rids.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
                expected_rids.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            );
        }
        // Frame count proportional to layers. Pre-4c forwarded only
        // one rid (≤ 1.5 × input); post-4c forwards N (~ N × input).
        // Assert ≥ 2× to leave daylight from the pre-4c behavior even
        // with encoder warm-up quirks.
        assert!(
            received >= input_count * 2,
            "expected ≥ {} frames forwarded for {input_count} inputs across \
             {n_layers} layers; got {received} — pre-4c behavior would \
             land at ~{} (1× input)",
            input_count * 2,
            input_count,
        );

        shutdown.cancel();
        let _ = intake_handle.await;
    }

    /// **3c.3b.2a contract: lossy forwarding (try_send).**
    ///
    /// The intake forwarder uses `try_send` and drops on `Full`,
    /// incrementing `drops_counter`. A slow peer (full mpsc) sees
    /// frames dropped while the forwarder stays responsive to
    /// cancellation — `send().await` would park inside the mpsc and
    /// make the cancel path unreachable.
    ///
    /// Pre-fix `send().await` would park the forwarder inside the mpsc
    /// when full. Cancellation would only fire on the next `rx.recv()`,
    /// which can't be reached while parked — making the cancel path
    /// effectively unbounded (or bounded only by the slow consumer's
    /// drain rate). This test pins that by:
    ///
    ///   1. Wiring a tiny driver mpsc (capacity 1) and never draining
    ///      it until late in the test.
    ///   2. Pushing many input frames so the encoder produces a burst
    ///      that overflows the mpsc.
    ///   3. Asserting `drops_counter > 0` (frames were dropped, not
    ///      blocked-on).
    ///   4. Asserting `shutdown.cancel()` causes the intake to exit
    ///      within a tight bound (parked-send would exceed it).
    ///
    /// VP8-specific (gated off Windows): drives a VP8 always-on pool and
    /// subscribes with a VP8 preference (no VP8 backend on Windows). The
    /// lossy try_send + prompt-cancel behavior is codec-agnostic and
    /// exercised on macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_drops_lossily_when_driver_mpsc_full() {
        use crate::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            move |w, h| vec![LayerSpec::single(CodecKind::Vp8, w, h, 30)],
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (initial_subs, initial_lease) =
            pool.subscribe(&prefs).expect("VP8 always-on subscribe");

        // Tiny mpsc — fills almost immediately. Keep the receiver
        // alive but never drain it during the push phase.
        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(1);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let drops_for_intake = Arc::clone(&drops);
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            drops_for_intake,
            kf_rx,
            intake_shutdown,
        ));

        // Push enough frames that the bounded mpsc(1) overflows
        // significantly. With one always-on encoder and one i420
        // input → one encoded frame, 30 inputs ≫ 1 mpsc slot, so
        // drops should be substantial.
        let frame = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..30 {
            pool.push_i420_frame(Arc::clone(&frame), Instant::now());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Allow the encoder + forwarder a moment to process the burst
        // before reading the counter.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let dropped = drops.load(Ordering::Relaxed);
        assert!(
            dropped > 0,
            "drops_counter must be incremented when the driver mpsc \
             fills; got 0. Either the forwarder is using send().await \
             (parking instead of dropping), or the mpsc isn't actually \
             filling (encoder slower than expected). Pre-fix behavior \
             would also produce 0 drops because the forwarder would \
             park indefinitely."
        );

        // Now prove cancellation propagates promptly: pre-fix, a
        // parked send would only release when frame_rx drained; we'd
        // see cancel take many ms. With try_send the forwarder is
        // never parked, so cancel + intake exit inside a tight bound.
        let cancel_start = Instant::now();
        shutdown.cancel();
        let exited = tokio::time::timeout(Duration::from_secs(1), intake_handle).await;
        let cancel_elapsed = cancel_start.elapsed();
        assert!(
            exited.is_ok(),
            "intake must exit within 1s of shutdown.cancel() (took {:?}); \
             a longer wait indicates the forwarder parked on send()",
            cancel_elapsed,
        );

        // Belt: the receiver eventually drained from the test's side
        // is fine — proves the peer COULD have consumed if it had.
        // Drain to silence "you held the receiver but never read".
        while frame_rx.try_recv().is_ok() {}
    }

    // -----------------------------------------------------------------------
    // Phase-4c-prep: OutboundEncodedFrame + per-(spec, rid) keyframe gate
    // -----------------------------------------------------------------------

    /// **Phase 4c**: every frame the intake forwards must carry the
    /// rid of the subscription that produced it. This is the
    /// mechanism that lets the driver's multi-RID write path look up
    /// the right SSRC + per-`(spec, rid)` keyframe gate at write
    /// time without needing to redundantly embed the rid in
    /// `EncodedFrame` (which is the encoder pool's output type,
    /// shared across subscribers of one slot).
    ///
    /// With multi-forwarder intake (this commit), each rid's
    /// forwarder wraps frames with its own rid. The mpsc receives
    /// frames tagged with multiple rids, and the rid on each frame
    /// matches the encoder slot that produced it. This test pins
    /// that no rid leaks across forwarders (e.g. forwarder A
    /// accidentally tagging with B's rid).
    ///
    /// Replaces the pre-4c
    /// `pool_intake_wraps_forwarded_frames_with_active_subscription_rid`
    /// (which assumed single-active-subscription) — the multi-rid
    /// version pins per-forwarder rid integrity.
    ///
    /// VP8-specific (gated off Windows): per-forwarder rid integrity is
    /// a multi-layer VP8-simulcast property; Windows runs a single
    /// full-res H.264 layer with no VP8 backend. Exercised on
    /// macOS/Linux.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pool_intake_wraps_forwarded_frames_with_per_subscription_rid() {
        use crate::encode::pool::{EncoderPool, LayerSpec};

        let pool = Arc::new(EncoderPool::new(
            64,
            64,
            30,
            |w, h| LayerSpec::vp8_simulcast(w, h, 30),
            None,
        ));
        let prefs = PeerCodecPreferences::new(vec![CodecKind::Vp8]);
        let (initial_subs, initial_lease) = pool
            .subscribe(&prefs)
            .expect("VP8 simulcast subscribe must succeed");

        let subscribed_rids: std::collections::HashSet<SimulcastRid> =
            initial_subs.iter().map(|s| s.id.rid.clone()).collect();
        // Pre-condition: subscribed to multiple rids. If this fires,
        // the test no longer exercises multi-forwarder behavior.
        assert!(
            subscribed_rids.len() >= 2,
            "test setup expects multi-rid subscription set; got {} rids",
            subscribed_rids.len(),
        );

        let (frame_tx, mut frame_rx) = mpsc::channel::<OutboundEncodedFrame>(1024);
        let shutdown = CancellationToken::new();
        let pool_clone = Arc::clone(&pool);
        let intake_shutdown = shutdown.clone();
        let drops = Arc::new(AtomicU64::new(0));
        let (_kf_tx, kf_rx) = mpsc::channel::<SimulcastRid>(8);
        let intake_handle = tokio::spawn(pool_frame_intake(
            pool_clone,
            prefs,
            initial_subs,
            initial_lease,
            frame_tx,
            Arc::clone(&drops),
            kf_rx,
            intake_shutdown,
        ));

        let i420 = Arc::new(vec![0u8; 64 * 64 * 3 / 2]);
        for _ in 0..12 {
            pool.push_i420_frame(Arc::clone(&i420), Instant::now());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Every received frame's rid must be in the subscribed set —
        // a frame tagged with an unsubscribed rid would mean a
        // forwarder leaked its rid (e.g. an Arc<SimulcastRid> shared
        // across forwarders by mistake) or a stale rid persisted
        // through a resubscribe.
        let mut received: u32 = 0;
        let mut rid_counts: std::collections::HashMap<SimulcastRid, u32> =
            std::collections::HashMap::new();
        while let Ok(outbound) = frame_rx.try_recv() {
            assert!(
                subscribed_rids.contains(&outbound.rid),
                "frame {received} arrived with rid {} but subscribed \
                 rids are {:?} — per-forwarder rid wrap leaked or \
                 stale rid persisted",
                outbound.rid.as_str(),
                subscribed_rids
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>(),
            );
            *rid_counts.entry(outbound.rid).or_insert(0) += 1;
            received += 1;
        }
        assert!(
            received > 0,
            "intake must forward at least one frame for this test to \
             pin anything — got 0. Either the encoders aren't \
             producing output (test fixture broken) or the forwarders \
             aren't sending (refactor regression)."
        );
        // At least 2 distinct rids must have produced frames —
        // single-rid forwarding (pre-4c) would only show 1.
        assert!(
            rid_counts.len() >= 2,
            "expected ≥2 rids in forwarded stream; got {} ({:?}) — \
             multi-forwarder pool intake regressed to single-rid",
            rid_counts.len(),
            rid_counts.keys().map(|r| r.as_str()).collect::<Vec<_>>(),
        );

        shutdown.cancel();
        let _ = intake_handle.await;
    }
}
