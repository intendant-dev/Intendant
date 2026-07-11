//! Media and recording assets over the control channel: media frame
//! registration, presence video frames, annotation and clip uploads,
//! recordings and their assets, and session frame assets.

use super::*;

// The recording/frame asset content core — the RecordingAsset vocabulary,
// its resolvers (including the live-registry resolution the legacy
// /recordings* chain shares since S8), safety predicates, and range
// readers — moved verbatim to web_gateway::session_catalog
// (transport-unification S4b/S8): both lanes resolve assets through one
// core; this module keeps the tunnel's ranged byte-stream carriage.
pub(crate) use crate::web_gateway::{
    read_frame_asset_file_range, read_recording_asset_bytes_range, read_recording_asset_file_range,
    recording_asset_name_is_safe, recording_stream_name_is_safe,
    resolve_live_recording_asset_in_daemon_dir, resolve_session_recording_asset,
    session_frame_filename_is_safe, RecordingAsset,
};

pub(crate) fn media_http_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> serde_json::Value {
    http_body_response(id, status, body.to_string(), "dashboard media")
}

pub(crate) fn media_error_task_response(
    id: String,
    status: u16,
    error: impl Into<String>,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: media_http_response(
            id,
            status,
            serde_json::json!({
                "ok": false,
                "error": error.into(),
            }),
        ),
        byte_stream: None,
        done: true,
    }
}

pub(crate) fn media_task_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: media_http_response(id, status, body),
        byte_stream: None,
        done: true,
    }
}

pub(crate) fn read_inbound_upload_bytes(
    upload: &mut InboundUploadState,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(upload.received_bytes);
    upload
        .tmp
        .as_file_mut()
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|e| format!("seek upload tempfile: {e}"))?;
    upload
        .tmp
        .as_file_mut()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read upload tempfile: {e}"))?;
    if bytes.len() != upload.received_bytes {
        return Err(format!(
            "upload byte count changed while committing: expected {}, got {}",
            upload.received_bytes,
            bytes.len()
        ));
    }
    Ok(bytes)
}

pub(crate) async fn dashboard_media_session_handles(
    runtime: &ControlRuntime,
) -> (
    Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    Option<crate::web_gateway::WebQueryCtx>,
) {
    let session = runtime.shared_session.read().await;
    (session.frame_registry.clone(), session.query_ctx.clone())
}

// The media store core — frame registration, presence-video
// register+record, annotation/clip context injection, and the clip
// operation type — moved verbatim to web_gateway::media_store
// (transport-unification S8): the /ws media twins commit through the
// same fns; this module keeps the tunnel's upload-frame carriage and
// response shapes.
pub(crate) use crate::web_gateway::{
    inject_annotation_context, inject_clip_context, register_dashboard_media_frame,
};

pub(crate) async fn api_presence_video_frame_upload_task_response(
    id: String,
    mut upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let frame_id = string_param(&params, &["frame_id", "frameId"]);
    if frame_id.is_empty() {
        return media_error_task_response(id, 400, "missing frame_id");
    }
    let stream = optional_string_param(&params, &["stream", "stream_name", "streamName"])
        .unwrap_or_else(|| "cam0".to_string());
    let bytes = match read_inbound_upload_bytes(&mut upload) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return media_error_task_response(id, 400, "empty video frame upload"),
        Err(e) => return media_error_task_response(id, 500, e),
    };
    let (registered, recorded) =
        register_dashboard_presence_video_frame(&runtime, &frame_id, &stream, &bytes).await;
    media_task_response(
        id,
        200,
        serde_json::json!({
            "t": "presence_video_frame_saved",
            "ok": true,
            "frame_id": frame_id,
            "stream": stream,
            "registered": registered,
            "recorded": recorded,
        }),
    )
}

/// Tunnel edge of the presence-video store op: read the registries off
/// the shared session, then commit through the neutral store fn the
/// `/ws` `video_frame` twin shares.
pub(crate) async fn register_dashboard_presence_video_frame(
    runtime: &ControlRuntime,
    frame_id: &str,
    stream: &str,
    jpeg_bytes: &[u8],
) -> (bool, bool) {
    let session = runtime.shared_session.read().await;
    let frame_registry = session.frame_registry.clone();
    let recording_registry = session.recording_registry.clone();
    drop(session);
    crate::web_gateway::register_presence_video_frame(
        frame_registry,
        recording_registry,
        &runtime.bus,
        frame_id,
        stream,
        jpeg_bytes,
    )
    .await
}

