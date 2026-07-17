//! Continuous video recording for display and camera streams.
//!
//! ffmpeg is strictly the encoder: Linux display recordings let ffmpeg
//! capture directly via x11grab, while macOS display recordings capture
//! ScreenCaptureKit frames in-process and bridge them into ffmpeg as JPEGs
//! (image2pipe), exactly like browser camera frames. Output is segmented
//! MP4 files stored in the session directory. Stop paths extract the
//! [`RecordingGuard`] from the registry and await its async `finalize`;
//! plain `Drop` is only the kill-on-drop last resort.

use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::project::RecordingConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::process::Child;

/// Guard for a single ffmpeg recording process.
///
/// Deliberate stop paths extract the guard from the registry (releasing the
/// registry lock first) and await [`RecordingGuard::finalize`], which asks
/// ffmpeg to exit and waits for it to flush. `Drop` is only the last resort
/// for guards that never reach `finalize`: it aborts the bridge, closes
/// stdin, and lets `kill_on_drop(true)` reap the process — no blocking wait,
/// so dropping is safe on a tokio worker.
pub struct RecordingGuard {
    child: Child,
    /// Stdin handle for piping frames (None for x11grab mode, and for the
    /// macOS display bridge, whose task owns the pipe directly).
    stdin: Option<tokio::process::ChildStdin>,
    stream_name: String,
    segments_dir: PathBuf,
    #[allow(dead_code)]
    started_at: chrono::DateTime<chrono::Utc>,
    /// Background bridge task (frame-fed path only). Aborted on drop.
    bridge_task: Option<tokio::task::JoinHandle<()>>,
    /// In-process capture teardown (macOS display recordings): firing or
    /// dropping the sender wakes the capture pump, which quiesces the
    /// capture backend (`stop_capture`) on its own task.
    capture_stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// The capture pump itself, so [`RecordingGuard::finalize`] can await
    /// the backend quiesce with a bound.
    capture_pump: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRecordingOutcome {
    Saved,
    DiscardedEmpty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartDisplayRecordingOutcome {
    Started(String),
    AlreadyActive,
}

impl RecordingGuard {
    /// Check if the ffmpeg process is still alive.
    #[allow(dead_code)]
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn stream_name(&self) -> &str {
        &self.stream_name
    }

    pub fn segments_dir(&self) -> &Path {
        &self.segments_dir
    }

    #[allow(dead_code)]
    pub fn started_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.started_at
    }

    /// Feed a JPEG frame into the recording pipeline (frame-fed mode only).
    pub async fn feed_frame(&mut self, jpeg_data: &[u8]) -> Result<(), std::io::Error> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.write_all(jpeg_data).await?;
        }
        Ok(())
    }

    /// Gracefully finalize the recording without blocking a worker thread.
    ///
    /// Stops the feed bridge, quiesces any in-process capture (bounded —
    /// the pump owns the backend's `stop_capture`), closes stdin (image2pipe
    /// inputs finish cleanly on EOF; a bridge-owned pipe drops with the
    /// aborted bridge task), sends SIGINT (no-op on non-unix, where the EOF
    /// is the graceful path), and waits up to five seconds for ffmpeg to
    /// exit and flush its segment list. If ffmpeg is still alive after the
    /// timeout, dropping the guard lets `kill_on_drop(true)` reap it; the
    /// fragmented-MP4 segments stay playable without the final flush.
    ///
    /// Callers must not hold the recording-registry lock across this await —
    /// extract the guard under the lock, release it, then finalize.
    pub async fn finalize(mut self) {
        if let Some(handle) = self.bridge_task.take() {
            handle.abort();
        }
        // Quiesce in-process capture before asking ffmpeg to exit, so no
        // late frame races the encoder shutdown.
        if let Some(stop) = self.capture_stop.take() {
            let _ = stop.send(());
        }
        if let Some(pump) = self.capture_pump.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pump).await;
        }
        // Drop stdin first so ffmpeg sees EOF and can finalize.
        self.stdin.take();
        if let Some(id) = self.child.id() {
            crate::platform::interrupt_process(id);
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await;
    }
}

impl Drop for RecordingGuard {
    /// Last resort for guards that never went through
    /// [`RecordingGuard::finalize`] (every deliberate stop path does).
    /// Closing stdin hands ffmpeg its EOF; `kill_on_drop(true)` reaps the
    /// process right after this returns (a no-op when `finalize` already
    /// waited it out). No graceful wait happens here — Drop can run on a
    /// tokio worker, and the fragmented-MP4 segment format
    /// (`+frag_keyframe+empty_moov`) keeps already-written segments playable
    /// without ffmpeg's final flush.
    fn drop(&mut self) {
        if let Some(handle) = self.bridge_task.take() {
            handle.abort();
        }
        // Dropping the stop sender wakes the capture pump, which quiesces
        // the capture backend on its own task — Drop must not await.
        self.capture_stop.take();
        self.capture_pump.take();
        self.stdin.take();
    }
}

