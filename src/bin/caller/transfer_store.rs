//! Durable dashboard file-transfer job store.
//!
//! The Files tab needs state that survives page reloads and daemon restarts:
//! download resume tokens, upload destinations, and partially received upload
//! bytes. Job metadata is stored under `<project>/.intendant/transfers/jobs`
//! (daemon-materialized download sources under `.../transfers/artifacts`).
//! Upload partial files are created in the destination directory so the final
//! commit can be a same-directory rename.
//!
//! Every store function takes a [`StoreScope`]: project-rooted daemons use
//! the project-local store above; projectless daemons fall back to the
//! daemon-global store (`~/.intendant/global-store/transfers/...`, identical
//! layout and job format). The fallback store is pruned on daemon startup
//! after [`crate::global_store::GLOBAL_STORE_RETENTION_DAYS`] days of
//! inactivity — see `global_store.rs` for the resolution rule and retention
//! policy.

use crate::global_store::StoreScope;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    Download,
    Upload,
}

impl TransferKind {
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "download" => Some(Self::Download),
            "upload" => Some(Self::Upload),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Queued,
    Running,
    Paused,
    Ready,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferConflictPolicy {
    Fail,
    Rename,
    Overwrite,
}

impl TransferConflictPolicy {
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "fail" | "error" => Some(Self::Fail),
            "rename" | "keep_both" => Some(Self::Rename),
            "overwrite" | "replace" => Some(Self::Overwrite),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferJob {
    pub id: String,
    pub resume_token: String,
    pub kind: TransferKind,
    pub status: TransferStatus,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub source_path: Option<PathBuf>,
    #[serde(default)]
    pub source_kind: Option<String>,
    #[serde(default)]
    pub source_label: Option<String>,
    #[serde(default)]
    pub artifact: Option<serde_json::Value>,
    #[serde(default)]
    pub managed_source: bool,
    #[serde(default)]
    pub destination_path: Option<PathBuf>,
    #[serde(default)]
    pub final_path: Option<PathBuf>,
    #[serde(default)]
    pub temp_path: Option<PathBuf>,
    #[serde(default)]
    pub original_name: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub total_size: Option<u64>,
    /// Expected content hash for upload jobs (lowercase hex), declared
    /// at create and verified at commit. Absent from serialized jobs
    /// when unset, so pre-sha256 wire shapes are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default)]
    pub completed_bytes: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default = "default_conflict_policy")]
    pub conflict_policy: TransferConflictPolicy,
}

fn default_conflict_policy() -> TransferConflictPolicy {
    TransferConflictPolicy::Fail
}

#[derive(Debug, Clone)]
pub struct TransferStoreError {
    pub status: u16,
    pub message: String,
}

impl TransferStoreError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TransferStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TransferStoreError {}

/// Root of the transfer store for a scope: `<project>/.intendant/transfers`
/// or the daemon-global fallback `<global-store>/transfers`. Same layout
/// either way.
pub fn transfer_root(scope: &StoreScope) -> PathBuf {
    scope.store_base().join("transfers")
}

fn jobs_dir(scope: &StoreScope) -> PathBuf {
    transfer_root(scope).join("jobs")
}

fn artifacts_dir(scope: &StoreScope) -> PathBuf {
    transfer_root(scope).join("artifacts")
}

fn is_generated_transfer_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(idx, byte)| match idx {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || matches!(*byte, b'a'..=b'f'),
        })
}

fn job_path(scope: &StoreScope, id: &str) -> Result<PathBuf, TransferStoreError> {
    if !is_generated_transfer_key(id) {
        return Err(TransferStoreError::new(
            500,
            "transfer job id is not a generated UUID",
        ));
    }
    Ok(jobs_dir(scope).join(format!("{id}.json")))
}

fn lookup_job_path(scope: &StoreScope, id_or_token: &str) -> Option<PathBuf> {
    is_generated_transfer_key(id_or_token)
        .then(|| jobs_dir(scope).join(format!("{id_or_token}.json")))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn content_type_for_path(path: &Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("css") => "text/css; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("md") | Some("markdown") | Some("txt") | Some("toml") | Some("yaml") | Some("yml") => {
            "text/plain; charset=utf-8"
        }
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("wasm") => "application/wasm",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn cleanup_created_file_after_save_failure(path: &Path, label: &str) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!(
                "[transfer-store] failed to remove {label} {} after metadata save failed: {}",
                path.display(),
                err
            );
        }
    }
}

