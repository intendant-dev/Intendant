//! Single-subscriber tile streaming into a byte sink.
//!
//! The WebRTC tile bridge in `lib.rs` fans encoded tile frames out to
//! `WebRtcPeer` datachannels and owns the video-fallback/standby machinery
//! that only makes sense when a video lane exists. A Codex Cloud worker has
//! no WebRTC lane at all — its one viewer is the home daemon on the other
//! end of the attachment WebSocket — so this module composes the same tile
//! primitives (grid, damage, encode, wire transport) into a deliberately
//! smaller shape: one ordered reliable sink, tile mode only, no fallback,
//! no standby, session-independent epoch/seq counters.
//!
//! The consumer receives fully wire-encoded tile frames
//! ([`crate::tile::transport`] framing, each ≤ the datachannel message
//! cap) and can forward them verbatim; the browser's tile compositor is
//! transport-agnostic and reassembles from any ordered frame stream.
//! Backpressure is by `send().await`: a slow consumer pauses the encode
//! loop, and skipped capture frames are absorbed by the broadcast
//! channel's lag semantics — nothing is dropped mid-snapshot, so a
//! delivered snapshot group is always whole.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    capture, current_visual_marker_value, encode_tile_records, encode_tile_snapshot_frames,
    make_damage_backend, should_emit_tile_delta, tile, tile_delta_min_interval,
    tile_grid_for_frame, DisplaySession, TILE_STREAM_TILE_SIZE_PX,
};

/// Snapshot re-anchor period for the socket stream. Shorter than the
/// WebRTC bridge's 30 s tile-mode period: this lane has no GapReport
/// round-trip yet, so a frame lost above the sink (bounded relay lanes on
/// the home side) heals at the next periodic snapshot rather than via
/// recovery requests.
const SOCKET_SNAPSHOT_PERIOD: Duration = Duration::from_secs(10);

/// Handle to a running socket tile stream. Dropping it does NOT stop the
/// stream (the sink closing or the session shutting down does); call
/// [`TileSocketStream::stop`] for an explicit teardown.
pub struct TileSocketStream {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl TileSocketStream {
    /// Cancel the stream and wait for the task to exit.
    pub async fn stop(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }

