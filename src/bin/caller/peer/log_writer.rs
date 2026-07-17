//! Durable JSONL log writer for federated peer events.
//!
//! Each [`TaggedPeerEvent`] received on the channel is serialized
//! as one JSON line and appended to a file in the session log
//! directory. The receiver side of `PeerRegistry::new(log_sink)`
//! — every peer actor fans events into this writer before the
//! broadcast channel, so lagging UI consumers can't cause event
//! loss (the log is the authoritative record; the broadcast is
//! best-effort).
//!
//! ## Flush discipline
//!
//! `BufWriter::flush()` is called after every event so that
//! tailing the log (`tail -f ~/.intendant/logs/peers.jsonl`) shows
//! events as they happen, and so that a daemon crash loses at
//! most the in-memory mpsc buffer rather than whatever was
//! sitting in a filesystem write buffer. Federation traffic
//! volume is modest compared to, say, model-response deltas — a
//! few hundred events per minute at the high end — so the per-
//! event flush is fine. If this ever becomes a bottleneck, the
//! right fix is a periodic flush task rather than batching,
//! since batch boundaries would make "exactly which events were
//! durable at time T" fuzzy.
//!
//! ## Error handling
//!
//! File open failure and write errors are silently logged via
//! `eprintln!` to stderr and the task exits. Matches
//! the `session_log` module's posture: a logging failure must not cascade
//! into the rest of the daemon, and there's no good recovery
//! beyond surfacing the error so an operator can fix the disk.
//! The drop of `rx` unblocks any peer actor waiting on a
//! saturated `log_sink.send().await`, at which point those
//! actors observe `NotConnected`-style backpressure and continue
//! running (they don't hard-depend on the log for correctness).
//!
//! ## Stall forensics
//!
//! Each channel item carries its enqueue instant ([`EnqueuedPeerEvent`]);
//! the writer measures enqueue→consume and consume→durable per event and
//! emits one rate-limited `[peer-log-writer] SLOW …` stderr line when
//! either stage exceeds the shared writer-lane threshold
//! ([`crate::event::warn_writer_lane_slow`]). Silent on the happy path.

use crate::peer::event::TaggedPeerEvent;
use std::path::PathBuf;
use std::time::Instant;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A [`TaggedPeerEvent`] paired with the instant it was handed to the
/// writer channel. Only the inner event is serialized to the log; the
/// instant exists so the writer can attribute a stall to the lane
/// (enqueue→consume) vs the write path (consume→durable).
#[derive(Debug)]
pub struct EnqueuedPeerEvent {
    pub event: TaggedPeerEvent,
    pub enqueued_at: Instant,
}

impl From<TaggedPeerEvent> for EnqueuedPeerEvent {
    /// Stamp the enqueue instant at conversion — producers convert at the
    /// `send` call site, so a bounded-channel backpressure wait counts
    /// toward enqueue→consume (to the producer, that wait IS enqueue
    /// latency).
    fn from(event: TaggedPeerEvent) -> Self {
        Self {
            event,
            enqueued_at: Instant::now(),
        }
    }
}

/// Bounded capacity for the writer's input channel. Same sizing
/// philosophy as `peer::handle::EVENTS_CAPACITY`: generous enough
/// to absorb streaming bursts, bounded so a stuck writer applies
/// backpressure to producers instead of growing memory without
/// limit.
pub const LOG_CHANNEL_CAPACITY: usize = 2048;

/// Rotate the log once it grows past this size; the previous file is
/// kept as `<name>.1` (one generation), so the durable record is
/// bounded at roughly twice this figure instead of growing for the
/// daemon's lifetime. A long-lived daemon federated with busy peers
/// otherwise accretes an unbounded `peers.jsonl` (this box has had a
/// disk-pressure incident).
pub const LOG_ROTATE_BYTES: u64 = 64 * 1024 * 1024;

/// Spawn the peer log writer task and return the input channel
/// plus a join handle.
///
/// The caller typically:
/// 1. Calls `spawn_peer_log_writer(log_path)` during daemon
///    startup
/// 2. Threads the returned `Sender` into `PeerRegistry::new(...)`
/// 3. Holds the returned `JoinHandle` alongside other background
///    task handles for clean shutdown
///
/// Dropping the `Sender` (and all its clones held inside peer
/// actors) causes the writer task to drain the channel and exit.
pub fn spawn_peer_log_writer(
    log_path: PathBuf,
) -> (mpsc::Sender<EnqueuedPeerEvent>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<EnqueuedPeerEvent>(LOG_CHANNEL_CAPACITY);
    let handle = tokio::spawn(run_writer(log_path, rx, LOG_ROTATE_BYTES));
    (tx, handle)
}

/// Open the log for append and report its current size (the rotation
/// accumulator's starting point across daemon restarts).
async fn open_log(log_path: &PathBuf) -> Option<(BufWriter<tokio::fs::File>, u64)> {
    let file = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "peer log writer: failed to open {}: {e}",
                log_path.display()
            );
            return None;
        }
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    Some((BufWriter::new(file), len))
}

