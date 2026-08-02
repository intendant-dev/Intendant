//! Explicit working-tree snapshots for provider-neutral remote commands.
//!
//! Home captures the selected repository as a binary Git patch relative to a
//! pinned base commit plus non-ignored untracked regular files. The compressed,
//! content-addressed archive travels over the authenticated cloud-worker
//! attachment in bounded chunks. The worker materializes it in an isolated Git
//! worktree and keeps that worktree warm for later commands with the same
//! digest. Ignored files (notably `.env` and `target/`) never ride the snapshot.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, OwnedMutexGuard};

pub(super) const REMOTE_SOURCE_BEGIN_KIND: &str = "remote_source_begin";
pub(super) const REMOTE_SOURCE_CHUNK_KIND: &str = "remote_source_chunk";
pub(super) const REMOTE_SOURCE_FINISH_KIND: &str = "remote_source_finish";
pub(crate) const REMOTE_SOURCE_RESULT_KIND: &str = "remote_source_result";

const SOURCE_CHUNK_BYTES: usize = 192 * 1024;
const MAX_SOURCE_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SOURCE_EXPANDED_BYTES: usize = 128 * 1024 * 1024;
const MAX_UNTRACKED_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_UNTRACKED_FILES: usize = 4096;
const MAX_WORKER_UPLOADS: usize = 2;
const MAX_PREPARED_SOURCES: usize = 4;
const SOURCE_TRANSFER_TIMEOUT_S: u64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotEntryKind {
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotEntry {
    path: String,
    kind: SnapshotEntryKind,
    executable: bool,
    data_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotArchive {
    version: u32,
    base_revision: String,
    git_patch: String,
    untracked: Vec<SnapshotEntry>,
}

/// One stable, explicit local source capture. `archive` is never serialized
/// into a job view or log; only its digest and byte count are visible there.
#[derive(Clone)]
pub(super) struct HomeSourceSnapshot {
    pub base_revision: String,
    pub branch_hint: Option<String>,
    pub digest: String,
    pub source_id: String,
    pub archive: Arc<Vec<u8>>,
}

impl std::fmt::Debug for HomeSourceSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomeSourceSnapshot")
            .field("base_revision", &self.base_revision)
            .field("branch_hint", &self.branch_hint)
            .field("digest", &self.digest)
            .field("source_id", &self.source_id)
            .field("archive_bytes", &self.archive.len())
            .finish()
    }
}

impl HomeSourceSnapshot {
    pub fn archive_bytes(&self) -> u64 {
        self.archive.len() as u64
    }
}

/// Capture twice and accept only byte-identical results. Git's status path
/// list alone cannot detect a file changing in place while the patch is being
/// produced; two complete captures make the selected snapshot an honest point
/// in time or fail with an actionable retry.
pub(super) fn capture_working_tree(
    project_root: &Path,
    requested_base: Option<&str>,
) -> Result<HomeSourceSnapshot, String> {
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("resolve local project root: {error}"))?;
    let top = git_stdout(&project_root, &["rev-parse", "--show-toplevel"], None)?;
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim());
    let top = top
        .canonicalize()
        .map_err(|error| format!("resolve Git repository root: {error}"))?;
    if top != project_root {
        return Err(format!(
            "working-tree snapshots require the project root to be the Git repository root (project {}, Git {})",
            project_root.display(),
            top.display()
        ));
    }

    let base_ref = requested_base
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("INTENDANT_REMOTE_COMPUTE_BASE_REF")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "origin/main".to_string());
    if base_ref.len() > 256 || base_ref.starts_with('-') || base_ref.contains('\0') {
        return Err("remote-compute base ref has an invalid shape".to_string());
    }
    let commit_expr = format!("{base_ref}^{{commit}}");
    let base_revision = String::from_utf8_lossy(&git_stdout(
        &project_root,
        &["rev-parse", "--verify", &commit_expr],
        None,
    )?)
    .trim()
    .to_ascii_lowercase();
    if base_revision.len() != 40 || !base_revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "base ref {base_ref:?} did not resolve to a full Git commit id"
        ));
    }

    let branch_hint = branch_from_base_ref(&base_ref)
        .or_else(|| branch_for_revision(&project_root, &base_revision));
    let first = capture_once(&project_root, &base_revision, branch_hint.clone())?;
    let second = capture_once(&project_root, &base_revision, branch_hint)?;
    if first.digest != second.digest {
        return Err(
            "working tree changed while Intendant was capturing it; let edits settle and retry"
                .to_string(),
        );
    }
    Ok(second)
}