pub(crate) async fn api_media_annotation_upload_task_response(
    id: String,
    mut upload: InboundUploadState,
    runtime: ControlRuntime,
    submit: bool,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let frame_id = string_param(&params, &["frame_id", "frameId"]);
    if frame_id.is_empty() {
        return media_error_task_response(id, 400, "missing frame_id");
    }
    let stream = optional_string_param(&params, &["stream"]).unwrap_or_else(|| "annotation".into());
    let note = optional_string_param(&params, &["note"]).unwrap_or_default();
    let inject = params
        .get("inject")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let bytes = match read_inbound_upload_bytes(&mut upload) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return media_error_task_response(id, 400, "empty media upload"),
        Err(e) => return media_error_task_response(id, 500, e),
    };
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let (registry, query_ctx) = dashboard_media_session_handles(&runtime).await;
    let (saved_path, registered) = register_dashboard_media_frame(
        registry,
        &frame_id,
        &stream,
        if note.is_empty() {
            None
        } else {
            Some(note.clone())
        },
        &bytes,
        if submit {
            "annotation"
        } else {
            "annotation_attach"
        },
    )
    .await;

    if submit {
        let injected_to_queue =
            inject && inject_annotation_context(query_ctx.as_ref(), &note, data_b64);
        let status_label = if inject {
            if injected_to_queue {
                " (sent to agent)"
            } else {
                " (saved - no agent connected)"
            }
        } else {
            ""
        };
        runtime.bus.send(AppEvent::PresenceLog {
            message: format!("[annotation] {frame_id} on {stream}{status_label}"),
            level: Some(LogLevel::Info),
            turn: None,
        });
        media_task_response(
            id,
            200,
            serde_json::json!({
                "t": "annotation_saved",
                "ok": registered,
                "frame_id": frame_id,
                "stream": stream,
                "path": saved_path,
                "injected": injected_to_queue,
            }),
        )
    } else {
        runtime.bus.send(AppEvent::PresenceLog {
            message: format!("[annotation] {frame_id} attached (pending)"),
            level: Some(LogLevel::Info),
            turn: None,
        });
        media_task_response(
            id,
            200,
            serde_json::json!({
                "t": "annotation_attached",
                "ok": registered,
                "frame_id": frame_id,
                "stream": stream,
                "path": saved_path,
                "note": note,
            }),
        )
    }
}

pub(crate) fn f64_param(params: &serde_json::Value, name: &str, default: f64) -> f64 {
    params
        .get(name)
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
        })
        .unwrap_or(default)
}

pub(crate) fn usize_param(params: &serde_json::Value, name: &str, default: usize) -> usize {
    params
        .get(name)
        .and_then(|value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .or_else(|| value.as_str().and_then(|text| text.parse::<usize>().ok()))
        })
        .unwrap_or(default)
}

pub(crate) async fn api_media_clip_start_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    if clip_id.is_empty() {
        return media_http_response(
            id,
            400,
            serde_json::json!({"ok": false, "error": "missing clip_id"}),
        );
    }
    let total_frames = usize_param(&params, "total_frames", 0);
    if total_frames > DASHBOARD_MEDIA_CLIP_MAX_FRAMES {
        return media_http_response(
            id,
            413,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "clip has {total_frames} frames; cap is {DASHBOARD_MEDIA_CLIP_MAX_FRAMES}"
                ),
            }),
        );
    }
    let fps = usize_param(&params, "fps", 2).max(1) as u32;
    let op = DashboardMediaClipOperation {
        stream: optional_string_param(&params, &["stream"]).unwrap_or_else(|| "recording".into()),
        note: optional_string_param(&params, &["note"]).unwrap_or_default(),
        inject: params
            .get("inject")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        in_secs: f64_param(&params, "in_secs", 0.0),
        out_secs: f64_param(&params, "out_secs", 0.0),
        fps,
        expected_frames: total_frames,
        frames: Vec::with_capacity(total_frames),
    };
    let mut ops = runtime.media_clip_ops.lock().await;
    if ops.contains_key(&clip_id) {
        return media_http_response(
            id,
            409,
            serde_json::json!({"ok": false, "error": "clip operation already exists"}),
        );
    }
    ops.insert(clip_id.clone(), op);
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!("[clip] started {clip_id} ({total_frames} frames, {fps}fps)"),
        level: Some(LogLevel::Debug),
        turn: None,
    });
    media_http_response(
        id,
        200,
        serde_json::json!({
            "t": "media_clip_started",
            "ok": true,
            "op_id": clip_id,
            "clip_id": clip_id,
            "expected_frames": total_frames,
        }),
    )
}

pub(crate) async fn api_media_clip_frame_upload_task_response(
    id: String,
    mut upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let params = upload.params.clone();
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    let frame_id = string_param(&params, &["frame_id", "frameId"]);
    if clip_id.is_empty() {
        return media_error_task_response(id, 400, "missing clip_id");
    }
    if frame_id.is_empty() {
        return media_error_task_response(id, 400, "missing frame_id");
    }
    let bytes = match read_inbound_upload_bytes(&mut upload) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => return media_error_task_response(id, 400, "empty media upload"),
        Err(e) => return media_error_task_response(id, 500, e),
    };
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let requested_index = usize_param(&params, "frame_index", usize::MAX);
    let frames_received = {
        let mut ops = runtime.media_clip_ops.lock().await;
        let Some(op) = ops.get_mut(&clip_id) else {
            return media_error_task_response(id, 404, "unknown clip operation");
        };
        let next_index = op.frames.len();
        if requested_index != usize::MAX && requested_index != next_index {
            return media_error_task_response(
                id,
                409,
                format!("clip frame index mismatch: expected {next_index}, got {requested_index}"),
            );
        }
        if op.expected_frames > 0 && next_index >= op.expected_frames {
            return media_error_task_response(id, 409, "clip frame count exceeded");
        }
        op.frames.push((frame_id.clone(), data_b64));
        op.frames.len()
    };
    let (registry, _) = dashboard_media_session_handles(&runtime).await;
    let (_, registered) = register_dashboard_media_frame(
        registry,
        &frame_id,
        &format!("clip:{clip_id}"),
        None,
        &bytes,
        "clip",
    )
    .await;
    media_task_response(
        id,
        200,
        serde_json::json!({
            "t": "media_clip_frame_saved",
            "ok": true,
            "registered": registered,
            "op_id": clip_id,
            "clip_id": clip_id,
            "frame_id": frame_id,
            "frames_received": frames_received,
        }),
    )
}

