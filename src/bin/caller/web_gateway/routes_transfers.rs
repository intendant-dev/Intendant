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

/// Per-request body cap for `POST /api/transfers/{id}/chunk` (design
/// §4): bounds what one chunk may spool to memory/disk. Deliberately
/// far under the staged-attachment cap — resumability makes small
/// chunks cheap, and N capped chunks + commit is exactly how an
/// over-100-MiB upload rides direct HTTP (task #6). Pinned (with the
/// row's `Streaming` policy) by the gateway body-cap test.
pub(crate) const TRANSFER_HTTP_CHUNK_MAX_BYTES: usize = 32 * 1024 * 1024;

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

/// The one denial every failed job-scope re-check surfaces —
/// unresolvable handle, artifact-shaped job, or out-of-scope path all
/// read identically, so a denial never becomes a job-existence or
/// job-path oracle for a scope-restricted caller. Both lanes emit this
/// string verbatim (the mirror tests pin it).
pub(crate) const TRANSFER_JOB_SCOPE_DENIED: &str =
    "transfer job is outside the granted filesystem scope";

/// The filesystem access a job-addressed transfer method exercises on
/// the resolved job (the 2026-07-11 job-path re-check: scope-restricted
/// callers act on a job by re-checking the job's *real* filesystem path
/// under the standard scope rules, replacing the old blanket pathless
/// denial that made the flow unusable for exactly the principals it was
/// scoped for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferJobAccess {
    /// Ranged download reads: read the job's download source.
    ReadSource,
    /// List/status visibility: read the job's user path (a write scope
    /// implies read under `filesystem_access_allowed`, so a write-only
    /// grant still sees the upload jobs it can chunk into).
    ReadJobPath,
    /// Upload chunk appends and commits: write the upload destination.
    WriteDestination,
    /// Delete: write the job's user path (upload destination, else
    /// download source — cancelling a job is authority over its file).
    WriteJobPath,
}

impl TransferJobAccess {
    fn kind(self) -> crate::peer::access_policy::FilesystemAccessKind {
        match self {
            Self::ReadSource | Self::ReadJobPath => {
                crate::peer::access_policy::FilesystemAccessKind::Read
            }
            Self::WriteDestination | Self::WriteJobPath => {
                crate::peer::access_policy::FilesystemAccessKind::Write
            }
        }
    }
}

/// The transfer's *real* user-filesystem path for a scope re-check —
/// the job record's download source / upload destination, never the
/// store's JSON record location (`transfer_store::job_path`). `None`
/// (⇒ deny fail-closed) for artifact-shaped jobs — daemon-materialized
/// or artifact-resolved sources (session reports, staged uploads,
/// recording/frame assets; their creates are tunnel-only, divergence
/// #24) live in daemon-internal stores no user grant names — and for
/// jobs lacking the rule's path (e.g. a chunk aimed at a download job).
fn transfer_job_scope_path(
    job: &TransferJob,
    access: TransferJobAccess,
) -> Option<&std::path::Path> {
    if job.managed_source || job.artifact.is_some() {
        return None;
    }
    match access {
        TransferJobAccess::ReadSource => job.source_path.as_deref(),
        TransferJobAccess::WriteDestination => job.destination_path.as_deref(),
        TransferJobAccess::ReadJobPath | TransferJobAccess::WriteJobPath => job
            .destination_path
            .as_deref()
            .or(job.source_path.as_deref()),
    }
}

/// Whether one already-loaded job passes a scope-restricted caller's
/// policy under `access` — the list row's filter predicate and the core
/// of [`check_scoped_transfer_job`].
fn transfer_job_passes_scope(
    policy: &crate::peer::access_policy::FilesystemAccessPolicy,
    job: &TransferJob,
    access: TransferJobAccess,
) -> bool {
    let Some(path) = transfer_job_scope_path(job, access) else {
        return false;
    };
    let subject = match access {
        // List/status visibility must include in-flight upload jobs,
        // whose final destination is not on disk yet: judge the
        // nearest existing ancestor, exactly as the Write kind
        // normalizes inside `filesystem_access_allowed` — the strict
        // per-row checks still gate the actual IO.
        TransferJobAccess::ReadJobPath => {
            match path.ancestors().find(|candidate| candidate.exists()) {
                Some(existing) => existing,
                None => return false,
            }
        }
        _ => path,
    };
    crate::peer::access_policy::filesystem_access_allowed(policy, access.kind(), subject).is_ok()
}

/// A job-addressed re-check's outcome. `path` is the job's resolved
/// real filesystem path whenever the handle resolved to one — the
/// lanes' audit trails log it even on an out-of-scope denial — and a
/// denial always reads [`TRANSFER_JOB_SCOPE_DENIED`] verbatim.
pub(crate) struct TransferJobScopeCheck {
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) allowed: bool,
}