fn save_job(scope: &StoreScope, job: &TransferJob) -> Result<(), TransferStoreError> {
    // Only project stores live inside a checkout; the global store needs
    // no ignore rule (it is daemon state, not project content).
    if let Some(project_root) = scope.project_root() {
        crate::upload_store::ensure_project_uploads_ignored(project_root).map_err(|e| {
            TransferStoreError::new(500, format!("ensure transfer metadata ignored: {e}"))
        })?;
    }
    let bytes = serde_json::to_vec_pretty(job)
        .map_err(|e| TransferStoreError::new(500, format!("serialize transfer job: {e}")))?;
    let path = job_path(scope, &job.id)?;
    crate::file_watcher::atomic_write(&path, &bytes)
        .map_err(|e| TransferStoreError::new(500, format!("write transfer job: {e}")))
}

pub fn list_jobs(scope: &StoreScope) -> Vec<TransferJob> {
    let mut jobs = Vec::new();
    let Ok(entries) = fs::read_dir(jobs_dir(scope)) else {
        return jobs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        if let Ok(job) = serde_json::from_str::<TransferJob>(&content) {
            jobs.push(job);
        }
    }
    jobs.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    jobs
}

pub fn find_job(scope: &StoreScope, id_or_token: &str) -> Option<TransferJob> {
    let needle = id_or_token.trim();
    if needle.is_empty() {
        return None;
    }
    if !is_generated_transfer_key(needle) {
        return None;
    }
    if let Some(direct) = lookup_job_path(scope, needle).filter(|path| path.is_file()) {
        if let Ok(content) = fs::read_to_string(direct) {
            if let Ok(job) = serde_json::from_str::<TransferJob>(&content) {
                return Some(job);
            }
        }
    }
    list_jobs(scope)
        .into_iter()
        .find(|job| job.id == needle || job.resume_token == needle)
}

fn required_job(scope: &StoreScope, id_or_token: &str) -> Result<TransferJob, TransferStoreError> {
    find_job(scope, id_or_token)
        .ok_or_else(|| TransferStoreError::new(404, "transfer job not found"))
}

pub fn create_download_job(
    scope: &StoreScope,
    raw_path: &str,
) -> Result<TransferJob, TransferStoreError> {
    let path = crate::web_gateway::expand_dashboard_fs_path(raw_path)
        .map_err(|e| TransferStoreError::new(400, e))?;
    create_download_job_from_path(
        scope,
        path,
        None,
        None,
        Some("filesystem".to_string()),
        None,
        None,
    )
}

pub fn create_download_job_from_path(
    scope: &StoreScope,
    path: PathBuf,
    filename: Option<String>,
    mime: Option<String>,
    source_kind: Option<String>,
    source_label: Option<String>,
    artifact: Option<serde_json::Value>,
) -> Result<TransferJob, TransferStoreError> {
    let metadata = fs::metadata(&path)
        .map_err(|e| TransferStoreError::new(404, format!("file not accessible: {e}")))?;
    if !metadata.is_file() {
        return Err(TransferStoreError::new(400, "path is not a regular file"));
    }
    let display_path = fs::canonicalize(&path).unwrap_or(path);
    let now = now_unix();
    let id = uuid::Uuid::new_v4().to_string();
    let fallback_filename = display_path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty());
    let filename = filename
        .map(|value| crate::upload_store::sanitize_name(&value))
        .filter(|value| !value.is_empty())
        .or(fallback_filename);
    let job = TransferJob {
        id,
        resume_token: uuid::Uuid::new_v4().to_string(),
        kind: TransferKind::Download,
        status: TransferStatus::Queued,
        created_at: now,
        updated_at: now,
        source_path: Some(display_path.clone()),
        source_kind,
        source_label,
        artifact,
        managed_source: false,
        destination_path: None,
        final_path: None,
        temp_path: None,
        original_name: filename.clone(),
        filename,
        mime: Some(mime.unwrap_or_else(|| content_type_for_path(&display_path))),
        total_size: Some(metadata.len()),
        sha256: None,
        completed_bytes: 0,
        error: None,
        conflict_policy: TransferConflictPolicy::Fail,
    };
    if let Err(err) = save_job(scope, &job) {
        if let Some(temp_path) = &job.temp_path {
            if let Err(cleanup_err) = fs::remove_file(temp_path) {
                eprintln!(
                    "[transfer-store] failed to remove upload partial {} after metadata save failed: {}",
                    temp_path.display(),
                    cleanup_err
                );
            }
        }
        return Err(err);
    }
    Ok(job)
}