pub(crate) async fn api_media_clip_end_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    if clip_id.is_empty() {
        return media_http_response(
            id,
            400,
            serde_json::json!({"ok": false, "error": "missing clip_id"}),
        );
    }
    let frames_sent = usize_param(&params, "frames_sent", usize::MAX);
    let clip = {
        let mut ops = runtime.media_clip_ops.lock().await;
        let Some(op) = ops.get(&clip_id) else {
            return media_http_response(
                id,
                404,
                serde_json::json!({"ok": false, "error": "unknown clip operation"}),
            );
        };
        let frames_registered = op.frames.len();
        if frames_sent != usize::MAX && frames_sent != frames_registered {
            return media_http_response(
                id,
                409,
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "clip frame count mismatch: expected {frames_registered}, got {frames_sent}"
                    ),
                }),
            );
        }
        if op.expected_frames > 0 && op.expected_frames != frames_registered {
            return media_http_response(
                id,
                409,
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "clip incomplete: expected {}, got {}",
                        op.expected_frames, frames_registered
                    ),
                }),
            );
        }
        ops.remove(&clip_id).expect("clip op existed")
    };
    let (_, query_ctx) = dashboard_media_session_handles(runtime).await;
    let injected = clip.inject && inject_clip_context(query_ctx.as_ref(), &clip_id, &clip);
    let frames_registered = clip.frames.len();
    runtime.bus.send(AppEvent::PresenceLog {
        message: format!(
            "[clip] {clip_id} - {frames_registered} frames{}",
            if injected {
                " (sent to agent)"
            } else {
                " (saved)"
            }
        ),
        level: Some(LogLevel::Info),
        turn: None,
    });
    media_http_response(
        id,
        200,
        serde_json::json!({
            "t": "clip_saved",
            "ok": true,
            "op_id": clip_id,
            "clip_id": clip_id,
            "frames_registered": frames_registered,
            "injected": injected,
        }),
    )
}

pub(crate) async fn api_media_clip_cancel_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let clip_id = string_param(&params, &["clip_id", "clipId", "op_id", "opId"]);
    if clip_id.is_empty() {
        return media_http_response(
            id,
            400,
            serde_json::json!({"ok": false, "error": "missing clip_id"}),
        );
    }
    let existed = runtime
        .media_clip_ops
        .lock()
        .await
        .remove(&clip_id)
        .is_some();
    media_http_response(
        id,
        200,
        serde_json::json!({
            "t": "media_clip_cancelled",
            "ok": true,
            "op_id": clip_id,
            "clip_id": clip_id,
            "existed": existed,
        }),
    )
}

pub(crate) async fn api_recordings_response(
    id: String,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    // Transport edge: resolve the real daemon recordings dir once; the
    // parity fixture drives the `_in_daemon_dir` variant with an
    // injected tempdir.
    api_recordings_response_in_daemon_dir(id, runtime, &crate::debug::daemon_recordings_dir()).await
}

pub(crate) async fn api_recordings_response_in_daemon_dir(
    id: String,
    runtime: &ControlRuntime,
    daemon_dir: &std::path::Path,
) -> serde_json::Value {
    let recording_registry = active_recording_registry(runtime).await;
    // Body-only framing (this method predates the injected-status
    // envelope); the neutral fn always answers 200 with the listing.
    frame_api_json_body_response(
        id,
        crate::web_gateway::recordings_list_api_response_in_daemon_dir(
            recording_registry,
            daemon_dir,
        )
        .await,
        "recordings",
    )
}

pub(crate) async fn api_session_recordings_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    // Transport edge: resolve the real home once; the parity fixtures
    // drive the `_from_home` variant with an injected temp home.
    api_session_recordings_response_from_home(id, params, &crate::platform::home_dir()).await
}

pub(crate) async fn api_session_recordings_response_from_home(
    id: String,
    params: Option<&serde_json::Value>,
    home: &std::path::Path,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    frame_api_response(
        id,
        crate::web_gateway::session_recordings_api_response(home, &session_id),
        "session recordings",
    )
}

pub(crate) async fn api_recording_asset_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    // Transport edge: resolve the real daemon recordings dir once; the
    // parity fixture drives the `_in_daemon_dir` variant with an
    // injected tempdir.
    api_recording_asset_task_response_in_daemon_dir(
        id,
        params,
        runtime,
        &crate::debug::daemon_recordings_dir(),
    )
    .await
}

pub(crate) async fn api_recording_asset_task_response_in_daemon_dir(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
    daemon_dir: &std::path::Path,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let (stream_name, asset, offset, length) = match recording_asset_request_params(&params) {
        Ok(params) => params,
        Err((status, error)) => {
            return recording_asset_error_task_response(id, status, error);
        }
    };
    let Some(registry) = active_recording_registry(runtime).await else {
        return recording_asset_error_task_response(
            id,
            404,
            serde_json::json!({ "ok": false, "error": "recording registry unavailable" }),
        );
    };
    let resolved =
        resolve_live_recording_asset_in_daemon_dir(registry, daemon_dir, &stream_name, &asset)
            .await;
    recording_asset_task_response(id, stream_name, asset, offset, length, resolved).await
}

