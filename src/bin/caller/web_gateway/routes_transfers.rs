//! Transport-neutral cores of the transfer-jobs family (transport-
//! unification design §4 / S9): durable resumable transfers — job list,
//! path-based create (download sources and upload destinations),
//! offset-addressed chunk appends, commit, delete, and ranged download
//! reads — shaped as [`ApiResponse`]s over an injected
//! [`StoreScope`]. The datachannel tunnel's `api_transfer_*` handlers
//! delegate here (S9a, byte-identical per the goldens); the six
//! `/api/transfers` HTTP rows are thin adapters over the same fns
//! (S9b, task #6).
//!
//! Artifact-shaped download creates (session reports, staged uploads,
//! recording/frame assets) resolve against live session handles and
//! stay transport-edge-owned: the tunnel resolves them with its
//! `ControlRuntime` and feeds the result back through
//! [`transfer_job_result_api_response`]; the HTTP lane answers a clear
//! 400 (parity divergence #24).

use super::*;
use crate::global_store::StoreScope;
use crate::transfer_store::{TransferJob, TransferKind, TransferStoreError};

/// The job-handle aliases every transfer method accepts, `id` first
/// (the tunnel's historical precedence). A resume token works anywhere
/// an id does — `transfer_store::find_job` resolves either.
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

/// `{"ok": false, "error": …}` under the canonical json tail — the
/// transfer family's error shape on both lanes (the tunnel injects its
/// `_httpStatus` metadata on top; HTTP carries the status line).
pub(crate) fn transfer_error_api_response(status: u16, message: impl Into<String>) -> ApiResponse {
    ApiResponse::json(
        status,
        JsonBody::Value(serde_json::json!({ "ok": false, "error": message.into() })),
    )
}

fn transfer_store_error_api_response(error: TransferStoreError) -> ApiResponse {
    transfer_error_api_response(error.status, error.message)
}

/// `{"ok": true, "job": <job>}` / store-error shaping shared by create,
/// chunk, commit — and the tunnel's runtime-coupled artifact-create arm.
pub(crate) fn transfer_job_result_api_response(
    result: Result<TransferJob, TransferStoreError>,
) -> ApiResponse {
    match result {
        Ok(job) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "ok": true, "job": job })),
        ),
        Err(error) => transfer_store_error_api_response(error),
    }
}

/// Blocking-task join failures (panic/cancel — unreachable in practice)
/// converge on the enveloped 500, exactly as the fs quartet's
/// conversion settled it (parity divergence #10/#25).
fn transfer_join_error(label: &str, error: tokio::task::JoinError) -> ApiResponse {
    transfer_error_api_response(500, format!("{label} task failed: {error}"))
}

/// List jobs, newest first: `{"ok": true, "jobs": […]}`. A job-handle
/// param (any [`transfer_id_param`] alias) filters to that job — the
/// HTTP row's `?id=` filter; the tunnel gains it additively (its
/// callers historically pass no params, pinned by the goldens).
pub(crate) async fn transfer_jobs_api_response(
    scope: StoreScope,
    params: &serde_json::Value,
) -> ApiResponse {
    let filter = transfer_id_param(params);
    let result =
        tokio::task::spawn_blocking(move || crate::transfer_store::list_jobs(&scope)).await;
    match result {
        Ok(jobs) => {
            let jobs: Vec<TransferJob> = if filter.is_empty() {
                jobs
            } else {
                jobs.into_iter()
                    .filter(|job| job.id == filter || job.resume_token == filter)
                    .collect()
            };
            ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({ "ok": true, "jobs": jobs })),
            )
        }
        Err(e) => transfer_join_error("transfer jobs", e),
    }
}

/// How a create request routes, preserving the tunnel's historical
/// order: the kind parses first (its 400 wins over everything), and
/// only a download create carrying an `artifact` object is
/// runtime-coupled (an upload create's `artifact` key is ignored,
/// exactly as before).
pub(crate) enum TransferCreateRequest {
    Path(TransferKind),
    Artifact(serde_json::Value),
}

pub(crate) fn classify_transfer_create(
    params: &serde_json::Value,
) -> Result<TransferCreateRequest, ApiResponse> {
    let kind = string_param(params, &["kind", "type"]);
    let Some(kind) = TransferKind::from_str(&kind.to_ascii_lowercase()) else {
        return Err(transfer_error_api_response(
            400,
            "transfer kind must be download or upload",
        ));
    };
    if kind == TransferKind::Download {
        if let Some(artifact) = params
            .get("artifact")
            .filter(|value| value.is_object())
            .cloned()
        {
            return Ok(TransferCreateRequest::Artifact(artifact));
        }
    }
    Ok(TransferCreateRequest::Path(kind))
}