pub fn create_download_job_from_bytes(
    scope: &StoreScope,
    bytes: Vec<u8>,
    filename: &str,
    mime: &str,
    source_kind: impl Into<String>,
    source_label: Option<String>,
    artifact: Option<serde_json::Value>,
) -> Result<TransferJob, TransferStoreError> {
    if let Some(project_root) = scope.project_root() {
        crate::upload_store::ensure_project_uploads_ignored(project_root).map_err(|e| {
            TransferStoreError::new(500, format!("ensure transfer metadata ignored: {e}"))
        })?;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let safe_name = crate::upload_store::sanitize_name(filename);
    let artifact_path = artifacts_dir(scope).join(format!("{id}-{safe_name}"));
    crate::file_watcher::atomic_write(&artifact_path, &bytes)
        .map_err(|e| TransferStoreError::new(500, format!("write transfer artifact: {e}")))?;
    let now = now_unix();
    let mime = if mime.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime.trim().to_string()
    };
    let job = TransferJob {
        id,
        resume_token: uuid::Uuid::new_v4().to_string(),
        kind: TransferKind::Download,
        status: TransferStatus::Queued,
        created_at: now,
        updated_at: now,
        source_path: Some(artifact_path.clone()),
        source_kind: Some(source_kind.into()),
        source_label,
        artifact,
        managed_source: true,
        destination_path: None,
        final_path: None,
        temp_path: None,
        original_name: Some(safe_name.clone()),
        filename: Some(safe_name),
        mime: Some(mime),
        total_size: Some(bytes.len() as u64),
        sha256: None,
        completed_bytes: 0,
        error: None,
        conflict_policy: TransferConflictPolicy::Fail,
    };
    if let Err(err) = save_job(scope, &job) {
        cleanup_created_file_after_save_failure(&artifact_path, "transfer artifact");
        return Err(err);
    }
    Ok(job)
}

fn choose_unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "upload".to_string());
    let ext = path
        .extension()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty());
    for n in 1..10_000 {
        let filename = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(filename);
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem} ({})", uuid::Uuid::new_v4()))
}

fn resolve_upload_destination(
    raw_destination: &str,
    original_name: &str,
    policy: TransferConflictPolicy,
) -> Result<(PathBuf, PathBuf), TransferStoreError> {
    let requested = crate::web_gateway::expand_dashboard_fs_path(raw_destination)
        .map_err(|e| TransferStoreError::new(400, e))?;
    let safe_name = crate::upload_store::sanitize_name(original_name);
    let target = if requested.is_dir() {
        requested.join(safe_name)
    } else {
        requested
    };
    if target.is_dir() {
        return Err(TransferStoreError::new(
            409,
            "destination already exists and is a directory",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| TransferStoreError::new(400, "destination has no parent directory"))?;
    let parent = fs::canonicalize(parent).map_err(|e| {
        TransferStoreError::new(404, format!("destination parent not accessible: {e}"))
    })?;
    if !parent.is_dir() {
        return Err(TransferStoreError::new(
            400,
            "destination parent is not a directory",
        ));
    }
    let requested_name = target
        .file_name()
        .ok_or_else(|| TransferStoreError::new(400, "destination filename is missing"))?;
    let mut final_path = parent.join(requested_name);
    if final_path.exists() {
        match policy {
            TransferConflictPolicy::Fail => {
                return Err(TransferStoreError::new(409, "destination already exists"));
            }
            TransferConflictPolicy::Rename => {
                final_path = choose_unique_path(&final_path);
            }
            TransferConflictPolicy::Overwrite => {
                if final_path.is_dir() {
                    return Err(TransferStoreError::new(
                        409,
                        "destination already exists and is a directory",
                    ));
                }
                if cfg!(windows) {
                    return Err(TransferStoreError::new(
                        409,
                        "atomic overwrite is not supported on this platform; choose rename or remove the destination first",
                    ));
                }
            }
        }
    }
    Ok((final_path, parent))
}

/// Normalize a declared upload checksum: lowercase hex, 64 chars.
fn normalize_expected_sha256(raw: Option<String>) -> Result<Option<String>, TransferStoreError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }
    if normalized.len() != 64 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(TransferStoreError::new(
            400,
            "sha256 must be 64 hex characters",
        ));
    }
    Ok(Some(normalized))
}

/// Streaming content hash of a file (lowercase hex) — commit-time
/// verification must not buffer a multi-gigabyte partial in memory.
fn sha256_file_hex(path: &Path) -> Result<String, std::io::Error> {
    use sha2::{Digest as _, Sha256};
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(crate::file_watcher::hex_encode(&digest))
}