pub(crate) async fn api_session_recording_asset_task_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> ControlTaskResponse {
    // Transport edge: resolve the real home once; the parity fixture
    // drives the `_from_home` variant with an injected temp home.
    api_session_recording_asset_task_response_from_home(id, params, &crate::platform::home_dir())
        .await
}

pub(crate) async fn api_session_recording_asset_task_response_from_home(
    id: String,
    params: Option<&serde_json::Value>,
    home: &std::path::Path,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
        return recording_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "invalid session id" }),
        );
    }
    let (stream_name, asset, offset, length) = match recording_asset_request_params(&params) {
        Ok(params) => params,
        Err((status, error)) => {
            return recording_asset_error_task_response(id, status, error);
        }
    };
    let session_dir = crate::web_gateway::resolve_bare_session_dir_from_home(home, &session_id);
    let resolved = resolve_session_recording_asset(session_dir, &stream_name, &asset);
    recording_asset_task_response(id, stream_name, asset, offset, length, resolved).await
}

pub(crate) fn recording_asset_request_params(
    params: &serde_json::Value,
) -> Result<(String, String, u64, Option<u64>), (u16, serde_json::Value)> {
    let Some(stream_name) = optional_string_param(params, &["stream_name", "streamName", "stream"])
    else {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "missing stream_name" }),
        ));
    };
    if !recording_stream_name_is_safe(&stream_name) {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "invalid stream_name" }),
        ));
    }
    let Some(asset) = optional_string_param(params, &["asset", "filename", "path"]) else {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "missing recording asset" }),
        ));
    };
    if !recording_asset_name_is_safe(&asset) {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "invalid recording asset" }),
        ));
    }
    let offset = optional_u64_param(params, &["offset", "start"])
        .map_err(|error| (400, serde_json::json!({ "ok": false, "error": error })))?
        .unwrap_or(0);
    let length = optional_u64_param(params, &["length", "limit"])
        .map_err(|error| (400, serde_json::json!({ "ok": false, "error": error })))?;
    Ok((stream_name, asset, offset, length))
}

pub(crate) async fn recording_asset_task_response(
    id: String,
    stream_name: String,
    asset_name: String,
    offset: u64,
    length: Option<u64>,
    resolved: Result<RecordingAsset, (u16, serde_json::Value)>,
) -> ControlTaskResponse {
    let resolved_asset = match resolved {
        Ok(asset) => asset,
        Err((status, body)) => return recording_asset_error_task_response(id, status, body),
    };
    let read_result = match resolved_asset {
        RecordingAsset::Bytes {
            bytes,
            content_type,
            filename,
        } => {
            tokio::task::spawn_blocking(move || {
                read_recording_asset_bytes_range(bytes, offset, length).map(
                    |(bytes, total_size, end)| (bytes, total_size, end, content_type, filename),
                )
            })
            .await
        }
        RecordingAsset::File {
            path,
            content_type,
            filename,
        } => {
            tokio::task::spawn_blocking(move || {
                read_recording_asset_file_range(&path, offset, length).map(
                    |(bytes, total_size, end)| (bytes, total_size, end, content_type, filename),
                )
            })
            .await
        }
    };
    let (bytes, total_size, end, content_type, filename) = match read_result {
        Ok(Ok(value)) => value,
        Ok(Err((status, body))) => return recording_asset_error_task_response(id, status, body),
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("recording asset task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let size = bytes.len();
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:recording:{stream_name}:{asset_name}"),
            content_type: content_type.to_string(),
            filename: Some(filename.clone()),
            bytes,
            result: serde_json::json!({
                "ok": true,
                "stream_name": stream_name,
                "asset": asset_name,
                "filename": filename,
                "content_type": content_type,
                "size": size,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
            }),
        }),
        done: true,
    }
}

pub(crate) fn recording_asset_error_task_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: http_body_response(id, status, body.to_string(), "recording asset"),
        byte_stream: None,
        done: true,
    }
}

pub(crate) async fn api_session_frame_asset_task_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> ControlTaskResponse {
    // Transport edge: resolve the real home once; the RPC fixture drives
    // the `_from_home` variant with an injected temp home.
    api_session_frame_asset_task_response_from_home(id, params, &crate::platform::home_dir()).await
}