async fn run_writer(
    log_path: PathBuf,
    mut rx: mpsc::Receiver<EnqueuedPeerEvent>,
    rotate_bytes: u64,
) {
    // Ensure the parent directory exists before the append open.
    // Failing to create it is the same failure class as failing to
    // open the file, so we report and exit.
    if let Some(parent) = log_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            eprintln!(
                "peer log writer: failed to create log dir {}: {e}",
                parent.display()
            );
            return;
        }
    }

    let Some((mut writer, mut written)) = open_log(&log_path).await else {
        return;
    };
    let mut last_slow_warn: Option<Instant> = None;

    while let Some(EnqueuedPeerEvent { event, enqueued_at }) = rx.recv().await {
        let consumed_at = Instant::now();
        let line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                // Unserializable event — shouldn't happen, but if
                // it does, drop the event and keep running rather
                // than tearing down the writer.
                eprintln!(
                    "peer log writer: failed to serialize event from {}: {e}",
                    event.peer
                );
                continue;
            }
        };
        if writer.write_all(line.as_bytes()).await.is_err() {
            eprintln!("peer log writer: write failed, shutting down");
            return;
        }
        if writer.write_all(b"\n").await.is_err() {
            eprintln!("peer log writer: newline write failed, shutting down");
            return;
        }
        if writer.flush().await.is_err() {
            eprintln!("peer log writer: flush failed, shutting down");
            return;
        }
        // Writer-lane latency forensics: measured right after the flush so
        // consume→durable covers exactly the append+flush path; rotation
        // cost below lands in the NEXT item's enqueue→consume, where it
        // belongs. Stderr only on pathology, rate-limited.
        crate::event::warn_writer_lane_slow(
            "peer-log-writer",
            || crate::event::debug_variant_name(&event.payload),
            consumed_at.saturating_duration_since(enqueued_at),
            consumed_at.elapsed(),
            &mut last_slow_warn,
        );
        written = written.saturating_add(line.len() as u64 + 1);

        // Size-based rotation: shift the full file to `<name>.1`
        // (replacing the previous generation) and start fresh. On any
        // rotation error, keep appending to the current file — losing
        // rotation is recoverable, losing events is not.
        if written >= rotate_bytes {
            drop(writer);
            let rotated = rotated_path(&log_path);
            // Windows rename refuses to replace an existing file;
            // removing first keeps the behavior uniform.
            let _ = tokio::fs::remove_file(&rotated).await;
            if let Err(e) = tokio::fs::rename(&log_path, &rotated).await {
                eprintln!(
                    "peer log writer: rotate {} -> {} failed: {e}",
                    log_path.display(),
                    rotated.display()
                );
            }
            match open_log(&log_path).await {
                Some((reopened, len)) => {
                    writer = reopened;
                    written = len;
                }
                None => return,
            }
        }
    }

    // Channel closed: all peer actors + the registry have dropped
    // their senders. Final flush on shutdown.
    let _ = writer.flush().await;
}