/// Resolve and scope-check one job-addressed transfer method for a
/// scope-restricted caller — the shared, transport-neutral twin of the
/// create-time target check. Both lanes call exactly this fn (HTTP's
/// job-addressed rows and the tunnel's `api_transfer_*` authorizer), so
/// their decisions and denial wording are identical by construction.
/// The handle resolves by id or resume token through the *lane's own*
/// store scope (`http_transfer_scope` / the tunnel's
/// `transfer_store_scope`); unrestricted principals never reach here —
/// their job-addressed rows stay op-only, with no store lookup.
pub(crate) fn check_scoped_transfer_job(
    store: &StoreScope,
    policy: &crate::peer::access_policy::FilesystemAccessPolicy,
    handle: &str,
    access: TransferJobAccess,
) -> TransferJobScopeCheck {
    let Some(job) = crate::transfer_store::find_job(store, handle) else {
        return TransferJobScopeCheck {
            path: None,
            allowed: false,
        };
    };
    TransferJobScopeCheck {
        path: transfer_job_scope_path(&job, access).map(std::path::Path::to_path_buf),
        allowed: transfer_job_passes_scope(policy, &job, access),
    }
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
///
/// `fs_scope` is the caller's filesystem policy when the caller is
/// scope-restricted (`None` = unrestricted, listing unchanged): the
/// listing is never blanket-denied for scoped callers — it is filtered
/// to the jobs whose real paths pass the caller's read scope
/// ([`TransferJobAccess::ReadJobPath`]; artifact-shaped jobs are
/// daemon-internal and never listed). The `?id=`/resume-token form gets
/// the same per-job re-check *as a filter*: an out-of-scope or
/// unresolvable handle yields an empty 200, never a denial that would
/// oracle job existence. Both lanes pass their own caller's policy, so
/// the filtered listings are identical.
pub(crate) async fn transfer_jobs_api_response(
    scope: StoreScope,
    params: &serde_json::Value,
    fs_scope: Option<&crate::peer::access_policy::FilesystemAccessPolicy>,
) -> ApiResponse {
    let filter = transfer_id_param(params);
    let policy = fs_scope.cloned();
    let result = tokio::task::spawn_blocking(move || {
        let jobs = crate::transfer_store::list_jobs(&scope);
        match policy {
            Some(policy) => jobs
                .into_iter()
                .filter(|job| {
                    transfer_job_passes_scope(&policy, job, TransferJobAccess::ReadJobPath)
                })
                .collect(),
            None => jobs,
        }
    })
    .await;
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
struct UploadCreateParams {
    destination: String,
    original_name: String,
    mime: String,
    total_size: Option<u64>,
    /// Declared content hash, verified at commit (design §4; the store
    /// normalizes/validates it).
    sha256: Option<String>,
    conflict_policy: crate::transfer_store::TransferConflictPolicy,
}

fn upload_create_params(
    params: &serde_json::Value,
) -> Result<UploadCreateParams, TransferStoreError> {
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
    let sha256 = optional_string_param(params, &["sha256"]);
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
    Ok(UploadCreateParams {
        destination,
        original_name,
        mime,
        total_size,
        sha256,
        conflict_policy,
    })
}

/// The path a path-based create targets — the download source (its
/// param aliases) or the upload destination (its). This is the path
/// the caller's lane gate scope-checks at create; the job-addressed
/// methods (chunk/commit/delete/download) carry no path of their own —
/// for scope-restricted callers each re-checks the resolved job's real
/// path instead ([`check_scoped_transfer_job`]).
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
            let decoded = match upload_create_params(&params) {
                Ok(decoded) => decoded,
                Err(error) => return transfer_store_error_api_response(error),
            };
            tokio::task::spawn_blocking(move || {
                crate::transfer_store::create_upload_job(
                    &scope,
                    &decoded.destination,
                    &decoded.original_name,
                    &decoded.mime,
                    decoded.total_size,
                    decoded.sha256,
                    decoded.conflict_policy,
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

// ── The HTTP lane: thin adapters over the cores above (S9b, the §4
//    rows). Transport edges only — query/body/Range parsing, the
//    transfer-family authorization mirror, store-scope resolution —
//    every response body comes from the shared fns. ──

/// What a transfer row authorizes for scope-restricted callers (a peer
/// identity or an fs-scoped IAM grant; unrestricted principals pass on
/// the row's operation alone, which the pre-dispatch gate checked).
pub(crate) enum TransferAccessTarget<'a> {
    /// POST create: the kind-aware target path the create will act on.
    /// `None` — an artifact-shaped or unparseable create — denies
    /// scope-restricted callers fail-closed, wording unchanged.
    Create(Option<&'a str>),
    /// A job-addressed row: re-check the resolved job's real path in
    /// the lane's store under the row's access rule
    /// ([`check_scoped_transfer_job`]).
    Job {
        store: &'a StoreScope,
        handle: &'a str,
        access: TransferJobAccess,
    },
}

/// The transfer family's fs gate, mirroring the tunnel's
/// `authorize_dashboard_control_filesystem` exactly:
///
/// - **create** names a target path (`Create(Some)`) — scope-checked
///   (+ audited) through [`authorize_http_filesystem_access`],
///   write-kind for both job kinds, exactly as the tunnel derives kind
///   from the method's operation; a pathless create (artifact-shaped —
///   tunnel-only, divergence #24 — or unparseable) keeps the historical
///   fail-closed denial for scope-restricted callers;
/// - the **job-addressed** methods name only a job: a scope-restricted
///   caller is re-checked against the resolved job's real filesystem
///   path through the shared [`check_scoped_transfer_job`] (the tunnel
///   authorizer calls the same fn), denying fail-closed — with one
///   uniform wording — on an unresolvable handle, an artifact-shaped
///   job, or an out-of-scope path, while unrestricted principals pass
///   on the row's operation alone with no store lookup.
///
/// `pub(crate)` so the tunnel's parity fixtures can pin both lanes'
/// decisions against each other.
pub(crate) fn authorize_http_transfer_access(
    access: &HttpAccessContext,
    identity: Option<&PeerConnectionIdentity>,
    op: crate::peer::access_policy::PeerOperation,
    target: TransferAccessTarget<'_>,
    bus: &EventBus,
) -> Result<(), String> {
    let (store, handle, job_access) = match target {
        TransferAccessTarget::Create(Some(path)) => {
            return authorize_http_filesystem_access(
                access,
                identity,
                op,
                crate::peer::access_policy::FilesystemAccessKind::Write,
                path,
                bus,
            );
        }
        TransferAccessTarget::Create(None) => {
            let denied = "filesystem request missing path".to_string();
            if let Some(identity) = identity {
                audit_peer_filesystem_access(bus, identity, op, "", false, &denied);
                return Err(denied);
            }
            if http_transfer_fs_scope(access, None).is_some() {
                bus.send(AppEvent::PresenceLog {
                    message: format!(
                        "[grant-fs] denied principal={} op={:?} path= detail={}",
                        access.principal.label, op, denied
                    ),
                    level: Some(LogLevel::Warn),
                    turn: None,
                });
                return Err(denied);
            }
            return Ok(());
        }
        TransferAccessTarget::Job {
            store,
            handle,
            access,
        } => (store, handle, access),
    };
    if let Some(identity) = identity {
        let check = check_scoped_transfer_job(store, &identity.filesystem, handle, job_access);
        // Audit the job's resolved real path when the handle resolved
        // to one (even on an out-of-scope denial); the raw handle is
        // the best identity an unresolvable denial has.
        let audit_path = check
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| handle.to_string());
        if check.allowed {
            audit_peer_filesystem_access(bus, identity, op, &audit_path, true, "allowed");
            return Ok(());
        }
        audit_peer_filesystem_access(
            bus,
            identity,
            op,
            &audit_path,
            false,
            TRANSFER_JOB_SCOPE_DENIED,
        );
        return Err(TRANSFER_JOB_SCOPE_DENIED.to_string());
    }
    let Some(policy) = http_transfer_fs_scope(access, None) else {
        return Ok(());
    };
    let check = check_scoped_transfer_job(store, policy, handle, job_access);
    if check.allowed {
        return Ok(());
    }
    let audit_path = check
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| handle.to_string());
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[grant-fs] denied principal={} op={:?} path={} detail={}",
            access.principal.label, op, audit_path, TRANSFER_JOB_SCOPE_DENIED
        ),
        level: Some(LogLevel::Warn),
        turn: None,
    });
    Err(TRANSFER_JOB_SCOPE_DENIED.to_string())
}