/// Check if ffmpeg is available on the system.
pub fn is_ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pick the next unused stream-name slot for a display recording.
/// Probes the filesystem for existing `display_<id>` / `display_<id>_N` dirs
/// and returns the first unused name.
fn pick_next_display_stream_name(recordings_dir: &Path, display_id: u32) -> String {
    let base = format!("display_{}", display_id);
    if !recordings_dir.join(&base).exists() {
        return base;
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{}_{}", base, n);
        if !recordings_dir.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

fn display_recording_base(display_id: u32) -> String {
    format!("display_{display_id}")
}

fn parse_display_recording_base(stream_name: &str) -> Option<u32> {
    stream_name.strip_prefix("display_")?.parse().ok()
}

/// Match the canonical stream name for one display and the numeric suffixes
/// generated by [`pick_next_display_stream_name`]. Keeping this strict avoids
/// treating another display (`display_10`) or an arbitrary named stream as a
/// sibling of `display_1`.
fn display_recording_stream_matches(stream_name: &str, display_id: u32) -> bool {
    let base = display_recording_base(display_id);
    if stream_name == base {
        return true;
    }
    stream_name
        .strip_prefix(&format!("{base}_"))
        .and_then(|suffix| suffix.parse::<u32>().ok())
        .is_some()
}

/// Resolve every active stream a stop request must close. Dashboard controls
/// address displays by their unsuffixed `display_N` name, while an internal
/// bridge may address its exact auto-unique name. A base request closes every
/// sibling defensively so a legacy duplicate cannot keep capturing.
fn recording_stop_targets(active: &[String], requested: &str) -> Vec<String> {
    match parse_display_recording_base(requested) {
        Some(display_id) => active
            .iter()
            .filter(|name| display_recording_stream_matches(name, display_id))
            .cloned()
            .collect(),
        None => active
            .iter()
            .filter(|name| name.as_str() == requested)
            .cloned()
            .collect(),
    }
}

/// After `ffmpeg` is spawned, give it a short grace window to fail fast.
/// x11grab errors out within <100ms on a misconfigured display; without this
/// check, the outer code emits `RecordingStarted` for a process that's already
/// dead and the user gets an empty/unplayable mp4.
///
/// Uses sleep + `try_wait` rather than `child.wait()` because tokio's
/// `wait()` synchronously drops `self.stdin` (to prevent deadlocks) the
/// moment it's called — which would close our image2pipe stdin pipe before
/// the caller ever feeds a frame, making every frame-fed recording die
/// with `Error opening input: End of file`.
///
/// Returns Ok if ffmpeg is still running after the grace window, Err with a
/// tail of `ffmpeg.log` if it already exited.
async fn verify_ffmpeg_started(
    child: &mut Child,
    log_path: &Path,
    context: &str,
) -> Result<(), String> {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    match child.try_wait() {
        Ok(None) => Ok(()),
        Ok(Some(status)) => {
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            let tail_lines: Vec<&str> = log.lines().rev().take(10).collect();
            let tail: Vec<&str> = tail_lines.into_iter().rev().collect();
            let tail = tail.join(" | ");
            let status = format!(
                "exit {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            );
            Err(format!(
                "ffmpeg ({}) exited immediately ({}): {}",
                context,
                status,
                if tail.is_empty() {
                    "no stderr captured".to_string()
                } else {
                    tail
                }
            ))
        }
        Err(e) => Err(format!("ffmpeg ({}): try_wait failed: {}", context, e)),
    }
}

/// Start recording a display via ffmpeg.
///
/// ffmpeg is strictly the encoder: Linux lets it capture directly via
/// `x11grab`; macOS captures the requested display in-process via
/// ScreenCaptureKit and bridges the frames into ffmpeg as JPEGs
/// (`image2pipe`), so the capture permission and the display targeting both
/// belong to this process rather than to a spawned helper binary.
pub async fn start_display_recording(
    display_id: u32,
    width: u32,
    height: u32,
    config: &RecordingConfig,
    session_dir: &Path,
    bus: &EventBus,
) -> Result<RecordingGuard, String> {
    #[cfg(target_os = "macos")]
    {
        // The recorded resolution comes from the live capture, not the
        // caller's probe (which has no X server to ask on macOS).
        let _ = (width, height);
        start_display_recording_macos(display_id, config, session_dir, bus).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bus;
        start_display_recording_x11grab(display_id, width, height, config, session_dir).await
    }
}

/// macOS display recording: in-process ScreenCaptureKit capture of the
/// requested display, bridged into the existing image2pipe encoder guard.
///
/// Replaces the March-era per-frame `screencapture` feeder, which predated
/// in-process SCK capture and carried two latent bugs by construction: it
/// never passed a display selector (recording the main display whatever id
/// was requested) and funneled every instance through one fixed temp file
/// (concurrent recordings corrupted each other's frames).
#[cfg(target_os = "macos")]
async fn start_display_recording_macos(
    display_id: u32,
    config: &RecordingConfig,
    session_dir: &Path,
    bus: &EventBus,
) -> Result<RecordingGuard, String> {
    let (backend, frames) = start_macos_display_capture(display_id, config.framerate).await?;
    let (width, height) = backend.resolution();

    let recordings_dir = session_dir.join("recordings");
    let stream_name = pick_next_display_stream_name(&recordings_dir, display_id);
    let mut guard = match start_frame_recording(&stream_name, config, session_dir).await {
        Ok(guard) => guard,
        Err(e) => {
            // A failed encoder start must quiesce the capture it rode in on.
            backend.stop_capture().await;
            return Err(e);
        }
    };

    // Upgrade the camera-shaped manifest with the display facts. Best
    // effort: the recording itself does not depend on manifest contents.
    let manifest = serde_json::json!({
        "stream_name": stream_name,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "framerate": config.framerate,
        "resolution": format!("{}x{}", width, height),
        "codec": "h264",
        "source": "screencapturekit_image2pipe",
    });
    let _ = std::fs::write(
        guard.segments_dir().join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    );

    // The bridge owns ffmpeg's stdin: aborting it (finalize/Drop) is what
    // hands ffmpeg its EOF.
    let Some(stdin) = guard.stdin.take() else {
        backend.stop_capture().await;
        return Err("frame recording pipeline has no stdin pipe".to_string());
    };
    let slot: ExternalFrameSlot = std::sync::Arc::new(tokio::sync::RwLock::new(None));
    let (capture_stop, capture_pump) = spawn_external_capture_pump(
        backend,
        frames,
        std::sync::Arc::clone(&slot),
        stream_name.clone(),
        bus.clone(),
    );
    let bridge = tokio::spawn(run_frame_bridge(
        BridgeSource::External { slot },
        StdinFeedSink { stdin },
        frame_to_jpeg,
        stream_name,
        config.framerate,
        bus.clone(),
    ));
    guard.bridge_task = Some(bridge);
    guard.capture_stop = Some(capture_stop);
    guard.capture_pump = Some(capture_pump);
    Ok(guard)
}

/// Start in-process ScreenCaptureKit capture of one display for recording.
///
/// `display_id` is a CGDisplayID; `0` (`kCGNullDirectDisplay`) selects the
/// main display — the debug-screen path records "the screen the browser
/// opened on" that way. Failure is actionable at the start boundary: the
/// display backend's error already carries the Screen Recording (TCC)
/// recovery guidance, because the capture now lives in this process —
/// previously Apple's `screencapture` binary held the grant instead.
#[cfg(target_os = "macos")]
async fn start_macos_display_capture(
    display_id: u32,
    fps: u32,
) -> Result<
    (
        std::sync::Arc<dyn crate::display::DisplayBackend>,
        tokio::sync::mpsc::Receiver<crate::display::Frame>,
    ),
    String,
> {
    let backend: std::sync::Arc<dyn crate::display::DisplayBackend> = if display_id == 0 {
        std::sync::Arc::new(crate::display::macos::MacOSBackend::new())
    } else {
        std::sync::Arc::new(crate::display::macos::MacOSBackend::with_display_id(
            display_id,
        ))
    };
    let frames = backend.start_capture(fps).await.map_err(|e| {
        format!("in-process screen capture failed to start for display {display_id}: {e}")
    })?;
    Ok((backend, frames))
}

/// Non-macOS display recording: ffmpeg captures directly via `x11grab`.
#[cfg(not(target_os = "macos"))]
async fn start_display_recording_x11grab(
    display_id: u32,
    width: u32,
    height: u32,
    config: &RecordingConfig,
    session_dir: &Path,
) -> Result<RecordingGuard, String> {
    let recordings_dir = session_dir.join("recordings");
    let stream_name = pick_next_display_stream_name(&recordings_dir, display_id);
    let segments_dir = recordings_dir.join(&stream_name);
    std::fs::create_dir_all(&segments_dir)
        .map_err(|e| format!("Failed to create recordings dir: {}", e))?;

    let fps_arg = config.framerate.to_string();
    let crf_arg = config.crf().to_string();
    let seg_time_arg = config.segment_duration_secs.to_string();
    let output_pattern = segments_dir.join("seg_%05d.mp4");
    let segment_list = segments_dir.join("segments.csv");

    // Write manifest
    let manifest = serde_json::json!({
        "stream_name": stream_name,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "framerate": config.framerate,
        "resolution": format!("{}x{}", width, height),
        "codec": "h264",
        "source": "x11grab",
    });
    let manifest_path = segments_dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|e| format!("Failed to write manifest: {}", e))?;

    let display_arg = format!(":{}", display_id);
    let size_arg = format!("{}x{}", width, height);
    let input_args: Vec<String> = vec![
        "-f".into(),
        "x11grab".into(),
        "-framerate".into(),
        fps_arg,
        "-video_size".into(),
        size_arg,
        "-i".into(),
        display_arg,
    ];

    // Force keyframes at segment boundaries so segments split reliably.
    let keyframe_expr = format!("expr:gte(t,n_forced*{})", config.segment_duration_secs);
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(&input_args)
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            &crf_arg,
            "-pix_fmt",
            "yuv420p",
            "-force_key_frames",
            &keyframe_expr,
            "-vsync",
            "cfr",
            "-f",
            "segment",
            "-segment_time",
            &seg_time_arg,
            "-segment_format",
            "mp4",
            "-segment_format_options",
            "movflags=+frag_keyframe+empty_moov+default_base_moof",
            "-segment_list",
            segment_list.to_str().unwrap_or("segments.csv"),
            "-segment_list_type",
            "csv",
            "-reset_timestamps",
            "1",
        ])
        .arg(output_pattern.to_str().unwrap_or("seg_%05d.mp4"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr({
            let log_path = segments_dir.join("ffmpeg.log");
            std::fs::File::create(&log_path)
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null())
        })
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_dir_all(&segments_dir);
        format!("Failed to spawn ffmpeg for display recording: {}", e)
    })?;

    // Guard against fail-fast errors: x11grab on a Wayland-only session
    // exits within ~50ms.
    verify_ffmpeg_started(&mut child, &segments_dir.join("ffmpeg.log"), "x11grab")
        .await
        .inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&segments_dir);
        })?;

    Ok(RecordingGuard {
        child,
        stdin: None,
        stream_name,
        segments_dir,
        started_at: chrono::Utc::now(),
        bridge_task: None,
        capture_stop: None,
        capture_pump: None,
    })
}

