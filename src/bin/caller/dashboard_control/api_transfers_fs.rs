//! Transfer jobs and the scoped filesystem surface: job creation for
//! artifacts/reports/uploads/recordings/frames, chunked upload and
//! download IO, and the fs stat/list/mkdir/rename/delete/read/write
//! handlers with their range readers.

use super::*;

/// Resolve where transfer jobs persist for this daemon: the project store
/// when a project root exists, the daemon-global fallback store otherwise
/// (see `global_store.rs`). Infallible — projectless daemons are served,
/// not refused.
pub(crate) fn transfer_store_scope(runtime: &ControlRuntime) -> crate::global_store::StoreScope {
    crate::global_store::StoreScope::resolve_in(
        runtime.project_root.as_deref(),
        &runtime.state_root,
    )
}

// The job-handle alias reader moved to the neutral transfer core
// (`web_gateway::routes_transfers`) with the S9 conversion; the
// re-export keeps existing references compiling.
pub(crate) use crate::web_gateway::transfer_id_param;

pub(crate) fn transfer_store_task_error(
    error: tokio::task::JoinError,
    label: &str,
) -> crate::transfer_store::TransferStoreError {
    crate::transfer_store::TransferStoreError::new(500, format!("{label} task failed: {error}"))
}

pub(crate) fn transfer_json_error_message(body: &serde_json::Value) -> String {
    body.get("error")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| body.to_string())
}

pub(crate) fn transfer_artifact_type(artifact: &serde_json::Value) -> String {
    string_param(artifact, &["type", "kind", "source_kind", "sourceKind"]).to_ascii_lowercase()
}

pub(crate) async fn transfer_create_artifact_download_job(
    home: &std::path::Path,
    scope: crate::global_store::StoreScope,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    match transfer_artifact_type(&artifact).as_str() {
        "session_report" | "session-report" => {
            transfer_create_session_report_download_job(home, scope, artifact, runtime).await
        }
        "staged_upload" | "staged-upload" | "upload" => {
            transfer_create_staged_upload_download_job(scope, artifact, runtime).await
        }
        "recording_asset" | "recording-asset" => {
            transfer_create_recording_asset_download_job(home, scope, artifact, runtime, false)
                .await
        }
        "session_recording_asset" | "session-recording-asset" => {
            transfer_create_recording_asset_download_job(home, scope, artifact, runtime, true)
                .await
        }
        "session_frame_asset" | "session-frame-asset" | "frame_asset" | "frame-asset" => {
            transfer_create_session_frame_download_job(home, scope, artifact).await
        }
        "" => Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing artifact type",
        )),
        other => Err(crate::transfer_store::TransferStoreError::new(
            400,
            format!("unsupported transfer artifact type: {other}"),
        )),
    }
}

pub(crate) async fn transfer_create_session_report_download_job(
    home: &std::path::Path,
    scope: crate::global_store::StoreScope,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let session_id = optional_string_param(&artifact, &["session_id", "sessionId", "id"])
        .unwrap_or_else(|| "current".to_string());
    let (session_log, query_ctx) = {
        let session = runtime.shared_session.read().await;
        (session.session_log.clone(), session.query_ctx.clone())
    };
    let report = tokio::task::spawn_blocking({
        let session_id = session_id.clone();
        let home = home.to_path_buf();
        move || {
            crate::web_gateway::session_report_zip_for_request(
                &home,
                &session_id,
                session_log.as_ref(),
                query_ctx.as_ref(),
            )
        }
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "session report transfer"))?
    .map_err(|err| {
        let (status, message) = match err {
            crate::web_gateway::SessionReportZipError::InvalidSessionId => {
                (400, "invalid session id".to_string())
            }
            crate::web_gateway::SessionReportZipError::NotFound => {
                (404, "Session not found".to_string())
            }
            crate::web_gateway::SessionReportZipError::Build(error) => {
                (500, format!("Failed to build report: {error}"))
            }
        };
        crate::transfer_store::TransferStoreError::new(status, message)
    })?;
    tokio::task::spawn_blocking(move || {
        crate::transfer_store::create_download_job_from_bytes(
            &scope,
            report.bytes,
            &report.filename,
            "application/zip",
            "session_report",
            Some("Session report".to_string()),
            Some(artifact),
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "session report transfer"))?
}

pub(crate) async fn transfer_create_staged_upload_download_job(
    scope: crate::global_store::StoreScope,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let upload_id = transfer_id_param(&artifact);
    if upload_id.is_empty() {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing upload id",
        ));
    }
    let (upload_root, session_dir) = active_upload_handles(runtime)
        .await
        .map_err(|error| crate::transfer_store::TransferStoreError::new(500, error))?;
    // The staged upload may live under the active session's project store
    // (when one is set) rather than the daemon's own; without either, both
    // resolve to the daemon-global store.
    let upload_scope = match upload_root {
        Some(root) => crate::global_store::StoreScope::Project(root),
        None => scope.clone(),
    };
    let session_dir = session_dir
        .unwrap_or_else(|| crate::web_gateway::pending_upload_session_dir(&upload_scope));
    tokio::task::spawn_blocking(move || {
        let descriptor = crate::upload_store::find_upload(&upload_id, &session_dir, &upload_scope)
            .ok_or_else(|| {
                crate::transfer_store::TransferStoreError::new(404, "upload not found")
            })?;
        crate::transfer_store::create_download_job_from_path(
            &scope,
            descriptor.path.clone(),
            Some(descriptor.name.clone()),
            Some(descriptor.mime.clone()),
            Some("staged_upload".to_string()),
            Some(format!("Staged upload {}", descriptor.name)),
            Some(artifact),
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "staged upload transfer"))?
}

pub(crate) async fn transfer_create_recording_asset_download_job(
    home: &std::path::Path,
    scope: crate::global_store::StoreScope,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
    session_scoped: bool,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let stream_name = optional_string_param(&artifact, &["stream_name", "streamName", "stream"])
        .ok_or_else(|| {
            crate::transfer_store::TransferStoreError::new(400, "missing stream_name")
        })?;
    if !recording_stream_name_is_safe(&stream_name) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid stream_name",
        ));
    }
    let asset =
        optional_string_param(&artifact, &["asset", "filename", "path"]).ok_or_else(|| {
            crate::transfer_store::TransferStoreError::new(400, "missing recording asset")
        })?;
    if !recording_asset_name_is_safe(&asset) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid recording asset",
        ));
    }
    let resolved = if session_scoped {
        let session_id = string_param(&artifact, &["session_id", "sessionId", "id"]);
        if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
            return Err(crate::transfer_store::TransferStoreError::new(
                400,
                "invalid session id",
            ));
        }
        let session_dir = crate::web_gateway::resolve_bare_session_dir_from_home(home, &session_id);
        resolve_session_recording_asset(session_dir, &stream_name, &asset)
    } else {
        let Some(registry) = active_recording_registry(runtime).await else {
            return Err(crate::transfer_store::TransferStoreError::new(
                404,
                "recording registry unavailable",
            ));
        };
        // Transport edge: resolve the real daemon recordings dir here
        // (the shared core takes it injected since S8).
        resolve_live_recording_asset_in_daemon_dir(
            registry,
            &crate::debug::daemon_recordings_dir(),
            &stream_name,
            &asset,
        )
        .await
    }
    .map_err(|(status, body)| {
        crate::transfer_store::TransferStoreError::new(status, transfer_json_error_message(&body))
    })?;
    transfer_create_recording_asset_job(scope, artifact, stream_name, asset, resolved).await
}

pub(crate) async fn transfer_create_recording_asset_job(
    scope: crate::global_store::StoreScope,
    artifact: serde_json::Value,
    stream_name: String,
    asset: String,
    resolved: RecordingAsset,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    match resolved {
        RecordingAsset::Bytes {
            bytes,
            content_type,
            filename,
        } => tokio::task::spawn_blocking(move || {
            crate::transfer_store::create_download_job_from_bytes(
                &scope,
                bytes,
                &filename,
                content_type,
                "recording_asset",
                Some(format!("{stream_name} {asset}")),
                Some(artifact),
            )
        })
        .await
        .map_err(|e| transfer_store_task_error(e, "recording artifact transfer"))?,
        RecordingAsset::File {
            path,
            content_type,
            filename,
        } => tokio::task::spawn_blocking(move || {
            crate::transfer_store::create_download_job_from_path(
                &scope,
                path,
                Some(filename),
                Some(content_type.to_string()),
                Some("recording_asset".to_string()),
                Some(format!("{stream_name} {asset}")),
                Some(artifact),
            )
        })
        .await
        .map_err(|e| transfer_store_task_error(e, "recording artifact transfer"))?,
    }
}

pub(crate) async fn transfer_create_session_frame_download_job(
    home: &std::path::Path,
    scope: crate::global_store::StoreScope,
    artifact: serde_json::Value,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let session_id = string_param(&artifact, &["session_id", "sessionId", "id"]);
    if !crate::web_gateway::session_lookup_id_is_safe(&session_id) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid session id",
        ));
    }
    let filename = optional_string_param(&artifact, &["filename", "frame", "asset", "name"])
        .ok_or_else(|| {
            crate::transfer_store::TransferStoreError::new(400, "missing frame filename")
        })?;
    if !session_frame_filename_is_safe(&filename) {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "invalid frame filename",
        ));
    }
    let session_dir = crate::web_gateway::resolve_bare_session_dir_from_home(home, &session_id)
        .ok_or_else(|| crate::transfer_store::TransferStoreError::new(404, "session not found"))?;
    let path = session_dir.join("frames").join(&filename);
    if !path.exists() {
        return Err(crate::transfer_store::TransferStoreError::new(
            404,
            "frame not found",
        ));
    }
    let content_type = if filename.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };
    tokio::task::spawn_blocking(move || {
        crate::transfer_store::create_download_job_from_path(
            &scope,
            path,
            Some(filename.clone()),
            Some(content_type.to_string()),
            Some("session_frame_asset".to_string()),
            Some(format!("{session_id} {filename}")),
            Some(artifact),
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "session frame transfer"))?
}

pub(crate) async fn api_transfer_jobs_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    frame_api_response(
        id,
        crate::web_gateway::transfer_jobs_api_response(scope, &params).await,
        "transfer jobs",
    )
}