pub(crate) async fn api_session_frame_asset_task_response_from_home(
    id: String,
    params: Option<&serde_json::Value>,
    home: &std::path::Path,
) -> ControlTaskResponse {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let session_id = string_param(&params, &["session_id", "sessionId", "id"]);
    if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
        return session_frame_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "invalid session id" }),
        );
    }
    let Some(filename) = optional_string_param(&params, &["filename", "frame", "asset", "name"])
    else {
        return session_frame_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "missing frame filename" }),
        );
    };
    if !session_frame_filename_is_safe(&filename) {
        return session_frame_asset_error_task_response(
            id,
            400,
            serde_json::json!({ "ok": false, "error": "invalid frame filename" }),
        );
    }
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            return session_frame_asset_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(value) => value,
        Err(error) => {
            return session_frame_asset_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };

    let Some(session_dir) =
        crate::web_gateway::resolve_bare_session_dir_from_home(home, &session_id)
    else {
        return session_frame_asset_error_task_response(
            id,
            404,
            serde_json::json!({ "ok": false, "error": "session not found" }),
        );
    };
    let path = session_dir.join("frames").join(&filename);
    if !path.exists() {
        return session_frame_asset_error_task_response(
            id,
            404,
            serde_json::json!({ "ok": false, "error": "frame not found" }),
        );
    }
    let content_type = if filename.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };
    let read_result =
        tokio::task::spawn_blocking(move || read_frame_asset_file_range(&path, offset, length))
            .await;
    let (bytes, total_size, end) = match read_result {
        Ok(Ok(value)) => value,
        Ok(Err((status, body))) => {
            return session_frame_asset_error_task_response(id, status, body)
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("session frame task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let size = bytes.len();
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:session-frame:{session_id}:{filename}"),
            content_type: content_type.to_string(),
            filename: Some(filename.clone()),
            bytes,
            result: serde_json::json!({
                "ok": true,
                "session_id": session_id,
                "filename": filename,
                "content_type": content_type,
                "size": size,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
            }),
        }),
        done: true,
    }
}