#[allow(clippy::too_many_arguments)]
pub fn create_upload_job(
    scope: &StoreScope,
    raw_destination: &str,
    original_name: &str,
    mime: &str,
    total_size: Option<u64>,
    sha256: Option<String>,
    conflict_policy: TransferConflictPolicy,
) -> Result<TransferJob, TransferStoreError> {
    let sha256 = normalize_expected_sha256(sha256)?;
    let (final_path, parent) =
        resolve_upload_destination(raw_destination, original_name, conflict_policy)?;
    let id = uuid::Uuid::new_v4().to_string();
    let temp_path = parent.join(format!(".intendant-upload-{id}.part"));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| TransferStoreError::new(500, format!("create upload partial: {e}")))?;
    let now = now_unix();
    let filename = final_path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::upload_store::sanitize_name(original_name));
    let job = TransferJob {
        id,
        resume_token: uuid::Uuid::new_v4().to_string(),
        kind: TransferKind::Upload,
        status: TransferStatus::Queued,
        created_at: now,
        updated_at: now,
        source_path: None,
        source_kind: None,
        source_label: None,
        artifact: None,
        managed_source: false,
        destination_path: Some(final_path.clone()),
        final_path: None,
        temp_path: Some(temp_path.clone()),
        original_name: Some(original_name.to_string()),
        filename: Some(filename),
        mime: Some(if mime.trim().is_empty() {
            "application/octet-stream".to_string()
        } else {
            mime.trim().to_string()
        }),
        total_size,
        sha256,
        completed_bytes: 0,
        error: None,
        conflict_policy,
    };
    if let Err(err) = save_job(scope, &job) {
        cleanup_created_file_after_save_failure(&temp_path, "upload partial");
        return Err(err);
    }
    Ok(job)
}

pub fn append_upload_tempfile(
    scope: &StoreScope,
    id_or_token: &str,
    offset: u64,
    mut chunk: tempfile::NamedTempFile,
    chunk_len: u64,
) -> Result<TransferJob, TransferStoreError> {
    let mut job = required_job(scope, id_or_token)?;
    if job.kind != TransferKind::Upload {
        return Err(TransferStoreError::new(
            400,
            "transfer job is not an upload",
        ));
    }
    if matches!(
        job.status,
        TransferStatus::Completed | TransferStatus::Cancelled | TransferStatus::Failed
    ) {
        return Err(TransferStoreError::new(
            409,
            "upload job is not writable in its current state",
        ));
    }
    if let Some(total) = job.total_size {
        if offset.saturating_add(chunk_len) > total {
            return Err(TransferStoreError::new(
                413,
                "upload chunk exceeds declared total size",
            ));
        }
    }
    if offset < job.completed_bytes {
        if offset.saturating_add(chunk_len) <= job.completed_bytes {
            return Ok(job);
        }
        return Err(TransferStoreError::new(
            409,
            "upload chunk overlaps already persisted bytes",
        ));
    }
    if offset != job.completed_bytes {
        return Err(TransferStoreError::new(
            409,
            "upload chunk offset does not match persisted size",
        ));
    }
    let temp_path = job
        .temp_path
        .clone()
        .ok_or_else(|| TransferStoreError::new(500, "upload job has no partial path"))?;
    let on_disk = fs::metadata(&temp_path).map(|m| m.len()).unwrap_or(0);
    if on_disk != job.completed_bytes {
        return Err(TransferStoreError::new(
            409,
            "upload partial size does not match job metadata",
        ));
    }
    let mut output = fs::OpenOptions::new()
        .append(true)
        .open(&temp_path)
        .map_err(|e| TransferStoreError::new(500, format!("open upload partial: {e}")))?;
    chunk
        .as_file_mut()
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|e| TransferStoreError::new(500, format!("seek upload chunk: {e}")))?;
    let copied = std::io::copy(chunk.as_file_mut(), &mut output)
        .map_err(|e| TransferStoreError::new(500, format!("append upload chunk: {e}")))?;
    if copied != chunk_len {
        return Err(TransferStoreError::new(
            400,
            "upload chunk length did not match declared size",
        ));
    }
    output
        .flush()
        .map_err(|e| TransferStoreError::new(500, format!("flush upload partial: {e}")))?;
    job.completed_bytes = job.completed_bytes.saturating_add(copied);
    job.updated_at = now_unix();
    job.status = match job.total_size {
        Some(total) if job.completed_bytes >= total => TransferStatus::Ready,
        _ => TransferStatus::Running,
    };
    save_job(scope, &job)?;
    Ok(job)
}