pub(crate) async fn api_transfer_job_create_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    // Transport edge: resolve the real home once; the artifact fixtures
    // drive the `_from_home` variant with an injected temp home.
    api_transfer_job_create_response_from_home(id, params, runtime, &crate::platform::home_dir())
        .await
}

pub(crate) async fn api_transfer_job_create_response_from_home(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
    home: &std::path::Path,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let response = match crate::web_gateway::classify_transfer_create(&params) {
        Err(response) => response,
        Ok(crate::web_gateway::TransferCreateRequest::Artifact(artifact)) => {
            // The runtime-coupled arm: artifact resolution reads live
            // session handles (report builders, staged uploads, the
            // recording registry) — the transport edge HTTP
            // deliberately lacks (divergence #24).
            crate::web_gateway::transfer_job_result_api_response(
                transfer_create_artifact_download_job(home, scope, artifact, runtime).await,
            )
        }
        Ok(crate::web_gateway::TransferCreateRequest::Path(kind)) => {
            crate::web_gateway::transfer_path_create_api_response(scope, params, kind).await
        }
    };
    frame_api_response(id, response, "transfer create")
}

pub(crate) async fn api_transfer_job_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    frame_api_response(
        id,
        crate::web_gateway::transfer_job_delete_api_response(scope, &params).await,
        "transfer delete",
    )
}

pub(crate) async fn api_transfer_upload_commit_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    frame_api_response(
        id,
        crate::web_gateway::transfer_upload_commit_api_response(scope, &params).await,
        "transfer upload commit",
    )
}

/// Terminal leg of a transfer chunk upload: the bytes arrived via
/// `upload_start`/`upload_chunk` frames (op-level authority checked at
/// `upload_start`; the destination was path-scoped at job create), and
/// the spool rides the same [`crate::web_gateway::SpooledBody`] handle
/// the HTTP chunk row streams into.
pub(crate) async fn api_transfer_upload_chunk_task_response(
    id: String,
    upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let scope = transfer_store_scope(&runtime);
    let (params, body) = upload.into_spooled_body();
    let frame = frame_api_response(
        id.clone(),
        crate::web_gateway::transfer_upload_chunk_api_response(scope, &params, body).await,
        "transfer upload chunk",
    );
    ControlTaskResponse {
        id,
        frame,
        byte_stream: None,
        done: true,
    }
}

pub(crate) async fn api_transfer_download_read_task_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> ControlTaskResponse {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    // Transport-owned range normalization: the tunnel's offset/length
    // param aliases become the neutral request's ByteRange, exactly as
    // the fs read wrapper normalizes (design §2.1).
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            return frame_api_task_response(
                id,
                crate::web_gateway::transfer_error_api_response(400, error),
                "transfer-download",
                "transfer download",
            );
        }
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(value) => value,
        Err(error) => {
            return frame_api_task_response(
                id,
                crate::web_gateway::transfer_error_api_response(400, error),
                "transfer-download",
                "transfer download",
            );
        }
    };
    let response = crate::web_gateway::transfer_download_read_api_response(
        scope,
        &params,
        crate::web_gateway::ByteRange::OffsetLength { offset, length },
    )
    .await;
    frame_api_task_response(id, response, "transfer-download", "transfer download")
}

/// Build the neutral [`crate::web_gateway::ApiRequest`] for a tunnel
/// method: the frame's `params` object rides verbatim (transport-
/// unification design §2.1 — the tunnel's shape is the canonical one).
fn control_api_request(
    params: Option<&serde_json::Value>,
    range: Option<crate::web_gateway::ByteRange>,
) -> crate::web_gateway::ApiRequest {
    crate::web_gateway::ApiRequest {
        params: params.cloned().unwrap_or_else(|| serde_json::json!({})),
        range,
    }
}