    /// Cancel the stream without waiting.
    pub fn stop_nowait(&self) {
        self.stop.cancel();
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl DisplaySession {
    /// Stream this session's display as wire-encoded tile frames into
    /// `out` until the sink closes, the session shuts down, or the
    /// returned handle is stopped.
    ///
    /// Subscribing counts as external frame demand (unlike the internal
    /// WebRTC tile bridge), so a paced capture backend runs at full rate
    /// exactly while a stream is attached and drops back to keepalive
    /// cadence when it ends.
    pub fn spawn_tile_socket_stream(&self, out: mpsc::Sender<bytes::Bytes>) -> TileSocketStream {
        let mut frames = self.frame_tx.subscribe();
        let shutdown = self.shutdown.clone();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let counters = Arc::clone(&self.counters);
        let marker_flag = Arc::clone(&self.diagnostics_visual_marker);
        let session_epoch = self.session_epoch;
        let display_id = self.display_id;
        let backend_kind = self.backend.kind();
        let display_hint = self.backend.x11_display_hint();
        let (initial_w, initial_h) = self.backend.resolution();

        let task = tokio::spawn(async move {
            let mut damage =
                make_damage_backend(initial_w, initial_h, backend_kind, display_hint.as_deref());
            let mut frame_diff: Option<capture::frame_diff::FrameDiffDamageTracker> = Some(
                capture::frame_diff::FrameDiffDamageTracker::new(TILE_STREAM_TILE_SIZE_PX),
            );
            let mut grid: Option<tile::grid::TileGrid> = None;
            let mut epoch: u32 = 1;
            let mut seq: u32 = 1;
            let mut snapshot_id: u32 = 1;
            let mut next_snapshot_at = Instant::now();
            let mut last_delta_tick_at: Option<Instant> = None;
            let mut pending_rects: Vec<capture::damage::Rect> = Vec::new();

            loop {
                let frame = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = stopped.cancelled() => break,
                    result = frames.recv() => match result {
                        Ok(frame) => frame,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                };

                let Some(next_grid) = tile_grid_for_frame(&frame) else {
                    continue;
                };
                let visual_marker_value = current_visual_marker_value(&marker_flag, session_epoch);

                // Resize (or first frame): announce the grid, re-anchor
                // with a full snapshot, reset per-epoch state.
                if grid != Some(next_grid) {
                    if grid.is_some() {
                        epoch = epoch.wrapping_add(1);
                    }
                    seq = 1;
                    grid = Some(next_grid);
                    last_delta_tick_at = None;
                    pending_rects.clear();
                    frame_diff = Some(capture::frame_diff::FrameDiffDamageTracker::new(
                        TILE_STREAM_TILE_SIZE_PX,
                    ));
                    let resize = tile::transport::TileFrame::Resize {
                        new_epoch: epoch,
                        grid_w_tiles: next_grid.width_tiles,
                        grid_h_tiles: next_grid.height_tiles,
                        tile_size_px: next_grid.tile_size_px,
                    };
                    let encoded = match tile::transport::encode_frame(&resize) {
                        Ok(bytes) => bytes::Bytes::from(bytes),
                        Err(e) => {
                            eprintln!("[display/tile-socket] display {display_id} resize encode failed: {e}");
                            continue;
                        }
                    };
                    if out.send(encoded).await.is_err() {
                        break;
                    }
                    if !send_snapshot(
                        &out,
                        Arc::clone(&frame),
                        epoch,
                        &mut snapshot_id,
                        visual_marker_value,
                        &counters,
                    )
                    .await
                    {
                        break;
                    }
                    next_snapshot_at = Instant::now() + SOCKET_SNAPSHOT_PERIOD;
                    continue;
                }

                // Periodic re-anchor snapshot.
                if Instant::now() >= next_snapshot_at {
                    if !send_snapshot(
                        &out,
                        Arc::clone(&frame),
                        epoch,
                        &mut snapshot_id,
                        visual_marker_value,
                        &counters,
                    )
                    .await
                    {
                        break;
                    }
                    next_snapshot_at = Instant::now() + SOCKET_SNAPSHOT_PERIOD;
                    continue;
                }

                // Damage collection mirrors the WebRTC bridge: cheap
                // sources every frame (nothing lost across cadence
                // skips), frame-diff only on allowed ticks.
                let uses_frame_diff = frame.dirty_rects.is_none()
                    && matches!(
                        damage.capability(),
                        capture::damage::DamageCapability::FrameDiff
                            | capture::damage::DamageCapability::None
                    );
                if let Some(rects) = frame.dirty_rects.clone() {
                    pending_rects.extend(rects);
                } else if !uses_frame_diff {
                    match damage.poll_damage() {
                        Ok(rects) => pending_rects.extend(rects),
                        Err(e) => {
                            eprintln!(
                                "[display/tile-socket] display {display_id} damage poll failed: {e}"
                            );
                        }
                    }
                }

                let now = Instant::now();
                if !should_emit_tile_delta(now, last_delta_tick_at, tile_delta_min_interval()) {
                    continue;
                }
                last_delta_tick_at = Some(now);

                let rects = crate::resolve_tile_tick_damage(
                    &frame,
                    std::mem::take(&mut pending_rects),
                    uses_frame_diff,
                    &mut frame_diff,
                    display_id,
                    "[display/tile-socket]",
                )
                .await;

                if rects.is_empty() {
                    continue;
                }
                let dirty: Vec<_> = next_grid.dirty_tiles(&rects).into_iter().collect();
                if dirty.is_empty() {
                    continue;
                }
                counters.record_tile_damage_sample(
                    rects.len(),
                    dirty.len(),
                    next_grid.dirty_fraction(dirty.len()),
                );

                let encode_result = tokio::task::spawn_blocking({
                    let frame = Arc::clone(&frame);
                    move || encode_tile_records(&frame, dirty, visual_marker_value)
                })
                .await;
                let Ok(Ok(records)) = encode_result else {
                    eprintln!("[display/tile-socket] display {display_id} tile encode failed");
                    continue;
                };
                let record_count = records.len();
                let wire_frames = match tile::transport::pack_tile_updates(epoch, seq, records) {
                    Ok(frames) => frames,
                    Err(e) => {
                        eprintln!("[display/tile-socket] display {display_id} pack failed: {e}");
                        continue;
                    }
                };
                seq = seq.wrapping_add(wire_frames.len() as u32);

                let mut byte_count = 0usize;
                let frame_count = wire_frames.len();
                let mut closed = false;
                for wire_frame in wire_frames {
                    match tile::transport::encode_frame(&wire_frame) {
                        Ok(bytes) => {
                            byte_count = byte_count.saturating_add(bytes.len());
                            if out.send(bytes::Bytes::from(bytes)).await.is_err() {
                                closed = true;
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[display/tile-socket] display {display_id} wire encode failed: {e}"
                            );
                        }
                    }
                }
                counters.record_tile_delta_source(record_count, frame_count, byte_count);
                if closed {
                    break;
                }
            }
        });

        TileSocketStream { stop, task }
    }
}

/// Encode and send one complete snapshot group. Returns `false` when the
/// sink closed (the caller breaks its loop).
async fn send_snapshot(
    out: &mpsc::Sender<bytes::Bytes>,
    frame: Arc<crate::Frame>,
    epoch: u32,
    snapshot_id: &mut u32,
    visual_marker_value: Option<u32>,
    counters: &Arc<crate::DisplayMetricsCounters>,
) -> bool {
    let id = *snapshot_id;
    *snapshot_id = snapshot_id.wrapping_add(1);
    let Some(frames) =
        encode_tile_snapshot_frames(frame, epoch, id, visual_marker_value, counters).await
    else {
        return true;
    };
    for bytes in frames {
        if out.send(bytes).await.is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::SyntheticBackend;

    /// End-to-end over the synthetic backend: the stream must open with a
    /// Resize + a complete snapshot group, then keep emitting frames the
    /// wire codec round-trips.
    #[tokio::test]
    async fn socket_stream_emits_resize_then_whole_snapshot() {
        let backend = Arc::new(SyntheticBackend::new());
        let session = Arc::new(DisplaySession::new(901, backend));
        session
            .start(10, None, None)
            .await
            .expect("synthetic session starts");
        let (tx, mut rx) = mpsc::channel(1024);
        let stream = session.spawn_tile_socket_stream(tx);

        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("first frame within deadline")
            .expect("stream open");
        let decoded = tile::transport::decode_frame(&first).expect("decodable");
        match decoded {
            tile::transport::TileFrame::Resize {
                grid_w_tiles,
                grid_h_tiles,
                tile_size_px,
                ..
            } => {
                assert!(grid_w_tiles > 0 && grid_h_tiles > 0);
                assert_eq!(tile_size_px, TILE_STREAM_TILE_SIZE_PX);
            }
            other => panic!("expected Resize first, got {other:?}"),
        }

        // The snapshot group follows immediately and must be complete:
        // chunk indices 0..chunk_count for one snapshot_id.
        let mut seen = 0u16;
        let mut expected: Option<(u32, u16)> = None;
        while expected.map(|(_, total)| seen < total).unwrap_or(true) {
            let bytes = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("snapshot chunk within deadline")
                .expect("stream open");
            match tile::transport::decode_frame(&bytes).expect("decodable") {
                tile::transport::TileFrame::SnapshotChunk {
                    snapshot_id,
                    chunk_index,
                    chunk_count,
                    ..
                } => {
                    let (id, total) = *expected.get_or_insert((snapshot_id, chunk_count));
                    assert_eq!(snapshot_id, id, "one snapshot group, no interleave");
                    assert_eq!(chunk_count, total);
                    assert_eq!(chunk_index, seen, "chunks in order");
                    seen += 1;
                }
                other => panic!("expected SnapshotChunk, got {other:?}"),
            }
        }

        stream.stop().await;
        session.stop().await;
    }

    /// Closing the sink ends the stream task; the session stays healthy.
    #[tokio::test]
    async fn socket_stream_ends_when_sink_closes() {
        let backend = Arc::new(SyntheticBackend::new());
        let session = Arc::new(DisplaySession::new(902, backend));
        session
            .start(10, None, None)
            .await
            .expect("synthetic session starts");
        let (tx, mut rx) = mpsc::channel(8);
        let stream = session.spawn_tile_socket_stream(tx);
        let _ = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
        drop(rx);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !stream.is_finished() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("stream task exits after sink close");
        session.stop().await;
    }
}