/// The caller's filesystem scope on this request, `None` when the
/// caller is unrestricted: a peer identity's connection policy, else an
/// fs-scoped IAM grant's scope.
fn http_transfer_fs_scope<'a>(
    access: &'a HttpAccessContext,
    identity: Option<&'a PeerConnectionIdentity>,
) -> Option<&'a crate::peer::access_policy::FilesystemAccessPolicy> {
    if let Some(identity) = identity {
        return Some(&identity.filesystem);
    }
    access
        .iam_state
        .as_ref()
        .and_then(|state| crate::access::iam::fs_scope_for_principal(state, &access.principal))
}

/// Transport edge: where transfer jobs persist for this daemon — the
/// project store when rooted, the daemon-global fallback otherwise
/// (`transfer_store_scope`'s HTTP twin; the cores take the result
/// injected).
fn http_transfer_scope(project_root: Option<&std::path::Path>) -> StoreScope {
    StoreScope::resolve(project_root)
}

/// `GET /api/transfers` — list jobs (`?id=`/`?resume_token=` filter).
/// Never blanket-denied for scope-restricted callers: the shared list
/// core filters the listing to the jobs whose real paths pass the
/// caller's read scope (the `?id=` form gets the same per-job re-check
/// as a filter); unrestricted principals list unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_transfer_jobs(
    stream: DemuxStream,
    request_line: &str,
    project_root: Option<std::path::PathBuf>,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let mut params = serde_json::Map::new();
    for key in ["id", "resume_token"] {
        if let Some(value) = query_param(request_line, key) {
            params.insert(key.to_string(), serde_json::Value::String(value));
        }
    }
    let params = serde_json::Value::Object(params);
    let fs_scope = http_transfer_fs_scope(&http_access_context, peer_connection_identity.as_ref());
    if let Some(identity) = peer_connection_identity.as_ref() {
        // The scope-filtered listing is an allowed read; leave the
        // same [peer-fs] trail line the other filesystem reads leave.
        audit_peer_filesystem_access(
            &bus,
            identity,
            crate::peer::access_policy::PeerOperation::FilesystemRead,
            "",
            true,
            "allowed",
        );
    }
    let response = transfer_jobs_api_response(
        http_transfer_scope(project_root.as_deref()),
        &params,
        fs_scope,
    )
    .await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// `POST /api/transfers` — create a job from the JSON body (the