fn capture_once(
    project_root: &Path,
    base_revision: &str,
    branch_hint: Option<String>,
) -> Result<HomeSourceSnapshot, String> {
    let patch = git_stdout(
        project_root,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            base_revision,
            "--",
        ],
        None,
    )?;
    let git_patch = String::from_utf8(patch)
        .map_err(|_| "Git produced a non-UTF-8 binary patch".to_string())?;
    let untracked_output = git_stdout(
        project_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
    )?;
    let mut paths = Vec::new();
    for raw in untracked_output.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(raw)
            .map_err(|_| "working-tree snapshot contains a non-UTF-8 path".to_string())?
            .to_string();
        validate_source_path(&path)?;
        paths.push(path);
    }
    paths.sort();
    if paths.len() > MAX_UNTRACKED_FILES {
        return Err(format!(
            "working-tree snapshot has {} non-ignored untracked files; the limit is {MAX_UNTRACKED_FILES}",
            paths.len()
        ));
    }

    let mut untracked = Vec::with_capacity(paths.len());
    let mut expanded_bytes = git_patch.len();
    for path in paths {
        let local_path = project_root.join(&path);
        let metadata = std::fs::symlink_metadata(&local_path)
            .map_err(|error| format!("inspect untracked source {path}: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "untracked symlink {path:?} is not supported in a remote source snapshot; add it to Git or exclude it"
            ));
        }
        if !metadata.is_file() {
            return Err(format!("untracked source {path:?} is not a regular file"));
        }
        if metadata.len() > MAX_UNTRACKED_FILE_BYTES {
            return Err(format!(
                "untracked source {path:?} is {} bytes; the per-file limit is {MAX_UNTRACKED_FILE_BYTES}",
                metadata.len()
            ));
        }
        let canonical = local_path
            .canonicalize()
            .map_err(|error| format!("resolve untracked source {path}: {error}"))?;
        if !canonical.starts_with(project_root) {
            return Err(format!(
                "untracked source {path:?} resolves outside the repository"
            ));
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|error| format!("read untracked source {path}: {error}"))?;
        expanded_bytes = expanded_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| "working-tree snapshot is too large".to_string())?;
        if expanded_bytes > MAX_SOURCE_EXPANDED_BYTES {
            return Err(format!(
                "working-tree snapshot exceeds the {} MiB expanded limit",
                MAX_SOURCE_EXPANDED_BYTES / (1024 * 1024)
            ));
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt as _;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        untracked.push(SnapshotEntry {
            path,
            kind: SnapshotEntryKind::File,
            executable,
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }

    let archive = SnapshotArchive {
        version: 1,
        base_revision: base_revision.to_string(),
        git_patch,
        untracked,
    };
    let json = serde_json::to_vec(&archive)
        .map_err(|error| format!("serialize working-tree snapshot: {error}"))?;
    if json.len() > MAX_SOURCE_EXPANDED_BYTES {
        return Err(format!(
            "working-tree snapshot exceeds the {} MiB expanded limit",
            MAX_SOURCE_EXPANDED_BYTES / (1024 * 1024)
        ));
    }
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(&json)
        .map_err(|error| format!("compress working-tree snapshot: {error}"))?;
    let compressed = encoder
        .finish()
        .map_err(|error| format!("finish working-tree snapshot: {error}"))?;
    if compressed.len() > MAX_SOURCE_ARCHIVE_BYTES {
        return Err(format!(
            "working-tree snapshot compresses to {} MiB; the transfer limit is {} MiB",
            compressed.len() / (1024 * 1024),
            MAX_SOURCE_ARCHIVE_BYTES / (1024 * 1024)
        ));
    }
    let digest = sha256_hex(&compressed);
    Ok(HomeSourceSnapshot {
        base_revision: base_revision.to_string(),
        branch_hint,
        source_id: format!("source-{digest}"),
        digest,
        archive: Arc::new(compressed),
    })
}

fn branch_from_base_ref(base_ref: &str) -> Option<String> {
    base_ref
        .strip_prefix("origin/")
        .or_else(|| base_ref.strip_prefix("refs/remotes/origin/"))
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
}

pub(super) fn branch_for_revision(project_root: &Path, revision: &str) -> Option<String> {
    let branch = String::from_utf8_lossy(
        &git_stdout(project_root, &["branch", "--show-current"], None).ok()?,
    )
    .trim()
    .to_string();
    if branch.is_empty() {
        return None;
    }
    let remote_ref = format!("refs/remotes/origin/{branch}^{{commit}}");
    let remote = String::from_utf8_lossy(
        &git_stdout(project_root, &["rev-parse", "--verify", &remote_ref], None).ok()?,
    )
    .trim()
    .to_ascii_lowercase();
    let expected = revision.trim().to_ascii_lowercase();
    remote.starts_with(&expected).then_some(branch)
}

pub(super) fn validate_provider_branch(raw: &str) -> Result<String, String> {
    let branch = raw.trim();
    if branch.is_empty() || branch.len() > 255 || branch.starts_with('-') {
        return Err("branch must be a valid pushed Git branch name".to_string());
    }
    let full_ref = format!("refs/heads/{branch}");
    let valid = std::process::Command::new("git")
        .args(["check-ref-format", &full_ref])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("validate provider branch with Git: {error}"))?
        .success();
    if !valid {
        return Err("branch must be a valid pushed Git branch name".to_string());
    }
    Ok(branch.to_string())
}

