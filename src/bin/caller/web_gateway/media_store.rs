//! Media/frame-registry store core shared across the media lanes
//! (transport-unification S8): HQ-frame registration, presence-video
//! register+record, and the annotation/clip context injections. The
//! datachannel tunnel's `api_media_*` residue handlers and their `/ws`
//! twins (`video_frame`, `annotation_attach/submit`, `clip_*`) commit
//! through these fns — the store behavior is written once; each lane
//! keeps its own wire framing, response shapes, and log strings.
//! Moved from `dashboard_control::api_media` (which re-exports them for
//! its callers); the clip-operation type moved from
//! `dashboard_control::mod` with its fields widened for the `/ws`
//! accumulator.

use std::sync::Arc;

pub(crate) const DASHBOARD_MEDIA_CLIP_MAX_FRAMES: usize = 1000;

/// Validate one required browser/client media identifier at a transport edge.
/// The frame registry repeats this check before every filesystem access; clip
/// ids use the same grammar so they are safe in maps, logs, and context labels.
pub(crate) fn validate_dashboard_media_id(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("missing {field}"));
    }
    crate::frames::validate_media_id(value).map_err(|err| format!("invalid {field}: {err}"))
}

/// Reject an invalid legacy `/ws` media identifier before it reaches a map,
/// log, context label, or the frame registry. The binary dashboard-control
/// lane returns the same validation error in its ordinary HTTP-shaped result.
pub(crate) fn reject_invalid_dashboard_media_id(
    direct_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    field: &str,
    value: &str,
) -> bool {
    let Err(error) = validate_dashboard_media_id(field, value) else {
        return false;
    };
    let _ = direct_tx.send(
        serde_json::json!({
            "t": "media_error",
            "ok": false,
            "error": error,
        })
        .to_string(),
    );
    true
}

/// Apply the same bounded allocation policy to the legacy `/ws` clip lane as
/// the binary dashboard-control lane.
pub(crate) fn validate_dashboard_clip_frame_count(total: u64) -> Result<usize, String> {
    if total > DASHBOARD_MEDIA_CLIP_MAX_FRAMES as u64 {
        return Err(format!(
            "clip has {total} frames; cap is {DASHBOARD_MEDIA_CLIP_MAX_FRAMES}"
        ));
    }
    usize::try_from(total)
        .map_err(|_| "clip frame count is too large for this platform".to_string())
}

/// One in-flight clip operation: `clip_start` metadata plus the
/// accumulated `(frame_id, base64_jpeg)` frames, injected into the agent
/// context at `clip_end` when requested. The tunnel keeps these in the
/// runtime-shared `media_clip_ops` map; the `/ws` lane accumulates
/// per-connection.
#[derive(Debug)]
pub(crate) struct DashboardMediaClipOperation {
    pub(crate) stream: String,
    pub(crate) note: String,
    pub(crate) inject: bool,
    pub(crate) in_secs: f64,
    pub(crate) out_secs: f64,
    pub(crate) fps: u32,
    pub(crate) expected_frames: usize,
    pub(crate) frames: Vec<(String, String)>,
}

/// Register one media frame (annotation, clip frame) in the HQ frame
/// registry. Returns the saved path and whether registration happened —
/// a missing registry degrades to `("", false)`, exactly as every lane
/// always treated it.
pub(crate) async fn register_dashboard_media_frame(
    registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    frame_id: &str,
    stream: &str,
    note: Option<String>,
    bytes: &[u8],
    log_label: &str,
) -> (String, bool) {
    let Some(registry) = registry else {
        return (String::new(), false);
    };
    let meta = presence_core::FrameMeta {
        frame_id: frame_id.to_string(),
        stream: stream.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        sent_to_live: false,
        live_resolution: None,
        hq_resolution: None,
        note,
    };
    let mut reg = registry.write().await;
    match reg.register(meta, bytes) {
        Ok(path) => (path.display().to_string(), true),
        Err(e) => {
            eprintln!("{log_label} frame registry write failed: {e}");
            (String::new(), false)
        }
    }
}