/// tunnel's params shape verbatim). The target path is scope-checked
/// here, at create, for both kinds; artifact-shaped creates are
/// tunnel-only (divergence #24).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_transfer_job_create(
    stream: DemuxStream,
    body_text: String,
    project_root: Option<std::path::PathBuf>,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match serde_json::from_str::<serde_json::Value>(&body_text) {
        Ok(params) if params.is_object() => {
            // The scope gate sees the path the create will actually
            // target (kind-aware aliases); a kind that fails to parse
            // falls through pathless — fail-closed for scoped callers,
            // the shared 400 for everyone else.
            let target = classify_transfer_create(&params)
                .ok()
                .and_then(|request| match request {
                    TransferCreateRequest::Path(kind) => transfer_create_target_path(&params, kind),
                    TransferCreateRequest::Artifact(_) => None,
                });
            match authorize_http_transfer_access(
                &http_access_context,
                peer_connection_identity.as_ref(),
                crate::peer::access_policy::PeerOperation::FilesystemWrite,
                TransferAccessTarget::Create(target.as_deref()),
                &bus,
            ) {
                Ok(()) => {
                    transfer_job_create_http_api_response(
                        http_transfer_scope(project_root.as_deref()),
                        params,
                    )
                    .await
                }
                Err(message) => transfer_error_api_response(403, message),
            }
        }
        Ok(_) => transfer_error_api_response(400, "request body must be a JSON object"),
        Err(e) => transfer_error_api_response(400, format!("invalid JSON: {e}")),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// `POST /api/transfers/{id}/chunk?offset=N[&resume_token=…]` — spool
/// the raw body (S8's `SpooledBody` lane, capped at
/// [`TRANSFER_HTTP_CHUNK_MAX_BYTES`]) and append it through the shared
/// core. Chunk auth is the row's operation plus, for scope-restricted
/// callers, the job-path re-check: the resolved job's destination must
/// pass the caller's write scope (denied before the body is read).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_transfer_upload_chunk(
    mut stream: DemuxStream,
    header_text: &str,
    request_line: &str,
    discard: Vec<u8>,
    job_id: String,
    project_root: Option<std::path::PathBuf>,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    use tokio::io::AsyncWriteExt;
    let scope = http_transfer_scope(project_root.as_deref());
    if let Err(message) = authorize_http_transfer_access(
        &http_access_context,
        peer_connection_identity.as_ref(),
        crate::peer::access_policy::PeerOperation::FilesystemWrite,
        TransferAccessTarget::Job {
            store: &scope,
            handle: &job_id,
            access: TransferJobAccess::WriteDestination,
        },
        &bus,
    ) {
        write_api_response(
            stream,
            transfer_error_api_response(403, message),
            cors,
            fleet_origin,
        )
        .await;
        return;
    }
    let mut params = serde_json::Map::new();
    params.insert("id".to_string(), serde_json::Value::String(job_id));
    for key in ["offset", "resume_token"] {
        if let Some(value) = query_param(request_line, key) {
            params.insert(key.to_string(), serde_json::Value::String(value));
        }
    }
    let params = serde_json::Value::Object(params);
    if header_text
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("expect: 100-continue"))
    {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
    }
    let response = match stream_body_to_tempfile(
        header_text,
        &discard,
        &mut stream,
        TRANSFER_HTTP_CHUNK_MAX_BYTES,
    )
    .await
    {
        Err(e) => {
            let status = if e.contains("too large") { 413 } else { 400 };
            transfer_error_api_response(status, e)
        }
        Ok(body) => transfer_upload_chunk_api_response(scope, &params, body).await,
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// `POST /api/transfers/{id}/commit` — verify and place the finished
/// upload. An optional JSON body may carry extra params; the path
/// capture is the job handle. Scope-restricted callers are re-checked
/// against the job's destination path (write kind) before anything
/// parses.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_transfer_upload_commit(
    stream: DemuxStream,
    body_text: String,
    job_id: String,
    project_root: Option<std::path::PathBuf>,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let scope = http_transfer_scope(project_root.as_deref());
    if let Err(message) = authorize_http_transfer_access(
        &http_access_context,
        peer_connection_identity.as_ref(),
        crate::peer::access_policy::PeerOperation::FilesystemWrite,
        TransferAccessTarget::Job {
            store: &scope,
            handle: &job_id,
            access: TransferJobAccess::WriteDestination,
        },
        &bus,
    ) {
        write_api_response(
            stream,
            transfer_error_api_response(403, message),
            cors,
            fleet_origin,
        )
        .await;
        return;
    }
    let params = if body_text.trim().is_empty() {
        Ok(serde_json::Map::new())
    } else {
        match serde_json::from_str::<serde_json::Value>(&body_text) {
            Ok(serde_json::Value::Object(map)) => Ok(map),
            Ok(_) => Err("request body must be a JSON object".to_string()),
            Err(e) => Err(format!("invalid JSON: {e}")),
        }
    };
    let response = match params {
        Ok(mut params) => {
            params.insert("id".to_string(), serde_json::Value::String(job_id));
            transfer_upload_commit_api_response(scope, &serde_json::Value::Object(params)).await
        }
        Err(message) => transfer_error_api_response(400, message),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// `DELETE /api/transfers/{id}` (+ the WKWebView POST
/// `/api/transfers/{id}/delete` fallback — both shapes capture the same
/// id and share this handler). Scope-restricted callers are re-checked
/// against the job's user path (write kind).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_transfer_job_delete(
    stream: DemuxStream,
    job_id: String,
    project_root: Option<std::path::PathBuf>,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let scope = http_transfer_scope(project_root.as_deref());
    if let Err(message) = authorize_http_transfer_access(
        &http_access_context,
        peer_connection_identity.as_ref(),
        crate::peer::access_policy::PeerOperation::FilesystemWrite,
        TransferAccessTarget::Job {
            store: &scope,
            handle: &job_id,
            access: TransferJobAccess::WriteJobPath,
        },
        &bus,
    ) {
        write_api_response(
            stream,
            transfer_error_api_response(403, message),
            cors,
            fleet_origin,
        )
        .await;
        return;
    }
    let response =
        transfer_job_delete_api_response(scope, &serde_json::json!({ "id": job_id })).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// `GET /api/transfers/{id}/download` — ranged read: an HTTP `Range`
/// header takes precedence (it is the protocol's range mechanism);
/// otherwise `?offset=&length=`; otherwise the full (capped) extent.
/// Scope-restricted callers are re-checked against the job's source
/// path (read kind).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_transfer_download_read(
    stream: DemuxStream,
    header_text: &str,
    request_line: &str,
    job_id: String,
    project_root: Option<std::path::PathBuf>,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let scope = http_transfer_scope(project_root.as_deref());
    if let Err(message) = authorize_http_transfer_access(
        &http_access_context,
        peer_connection_identity.as_ref(),
        crate::peer::access_policy::PeerOperation::FilesystemRead,
        TransferAccessTarget::Job {
            store: &scope,
            handle: &job_id,
            access: TransferJobAccess::ReadSource,
        },
        &bus,
    ) {
        write_api_response(
            stream,
            transfer_error_api_response(403, message),
            cors,
            fleet_origin,
        )
        .await;
        return;
    }
    let params = serde_json::json!({ "id": job_id });
    let range = if let Some(header) = dashboard_http_header_value(header_text, "range") {
        Ok(ByteRange::HttpHeader(header.to_string()))
    } else {
        let query = serde_json::json!({
            "offset": query_param(request_line, "offset"),
            "length": query_param(request_line, "length"),
        });
        match (
            optional_u64_param(&query, &["offset"]),
            optional_u64_param(&query, &["length"]),
        ) {
            (Ok(offset), Ok(length)) => Ok(ByteRange::OffsetLength {
                offset: offset.unwrap_or(0),
                length,
            }),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    };
    let response = match range {
        Ok(range) => transfer_download_read_api_response(scope, &params, range).await,
        Err(message) => transfer_error_api_response(400, message),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
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
            ApiResponse::Bytes { .. } | ApiResponse::Stream { .. } => {
                panic!("expected a JSON response")
            }
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
            ApiResponse::Stream { .. } => panic!("expected a bytes response, got a stream"),
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
        // resume token) and read the received extent off the job. The
        // status poll works scope-filtered too — a write-only grant on
        // the destination still sees its own in-flight job (write
        // implies read; the pending destination is judged by its
        // nearest existing ancestor).
        let write_only = crate::peer::access_policy::FilesystemAccessPolicy {
            read_roots: vec![],
            write_roots: vec![dest_dir.clone()],
        };
        let (status, listed) = json_body(
            &transfer_jobs_api_response(
                scope.clone(),
                &serde_json::json!({ "resume_token": resume_token }),
                Some(&write_only),
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

    /// The HTTP lane's transfer-family gate mirrors the tunnel's
    /// `authorize_dashboard_control_filesystem` exactly (both lanes run
    /// the shared [`check_scoped_transfer_job`]): unrestricted
    /// principals pass on the row's operation alone, with no store
    /// lookup; scope-restricted callers (fs-scoped grants, peer
    /// identities) are scope-checked on the create target and re-checked
    /// against the resolved job's real filesystem path on the
    /// job-addressed rows — in-scope jobs pass each row's access rule,
    /// while out-of-scope, artifact-shaped, and unresolvable handles are
    /// denied fail-closed with one uniform wording.
    #[test]
    fn transfer_authorization_mirrors_the_tunnel_scope_recheck_rule() {
        use crate::peer::access_policy::PeerOperation;
        let bus = crate::event::EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let in_scope = dir.path().join("shared");
        let outside = dir.path().join("outside");
        let project = dir.path().join("project");
        for path in [&in_scope, &outside, &project] {
            std::fs::create_dir_all(path).unwrap();
        }
        let store = project_scope(&project);

        // Fixtures: a download job with an in-scope source, an upload
        // job with an in-scope destination, an out-of-scope download
        // job, and an artifact-shaped (daemon-materialized) job.
        std::fs::write(in_scope.join("data.txt"), b"in scope").unwrap();
        std::fs::write(outside.join("secret.txt"), b"out of scope").unwrap();
        let dl_in = crate::transfer_store::create_download_job(
            &store,
            in_scope.join("data.txt").to_str().unwrap(),
        )
        .unwrap();
        let up_in = crate::transfer_store::create_upload_job(
            &store,
            in_scope.join("up.bin").to_str().unwrap(),
            "up.bin",
            "application/octet-stream",
            Some(4),
            None,
            crate::transfer_store::TransferConflictPolicy::Fail,
        )
        .unwrap();
        let dl_out = crate::transfer_store::create_download_job(
            &store,
            outside.join("secret.txt").to_str().unwrap(),
        )
        .unwrap();
        let artifact = crate::transfer_store::create_download_job_from_bytes(
            &store,
            b"report".to_vec(),
            "report.zip",
            "application/zip",
            "session_report",
            None,
            Some(serde_json::json!({ "type": "session_report" })),
        )
        .unwrap();
        let unresolvable = "00000000-0000-0000-0000-000000000000";

        // Unrestricted (root-session) context: job-addressed rows and
        // any create target pass on the operation alone — even an
        // unresolvable handle proves no store lookup gates them.
        let root = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                "unit-test",
                "https",
            ),
            iam_state: None,
        };
        for (op, access) in [
            (PeerOperation::FilesystemRead, TransferJobAccess::ReadSource),
            (
                PeerOperation::FilesystemWrite,
                TransferJobAccess::WriteDestination,
            ),
            (
                PeerOperation::FilesystemWrite,
                TransferJobAccess::WriteJobPath,
            ),
        ] {
            assert!(authorize_http_transfer_access(
                &root,
                None,
                op,
                TransferAccessTarget::Job {
                    store: &store,
                    handle: unresolvable,
                    access,
                },
                &bus
            )
            .is_ok());
        }
        assert!(authorize_http_transfer_access(
            &root,
            None,
            PeerOperation::FilesystemWrite,
            TransferAccessTarget::Create(None),
            &bus
        )
        .is_ok());
        assert!(authorize_http_transfer_access(
            &root,
            None,
            PeerOperation::FilesystemWrite,
            TransferAccessTarget::Create(Some(&in_scope.join("up.bin").to_string_lossy())),
            &bus
        )
        .is_ok());

        // An fs-scoped user-client grant: in-scope create passes,
        // out-of-scope create is refused, and a pathless create keeps
        // the historical fail-closed wording.
        let mut state = crate::access::iam::LocalIamState::default();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("admin", "https");
        let result = crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:11".to_string()),
                role_id: Some("role:operator".to_string()),
                fs_write_roots: vec![in_scope.to_string_lossy().to_string()],
                fs_read_roots: vec![in_scope.to_string_lossy().to_string()],
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let scoped = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal {
                grant_id: Some(result.grant.id.clone()),
                ..crate::access::iam::AccessPrincipal::root_dashboard_session("scoped", "https")
            },
            iam_state: Some(state),
        };
        assert!(authorize_http_transfer_access(
            &scoped,
            None,
            PeerOperation::FilesystemWrite,
            TransferAccessTarget::Create(Some(&in_scope.join("up2.bin").to_string_lossy())),
            &bus
        )
        .is_ok());
        assert!(authorize_http_transfer_access(
            &scoped,
            None,
            PeerOperation::FilesystemWrite,
            TransferAccessTarget::Create(Some(&outside.join("escape.bin").to_string_lossy())),
            &bus
        )
        .is_err());
        assert_eq!(
            authorize_http_transfer_access(
                &scoped,
                None,
                PeerOperation::FilesystemWrite,
                TransferAccessTarget::Create(None),
                &bus
            )
            .unwrap_err(),
            "filesystem request missing path",
        );

        // The job-addressed rows re-check the resolved job's real path,
        // for the fs-scoped grant and for a peer identity carrying the
        // same policy — identical decisions and wording on both.
        let peer = PeerConnectionIdentity {
            fingerprint: "aabbccdd".to_string(),
            label: "peer".to_string(),
            profile: "file-operator".to_string(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy {
                read_roots: vec![in_scope.clone()],
                write_roots: vec![in_scope.clone()],
            },
        };
        let authorize = |identity: Option<&PeerConnectionIdentity>,
                         op: PeerOperation,
                         handle: &str,
                         access: TransferJobAccess| {
            let context = if identity.is_some() { &root } else { &scoped };
            authorize_http_transfer_access(
                context,
                identity,
                op,
                TransferAccessTarget::Job {
                    store: &store,
                    handle,
                    access,
                },
                &bus,
            )
        };
        for identity in [None, Some(&peer)] {
            // Download read (read-checks the source): the in-scope
            // download passes; an upload job has no source.
            let read = TransferJobAccess::ReadSource;
            assert!(authorize(identity, PeerOperation::FilesystemRead, &dl_in.id, read).is_ok());
            assert!(authorize(identity, PeerOperation::FilesystemRead, &up_in.id, read).is_err());

            // Chunk/commit (write-check the destination): the in-scope
            // upload passes (its resume token works anywhere its id
            // does); a download job has no destination.
            let write = TransferJobAccess::WriteDestination;
            assert!(authorize(identity, PeerOperation::FilesystemWrite, &up_in.id, write).is_ok());
            assert!(authorize(
                identity,
                PeerOperation::FilesystemWrite,
                &up_in.resume_token,
                write
            )
            .is_ok());
            assert!(authorize(identity, PeerOperation::FilesystemWrite, &dl_in.id, write).is_err());

            // Delete (write-checks the job's user path): both in-scope
            // jobs pass.
            let delete = TransferJobAccess::WriteJobPath;
            for job_id in [&up_in.id, &dl_in.id] {
                assert!(
                    authorize(identity, PeerOperation::FilesystemWrite, job_id, delete).is_ok()
                );
            }

            // Out-of-scope, artifact-shaped, and unresolvable handles
            // deny fail-closed with the one uniform wording on every
            // job-addressed rule.
            for access in [read, write, delete] {
                let op = match access {
                    TransferJobAccess::ReadSource => PeerOperation::FilesystemRead,
                    _ => PeerOperation::FilesystemWrite,
                };
                for handle in [dl_out.id.as_str(), artifact.id.as_str(), unresolvable, ""] {
                    assert_eq!(
                        authorize(identity, op, handle, access).unwrap_err(),
                        TRANSFER_JOB_SCOPE_DENIED,
                        "{access:?} {handle:?} peer={}",
                        identity.is_some(),
                    );
                }
            }
        }
    }

    /// Raw HTTP transcripts for the rows' distinctive wire shapes
    /// (design §8 goldens: status lines, header order, bodies): the
    /// download row's 200/206/416 forms and the commit row's
    /// sha-mismatch 409, rendered through the one HTTP adapter the
    /// dispatch arms use.
    #[tokio::test]
    async fn golden_http_transcripts_pin_download_and_commit_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let dest_dir = dir.path().join("dest");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        let scope = project_scope(&project);
        let source = dir.path().join("payload.txt");
        std::fs::write(&source, b"hello transfer").unwrap();
        let job =
            crate::transfer_store::create_download_job(&scope, source.to_str().unwrap()).unwrap();
        let params = serde_json::json!({ "id": job.id });
        let transcript = |response: ApiResponse| {
            String::from_utf8(crate::web_gateway::api_response_http_bytes(
                response,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            ))
            .unwrap()
        };

        // Full read: exact head (status line + header order) and body.
        let full = transcript(
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
        let sha = fs_sha256_hex(b"hello transfer");
        let expected_full = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 14\r\n\
             Accept-Ranges: bytes\r\n\
             X-Transfer-Range-Start: 0\r\n\
             X-Transfer-Range-End: 14\r\n\
             X-Transfer-Total-Size: 14\r\n\
             X-Transfer-Resumable: true\r\n\
             X-Content-Sha256: {sha}\r\n\
             Content-Disposition: attachment; filename=\"payload.txt\"\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n\
             hello transfer"
        );
        assert_eq!(full, expected_full);

        // Partial read: 206 with Content-Range before the resume echoes.
        let partial = transcript(
            transfer_download_read_api_response(
                scope.clone(),
                &params,
                ByteRange::HttpHeader("bytes=6-13".to_string()),
            )
            .await,
        );
        let expected_partial = "HTTP/1.1 206 Partial Content\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 8\r\n\
             Accept-Ranges: bytes\r\n\
             Content-Range: bytes 6-13/14\r\n\
             X-Transfer-Range-Start: 6\r\n\
             X-Transfer-Range-End: 14\r\n\
             X-Transfer-Total-Size: 14\r\n\
             X-Transfer-Resumable: true\r\n\
             Content-Disposition: attachment; filename=\"payload.txt\"\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n\
             transfer";
        assert_eq!(partial, expected_partial);

        // Unsatisfiable header: 416 with the probing Content-Range tail.
        let unsatisfiable = transcript(
            transfer_download_read_api_response(
                scope.clone(),
                &params,
                ByteRange::HttpHeader("bytes=99-".to_string()),
            )
            .await,
        );
        let expected_416 = "HTTP/1.1 416 Range Not Satisfiable\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 51\r\n\
             Content-Range: bytes */14\r\n\
             Accept-Ranges: bytes\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n\
             {\"error\":\"range is not satisfiable\",\"ok\":false}";
        // serde_json object key order is alphabetical for json! literals
        // built in one shot; compute the length from the actual body to
        // keep the pin honest.
        let body = expected_416.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(
            unsatisfiable,
            expected_416.replace(
                "Content-Length: 51",
                &format!("Content-Length: {}", body.len())
            )
        );

        // Commit sha mismatch: the 409 wire shape end to end.
        let upload = crate::transfer_store::create_upload_job(
            &scope,
            dest_dir.join("sum.bin").to_str().unwrap(),
            "sum.bin",
            "application/octet-stream",
            Some(4),
            Some("0".repeat(64)),
            crate::transfer_store::TransferConflictPolicy::Fail,
        )
        .unwrap();
        let chunked = transfer_upload_chunk_api_response(
            scope.clone(),
            &serde_json::json!({ "id": upload.id, "offset": 0 }),
            spooled(b"data"),
        )
        .await;
        let (status, body) = json_body(&chunked);
        assert_eq!(status, 200, "{body}");
        let mismatch = transcript(
            transfer_upload_commit_api_response(
                scope.clone(),
                &serde_json::json!({ "id": upload.id }),
            )
            .await,
        );
        assert_eq!(
            mismatch,
            "HTTP/1.1 409 Conflict\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 45\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n\
             {\"error\":\"upload sha256 mismatch\",\"ok\":false}"
        );
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

        let (_, all) = json_body(
            &transfer_jobs_api_response(scope.clone(), &serde_json::json!({}), None).await,
        );
        assert_eq!(all["jobs"].as_array().unwrap().len(), 2);

        for (params, expect) in [
            (serde_json::json!({ "id": job_a.id }), &job_a),
            (
                serde_json::json!({ "resume_token": job_b.resume_token }),
                &job_b,
            ),
        ] {
            let (status, filtered) =
                json_body(&transfer_jobs_api_response(scope.clone(), &params, None).await);
            assert_eq!(status, 200);
            let jobs = filtered["jobs"].as_array().unwrap();
            assert_eq!(jobs.len(), 1, "{params}");
            assert_eq!(jobs[0]["id"], expect.id.as_str());
        }

        let (status, missing) = json_body(
            &transfer_jobs_api_response(
                scope.clone(),
                &serde_json::json!({ "id": "00000000-0000-0000-0000-000000000000" }),
                None,
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(missing["jobs"].as_array().unwrap().len(), 0);
    }

    /// The list row's scope filter (the one judgment call of the
    /// job-path re-check): a scope-restricted caller is never
    /// blanket-denied — the listing shows exactly the jobs whose real
    /// paths pass its read scope, artifact-shaped jobs stay
    /// daemon-internal, and the `?id=` form applies the same per-job
    /// re-check as a filter (an out-of-scope handle reads as an empty
    /// 200, never a denial that would oracle job existence).
    #[tokio::test]
    async fn jobs_list_scope_filters_to_the_callers_readable_paths() {
        let dir = tempfile::tempdir().unwrap();
        let in_scope = dir.path().join("shared");
        let outside = dir.path().join("outside");
        let project = dir.path().join("project");
        for path in [&in_scope, &outside, &project] {
            std::fs::create_dir_all(path).unwrap();
        }
        let scope = project_scope(&project);
        std::fs::write(in_scope.join("data.txt"), b"in scope").unwrap();
        std::fs::write(outside.join("secret.txt"), b"out of scope").unwrap();
        let dl_in = crate::transfer_store::create_download_job(
            &scope,
            in_scope.join("data.txt").to_str().unwrap(),
        )
        .unwrap();
        // Pending upload: the destination is not on disk yet — judged
        // by its nearest existing ancestor, so the in-flight job stays
        // visible to the grant that is chunking into it.
        let up_in = crate::transfer_store::create_upload_job(
            &scope,
            in_scope.join("up.bin").to_str().unwrap(),
            "up.bin",
            "application/octet-stream",
            Some(4),
            None,
            crate::transfer_store::TransferConflictPolicy::Fail,
        )
        .unwrap();
        let dl_out = crate::transfer_store::create_download_job(
            &scope,
            outside.join("secret.txt").to_str().unwrap(),
        )
        .unwrap();
        crate::transfer_store::create_download_job_from_bytes(
            &scope,
            b"report".to_vec(),
            "report.zip",
            "application/zip",
            "session_report",
            None,
            Some(serde_json::json!({ "type": "session_report" })),
        )
        .unwrap();

        // Unrestricted callers list all four jobs, unchanged.
        let (_, all) = json_body(
            &transfer_jobs_api_response(scope.clone(), &serde_json::json!({}), None).await,
        );
        assert_eq!(all["jobs"].as_array().unwrap().len(), 4);

        // A read-only grant on the shared root sees exactly the two
        // in-scope jobs (write roots imply read, pinned by the resume
        // flow test's write-only policy).
        let policy = crate::peer::access_policy::FilesystemAccessPolicy {
            read_roots: vec![in_scope.clone()],
            write_roots: vec![],
        };
        let (status, listed) = json_body(
            &transfer_jobs_api_response(scope.clone(), &serde_json::json!({}), Some(&policy)).await,
        );
        assert_eq!(status, 200);
        let ids: Vec<&str> = listed["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|job| job["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&dl_in.id.as_str()));
        assert!(ids.contains(&up_in.id.as_str()));

        // ?id= on an in-scope job answers it; on an out-of-scope job it
        // answers empty — same 200 shape either way.
        let (_, one) = json_body(
            &transfer_jobs_api_response(
                scope.clone(),
                &serde_json::json!({ "id": dl_in.id }),
                Some(&policy),
            )
            .await,
        );
        assert_eq!(one["jobs"].as_array().unwrap().len(), 1);
        let (status, hidden) = json_body(
            &transfer_jobs_api_response(
                scope.clone(),
                &serde_json::json!({ "id": dl_out.id }),
                Some(&policy),
            )
            .await,
        );
        assert_eq!(status, 200);
        assert_eq!(hidden["jobs"].as_array().unwrap().len(), 0);
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
            ApiResponse::Bytes { .. } | ApiResponse::Stream { .. } => {
                panic!("expected the 416 JSON shape")
            }
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