/// `peers.jsonl` -> `peers.jsonl.1`.
fn rotated_path(log_path: &std::path::Path) -> PathBuf {
    let mut name = log_path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    log_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::event::{PeerEvent, PeerStatus, TaggedPeerEvent};
    use crate::peer::id::{PeerId, PeerKind};
    use tempfile::TempDir;
    use tokio::time::{timeout, Duration};

    fn make_event(seq: u64, status: PeerStatus) -> TaggedPeerEvent {
        TaggedPeerEvent {
            peer: PeerId::new(PeerKind::Intendant, "test"),
            payload: PeerEvent::StatusChanged { status },
            seq,
        }
    }

    /// Happy path: send a few events, drop the sender, assert the
    /// file contains exactly those events as JSONL in order.
    #[tokio::test]
    async fn writes_events_as_jsonl_in_order() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("peers.jsonl");
        let (tx, handle) = spawn_peer_log_writer(log_path.clone());

        tx.send(make_event(1, PeerStatus::Idle).into())
            .await
            .unwrap();
        tx.send(make_event(2, PeerStatus::Working).into())
            .await
            .unwrap();
        tx.send(make_event(3, PeerStatus::NeedsApproval).into())
            .await
            .unwrap();

        drop(tx);
        timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();

        let contents = tokio::fs::read_to_string(&log_path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);

        // Each line round-trips back to a TaggedPeerEvent.
        for (idx, line) in lines.iter().enumerate() {
            let parsed: TaggedPeerEvent = serde_json::from_str(line).unwrap();
            assert_eq!(parsed.seq, idx as u64 + 1);
        }
    }

    /// Each event is flushed immediately so a tailer (or a test
    /// reader that reads mid-run) can see it without waiting for
    /// the writer to exit or for an internal buffer to fill.
    #[tokio::test]
    async fn events_are_flushed_immediately() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("peers.jsonl");
        let (tx, _handle) = spawn_peer_log_writer(log_path.clone());

        tx.send(make_event(1, PeerStatus::Idle).into())
            .await
            .unwrap();

        // Open the log and read a line without waiting for the
        // writer task to finish. If flushes are deferred, the
        // file would be empty and this would block until the
        // timeout fires. Also tolerate NotFound for the tiny
        // window between test spawn and the writer actually
        // creating the file.
        let result = timeout(Duration::from_secs(2), async {
            loop {
                match tokio::fs::read_to_string(&log_path).await {
                    Ok(contents) if !contents.is_empty() => return contents,
                    _ => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        })
        .await;
        let contents = result.expect("event was not flushed within 2s");
        assert!(contents.contains("\"event\":\"status_changed\""));
    }

    /// Parent directories that don't exist are created
    /// automatically, so callers don't need to mkdir -p before
    /// spawning the writer. Matches how the session_log module handles
    /// fresh session directories.
    #[tokio::test]
    async fn creates_parent_directory_if_missing() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("nested").join("subdir").join("peers.jsonl");
        assert!(!log_path.parent().unwrap().exists());

        let (tx, _handle) = spawn_peer_log_writer(log_path.clone());
        tx.send(make_event(1, PeerStatus::Idle).into())
            .await
            .unwrap();

        // Wait for the write to land.
        timeout(Duration::from_secs(2), async {
            loop {
                if log_path.exists() && tokio::fs::metadata(&log_path).await.unwrap().len() > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("event never landed");

        assert!(log_path.parent().unwrap().exists());
    }

    /// Appending to an existing file preserves prior content.
    /// Guards against accidentally opening in truncate mode.
    #[tokio::test]
    async fn appends_to_existing_log() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("peers.jsonl");
        tokio::fs::write(&log_path, b"{\"existing\":\"entry\"}\n")
            .await
            .unwrap();

        let (tx, handle) = spawn_peer_log_writer(log_path.clone());
        tx.send(make_event(1, PeerStatus::Idle).into())
            .await
            .unwrap();
        drop(tx);
        timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();

        let contents = tokio::fs::read_to_string(&log_path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{\"existing\":\"entry\"}");
        assert!(lines[1].contains("\"seq\":1"));
    }

    /// When the sender is cloned (as happens when multiple peer
    /// actors share one log sink), dropping one clone doesn't
    /// shut down the writer — only when all senders drop does
    /// the writer exit.
    #[tokio::test]
    async fn writer_survives_partial_sender_drops() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("peers.jsonl");
        let (tx1, handle) = spawn_peer_log_writer(log_path.clone());
        let tx2 = tx1.clone();

        tx1.send(make_event(1, PeerStatus::Idle).into())
            .await
            .unwrap();
        drop(tx1);
        // tx2 still alive; writer should still be running.
        tx2.send(make_event(2, PeerStatus::Working).into())
            .await
            .unwrap();
        drop(tx2);
        // Now all senders gone, writer should exit.
        timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();

        let contents = tokio::fs::read_to_string(&log_path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
    }

    /// `LOG_CHANNEL_CAPACITY` is non-zero. A zero capacity would
    /// turn the bounded mpsc into a rendezvous channel and change
    /// backpressure semantics silently — same guard we added for
    /// the per-peer command channel.
    #[test]
    fn channel_capacity_is_nonzero() {
        assert!(LOG_CHANNEL_CAPACITY > 0);
    }

    /// Crossing the size cap shifts the log to `<name>.1` and starts
    /// fresh: the live file stays bounded, the previous generation is
    /// retained, no event is lost across the boundary, and every
    /// surviving line is intact JSONL.
    #[tokio::test]
    async fn rotates_log_at_size_cap() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("peers.jsonl");
        let (tx, rx) = mpsc::channel::<EnqueuedPeerEvent>(LOG_CHANNEL_CAPACITY);
        let rotate_bytes: u64 = 1024;
        let handle = tokio::spawn(run_writer(log_path.clone(), rx, rotate_bytes));
        for seq in 1..=40 {
            tx.send(make_event(seq, PeerStatus::Idle).into())
                .await
                .unwrap();
        }
        drop(tx);
        timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();

        let rotated = rotated_path(&log_path);
        assert!(rotated.exists(), "size cap must produce {rotated:?}");

        // Only the newest two generations survive by design; the newest
        // event must be present and every surviving line must parse.
        let mut max_seq = 0;
        for path in [&rotated, &log_path] {
            let Ok(contents) = tokio::fs::read_to_string(path).await else {
                continue;
            };
            for line in contents.lines() {
                let parsed: TaggedPeerEvent = serde_json::from_str(line).unwrap();
                max_seq = max_seq.max(parsed.seq);
            }
        }
        assert_eq!(max_seq, 40, "the newest event survives rotation");

        // The live file restarted below the cap (bounded by one line of
        // overshoot at most).
        let live_len = tokio::fs::metadata(&log_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(
            live_len <= rotate_bytes + 256,
            "live log stays bounded, got {live_len} bytes"
        );
    }
}