/// Start recording a frame-fed stream (camera frames piped via stdin as JPEG).
pub async fn start_frame_recording(
    stream_name: &str,
    config: &RecordingConfig,
    session_dir: &Path,
) -> Result<RecordingGuard, String> {
    let segments_dir = session_dir.join("recordings").join(stream_name);
    std::fs::create_dir_all(&segments_dir)
        .map_err(|e| format!("Failed to create recordings dir: {}", e))?;

    let fps_arg = config.framerate.to_string();
    let crf_arg = config.crf().to_string();
    let seg_time_arg = config.segment_duration_secs.to_string();
    let output_pattern = segments_dir.join("seg_%05d.mp4");
    let segment_list = segments_dir.join("segments.csv");

    // Write manifest
    let manifest = serde_json::json!({
        "stream_name": stream_name,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "framerate": config.framerate,
        "codec": "h264",
        "source": "image2pipe",
    });
    let manifest_path = segments_dir.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|e| format!("Failed to write manifest: {}", e))?;

    let keyframe_expr = format!("expr:gte(t,n_forced*{})", config.segment_duration_secs);
    let log_path = segments_dir.join("ffmpeg.log");
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-f",
            "image2pipe",
            "-framerate",
            &fps_arg,
            "-use_wallclock_as_timestamps",
            "1",
            "-i",
            "pipe:0",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            &crf_arg,
            "-pix_fmt",
            "yuv420p",
            "-force_key_frames",
            &keyframe_expr,
            "-vsync",
            "cfr",
            "-f",
            "segment",
            "-segment_time",
            &seg_time_arg,
            "-segment_format",
            "mp4",
            "-segment_format_options",
            "movflags=+frag_keyframe+empty_moov+default_base_moof",
            "-segment_list",
            segment_list.to_str().unwrap_or("segments.csv"),
            "-segment_list_type",
            "csv",
            "-reset_timestamps",
            "1",
        ])
        .arg(output_pattern.to_str().unwrap_or("seg_%05d.mp4"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(
            std::fs::File::create(&log_path)
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null()),
        )
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&segments_dir);
            format!("Failed to spawn ffmpeg for frame recording: {}", e)
        })?;

    // Catch startup-time failures (missing libx264, bad args, etc.) before we
    // hand out a RecordingGuard that will quietly produce empty segments.
    verify_ffmpeg_started(&mut child, &log_path, "image2pipe")
        .await
        .inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&segments_dir);
        })?;

    // Take stdin out of the child before moving it into the guard
    let stdin = child.stdin.take();

    Ok(RecordingGuard {
        child,
        stdin,
        stream_name: stream_name.to_string(),
        segments_dir,
        started_at: chrono::Utc::now(),
        bridge_task: None,
        capture_stop: None,
        capture_pump: None,
    })
}

/// Information about a recorded segment.
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub filename: String,
    pub start_secs: f64,
    pub end_secs: f64,
    pub path: PathBuf,
}

/// Result of seeking to a timestamp within a recording.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SeekResult {
    pub segment_path: PathBuf,
    pub offset_secs: f64,
}

/// Registry tracking active recordings and providing segment queries.
pub struct RecordingRegistry {
    recordings: HashMap<String, RecordingGuard>,
    /// Streams started via --record-display (external, persist across tasks).
    external_streams: std::collections::HashSet<String>,
    session_dir: PathBuf,
    config: RecordingConfig,
}

impl RecordingRegistry {
    pub fn new(session_dir: &Path, config: RecordingConfig) -> Self {
        Self {
            recordings: HashMap::new(),
            external_streams: std::collections::HashSet::new(),
            session_dir: session_dir.to_path_buf(),
            config,
        }
    }

    /// Whether recording is enabled in config.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Start recording a display stream.
    ///
    /// If a previous recording exists for this display but its ffmpeg process
    /// has died (e.g. Xvfb was killed between tasks), the stale entry is
    /// replaced with a fresh recording.
    pub async fn start_display(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
        bus: &EventBus,
    ) -> Result<String, String> {
        let guard = start_display_recording(
            display_id,
            width,
            height,
            &self.config,
            &self.session_dir,
            bus,
        )
        .await?;
        let stream_name = guard.stream_name().to_string();
        self.recordings.insert(stream_name.clone(), guard);
        Ok(stream_name)
    }

    /// Start recording an external display (--record-display).
    /// External streams persist across task completions.
    pub async fn start_external_display(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
        bus: &EventBus,
    ) -> Result<String, String> {
        let stream_name = self.start_display(display_id, width, height, bus).await?;
        self.external_streams.insert(stream_name.clone());
        Ok(stream_name)
    }

    /// Start a frame-fed display recording with an auto-unique stream name.
    ///
    /// Use this when a `DisplaySession` already exists for the display — the
    /// caller is responsible for driving frames into the recording via
    /// `feed_frame` (typically from a bridge task subscribed to the session's
    /// frame broadcast).  This path is required on Wayland, where the capture
    /// backend is PipeWire/portal and `x11grab` cannot see the user's desktop.
    pub async fn start_display_frame_fed(&mut self, display_id: u32) -> Result<String, String> {
        let recordings_dir = self.session_dir.join("recordings");
        let stream_name = pick_next_display_stream_name(&recordings_dir, display_id);
        let guard = start_frame_recording(&stream_name, &self.config, &self.session_dir).await?;
        self.recordings.insert(stream_name.clone(), guard);
        Ok(stream_name)
    }

    /// Start recording a frame-fed stream (e.g. camera).
    pub async fn start_stream(&mut self, stream_name: &str) -> Result<(), String> {
        if self.recordings.contains_key(stream_name) {
            return Err(format!("Already recording stream: {}", stream_name));
        }
        let guard = start_frame_recording(stream_name, &self.config, &self.session_dir).await?;
        self.recordings.insert(stream_name.to_string(), guard);
        Ok(())
    }

    /// Feed a JPEG frame to an active frame-fed recording.
    pub async fn feed_frame(
        &mut self,
        stream_name: &str,
        jpeg_data: &[u8],
    ) -> Result<(), std::io::Error> {
        if let Some(guard) = self.recordings.get_mut(stream_name) {
            guard.feed_frame(jpeg_data).await?;
        }
        Ok(())
    }

    /// Check if a stream is currently being recorded.
    pub fn is_recording(&self, stream_name: &str) -> bool {
        self.recordings.contains_key(stream_name)
    }

    /// Remove a recording from the registry for stopping, handing its guard
    /// to the caller. Callers release the registry lock, then await
    /// [`RecordingGuard::finalize`] (typically via
    /// [`finalize_stopped_recording`]) — finalization waits on ffmpeg and
    /// must never run under the registry write lock, where it would freeze
    /// every other recording's feed path.
    #[must_use]
    pub fn take_stop(&mut self, stream_name: &str) -> Option<RecordingGuard> {
        self.recordings.remove(stream_name)
    }

    /// Remove a recording from the registry for deletion (also forgetting
    /// external-stream membership), handing its guard to the caller.
    /// Finalize off-lock, then call [`Self::delete_files`].
    #[must_use]
    pub fn take_delete(&mut self, stream_name: &str) -> Option<RecordingGuard> {
        self.external_streams.remove(stream_name);
        self.recordings.remove(stream_name)
    }