/// The upload-create params, decoded with the tunnel's historical
/// lenient alias reads.
fn upload_create_params(
    params: &serde_json::Value,
) -> Result<
    (
        String,
        String,
        String,
        Option<u64>,
        crate::transfer_store::TransferConflictPolicy,
    ),
    TransferStoreError,
> {
    let destination = string_param(
        params,
        &["destination", "destination_path", "destinationPath", "path"],
    );
    if destination.is_empty() {
        return Err(TransferStoreError::new(400, "missing destination"));
    }
    let original_name = optional_string_param(
        params,
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
    let mime = optional_string_param(params, &["mime", "content_type", "contentType"])
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let total_size = optional_u64_param(
        params,
        &[
            "total_size",
            "totalSize",
            "total_bytes",
            "totalBytes",
            "size",
        ],
    )
    .map_err(|error| TransferStoreError::new(400, error))?;
    let conflict = optional_string_param(
        params,
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
                TransferStoreError::new(400, "conflict policy must be fail, rename, or overwrite")
            })?;
    Ok((
        destination,
        original_name,
        mime,
        total_size,
        conflict_policy,
    ))
}

/// The path a path-based create targets — the download source (its
/// param aliases) or the upload destination (its). This is the path
/// the caller's lane gate scope-checks at create; the job-addressed
/// methods (chunk/commit/delete/download) act on that already-scoped
/// destination and carry no path of their own.
// Consumed by the S9b HTTP rows' create gate (this allow leaves with
// them).
#[allow(dead_code)]
pub(crate) fn transfer_create_target_path(
    params: &serde_json::Value,
    kind: TransferKind,
) -> Option<String> {
    match kind {
        TransferKind::Download => {
            optional_string_param(params, &["path", "source_path", "sourcePath", "source"])
        }
        TransferKind::Upload => optional_string_param(
            params,
            &["destination", "destination_path", "destinationPath", "path"],
        ),
    }
}

/// Path-based create, both kinds: a download job for an existing
/// source file, or an upload job (destination + partial file) awaiting
/// chunks. Path authorization is the caller's lane gate.
pub(crate) async fn transfer_path_create_api_response(
    scope: StoreScope,
    params: serde_json::Value,
    kind: TransferKind,
) -> ApiResponse {
    let result = match kind {
        TransferKind::Download => {
            let path = string_param(&params, &["path", "source_path", "sourcePath", "source"]);
            if path.is_empty() {
                return transfer_error_api_response(400, "missing path");
            }
            tokio::task::spawn_blocking(move || {
                crate::transfer_store::create_download_job(&scope, &path)
            })
            .await
        }
        TransferKind::Upload => {
            let (destination, original_name, mime, total_size, conflict_policy) =
                match upload_create_params(&params) {
                    Ok(decoded) => decoded,
                    Err(error) => return transfer_store_error_api_response(error),
                };
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
        }
    };
    match result {
        Ok(result) => transfer_job_result_api_response(result),
        Err(e) => transfer_join_error("transfer create", e),
    }
}

/// The HTTP lane's create composition: the shared classify, path-based
/// creates through the shared core, and the tunnel-only 400 for
/// artifact-shaped creates (parity divergence #24 — their resolution
/// reads live session handles the HTTP edge deliberately lacks). Path
/// authorization is the caller's lane gate, on
/// [`transfer_create_target_path`].
// Consumed by the S9b `POST /api/transfers` row (pinned by the parity
// fixtures meanwhile; this allow leaves with the row).
#[allow(dead_code)]
pub(crate) async fn transfer_job_create_http_api_response(
    scope: StoreScope,
    params: serde_json::Value,
) -> ApiResponse {
    match classify_transfer_create(&params) {
        Err(response) => response,
        Ok(TransferCreateRequest::Artifact(_)) => {
            transfer_error_api_response(400, "artifact transfers require the dashboard tunnel")
        }
        Ok(TransferCreateRequest::Path(kind)) => {
            transfer_path_create_api_response(scope, params, kind).await
        }
    }
}