/// Register a presence/camera video frame in the HQ frame registry and
/// feed it to the recording pipeline (auto-starting the stream's
/// recorder on first frame when enabled and ffmpeg is present,
/// broadcasting `RecordingStarted`). Returns `(registered, recorded)`.
pub(crate) async fn register_presence_video_frame(
    frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    bus: &crate::event::EventBus,
    frame_id: &str,
    stream: &str,
    jpeg_bytes: &[u8],
) -> (bool, bool) {
    let mut registered = false;
    if let Some(registry) = frame_registry {
        let meta = presence_core::FrameMeta {
            frame_id: frame_id.to_string(),
            stream: stream.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            sent_to_live: true,
            live_resolution: Some("768x768".to_string()),
            hq_resolution: None,
            note: None,
        };
        let mut reg = registry.write().await;
        match reg.register(meta, jpeg_bytes) {
            Ok(_) => registered = true,
            Err(e) => eprintln!("presence video frame registry write failed: {e}"),
        }
    }

    let mut recorded = false;
    if let Some(registry) = recording_registry {
        let mut rec = registry.write().await;
        if rec.is_enabled() {
            if !rec.is_recording(stream) && crate::recording::is_ffmpeg_available() {
                match rec.start_stream(stream).await {
                    Ok(()) => {
                        bus.send(crate::event::AppEvent::RecordingStarted {
                            stream_name: stream.to_string(),
                        });
                    }
                    Err(e) => eprintln!("presence video recording start failed: {e}"),
                }
            }
            if let Err(e) = rec.feed_frame(stream, jpeg_bytes).await {
                eprintln!("presence video recording frame failed: {e}");
            } else {
                recorded = true;
            }
        }
    }

    (registered, recorded)
}

/// Queue a submitted annotation (note + jpeg) into the agent's context
/// injection queue. Returns whether the injection actually landed (no
/// presence/agent connected degrades to `false`).
pub(crate) fn inject_annotation_context(
    query_ctx: Option<&crate::web_gateway::WebQueryCtx>,
    note: &str,
    data_b64: String,
) -> bool {
    let Some(ctx) = query_ctx else {
        return false;
    };
    let Some(ciq) = ctx.context_injection.as_ref() else {
        return false;
    };
    let Ok(mut queue) = ciq.lock() else {
        return false;
    };
    let label = if note.is_empty() {
        "[User Annotation] User highlighted something on the screen.".to_string()
    } else {
        format!("[User Annotation] {note}")
    };
    queue.push(crate::event::ContextInjection {
        text: label,
        images: vec![crate::conversation::ImageData {
            media_type: "image/jpeg".to_string(),
            data: data_b64,
        }],
        source: crate::event::InjectionSource::User,
        target_session_id: None,
        steer_id: None,
    });
    true
}

/// Queue a completed clip (accumulated frames + metadata) into the
/// agent's context injection queue. Returns whether the injection
/// actually landed.
pub(crate) fn inject_clip_context(
    query_ctx: Option<&crate::web_gateway::WebQueryCtx>,
    _clip_id: &str,
    clip: &DashboardMediaClipOperation,
) -> bool {
    let Some(ctx) = query_ctx else {
        return false;
    };
    let Some(ciq) = ctx.context_injection.as_ref() else {
        return false;
    };
    let Ok(mut queue) = ciq.lock() else {
        return false;
    };
    let frames_registered = clip.frames.len();
    let label = if clip.note.is_empty() {
        format!(
            "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps)",
            clip.stream, clip.in_secs, clip.out_secs, frames_registered, clip.fps,
        )
    } else {
        format!(
            "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps). {}",
            clip.stream, clip.in_secs, clip.out_secs, frames_registered, clip.fps, clip.note,
        )
    };
    let images = clip
        .frames
        .iter()
        .map(|(_, data)| crate::conversation::ImageData {
            media_type: "image/jpeg".to_string(),
            data: data.clone(),
        })
        .collect();
    queue.push(crate::event::ContextInjection {
        text: label,
        images,
        source: crate::event::InjectionSource::User,
        target_session_id: None,
        steer_id: None,
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_websocket_rejects_unsafe_media_identifiers() {
        let (direct_tx, mut direct_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(reject_invalid_dashboard_media_id(
            &direct_tx,
            "frame_id",
            "../escape"
        ));
        let response: serde_json::Value =
            serde_json::from_str(&direct_rx.try_recv().unwrap()).unwrap();
        assert_eq!(response["t"], "media_error");
        assert_eq!(response["ok"], false);
        assert_eq!(
            response["error"],
            "invalid frame_id: must contain only ASCII letters, digits, '_' or '-'"
        );

        assert!(!reject_invalid_dashboard_media_id(
            &direct_tx,
            "clip_id",
            "clip-display_0-1"
        ));
        assert!(direct_rx.try_recv().is_err());
    }

    #[test]
    fn legacy_clip_count_is_bounded_before_allocation() {
        assert_eq!(validate_dashboard_clip_frame_count(1000), Ok(1000));
        assert_eq!(
            validate_dashboard_clip_frame_count(u64::MAX),
            Err(format!(
                "clip has {} frames; cap is {}",
                u64::MAX,
                DASHBOARD_MEDIA_CLIP_MAX_FRAMES
            ))
        );
    }
}