pub(crate) fn session_frame_asset_error_task_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: http_body_response(id, status, body.to_string(), "session frame asset"),
        byte_stream: None,
        done: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recordings fixture under an injected tempdir home's
    /// `.intendant/logs` store (both parity lanes take the same temp
    /// home, so the fixture never touches the machine's real
    /// `~/.intendant`; a fixed id is fine — each test owns its store).
    fn parity_recordings_fixture(prefix: &str) -> (tempfile::TempDir, String, std::path::PathBuf) {
        let home = tempfile::tempdir().expect("temp home");
        let session_id = prefix.to_string();
        let log_dir = crate::platform::intendant_home_in(home.path())
            .join("logs")
            .join(&session_id);
        let stream_dir = log_dir.join("recordings").join("screen");
        std::fs::create_dir_all(&stream_dir).unwrap();
        std::fs::write(stream_dir.join("seg_00001.mp4"), b"parity segment bytes").unwrap();
        std::fs::write(stream_dir.join("segments.csv"), "seg_00001.mp4,0.0,2.0\n").unwrap();
        (home, session_id, log_dir)
    }

    fn parity_http_json_body(
        response: crate::web_gateway::ApiResponse,
    ) -> (u16, serde_json::Value) {
        let bytes = crate::web_gateway::api_response_http_bytes(
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        );
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header/body split");
        let head = String::from_utf8(bytes[..split].to_vec()).expect("ascii head");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("HTTP/1.1 "))
            .and_then(|line| line.split_whitespace().next())
            .and_then(|code| code.parse::<u16>().ok())
            .expect("status line");
        (
            status,
            serde_json::from_slice(&bytes[split + 4..]).expect("json body"),
        )
    }

    // ── S4b parity: recordings list + the listing-asset vocabulary ──

    #[tokio::test]
    async fn parity_session_recordings_list_shares_bodies_with_status_metadata() {
        let (home, session_id, _log_dir) = parity_recordings_fixture("parity-rec-list");
        let (status, http_body) = parity_http_json_body(
            crate::web_gateway::session_recordings_api_response(home.path(), &session_id),
        );
        assert_eq!(status, 200);
        let frame = api_session_recordings_response_from_home(
            "parity-rec-list".to_string(),
            Some(&serde_json::json!({ "session_id": session_id })),
            home.path(),
        )
        .await;
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], true);
        // The list body is a json ARRAY: the injected-status envelope
        // only decorates objects, so the array passes through untouched.
        assert!(frame["result"].is_array(), "{frame}");
        assert_eq!(frame["result"], http_body);

        // Invalid id: an object body — the injection appears and matches
        // the HTTP status. (Bare-id check before any store access; the
        // tunnel lane exercises the full public adapter.)
        let (status, http_body) = parity_http_json_body(
            crate::web_gateway::session_recordings_api_response(home.path(), ".."),
        );
        assert_eq!(status, 400);
        let frame = api_session_recordings_response(
            "parity-rec-list-invalid".to_string(),
            Some(&serde_json::json!({ "session_id": ".." })),
        )
        .await;
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(map.remove("_httpStatus"), Some(serde_json::json!(400)));
        assert_eq!(map.remove("_httpOk"), Some(serde_json::json!(false)));
        assert_eq!(result, http_body);
    }

    #[tokio::test]
    async fn parity_recording_listing_assets_share_bytes_on_both_transports() {
        let (home, session_id, _log_dir) = parity_recordings_fixture("parity-rec-assets");
        for asset in ["segments", "playlist.m3u8"] {
            // HTTP: the shared resolver under the canonical tail.
            let response = crate::web_gateway::session_recording_listing_asset_api_response(
                home.path(),
                &session_id,
                "screen",
                asset,
            );
            let (http_bytes, http_ct) = match response {
                crate::web_gateway::ApiResponse::Bytes {
                    bytes: crate::web_gateway::BytesPayload::InMemory(payload),
                    content_type,
                    ..
                } => (payload, content_type),
                _ => panic!("listing asset must ride the bytes lane"),
            };
            // Tunnel: the same asset vocabulary through the ranged
            // byte-stream carriage (offset 0, unbounded).
            let task = api_session_recording_asset_task_response_from_home(
                format!("parity-asset-{asset}"),
                Some(&serde_json::json!({
                    "session_id": session_id,
                    "stream_name": "screen",
                    "asset": asset,
                })),
                home.path(),
            )
            .await;
            let stream = task.byte_stream.expect("tunnel byte stream");
            assert_eq!(stream.bytes, http_bytes, "{asset}");
            assert_eq!(stream.content_type, http_ct, "{asset}");
            assert_eq!(stream.result["ok"], true);
            assert_eq!(stream.result["total_size"], http_bytes.len());
        }
    }

    // ── S8 parity: the LIVE (daemon-scoped) recordings lanes — the
    // legacy /recordings* chain shapes and the tunnel residue resolve
    // through one core over the same injected dirs (no ambient
    // daemon_recordings_dir read on either lane).

    /// A registry over a temp session dir holding one recorded stream
    /// ("screen"), plus an injected daemon recordings dir holding
    /// another ("daemon0") — segments csv + playable bytes each.
    fn parity_live_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>,
    ) {
        let session_dir = tempfile::tempdir().expect("temp session dir");
        let daemon_dir = tempfile::tempdir().expect("temp daemon dir");
        for (root, stream, bytes) in [
            (
                session_dir.path().join("recordings"),
                "screen",
                &b"live session segment bytes"[..],
            ),
            (
                daemon_dir.path().to_path_buf(),
                "daemon0",
                &b"daemon-scoped segment bytes"[..],
            ),
        ] {
            let stream_dir = root.join(stream);
            std::fs::create_dir_all(&stream_dir).unwrap();
            std::fs::write(stream_dir.join("seg_00001.mp4"), bytes).unwrap();
            std::fs::write(stream_dir.join("segments.csv"), "seg_00001.mp4,0.0,2.0\n").unwrap();
        }
        let registry = Arc::new(tokio::sync::RwLock::new(
            crate::recording::RecordingRegistry::new(
                session_dir.path(),
                crate::project::RecordingConfig::default(),
            ),
        ));
        (session_dir, daemon_dir, registry)
    }

    #[tokio::test]
    async fn parity_live_recordings_list_shares_bodies_across_lanes() {
        let (_session_dir, daemon_dir, registry) = parity_live_fixture();
        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.recording_registry = Some(registry.clone());
        }

        let (status, http_body) = parity_http_json_body(
            crate::web_gateway::recordings_list_api_response_in_daemon_dir(
                Some(registry),
                daemon_dir.path(),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(
            http_body.as_array().map(|entries| entries.len()),
            Some(2),
            "{http_body}"
        );

        let frame = api_recordings_response_in_daemon_dir(
            "parity-live-list".to_string(),
            &rt,
            daemon_dir.path(),
        )
        .await;
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], true);
        // Body-only framing over a json ARRAY — no status injection.
        assert_eq!(frame["result"], http_body);
    }

    #[tokio::test]
    async fn parity_live_recording_assets_share_bytes_across_lanes() {
        let (_session_dir, daemon_dir, registry) = parity_live_fixture();
        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.recording_registry = Some(registry.clone());
        }

        // "screen" resolves through the registry's session dir; "daemon0"
        // exercises the daemon-dir csv fallback — on both lanes.
        for stream_name in ["screen", "daemon0"] {
            for asset in ["segments", "playlist.m3u8", "seg_00001.mp4"] {
                let response = crate::web_gateway::live_recordings_path_api_response(
                    Some(registry.clone()),
                    daemon_dir.path(),
                    &format!("{stream_name}/{asset}"),
                )
                .await;
                let (http_bytes, http_ct) = match response {
                    crate::web_gateway::ApiResponse::Bytes {
                        status: 200,
                        bytes: crate::web_gateway::BytesPayload::InMemory(payload),
                        content_type,
                        ..
                    } => (payload, content_type),
                    other => panic!(
                        "{stream_name}/{asset} must ride the bytes lane 200: {:?}",
                        std::mem::discriminant(&other)
                    ),
                };

                let task = api_recording_asset_task_response_in_daemon_dir(
                    format!("parity-live-{stream_name}-{asset}"),
                    Some(&serde_json::json!({
                        "stream_name": stream_name,
                        "asset": asset,
                    })),
                    &rt,
                    daemon_dir.path(),
                )
                .await;
                let stream = task.byte_stream.expect("tunnel byte stream");
                assert_eq!(stream.bytes, http_bytes, "{stream_name}/{asset}");
                assert_eq!(stream.content_type, http_ct, "{stream_name}/{asset}");
                assert_eq!(stream.result["ok"], true, "{stream_name}/{asset}");
                assert_eq!(
                    stream.result["total_size"],
                    http_bytes.len(),
                    "{stream_name}/{asset}"
                );
            }
        }
    }

    use crate::dashboard_control::tests::{runtime, test_upload_state};

    #[tokio::test]
    async fn media_annotation_upload_registers_frame() {
        let session_dir = tempfile::tempdir().unwrap();
        let rt = runtime();
        let registry = Arc::new(tokio::sync::RwLock::new(crate::frames::FrameRegistry::new(
            session_dir.path(),
        )));
        {
            let mut session = rt.shared_session.write().await;
            session.frame_registry = Some(registry.clone());
        }
        let bytes = b"jpeg annotation bytes";
        let upload = test_upload_state(
            "api_media_annotation_submit",
            serde_json::json!({
                "frame_id": "ann-test-1",
                "stream": "annotation",
                "note": "look here",
                "inject": false,
            }),
            bytes,
        );

        let response =
            api_media_annotation_upload_task_response("ann1".into(), upload, rt.clone(), true)
                .await;

        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["t"], "annotation_saved");
        assert_eq!(response.frame["result"]["ok"], true);
        assert_eq!(response.frame["result"]["frame_id"], "ann-test-1");
        assert_eq!(response.frame["result"]["injected"], false);
        let stored = registry.read().await.read_hq("ann-test-1").unwrap();
        assert_eq!(stored, bytes);
    }

    #[tokio::test]
    async fn media_clip_operation_commits_ordered_frames() {
        let session_dir = tempfile::tempdir().unwrap();
        let rt = runtime();
        let registry = Arc::new(tokio::sync::RwLock::new(crate::frames::FrameRegistry::new(
            session_dir.path(),
        )));
        {
            let mut session = rt.shared_session.write().await;
            session.frame_registry = Some(registry.clone());
        }

        let start = api_media_clip_start_response(
            "clip-start".into(),
            Some(&serde_json::json!({
                "clip_id": "clip-test-1",
                "stream": "recording",
                "fps": 2,
                "total_frames": 1,
                "inject": false,
            })),
            &rt,
        )
        .await;
        assert_eq!(start["result"]["_httpStatus"], 200);
        assert_eq!(start["result"]["t"], "media_clip_started");

        let bytes = b"jpeg clip frame";
        let frame_upload = test_upload_state(
            "api_media_clip_frame",
            serde_json::json!({
                "clip_id": "clip-test-1",
                "frame_id": "clip-test-1-f000",
                "frame_index": 0,
            }),
            bytes,
        );
        let frame = api_media_clip_frame_upload_task_response(
            "clip-frame".into(),
            frame_upload,
            rt.clone(),
        )
        .await;
        assert_eq!(frame.frame["result"]["_httpStatus"], 200);
        assert_eq!(frame.frame["result"]["t"], "media_clip_frame_saved");
        assert_eq!(frame.frame["result"]["frames_received"], 1);
        assert_eq!(
            registry.read().await.read_hq("clip-test-1-f000").unwrap(),
            bytes
        );

        let end = api_media_clip_end_response(
            "clip-end".into(),
            Some(&serde_json::json!({
                "clip_id": "clip-test-1",
                "frames_sent": 1,
            })),
            &rt,
        )
        .await;
        assert_eq!(end["result"]["_httpStatus"], 200);
        assert_eq!(end["result"]["t"], "clip_saved");
        assert_eq!(end["result"]["frames_registered"], 1);
        assert_eq!(end["result"]["injected"], false);
    }

    #[tokio::test]
    async fn recording_rpcs_preserve_shapes_and_status() {
        let rt = runtime();

        // Injected daemon dir (hermetic — the ambient variant would scan
        // the machine's real ~/.intendant/recordings).
        let daemon_dir = tempfile::tempdir().unwrap();
        let recordings =
            api_recordings_response_in_daemon_dir("rec1".to_string(), &rt, daemon_dir.path()).await;
        assert_eq!(recordings["t"], "response");
        assert_eq!(recordings["ok"], true);
        assert!(recordings["result"].as_array().is_some());

        let invalid_session = api_session_recordings_response(
            "rec2".to_string(),
            Some(&serde_json::json!({ "session_id": "../bad" })),
        )
        .await;
        assert_eq!(invalid_session["t"], "response");
        assert_eq!(invalid_session["ok"], true);
        assert_eq!(invalid_session["result"]["error"], "invalid session id");
        assert_eq!(invalid_session["result"]["_httpStatus"], 400);
        assert_eq!(invalid_session["result"]["_httpOk"], false);

        let workspace_snapshot = api_browser_workspace_snapshot_response("bw1".to_string()).await;
        assert_eq!(workspace_snapshot["t"], "response");
        assert_eq!(workspace_snapshot["ok"], true);
        assert_eq!(
            workspace_snapshot["result"]["t"],
            "browser_workspace_snapshot"
        );
        assert!(workspace_snapshot["result"]["workspaces"]
            .as_array()
            .is_some());
    }

    #[tokio::test]
    async fn recording_asset_rpc_streams_segments_and_media_ranges() {
        let session_dir = tempfile::tempdir().unwrap();
        let stream_dir = session_dir.path().join("recordings").join("display_0");
        std::fs::create_dir_all(&stream_dir).unwrap();
        std::fs::write(
            stream_dir.join("segments.csv"),
            "seg_00000.mp4,0,1.25\nseg_00001.ts,1.25,2.00\n",
        )
        .unwrap();
        let media = b"recording segment bytes";
        std::fs::write(stream_dir.join("seg_00000.mp4"), media).unwrap();
        let ts_media = b"recording transport stream bytes";
        std::fs::write(stream_dir.join("seg_00001.ts"), ts_media).unwrap();

        let rt = runtime();
        {
            let mut session = rt.shared_session.write().await;
            session.recording_registry = Some(Arc::new(tokio::sync::RwLock::new(
                crate::recording::RecordingRegistry::new(
                    session_dir.path(),
                    crate::project::RecordingConfig::default(),
                ),
            )));
        }

        let segments = api_recording_asset_task_response(
            "rec-asset1".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "segments",
            })),
            &rt,
        )
        .await;
        assert!(segments.done);
        assert!(segments.byte_stream.is_some());
        let stream = segments.byte_stream.unwrap();
        assert_eq!(stream.content_type, "application/json");
        assert_eq!(stream.filename.as_deref(), Some("segments.json"));
        let json: serde_json::Value = serde_json::from_slice(&stream.bytes).unwrap();
        assert_eq!(json[0]["filename"], "seg_00000.mp4");
        assert_eq!(json[1]["filename"], "seg_00001.ts");
        assert_eq!(stream.result["stream_name"], "display_0");
        assert_eq!(stream.result["asset"], "segments");
        assert_eq!(stream.result["resumable"], true);

        let playlist = api_recording_asset_task_response(
            "rec-asset-playlist".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "playlist.m3u8",
            })),
            &rt,
        )
        .await;
        assert!(playlist.done);
        assert!(playlist.byte_stream.is_some());
        let stream = playlist.byte_stream.unwrap();
        assert_eq!(stream.content_type, "application/vnd.apple.mpegurl");
        assert_eq!(stream.filename.as_deref(), Some("playlist.m3u8"));
        let playlist_text = String::from_utf8(stream.bytes).unwrap();
        assert!(playlist_text.contains("#EXTM3U"));
        assert!(playlist_text.contains("seg_00001.ts"));

        let segment = api_recording_asset_task_response(
            "rec-asset2".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "seg_00000.mp4",
                "offset": 10,
                "length": 7,
            })),
            &rt,
        )
        .await;
        assert!(segment.done);
        assert!(segment.byte_stream.is_some());
        let stream = segment.byte_stream.unwrap();
        assert_eq!(
            stream.stream_id,
            "rec-asset2:recording:display_0:seg_00000.mp4"
        );
        assert_eq!(stream.content_type, "video/mp4");
        assert_eq!(stream.filename.as_deref(), Some("seg_00000.mp4"));
        assert_eq!(stream.bytes, b"segment");
        assert_eq!(stream.result["size"], 7);
        assert_eq!(stream.result["total_size"], media.len());
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 17);

        let ts_segment = api_recording_asset_task_response(
            "rec-asset-ts".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "seg_00001.ts",
                "offset": 10,
                "length": 9,
            })),
            &rt,
        )
        .await;
        assert!(ts_segment.done);
        assert!(ts_segment.byte_stream.is_some());
        let stream = ts_segment.byte_stream.unwrap();
        assert_eq!(stream.content_type, "video/mp2t");
        assert_eq!(stream.filename.as_deref(), Some("seg_00001.ts"));
        assert_eq!(stream.bytes, b"transport");
        assert_eq!(stream.result["size"], 9);
        assert_eq!(stream.result["total_size"], ts_media.len());
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 19);

        let invalid = api_recording_asset_task_response(
            "rec-asset3".to_string(),
            Some(&serde_json::json!({
                "stream_name": "display_0",
                "asset": "../seg_00000.mp4",
            })),
            &rt,
        )
        .await;
        assert!(invalid.done);
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["result"]["_httpStatus"], 400);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn session_frame_asset_rpc_streams_validated_frame_ranges() {
        // The RPC resolves sessions from an injected temp home's
        // `.intendant/logs` store (the `_from_home` variant; the public
        // adapter resolves the real home at the transport edge).
        let home = tempfile::tempdir().unwrap();
        let session_id = "dashboard-frame-test";
        let session_dir = crate::platform::intendant_home_in(home.path())
            .join("logs")
            .join(session_id);
        let frames_dir = session_dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let frame_bytes = b"dashboard frame bytes";
        std::fs::write(frames_dir.join("ann-test.png"), frame_bytes).unwrap();

        let response = api_session_frame_asset_task_response_from_home(
            "frame-asset1".to_string(),
            Some(&serde_json::json!({
                "session_id": session_id,
                "filename": "ann-test.png",
                "offset": 10,
                "length": 5,
            })),
            home.path(),
        )
        .await;

        assert!(response.done);
        assert!(response.byte_stream.is_some());
        let stream = response.byte_stream.unwrap();
        assert_eq!(stream.content_type, "image/png");
        assert_eq!(stream.filename.as_deref(), Some("ann-test.png"));
        assert_eq!(stream.bytes, b"frame");
        assert_eq!(
            stream.stream_id,
            format!("frame-asset1:session-frame:{session_id}:ann-test.png")
        );
        assert_eq!(stream.result["session_id"], session_id);
        assert_eq!(stream.result["filename"], "ann-test.png");
        assert_eq!(stream.result["size"], 5);
        assert_eq!(stream.result["total_size"], frame_bytes.len());
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 15);
        assert_eq!(stream.result["resumable"], true);

        let invalid = api_session_frame_asset_task_response(
            "frame-asset2".to_string(),
            Some(&serde_json::json!({
                "session_id": "current",
                "filename": "../ann-test.png",
            })),
        )
        .await;
        assert!(invalid.done);
        assert!(invalid.byte_stream.is_none());
        assert_eq!(invalid.frame["result"]["_httpStatus"], 400);
        assert_eq!(invalid.frame["result"]["_httpOk"], false);
    }
}