/// Offset-addressed chunk append: the spooled body — however its
/// transport carried it, upload frames or a raw streamed HTTP body —
/// appends at `offset` (alias `start`; default 0). The destination was
/// path-scoped when the job was created; the chunk names only the job.
pub(crate) async fn transfer_upload_chunk_api_response(
    scope: StoreScope,
    params: &serde_json::Value,
    body: SpooledBody,
) -> ApiResponse {
    let job_id = transfer_id_param(params);
    if job_id.is_empty() {
        return transfer_error_api_response(400, "missing id");
    }
    let offset = match optional_u64_param(params, &["offset", "start"]) {
        Ok(value) => value.unwrap_or(0),
        Err(error) => return transfer_error_api_response(400, error),
    };
    let chunk_len = body.len as u64;
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::append_upload_tempfile(&scope, &job_id, offset, body.tmp, chunk_len)
    })
    .await;
    match result {
        Ok(result) => transfer_job_result_api_response(result),
        Err(e) => transfer_join_error("transfer upload chunk", e),
    }
}

/// Verify and atomically rename the finished upload into place.
pub(crate) async fn transfer_upload_commit_api_response(
    scope: StoreScope,
    params: &serde_json::Value,
) -> ApiResponse {
    let job_id = transfer_id_param(params);
    if job_id.is_empty() {
        return transfer_error_api_response(400, "missing id");
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::commit_upload_job(&scope, &job_id)
    })
    .await;
    match result {
        Ok(result) => transfer_job_result_api_response(result),
        Err(e) => transfer_join_error("transfer upload commit", e),
    }
}

/// Delete a job (cancelling partials / managed artifacts):
/// `{"ok": true, "deleted": <bool>}` — an already-gone job answers
/// `deleted: false`, still 200-shaped.
pub(crate) async fn transfer_job_delete_api_response(
    scope: StoreScope,
    params: &serde_json::Value,
) -> ApiResponse {
    let job_id = transfer_id_param(params);
    if job_id.is_empty() {
        return transfer_error_api_response(400, "missing id");
    }
    let result =
        tokio::task::spawn_blocking(move || crate::transfer_store::delete_job(&scope, &job_id))
            .await;
    match result {
        Ok(Ok(deleted)) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({ "ok": true, "deleted": deleted })),
        ),
        Ok(Err(error)) => transfer_store_error_api_response(error),
        Err(e) => transfer_join_error("transfer delete", e),
    }
}

/// Ranged download read (BYTES lane). Each [`ByteRange`] form keeps its
/// transport's historical semantics:
///
/// - `OffsetLength` — the tunnel's resumable form, exactly as before:
///   store errors (404/416/413) keep their body-only `{"ok":false,…}`
///   shapes.
/// - `HttpHeader` — the end-inclusive header, normalized against the
///   job's source size before the same store read; parse failures
///   answer 416 with the probing `Content-Range: bytes */N` tail
///   (mirroring the fs read's header form — divergence #26).
///
/// Success carries both lanes' decoration: the resume metadata object
/// (`byte_stream_end.result` on the tunnel, byte-identical to the
/// pre-conversion shape) plus the HTTP header tail — `Accept-Ranges`,
/// `Content-Range` + 206 on partials, the `X-Transfer-Range-Start` /
/// `X-Transfer-Range-End` (end-exclusive) / `X-Transfer-Total-Size` /
/// `X-Transfer-Resumable` resume echoes, `X-Content-Sha256` on
/// extent-full reads, and the attachment `Content-Disposition`. Reads
/// are capped at [`UPLOAD_MAX_BYTES`] per request on both forms (413
/// tells the client to range — resumability makes small reads cheap).
pub(crate) async fn transfer_download_read_api_response(
    scope: StoreScope,
    params: &serde_json::Value,
    range: ByteRange,
) -> ApiResponse {
    let job_id = transfer_id_param(params);
    if job_id.is_empty() {
        return transfer_error_api_response(400, "missing id");
    }
    let (offset, length) = match range {
        ByteRange::OffsetLength { offset, length } => (offset, length),
        ByteRange::HttpHeader(header) => {
            match resolve_transfer_range_header(scope.clone(), job_id.clone(), header).await {
                Ok(resolved) => resolved,
                Err(response) => return response,
            }
        }
    };
    let read_scope = scope.clone();
    let read_id = job_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::transfer_store::read_download_range(
            &read_scope,
            &read_id,
            offset,
            length,
            UPLOAD_MAX_BYTES as u64,
        )
    })
    .await;
    let (job, bytes, end) = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return transfer_store_error_api_response(error),
        Err(e) => return transfer_join_error("transfer download", e),
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
    // The tunnel's result sidecar, byte-identical to the
    // pre-conversion shape (the goldens pin it).
    let meta = serde_json::json!({
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
    });
    let full = offset == 0 && end >= total_size;
    let mut headers: Vec<(&'static str, String)> = vec![("Accept-Ranges", "bytes".to_string())];
    if !full && end > offset {
        headers.push((
            "Content-Range",
            format!("bytes {}-{}/{}", offset, end - 1, total_size),
        ));
    }
    headers.push(("X-Transfer-Range-Start", offset.to_string()));
    headers.push(("X-Transfer-Range-End", end.to_string()));
    headers.push(("X-Transfer-Total-Size", total_size.to_string()));
    headers.push(("X-Transfer-Resumable", "true".to_string()));
    if full {
        headers.push(("X-Content-Sha256", fs_sha256_hex(&bytes)));
    }
    if let Some(name) = &filename {
        headers.push((
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", name.replace('"', "")),
        ));
    }
    headers.push(("Cache-Control", "no-cache".to_string()));
    headers.push(("Connection", "close".to_string()));
    ApiResponse::Bytes {
        status: if full { 200 } else { 206 },
        content_type,
        headers,
        bytes: BytesPayload::InMemory(bytes),
        meta,
    }
}