    /// Delete a recording's files from disk. Any active guard must be taken
    /// out via [`Self::take_delete`] and finalized first, so ffmpeg is no
    /// longer writing into the directory being removed.
    pub fn delete_files(&self, stream_name: &str) {
        let dir = self.session_dir.join("recordings").join(stream_name);
        if dir.is_dir() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Remove every recording (including external `--record-display`
    /// streams), handing the guards to the caller for finalization.
    #[must_use]
    pub fn take_all(&mut self) -> Vec<(String, RecordingGuard)> {
        self.recordings.drain().collect()
    }

    /// Remove only agent-managed recordings, keeping external
    /// (`--record-display`) streams alive. The caller finalizes each
    /// returned guard off-lock.
    #[must_use]
    pub fn take_agent_streams(&mut self) -> Vec<(String, RecordingGuard)> {
        let to_stop: Vec<String> = self
            .recordings
            .keys()
            .filter(|name| !self.external_streams.contains(*name))
            .cloned()
            .collect();
        to_stop
            .into_iter()
            .filter_map(|name| {
                let guard = self.recordings.remove(&name)?;
                Some((name, guard))
            })
            .collect()
    }

    /// List active recording stream names.
    pub fn active_streams(&self) -> Vec<String> {
        let mut names: Vec<String> = self.recordings.keys().cloned().collect();
        names.sort();
        names
    }

    fn has_active_display_recording(&self, display_id: u32) -> bool {
        self.recordings
            .keys()
            .any(|name| display_recording_stream_matches(name, display_id))
    }

    /// Parse the segments.csv for a stream and return segment info.
    pub fn segments(&self, stream_name: &str) -> Vec<SegmentInfo> {
        let segments_dir = self.session_dir.join("recordings").join(stream_name);
        let csv_path = segments_dir.join("segments.csv");
        parse_segment_csv(&csv_path, &segments_dir)
    }

    /// Seek to a specific time offset (seconds from recording start) within a stream.
    #[allow(dead_code)]
    pub fn seek(&self, stream_name: &str, offset_secs: f64) -> Option<SeekResult> {
        let segments = self.segments(stream_name);
        for seg in &segments {
            if offset_secs >= seg.start_secs && offset_secs < seg.end_secs {
                return Some(SeekResult {
                    segment_path: seg.path.clone(),
                    offset_secs: offset_secs - seg.start_secs,
                });
            }
        }
        // If past the end, return the last segment at its end
        segments.last().map(|seg| SeekResult {
            segment_path: seg.path.clone(),
            offset_secs: seg.end_secs - seg.start_secs,
        })
    }

    /// Get the session directory path (for serving segment files).
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Read the manifest.json for a stream, if it exists.
    pub fn manifest(&self, stream_name: &str) -> Option<serde_json::Value> {
        let manifest_path = self
            .session_dir
            .join("recordings")
            .join(stream_name)
            .join("manifest.json");
        let content = std::fs::read_to_string(manifest_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Get all recorded streams (including stopped ones that have segments on disk).
    pub fn all_streams(&self) -> Vec<String> {
        let recordings_dir = self.session_dir.join("recordings");
        let mut streams = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&recordings_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        if self.recordings.contains_key(name)
                            || recording_dir_has_playable_segments(&entry.path())
                        {
                            streams.push(name.to_string());
                        }
                    }
                }
            }
        }
        streams.sort();
        streams
    }
}

/// Gracefully finalize an extracted guard, then classify the artifact.
/// Await this only after releasing the recording-registry lock.
async fn finalize_stopped_recording(guard: RecordingGuard) -> StopRecordingOutcome {
    let segments_dir = guard.segments_dir().to_path_buf();
    guard.finalize().await;

    if recording_dir_has_playable_segments(&segments_dir) {
        StopRecordingOutcome::Saved
    } else {
        let _ = std::fs::remove_dir_all(&segments_dir);
        StopRecordingOutcome::DiscardedEmpty
    }
}

fn emit_recording_stopped(bus: &EventBus, stream_name: String, outcome: StopRecordingOutcome) {
    bus.send(AppEvent::RecordingStopped {
        stream_name: stream_name.clone(),
    });
    if outcome == StopRecordingOutcome::DiscardedEmpty {
        bus.send(AppEvent::RecordingError {
            stream_name: stream_name.clone(),
            message: "No playable video frames were captured; empty recording discarded"
                .to_string(),
        });
        bus.send(AppEvent::RecordingDeleted { stream_name });
    }
}

pub fn recording_dir_has_playable_segments(segments_dir: &Path) -> bool {
    parse_segment_csv(&segments_dir.join("segments.csv"), segments_dir)
        .into_iter()
        .any(|segment| {
            segment.end_secs > segment.start_secs
                && segment
                    .path
                    .metadata()
                    .map(|metadata| metadata.is_file() && metadata.len() > 0)
                    .unwrap_or(false)
        })
}

/// Parse ffmpeg's segment list CSV (filename,start_time,end_time).
pub fn parse_segment_csv_pub(csv_path: &Path, segments_dir: &Path) -> Vec<SegmentInfo> {
    parse_segment_csv(csv_path, segments_dir)
}

fn parse_segment_csv(csv_path: &Path, segments_dir: &Path) -> Vec<SegmentInfo> {
    let content = std::fs::read_to_string(csv_path).unwrap_or_default();
    let mut segments = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() >= 3 {
            let filename = parts[0].trim().to_string();
            let start_secs: f64 = parts[1].trim().parse().unwrap_or(0.0);
            let end_secs: f64 = parts[2].trim().parse().unwrap_or(0.0);
            let path = segments_dir.join(&filename);
            segments.push(SegmentInfo {
                filename,
                start_secs,
                end_secs,
                path,
            });
        }
    }

    // Fallback: if segments.csv was empty or missing but segment files exist on
    // disk, discover them directly.  This happens when ffmpeg was interrupted
    // before flushing the CSV — the fMP4 segment files are still playable because
    // fragmented MP4 stores its index (moof boxes) inline in each fragment rather
    // than in a final moov atom, so each file is self-contained.
    if segments.is_empty() {
        if let Ok(entries) = std::fs::read_dir(segments_dir) {
            let mut found: Vec<String> = entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with("seg_") && (name.ends_with(".mp4") || name.ends_with(".ts"))
                    {
                        Some(name)
                    } else {
                        None
                    }
                })
                .collect();
            found.sort();
            // Without CSV timing data we estimate duration from file size and
            // the configured segment duration as a rough upper bound per segment.
            let fallback_dur = 60.0_f64;
            let mut offset = 0.0_f64;
            for name in found {
                let path = segments_dir.join(&name);
                let dur = if path.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
                    fallback_dur
                } else {
                    0.0
                };
                segments.push(SegmentInfo {
                    filename: name,
                    start_secs: offset,
                    end_secs: offset + dur,
                    path,
                });
                offset += dur;
            }
        }
    }

    segments
}

/// Convert a raw display frame into a JPEG for piping into ffmpeg.
///
/// Writes RGB (not RGBA): the `image` crate's JPEG encoder does not support
/// the `Rgba8` color type, so carrying the alpha channel through would make
/// `write_to` fail with "does not support color type Rgba8". JPEG is opaque
/// anyway, so the alpha byte is simply dropped during conversion.
///
/// Handles both source orderings (BGRA from Wayland/X11, RGBA from other
/// backends) and strips any stride row padding.
fn frame_to_jpeg(frame: &crate::display::Frame) -> Option<Vec<u8>> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let stride = frame.stride as usize;
    let mut rgb = Vec::with_capacity(w * h * 3);
    match frame.format {
        crate::display::FrameFormat::Bgra => {
            for row in 0..h {
                let row_start = row * stride;
                for col in 0..w {
                    let px = row_start + col * 4;
                    rgb.push(frame.data[px + 2]); // R
                    rgb.push(frame.data[px + 1]); // G
                    rgb.push(frame.data[px]); // B
                }
            }
        }
        crate::display::FrameFormat::Rgba => {
            for row in 0..h {
                let row_start = row * stride;
                for col in 0..w {
                    let px = row_start + col * 4;
                    rgb.push(frame.data[px]); // R
                    rgb.push(frame.data[px + 1]); // G
                    rgb.push(frame.data[px + 2]); // B
                }
            }
        }
    }
    let img = image::RgbImage::from_raw(frame.width, frame.height, rgb)?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Jpeg).ok()?;
    Some(buf.into_inner())
}

/// Reuse-or-encode step for one recording-bridge tick.
///
/// Idle displays return the same `Arc<Frame>` from `latest_frame()` tick
/// after tick (event-driven capture backends emit nothing while the desktop
/// is unchanged), so `encode` runs on the blocking pool only when `frame` is
/// not pointer-identical to the frame the cached bytes were encoded from.
/// The caller still feeds every tick — ffmpeg's image2pipe wallclock timing
/// assumes a steady cadence — it just feeds the cached bytes. Mirrors the
/// FrameRegistry sampler's `last_jpeg` cache in `intendant-display`.
///
/// Returns `true` when `cache` now holds JPEG bytes for `frame` (reused or
/// freshly encoded); `false` when encoding failed and this tick's feed must
/// be skipped. On failure the previous entry is kept: it is identity-keyed,
/// so it can only ever be reused for the exact frame it was encoded from.
async fn reuse_or_encode_jpeg<E>(
    cache: &mut Option<(std::sync::Arc<crate::display::Frame>, Vec<u8>)>,
    frame: std::sync::Arc<crate::display::Frame>,
    encode: E,
) -> bool
where
    E: FnOnce(&crate::display::Frame) -> Option<Vec<u8>> + Send + 'static,
{
    let cache_hit = cache
        .as_ref()
        .is_some_and(|(cached, _)| std::sync::Arc::ptr_eq(cached, &frame));
    if cache_hit {
        return true;
    }
    let encoded = tokio::task::spawn_blocking({
        let frame = std::sync::Arc::clone(&frame);
        move || encode(&frame)
    })
    .await
    .ok()
    .flatten();
    match encoded {
        Some(jpeg) => {
            *cache = Some((frame, jpeg));
            true
        }
        None => false,
    }
}

