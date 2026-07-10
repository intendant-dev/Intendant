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
    crate::global_store::StoreScope::resolve(runtime.project_root.as_deref())
}

pub(crate) fn transfer_http_error_response(
    id: String,
    status: u16,
    error: impl Into<String>,
    label: &str,
) -> serde_json::Value {
    http_body_response(
        id,
        status,
        serde_json::json!({
            "ok": false,
            "error": error.into(),
        })
        .to_string(),
        label,
    )
}

pub(crate) fn transfer_store_error_response(
    id: String,
    error: crate::transfer_store::TransferStoreError,
    label: &str,
) -> serde_json::Value {
    transfer_http_error_response(id, error.status, error.message, label)
}

pub(crate) fn transfer_id_param(params: &serde_json::Value) -> String {
    string_param(
        params,
        &[
            "id",
            "job_id",
            "jobId",
            "resume_token",
            "resumeToken",
            "token",
        ],
    )
}

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

pub(crate) async fn transfer_create_download_job_from_params(
    scope: crate::global_store::StoreScope,
    params: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    if let Some(artifact) = params
        .get("artifact")
        .filter(|value| value.is_object())
        .cloned()
    {
        return transfer_create_artifact_download_job(scope, artifact, runtime).await;
    }
    let path = string_param(&params, &["path", "source_path", "sourcePath", "source"]);
    if path.is_empty() {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing path",
        ));
    }
    tokio::task::spawn_blocking(move || crate::transfer_store::create_download_job(&scope, &path))
        .await
        .map_err(|e| transfer_store_task_error(e, "transfer create"))?
}

pub(crate) async fn transfer_create_upload_job_from_params(
    scope: crate::global_store::StoreScope,
    params: serde_json::Value,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    let destination = string_param(
        &params,
        &["destination", "destination_path", "destinationPath", "path"],
    );
    if destination.is_empty() {
        return Err(crate::transfer_store::TransferStoreError::new(
            400,
            "missing destination",
        ));
    }
    let original_name = optional_string_param(
        &params,
        &[
            "name",
            "filename",
            "file_name",
            "fileName",
            "original_name",
            "originalName",
        ],
    )
    .unwrap_or_else(|| "upload.bin".to_string());
    let mime = optional_string_param(&params, &["mime", "content_type", "contentType"])
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let total_size = optional_u64_param(
        &params,
        &[
            "total_size",
            "totalSize",
            "total_bytes",
            "totalBytes",
            "size",
        ],
    )
    .map_err(|error| crate::transfer_store::TransferStoreError::new(400, error))?;
    let conflict = optional_string_param(
        &params,
        &[
            "conflict",
            "conflict_policy",
            "conflictPolicy",
            "if_exists",
            "ifExists",
        ],
    )
    .unwrap_or_else(|| "fail".to_string());
    let conflict_policy =
        crate::transfer_store::TransferConflictPolicy::from_str(&conflict.to_ascii_lowercase())
            .ok_or_else(|| {
                crate::transfer_store::TransferStoreError::new(
                    400,
                    "conflict policy must be fail, rename, or overwrite",
                )
            })?;
    tokio::task::spawn_blocking(move || {
        crate::transfer_store::create_upload_job(
            &scope,
            &destination,
            &original_name,
            &mime,
            total_size,
            conflict_policy,
        )
    })
    .await
    .map_err(|e| transfer_store_task_error(e, "transfer create"))?
}