pub fn commit_upload_job(
    scope: &StoreScope,
    id_or_token: &str,
) -> Result<TransferJob, TransferStoreError> {
    let mut job = required_job(scope, id_or_token)?;
    if job.kind != TransferKind::Upload {
        return Err(TransferStoreError::new(
            400,
            "transfer job is not an upload",
        ));
    }
    let temp_path = job
        .temp_path
        .clone()
        .ok_or_else(|| TransferStoreError::new(500, "upload job has no partial path"))?;
    let mut destination = job
        .destination_path
        .clone()
        .ok_or_else(|| TransferStoreError::new(500, "upload job has no destination path"))?;
    let size = fs::metadata(&temp_path)
        .map_err(|e| TransferStoreError::new(404, format!("upload partial missing: {e}")))?
        .len();
    if let Some(total) = job.total_size {
        if size != total || job.completed_bytes != total {
            return Err(TransferStoreError::new(
                409,
                "upload is not complete enough to commit",
            ));
        }
    }
    // Verify the declared checksum before the partial is placed — a
    // mismatch leaves the job resumable-from-scratch (delete + retry)
    // rather than a corrupt file at the destination.
    if let Some(expected) = &job.sha256 {
        let actual = sha256_file_hex(&temp_path)
            .map_err(|e| TransferStoreError::new(500, format!("hash upload partial: {e}")))?;
        if &actual != expected {
            return Err(TransferStoreError::new(409, "upload sha256 mismatch"));
        }
    }
    if destination.exists() {
        match job.conflict_policy {
            TransferConflictPolicy::Fail => {
                return Err(TransferStoreError::new(409, "destination already exists"));
            }
            TransferConflictPolicy::Rename => {
                destination = choose_unique_path(&destination);
            }
            TransferConflictPolicy::Overwrite => {
                if destination.is_dir() {
                    return Err(TransferStoreError::new(
                        409,
                        "destination already exists and is a directory",
                    ));
                }
                if cfg!(windows) {
                    return Err(TransferStoreError::new(
                        409,
                        "atomic overwrite is not supported on this platform; choose rename or remove the destination first",
                    ));
                }
            }
        }
    }
    fs::rename(&temp_path, &destination)
        .map_err(|e| TransferStoreError::new(500, format!("commit upload: {e}")))?;
    job.destination_path = Some(destination.clone());
    job.final_path = Some(destination);
    job.temp_path = None;
    job.completed_bytes = size;
    job.total_size = Some(size);
    job.status = TransferStatus::Completed;
    job.updated_at = now_unix();
    job.error = None;
    save_job(scope, &job)?;
    Ok(job)
}

/// Resolve a download job's source: the job, its source path, and the
/// on-disk size — the shared preamble of [`read_download_range`] and the
/// HTTP lane's `Range`-header normalization (which must know the total
/// before an end-inclusive header can become an offset/length read).
pub fn download_source(
    scope: &StoreScope,
    id_or_token: &str,
) -> Result<(TransferJob, PathBuf, u64), TransferStoreError> {
    let job = required_job(scope, id_or_token)?;
    if job.kind != TransferKind::Download {
        return Err(TransferStoreError::new(
            400,
            "transfer job is not a download",
        ));
    }
    let path = job
        .source_path
        .clone()
        .ok_or_else(|| TransferStoreError::new(500, "download job has no source path"))?;
    let metadata = fs::metadata(&path)
        .map_err(|e| TransferStoreError::new(404, format!("file not accessible: {e}")))?;
    if !metadata.is_file() {
        return Err(TransferStoreError::new(400, "path is not a regular file"));
    }
    let total_size = metadata.len();
    Ok((job, path, total_size))
}

pub fn read_download_range(
    scope: &StoreScope,
    id_or_token: &str,
    offset: u64,
    length: Option<u64>,
    max_bytes: u64,
) -> Result<(TransferJob, Vec<u8>, u64), TransferStoreError> {
    let (mut job, path, total_size) = download_source(scope, id_or_token)?;
    if offset > total_size {
        return Err(TransferStoreError::new(416, "range start beyond file size"));
    }
    let available = total_size.saturating_sub(offset);
    let requested = length.unwrap_or(available).min(available);
    if requested > max_bytes {
        return Err(TransferStoreError::new(
            413,
            format!("range too large: {requested} bytes (cap is {max_bytes})"),
        ));
    }
    let transfer_len = usize::try_from(requested)
        .map_err(|_| TransferStoreError::new(413, "range too large for this platform"))?;
    let mut file = fs::File::open(&path)
        .map_err(|e| TransferStoreError::new(500, format!("open file: {e}")))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|e| TransferStoreError::new(500, format!("seek file: {e}")))?;
    let mut bytes = vec![0u8; transfer_len];
    file.read_exact(&mut bytes)
        .map_err(|e| TransferStoreError::new(500, format!("read file: {e}")))?;
    let end = offset.saturating_add(requested);
    job.total_size = Some(total_size);
    job.completed_bytes = job.completed_bytes.max(end);
    job.status = if end >= total_size {
        TransferStatus::Completed
    } else {
        TransferStatus::Running
    };
    job.updated_at = now_unix();
    save_job(scope, &job)?;
    Ok((job, bytes, end))
}