/// Latest-frame slot an in-process capture pump fills and the external
/// bridge arm polls. Latest-wins, mirroring `DisplaySession::latest_frame`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
type ExternalFrameSlot =
    std::sync::Arc<tokio::sync::RwLock<Option<std::sync::Arc<crate::display::Frame>>>>;

/// Where a frame-fed recording bridge reads its latest frame, and what
/// authorization each tick must uphold.
enum BridgeSource {
    /// An agent-visible `DisplaySession`: every tick and every feed is
    /// bound to the exact current registry session, so revoke, replacement,
    /// or private reopen stops the artifact.
    Session {
        session_registry: crate::display::SharedSessionRegistry,
        session: std::sync::Arc<crate::display::DisplaySession>,
        display_id: u32,
    },
    /// Trusted-local in-process capture with no display session to
    /// authorize against (`--record-display`, the macOS debug screen): a
    /// capture pump fills the slot. Only the macOS pipeline constructs this
    /// in production; the seam itself is cross-platform and test-covered
    /// everywhere.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    External { slot: ExternalFrameSlot },
}

impl BridgeSource {
    async fn latest_frame(&self) -> Option<std::sync::Arc<crate::display::Frame>> {
        match self {
            BridgeSource::Session { session, .. } => session.latest_frame().await,
            BridgeSource::External { slot } => slot.read().await.clone(),
        }
    }
}

/// Ceiling on one bridge tick's feed, so a wedged ffmpeg can never park the
/// bridge (nor, on the session-bound arm, park it while the session-registry
/// read guard is held).
const BRIDGE_FEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Sink for one bridge tick's JPEG bytes. Production sinks bound every feed
/// by [`BRIDGE_FEED_TIMEOUT`]; tests inject counting or failing sinks.
/// Returns `false` when the feed failed and the bridge must stop the
/// recording.
#[async_trait::async_trait]
trait BridgeFeedSink: Send + 'static {
    async fn feed(&mut self, jpeg: &[u8]) -> bool;
}

/// Feeds through the recording registry into the guard's stdin — the
/// session-bound arm, where the registry lookup keeps the feed linearized
/// with stop/delete guard extraction.
struct RegistryFeedSink {
    registry: std::sync::Arc<tokio::sync::RwLock<RecordingRegistry>>,
    stream_name: String,
}

#[async_trait::async_trait]
impl BridgeFeedSink for RegistryFeedSink {
    async fn feed(&mut self, jpeg: &[u8]) -> bool {
        matches!(
            tokio::time::timeout(BRIDGE_FEED_TIMEOUT, async {
                let mut r = self.registry.write().await;
                r.feed_frame(&self.stream_name, jpeg).await
            })
            .await,
            Ok(Ok(()))
        )
    }
}

/// Owns ffmpeg's stdin directly — the external arm, whose guard may live
/// outside the recording registry entirely (the debug screen holds its
/// guard locally). Dropping the sink with its aborted bridge task is what
/// hands ffmpeg its EOF.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct StdinFeedSink {
    stdin: tokio::process::ChildStdin,
}

#[async_trait::async_trait]
impl BridgeFeedSink for StdinFeedSink {
    async fn feed(&mut self, jpeg: &[u8]) -> bool {
        matches!(
            tokio::time::timeout(BRIDGE_FEED_TIMEOUT, self.stdin.write_all(jpeg)).await,
            Ok(Ok(()))
        )
    }
}

/// Drive a frame-fed recording bridge until the recording stops.
///
/// Ticks at the configured recording framerate and polls the source's
/// latest frame on each tick, mirroring how the WebRTC encoder bridge keeps
/// a steady cadence even when the capture backend delivers new frames
/// slowly (event-driven backends emit nothing while the desktop is idle).
/// Without this, the recording would contain only the handful of frames
/// that happened to arrive during the recording window. Unchanged frames
/// reuse the identity-keyed JPEG cache instead of re-encoding, but every
/// tick still feeds — ffmpeg's image2pipe wallclock timing assumes a steady
/// cadence. A failed feed stops the recording via the lossless control
/// lane; on the session-bound arm, every feed is additionally bound to the
/// exact current agent-visible registry session.
async fn run_frame_bridge<S, E>(
    source: BridgeSource,
    mut sink: S,
    encode: E,
    stream_name: String,
    fps: u32,
    bus: EventBus,
) where
    S: BridgeFeedSink,
    E: Fn(&crate::display::Frame) -> Option<Vec<u8>> + Clone + Send + 'static,
{
    let interval = std::time::Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 67 });
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The frame the cached JPEG was encoded from, so an unchanged
    // `latest_frame` reuses the encoded bytes instead of burning an RGB
    // conversion + JPEG encode per tick on an idle display.
    let mut last_jpeg: Option<(std::sync::Arc<crate::display::Frame>, Vec<u8>)> = None;
    loop {
        tick.tick().await;
        if let BridgeSource::Session {
            session_registry,
            session,
            display_id,
        } = &source
        {
            match recording_session_state(session_registry, *display_id, session).await {
                RecordingSessionState::Authorized => {}
                state => {
                    stop_recording_for_session_change(&bus, &stream_name, *display_id, state);
                    break;
                }
            }
        }
        let Some(frame) = source.latest_frame().await else {
            continue;
        };
        if !reuse_or_encode_jpeg(&mut last_jpeg, frame, encode.clone()).await {
            continue;
        }
        let Some((_, jpeg)) = last_jpeg.as_ref() else {
            continue;
        };
        let fed = match &source {
            BridgeSource::Session {
                session_registry,
                session,
                display_id,
            } => {
                // JPEG encoding runs on the blocking pool. Re-check after
                // it, then retain the registry read guard through the final
                // feed. A revoke/replacement needs the write guard, so the
                // artifact write is linearized wholly before or wholly
                // after that lifecycle boundary.
                let session_guard = session_registry.read().await;
                let state = recording_session_state_in(&session_guard, *display_id, session);
                if state != RecordingSessionState::Authorized {
                    drop(session_guard);
                    stop_recording_for_session_change(&bus, &stream_name, *display_id, state);
                    break;
                }
                let fed = sink.feed(jpeg).await;
                drop(session_guard);
                fed
            }
            BridgeSource::External { .. } => sink.feed(jpeg).await,
        };
        if !fed {
            bus.send(AppEvent::ControlCommand(ControlMsg::StopRecording {
                stream_name: stream_name.clone(),
            }));
            break;
        }
    }
}

/// Drain an in-process capture backend's frames into the external bridge
/// slot until stopped, then quiesce the backend.
///
/// Teardown routes through the returned stop sender: `finalize` fires it
/// and awaits the pump with a bound; `Drop` merely drops it, which wakes
/// the pump the same way without awaiting. If the capture source ends on
/// its own (display disconnected, capture stream failure), the pump stops
/// the recording via the lossless control lane instead of letting the
/// bridge freeze on the last frame forever.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn spawn_external_capture_pump(
    backend: std::sync::Arc<dyn crate::display::DisplayBackend>,
    mut frames: tokio::sync::mpsc::Receiver<crate::display::Frame>,
    slot: ExternalFrameSlot,
    stream_name: String,
    bus: EventBus,
) -> (tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                frame = frames.recv() => match frame {
                    Some(frame) => {
                        *slot.write().await = Some(std::sync::Arc::new(frame));
                    }
                    None => {
                        bus.send(AppEvent::ControlCommand(ControlMsg::StopRecording {
                            stream_name: stream_name.clone(),
                        }));
                        break;
                    }
                },
            }
        }
        backend.stop_capture().await;
    });
    (stop_tx, task)
}