pub(crate) async fn transfer_create_artifact_download_job(
    scope: crate::global_store::StoreScope,
    artifact: serde_json::Value,
    runtime: &ControlRuntime,
) -> Result<crate::transfer_store::TransferJob, crate::transfer_store::TransferStoreError> {
    match transfer_artifact_type(&artifact).as_str() {
        "session_report" | "session-report" => {
            transfer_create_session_report_download_job(scope, artifact, runtime).await
        }
        "staged_upload" | "staged-upload" | "upload" => {
            transfer_create_staged_upload_download_job(scope, artifact, runtime).await
        }
        "recording_asset" | "recording-asset" => {
            transfer_create_recording_asset_download_job(scope, artifact, runtime, false).await
        }
        "session_recording_asset" | "session-recording-asset" => {
            transfer_create_recording_asset_download_job(scope, artifact, runtime, true).await
        }
        "session_frame_asset" | "session-frame-asset" | "frame_asset" | "frame-asset" => {
            transfer_create_session_frame_download_job(scope, artifact).await
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
        move || {
            crate::web_gateway::session_report_zip_for_request(
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
        let session_dir = crate::web_gateway::resolve_session_dir(&session_id);
        resolve_session_recording_asset(session_dir, &stream_name, &asset)
    } else {
        let Some(registry) = active_recording_registry(runtime).await else {
            return Err(crate::transfer_store::TransferStoreError::new(
                404,
                "recording registry unavailable",
            ));
        };
        resolve_live_recording_asset(registry, &stream_name, &asset).await
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
    let session_dir = crate::web_gateway::resolve_session_dir(&session_id)
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
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let result =
        tokio::task::spawn_blocking(move || crate::transfer_store::list_jobs(&scope)).await;
    let jobs = match result {
        Ok(jobs) => jobs,
        Err(e) => {
            return serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("transfer jobs task failed: {e}"),
            });
        }
    };
    http_body_response(
        id,
        200,
        serde_json::json!({
            "ok": true,
            "jobs": jobs,
        })
        .to_string(),
        "transfer jobs",
    )
}

pub(crate) async fn api_transfer_job_create_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let kind = string_param(&params, &["kind", "type"]);
    let kind = match crate::transfer_store::TransferKind::from_str(&kind.to_ascii_lowercase()) {
        Some(kind) => kind,
        None => {
            return transfer_http_error_response(
                id,
                400,
                "transfer kind must be download or upload",
                "transfer create",
            );
        }
    };
    let result = match kind {
        crate::transfer_store::TransferKind::Download => {
            transfer_create_download_job_from_params(scope, params, runtime).await
        }
        crate::transfer_store::TransferKind::Upload => {
            transfer_create_upload_job_from_params(scope, params).await
        }
    };
    match result {
        Ok(job) => http_body_response(
            id,
            200,
            serde_json::json!({
                "ok": true,
                "job": job,
            })
            .to_string(),
            "transfer create",
        ),
        Err(error) => transfer_store_error_response(id, error, "transfer create"),
    }
}

pub(crate) async fn api_transfer_job_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let job_id = transfer_id_param(&params);
    if job_id.is_empty() {
        return transfer_http_error_response(id, 400, "missing id", "transfer delete");
    }
    let result =
        tokio::task::spawn_blocking(move || crate::transfer_store::delete_job(&scope, &job_id))
            .await;
    match result {
        Ok(Ok(deleted)) => http_body_response(
            id,
            200,
            serde_json::json!({
                "ok": true,
                "deleted": deleted,
            })
            .to_string(),
            "transfer delete",
        ),
        Ok(Err(error)) => transfer_store_error_response(id, error, "transfer delete"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("transfer delete task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_transfer_upload_commit_response(
    id: String,
    params: Option<&serde_json::Value>,
    runtime: &ControlRuntime,
) -> serde_json::Value {
    let scope = transfer_store_scope(runtime);
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let job_id = transfer_id_param(&params);
    if job_id.is_empty() {
        return transfer_http_error_response(id, 400, "missing id", "transfer upload commit");
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::commit_upload_job(&scope, &job_id)
    })
    .await;
    match result {
        Ok(Ok(job)) => http_body_response(
            id,
            200,
            serde_json::json!({
                "ok": true,
                "job": job,
            })
            .to_string(),
            "transfer upload commit",
        ),
        Ok(Err(error)) => transfer_store_error_response(id, error, "transfer upload commit"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("transfer upload commit task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_transfer_upload_chunk_task_response(
    id: String,
    upload: InboundUploadState,
    runtime: ControlRuntime,
) -> ControlTaskResponse {
    let scope = transfer_store_scope(&runtime);
    let job_id = transfer_id_param(&upload.params);
    if job_id.is_empty() {
        return ControlTaskResponse {
            id: id.clone(),
            frame: transfer_http_error_response(id, 400, "missing id", "transfer upload chunk"),
            byte_stream: None,
            done: true,
        };
    }
    let offset = match optional_u64_param(&upload.params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: transfer_http_error_response(id, 400, error, "transfer upload chunk"),
                byte_stream: None,
                done: true,
            };
        }
    };
    let chunk_len = upload.received_bytes as u64;
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::append_upload_tempfile(
            &scope, &job_id, offset, upload.tmp, chunk_len,
        )
    })
    .await;
    let frame = match result {
        Ok(Ok(job)) => http_body_response(
            id.clone(),
            200,
            serde_json::json!({
                "ok": true,
                "job": job,
            })
            .to_string(),
            "transfer upload chunk",
        ),
        Ok(Err(error)) => transfer_store_error_response(id.clone(), error, "transfer upload chunk"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("transfer upload chunk task failed: {e}"),
        }),
    };
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
    let job_id = transfer_id_param(&params);
    if job_id.is_empty() {
        return transfer_download_error_task_response(id, 400, "missing id");
    }
    let offset = match optional_u64_param(&params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => return transfer_download_error_task_response(id, 400, error),
    };
    let length = match optional_u64_param(&params, &["length", "limit"]) {
        Ok(value) => value,
        Err(error) => return transfer_download_error_task_response(id, 400, error),
    };
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::read_download_range(
            &scope,
            &job_id,
            offset,
            length,
            crate::web_gateway::UPLOAD_MAX_BYTES as u64,
        )
    })
    .await;
    let (job, bytes, end) = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            return transfer_download_error_task_response(id, error.status, error.message);
        }
        Err(e) => {
            return ControlTaskResponse {
                id: id.clone(),
                frame: serde_json::json!({
                    "t": "response",
                    "id": id,
                    "ok": false,
                    "error": format!("transfer download task failed: {e}"),
                }),
                byte_stream: None,
                done: true,
            };
        }
    };
    let content_type = job
        .mime
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let filename = job.filename.clone();
    let total_size = job.total_size.unwrap_or(bytes.len() as u64);
    let size = bytes.len();
    let source_path = job
        .source_path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let job_value = serde_json::to_value(&job).unwrap_or_else(|_| serde_json::json!({}));
    ControlTaskResponse {
        id: id.clone(),
        frame: serde_json::Value::Null,
        byte_stream: Some(ControlByteStream {
            id: id.clone(),
            stream_id: format!("{id}:transfer-download"),
            content_type: content_type.clone(),
            filename: filename.clone(),
            bytes,
            result: serde_json::json!({
                "ok": true,
                "id": job.id,
                "resume_token": job.resume_token,
                "path": source_path,
                "filename": filename,
                "content_type": content_type,
                "size": size,
                "total_size": total_size,
                "offset": offset,
                "range_start": offset,
                "range_end": end,
                "resumable": true,
                "completed_bytes": job.completed_bytes,
                "status": job.status,
                "job": job_value,
            }),
        }),
        done: true,
    }
}