pub fn delete_job(scope: &StoreScope, id_or_token: &str) -> Result<bool, TransferStoreError> {
    let Some(mut job) = find_job(scope, id_or_token) else {
        return Ok(false);
    };
    if let Some(temp_path) = job.temp_path.take() {
        let _ = fs::remove_file(temp_path);
    }
    if job.managed_source {
        if let Some(source_path) = job.source_path.take() {
            let _ = fs::remove_file(source_path);
        }
    }
    job.status = TransferStatus::Cancelled;
    job.updated_at = now_unix();
    let _ = save_job(scope, &job);
    let path = job_path(scope, &job.id)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(TransferStoreError::new(
            500,
            format!("delete transfer job: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_chunk(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(bytes).unwrap();
        tmp.flush().unwrap();
        tmp
    }

    fn scope(project: &Path) -> StoreScope {
        StoreScope::Project(project.to_path_buf())
    }

    fn block_jobs_dir(project: &Path) {
        let transfers = transfer_root(&scope(project));
        fs::create_dir_all(&transfers).unwrap();
        fs::write(transfers.join("jobs"), b"not a directory").unwrap();
    }

    #[test]
    fn download_job_persists_and_reads_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let source = tmp.path().join("fixture.txt");
        fs::write(&source, b"hello transfer").unwrap();

        let job = create_download_job(&scope(&project), source.to_str().unwrap()).unwrap();
        assert_eq!(job.kind, TransferKind::Download);
        assert_eq!(job.total_size, Some(14));
        assert!(!job.resume_token.is_empty());

        let listed = list_jobs(&scope(&project));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job.id);

        let (updated, bytes, end) =
            read_download_range(&scope(&project), &job.resume_token, 6, Some(8), 100).unwrap();
        assert_eq!(&bytes, b"transfer");
        assert_eq!(end, 14);
        assert_eq!(updated.status, TransferStatus::Completed);
        assert_eq!(updated.completed_bytes, 14);
    }

    /// A projectless daemon's scope stores job metadata and materialized
    /// artifacts under `<global-store>/transfers/` with the same layout,
    /// writes no git ignore metadata, and the full job lifecycle
    /// (create/list/read/delete) works unchanged.
    #[test]
    fn global_scope_stores_jobs_under_global_store_root() {
        let tmp = tempfile::tempdir().unwrap();
        let global_base = crate::global_store::global_store_root_in(tmp.path());
        let global = StoreScope::Global(global_base.clone());
        let source = tmp.path().join("fixture.txt");
        fs::write(&source, b"projectless transfer").unwrap();

        let job = create_download_job(&global, source.to_str().unwrap()).unwrap();
        assert!(global_base
            .join("transfers")
            .join("jobs")
            .join(format!("{}.json", job.id))
            .is_file());
        assert!(!global_base.join(".gitignore").exists());
        assert!(!tmp.path().join(".gitignore").exists());

        let materialized = create_download_job_from_bytes(
            &global,
            b"generated bytes".to_vec(),
            "report.zip",
            "application/zip",
            "session_report",
            None,
            None,
        )
        .unwrap();
        assert!(materialized
            .source_path
            .as_ref()
            .unwrap()
            .starts_with(global_base.join("transfers").join("artifacts")));

        assert_eq!(list_jobs(&global).len(), 2);
        let (_, bytes, _) = read_download_range(&global, &job.resume_token, 0, None, 100).unwrap();
        assert_eq!(&bytes, b"projectless transfer");

        assert!(delete_job(&global, &job.id).unwrap());
        assert!(delete_job(&global, &materialized.id).unwrap());
        assert_eq!(list_jobs(&global).len(), 0);
    }

    #[test]
    fn transfer_lookup_rejects_malformed_uuid_strings() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let source = tmp.path().join("fixture.txt");
        fs::write(&source, b"hello transfer").unwrap();

        let job = create_download_job(&scope(&project), source.to_str().unwrap()).unwrap();
        assert!(find_job(&scope(&project), &job.id).is_some());
        assert!(find_job(&scope(&project), &job.resume_token).is_some());
        assert!(find_job(&scope(&project), &format!("../{}", job.id)).is_none());
        assert!(find_job(&scope(&project), &format!("{}!!", job.id)).is_none());
        assert!(find_job(&scope(&project), &job.id.to_uppercase()).is_none());
    }

    #[test]
    fn generated_download_job_materializes_and_cleans_up_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();

        let job = create_download_job_from_bytes(
            &scope(&project),
            b"generated report bytes".to_vec(),
            "report.zip",
            "application/zip",
            "session_report",
            Some("Session report".to_string()),
            Some(serde_json::json!({
                "type": "session_report",
                "session_id": "current",
            })),
        )
        .unwrap();
        assert_eq!(job.kind, TransferKind::Download);
        assert_eq!(job.source_kind.as_deref(), Some("session_report"));
        assert_eq!(job.source_label.as_deref(), Some("Session report"));
        assert_eq!(job.filename.as_deref(), Some("report.zip"));
        assert_eq!(job.mime.as_deref(), Some("application/zip"));
        assert!(job.managed_source);
        let source_path = job.source_path.clone().unwrap();
        assert!(source_path.exists());

        let (_, bytes, end) =
            read_download_range(&scope(&project), &job.id, 10, Some(6), 100).unwrap();
        assert_eq!(&bytes, b"report");
        assert_eq!(end, 16);

        assert!(delete_job(&scope(&project), &job.id).unwrap());
        assert!(!source_path.exists());
    }

    #[test]
    fn generated_download_artifact_is_removed_when_metadata_save_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        block_jobs_dir(&project);

        let err = create_download_job_from_bytes(
            &scope(&project),
            b"generated report bytes".to_vec(),
            "report.zip",
            "application/zip",
            "session_report",
            Some("Session report".to_string()),
            None,
        )
        .unwrap_err();
        assert_eq!(err.status, 500);

        let artifacts = artifacts_dir(&scope(&project));
        assert!(artifacts.exists());
        assert!(fs::read_dir(artifacts).unwrap().next().is_none());
    }

    #[test]
    fn upload_job_appends_and_commits_atomically_to_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let dest_dir = tmp.path().join("dest");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("out.txt");

        let job = create_upload_job(
            &scope(&project),
            dest.to_str().unwrap(),
            "out.txt",
            "text/plain",
            Some(11),
            None,
            TransferConflictPolicy::Fail,
        )
        .unwrap();
        let temp_path = job.temp_path.clone().unwrap();
        assert!(temp_path.starts_with(fs::canonicalize(&dest_dir).unwrap()));

        let job = append_upload_tempfile(&scope(&project), &job.id, 0, write_chunk(b"hello "), 6)
            .unwrap();
        assert_eq!(job.completed_bytes, 6);
        assert_eq!(job.status, TransferStatus::Running);

        let job = append_upload_tempfile(
            &scope(&project),
            &job.resume_token,
            6,
            write_chunk(b"world"),
            5,
        )
        .unwrap();
        assert_eq!(job.status, TransferStatus::Ready);

        let committed = commit_upload_job(&scope(&project), &job.id).unwrap();
        assert_eq!(committed.status, TransferStatus::Completed);
        let expected_final_path = fs::canonicalize(&dest_dir).unwrap().join("out.txt");
        assert_eq!(
            committed.final_path.as_deref(),
            Some(expected_final_path.as_path())
        );
        assert_eq!(fs::read(&dest).unwrap(), b"hello world");
        assert!(!temp_path.exists());
    }

    #[test]
    fn upload_job_rejects_conflict_by_default_and_can_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let dest_dir = tmp.path().join("dest");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("out.txt");
        fs::write(&dest, b"existing").unwrap();

        let err = create_upload_job(
            &scope(&project),
            dest.to_str().unwrap(),
            "out.txt",
            "text/plain",
            Some(3),
            None,
            TransferConflictPolicy::Fail,
        )
        .unwrap_err();
        assert_eq!(err.status, 409);

        let job = create_upload_job(
            &scope(&project),
            dest.to_str().unwrap(),
            "out.txt",
            "text/plain",
            Some(3),
            None,
            TransferConflictPolicy::Rename,
        )
        .unwrap();
        assert_ne!(job.destination_path.as_deref(), Some(dest.as_path()));
        let job =
            append_upload_tempfile(&scope(&project), &job.id, 0, write_chunk(b"new"), 3).unwrap();
        let committed = commit_upload_job(&scope(&project), &job.id).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"existing");
        assert_eq!(fs::read(committed.final_path.unwrap()).unwrap(), b"new");
    }

    #[test]
    fn upload_partial_is_removed_when_metadata_save_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let dest_dir = tmp.path().join("dest");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        block_jobs_dir(&project);

        let err = create_upload_job(
            &scope(&project),
            dest_dir.join("out.txt").to_str().unwrap(),
            "out.txt",
            "text/plain",
            Some(3),
            None,
            TransferConflictPolicy::Fail,
        )
        .unwrap_err();
        assert_eq!(err.status, 500);
        let partials = fs::read_dir(&dest_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".intendant-upload-")
            })
            .count();
        assert_eq!(partials, 0);
    }

    #[test]
    fn upload_commit_verifies_declared_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let dest_dir = tmp.path().join("dest");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        let payload = b"hello world";
        let good_hash =
            crate::file_watcher::hex_encode(&crate::file_watcher::sha256_hash(payload));

        // Malformed declarations refuse at create.
        let err = create_upload_job(
            &scope(&project),
            dest_dir.join("bad-decl.bin").to_str().unwrap(),
            "bad-decl.bin",
            "application/octet-stream",
            Some(payload.len() as u64),
            Some("not-a-hash".to_string()),
            TransferConflictPolicy::Fail,
        )
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.message, "sha256 must be 64 hex characters");

        // A wrong declared hash refuses at commit; the partial and job
        // survive (delete + retry, never a corrupt destination).
        let wrong = create_upload_job(
            &scope(&project),
            dest_dir.join("wrong.bin").to_str().unwrap(),
            "wrong.bin",
            "application/octet-stream",
            Some(payload.len() as u64),
            Some("0".repeat(64)),
            TransferConflictPolicy::Fail,
        )
        .unwrap();
        let wrong =
            append_upload_tempfile(&scope(&project), &wrong.id, 0, write_chunk(payload), 11)
                .unwrap();
        let err = commit_upload_job(&scope(&project), &wrong.id).unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.message, "upload sha256 mismatch");
        assert!(!dest_dir.join("wrong.bin").exists());
        assert!(wrong.temp_path.unwrap().exists());

        // The correct hash (declared uppercase — normalized at create)
        // commits, and the job carries the normalized value.
        let good = create_upload_job(
            &scope(&project),
            dest_dir.join("good.bin").to_str().unwrap(),
            "good.bin",
            "application/octet-stream",
            Some(payload.len() as u64),
            Some(good_hash.to_ascii_uppercase()),
            TransferConflictPolicy::Fail,
        )
        .unwrap();
        assert_eq!(good.sha256.as_deref(), Some(good_hash.as_str()));
        let good =
            append_upload_tempfile(&scope(&project), &good.id, 0, write_chunk(payload), 11)
                .unwrap();
        let committed = commit_upload_job(&scope(&project), &good.id).unwrap();
        assert_eq!(committed.status, TransferStatus::Completed);
        assert_eq!(fs::read(dest_dir.join("good.bin")).unwrap(), payload);
    }

    #[test]
    fn duplicate_already_persisted_upload_chunk_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let dest_dir = tmp.path().join("dest");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();

        let job = create_upload_job(
            &scope(&project),
            dest_dir.join("out.txt").to_str().unwrap(),
            "out.txt",
            "text/plain",
            Some(6),
            None,
            TransferConflictPolicy::Fail,
        )
        .unwrap();
        let job = append_upload_tempfile(&scope(&project), &job.id, 0, write_chunk(b"hello "), 6)
            .unwrap();
        let same = append_upload_tempfile(&scope(&project), &job.id, 0, write_chunk(b"hello "), 6)
            .unwrap();
        assert_eq!(same.completed_bytes, 6);
        assert_eq!(same.status, TransferStatus::Ready);
    }

    #[cfg(not(windows))]
    #[test]
    fn upload_overwrite_replaces_existing_destination_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let dest_dir = tmp.path().join("dest");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("out.txt");
        fs::write(&dest, b"existing").unwrap();

        let job = create_upload_job(
            &scope(&project),
            dest.to_str().unwrap(),
            "out.txt",
            "text/plain",
            Some(3),
            None,
            TransferConflictPolicy::Overwrite,
        )
        .unwrap();
        let job =
            append_upload_tempfile(&scope(&project), &job.id, 0, write_chunk(b"new"), 3).unwrap();
        let committed = commit_upload_job(&scope(&project), &job.id).unwrap();

        let expected_final_path = fs::canonicalize(&dest_dir).unwrap().join("out.txt");
        assert_eq!(
            committed.final_path.as_deref(),
            Some(expected_final_path.as_path())
        );
        assert_eq!(fs::read(&dest).unwrap(), b"new");
    }

    #[cfg(windows)]
    #[test]
    fn upload_overwrite_rejects_existing_destination_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let dest_dir = tmp.path().join("dest");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("out.txt");
        fs::write(&dest, b"existing").unwrap();

        let err = create_upload_job(
            &scope(&project),
            dest.to_str().unwrap(),
            "out.txt",
            "text/plain",
            Some(3),
            None,
            TransferConflictPolicy::Overwrite,
        )
        .unwrap_err();
        assert_eq!(err.status, 409);
    }
}