/// Spawn the bridge task that feeds an agent-visible session's frames into
/// a frame-fed recording, bound to that exact session's authorization.
///
/// Returns the `JoinHandle` so the caller can store it in the
/// `RecordingGuard` for abort-on-drop.
fn spawn_frame_bridge(
    registry: std::sync::Arc<tokio::sync::RwLock<RecordingRegistry>>,
    session_registry: crate::display::SharedSessionRegistry,
    session: std::sync::Arc<crate::display::DisplaySession>,
    display_id: u32,
    stream_name: String,
    fps: u32,
    bus: EventBus,
) -> tokio::task::JoinHandle<()> {
    let sink = RegistryFeedSink {
        registry,
        stream_name: stream_name.clone(),
    };
    let source = BridgeSource::Session {
        session_registry,
        session,
        display_id,
    };
    tokio::spawn(run_frame_bridge(
        source,
        sink,
        frame_to_jpeg,
        stream_name,
        fps,
        bus,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordingSessionState {
    Authorized,
    Replaced { width: u32, height: u32 },
    Unavailable,
}

fn recording_session_state_in(
    session_registry: &crate::display::SessionRegistry,
    display_id: u32,
    expected: &std::sync::Arc<crate::display::DisplaySession>,
) -> RecordingSessionState {
    let Some(current) = session_registry.get(display_id) else {
        return RecordingSessionState::Unavailable;
    };
    if std::sync::Arc::ptr_eq(&current, expected) {
        RecordingSessionState::Authorized
    } else {
        let (width, height) = current.resolution();
        RecordingSessionState::Replaced { width, height }
    }
}

async fn recording_session_state(
    session_registry: &crate::display::SharedSessionRegistry,
    display_id: u32,
    expected: &std::sync::Arc<crate::display::DisplaySession>,
) -> RecordingSessionState {
    let session_guard = session_registry.read().await;
    recording_session_state_in(&session_guard, display_id, expected)
}

fn stop_recording_for_session_change(
    bus: &EventBus,
    stream_name: &str,
    display_id: u32,
    state: RecordingSessionState,
) {
    bus.send(AppEvent::ControlCommand(ControlMsg::StopRecording {
        stream_name: stream_name.to_string(),
    }));
    if let RecordingSessionState::Replaced { width, height } = state {
        // Keep configured auto-recording continuous across a visible session
        // replacement. Stop and re-announcement share the ordered lossless
        // lane, so the old guard is gone before the new start is considered.
        bus.send(AppEvent::DisplayReady {
            display_id,
            width,
            height,
            agent_visible: true,
        });
    }
}

async fn recording_session_remains_authorized(
    session_registry: &crate::display::SharedSessionRegistry,
    display_id: u32,
    expected: &std::sync::Arc<crate::display::DisplaySession>,
) -> bool {
    recording_session_state(session_registry, display_id, expected).await
        == RecordingSessionState::Authorized
}

/// Start a display recording from an active agent-visible `DisplaySession`.
/// Both the DisplayReady auto-start path and manual `StartRecording` use this
/// gate. Private or absent sessions fail closed: recording files live in the
/// ordinary session artifact tree, so allowing a private view to reach disk
/// would bypass its owner-only media boundary through filesystem and transfer
/// APIs. The trusted-local `--record-display` CLI remains a separate explicit
/// capture path (in-process ScreenCaptureKit on macOS, x11grab on Linux).
async fn start_display_auto(
    registry: &std::sync::Arc<tokio::sync::RwLock<RecordingRegistry>>,
    session_registry: Option<&crate::display::SharedSessionRegistry>,
    display_id: u32,
    bus: &EventBus,
) -> Result<StartDisplayRecordingOutcome, String> {
    let session_registry = session_registry.cloned().ok_or_else(|| {
        format!(
            "display {display_id} is not an active agent-visible session; \
             private views cannot be recorded"
        )
    })?;
    let display_session = session_registry
        .read()
        .await
        .get(display_id)
        .ok_or_else(|| {
            format!(
                "display {display_id} is not an active agent-visible session; \
                 private views cannot be recorded"
            )
        })?;

    if registry
        .read()
        .await
        .has_active_display_recording(display_id)
    {
        return Ok(StartDisplayRecordingOutcome::AlreadyActive);
    }
    if !is_ffmpeg_available() {
        return Err("ffmpeg not installed".to_string());
    }

    let mut reg = registry.write().await;
    // DisplayReady is also a state re-announcement (existing grants and
    // reconnects emit it). Make the common start boundary idempotent rather
    // than relying on each caller to remember a preflight check.
    if reg.has_active_display_recording(display_id) {
        return Ok(StartDisplayRecordingOutcome::AlreadyActive);
    }
    let fps = reg.config.framerate;
    let stream_name = reg.start_display_frame_fed(display_id).await?;
    drop(reg);
    // ffmpeg startup awaits while the display registry can change. Bind the
    // new artifact to the exact still-visible session before launching the
    // feed bridge; otherwise discard it immediately.
    if !recording_session_remains_authorized(&session_registry, display_id, &display_session).await
    {
        let guard = registry.write().await.take_stop(&stream_name);
        if let Some(guard) = guard {
            // Nothing was fed yet, so finalization discards the empty
            // artifact; run it after the registry lock is released.
            let _ = finalize_stopped_recording(guard).await;
        }
        return Err(format!(
            "display {display_id} stopped being an active agent-visible session"
        ));
    }
    let handle = spawn_frame_bridge(
        registry.clone(),
        session_registry,
        display_session,
        display_id,
        stream_name.clone(),
        fps,
        bus.clone(),
    );
    let mut reg = registry.write().await;
    if let Some(guard) = reg.recordings.get_mut(&stream_name) {
        guard.bridge_task = Some(handle);
    } else {
        handle.abort();
    }
    Ok(StartDisplayRecordingOutcome::Started(stream_name))
}

/// Spawn a background task that listens for DisplayReady events and starts
/// display recording automatically.
///
/// Recording uses the frame-fed path for an active agent-visible
/// `DisplaySession`: frames are subscribed from the session's broadcast
/// channel, JPEG-encoded, and piped into ffmpeg via `image2pipe`. Private or
/// absent sessions are rejected before any artifact is created. The event
/// receiver is the EventBus's ordered, lossless intent lane: recording controls,
/// `DisplayReady`, and `TaskComplete` must not be dropped or reordered by a
/// model-delta flood.
pub fn spawn_recording_listener(
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    registry: std::sync::Arc<tokio::sync::RwLock<RecordingRegistry>>,
    bus: EventBus,
    session_registry: Option<crate::display::SharedSessionRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            match event {
                AppEvent::DisplayReady {
                    display_id,
                    agent_visible,
                    ..
                } => {
                    // Auto-recording is opt-in; if disabled in config, skip.
                    if !registry.read().await.is_enabled() {
                        continue;
                    }
                    // Never auto-record a private user view: its frames would
                    // land in the ordinary session artifact tree. The common
                    // start helper rechecks this boundary for manual starts
                    // and lifecycle races too.
                    if !agent_visible {
                        eprintln!(
                            "[recording] display {display_id} is a private user view; \
                             skipping auto-record"
                        );
                        continue;
                    }
                    match start_display_auto(&registry, session_registry.as_ref(), display_id, &bus)
                        .await
                    {
                        Ok(StartDisplayRecordingOutcome::Started(stream_name)) => {
                            bus.send(AppEvent::RecordingStarted { stream_name });
                        }
                        Ok(StartDisplayRecordingOutcome::AlreadyActive) => {}
                        Err(e) => {
                            bus.send(AppEvent::RecordingError {
                                stream_name: format!("display_{}", display_id),
                                message: e,
                            });
                        }
                    }
                }
                AppEvent::ControlCommand(crate::event::ControlMsg::StartRecording {
                    stream_name,
                }) => {
                    // Only display_N streams are startable via ControlMsg.
                    let Some(id_str) = stream_name.strip_prefix("display_") else {
                        continue;
                    };
                    let Ok(display_id) = id_str.parse::<u32>() else {
                        continue;
                    };
                    match start_display_auto(&registry, session_registry.as_ref(), display_id, &bus)
                        .await
                    {
                        Ok(StartDisplayRecordingOutcome::Started(name)) => {
                            bus.send(AppEvent::RecordingStarted { stream_name: name })
                        }
                        Ok(StartDisplayRecordingOutcome::AlreadyActive) => {}
                        Err(e) => bus.send(AppEvent::RecordingError {
                            stream_name,
                            message: e,
                        }),
                    }
                }
                AppEvent::ControlCommand(crate::event::ControlMsg::StopRecording {
                    stream_name,
                }) => {
                    // Extract the guards under the write lock, but finalize
                    // them (ffmpeg SIGINT + bounded wait) in background
                    // tasks: holding the registry lock across finalization
                    // froze every other recording's feed path, and blocking
                    // the lossless control lane would delay the commands
                    // behind this one. RecordingStopped still fires only
                    // once the artifact is finalized and playable.
                    let stopped = {
                        let mut reg = registry.write().await;
                        let active = reg.active_streams();
                        recording_stop_targets(&active, &stream_name)
                            .into_iter()
                            .filter_map(|actual| {
                                reg.take_stop(&actual).map(|guard| (actual, guard))
                            })
                            .collect::<Vec<_>>()
                    };
                    for (actual, guard) in stopped {
                        let bus = bus.clone();
                        tokio::spawn(async move {
                            let outcome = finalize_stopped_recording(guard).await;
                            emit_recording_stopped(&bus, actual, outcome);
                        });
                    }
                }
                AppEvent::ControlCommand(crate::event::ControlMsg::DeleteRecording {
                    stream_name,
                }) => {
                    let active_guard = {
                        let mut reg = registry.write().await;
                        let guard = reg.take_delete(&stream_name);
                        if guard.is_none() {
                            // Not live — nothing to finalize, remove in place.
                            reg.delete_files(&stream_name);
                        }
                        guard
                    };
                    match active_guard {
                        None => bus.send(AppEvent::RecordingDeleted { stream_name }),
                        Some(guard) => {
                            // Finalize off-lock so a live delete cannot stall
                            // other recordings; ffmpeg must be gone before its
                            // directory is removed. The removal itself re-takes
                            // the lock so it stays serialized with stream-name
                            // picking, which probes the same directories.
                            let registry = registry.clone();
                            let bus = bus.clone();
                            tokio::spawn(async move {
                                guard.finalize().await;
                                registry.write().await.delete_files(&stream_name);
                                bus.send(AppEvent::RecordingStopped {
                                    stream_name: stream_name.clone(),
                                });
                                bus.send(AppEvent::RecordingDeleted { stream_name });
                            });
                        }
                    }
                }
                AppEvent::TaskComplete { .. } => {
                    // Stop agent-managed recordings, keep external
                    // (--record-display) alive. Guards come out under the
                    // lock; finalization runs in background tasks so the
                    // next task's DisplayReady is not delayed behind ffmpeg
                    // shutdown, and events fire only once each artifact is
                    // finalized.
                    let stopped = {
                        let mut reg = registry.write().await;
                        reg.take_agent_streams()
                    };
                    for (stream, guard) in stopped {
                        let bus = bus.clone();
                        tokio::spawn(async move {
                            let outcome = finalize_stopped_recording(guard).await;
                            emit_recording_stopped(&bus, stream, outcome);
                        });
                    }
                    // Don't break — keep listening for new tasks (--continue)
                }
                _ => continue,
            }
        }
        // Lossless lane closed — stop everything including external streams.
        // Shutdown can afford awaiting each finalize inline; the emit still
        // follows finalization so a listener that observes RecordingStopped
        // sees a playable artifact.
        let stopped = {
            let mut reg = registry.write().await;
            reg.take_all()
        };
        for (stream, guard) in stopped {
            guard.finalize().await;
            bus.send(AppEvent::RecordingStopped {
                stream_name: stream,
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame() -> std::sync::Arc<crate::display::Frame> {
        std::sync::Arc::new(crate::display::Frame {
            data: vec![0u8; 2 * 2 * 4],
            format: crate::display::FrameFormat::Bgra,
            width: 2,
            height: 2,
            stride: 8,
            timestamp: std::time::Instant::now(),
            dirty_rects: None,
        })
    }

    #[tokio::test]
    async fn bridge_jpeg_cache_reuses_bytes_for_identical_frame_arc() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let encodes = std::sync::Arc::new(AtomicUsize::new(0));
        let counting = |bytes: Vec<u8>| {
            let encodes = std::sync::Arc::clone(&encodes);
            move |_: &crate::display::Frame| {
                encodes.fetch_add(1, Ordering::SeqCst);
                Some(bytes)
            }
        };

        let mut cache = None;
        let frame_a = test_frame();
        assert!(
            reuse_or_encode_jpeg(
                &mut cache,
                std::sync::Arc::clone(&frame_a),
                counting(vec![1])
            )
            .await
        );
        assert_eq!(encodes.load(Ordering::SeqCst), 1);
        assert_eq!(cache.as_ref().unwrap().1, vec![1]);

        // The identical Arc<Frame> reuses the cached bytes without re-encoding.
        assert!(
            reuse_or_encode_jpeg(
                &mut cache,
                std::sync::Arc::clone(&frame_a),
                counting(vec![2])
            )
            .await
        );
        assert_eq!(
            encodes.load(Ordering::SeqCst),
            1,
            "an unchanged Arc<Frame> must not re-encode"
        );
        assert_eq!(cache.as_ref().unwrap().1, vec![1]);

        // A new frame re-encodes even when its contents are identical:
        // the cache is keyed on frame identity, not pixel equality.
        let frame_b = test_frame();
        assert!(
            reuse_or_encode_jpeg(
                &mut cache,
                std::sync::Arc::clone(&frame_b),
                counting(vec![3])
            )
            .await
        );
        assert_eq!(encodes.load(Ordering::SeqCst), 2);
        assert_eq!(cache.as_ref().unwrap().1, vec![3]);

        // Encode failure skips the tick but keeps the previous entry, which
        // still hits for its own frame afterwards.
        let failing = {
            let encodes = std::sync::Arc::clone(&encodes);
            move |_: &crate::display::Frame| {
                encodes.fetch_add(1, Ordering::SeqCst);
                None
            }
        };
        assert!(!reuse_or_encode_jpeg(&mut cache, test_frame(), failing).await);
        assert_eq!(encodes.load(Ordering::SeqCst), 3);
        assert!(
            reuse_or_encode_jpeg(
                &mut cache,
                std::sync::Arc::clone(&frame_b),
                counting(vec![4])
            )
            .await
        );
        assert_eq!(encodes.load(Ordering::SeqCst), 3);
        assert_eq!(cache.as_ref().unwrap().1, vec![3]);
    }

    #[tokio::test]
    async fn bridge_jpeg_cache_carries_real_encoder_output() {
        let mut cache = None;
        assert!(reuse_or_encode_jpeg(&mut cache, test_frame(), frame_to_jpeg).await);
        let (_, jpeg) = cache.as_ref().unwrap();
        assert!(
            jpeg.starts_with(&[0xFF, 0xD8]),
            "cached bytes must be a JPEG (SOI marker)"
        );
    }

    #[test]
    fn display_recording_stream_matching_is_id_scoped_and_suffix_strict() {
        assert!(display_recording_stream_matches("display_1", 1));
        assert!(display_recording_stream_matches("display_1_2", 1));
        assert!(display_recording_stream_matches("display_1_99", 1));
        assert!(!display_recording_stream_matches("display_10", 1));
        assert!(!display_recording_stream_matches("display_1_camera", 1));
        assert!(!display_recording_stream_matches("camera_display_1", 1));
    }

    #[test]
    fn display_base_stop_closes_every_sibling_but_exact_stop_stays_exact() {
        let active = vec![
            "camera".to_string(),
            "display_1".to_string(),
            "display_1_2".to_string(),
            "display_10".to_string(),
        ];
        assert_eq!(
            recording_stop_targets(&active, "display_1"),
            vec!["display_1".to_string(), "display_1_2".to_string()]
        );
        assert_eq!(
            recording_stop_targets(&active, "display_1_2"),
            vec!["display_1_2".to_string()]
        );
        assert_eq!(
            recording_stop_targets(&active, "camera"),
            vec!["camera".to_string()]
        );
    }

    #[test]
    fn recording_config_crf_values() {
        let mut config = RecordingConfig::default();
        assert_eq!(config.crf(), 28); // medium default
        config.quality = "low".to_string();
        assert_eq!(config.crf(), 35);
        config.quality = "high".to_string();
        assert_eq!(config.crf(), 20);
        config.quality = "unknown".to_string();
        assert_eq!(config.crf(), 28); // fallback to medium
    }

    #[test]
    fn recording_config_defaults() {
        let config = RecordingConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.framerate, 15);
        assert_eq!(config.segment_duration_secs, 60);
        assert_eq!(config.quality, "medium");
        assert!(config.max_retention_hours.is_none());
    }

    #[test]
    fn recording_config_from_toml() {
        let toml_str = r#"
enabled = true
framerate = 15
segment_duration_secs = 120
quality = "high"
max_retention_hours = 48
"#;
        let config: RecordingConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.framerate, 15);
        assert_eq!(config.segment_duration_secs, 120);
        assert_eq!(config.quality, "high");
        assert_eq!(config.max_retention_hours, Some(48));
    }

    #[test]
    fn parse_segment_csv_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let csv = tmp.path().join("segments.csv");
        std::fs::write(
            &csv,
            "seg_00000.mp4,0.000000,60.000000\nseg_00001.mp4,60.000000,120.000000\n",
        )
        .unwrap();

        let segments = parse_segment_csv(&csv, tmp.path());
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].filename, "seg_00000.mp4");
        assert!((segments[0].start_secs - 0.0).abs() < 0.001);
        assert!((segments[0].end_secs - 60.0).abs() < 0.001);
        assert_eq!(segments[1].filename, "seg_00001.mp4");
        assert!((segments[1].start_secs - 60.0).abs() < 0.001);
    }

    #[test]
    fn parse_segment_csv_missing_file() {
        let segments = parse_segment_csv(Path::new("/nonexistent/segments.csv"), Path::new("/tmp"));
        assert!(segments.is_empty());
    }

    #[test]
    fn parse_segment_csv_empty_discovers_files() {
        let tmp = tempfile::tempdir().unwrap();
        let csv = tmp.path().join("segments.csv");
        std::fs::write(&csv, "").unwrap(); // empty CSV
        std::fs::write(tmp.path().join("seg_00000.mp4"), b"fakedata").unwrap();
        std::fs::write(tmp.path().join("seg_00001.mp4"), b"fakedata").unwrap();

        let segments = parse_segment_csv(&csv, tmp.path());
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].filename, "seg_00000.mp4");
        assert_eq!(segments[1].filename, "seg_00001.mp4");
        assert!(segments[0].end_secs > 0.0);
    }

    #[test]
    fn recording_dir_has_playable_segments_rejects_manifest_only_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("manifest.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("segments.csv"), "").unwrap();

        assert!(!recording_dir_has_playable_segments(tmp.path()));
    }

    #[test]
    fn recording_dir_has_playable_segments_requires_non_empty_segment_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("segments.csv"),
            "seg_00000.mp4,0.000000,1.000000\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("seg_00000.mp4"), b"").unwrap();

        assert!(!recording_dir_has_playable_segments(tmp.path()));

        std::fs::write(tmp.path().join("seg_00000.mp4"), b"fakedata").unwrap();
        assert!(recording_dir_has_playable_segments(tmp.path()));
    }

    #[test]
    fn registry_new_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        assert!(!reg.is_enabled());
        assert!(reg.active_streams().is_empty());
        assert!(reg.all_streams().is_empty());
    }

    #[tokio::test]
    async fn browser_recording_rejects_private_and_absent_displays_before_ffmpeg() {
        let tmp = tempfile::tempdir().unwrap();
        let recordings = std::sync::Arc::new(tokio::sync::RwLock::new(RecordingRegistry::new(
            tmp.path(),
            RecordingConfig::default(),
        )));
        let sessions = std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let private = std::sync::Arc::new(crate::display::DisplaySession::new(
            9,
            std::sync::Arc::new(crate::display::synthetic::SyntheticBackend::new()),
        ));
        private.set_agent_visible(false);
        sessions.write().await.insert(9, private);

        for display_id in [9, 10] {
            let error =
                start_display_auto(&recordings, Some(&sessions), display_id, &EventBus::new())
                    .await
                    .expect_err("private and absent displays must fail before ffmpeg is consulted");
            assert!(error.contains("not an active agent-visible session"));
            assert!(error.contains("private views cannot be recorded"));
        }
        assert!(recordings.read().await.active_streams().is_empty());
    }

    #[tokio::test]
    async fn recording_feed_is_bound_to_visible_registry_identity() {
        let sessions = std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let original = std::sync::Arc::new(crate::display::DisplaySession::new(
            11,
            std::sync::Arc::new(crate::display::synthetic::SyntheticBackend::new()),
        ));
        sessions
            .write()
            .await
            .insert(11, std::sync::Arc::clone(&original));
        assert!(recording_session_remains_authorized(&sessions, 11, &original).await);

        let private_replacement = std::sync::Arc::new(crate::display::DisplaySession::new(
            11,
            std::sync::Arc::new(crate::display::synthetic::SyntheticBackend::new()),
        ));
        private_replacement.set_agent_visible(false);
        sessions.write().await.insert(11, private_replacement);
        assert!(
            !recording_session_remains_authorized(&sessions, 11, &original).await,
            "a private reopen under the same id must stop the old recording"
        );

        let visible_replacement = std::sync::Arc::new(crate::display::DisplaySession::new(
            11,
            std::sync::Arc::new(crate::display::synthetic::SyntheticBackend::new()),
        ));
        sessions
            .write()
            .await
            .insert(11, std::sync::Arc::clone(&visible_replacement));
        assert!(
            !recording_session_remains_authorized(&sessions, 11, &original).await,
            "display-id reuse must not transfer an old recording to a new session"
        );
        assert!(recording_session_remains_authorized(&sessions, 11, &visible_replacement).await);
    }

    #[test]
    fn registry_seek_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        assert!(reg.seek("display_99", 10.0).is_none());
    }

    #[test]
    fn registry_seek_with_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path().join("recordings").join("display_99");
        std::fs::create_dir_all(&stream_dir).unwrap();
        // Write segment files so they exist
        std::fs::write(stream_dir.join("seg_00000.mp4"), b"fake").unwrap();
        std::fs::write(stream_dir.join("seg_00001.mp4"), b"fake").unwrap();
        // Write segment CSV
        std::fs::write(
            stream_dir.join("segments.csv"),
            "seg_00000.mp4,0.000000,60.000000\nseg_00001.mp4,60.000000,120.000000\n",
        )
        .unwrap();

        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());

        // Seek within first segment
        let result = reg.seek("display_99", 30.0).unwrap();
        assert!(result.segment_path.ends_with("seg_00000.mp4"));
        assert!((result.offset_secs - 30.0).abs() < 0.001);

        // Seek within second segment
        let result = reg.seek("display_99", 90.0).unwrap();
        assert!(result.segment_path.ends_with("seg_00001.mp4"));
        assert!((result.offset_secs - 30.0).abs() < 0.001);
    }

    #[test]
    fn registry_all_streams_skips_empty_stopped_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let rec_dir = tmp.path().join("recordings");
        let display_dir = rec_dir.join("display_99");
        std::fs::create_dir_all(&display_dir).unwrap();
        std::fs::write(
            display_dir.join("segments.csv"),
            "seg_00000.mp4,0.000000,1.000000\n",
        )
        .unwrap();
        std::fs::write(display_dir.join("seg_00000.mp4"), b"fakedata").unwrap();
        std::fs::create_dir_all(rec_dir.join("cam0")).unwrap();

        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        let streams = reg.all_streams();
        assert_eq!(streams, vec!["display_99"]);
    }

    #[test]
    fn is_recording_returns_false_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        assert!(!reg.is_recording("display_99"));
    }

    #[test]
    fn pick_next_display_stream_name_returns_base_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let name = pick_next_display_stream_name(tmp.path(), 0);
        assert_eq!(name, "display_0");
    }

    #[test]
    fn pick_next_display_stream_name_increments_past_existing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("display_0")).unwrap();
        assert_eq!(pick_next_display_stream_name(tmp.path(), 0), "display_0_2");

        std::fs::create_dir_all(tmp.path().join("display_0_2")).unwrap();
        assert_eq!(pick_next_display_stream_name(tmp.path(), 0), "display_0_3");

        // Unrelated display still starts from base.
        assert_eq!(pick_next_display_stream_name(tmp.path(), 1), "display_1");
    }
}