fn git_stdout(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if let Some(bytes) = stdin {
        child
            .stdin
            .take()
            .ok_or_else(|| "Git stdin pipe was unavailable".to_string())?
            .write_all(bytes)
            .map_err(|error| format!("write git {} stdin: {error}", args.join(" ")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn validate_source_path(raw: &str) -> Result<(), String> {
    if raw.is_empty() || raw.len() > 4096 {
        return Err("snapshot paths must contain 1-4096 bytes".to_string());
    }
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "snapshot path {raw:?} is not a safe repository-relative path"
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Send a source archive to a worker, skipping the body when that worker
/// already has the exact digest materialized.
pub(super) async fn transfer_snapshot(
    host: &str,
    snapshot: &HomeSourceSnapshot,
) -> Result<String, String> {
    let key = format!("{host}:{}", snapshot.source_id);
    let (flight, leader) = {
        let mut flights = source_flights()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match flights.get(&key) {
            Some(flight) => (Arc::clone(flight), false),
            None => {
                let flight = Arc::new(SourceFlight {
                    result: Mutex::new(None),
                    notify: tokio::sync::Notify::new(),
                });
                flights.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        }
    };
    if leader {
        let result = transfer_snapshot_inner(host, snapshot).await;
        *flight
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result.clone());
        flight.notify.notify_waiters();
        source_flights()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        return result;
    }
    loop {
        let notified = flight.notify.notified();
        if let Some(result) = flight
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return result;
        }
        notified.await;
    }
}

struct SourceFlight {
    result: Mutex<Option<Result<String, String>>>,
    notify: tokio::sync::Notify,
}

static SOURCE_FLIGHTS: OnceLock<Mutex<HashMap<String, Arc<SourceFlight>>>> = OnceLock::new();

fn source_flights() -> &'static Mutex<HashMap<String, Arc<SourceFlight>>> {
    SOURCE_FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn transfer_snapshot_inner(
    host: &str,
    snapshot: &HomeSourceSnapshot,
) -> Result<String, String> {
    let task_id = crate::codex_cloud_attach::cloud_host_task_id(host)
        .ok_or_else(|| "source snapshots require a cloud worker host".to_string())?;
    let Some((to_worker, mut from_worker)) = crate::codex_cloud_attach::attachment_channel(task_id)
    else {
        return Err(format!("remote host {host} has no live attachment"));
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(SOURCE_TRANSFER_TIMEOUT_S);
    super::send_home_frame(
        &to_worker,
        serde_json::json!({
            "t": REMOTE_SOURCE_BEGIN_KIND,
            "host_id": host,
            "id": snapshot.source_id,
            "digest": snapshot.digest,
            "base_revision": snapshot.base_revision,
            "compressed_bytes": snapshot.archive.len(),
        })
        .to_string(),
    )
    .await
    .map_err(|_| "remote worker detached before source preparation".to_string())?;

    match wait_source_reply(&mut from_worker, &snapshot.source_id, deadline).await? {
        SourceReply::Ready => return Ok(snapshot.source_id.clone()),
        SourceReply::Upload => {}
    }

    for (index, chunk) in snapshot.archive.chunks(SOURCE_CHUNK_BYTES).enumerate() {
        let offset = index
            .checked_mul(SOURCE_CHUNK_BYTES)
            .ok_or_else(|| "source snapshot offset overflow".to_string())?;
        super::send_home_frame(
            &to_worker,
            serde_json::json!({
                "t": REMOTE_SOURCE_CHUNK_KIND,
                "host_id": host,
                "id": snapshot.source_id,
                "offset": offset,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            })
            .to_string(),
        )
        .await
        .map_err(|_| "remote worker detached during source transfer".to_string())?;
    }
    super::send_home_frame(
        &to_worker,
        serde_json::json!({
            "t": REMOTE_SOURCE_FINISH_KIND,
            "host_id": host,
            "id": snapshot.source_id,
        })
        .to_string(),
    )
    .await
    .map_err(|_| "remote worker detached before source materialization".to_string())?;
    match wait_source_reply(&mut from_worker, &snapshot.source_id, deadline).await? {
        SourceReply::Ready => Ok(snapshot.source_id.clone()),
        SourceReply::Upload => Err("worker requested a second source upload".to_string()),
    }
}

enum SourceReply {
    Upload,
    Ready,
}

async fn wait_source_reply(
    receiver: &mut tokio::sync::broadcast::Receiver<String>,
    source_id: &str,
    deadline: tokio::time::Instant,
) -> Result<SourceReply, String> {
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("source transfer timed out".to_string());
        }
        match tokio::time::timeout(remaining, receiver.recv()).await {
            Ok(Ok(text)) => {
                let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if frame.get("t").and_then(serde_json::Value::as_str)
                    != Some(REMOTE_SOURCE_RESULT_KIND)
                    || frame.get("id").and_then(serde_json::Value::as_str) != Some(source_id)
                {
                    continue;
                }
                return match frame.get("state").and_then(serde_json::Value::as_str) {
                    Some("upload") => Ok(SourceReply::Upload),
                    Some("ready") => Ok(SourceReply::Ready),
                    Some("error") => Err(frame
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("worker rejected the source snapshot")
                        .to_string()),
                    Some(other) => Err(format!("worker returned unknown source state {other:?}")),
                    None => Err("worker source reply carried no state".to_string()),
                };
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err("remote worker detached during source transfer".to_string())
            }
            Err(_) => return Err("source transfer timed out".to_string()),
        }
    }
}

struct SourceUpload {
    digest: String,
    base_revision: String,
    expected_bytes: usize,
    bytes: Vec<u8>,
}

struct PreparedSource {
    root: PathBuf,
    baseline_source_digest: String,
    command_lock: Arc<tokio::sync::Mutex<()>>,
}

struct WorkerSourceState {
    uploads: HashMap<String, SourceUpload>,
    prepared: HashMap<String, Arc<PreparedSource>>,
    order: VecDeque<String>,
}

struct WorkerSourcesInner {
    project_root: PathBuf,
    scratch: tempfile::TempDir,
    state: tokio::sync::Mutex<WorkerSourceState>,
}

#[derive(Clone)]
pub(super) struct WorkerSources {
    inner: Arc<WorkerSourcesInner>,
}

pub(super) struct WorkerSourceLease {
    pub root: PathBuf,
    pub baseline_source_digest: String,
    _guard: OwnedMutexGuard<()>,
    _prepared: Arc<PreparedSource>,
}

impl WorkerSources {
    pub fn new(project_root: PathBuf) -> Result<Self, String> {
        let scratch = tempfile::Builder::new()
            .prefix("intendant-remote-sources-")
            .tempdir()
            .map_err(|error| format!("create remote source scratch: {error}"))?;
        Ok(Self {
            inner: Arc::new(WorkerSourcesInner {
                project_root,
                scratch,
                state: tokio::sync::Mutex::new(WorkerSourceState {
                    uploads: HashMap::new(),
                    prepared: HashMap::new(),
                    order: VecDeque::new(),
                }),
            }),
        })
    }

    pub async fn serve_frame(
        &self,
        frame: &serde_json::Value,
        out_tx: &mpsc::Sender<String>,
        host_id: &str,
    ) -> bool {
        match frame.get("t").and_then(serde_json::Value::as_str) {
            Some(REMOTE_SOURCE_BEGIN_KIND) => {
                self.begin(frame, out_tx, host_id).await;
                true
            }
            Some(REMOTE_SOURCE_CHUNK_KIND) => {
                self.chunk(frame, out_tx, host_id).await;
                true
            }
            Some(REMOTE_SOURCE_FINISH_KIND) => {
                self.finish(frame, out_tx, host_id).await;
                true
            }
            _ => false,
        }
    }

    async fn begin(&self, frame: &serde_json::Value, out_tx: &mpsc::Sender<String>, host_id: &str) {
        let id = frame_string(frame, "id");
        let digest = frame_string(frame, "digest");
        let base_revision = frame_string(frame, "base_revision");
        let expected_bytes = frame
            .get("compressed_bytes")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        let invalid = id != format!("source-{digest}")
            || digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !(7..=64).contains(&base_revision.len())
            || !base_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            || expected_bytes.is_none_or(|bytes| bytes > MAX_SOURCE_ARCHIVE_BYTES);
        if invalid {
            send_source_reply(
                out_tx,
                host_id,
                &id,
                "error",
                Some("source begin frame has invalid metadata"),
            )
            .await;
            return;
        }
        let expected_bytes = expected_bytes.unwrap_or_default();
        let mut state = self.inner.state.lock().await;
        if state.prepared.contains_key(&id) {
            drop(state);
            send_source_reply(out_tx, host_id, &id, "ready", None).await;
            return;
        }
        if let Some(upload) = state.uploads.get_mut(&id) {
            let same = upload.digest == digest
                && upload.base_revision == base_revision
                && upload.expected_bytes == expected_bytes;
            if same {
                // Begin is an idempotent restart, not a resume protocol. Home
                // always retransmits from offset zero after a detach.
                upload.bytes.clear();
            }
            drop(state);
            send_source_reply(
                out_tx,
                host_id,
                &id,
                if same { "upload" } else { "error" },
                (!same).then_some("source id is already reserved with different metadata"),
            )
            .await;
            return;
        }
        if state.uploads.len() >= MAX_WORKER_UPLOADS {
            drop(state);
            send_source_reply(
                out_tx,
                host_id,
                &id,
                "error",
                Some("worker already has the maximum number of source uploads"),
            )
            .await;
            return;
        }
        state.uploads.insert(
            id.clone(),
            SourceUpload {
                digest,
                base_revision,
                expected_bytes,
                bytes: Vec::with_capacity(expected_bytes.min(4 * 1024 * 1024)),
            },
        );
        drop(state);
        send_source_reply(out_tx, host_id, &id, "upload", None).await;
    }

    async fn chunk(&self, frame: &serde_json::Value, out_tx: &mpsc::Sender<String>, host_id: &str) {
        let id = frame_string(frame, "id");
        let offset = frame
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        let data = frame
            .get("data")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok());
        let mut state = self.inner.state.lock().await;
        let Some(upload) = state.uploads.get_mut(&id) else {
            drop(state);
            send_source_reply(
                out_tx,
                host_id,
                &id,
                "error",
                Some("source upload was not begun"),
            )
            .await;
            return;
        };
        let valid = offset == Some(upload.bytes.len())
            && data.as_ref().is_some_and(|data| {
                data.len() <= SOURCE_CHUNK_BYTES
                    && upload.bytes.len().saturating_add(data.len()) <= upload.expected_bytes
            });
        if !valid {
            state.uploads.remove(&id);
            drop(state);
            send_source_reply(
                out_tx,
                host_id,
                &id,
                "error",
                Some("source chunk is invalid or out of order"),
            )
            .await;
            return;
        }
        upload
            .bytes
            .extend_from_slice(data.as_deref().unwrap_or_default());
    }

    async fn finish(
        &self,
        frame: &serde_json::Value,
        out_tx: &mpsc::Sender<String>,
        host_id: &str,
    ) {
        let id = frame_string(frame, "id");
        let upload = self.inner.state.lock().await.uploads.remove(&id);
        let Some(upload) = upload else {
            send_source_reply(
                out_tx,
                host_id,
                &id,
                "error",
                Some("source upload was not begun"),
            )
            .await;
            return;
        };
        if upload.bytes.len() != upload.expected_bytes || sha256_hex(&upload.bytes) != upload.digest
        {
            send_source_reply(
                out_tx,
                host_id,
                &id,
                "error",
                Some("source archive length or digest mismatch"),
            )
            .await;
            return;
        }
        let project_root = self.inner.project_root.clone();
        let workspace = self
            .inner
            .scratch
            .path()
            .join(format!("source-{}", &upload.digest[..16]));
        let bytes = upload.bytes;
        let materialize_revision = upload.base_revision;
        let materialized = tokio::task::spawn_blocking(move || {
            materialize_snapshot(&project_root, &workspace, &bytes, &materialize_revision)
                .map(|baseline_source_digest| (workspace, baseline_source_digest))
        })
        .await
        .map_err(|error| format!("source materialization task failed: {error}"))
        .and_then(|result| result);
        let (root, baseline_source_digest) = match materialized {
            Ok(result) => result,
            Err(error) => {
                send_source_reply(out_tx, host_id, &id, "error", Some(&error)).await;
                return;
            }
        };

        let prepared = Arc::new(PreparedSource {
            root,
            baseline_source_digest,
            command_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        let mut state = self.inner.state.lock().await;
        while state.prepared.len() >= MAX_PREPARED_SOURCES {
            let removable = state.order.iter().position(|source_id| {
                state
                    .prepared
                    .get(source_id)
                    .is_some_and(|source| Arc::strong_count(source) == 1)
            });
            let Some(position) = removable else {
                drop(state);
                cleanup_worktree(&self.inner.project_root, &prepared.root);
                send_source_reply(
                    out_tx,
                    host_id,
                    &id,
                    "error",
                    Some("worker source cache is full with active workspaces"),
                )
                .await;
                return;
            };
            if let Some(old_id) = state.order.remove(position) {
                if let Some(old) = state.prepared.remove(&old_id) {
                    cleanup_worktree(&self.inner.project_root, &old.root);
                }
            }
        }
        state.order.push_back(id.clone());
        state.prepared.insert(id.clone(), prepared);
        drop(state);
        send_source_reply(out_tx, host_id, &id, "ready", None).await;
    }

    pub async fn acquire(&self, source_id: &str) -> Result<WorkerSourceLease, String> {
        let prepared = self
            .inner
            .state
            .lock()
            .await
            .prepared
            .get(source_id)
            .cloned()
            .ok_or_else(|| "remote source snapshot is not prepared on this worker".to_string())?;
        let guard = Arc::clone(&prepared.command_lock).lock_owned().await;
        let actual_source_digest = workspace_source_digest(&prepared.root).await?;
        if actual_source_digest != prepared.baseline_source_digest {
            {
                let mut state = self.inner.state.lock().await;
                if state
                    .prepared
                    .get(source_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &prepared))
                {
                    state.prepared.remove(source_id);
                    state.order.retain(|candidate| candidate != source_id);
                }
            }
            let root = prepared.root.clone();
            drop(guard);
            cleanup_worktree(&self.inner.project_root, &root);
            return Err(
                "prepared source changed since capture; it was discarded, so retry the job to rematerialize the snapshot"
                    .to_string(),
            );
        }
        Ok(WorkerSourceLease {
            root: prepared.root.clone(),
            baseline_source_digest: prepared.baseline_source_digest.clone(),
            _guard: guard,
            _prepared: prepared,
        })
    }
}

fn frame_string(frame: &serde_json::Value, name: &str) -> String {
    frame
        .get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

async fn send_source_reply(
    out_tx: &mpsc::Sender<String>,
    host_id: &str,
    id: &str,
    state: &str,
    error: Option<&str>,
) {
    let mut frame = serde_json::json!({
        "t": REMOTE_SOURCE_RESULT_KIND,
        "host_id": host_id,
        "id": id,
        "state": state,
    });
    if let Some(error) = error {
        frame["error"] = error.into();
    }
    let _ = out_tx.send(frame.to_string()).await;
}

fn materialize_snapshot(
    project_root: &Path,
    workspace: &Path,
    compressed: &[u8],
    expected_base_revision: &str,
) -> Result<String, String> {
    if workspace.exists() {
        cleanup_worktree(project_root, workspace);
    }
    let decoder = flate2::read::GzDecoder::new(compressed);
    let mut expanded = Vec::new();
    decoder
        .take((MAX_SOURCE_EXPANDED_BYTES + 1) as u64)
        .read_to_end(&mut expanded)
        .map_err(|error| format!("decompress source snapshot: {error}"))?;
    if expanded.len() > MAX_SOURCE_EXPANDED_BYTES {
        return Err("source snapshot exceeds the expanded size limit".to_string());
    }
    let archive: SnapshotArchive = serde_json::from_slice(&expanded)
        .map_err(|error| format!("parse source snapshot: {error}"))?;
    if archive.version != 1 {
        return Err(format!(
            "unsupported source snapshot version {}",
            archive.version
        ));
    }
    if archive.base_revision != expected_base_revision {
        return Err(
            "source snapshot base revision does not match its transfer metadata".to_string(),
        );
    }
    for entry in &archive.untracked {
        validate_source_path(&entry.path)?;
        if entry.kind != SnapshotEntryKind::File {
            return Err(format!(
                "unsupported source snapshot entry kind for {:?}",
                entry.path
            ));
        }
    }

    let workspace_text = workspace
        .to_str()
        .ok_or_else(|| "worker source workspace path is not UTF-8".to_string())?;
    git_stdout(
        project_root,
        &[
            "worktree",
            "add",
            "--detach",
            "--force",
            workspace_text,
            expected_base_revision,
        ],
        None,
    )?;
    let result = (|| {
        if !archive.git_patch.is_empty() {
            git_stdout(
                workspace,
                &["apply", "--binary", "--whitespace=nowarn", "-"],
                Some(archive.git_patch.as_bytes()),
            )?;
        }
        for entry in archive.untracked {
            write_untracked_entry(workspace, &entry)?;
        }
        workspace_source_digest_sync(workspace)
    })();
    if result.is_err() {
        cleanup_worktree(project_root, workspace);
    }
    result
}

fn write_untracked_entry(workspace: &Path, entry: &SnapshotEntry) -> Result<(), String> {
    validate_source_path(&entry.path)?;
    let relative = Path::new(&entry.path);
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut cursor = workspace.to_path_buf();
    for component in parent.components() {
        let Component::Normal(part) = component else {
            return Err(format!("unsafe source path {:?}", entry.path));
        };
        cursor.push(part);
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "source path {:?} crosses a non-directory or symlink",
                    entry.path
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cursor).map_err(|error| {
                    format!("create source directory {}: {error}", cursor.display())
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "inspect source directory {}: {error}",
                    cursor.display()
                ))
            }
        }
    }
    let path = workspace.join(relative);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&entry.data_b64)
        .map_err(|error| format!("decode untracked source {:?}: {error}", entry.path))?;
    if bytes.len() as u64 > MAX_UNTRACKED_FILE_BYTES {
        return Err(format!(
            "untracked source {:?} exceeds the per-file limit",
            entry.path
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(&path)
        .map_err(|error| format!("create untracked source {:?}: {error}", entry.path))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write untracked source {:?}: {error}", entry.path))?;
    #[cfg(unix)]
    if entry.executable {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(
            |error| format!("mark untracked source {:?} executable: {error}", entry.path),
        )?;
    }
    Ok(())
}

fn cleanup_worktree(project_root: &Path, workspace: &Path) {
    if let Some(workspace) = workspace.to_str() {
        let _ = git_stdout(
            project_root,
            &["worktree", "remove", "--force", workspace],
            None,
        );
    }
    if workspace.exists() {
        let _ = std::fs::remove_dir_all(workspace);
    }
}

fn workspace_source_digest_sync(workspace: &Path) -> Result<String, String> {
    let patch = git_stdout(
        workspace,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
            "--",
        ],
        None,
    )?;
    if patch.len() > MAX_SOURCE_EXPANDED_BYTES {
        return Err("prepared source diff exceeds the source-state size limit".to_string());
    }
    let untracked_output = git_stdout(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
    )?;
    let mut paths = Vec::new();
    for raw in untracked_output.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(raw)
            .map_err(|_| "prepared source contains a non-UTF-8 path".to_string())?
            .to_string();
        validate_source_path(&path)?;
        paths.push(path);
    }
    paths.sort();
    if paths.len() > MAX_UNTRACKED_FILES {
        return Err("prepared source has too many non-ignored untracked files".to_string());
    }

    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|error| format!("resolve prepared source workspace: {error}"))?;
    let mut expanded_bytes = patch.len();
    let mut digest = sha2::Sha256::new();
    hash_source_part(&mut digest, b"patch", &patch);
    for path in paths {
        let local_path = workspace.join(&path);
        let metadata = std::fs::symlink_metadata(&local_path)
            .map_err(|error| format!("inspect prepared source {path}: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!("prepared source {path:?} is not a regular file"));
        }
        if metadata.len() > MAX_UNTRACKED_FILE_BYTES {
            return Err(format!(
                "prepared source {path:?} exceeds the per-file size limit"
            ));
        }
        let canonical = local_path
            .canonicalize()
            .map_err(|error| format!("resolve prepared source {path}: {error}"))?;
        if !canonical.starts_with(&canonical_workspace) {
            return Err(format!(
                "prepared source {path:?} resolves outside its workspace"
            ));
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|error| format!("read prepared source {path}: {error}"))?;
        expanded_bytes = expanded_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| "prepared source state is too large".to_string())?;
        if expanded_bytes > MAX_SOURCE_EXPANDED_BYTES {
            return Err("prepared source state exceeds the expanded size limit".to_string());
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt as _;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        hash_source_part(&mut digest, b"path", path.as_bytes());
        hash_source_part(&mut digest, b"mode", &[u8::from(executable)]);
        hash_source_part(&mut digest, b"data", &bytes);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn hash_source_part(digest: &mut sha2::Sha256, label: &[u8], bytes: &[u8]) {
    digest.update((label.len() as u64).to_le_bytes());
    digest.update(label);
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

pub(super) async fn workspace_source_digest(workspace: &Path) -> Result<String, String> {
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || workspace_source_digest_sync(&workspace))
        .await
        .map_err(|error| format!("prepared source digest task failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) -> String {
        String::from_utf8_lossy(&git_stdout(repo, args, None).unwrap())
            .trim()
            .to_string()
    }

    fn base_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        std::fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "tracked.txt"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Intendant Test",
                "-c",
                "user.email=intendant-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        let revision = git(dir.path(), &["rev-parse", "HEAD"]);
        (dir, revision)
    }

    #[test]
    fn capture_and_materialize_tracks_changes_and_nonignored_untracked_files() {
        let (home, revision) = base_repo();
        let worker = tempfile::tempdir().unwrap();
        git(
            worker.path(),
            &["clone", "--quiet", home.path().to_str().unwrap(), "."],
        );

        std::fs::write(home.path().join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(home.path().join("new.txt"), "new\n").unwrap();
        std::fs::write(
            home.path().join(".gitignore"),
            "ignored.txt\n.env\ntarget/\n",
        )
        .unwrap();
        git(home.path(), &["add", ".gitignore"]);
        std::fs::write(home.path().join("ignored.txt"), "secret-ish\n").unwrap();
        std::fs::write(home.path().join(".env"), "API_KEY=never-transfer\n").unwrap();
        std::fs::create_dir(home.path().join("target")).unwrap();
        std::fs::write(home.path().join("target/build-cache"), "ephemeral\n").unwrap();

        let snapshot = capture_working_tree(home.path(), Some(&revision)).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let workspace = scratch.path().join("source");
        let baseline = materialize_snapshot(
            worker.path(),
            &workspace,
            snapshot.archive.as_slice(),
            &revision,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("new.txt")).unwrap(),
            "new\n"
        );
        assert!(!workspace.join("ignored.txt").exists());
        assert!(!workspace.join(".env").exists());
        assert!(!workspace.join("target").exists());
        assert_eq!(baseline, workspace_source_digest_sync(&workspace).unwrap());
        std::fs::write(workspace.join("tracked.txt"), "different bytes\n").unwrap();
        assert_ne!(baseline, workspace_source_digest_sync(&workspace).unwrap());
        std::fs::write(workspace.join("tracked.txt"), "changed\n").unwrap();
        assert_eq!(baseline, workspace_source_digest_sync(&workspace).unwrap());
        std::fs::write(workspace.join("new.txt"), "different untracked bytes\n").unwrap();
        assert_ne!(baseline, workspace_source_digest_sync(&workspace).unwrap());
    }

    #[test]
    fn unsafe_and_symlinked_untracked_paths_are_refused() {
        assert!(validate_source_path("../escape").is_err());
        assert!(validate_source_path("/absolute").is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let (repo, revision) = base_repo();
            symlink("/tmp", repo.path().join("out")).unwrap();
            let error = capture_working_tree(repo.path(), Some(&revision)).unwrap_err();
            assert!(error.contains("symlink"), "{error}");
        }
    }

    #[test]
    fn archive_digest_is_stable_for_an_unchanged_tree() {
        let (repo, revision) = base_repo();
        std::fs::write(repo.path().join("untracked.txt"), "same\n").unwrap();
        let first = capture_working_tree(repo.path(), Some(&revision)).unwrap();
        let second = capture_working_tree(repo.path(), Some(&revision)).unwrap();
        assert_eq!(first.digest, second.digest);
        assert_eq!(first.archive.as_slice(), second.archive.as_slice());
    }

    #[test]
    fn provider_branch_validation_uses_git_ref_rules() {
        assert_eq!(
            validate_provider_branch("feature/cloud-worker").unwrap(),
            "feature/cloud-worker"
        );
        assert!(validate_provider_branch("-provider-option").is_err());
        assert!(validate_provider_branch("feature..broken").is_err());
        assert!(validate_provider_branch("refs/heads/").is_err());
    }

    #[tokio::test]
    async fn worker_protocol_verifies_and_prepares_a_chunked_snapshot() {
        let (home, revision) = base_repo();
        let worker = tempfile::tempdir().unwrap();
        git(
            worker.path(),
            &["clone", "--quiet", home.path().to_str().unwrap(), "."],
        );
        std::fs::write(home.path().join("tracked.txt"), "remote\n").unwrap();
        let snapshot = capture_working_tree(home.path(), Some(&revision)).unwrap();
        let sources = WorkerSources::new(worker.path().to_path_buf()).unwrap();
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let host = "cloud:test";

        let begin = serde_json::json!({
            "t": REMOTE_SOURCE_BEGIN_KIND,
            "id": snapshot.source_id,
            "digest": snapshot.digest,
            "base_revision": snapshot.base_revision,
            "compressed_bytes": snapshot.archive.len(),
        });
        assert!(sources.serve_frame(&begin, &out_tx, host).await);
        let upload: serde_json::Value =
            serde_json::from_str(&out_rx.recv().await.unwrap()).unwrap();
        assert_eq!(upload["state"], "upload");

        for (index, chunk) in snapshot.archive.chunks(SOURCE_CHUNK_BYTES).enumerate() {
            let frame = serde_json::json!({
                "t": REMOTE_SOURCE_CHUNK_KIND,
                "id": snapshot.source_id,
                "offset": index * SOURCE_CHUNK_BYTES,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
            });
            assert!(sources.serve_frame(&frame, &out_tx, host).await);
        }
        let finish = serde_json::json!({
            "t": REMOTE_SOURCE_FINISH_KIND,
            "id": snapshot.source_id,
        });
        assert!(sources.serve_frame(&finish, &out_tx, host).await);
        let ready: serde_json::Value = serde_json::from_str(&out_rx.recv().await.unwrap()).unwrap();
        assert_eq!(ready["state"], "ready");

        let lease = sources.acquire(&snapshot.source_id).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(lease.root.join("tracked.txt")).unwrap(),
            "remote\n"
        );
        let prepared_root = lease.root.clone();
        drop(lease);
        std::fs::write(prepared_root.join("tracked.txt"), "mutated\n").unwrap();
        let error = match sources.acquire(&snapshot.source_id).await {
            Err(error) => error,
            Ok(_) => panic!("mutated prepared source should be rejected"),
        };
        assert!(error.contains("discarded"), "{error}");
    }
}