pub(crate) async fn api_fs_stat_response(id: String, params: Option<&serde_json::Value>) -> serde_json::Value {
    let request = control_api_request(params, None);
    let result =
        tokio::task::spawn_blocking(move || crate::web_gateway::fs_stat_api_response(&request))
            .await;
    match result {
        Ok(response) => frame_api_response(id, response, "filesystem stat"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("filesystem stat task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_fs_list_response(id: String, params: Option<&serde_json::Value>) -> serde_json::Value {
    let request = control_api_request(params, None);
    let result =
        tokio::task::spawn_blocking(move || crate::web_gateway::fs_list_api_response(&request))
            .await;
    match result {
        Ok(response) => frame_api_response(id, response, "filesystem list"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("filesystem list task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_fs_mkdir_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    // Transport-owned param decode (HTTP's twin decodes a typed serde
    // request); the historical string_param read carries over verbatim.
    let path = string_param(&params, &["path"]);
    frame_api_response(
        id,
        crate::web_gateway::fs_mkdir_api_response(&path),
        "filesystem mkdir",
    )
}

pub(crate) async fn api_fs_rename_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    // Transport-owned param decode: the historical lenient reads,
    // verbatim (the neutral fn owns the blocking apply leg).
    let from = params
        .and_then(|p| p.get("from"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let to = params
        .and_then(|p| p.get("to"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    frame_api_response(
        id,
        crate::web_gateway::fs_rename_api_response(from, to).await,
        "filesystem rename",
    )
}

pub(crate) async fn api_fs_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    // Transport-owned param decode: the historical lenient reads,
    // verbatim (the neutral fn owns the blocking apply leg).
    let path = params
        .and_then(|p| p.get("path"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let recursive = params
        .and_then(|p| p.get("recursive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    frame_api_response(
        id,
        crate::web_gateway::fs_delete_api_response(path, recursive).await,
        "filesystem delete",
    )
}

/// Terminal leg of an `api_fs_write` upload: the file contents arrived via
/// `upload_start`/`upload_chunk` frames (op-level authority checked at
/// `upload_start`); the path scope check runs here, where the params are
/// final, via the same `authorize_dashboard_control_method` gate a plain
/// request would pass through.
pub(crate) async fn api_fs_write_upload_task_response(
    id: String,
    upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    if let Err(error) =
        authorize_dashboard_control_method(&runtime, "api_fs_write", Some(&upload.params))
    {
        return ControlTaskResponse {
            id: id.clone(),
            frame: http_body_response(
                id,
                403,
                serde_json::json!({ "error": error }).to_string(),
                "filesystem write",
            ),
            byte_stream: None,
            done: true,
        };
    }
    // Drain the upload spool off the async runtime — the content
    // carriage is transport-owned (the tunnel's upload lane vs HTTP's
    // JSON content fields); the apply leg is the shared neutral seam.
    let read_result = tokio::task::spawn_blocking(move || {
        std::fs::read(upload.tmp.path()).map(|bytes| (upload.params, bytes))
    })
    .await;
    let frame = match read_result {
        Ok(Ok((params, bytes))) => frame_api_response(
            id.clone(),
            crate::web_gateway::fs_write_bytes_api_response(
                crate::web_gateway::fs_write_args_from_params(&params),
                bytes,
            )
            .await,
            "filesystem write",
        ),
        Ok(Err(e)) => http_body_response(
            id.clone(),
            500,
            serde_json::json!({
                "error": format!("could not read upload tempfile: {e}")
            })
            .to_string(),
            "filesystem write",
        ),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("filesystem write task failed: {e}"),
        }),
    };
    ControlTaskResponse {
        id,
        frame,
        byte_stream: None,
        done: true,
    }
}

pub(crate) async fn api_fs_read_task_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> ControlTaskResponse {
    // Transport-owned range normalization: the tunnel's offset/length
    // param aliases become the neutral request's ByteRange, exactly as
    // the HTTP shim lifts its `Range` header (design §2.1 — ranges
    // arrive pre-normalized, per transport).
    let offset = match params
        .map(|p| optional_u64_param(p, &["offset", "start"]))
        .unwrap_or(Ok(None))
    {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            return filesystem_read_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let length = match params
        .map(|p| optional_u64_param(p, &["length", "limit"]))
        .unwrap_or(Ok(None))
    {
        Ok(value) => value,
        Err(error) => {
            return filesystem_read_error_task_response(
                id,
                400,
                serde_json::json!({ "ok": false, "error": error }),
            );
        }
    };
    let request = control_api_request(
        params,
        Some(crate::web_gateway::ByteRange::OffsetLength { offset, length }),
    );
    let result =
        tokio::task::spawn_blocking(move || crate::web_gateway::fs_read_api_response(&request))
            .await;
    match result {
        Ok(response) => frame_api_task_response(id, response, "fs-read", "filesystem read"),
        Err(e) => ControlTaskResponse {
            id: id.clone(),
            frame: serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("filesystem read task failed: {e}"),
            }),
            byte_stream: None,
            done: true,
        },
    }
}

pub(crate) fn filesystem_read_error_task_response(
    id: String,
    status: u16,
    body: serde_json::Value,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: http_body_response(id, status, body.to_string(), "filesystem read"),
        byte_stream: None,
        done: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard_control::tests::{runtime, test_upload_state};

    // ── Tunnel/HTTP parity fixtures (transport-unification design §8) ──
    //
    // The S2 convergence claim: the same params through the ONE neutral
    // fn (`crate::web_gateway::fs_{stat,list,read}_api_response`),
    // rendered by the HTTP adapter (`api_response_http_bytes`) and by
    // the tunnel framer (`api_fs_*_response` → `frame_api_*response`),
    // yield IDENTICAL JSON result bodies. The envelope differences —
    // deliberate, historical, and pinned by these fixtures — are:
    //
    //  1. Frame envelope: the tunnel wraps the body as `{t:"response",
    //     id, ok:true, result:<body>}` and injects `_httpStatus` /
    //     `_httpOk` into the result object (`http_body_response`);
    //     HTTP sends the raw body with the status on the status line.
    //  2. Headers: HTTP decorates with `Content-Type`/`Content-Length`,
    //     the `Cache-Control`/`Connection` tail, and CORS per the
    //     route's posture; the tunnel has no header lane.
    //  3. fs_read success: both lanes carry the same payload bytes.
    //     HTTP renders the meta as headers (`X-Content-Sha256` on full
    //     reads, `Content-Range` + 206 on partials,
    //     `Content-Disposition`); the tunnel emits
    //     `byte_stream_start/chunk/end` with the meta object emitted
    //     verbatim as `byte_stream_end.result`.
    //  4. Range addressing: HTTP `Range: bytes=a-b` is end-inclusive;
    //     the tunnel's offset/length form reports an end-EXCLUSIVE
    //     `range_end` (`bytes=7-16` ≙ offset=7,length=10 → 17).
    //  5. fs_read errors: the header form answers `{"error": …}`; the
    //     offset/length form keeps its historical `{"ok":false,
    //     "error": …}` body (`total_size` inline on 416 instead of a
    //     `Content-Range: bytes */N` header), and each form keeps its
    //     historical wordings (e.g. "<path> is not a file" vs "path is
    //     not a regular file").
    //  6. fs_read filename: HTTP sanitizes the canonicalized name
    //     (fallback "download.bin"); the offset/length form reports
    //     the expanded path's file name verbatim (nullable).
    //  7. fs_read sha256: HTTP hashes form-full reads (no `Range`
    //     header sent); the offset/length form hashes extent-full
    //     reads (offset 0 covering the whole file).

    /// Render a neutral response through the HTTP adapter and split it
    /// into (status, headers, body).
    fn http_parts(
        response: crate::web_gateway::ApiResponse,
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
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
        let body = bytes[split + 4..].to_vec();
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.strip_prefix("HTTP/1.1 "))
            .and_then(|line| line.split_whitespace().next())
            .and_then(|code| code.parse::<u16>().ok())
            .expect("status line");
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(": ").expect("header line");
                (name.to_string(), value.to_string())
            })
            .collect();
        (status, headers, body)
    }

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    /// Strip the tunnel envelope (difference #1): assert the
    /// `http_body_response` shape, check the injected status metadata
    /// against the HTTP adapter's status, and return the result object
    /// minus the injected keys.
    fn tunnel_result_body(frame: &serde_json::Value, expect_status: u16) -> serde_json::Value {
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["ok"], true, "{frame}");
        let mut result = frame["result"].clone();
        let map = result.as_object_mut().expect("result object");
        assert_eq!(
            map.remove("_httpStatus"),
            Some(serde_json::json!(expect_status)),
            "{frame}"
        );
        assert_eq!(
            map.remove("_httpOk"),
            Some(serde_json::json!((200..300).contains(&expect_status)))
        );
        result
    }

    #[tokio::test]
    async fn parity_fs_stat_serves_the_same_body_on_both_transports() {
        let dir = tempfile::tempdir().unwrap();
        for params in [
            serde_json::json!({ "path": dir.path().to_string_lossy() }),
            serde_json::json!({ "path": "relative/path" }),
        ] {
            let request = crate::web_gateway::ApiRequest {
                params: params.clone(),
                range: None,
            };
            let (status, _headers, body) =
                http_parts(crate::web_gateway::fs_stat_api_response(&request));
            let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();

            let frame = api_fs_stat_response("parity-stat".to_string(), Some(&params)).await;
            assert_eq!(tunnel_result_body(&frame, status), http_body, "{params}");
        }
    }

    #[tokio::test]
    async fn parity_fs_list_serves_the_same_body_on_both_transports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::write(dir.path().join("alpha.txt"), b"a").unwrap();
        let not_a_dir = dir.path().join("alpha.txt");
        for params in [
            serde_json::json!({ "path": dir.path().to_string_lossy() }),
            serde_json::json!({ "path": not_a_dir.to_string_lossy() }),
        ] {
            let request = crate::web_gateway::ApiRequest {
                params: params.clone(),
                range: None,
            };
            let (status, _headers, body) =
                http_parts(crate::web_gateway::fs_list_api_response(&request));
            let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();

            let frame = api_fs_list_response("parity-list".to_string(), Some(&params)).await;
            assert_eq!(tunnel_result_body(&frame, status), http_body, "{params}");
        }
    }

    #[tokio::test]
    async fn parity_fs_read_serves_the_same_bytes_and_meta_on_both_transports() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("parity.txt");
        std::fs::write(&file, b"parity read fixture payload").unwrap();
        let path = file.to_string_lossy().into_owned();

        // Full read: no range on either transport — same bytes, and the
        // tunnel meta matches the HTTP headers (differences #3/#7).
        let request = crate::web_gateway::ApiRequest {
            params: serde_json::json!({ "path": path }),
            range: None,
        };
        let (status, headers, body) =
            http_parts(crate::web_gateway::fs_read_api_response(&request));
        assert_eq!(status, 200);
        let full = api_fs_read_task_response(
            "parity-read".to_string(),
            Some(&serde_json::json!({ "path": path })),
        )
        .await;
        let stream = full.byte_stream.expect("tunnel byte stream");
        assert_eq!(stream.bytes, body);
        assert_eq!(
            stream.result["sha256"].as_str(),
            header_value(&headers, "X-Content-Sha256")
        );
        assert_eq!(
            Some(stream.content_type.as_str()),
            header_value(&headers, "Content-Type")
        );
        assert_eq!(stream.result["total_size"].as_u64(), Some(body.len() as u64));

        // Ranged read: offset=7,length=10 ≙ `Range: bytes=7-16` — same
        // bytes; inclusive header vs exclusive range_end (difference #4).
        let request = crate::web_gateway::ApiRequest {
            params: serde_json::json!({ "path": path }),
            range: Some(crate::web_gateway::ByteRange::HttpHeader(
                "bytes=7-16".to_string(),
            )),
        };
        let (status, headers, body) =
            http_parts(crate::web_gateway::fs_read_api_response(&request));
        assert_eq!(status, 206);
        assert_eq!(
            header_value(&headers, "Content-Range"),
            Some("bytes 7-16/27")
        );
        let ranged = api_fs_read_task_response(
            "parity-read-range".to_string(),
            Some(&serde_json::json!({ "path": path, "offset": 7, "length": 10 })),
        )
        .await;
        let stream = ranged.byte_stream.expect("tunnel byte stream");
        assert_eq!(stream.bytes, body);
        assert_eq!(stream.result["range_start"].as_u64(), Some(7));
        assert_eq!(stream.result["range_end"].as_u64(), Some(17));
        assert!(stream.result["sha256"].is_null());
    }

    #[tokio::test]
    async fn parity_fs_read_errors_share_messages_under_their_own_envelopes() {
        // Difference #5 pinned from both sides: the same expand failure
        // surfaces the same message under each form's historical body.
        let params = serde_json::json!({ "path": "relative/path" });
        let request = crate::web_gateway::ApiRequest {
            params: params.clone(),
            range: None,
        };
        let (status, _headers, body) =
            http_parts(crate::web_gateway::fs_read_api_response(&request));
        assert_eq!(status, 400);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let response =
            api_fs_read_task_response("parity-read-error".to_string(), Some(&params)).await;
        assert!(response.byte_stream.is_none());
        let tunnel_body = tunnel_result_body(&response.frame, 400);
        assert_eq!(tunnel_body["error"], http_body["error"]);
        assert_eq!(tunnel_body["ok"], false);
        assert!(http_body.get("ok").is_none());
    }

    // ── Quartet parity (S2b): mkdir/rename/delete/write ──
    //
    // Same discipline as the stat/list/read set above; all four are
    // JSON-lane, so envelope differences #1 (tunnel result wrapper +
    // injected status metadata) and #2 (HTTP header decoration) apply
    // verbatim. The quartet-specific differences, pinned here:
    //
    //  8. Write content carriage (design §2.7's preserved asymmetry):
    //     HTTP carries JSON `content`/`content_b64` (exactly one,
    //     under the row's body cap) decoded by the serde
    //     FsWriteRequest; the tunnel spools raw bytes via
    //     upload_start/chunk/end frames and folds its params
    //     leniently (fs_write_args_from_params).
    //  9. Param decode is transport-owned: HTTP's typed serde decode
    //     rejects missing/mistyped fields as 400 "invalid JSON"
    //     before the neutral fn runs; the tunnel's historical lenient
    //     reads (string_param trim/coercion on mkdir; as_str/as_bool
    //     defaults on rename/delete/write) reach the neutral fn with
    //     defaults instead.
    // 10. The formerly tunnel-side spawn-join arms (unreachable in
    //     practice) now converge on the neutral fns' enveloped 500s;
    //     only the tunnel's upload-spool read keeps its historical
    //     bare-frame join arm and tempfile-read 500.
    //
    // Mutations are compared under identical pre-state: fixtures
    // reset the filesystem between the HTTP-render leg and the tunnel
    // leg, so "same params ⇒ same body" holds exactly (the write
    // success leg normalizes the time-varying `modified_ms` only).

    #[tokio::test]
    async fn parity_fs_mkdir_serves_the_same_body_on_both_transports() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("made-here");
        let target_params = serde_json::json!({ "path": target.to_string_lossy() });

        // Create leg (reset between transports).
        let (status, _headers, body) = http_parts(crate::web_gateway::fs_mkdir_api_response(
            &target.to_string_lossy(),
        ));
        assert_eq!(status, 200);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(http_body["created"], true);
        std::fs::remove_dir(&target).unwrap();
        let frame = api_fs_mkdir_response("parity-mkdir".to_string(), Some(&target_params)).await;
        assert_eq!(tunnel_result_body(&frame, status), http_body);

        // Already-exists and relative-path legs are idempotent.
        for (params, path) in [
            (
                serde_json::json!({ "path": dir.path().to_string_lossy() }),
                dir.path().to_string_lossy().into_owned(),
            ),
            (
                serde_json::json!({ "path": "relative/path" }),
                "relative/path".to_string(),
            ),
        ] {
            let (status, _headers, body) =
                http_parts(crate::web_gateway::fs_mkdir_api_response(&path));
            let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let frame = api_fs_mkdir_response("parity-mkdir".to_string(), Some(&params)).await;
            assert_eq!(tunnel_result_body(&frame, status), http_body, "{params}");
        }
    }

    #[tokio::test]
    async fn parity_fs_rename_serves_the_same_body_on_both_transports() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from.txt");
        let dest = dir.path().join("to.txt");
        let params = serde_json::json!({
            "from": source.to_string_lossy(),
            "to": dest.to_string_lossy(),
        });

        // Success leg (reset between transports).
        std::fs::write(&source, b"payload").unwrap();
        let (status, _headers, body) = http_parts(
            crate::web_gateway::fs_rename_api_response(
                source.to_string_lossy().into_owned(),
                dest.to_string_lossy().into_owned(),
            )
            .await,
        );
        assert_eq!(status, 200);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(http_body["renamed"], true);
        std::fs::rename(&dest, &source).unwrap();
        let frame = api_fs_rename_response("parity-rename".to_string(), Some(&params)).await;
        assert_eq!(tunnel_result_body(&frame, status), http_body);
        std::fs::rename(&dest, &source).unwrap();

        // Destination-exists 409 (idempotent: state untouched).
        std::fs::write(&dest, b"occupied").unwrap();
        let (status, _headers, body) = http_parts(
            crate::web_gateway::fs_rename_api_response(
                source.to_string_lossy().into_owned(),
                dest.to_string_lossy().into_owned(),
            )
            .await,
        );
        assert_eq!(status, 409);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(http_body["code"], "exists");
        let frame = api_fs_rename_response("parity-rename".to_string(), Some(&params)).await;
        assert_eq!(tunnel_result_body(&frame, status), http_body);

        // Missing-source 404 (idempotent).
        std::fs::remove_file(&source).unwrap();
        std::fs::remove_file(&dest).unwrap();
        let (status, _headers, body) = http_parts(
            crate::web_gateway::fs_rename_api_response(
                source.to_string_lossy().into_owned(),
                dest.to_string_lossy().into_owned(),
            )
            .await,
        );
        assert_eq!(status, 404);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let frame = api_fs_rename_response("parity-rename".to_string(), Some(&params)).await;
        assert_eq!(tunnel_result_body(&frame, status), http_body);
    }

    #[tokio::test]
    async fn parity_fs_delete_serves_the_same_body_on_both_transports() {
        let dir = tempfile::tempdir().unwrap();

        // File-delete success leg (reset between transports).
        let file = dir.path().join("victim.txt");
        std::fs::write(&file, b"bytes").unwrap();
        let params = serde_json::json!({ "path": file.to_string_lossy() });
        let (status, _headers, body) = http_parts(
            crate::web_gateway::fs_delete_api_response(
                file.to_string_lossy().into_owned(),
                false,
            )
            .await,
        );
        assert_eq!(status, 200);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(http_body["deleted"], true);
        std::fs::write(&file, b"bytes").unwrap();
        let frame = api_fs_delete_response("parity-delete".to_string(), Some(&params)).await;
        assert_eq!(tunnel_result_body(&frame, status), http_body);

        // Non-empty directory 409 without recursive (idempotent).
        let full = dir.path().join("occupied");
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("keep.txt"), b"keep").unwrap();
        let params = serde_json::json!({ "path": full.to_string_lossy() });
        let (status, _headers, body) = http_parts(
            crate::web_gateway::fs_delete_api_response(
                full.to_string_lossy().into_owned(),
                false,
            )
            .await,
        );
        assert_eq!(status, 409);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(http_body["code"], "not_empty");
        let frame = api_fs_delete_response("parity-delete".to_string(), Some(&params)).await;
        assert_eq!(tunnel_result_body(&frame, status), http_body);
    }

    #[tokio::test]
    async fn parity_fs_write_serves_the_same_body_under_both_carriages() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("edited.toml");
        let content = b"[section]\nkey = \"value\"\n";
        std::fs::write(&file, content).unwrap();

        // Force-overwrite with the byte-identical payload: pre-state is
        // identical before each leg; only `modified_ms` varies (pinned
        // present, then normalized out).
        let http_request: crate::web_gateway::FsWriteRequest =
            serde_json::from_value(serde_json::json!({
                "path": file.to_string_lossy(),
                "content": String::from_utf8_lossy(content),
                "force": true,
            }))
            .unwrap();
        let (status, _headers, body) =
            http_parts(crate::web_gateway::fs_write_api_response(http_request).await);
        assert_eq!(status, 200);
        let mut http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tunnel_params = serde_json::json!({
            "path": file.to_string_lossy(),
            "force": true,
        });
        let response = api_fs_write_upload_task_response(
            "parity-write".to_string(),
            test_upload_state("api_fs_write", tunnel_params, content),
            runtime(),
        )
        .await;
        let mut tunnel_body = tunnel_result_body(&response.frame, status);
        for body in [&mut http_body, &mut tunnel_body] {
            let map = body.as_object_mut().expect("write body object");
            assert!(
                map.remove("modified_ms").is_some(),
                "modified_ms present: {map:?}"
            );
        }
        assert_eq!(tunnel_body, http_body);

        // Precondition-required 400 (idempotent, fully deterministic).
        let http_request: crate::web_gateway::FsWriteRequest =
            serde_json::from_value(serde_json::json!({
                "path": file.to_string_lossy(),
                "content": String::from_utf8_lossy(content),
            }))
            .unwrap();
        let (status, _headers, body) =
            http_parts(crate::web_gateway::fs_write_api_response(http_request).await);
        assert_eq!(status, 400);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(http_body["code"], "precondition_required");
        let tunnel_params = serde_json::json!({ "path": file.to_string_lossy() });
        let response = api_fs_write_upload_task_response(
            "parity-write-precondition".to_string(),
            test_upload_state("api_fs_write", tunnel_params, content),
            runtime(),
        )
        .await;
        assert_eq!(tunnel_result_body(&response.frame, status), http_body);
    }

    #[tokio::test]
    async fn fs_read_returns_bounded_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"filesystem read fixture").unwrap();

        let response = api_fs_read_task_response(
            "fs-read".to_string(),
            Some(&serde_json::json!({
                "path": file.to_string_lossy(),
                "offset": 11,
                "length": 4,
            })),
        )
        .await;

        assert!(response.byte_stream.is_some());
        let stream = response.byte_stream.unwrap();
        assert_eq!(stream.content_type, "text/plain; charset=utf-8");
        assert_eq!(stream.filename.as_deref(), Some("note.txt"));
        assert_eq!(stream.bytes, b"read");
        assert_eq!(stream.result["ok"], true);
        assert_eq!(stream.result["range_start"].as_u64(), Some(11));
        assert_eq!(stream.result["range_end"].as_u64(), Some(15));
        assert_eq!(
            stream.result["total_size"].as_u64(),
            Some("filesystem read fixture".len() as u64)
        );
        assert_eq!(stream.result["resumable"], true);
    }

    #[tokio::test]
    async fn fs_read_full_reads_carry_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, b"hash me").unwrap();

        let full = api_fs_read_task_response(
            "fs-read-full".to_string(),
            Some(&serde_json::json!({ "path": file.to_string_lossy() })),
        )
        .await;
        let stream = full.byte_stream.unwrap();
        assert_eq!(
            stream.result["sha256"].as_str(),
            Some(crate::web_gateway::fs_sha256_hex(b"hash me").as_str())
        );

        // Partial reads have no whole-file hash to offer.
        let partial = api_fs_read_task_response(
            "fs-read-partial".to_string(),
            Some(&serde_json::json!({
                "path": file.to_string_lossy(),
                "offset": 1,
                "length": 3,
            })),
        )
        .await;
        let stream = partial.byte_stream.unwrap();
        assert!(stream.result["sha256"].is_null());
    }

    #[tokio::test]
    async fn fs_write_upload_enforces_scope_and_preconditions() {
        let dir = tempfile::tempdir().unwrap();
        let scoped_runtime = || {
            let mut rt = runtime();
            rt.grant = DashboardControlGrant::Peer {
                fingerprint: "fp".into(),
                label: "peer".into(),
                profile: "file-operator".into(),
                filesystem: crate::peer::access_policy::FilesystemAccessPolicy {
                    read_roots: vec![],
                    write_roots: vec![dir.path().to_path_buf()],
                },
            };
            rt
        };

        // create_new inside the write root lands on disk.
        let target = dir.path().join("config.toml");
        let upload = test_upload_state(
            "api_fs_write",
            serde_json::json!({ "path": target.to_string_lossy(), "create_new": true }),
            b"key = 1\n",
        );
        let response =
            api_fs_write_upload_task_response("w1".to_string(), upload, scoped_runtime()).await;
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["created"], true);
        assert_eq!(
            response.frame["result"]["sha256"].as_str(),
            Some(crate::web_gateway::fs_sha256_hex(b"key = 1\n").as_str())
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"key = 1\n");

        // A path outside the write roots is refused before any disk IO.
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("escape.txt");
        let upload = test_upload_state(
            "api_fs_write",
            serde_json::json!({ "path": outside.to_string_lossy(), "create_new": true }),
            b"nope",
        );
        let response =
            api_fs_write_upload_task_response("w2".to_string(), upload, scoped_runtime()).await;
        assert_eq!(response.frame["result"]["_httpStatus"], 403);
        assert!(!outside.exists());

        // A read-only profile is refused at the operation ceiling.
        let mut reader = runtime();
        reader.grant = DashboardControlGrant::Peer {
            fingerprint: "fp".into(),
            label: "peer".into(),
            profile: "file-reader".into(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy {
                read_roots: vec![dir.path().to_path_buf()],
                write_roots: vec![dir.path().to_path_buf()],
            },
        };
        let upload = test_upload_state(
            "api_fs_write",
            serde_json::json!({ "path": target.to_string_lossy(), "force": true }),
            b"still nope",
        );
        let response = api_fs_write_upload_task_response("w3".to_string(), upload, reader).await;
        assert_eq!(response.frame["result"]["_httpStatus"], 403);
        assert_eq!(std::fs::read(&target).unwrap(), b"key = 1\n");

        // Stale expected_sha256 conflicts and reports the current hash.
        let upload = test_upload_state(
            "api_fs_write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "expected_sha256": crate::web_gateway::fs_sha256_hex(b"something else"),
            }),
            b"key = 2\n",
        );
        let response =
            api_fs_write_upload_task_response("w4".to_string(), upload, scoped_runtime()).await;
        assert_eq!(response.frame["result"]["_httpStatus"], 409);
        assert_eq!(response.frame["result"]["code"], "conflict");
        assert_eq!(
            response.frame["result"]["current_sha256"].as_str(),
            Some(crate::web_gateway::fs_sha256_hex(b"key = 1\n").as_str())
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"key = 1\n");

        // The matching baseline saves.
        let upload = test_upload_state(
            "api_fs_write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "expected_sha256": crate::web_gateway::fs_sha256_hex(b"key = 1\n"),
            }),
            b"key = 2\n",
        );
        let response =
            api_fs_write_upload_task_response("w5".to_string(), upload, scoped_runtime()).await;
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["created"], false);
        assert_eq!(std::fs::read(&target).unwrap(), b"key = 2\n");
    }

    #[tokio::test]
    async fn fs_write_upload_denials_are_audited() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.grant = DashboardControlGrant::Peer {
            fingerprint: "fp".into(),
            label: "audit-peer".into(),
            profile: "file-operator".into(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy {
                read_roots: vec![],
                write_roots: vec![dir.path().to_path_buf()],
            },
        };
        let mut events = rt.bus.subscribe();

        let outside = tempfile::tempdir().unwrap();
        let upload = test_upload_state(
            "api_fs_write",
            serde_json::json!({
                "path": outside.path().join("x.txt").to_string_lossy(),
                "create_new": true,
            }),
            b"x",
        );
        let response = api_fs_write_upload_task_response("a1".to_string(), upload, rt).await;
        assert_eq!(response.frame["result"]["_httpStatus"], 403);

        let mut audited = false;
        while let Ok(event) = events.try_recv() {
            if let AppEvent::PresenceLog { message, level, .. } = event {
                if message.contains("[peer-fs] denied")
                    && message.contains("peer=audit-peer")
                    && message.contains("profile=file-operator")
                {
                    assert_eq!(level, Some(LogLevel::Warn));
                    audited = true;
                }
            }
        }
        assert!(audited, "expected a [peer-fs] denied audit line on the bus");
    }

    #[tokio::test]
    async fn fs_read_rejects_relative_paths_and_directories() {
        let dir = tempfile::tempdir().unwrap();

        let relative = api_fs_read_task_response(
            "fs-read-relative".to_string(),
            Some(&serde_json::json!({
                "path": "relative/path",
            })),
        )
        .await;
        assert!(relative.byte_stream.is_none());
        assert_eq!(relative.frame["t"], "response");
        assert_eq!(relative.frame["result"]["_httpStatus"], 400);
        assert_eq!(relative.frame["result"]["_httpOk"], false);
        assert!(relative.frame["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("path must be absolute"));

        let directory = api_fs_read_task_response(
            "fs-read-dir".to_string(),
            Some(&serde_json::json!({
                "path": dir.path().to_string_lossy(),
            })),
        )
        .await;
        assert!(directory.byte_stream.is_none());
        assert_eq!(directory.frame["result"]["_httpStatus"], 400);
        assert_eq!(directory.frame["result"]["_httpOk"], false);
        assert!(directory.frame["result"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("not a regular file"));
    }

    #[tokio::test]
    async fn transfer_session_report_artifact_materializes_and_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let session_dir = dir.path().join("session-report");
        std::fs::create_dir_all(&project).unwrap();
        let log = crate::session_log::SessionLog::open(session_dir.clone()).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{\"ok\":true}\n").unwrap();
        std::fs::create_dir_all(session_dir.join("turns")).unwrap();
        std::fs::write(
            session_dir.join("turns").join("turn_001_stdout.txt"),
            "hello\n",
        )
        .unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);
        {
            let mut session = rt.shared_session.write().await;
            session.session_log = Some(Arc::new(std::sync::Mutex::new(log)));
        }

        let create = api_transfer_job_create_response(
            "report-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "session_report",
                    "session_id": "current",
                },
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(create["result"]["job"]["source_kind"], "session_report");
        assert_eq!(create["result"]["job"]["source_label"], "Session report");
        assert_eq!(create["result"]["job"]["managed_source"], true);
        assert_eq!(
            create["result"]["job"]["artifact"]["type"],
            "session_report"
        );
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let total_size = create["result"]["job"]["total_size"].as_u64().unwrap();

        let read = api_transfer_download_read_task_response(
            "report-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 0,
                "length": total_size,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "application/zip");
        assert!(stream.filename.as_deref().unwrap_or("").ends_with(".zip"));
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(stream.bytes)).unwrap();
        assert!(zip.by_name("summary.json").is_ok());
        assert!(zip.by_name("turns/turn_001_stdout.txt").is_ok());
    }

    #[tokio::test]
    async fn transfer_staged_upload_artifact_reads_byte_stream() {
        let project = tempfile::tempdir().unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project.path().to_path_buf());
        let bytes = b"dashboard raw upload bytes";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();

        let (status, body) = crate::web_gateway::current_upload_commit_response_body(
            &rt.state_root,
            Some(project.path()),
            None,
            Some(rt.session_id.as_str()),
            "raw.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            crate::web_gateway::SpooledBody {
                tmp,
                len: bytes.len(),
            },
            &rt.bus,
        );
        assert_eq!(status, "200 OK");
        let descriptor: crate::upload_store::UploadDescriptor =
            serde_json::from_str(&body).unwrap();

        let create = api_transfer_job_create_response(
            "staged-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "staged_upload",
                    "id": descriptor.id,
                },
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(create["result"]["job"]["source_kind"], "staged_upload");
        assert_eq!(
            create["result"]["job"]["source_label"],
            "Staged upload raw.txt"
        );
        assert_eq!(create["result"]["job"]["filename"], "raw.txt");
        assert_eq!(create["result"]["job"]["managed_source"], false);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();

        let read = api_transfer_download_read_task_response(
            "staged-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 10,
                "length": 6,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "text/plain");
        assert_eq!(stream.filename.as_deref(), Some("raw.txt"));
        assert_eq!(stream.bytes, &bytes[10..16]);
        assert_eq!(stream.result["resumable"], true);
    }

    #[tokio::test]
    async fn transfer_recording_asset_artifact_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let session_dir = dir.path().join("recording-session");
        let stream_dir = session_dir.join("recordings").join("display_0");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&stream_dir).unwrap();
        std::fs::write(stream_dir.join("segments.csv"), "seg_00000.mp4,0,1.25\n").unwrap();
        let media = b"recording segment bytes";
        std::fs::write(stream_dir.join("seg_00000.mp4"), media).unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);
        {
            let mut session = rt.shared_session.write().await;
            session.recording_registry = Some(Arc::new(tokio::sync::RwLock::new(
                crate::recording::RecordingRegistry::new(
                    &session_dir,
                    crate::project::RecordingConfig::default(),
                ),
            )));
        }

        let create = api_transfer_job_create_response(
            "recording-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "recording_asset",
                    "stream_name": "display_0",
                    "asset": "seg_00000.mp4",
                },
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(create["result"]["job"]["source_kind"], "recording_asset");
        assert_eq!(
            create["result"]["job"]["source_label"],
            "display_0 seg_00000.mp4"
        );
        assert_eq!(create["result"]["job"]["managed_source"], false);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();

        let read = api_transfer_download_read_task_response(
            "recording-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 10,
                "length": 7,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "video/mp4");
        assert_eq!(stream.filename.as_deref(), Some("seg_00000.mp4"));
        assert_eq!(stream.bytes, b"segment");
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 17);
    }

    #[tokio::test]
    async fn transfer_session_frame_artifact_reads_byte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        // Fixture under an injected temp home's `.intendant/logs` store —
        // the `_from_home` create variant resolves sessions from the same
        // home (the public adapter resolves the real home at the edge).
        let home = tempfile::tempdir().unwrap();
        let session_id = "dashboard-frame-transfer-test";
        let session_dir = crate::platform::intendant_home_in(home.path())
            .join("logs")
            .join(session_id);
        let frames_dir = session_dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let frame_bytes = b"dashboard frame bytes";
        std::fs::write(frames_dir.join("ann-test.png"), frame_bytes).unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);

        let create = api_transfer_job_create_response_from_home(
            "frame-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "session_frame_asset",
                    "session_id": session_id,
                    "filename": "ann-test.png",
                },
            })),
            &rt,
            home.path(),
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        assert_eq!(
            create["result"]["job"]["source_kind"],
            "session_frame_asset"
        );
        assert_eq!(create["result"]["job"]["filename"], "ann-test.png");
        assert_eq!(create["result"]["job"]["managed_source"], false);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();

        let read = api_transfer_download_read_task_response(
            "frame-transfer-read".to_string(),
            Some(&serde_json::json!({
                "id": job_id,
                "offset": 10,
                "length": 5,
            })),
            &rt,
        )
        .await;
        assert!(read.done);
        let stream = read.byte_stream.unwrap();
        assert_eq!(stream.content_type, "image/png");
        assert_eq!(stream.filename.as_deref(), Some("ann-test.png"));
        assert_eq!(stream.bytes, b"frame");
        assert_eq!(stream.result["range_start"], 10);
        assert_eq!(stream.result["range_end"], 15);
    }

    #[tokio::test]
    async fn transfer_upload_chunks_commit_to_arbitrary_destination() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let dest_dir = dir.path().join("dest");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("out.txt");

        let mut rt = runtime();
        rt.project_root = Some(project);

        let create = api_transfer_job_create_response(
            "upload-create".to_string(),
            Some(&serde_json::json!({
                "kind": "upload",
                "destination": dest.to_string_lossy(),
                "name": "out.txt",
                "mime": "text/plain",
                "total_size": 11,
                "conflict": "fail",
            })),
            &rt,
        )
        .await;
        assert_eq!(create["result"]["ok"], true);
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let resume_token = create["result"]["job"]["resume_token"]
            .as_str()
            .unwrap()
            .to_string();

        let first = api_transfer_upload_chunk_task_response(
            "upload-chunk-1".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({
                    "id": job_id,
                    "offset": 0,
                }),
                b"hello ",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(first.frame["result"]["ok"], true);
        assert_eq!(first.frame["result"]["job"]["completed_bytes"], 6);
        assert_eq!(first.frame["result"]["job"]["status"], "running");

        let second = api_transfer_upload_chunk_task_response(
            "upload-chunk-2".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({
                    "resume_token": resume_token,
                    "offset": 6,
                }),
                b"world",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(second.frame["result"]["ok"], true);
        assert_eq!(second.frame["result"]["job"]["completed_bytes"], 11);
        assert_eq!(second.frame["result"]["job"]["status"], "ready");

        let commit = api_transfer_upload_commit_response(
            "upload-commit".to_string(),
            Some(&serde_json::json!({ "id": job_id })),
            &rt,
        )
        .await;
        assert_eq!(commit["result"]["ok"], true);
        assert_eq!(commit["result"]["job"]["status"], "completed");
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
    }

    // ── S9 goldens: the tunnel transfer wire shapes ──
    //
    // Captured against the pre-conversion handlers and kept green through
    // the neutral-core delegation (transport-unification design §8: goldens
    // first, byte-identical after). Every frame below is pinned WHOLE —
    // envelope, injected `_httpStatus`/`_httpOk` metadata, and the full
    // 21-key serialized `TransferJob` object — with only the genuinely
    // volatile fields (uuids, unix times, tempdir-rooted paths) asserted
    // for shape and then normalized out.

    /// Remove `key` from a JSON object, panicking when absent — the
    /// golden's way of saying "this field exists; its value is volatile".
    fn take_field(value: &mut serde_json::Value, key: &str) -> serde_json::Value {
        let display = value.to_string();
        value
            .as_object_mut()
            .unwrap_or_else(|| panic!("not an object: {display}"))
            .remove(key)
            .unwrap_or_else(|| panic!("missing {key}: {display}"))
    }

    fn take_uuid(value: &mut serde_json::Value, key: &str) -> String {
        let taken = take_field(value, key);
        let text = taken.as_str().unwrap_or_else(|| panic!("{key}: {taken}"));
        assert_eq!(text.len(), 36, "{key} must be a uuid: {text}");
        text.to_string()
    }

    fn take_unix_time(value: &mut serde_json::Value, key: &str) {
        let taken = take_field(value, key);
        assert!(
            taken.as_u64().is_some_and(|t| t > 0),
            "{key} must be a unix time: {taken}"
        );
    }

    fn take_abs_path(value: &mut serde_json::Value, key: &str) -> String {
        let taken = take_field(value, key);
        let text = taken.as_str().unwrap_or_else(|| panic!("{key}: {taken}"));
        assert!(
            std::path::Path::new(text).is_absolute(),
            "{key} must be absolute: {text}"
        );
        text.to_string()
    }

    /// Normalize a serialized `TransferJob`'s volatile fields (id,
    /// resume_token, created/updated times) in place, returning
    /// (id, resume_token). The caller then pins the remaining object
    /// exactly, including every `null`-serialized Option.
    fn normalize_job(job: &mut serde_json::Value) -> (String, String) {
        let id = take_uuid(job, "id");
        let token = take_uuid(job, "resume_token");
        take_unix_time(job, "created_at");
        take_unix_time(job, "updated_at");
        (id, token)
    }

    /// A download-kind transfer rig: project-rooted runtime plus a
    /// 14-byte source fixture.
    fn download_rig() -> (tempfile::TempDir, ControlRuntime, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source = dir.path().join("fixture.txt");
        std::fs::write(&source, b"hello transfer").unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project);
        (dir, rt, source)
    }

    async fn create_download_job_frame(rt: &ControlRuntime, source: &std::path::Path) -> serde_json::Value {
        api_transfer_job_create_response(
            "g-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "path": source.to_string_lossy(),
            })),
            rt,
        )
        .await
    }

    #[tokio::test]
    async fn golden_transfer_job_create_download_frame() {
        let (_dir, rt, source) = download_rig();
        let mut frame = create_download_job_frame(&rt, &source).await;
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["id"], "g-create");
        assert_eq!(frame["ok"], true);
        let mut result = frame["result"].take();
        let (_, _) = normalize_job(&mut result["job"]);
        let source_path = take_abs_path(&mut result["job"], "source_path");
        // Canonicalized at create (symlinked tempdirs resolve).
        assert_eq!(
            source_path,
            std::fs::canonicalize(&source).unwrap().to_string_lossy()
        );
        assert_eq!(
            result,
            serde_json::json!({
                "_httpOk": true,
                "_httpStatus": 200,
                "ok": true,
                "job": {
                    "kind": "download",
                    "status": "queued",
                    "source_kind": "filesystem",
                    "source_label": null,
                    "artifact": null,
                    "managed_source": false,
                    "destination_path": null,
                    "final_path": null,
                    "temp_path": null,
                    "original_name": "fixture.txt",
                    "filename": "fixture.txt",
                    "mime": "text/plain; charset=utf-8",
                    "total_size": 14,
                    "completed_bytes": 0,
                    "error": null,
                    "conflict_policy": "fail",
                },
            })
        );
    }

    #[tokio::test]
    async fn golden_transfer_job_create_error_frames() {
        let (_dir, rt, _source) = download_rig();
        let bad_kind = api_transfer_job_create_response(
            "g-kind".to_string(),
            Some(&serde_json::json!({ "kind": "sideways" })),
            &rt,
        )
        .await;
        assert_eq!(
            bad_kind,
            serde_json::json!({
                "t": "response",
                "id": "g-kind",
                "ok": true,
                "result": {
                    "_httpOk": false,
                    "_httpStatus": 400,
                    "ok": false,
                    "error": "transfer kind must be download or upload",
                },
            })
        );
        let missing_path = api_transfer_job_create_response(
            "g-path".to_string(),
            Some(&serde_json::json!({ "kind": "download" })),
            &rt,
        )
        .await;
        assert_eq!(
            missing_path,
            serde_json::json!({
                "t": "response",
                "id": "g-path",
                "ok": true,
                "result": {
                    "_httpOk": false,
                    "_httpStatus": 400,
                    "ok": false,
                    "error": "missing path",
                },
            })
        );
        let missing_destination = api_transfer_job_create_response(
            "g-dest".to_string(),
            Some(&serde_json::json!({ "kind": "upload" })),
            &rt,
        )
        .await;
        assert_eq!(
            missing_destination["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 400,
                "ok": false,
                "error": "missing destination",
            })
        );
        let bad_conflict = api_transfer_job_create_response(
            "g-conflict".to_string(),
            Some(&serde_json::json!({
                "kind": "upload",
                "destination": "/tmp/wherever",
                "conflict": "merge",
            })),
            &rt,
        )
        .await;
        assert_eq!(
            bad_conflict["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 400,
                "ok": false,
                "error": "conflict policy must be fail, rename, or overwrite",
            })
        );
    }

    #[tokio::test]
    async fn golden_transfer_jobs_list_frames() {
        let (_dir, rt, source) = download_rig();
        let empty = api_transfer_jobs_response("g-list-empty".to_string(), None, &rt).await;
        assert_eq!(
            empty,
            serde_json::json!({
                "t": "response",
                "id": "g-list-empty",
                "ok": true,
                "result": {
                    "_httpOk": true,
                    "_httpStatus": 200,
                    "ok": true,
                    "jobs": [],
                },
            })
        );

        let create = create_download_job_frame(&rt, &source).await;
        let job_id = create["result"]["job"]["id"].as_str().unwrap();
        let mut listed = api_transfer_jobs_response("g-list".to_string(), None, &rt).await;
        assert_eq!(listed["t"], "response");
        assert_eq!(listed["ok"], true);
        assert_eq!(listed["result"]["_httpStatus"], 200);
        assert_eq!(listed["result"]["ok"], true);
        let jobs = listed["result"]["jobs"].as_array_mut().unwrap();
        assert_eq!(jobs.len(), 1);
        let (listed_id, _) = normalize_job(&mut jobs[0]);
        assert_eq!(listed_id, job_id);
        take_abs_path(&mut jobs[0], "source_path");
        assert_eq!(jobs[0]["kind"], "download");
        assert_eq!(jobs[0]["status"], "queued");
    }

    #[tokio::test]
    async fn golden_transfer_upload_chunk_frames() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let dest_dir = dir.path().join("dest");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project);

        let create = api_transfer_job_create_response(
            "g-up-create".to_string(),
            Some(&serde_json::json!({
                "kind": "upload",
                "destination": dest_dir.join("out.txt").to_string_lossy(),
                "name": "out.txt",
                "mime": "text/plain",
                "total_size": 11,
            })),
            &rt,
        )
        .await;
        assert_eq!(create["t"], "response");
        assert_eq!(create["ok"], true);
        let mut result = create["result"].clone();
        let (job_id, _) = normalize_job(&mut result["job"]);
        let temp_path = take_abs_path(&mut result["job"], "temp_path");
        assert!(temp_path.contains(".intendant-upload-"), "{temp_path}");
        let destination_path = take_abs_path(&mut result["job"], "destination_path");
        assert!(destination_path.ends_with("out.txt"), "{destination_path}");
        assert_eq!(
            result,
            serde_json::json!({
                "_httpOk": true,
                "_httpStatus": 200,
                "ok": true,
                "job": {
                    "kind": "upload",
                    "status": "queued",
                    "source_path": null,
                    "source_kind": null,
                    "source_label": null,
                    "artifact": null,
                    "managed_source": false,
                    "final_path": null,
                    "original_name": "out.txt",
                    "filename": "out.txt",
                    "mime": "text/plain",
                    "total_size": 11,
                    "completed_bytes": 0,
                    "error": null,
                    "conflict_policy": "fail",
                },
            })
        );

        // Missing id: refused before any store work, task-response shaped.
        let missing = api_transfer_upload_chunk_task_response(
            "g-chunk-noid".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({ "offset": 0 }),
                b"hello ",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(missing.id, "g-chunk-noid");
        assert!(missing.done);
        assert!(missing.byte_stream.is_none());
        assert_eq!(
            missing.frame,
            serde_json::json!({
                "t": "response",
                "id": "g-chunk-noid",
                "ok": true,
                "result": {
                    "_httpOk": false,
                    "_httpStatus": 400,
                    "ok": false,
                    "error": "missing id",
                },
            })
        );

        // First chunk lands; the whole job object rides the frame.
        let first = api_transfer_upload_chunk_task_response(
            "g-chunk-1".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({ "id": job_id, "offset": 0 }),
                b"hello ",
            ),
            rt.clone(),
        )
        .await;
        assert!(first.done);
        assert!(first.byte_stream.is_none());
        let mut frame = first.frame.clone();
        assert_eq!(frame["t"], "response");
        assert_eq!(frame["id"], "g-chunk-1");
        assert_eq!(frame["ok"], true);
        let mut result = frame["result"].take();
        normalize_job(&mut result["job"]);
        take_abs_path(&mut result["job"], "temp_path");
        take_abs_path(&mut result["job"], "destination_path");
        assert_eq!(
            result,
            serde_json::json!({
                "_httpOk": true,
                "_httpStatus": 200,
                "ok": true,
                "job": {
                    "kind": "upload",
                    "status": "running",
                    "source_path": null,
                    "source_kind": null,
                    "source_label": null,
                    "artifact": null,
                    "managed_source": false,
                    "final_path": null,
                    "original_name": "out.txt",
                    "filename": "out.txt",
                    "mime": "text/plain",
                    "total_size": 11,
                    "completed_bytes": 6,
                    "error": null,
                    "conflict_policy": "fail",
                },
            })
        );

        // A stale offset (not the persisted boundary, not fully covered)
        // is refused with the store's 409.
        let stale = api_transfer_upload_chunk_task_response(
            "g-chunk-stale".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({ "id": job_id, "offset": 3 }),
                b"hello ",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(
            stale.frame["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 409,
                "ok": false,
                "error": "upload chunk overlaps already persisted bytes",
            })
        );

        // A chunk past the declared total is refused with 413.
        let oversize = api_transfer_upload_chunk_task_response(
            "g-chunk-oversize".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({ "id": job_id, "offset": 6 }),
                b"world!!",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(
            oversize.frame["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 413,
                "ok": false,
                "error": "upload chunk exceeds declared total size",
            })
        );

        // Premature commit: the partial is short of the declared total.
        let premature = api_transfer_upload_commit_response(
            "g-commit-early".to_string(),
            Some(&serde_json::json!({ "id": job_id })),
            &rt,
        )
        .await;
        assert_eq!(
            premature["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 409,
                "ok": false,
                "error": "upload is not complete enough to commit",
            })
        );
    }

    #[tokio::test]
    async fn golden_transfer_commit_and_delete_frames() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let dest_dir = dir.path().join("dest");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        let mut rt = runtime();
        rt.project_root = Some(project);

        let missing_commit = api_transfer_upload_commit_response(
            "g-commit-noid".to_string(),
            None,
            &rt,
        )
        .await;
        assert_eq!(
            missing_commit,
            serde_json::json!({
                "t": "response",
                "id": "g-commit-noid",
                "ok": true,
                "result": {
                    "_httpOk": false,
                    "_httpStatus": 400,
                    "ok": false,
                    "error": "missing id",
                },
            })
        );

        let create = api_transfer_job_create_response(
            "g-cd-create".to_string(),
            Some(&serde_json::json!({
                "kind": "upload",
                "destination": dest_dir.join("done.txt").to_string_lossy(),
                "name": "done.txt",
                "total_size": 4,
            })),
            &rt,
        )
        .await;
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let chunk = api_transfer_upload_chunk_task_response(
            "g-cd-chunk".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                serde_json::json!({ "id": job_id, "offset": 0 }),
                b"data",
            ),
            rt.clone(),
        )
        .await;
        assert_eq!(chunk.frame["result"]["job"]["status"], "ready");

        let commit = api_transfer_upload_commit_response(
            "g-cd-commit".to_string(),
            Some(&serde_json::json!({ "id": job_id })),
            &rt,
        )
        .await;
        assert_eq!(commit["t"], "response");
        assert_eq!(commit["id"], "g-cd-commit");
        assert_eq!(commit["ok"], true);
        let mut result = commit["result"].clone();
        normalize_job(&mut result["job"]);
        let final_path = take_abs_path(&mut result["job"], "final_path");
        assert!(final_path.ends_with("done.txt"), "{final_path}");
        take_abs_path(&mut result["job"], "destination_path");
        assert_eq!(
            result,
            serde_json::json!({
                "_httpOk": true,
                "_httpStatus": 200,
                "ok": true,
                "job": {
                    "kind": "upload",
                    "status": "completed",
                    "source_path": null,
                    "source_kind": null,
                    "source_label": null,
                    "artifact": null,
                    "managed_source": false,
                    "temp_path": null,
                    "original_name": "done.txt",
                    "filename": "done.txt",
                    "mime": "application/octet-stream",
                    "total_size": 4,
                    "completed_bytes": 4,
                    "error": null,
                    "conflict_policy": "fail",
                },
            })
        );

        // Delete: missing id, then the real job (true), then a vanished
        // id (false — still 200-shaped).
        let missing_delete =
            api_transfer_job_delete_response("g-del-noid".to_string(), None, &rt).await;
        assert_eq!(
            missing_delete["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 400,
                "ok": false,
                "error": "missing id",
            })
        );
        let deleted = api_transfer_job_delete_response(
            "g-del".to_string(),
            Some(&serde_json::json!({ "id": job_id })),
            &rt,
        )
        .await;
        assert_eq!(
            deleted,
            serde_json::json!({
                "t": "response",
                "id": "g-del",
                "ok": true,
                "result": {
                    "_httpOk": true,
                    "_httpStatus": 200,
                    "ok": true,
                    "deleted": true,
                },
            })
        );
        let vanished = api_transfer_job_delete_response(
            "g-del-again".to_string(),
            Some(&serde_json::json!({ "id": job_id })),
            &rt,
        )
        .await;
        assert_eq!(
            vanished["result"],
            serde_json::json!({
                "_httpOk": true,
                "_httpStatus": 200,
                "ok": true,
                "deleted": false,
            })
        );
    }

    #[tokio::test]
    async fn golden_transfer_download_read_frames() {
        let (_dir, rt, source) = download_rig();
        let create = create_download_job_frame(&rt, &source).await;
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let resume_token = create["result"]["job"]["resume_token"]
            .as_str()
            .unwrap()
            .to_string();

        // Missing id / unknown job / range-beyond errors keep the plain
        // response envelope (no byte stream).
        let missing =
            api_transfer_download_read_task_response("g-dl-noid".to_string(), None, &rt).await;
        assert!(missing.byte_stream.is_none());
        assert_eq!(
            missing.frame["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 400,
                "ok": false,
                "error": "missing id",
            })
        );
        let unknown = api_transfer_download_read_task_response(
            "g-dl-unknown".to_string(),
            Some(&serde_json::json!({ "id": "00000000-0000-0000-0000-000000000000" })),
            &rt,
        )
        .await;
        assert_eq!(
            unknown.frame["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 404,
                "ok": false,
                "error": "transfer job not found",
            })
        );
        let beyond = api_transfer_download_read_task_response(
            "g-dl-beyond".to_string(),
            Some(&serde_json::json!({ "id": job_id, "offset": 15 })),
            &rt,
        )
        .await;
        assert_eq!(
            beyond.frame["result"],
            serde_json::json!({
                "_httpOk": false,
                "_httpStatus": 416,
                "ok": false,
                "error": "range start beyond file size",
            })
        );

        // Ranged read: byte-stream lane; the result sidecar carries the
        // resume metadata plus the whole updated job object.
        let ranged = api_transfer_download_read_task_response(
            "g-dl-range".to_string(),
            Some(&serde_json::json!({
                "resume_token": resume_token,
                "offset": 6,
                "length": 8,
            })),
            &rt,
        )
        .await;
        assert!(ranged.done);
        assert!(ranged.frame.is_null());
        let stream = ranged.byte_stream.expect("byte stream");
        assert_eq!(stream.id, "g-dl-range");
        assert_eq!(stream.stream_id, "g-dl-range:transfer-download");
        assert_eq!(stream.content_type, "text/plain; charset=utf-8");
        assert_eq!(stream.filename.as_deref(), Some("fixture.txt"));
        assert_eq!(stream.bytes, b"transfer");
        let mut result = stream.result.clone();
        take_abs_path(&mut result, "path");
        normalize_job(&mut result["job"]);
        take_abs_path(&mut result["job"], "source_path");
        assert_eq!(
            result,
            serde_json::json!({
                "ok": true,
                "id": job_id,
                "resume_token": resume_token,
                "filename": "fixture.txt",
                "content_type": "text/plain; charset=utf-8",
                "size": 8,
                "total_size": 14,
                "offset": 6,
                "range_start": 6,
                "range_end": 14,
                "resumable": true,
                "completed_bytes": 14,
                "status": "completed",
                "job": {
                    "kind": "download",
                    "status": "completed",
                    "source_kind": "filesystem",
                    "source_label": null,
                    "artifact": null,
                    "managed_source": false,
                    "destination_path": null,
                    "final_path": null,
                    "temp_path": null,
                    "original_name": "fixture.txt",
                    "filename": "fixture.txt",
                    "mime": "text/plain; charset=utf-8",
                    "total_size": 14,
                    "completed_bytes": 14,
                    "error": null,
                    "conflict_policy": "fail",
                },
            })
        );
    }

    // ── Transfer-family parity (S9a): the tunnel and the neutral cores'
    //    HTTP rendering serve the same bodies ──
    //
    // Same discipline as the fs sets above (envelope differences #1/#2
    // apply verbatim). The family-specific differences, continuing the
    // enumeration:
    //
    //  24. Artifact-shaped download creates are tunnel-only: their
    //      resolution reads live session handles (report builders,
    //      staged uploads, the recording registry) through the
    //      ControlRuntime, which the HTTP edge deliberately lacks — the
    //      HTTP create composition answers 400 for `artifact` bodies;
    //      every path-based create is fully neutral.
    //  25. The formerly bare-frame tunnel join-error arms (unreachable
    //      in practice) converge on the neutral fns' enveloped 500s —
    //      the same convergence the fs quartet settled (#10).
    //  26. Range addressing is transport-owned (fs #4 restated for
    //      jobs): HTTP's end-inclusive `Range` header normalizes
    //      against the source size before the shared store read, and
    //      its parse failures answer 416 with the probing
    //      `Content-Range: bytes */N` tail; the offset/length form
    //      keeps its body-only, store-worded 416/413 shapes on both
    //      lanes.
    //  27. Download reads are capped at UPLOAD_MAX_BYTES per request on
    //      BOTH forms — an unranged GET of a bigger job answers the
    //      store's 413; the jobs protocol's answer to big payloads is
    //      ranging (resumability makes small reads cheap), not one
    //      giant read.
    //
    // Additive, not a divergence: both lanes now honor a job-handle
    // param on the jobs list (`?id=` on HTTP); tunnel callers
    // historically pass no params and the no-param golden is unchanged.

    /// Twin stores for mutation parity: identical params against
    /// identical fresh state per leg, volatile fields normalized.
    struct ParityRig {
        _dir: tempfile::TempDir,
        rt: ControlRuntime,
        scope: crate::global_store::StoreScope,
        dest: std::path::PathBuf,
    }

    fn parity_rigs() -> (ParityRig, ParityRig) {
        let rig = || {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("project");
            let dest_dir = dir.path().join("dest");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::create_dir_all(&dest_dir).unwrap();
            let mut rt = runtime();
            rt.project_root = Some(project.clone());
            ParityRig {
                _dir: dir,
                rt,
                scope: crate::global_store::StoreScope::Project(project),
                dest: dest_dir.join("parity.bin"),
            }
        };
        (rig(), rig())
    }

    /// Normalize one transfer body for cross-lane comparison: job
    /// volatiles out, absolute paths asserted-and-dropped.
    fn normalize_transfer_body(body: &mut serde_json::Value) {
        if body.get("job").is_some_and(|job| job.is_object()) {
            normalize_job(&mut body["job"]);
            for key in ["temp_path", "destination_path", "final_path", "source_path"] {
                if body["job"].get(key).is_some_and(|value| value.is_string()) {
                    take_abs_path(&mut body["job"], key);
                }
            }
        }
    }

    fn spooled_body(bytes: &[u8]) -> crate::web_gateway::SpooledBody {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(bytes).unwrap();
        tmp.flush().unwrap();
        crate::web_gateway::SpooledBody {
            tmp,
            len: bytes.len(),
        }
    }

    #[tokio::test]
    async fn parity_transfer_create_errors_share_bodies_across_lanes() {
        let (http, tunnel) = parity_rigs();
        for params in [
            serde_json::json!({ "kind": "sideways" }),
            serde_json::json!({ "kind": "download" }),
            serde_json::json!({ "kind": "upload" }),
            serde_json::json!({ "kind": "upload", "destination": "/tmp/x", "conflict": "merge" }),
            serde_json::json!({ "kind": "upload", "destination": "/tmp/x", "total_size": "many" }),
        ] {
            let (status, _headers, body) = http_parts(
                crate::web_gateway::transfer_job_create_http_api_response(
                    http.scope.clone(),
                    params.clone(),
                )
                .await,
            );
            let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let frame = api_transfer_job_create_response(
                "parity-create".to_string(),
                Some(&params),
                &tunnel.rt,
            )
            .await;
            assert_eq!(tunnel_result_body(&frame, status), http_body, "{params}");
        }

        // Divergence #24: the artifact shape reaches the tunnel's
        // runtime-coupled resolver but answers the tunnel-only 400 on
        // the HTTP composition.
        let artifact_params = serde_json::json!({
            "kind": "download",
            "artifact": { "type": "unheard-of" },
        });
        let (status, _headers, body) = http_parts(
            crate::web_gateway::transfer_job_create_http_api_response(
                http.scope.clone(),
                artifact_params.clone(),
            )
            .await,
        );
        assert_eq!(status, 400);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            http_body["error"],
            "artifact transfers require the dashboard tunnel"
        );
        let frame = api_transfer_job_create_response(
            "parity-artifact".to_string(),
            Some(&artifact_params),
            &tunnel.rt,
        )
        .await;
        assert_eq!(
            tunnel_result_body(&frame, 400)["error"],
            "unsupported transfer artifact type: unheard-of"
        );
    }

    #[tokio::test]
    async fn parity_transfer_lifecycle_serves_the_same_bodies_on_both_transports() {
        let (http, tunnel) = parity_rigs();
        let create_params = |dest: &std::path::Path| {
            serde_json::json!({
                "kind": "upload",
                "destination": dest.to_string_lossy(),
                "name": "parity.bin",
                "mime": "application/octet-stream",
                "total_size": 11,
            })
        };

        // Create.
        let (status, _headers, body) = http_parts(
            crate::web_gateway::transfer_job_create_http_api_response(
                http.scope.clone(),
                create_params(&http.dest),
            )
            .await,
        );
        assert_eq!(status, 200);
        let mut http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let http_job_id = http_body["job"]["id"].as_str().unwrap().to_string();
        let frame = api_transfer_job_create_response(
            "parity-lc-create".to_string(),
            Some(&create_params(&tunnel.dest)),
            &tunnel.rt,
        )
        .await;
        let mut tunnel_body = tunnel_result_body(&frame, 200);
        let tunnel_job_id = tunnel_body["job"]["id"].as_str().unwrap().to_string();
        normalize_transfer_body(&mut http_body);
        normalize_transfer_body(&mut tunnel_body);
        assert_eq!(tunnel_body, http_body);

        // Chunk at 0 — the same spooled-body handle either transport
        // fills — then commit; same bodies at every step.
        let chunk = |job_id: &str| serde_json::json!({ "id": job_id, "offset": 0 });
        let (status, _headers, body) = http_parts(
            crate::web_gateway::transfer_upload_chunk_api_response(
                http.scope.clone(),
                &chunk(&http_job_id),
                spooled_body(b"hello world"),
            )
            .await,
        );
        assert_eq!(status, 200);
        let mut http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tunnel_frame = api_transfer_upload_chunk_task_response(
            "parity-lc-chunk".to_string(),
            test_upload_state(
                "api_transfer_upload_chunk",
                chunk(&tunnel_job_id),
                b"hello world",
            ),
            tunnel.rt.clone(),
        )
        .await;
        let mut tunnel_body = tunnel_result_body(&tunnel_frame.frame, 200);
        normalize_transfer_body(&mut http_body);
        normalize_transfer_body(&mut tunnel_body);
        assert_eq!(tunnel_body, http_body);
        assert_eq!(http_body["job"]["status"], "ready");

        let (status, _headers, body) = http_parts(
            crate::web_gateway::transfer_upload_commit_api_response(
                http.scope.clone(),
                &serde_json::json!({ "id": http_job_id }),
            )
            .await,
        );
        assert_eq!(status, 200);
        let mut http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let frame = api_transfer_upload_commit_response(
            "parity-lc-commit".to_string(),
            Some(&serde_json::json!({ "id": tunnel_job_id })),
            &tunnel.rt,
        )
        .await;
        let mut tunnel_body = tunnel_result_body(&frame, 200);
        normalize_transfer_body(&mut http_body);
        normalize_transfer_body(&mut tunnel_body);
        assert_eq!(tunnel_body, http_body);
        assert_eq!(std::fs::read(&http.dest).unwrap(), b"hello world");
        assert_eq!(std::fs::read(&tunnel.dest).unwrap(), b"hello world");

        // List (filtered to the one job by handle) and delete.
        let (status, _headers, body) = http_parts(
            crate::web_gateway::transfer_jobs_api_response(
                http.scope.clone(),
                &serde_json::json!({ "id": http_job_id }),
            )
            .await,
        );
        assert_eq!(status, 200);
        let mut http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let frame = api_transfer_jobs_response(
            "parity-lc-list".to_string(),
            Some(&serde_json::json!({ "id": tunnel_job_id })),
            &tunnel.rt,
        )
        .await;
        let mut tunnel_body = tunnel_result_body(&frame, 200);
        for body in [&mut http_body, &mut tunnel_body] {
            let jobs = body["jobs"].as_array_mut().unwrap();
            assert_eq!(jobs.len(), 1);
            normalize_job(&mut jobs[0]);
            take_abs_path(&mut jobs[0], "destination_path");
            take_abs_path(&mut jobs[0], "final_path");
        }
        assert_eq!(tunnel_body, http_body);

        let (status, _headers, body) = http_parts(
            crate::web_gateway::transfer_job_delete_api_response(
                http.scope.clone(),
                &serde_json::json!({ "id": http_job_id }),
            )
            .await,
        );
        assert_eq!(status, 200);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let frame = api_transfer_job_delete_response(
            "parity-lc-delete".to_string(),
            Some(&serde_json::json!({ "id": tunnel_job_id })),
            &tunnel.rt,
        )
        .await;
        assert_eq!(tunnel_result_body(&frame, 200), http_body);
        assert_eq!(http_body["deleted"], true);
    }

    #[tokio::test]
    async fn parity_transfer_download_read_meta_matches_http_headers() {
        let (_dir, rt, source) = download_rig();
        let create = create_download_job_frame(&rt, &source).await;
        let job_id = create["result"]["job"]["id"].as_str().unwrap().to_string();
        let scope = transfer_store_scope(&rt);

        // HTTP leg first; the same range re-read lands on identical
        // job bookkeeping, so the tunnel leg compares clean.
        let (status, headers, body) = http_parts(
            crate::web_gateway::transfer_download_read_api_response(
                scope.clone(),
                &serde_json::json!({ "id": job_id }),
                crate::web_gateway::ByteRange::OffsetLength {
                    offset: 6,
                    length: Some(8),
                },
            )
            .await,
        );
        assert_eq!(status, 206);
        assert_eq!(
            header_value(&headers, "Content-Range"),
            Some("bytes 6-13/14")
        );

        let read = api_transfer_download_read_task_response(
            "parity-dl".to_string(),
            Some(&serde_json::json!({ "id": job_id, "offset": 6, "length": 8 })),
            &rt,
        )
        .await;
        let stream = read.byte_stream.expect("tunnel byte stream");
        assert_eq!(stream.bytes, body);
        assert_eq!(
            Some(stream.content_type.as_str()),
            header_value(&headers, "Content-Type")
        );
        // The resume meta the tunnel carries as byte_stream_end.result
        // is what HTTP echoes as X-Transfer-* headers (design §4).
        for (meta_key, header_name) in [
            ("range_start", "X-Transfer-Range-Start"),
            ("range_end", "X-Transfer-Range-End"),
            ("total_size", "X-Transfer-Total-Size"),
            ("resumable", "X-Transfer-Resumable"),
        ] {
            assert_eq!(
                Some(stream.result[meta_key].to_string().as_str()),
                header_value(&headers, header_name),
                "{meta_key}"
            );
        }
        assert_eq!(header_value(&headers, "X-Content-Sha256"), None);

        // Error parity on the shared offset/length form (#26's
        // both-lanes half): same store wording under each envelope.
        let (status, _headers, body) = http_parts(
            crate::web_gateway::transfer_download_read_api_response(
                scope.clone(),
                &serde_json::json!({ "id": job_id }),
                crate::web_gateway::ByteRange::OffsetLength {
                    offset: 99,
                    length: None,
                },
            )
            .await,
        );
        assert_eq!(status, 416);
        let http_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let read = api_transfer_download_read_task_response(
            "parity-dl-416".to_string(),
            Some(&serde_json::json!({ "id": job_id, "offset": 99 })),
            &rt,
        )
        .await;
        assert_eq!(tunnel_result_body(&read.frame, 416), http_body);
    }
}