/// Normalize an end-inclusive `Range` header into the store's
/// offset/length form against the download source's current size. An
/// empty/whitespace header and any header against an empty source read
/// full (the fs read's filter semantics); parse failures answer 416
/// with the probing `Content-Range: bytes */N` tail.
async fn resolve_transfer_range_header(
    scope: StoreScope,
    job_id: String,
    header: String,
) -> Result<(u64, Option<u64>), ApiResponse> {
    let source = tokio::task::spawn_blocking(move || {
        crate::transfer_store::download_source(&scope, &job_id)
    })
    .await
    .map_err(|e| transfer_join_error("transfer download", e))?;
    let (_job, _path, total_size) = source.map_err(transfer_store_error_api_response)?;
    let header = header.trim();
    if header.is_empty() || total_size == 0 {
        return Ok((0, None));
    }
    match parse_dashboard_range_header(header, total_size) {
        Ok(range) => Ok((
            range.start,
            Some(range.end.saturating_add(1).saturating_sub(range.start)),
        )),
        Err(message) => Err(ApiResponse::Json {
            status: 416,
            body: JsonBody::Value(serde_json::json!({ "ok": false, "error": message })),
            headers: vec![
                ("Content-Range", format!("bytes */{total_size}")),
                ("Accept-Ranges", "bytes".to_string()),
                ("Cache-Control", "no-cache".to_string()),
                ("Connection", "close".to_string()),
            ],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn project_scope(root: &std::path::Path) -> StoreScope {
        StoreScope::Project(root.to_path_buf())
    }

    fn spooled(bytes: &[u8]) -> SpooledBody {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(bytes).unwrap();
        tmp.flush().unwrap();
        SpooledBody {
            tmp,
            len: bytes.len(),
        }
    }

    fn json_body(response: &ApiResponse) -> (u16, serde_json::Value) {
        match response {
            ApiResponse::Json { status, body, .. } => {
                let text = match body {
                    JsonBody::PreSerialized(text) => text.clone(),
                    JsonBody::Value(value) => value.to_string(),
                };
                (*status, serde_json::from_str(&text).unwrap())
            }
            ApiResponse::Bytes { .. } => panic!("expected a JSON response"),
        }
    }

    fn bytes_parts(
        response: ApiResponse,
    ) -> (
        u16,
        String,
        Vec<(&'static str, String)>,
        Vec<u8>,
        serde_json::Value,
    ) {
        match response {
            ApiResponse::Bytes {
                status,
                content_type,
                headers,
                bytes: BytesPayload::InMemory(bytes),
                meta,
            } => (status, content_type, headers, bytes, meta),
            ApiResponse::Json { status, body, .. } => panic!(
                "expected a bytes response, got {status}: {}",
                match body {
                    JsonBody::PreSerialized(text) => text,
                    JsonBody::Value(value) => value.to_string(),
                }
            ),
        }
    }

    fn header<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header, _)| *header == name)
            .map(|(_, value)| value.as_str())
    }

    /// The resumable upload flow over the neutral fns, exactly as a
    /// resuming client drives it: create → first chunk → re-list (the
    /// received extent survives) → resume at the boundary → commit —
    /// with the stale-offset and premature-commit refusals along the
    /// way.
    #[tokio::test]
    async fn resume_flow_creates_chunks_relists_resumes_and_commits() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let dest_dir = dir.path().join("dest");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        let scope = project_scope(&project);
        let payload = b"resumable transfer payload";
        let (head, tail) = payload.split_at(11);

        let params = serde_json::json!({
            "kind": "upload",
            "destination": dest_dir.join("resumed.bin").to_string_lossy(),
            "name": "resumed.bin",
            "mime": "application/octet-stream",
            "total_size": payload.len(),
        });
        let kind = match classify_transfer_create(&params).ok().unwrap() {
            TransferCreateRequest::Path(kind) => kind,
            TransferCreateRequest::Artifact(_) => panic!("path create"),
        };
        let (status, created) =
            json_body(&transfer_path_create_api_response(scope.clone(), params, kind).await);
        assert_eq!(status, 200);
        let job_id = created["job"]["id"].as_str().unwrap().to_string();
        let resume_token = created["job"]["resume_token"].as_str().unwrap().to_string();

        let (status, first) = json_body(
            &transfer_upload_chunk_api_response(
                scope.clone(),
                &serde_json::json!({ "id": job_id, "offset": 0 }),
                spooled(head),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(first["job"]["completed_bytes"], 11);
        assert_eq!(first["job"]["status"], "running");

        // Premature commit refuses; the partial survives.
        let (status, premature) = json_body(
            &transfer_upload_commit_api_response(
                scope.clone(),
                &serde_json::json!({ "id": job_id }),
            )
            .await,
        );
        assert_eq!(status, 409);
        assert_eq!(
            premature["error"],
            "upload is not complete enough to commit"
        );

        // The client vanished and came back: re-list (filtered by the
        // resume token) and read the received extent off the job.
        let (status, listed) = json_body(
            &transfer_jobs_api_response(
                scope.clone(),
                &serde_json::json!({ "resume_token": resume_token }),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(listed["jobs"].as_array().unwrap().len(), 1);
        let boundary = listed["jobs"][0]["completed_bytes"].as_u64().unwrap();
        assert_eq!(boundary, 11);

        // A stale offset behind the boundary that is not fully covered
        // is refused (the resuming client must continue at the extent).
        let (status, stale) = json_body(
            &transfer_upload_chunk_api_response(
                scope.clone(),
                &serde_json::json!({ "id": job_id, "offset": boundary - 1 }),
                spooled(tail),
            )
            .await,
        );
        assert_eq!(status, 409);
        assert_eq!(
            stale["error"],
            "upload chunk overlaps already persisted bytes"
        );

        let (status, second) = json_body(
            &transfer_upload_chunk_api_response(
                scope.clone(),
                &serde_json::json!({ "resume_token": resume_token, "offset": boundary }),
                spooled(tail),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(second["job"]["status"], "ready");

        let (status, committed) = json_body(
            &transfer_upload_commit_api_response(
                scope.clone(),
                &serde_json::json!({ "id": job_id }),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(committed["job"]["status"], "completed");
        assert_eq!(
            std::fs::read(dest_dir.join("resumed.bin")).unwrap(),
            payload
        );

        // Delete tears the finished job down; a second delete reports
        // deleted: false.
        let (status, deleted) = json_body(
            &transfer_job_delete_api_response(scope.clone(), &serde_json::json!({ "id": job_id }))
                .await,
        );
        assert_eq!(status, 200);
        assert_eq!(deleted["deleted"], true);
        let (_, again) = json_body(
            &transfer_job_delete_api_response(scope.clone(), &serde_json::json!({ "id": job_id }))
                .await,
        );
        assert_eq!(again["deleted"], false);
    }

    #[tokio::test]
    async fn jobs_list_filters_by_id_or_resume_token() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let scope = project_scope(&project);
        let source_a = dir.path().join("a.txt");
        let source_b = dir.path().join("b.txt");
        std::fs::write(&source_a, b"aaa").unwrap();
        std::fs::write(&source_b, b"bbb").unwrap();
        let job_a =
            crate::transfer_store::create_download_job(&scope, source_a.to_str().unwrap()).unwrap();
        let job_b =
            crate::transfer_store::create_download_job(&scope, source_b.to_str().unwrap()).unwrap();

        let (_, all) =
            json_body(&transfer_jobs_api_response(scope.clone(), &serde_json::json!({})).await);
        assert_eq!(all["jobs"].as_array().unwrap().len(), 2);

        for (params, expect) in [
            (serde_json::json!({ "id": job_a.id }), &job_a),
            (
                serde_json::json!({ "resume_token": job_b.resume_token }),
                &job_b,
            ),
        ] {
            let (status, filtered) =
                json_body(&transfer_jobs_api_response(scope.clone(), &params).await);
            assert_eq!(status, 200);
            let jobs = filtered["jobs"].as_array().unwrap();
            assert_eq!(jobs.len(), 1, "{params}");
            assert_eq!(jobs[0]["id"], expect.id.as_str());
        }

        let (status, missing) = json_body(
            &transfer_jobs_api_response(
                scope.clone(),
                &serde_json::json!({ "id": "00000000-0000-0000-0000-000000000000" }),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(missing["jobs"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn download_read_serves_range_header_forms() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let scope = project_scope(&project);
        let source = dir.path().join("payload.txt");
        std::fs::write(&source, b"hello transfer").unwrap();
        let job =
            crate::transfer_store::create_download_job(&scope, source.to_str().unwrap()).unwrap();
        let params = serde_json::json!({ "id": job.id });

        // End-inclusive header → 206 with the standard and resume tails.
        let (status, content_type, headers, bytes, meta) = bytes_parts(
            transfer_download_read_api_response(
                scope.clone(),
                &params,
                ByteRange::HttpHeader("bytes=6-13".to_string()),
            )
            .await,
        );
        assert_eq!(status, 206);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(bytes, b"transfer");
        assert_eq!(header(&headers, "Content-Range"), Some("bytes 6-13/14"));
        assert_eq!(header(&headers, "Accept-Ranges"), Some("bytes"));
        assert_eq!(header(&headers, "X-Transfer-Range-Start"), Some("6"));
        assert_eq!(header(&headers, "X-Transfer-Range-End"), Some("14"));
        assert_eq!(header(&headers, "X-Transfer-Total-Size"), Some("14"));
        assert_eq!(header(&headers, "X-Transfer-Resumable"), Some("true"));
        assert_eq!(header(&headers, "X-Content-Sha256"), None);
        assert_eq!(
            header(&headers, "Content-Disposition"),
            Some("attachment; filename=\"payload.txt\"")
        );
        assert_eq!(meta["range_start"], 6);
        assert_eq!(meta["range_end"], 14);
        assert_eq!(meta["resumable"], true);

        // Open-ended header reads to end.
        let (status, _, _, bytes, _) = bytes_parts(
            transfer_download_read_api_response(
                scope.clone(),
                &params,
                ByteRange::HttpHeader("bytes=6-".to_string()),
            )
            .await,
        );
        assert_eq!(status, 206);
        assert_eq!(bytes, b"transfer");

        // No range → full read: 200, hashed, no Content-Range.
        let (status, _, headers, bytes, meta) = bytes_parts(
            transfer_download_read_api_response(
                scope.clone(),
                &params,
                ByteRange::OffsetLength {
                    offset: 0,
                    length: None,
                },
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(bytes, b"hello transfer");
        assert_eq!(header(&headers, "Content-Range"), None);
        assert_eq!(
            header(&headers, "X-Content-Sha256"),
            Some(fs_sha256_hex(b"hello transfer").as_str())
        );
        assert_eq!(meta["status"], "completed");

        // Unparseable header → 416 with the probing Content-Range tail.
        let response = transfer_download_read_api_response(
            scope.clone(),
            &params,
            ByteRange::HttpHeader("lines=1-2".to_string()),
        )
        .await;
        match response {
            ApiResponse::Json {
                status,
                body,
                headers,
            } => {
                assert_eq!(status, 416);
                let body: serde_json::Value = serde_json::from_str(&body.into_string()).unwrap();
                assert_eq!(body["error"], "Range must use bytes");
                assert_eq!(body["ok"], false);
                assert_eq!(
                    headers
                        .iter()
                        .find(|(name, _)| *name == "Content-Range")
                        .map(|(_, value)| value.as_str()),
                    Some("bytes */14")
                );
            }
            ApiResponse::Bytes { .. } => panic!("expected the 416 JSON shape"),
        }

        // Unknown job on the header form resolves before parsing.
        let (status, body) = json_body(
            &transfer_download_read_api_response(
                scope.clone(),
                &serde_json::json!({ "id": "00000000-0000-0000-0000-000000000000" }),
                ByteRange::HttpHeader("bytes=0-1".to_string()),
            )
            .await,
        );
        assert_eq!(status, 404);
        assert_eq!(body["error"], "transfer job not found");
    }
}