pub(crate) fn transfer_download_error_task_response(
    id: String,
    status: u16,
    error: impl Into<String>,
) -> ControlTaskResponse {
    ControlTaskResponse {
        id: id.clone(),
        frame: transfer_http_error_response(id, status, error, "transfer download"),
        byte_stream: None,
        done: true,
    }
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
    let path = string_param(&params, &["path"]);
    let (status_line, body) = crate::web_gateway::dashboard_fs_mkdir_response_body(&path);
    http_body_response(id, status_line_code(&status_line), body, "filesystem mkdir")
}

pub(crate) async fn api_fs_rename_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::dashboard_fs_rename_response_parts(&params)
    })
    .await;
    match result {
        Ok((code, body)) => http_body_response(id, code, body, "filesystem rename"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("filesystem rename task failed: {e}"),
        }),
    }
}

pub(crate) async fn api_fs_delete_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::dashboard_fs_delete_response_parts(&params)
    })
    .await;
    match result {
        Ok((code, body)) => http_body_response(id, code, body, "filesystem delete"),
        Err(e) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("filesystem delete task failed: {e}"),
        }),
    }
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
    let result = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(upload.tmp.path())?;
        Ok::<_, std::io::Error>(crate::web_gateway::dashboard_fs_write_response_parts(
            &upload.params,
            &bytes,
        ))
    })
    .await;
    let frame = match result {
        Ok(Ok((code, body))) => http_body_response(id.clone(), code, body, "filesystem write"),
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
            Some(project.path()),
            None,
            Some(rt.session_id.as_str()),
            "raw.txt",
            "text/plain",
            crate::upload_store::UploadDestination::Task,
            tmp,
            bytes.len(),
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
        let session_id = format!(
            "dashboard-frame-transfer-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        // Fixture under the process state root, matching what the transfer
        // path resolves (per-process scratch in unit-test builds).
        let session_dir = crate::platform::intendant_home()
            .join("logs")
            .join(&session_id);
        let frames_dir = session_dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        let frame_bytes = b"dashboard frame bytes";
        std::fs::write(frames_dir.join("ann-test.png"), frame_bytes).unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);

        let create = api_transfer_job_create_response(
            "frame-transfer-create".to_string(),
            Some(&serde_json::json!({
                "kind": "download",
                "artifact": {
                    "type": "session_frame_asset",
                    "session_id": &session_id,
                    "filename": "ann-test.png",
                },
            })),
            &rt,
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
        let _ = std::fs::remove_dir_all(&session_dir);
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
}
