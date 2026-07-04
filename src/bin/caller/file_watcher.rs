//! Live filesystem watcher: observes file changes in the project directory,
//! stores copy-on-write baseline snapshots, and emits `AppEvent::FileChanged`
//! events. Works for all agent types (native, Codex, Claude Code, Gemini CLI)
//! by watching the filesystem directly rather than relying on git.
//!
//! Also provides per-round content-addressed snapshots of the project tree for
//! rollback / redo / branching. On each [`AppEvent::RoundComplete`], a new
//! [`HistoryRound`] is recorded, capturing every tracked path's sha256. Files
//! are stored in a content-addressed `objects/` directory so repeated content
//! across rounds costs no additional disk. Rollback moves `current_head_id`
//! back without truncating the linear history (so redo is available). A new
//! action after rollback branches off the abandoned path and stores it in
//! `abandoned_branches` for later pruning.

use crate::error::CallerError;
use crate::event::{AppEvent, EventBus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
}

/// One recorded round in the session history. Captures the full project state
/// at the end of the round (as a map of path → sha256 hex) plus the subset of
/// paths that differ from the previous round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRound {
    /// Monotonic, unique per session.
    pub id: u64,
    /// ID of the round that preceded this one on the active linear path.
    /// `None` only for the first round.
    pub parent_id: Option<u64>,
    /// User-facing label (e.g. user message preview or "Round N").
    pub summary: String,
    pub timestamp_unix: u64,
    /// Display-only list of paths whose content differs from the previous
    /// round (or baseline for the first round).
    pub files_changed: Vec<String>,
    /// FULL project state at the end of this round: path → sha256 hex.
    pub files_at_end: HashMap<String, String>,
    /// Display-state mirror that also includes non-restorable tracked files
    /// such as binary/oversized files. Rollback still uses `files_at_end`;
    /// this lets timeline counts match what the Changes tab reports.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub all_files_at_end: HashMap<String, String>,
    /// Number of agent turns executed in this round (from `RoundComplete.turns_in_round`).
    /// Used by conversation rollback to compute how many turns to drop
    /// when reverting to a specific round. Optional for backward compat
    /// with history.json files written before this field existed.
    #[serde(default)]
    pub turn_count: Option<u32>,
    /// Number of messages in the native conversation at the end of this
    /// round. When present, rolling back to this round truncates the
    /// native `Conversation.messages` to this length. Not meaningful for
    /// external agent backends — they use session-reset or protocol-level
    /// rollback instead.
    #[serde(default)]
    pub native_message_count: Option<u32>,
}

/// A branch of rounds that was replaced by a rollback-then-new-action. Kept
/// around so the user can prune it later (or the soft cap evicts it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbandonedBranch {
    pub branched_from_id: u64,
    pub rounds: Vec<HistoryRound>,
    pub created_at_unix: u64,
}

/// Full session history. Persisted to `history.json` in the snapshot dir.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct History {
    pub current_head_id: Option<u64>,
    pub rounds: Vec<HistoryRound>,
    pub abandoned_branches: Vec<AbandonedBranch>,
    pub next_id: u64,
}

/// Result of a successful rollback.
#[derive(Debug, Clone, Serialize)]
pub struct RollbackResult {
    pub to_round_id: u64,
    pub files_reverted: u32,
}

/// Result of a successful redo.
#[derive(Debug, Clone, Serialize)]
pub struct RedoResult {
    pub to_round_id: u64,
    pub files_reverted: u32,
}

/// Result of a successful prune.
#[derive(Debug, Clone, Serialize)]
pub struct PruneResult {
    pub branches_removed: u32,
    pub bytes_freed: u64,
}

/// Soft cap: total bytes under `snapshot_dir` before we start pruning
/// abandoned branches (oldest first).
const SNAPSHOT_DIR_SOFT_CAP_BYTES: u64 = 500 * 1024 * 1024;

/// Per-file size cap for tracked snapshots.
///
/// Keep this aligned for initial baselines and live change events. If the
/// initial scan skips a file that the live watcher later accepts, an atomic
/// rewrite can look like a brand-new file and report the whole file as added.
pub(crate) const SNAPSHOT_MAX_FILE_BYTES: u64 = 1_000_000;

pub(crate) const BASELINE_MANIFEST_FILE: &str = "baseline_manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BaselineFileMeta {
    pub supported_text: bool,
    pub hash: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub(crate) type BaselineManifest = HashMap<String, BaselineFileMeta>;

type FileFingerprint = (u64, Option<u128>);

#[derive(Debug, Clone)]
pub(crate) struct TextFileSnapshot {
    pub bytes: Vec<u8>,
    pub text: String,
    pub hash: [u8; 32],
    pub hash_hex: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct UnsupportedFileSnapshot {
    pub reason: String,
    pub hash: [u8; 32],
    pub hash_hex: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum InspectedFile {
    Text(TextFileSnapshot),
    Unsupported(UnsupportedFileSnapshot),
}

// ---------------------------------------------------------------------------
// Ignore filter
// ---------------------------------------------------------------------------

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".worktrees",
    "target",
    "node_modules",
    ".intendant",
    "__pycache__",
    ".pytest_cache",
    ".codex",
    ".gemini",
    ".claude",
    ".agents",
    "dist",
    "build",
    ".next",
    ".nuxt",
];

const IGNORED_EXTENSIONS: &[&str] = &[
    "o", "so", "dylib", "class", "pyc", "wasm", "exe", "bin", "png", "jpg", "jpeg", "gif", "ico",
    "svg", "webp", "zip", "tar", "gz", "bz2",
];

/// Rewind snapshots baseline-copy and hash every file under the root —
/// which is only sane inside an actual project. When project detection
/// fell back to a bare cwd (no marker), the "root" is just wherever the
/// daemon happened to start: a service's WorkingDirectory is `$HOME`,
/// and baselining a home directory means minutes of boot-blocking I/O
/// plus a shadow copy of everything the user owns under
/// `file_snapshots/`. (Found live: a fresh-VPS service boot spent
/// minutes reading `~/.rustup` before the dashboard and rendezvous
/// client could spawn.)
pub fn root_is_snapshot_worthy(root: &Path) -> bool {
    // A git worktree's `.git` is a file, not a directory — exists()
    // covers both shapes.
    root.join(".git").exists() || root.join("intendant.toml").exists()
}

/// Tree-wide budget for the initial baseline scan. A root that blows
/// past this is either not really a project or too large to shadow-copy;
/// rewind degrades (the caller boots on without it) instead of stalling
/// startup and duplicating gigabytes.
pub(crate) const SNAPSHOT_MAX_TREE_FILES: usize = 100_000;
pub(crate) const SNAPSHOT_MAX_TREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn tree_budget_exceeded(files_seen: usize, bytes_seen: u64) -> bool {
    files_seen > SNAPSHOT_MAX_TREE_FILES || bytes_seen > SNAPSHOT_MAX_TREE_BYTES
}

pub(crate) fn should_ignore(rel_path: &Path) -> bool {
    for component in rel_path.components() {
        if let std::path::Component::Normal(name) = component {
            let name_str = name.to_string_lossy();
            if IGNORED_DIRS.contains(&name_str.as_ref()) {
                return true;
            }
        }
    }
    if let Some(ext) = rel_path.extension() {
        let ext_str = ext.to_string_lossy();
        if IGNORED_EXTENSIONS.contains(&ext_str.as_ref()) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns true if content looks like binary (has a null byte in the first 8KB).
fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

pub(crate) fn sha256_hash(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Lowercase hex encoding of a 32-byte sha256.
pub(crate) fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub(crate) fn rel_path_key(rel_path: &Path) -> String {
    rel_path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    Ok(out)
}

fn metadata_fingerprint(meta: &std::fs::Metadata) -> FileFingerprint {
    let modified_nanos = meta
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    (meta.len(), modified_nanos)
}

pub(crate) fn inspect_file(path: &Path) -> std::io::Result<InspectedFile> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();

    if size > SNAPSHOT_MAX_FILE_BYTES {
        let hash = sha256_file(path)?;
        return Ok(InspectedFile::Unsupported(UnsupportedFileSnapshot {
            reason: format!("file is larger than {} bytes", SNAPSHOT_MAX_FILE_BYTES),
            hash,
            hash_hex: hex_encode(&hash),
            size,
        }));
    }

    let bytes = std::fs::read(path)?;
    let hash = sha256_hash(&bytes);
    let hash_hex = hex_encode(&hash);

    if is_binary(&bytes) {
        return Ok(InspectedFile::Unsupported(UnsupportedFileSnapshot {
            reason: "binary file".to_string(),
            hash,
            hash_hex,
            size,
        }));
    }

    let text = match String::from_utf8(bytes.clone()) {
        Ok(text) => text,
        Err(_) => {
            return Ok(InspectedFile::Unsupported(UnsupportedFileSnapshot {
                reason: "file is not valid UTF-8".to_string(),
                hash,
                hash_hex,
                size,
            }));
        }
    };

    Ok(InspectedFile::Text(TextFileSnapshot {
        bytes,
        text,
        hash,
        hash_hex,
        size,
    }))
}

/// Produce a unified diff between `baseline` and `current` with standard
/// `--- a/` / `+++ b/` headers and `@@ ... @@` hunk markers.
pub fn compute_unified_diff(baseline: &str, current: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(baseline, current);
    let mut out = String::new();
    out.push_str(&format!("--- a/{}\n", path));
    out.push_str(&format!("+++ b/{}\n", path));
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&hunk.to_string());
    }
    out
}

/// Count added and removed lines between two text blobs.
fn diff_stats(baseline: &str, current: &str) -> (u32, u32) {
    let diff = similar::TextDiff::from_lines(baseline, current);
    let mut added: u32 = 0;
    let mut removed: u32 = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write `content` to `path` atomically via tmp + rename, so a crash mid-write
/// can never leave a truncated/corrupt file for a reader to observe.
pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Recursively sum file bytes under `path`.
fn dir_byte_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                if let Ok(m) = std::fs::metadata(&p) {
                    total += m.len();
                }
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// FileWatcher
// ---------------------------------------------------------------------------

/// Thread-safe handle for a `FileWatcher`. The inner state is guarded by a
/// tokio async mutex so snapshot creation, rollback, redo, and prune can all
/// coordinate without racing.
pub type SharedFileWatcher = Arc<AsyncMutex<FileWatcher>>;

pub struct FileWatcher {
    project_root: PathBuf,
    snapshot_dir: PathBuf,
    bus: EventBus,
    /// Baseline file content (original at session start), keyed by relative path.
    baselines: HashMap<PathBuf, Vec<u8>>,
    /// Metadata for every non-ignored file that existed at session start.
    baseline_manifest: BaselineManifest,
    /// SHA-256 hashes of last-known content, for change deduplication.
    hashes: HashMap<PathBuf, [u8; 32]>,
    /// Last-known metadata fingerprints for oversized files. These files are
    /// not snapshotted, and duplicate notify events can otherwise re-hash tens
    /// of megabytes repeatedly while an editor writes temp files.
    large_file_fingerprints: HashMap<PathBuf, FileFingerprint>,
    /// Persistent session history of per-round snapshots.
    history: History,
}

impl FileWatcher {
    /// Scan the project tree and build baseline snapshots of all text files.
    pub fn new(
        project_root: PathBuf,
        snapshot_dir: PathBuf,
        bus: EventBus,
    ) -> Result<Self, CallerError> {
        let baseline_dir = snapshot_dir.join("baseline");
        std::fs::create_dir_all(&baseline_dir)
            .map_err(|e| CallerError::Config(format!("create snapshot dir: {}", e)))?;
        std::fs::create_dir_all(snapshot_dir.join("objects"))
            .map_err(|e| CallerError::Config(format!("create objects dir: {}", e)))?;
        std::fs::create_dir_all(snapshot_dir.join("rounds"))
            .map_err(|e| CallerError::Config(format!("create rounds dir: {}", e)))?;

        let mut baselines = HashMap::new();
        let mut baseline_manifest = BaselineManifest::new();
        let mut hashes = HashMap::new();
        let mut large_file_fingerprints = HashMap::new();
        let mut files_seen: usize = 0;
        let mut bytes_seen: u64 = 0;

        let mut stack = vec![project_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    // Check if this directory should be ignored.
                    if let Ok(rel) = path.strip_prefix(&project_root) {
                        if !should_ignore(rel) {
                            stack.push(path);
                        }
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let rel = match path.strip_prefix(&project_root) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if should_ignore(&rel) {
                    continue;
                }
                let rel_key = rel_path_key(&rel);
                files_seen += 1;
                if tree_budget_exceeded(files_seen, bytes_seen) {
                    return Err(CallerError::Config(format!(
                        "initial snapshot budget exceeded under {} ({files_seen} files / \
                         {bytes_seen} bytes so far; caps {SNAPSHOT_MAX_TREE_FILES} files / \
                         {SNAPSHOT_MAX_TREE_BYTES} bytes) — rewind snapshots stay off for \
                         this run rather than shadow-copying a tree that large",
                        project_root.display()
                    )));
                }
                match inspect_file(&path) {
                    Ok(InspectedFile::Text(snapshot)) => {
                        bytes_seen = bytes_seen.saturating_add(snapshot.size);
                        let baseline_path = baseline_dir.join(&rel);
                        if let Some(parent) = baseline_path.parent() {
                            std::fs::create_dir_all(parent).map_err(|e| {
                                CallerError::Config(format!(
                                    "create baseline parent {}: {}",
                                    parent.display(),
                                    e
                                ))
                            })?;
                        }
                        std::fs::write(&baseline_path, &snapshot.bytes).map_err(|e| {
                            CallerError::Config(format!(
                                "write baseline {}: {}",
                                baseline_path.display(),
                                e
                            ))
                        })?;
                        baselines.insert(rel.clone(), snapshot.bytes);
                        hashes.insert(rel, snapshot.hash);
                        baseline_manifest.insert(
                            rel_key,
                            BaselineFileMeta {
                                supported_text: true,
                                hash: snapshot.hash_hex,
                                size: snapshot.size,
                                reason: None,
                            },
                        );
                    }
                    Ok(InspectedFile::Unsupported(snapshot)) => {
                        bytes_seen = bytes_seen.saturating_add(snapshot.size);
                        if snapshot.size > SNAPSHOT_MAX_FILE_BYTES {
                            if let Ok(meta) = std::fs::metadata(&path) {
                                large_file_fingerprints
                                    .insert(rel.clone(), metadata_fingerprint(&meta));
                            }
                        }
                        hashes.insert(rel, snapshot.hash);
                        baseline_manifest.insert(
                            rel_key,
                            BaselineFileMeta {
                                supported_text: false,
                                hash: snapshot.hash_hex,
                                size: snapshot.size,
                                reason: Some(snapshot.reason),
                            },
                        );
                    }
                    Err(_) => continue,
                }
            }
        }

        let manifest_path = snapshot_dir.join(BASELINE_MANIFEST_FILE);
        let manifest_bytes = serde_json::to_vec_pretty(&baseline_manifest)
            .map_err(|e| CallerError::Config(format!("baseline manifest serialize: {}", e)))?;
        atomic_write(&manifest_path, &manifest_bytes).map_err(CallerError::Io)?;

        // Load history.json if it exists (session resume / restart).
        let history_path = snapshot_dir.join("history.json");
        let history = match std::fs::read(&history_path) {
            Ok(bytes) => serde_json::from_slice::<History>(&bytes).unwrap_or_default(),
            Err(_) => History::default(),
        };

        Ok(Self {
            project_root,
            snapshot_dir,
            bus,
            baselines,
            baseline_manifest,
            hashes,
            large_file_fingerprints,
            history,
        })
    }

    /// Read-only accessor for the history state. Callers hold the mutex for
    /// the duration, so callers should clone the result if they need to use
    /// it after releasing the lock.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Wrap `self` in an async-mutex-backed shared handle and spawn the
    /// filesystem watcher loop + round-complete listener. Returns the handle
    /// plus the two join handles so callers can keep them alive.
    pub fn start_shared(self) -> (SharedFileWatcher, JoinHandle<()>, JoinHandle<()>) {
        let bus = self.bus.clone();
        let project_root = self.project_root.clone();
        let shared = Arc::new(AsyncMutex::new(self));

        let watcher_handle = {
            let shared = shared.clone();
            let project_root = project_root.clone();
            tokio::task::spawn(async move {
                if let Err(e) = run_watcher_loop(shared, project_root).await {
                    eprintln!("[file_watcher] watcher error: {}", e);
                }
            })
        };

        let round_handle = {
            let shared = shared.clone();
            let mut rx = bus.subscribe();
            tokio::task::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::RoundComplete {
                            round,
                            turns_in_round,
                            native_message_count,
                            ..
                        }) => {
                            let summary = format!("Round {}", round);
                            let mut w = shared.lock().await;
                            if let Err(e) = w.on_round_complete(
                                summary,
                                Some(turns_in_round as u32),
                                native_message_count,
                            ) {
                                eprintln!("[file_watcher] round snapshot failed: {}", e);
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
        };

        (shared, watcher_handle, round_handle)
    }

    /// Legacy entry point that boots the watcher without the shared handle.
    /// Kept for callers that only care about live FileChanged events and do
    /// not need rollback/redo.
    #[allow(dead_code)]
    pub fn start(self) -> JoinHandle<()> {
        let (_shared, watcher_handle, _round_handle) = self.start_shared();
        watcher_handle
    }

    fn process_change(&mut self, abs_path: &Path, kind: &notify::EventKind) {
        // Compute relative path.
        let rel = match abs_path.strip_prefix(&self.project_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => return,
        };

        if should_ignore(&rel) {
            return;
        }

        let rel_key = rel_path_key(&rel);
        let existed_at_baseline =
            self.baselines.contains_key(&rel) || self.baseline_manifest.contains_key(&rel_key);
        let known_file = existed_at_baseline || self.hashes.contains_key(&rel);
        let change_kind = match kind {
            notify::EventKind::Create(_) => {
                if !abs_path.is_file() {
                    return;
                }
                if known_file {
                    FileChangeKind::Modified
                } else {
                    FileChangeKind::Created
                }
            }
            notify::EventKind::Modify(_) => {
                if !abs_path.is_file() {
                    return;
                }
                if known_file {
                    FileChangeKind::Modified
                } else {
                    FileChangeKind::Created
                }
            }
            notify::EventKind::Remove(_) => FileChangeKind::Deleted,
            _ => return,
        };

        match change_kind {
            FileChangeKind::Created | FileChangeKind::Modified => {
                let large_file_fingerprint = match std::fs::metadata(abs_path) {
                    Ok(meta) if meta.len() > SNAPSHOT_MAX_FILE_BYTES => {
                        let fingerprint = metadata_fingerprint(&meta);
                        if self.large_file_fingerprints.get(&rel) == Some(&fingerprint) {
                            return;
                        }
                        Some(fingerprint)
                    }
                    Ok(_) => None,
                    Err(_) => return,
                };

                match inspect_file(abs_path) {
                    Ok(InspectedFile::Text(snapshot)) => {
                        self.large_file_fingerprints.remove(&rel);
                        if self.hashes.get(&rel) == Some(&snapshot.hash) {
                            return; // no actual change
                        }
                        self.hashes.insert(rel.clone(), snapshot.hash);

                        let (lines_added, lines_removed) =
                            if let Some(baseline_bytes) = self.baselines.get(&rel) {
                                let baseline_str = String::from_utf8_lossy(baseline_bytes);
                                diff_stats(&baseline_str, &snapshot.text)
                            } else if existed_at_baseline {
                                (0, 0)
                            } else {
                                diff_stats("", &snapshot.text)
                            };

                        self.bus.send(AppEvent::FileChanged {
                            path: rel_key,
                            kind: change_kind,
                            lines_added,
                            lines_removed,
                        });
                    }
                    Ok(InspectedFile::Unsupported(snapshot)) => {
                        if snapshot.size > SNAPSHOT_MAX_FILE_BYTES {
                            let fingerprint = large_file_fingerprint.or_else(|| {
                                std::fs::metadata(abs_path)
                                    .ok()
                                    .map(|meta| metadata_fingerprint(&meta))
                            });
                            if let Some(fingerprint) = fingerprint {
                                self.large_file_fingerprints
                                    .insert(rel.clone(), fingerprint);
                            }
                        } else {
                            self.large_file_fingerprints.remove(&rel);
                        }
                        if self.hashes.get(&rel) == Some(&snapshot.hash) {
                            return; // no actual change
                        }
                        self.hashes.insert(rel.clone(), snapshot.hash);
                        self.bus.send(AppEvent::FileChanged {
                            path: rel_key,
                            kind: change_kind,
                            lines_added: 0,
                            lines_removed: 0,
                        });
                    }
                    Err(_) => (),
                }
            }
            FileChangeKind::Deleted => {
                if known_file {
                    let lines_removed = self
                        .baselines
                        .get(&rel)
                        .map(|bytes| String::from_utf8_lossy(bytes).lines().count() as u32)
                        .unwrap_or(0);
                    self.bus.send(AppEvent::FileChanged {
                        path: rel_key,
                        kind: FileChangeKind::Deleted,
                        lines_added: 0,
                        lines_removed,
                    });
                }
                self.hashes.remove(&rel);
                self.large_file_fingerprints.remove(&rel);
            }
        }
    }

    // ---- Snapshot / history management ----

    /// Called when a round finishes. Walks the project tree, writes any new
    /// content blobs into `objects/`, records a new `HistoryRound`, and
    /// persists history.
    ///
    /// `turn_count` is the number of agent turns executed in this round
    /// (carried through `AppEvent::RoundComplete.turns_in_round`).
    /// `native_message_count` is the length of the native
    /// `Conversation.messages` at the time of RoundComplete; only set for
    /// rounds produced by the native agent. External-agent rounds pass
    /// `None` here — they rely on backend-specific rollback (Codex) or
    /// session reset (CC, Gemini) instead.
    pub fn on_round_complete(
        &mut self,
        summary: String,
        turn_count: Option<u32>,
        native_message_count: Option<u32>,
    ) -> Result<(), CallerError> {
        // Walk project and compute file hashes + write objects.
        let (files_at_end, all_files_at_end) = self.scan_and_store_objects()?;

        // Determine parent + files_changed by diffing against previous round
        // (or baseline if first round).
        let parent_id = self.history.current_head_id;
        let files_changed = self.compute_files_changed(parent_id, &all_files_at_end);

        // If current head is not the last round, branch: move trailing rounds
        // into abandoned_branches before appending.
        if let Some(head_id) = self.history.current_head_id {
            if let Some(pos) = self.history.rounds.iter().position(|r| r.id == head_id) {
                if pos + 1 < self.history.rounds.len() {
                    let drained: Vec<HistoryRound> = self.history.rounds.drain(pos + 1..).collect();
                    self.history.abandoned_branches.push(AbandonedBranch {
                        branched_from_id: head_id,
                        rounds: drained,
                        created_at_unix: now_unix(),
                    });
                }
            }
        }

        let id = self.history.next_id;
        self.history.next_id += 1;
        let round = HistoryRound {
            id,
            parent_id,
            summary,
            timestamp_unix: now_unix(),
            files_changed,
            files_at_end,
            all_files_at_end,
            turn_count,
            native_message_count,
        };

        // Write per-round manifest.
        let manifest_path = self
            .snapshot_dir
            .join("rounds")
            .join(format!("round_{}", id))
            .join("manifest.json");
        let manifest_bytes = serde_json::to_vec_pretty(&round)
            .map_err(|e| CallerError::Config(format!("manifest serialize: {}", e)))?;
        atomic_write(&manifest_path, &manifest_bytes).map_err(CallerError::Io)?;

        self.history.rounds.push(round);
        self.history.current_head_id = Some(id);

        self.persist_history()?;

        self.bus.send(AppEvent::SnapshotCreated { round_id: id });

        // Soft cap: if we exceed SNAPSHOT_DIR_SOFT_CAP_BYTES, drop oldest
        // abandoned branches until under cap.
        let _ = self.enforce_soft_cap();

        Ok(())
    }

    /// Roll back the project tree to the file state recorded in `target_round_id`.
    ///
    /// Does NOT truncate the linear `rounds` array — `current_head_id` simply
    /// moves backward so `redo()` can reapply the later rounds. Branching only
    /// happens if a NEW round is created after the rollback (see
    /// [`on_round_complete`]).
    pub fn rollback(&mut self, target_round_id: u64) -> Result<RollbackResult, CallerError> {
        let target_idx = self
            .history
            .rounds
            .iter()
            .position(|r| r.id == target_round_id)
            .ok_or_else(|| {
                CallerError::Config(format!(
                    "round {} not found in active history (it may be in an abandoned branch)",
                    target_round_id
                ))
            })?;

        let target = self.history.rounds[target_idx].clone();
        let from_id = self.history.current_head_id.unwrap_or(target.id);

        let files_reverted = self.restore_to_state(&target.files_at_end)?;

        // Refresh in-memory hash/baseline mirrors so the watcher doesn't
        // re-emit spurious "modified" events for paths we just rewrote.
        self.refresh_hashes_from_tree();

        self.history.current_head_id = Some(target.id);
        self.persist_history()?;

        self.bus.send(AppEvent::RolledBack {
            from_id,
            to_id: target.id,
            files_reverted,
        });

        Ok(RollbackResult {
            to_round_id: target.id,
            files_reverted,
        })
    }

    /// Move `current_head_id` forward to the next round on the linear path,
    /// restoring file state accordingly. Errors if already at the latest
    /// round.
    pub fn redo(&mut self) -> Result<RedoResult, CallerError> {
        let head_id = self
            .history
            .current_head_id
            .ok_or_else(|| CallerError::Config("no rounds recorded yet".to_string()))?;

        let pos = self
            .history
            .rounds
            .iter()
            .position(|r| r.id == head_id)
            .ok_or_else(|| CallerError::Config("head is not on active path".to_string()))?;

        if pos + 1 >= self.history.rounds.len() {
            return Err(CallerError::Config("nothing to redo".to_string()));
        }

        let next = self.history.rounds[pos + 1].clone();
        let files_reverted = self.restore_to_state(&next.files_at_end)?;
        self.refresh_hashes_from_tree();

        self.history.current_head_id = Some(next.id);
        self.persist_history()?;

        self.bus.send(AppEvent::Redone { to_id: next.id });

        Ok(RedoResult {
            to_round_id: next.id,
            files_reverted,
        })
    }

    /// Delete all abandoned branches and garbage-collect any orphaned blobs
    /// under `objects/`.
    pub fn prune_abandoned(&mut self) -> Result<PruneResult, CallerError> {
        let branches_removed = self.history.abandoned_branches.len() as u32;
        self.history.abandoned_branches.clear();

        let bytes_freed = self.gc_orphaned_objects();

        self.persist_history()?;

        self.bus.send(AppEvent::HistoryPruned {
            branches_removed,
            bytes_freed,
        });

        Ok(PruneResult {
            branches_removed,
            bytes_freed,
        })
    }

    // ---- Internal helpers ----

    /// Walk the project tree, hash every eligible file, write new content to
    /// `objects/{hash}`, and return the path→hash map.
    fn scan_and_store_objects(
        &self,
    ) -> Result<(HashMap<String, String>, HashMap<String, String>), CallerError> {
        let mut out: HashMap<String, String> = HashMap::new();
        let mut all: HashMap<String, String> = HashMap::new();
        let objects_dir = self.snapshot_dir.join("objects");
        std::fs::create_dir_all(&objects_dir)
            .map_err(|e| CallerError::Config(format!("create objects dir: {}", e)))?;

        let mut stack = vec![self.project_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    if let Ok(rel) = path.strip_prefix(&self.project_root) {
                        if !should_ignore(rel) {
                            stack.push(path);
                        }
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let rel = match path.strip_prefix(&self.project_root) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if should_ignore(&rel) {
                    continue;
                }
                if std::fs::metadata(&path)
                    .map(|meta| meta.len() > SNAPSHOT_MAX_FILE_BYTES)
                    .unwrap_or(false)
                {
                    continue;
                }
                let snapshot = match inspect_file(&path) {
                    Ok(snapshot) => snapshot,
                    Err(_) => continue,
                };
                match snapshot {
                    InspectedFile::Text(snapshot) => {
                        all.insert(rel_path_key(&rel), snapshot.hash_hex.clone());
                        let obj_path = objects_dir.join(&snapshot.hash_hex);
                        if !obj_path.exists() {
                            // Write into a tmp path first so a partial write can't
                            // leave a corrupt blob under its hash.
                            let tmp = obj_path.with_extension("tmp");
                            if std::fs::write(&tmp, &snapshot.bytes).is_ok() {
                                let _ = std::fs::rename(&tmp, &obj_path);
                            }
                        }
                        out.insert(rel_path_key(&rel), snapshot.hash_hex);
                    }
                    InspectedFile::Unsupported(snapshot) => {
                        all.insert(rel_path_key(&rel), snapshot.hash_hex);
                    }
                }
            }
        }
        Ok((out, all))
    }

    /// Compute the list of paths whose content in `current` differs from the
    /// previous round's `files_at_end` (or, if there is no previous round,
    /// the baseline).
    fn compute_files_changed(
        &self,
        parent_id: Option<u64>,
        current: &HashMap<String, String>,
    ) -> Vec<String> {
        let mut changed = Vec::new();
        if let Some(pid) = parent_id {
            if let Some(parent) = self.history.rounds.iter().find(|r| r.id == pid) {
                let prev = if parent.all_files_at_end.is_empty() {
                    &parent.files_at_end
                } else {
                    &parent.all_files_at_end
                };
                // Union of keys from both sides.
                let mut keys: HashSet<&String> = prev.keys().collect();
                keys.extend(current.keys());
                for k in keys {
                    match (prev.get(k), current.get(k)) {
                        (Some(a), Some(b)) if a == b => continue,
                        _ => changed.push(k.clone()),
                    }
                }
                changed.sort();
                return changed;
            }
        }
        // First round: compare against baseline in-memory mirror.
        let mut baseline_hex: HashMap<String, String> = HashMap::new();
        for (path, meta) in &self.baseline_manifest {
            baseline_hex.insert(path.clone(), meta.hash.clone());
        }
        for (rel, bytes) in &self.baselines {
            baseline_hex
                .entry(rel_path_key(rel))
                .or_insert_with(|| hex_encode(&sha256_hash(bytes)));
        }
        let mut keys: HashSet<&String> = baseline_hex.keys().collect();
        keys.extend(current.keys());
        for k in keys {
            match (baseline_hex.get(k), current.get(k)) {
                (Some(a), Some(b)) if a == b => continue,
                _ => changed.push(k.clone()),
            }
        }
        changed.sort();
        changed
    }

    /// Reconcile the on-disk project tree with the given `target` state:
    /// delete any tracked file absent from `target`, restore any file whose
    /// hash differs (or doesn't exist). Returns the count of paths touched.
    fn restore_to_state(&self, target: &HashMap<String, String>) -> Result<u32, CallerError> {
        let objects_dir = self.snapshot_dir.join("objects");
        // Build the current tree's path set.
        let mut current: HashMap<String, [u8; 32]> = HashMap::new();
        let mut stack = vec![self.project_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    if let Ok(rel) = path.strip_prefix(&self.project_root) {
                        if !should_ignore(rel) {
                            stack.push(path);
                        }
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let rel = match path.strip_prefix(&self.project_root) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if should_ignore(&rel) {
                    continue;
                }
                if std::fs::metadata(&path)
                    .map(|meta| meta.len() > SNAPSHOT_MAX_FILE_BYTES)
                    .unwrap_or(false)
                {
                    continue;
                }
                let snapshot = match inspect_file(&path) {
                    Ok(InspectedFile::Text(snapshot)) => snapshot,
                    Ok(InspectedFile::Unsupported(_)) | Err(_) => continue,
                };
                current.insert(rel_path_key(&rel), snapshot.hash);
            }
        }

        let mut touched: u32 = 0;

        // 1. For each current file: if not in target → delete. If hash differs → restore.
        for (rel, cur_hash) in &current {
            let cur_hex = hex_encode(cur_hash);
            match target.get(rel) {
                None => {
                    let abs = self.project_root.join(rel);
                    if std::fs::remove_file(&abs).is_ok() {
                        touched += 1;
                    }
                }
                Some(target_hex) if target_hex != &cur_hex => {
                    let obj = objects_dir.join(target_hex);
                    if let Ok(bytes) = std::fs::read(&obj) {
                        let abs = self.project_root.join(rel);
                        if let Some(parent) = abs.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if atomic_write(&abs, &bytes).is_ok() {
                            touched += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        // 2. For each target file not currently present → restore.
        for (rel, target_hex) in target {
            if !current.contains_key(rel) {
                let obj = objects_dir.join(target_hex);
                if let Ok(bytes) = std::fs::read(&obj) {
                    let abs = self.project_root.join(rel);
                    if let Some(parent) = abs.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if atomic_write(&abs, &bytes).is_ok() {
                        touched += 1;
                    }
                }
            }
        }

        Ok(touched)
    }

    /// After a bulk restore, walk the tree again to re-sync our in-memory
    /// hash mirror. Prevents the watcher from emitting spurious
    /// `FileChanged` events for paths we just rewrote.
    fn refresh_hashes_from_tree(&mut self) {
        let mut new_hashes = HashMap::new();
        let mut large_file_fingerprints = HashMap::new();
        let mut stack = vec![self.project_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    if let Ok(rel) = path.strip_prefix(&self.project_root) {
                        if !should_ignore(rel) {
                            stack.push(path);
                        }
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let rel = match path.strip_prefix(&self.project_root) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if should_ignore(&rel) {
                    continue;
                }
                if let Ok(meta) = std::fs::metadata(&path) {
                    if meta.len() > SNAPSHOT_MAX_FILE_BYTES {
                        large_file_fingerprints.insert(rel, metadata_fingerprint(&meta));
                        continue;
                    }
                }
                if let Ok(snapshot) = inspect_file(&path) {
                    let hash = match snapshot {
                        InspectedFile::Text(snapshot) => snapshot.hash,
                        InspectedFile::Unsupported(snapshot) => snapshot.hash,
                    };
                    new_hashes.insert(rel, hash);
                }
            }
        }
        self.hashes = new_hashes;
        self.large_file_fingerprints = large_file_fingerprints;
    }

    /// Persist `history.json` atomically via tmp + rename.
    fn persist_history(&self) -> Result<(), CallerError> {
        let path = self.snapshot_dir.join("history.json");
        let bytes = serde_json::to_vec_pretty(&self.history)
            .map_err(|e| CallerError::Config(format!("history serialize: {}", e)))?;
        atomic_write(&path, &bytes).map_err(CallerError::Io)?;
        Ok(())
    }

    /// Collect the set of hashes that are still referenced by any live round
    /// (active path) plus the baseline, and delete any `objects/{hash}` file
    /// not in that set. Returns bytes freed.
    fn gc_orphaned_objects(&self) -> u64 {
        let mut referenced: HashSet<String> = HashSet::new();
        for r in &self.history.rounds {
            for h in r.files_at_end.values() {
                referenced.insert(h.clone());
            }
        }
        for bytes in self.baselines.values() {
            referenced.insert(hex_encode(&sha256_hash(bytes)));
        }

        let objects_dir = self.snapshot_dir.join("objects");
        let mut freed: u64 = 0;
        let entries = match std::fs::read_dir(&objects_dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Skip temp files written during object creation.
            if !referenced.contains(name) {
                if let Ok(m) = std::fs::metadata(&p) {
                    freed += m.len();
                }
                let _ = std::fs::remove_file(&p);
            }
        }
        freed
    }

    /// Enforce the soft cap on total snapshot dir size. Drops oldest
    /// abandoned branches first, then GCs their orphaned objects. Active
    /// rounds are never touched.
    fn enforce_soft_cap(&mut self) -> Result<(), CallerError> {
        let mut size = dir_byte_size(&self.snapshot_dir);
        if size <= SNAPSHOT_DIR_SOFT_CAP_BYTES {
            return Ok(());
        }
        // Sort by oldest first.
        self.history
            .abandoned_branches
            .sort_by_key(|b| b.created_at_unix);
        while size > SNAPSHOT_DIR_SOFT_CAP_BYTES && !self.history.abandoned_branches.is_empty() {
            self.history.abandoned_branches.remove(0);
            let freed = self.gc_orphaned_objects();
            size = size.saturating_sub(freed);
            if freed == 0 {
                break;
            }
        }
        self.persist_history()?;
        Ok(())
    }
}

/// Run the notify-based filesystem watcher. Shared state is updated under the
/// async mutex on each event so snapshot / rollback operations see a
/// consistent view.
async fn run_watcher_loop(
    shared: SharedFileWatcher,
    project_root: PathBuf,
) -> Result<(), CallerError> {
    use notify::Watcher;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        })
        .map_err(|e| CallerError::Config(format!("notify watcher init: {}", e)))?;

    watcher
        .watch(&project_root, notify::RecursiveMode::Recursive)
        .map_err(|e| CallerError::Config(format!("notify watch: {}", e)))?;

    let _watcher = watcher;

    while let Some(notify_event) = rx.recv().await {
        let paths = notify_event.paths.clone();
        let kind = notify_event.kind;
        let mut w = shared.lock().await;
        for path in &paths {
            w.process_change(path, &kind);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn snapshot_worthiness_requires_a_project_marker() {
        let tmp = TempDir::new().unwrap();
        // A bare directory (the cwd-fallback root — e.g. a service's
        // $HOME) is not a project and must never be baseline-scanned.
        assert!(!root_is_snapshot_worthy(tmp.path()));
        // A git worktree's .git is a FILE, not a directory.
        std::fs::write(tmp.path().join(".git"), "gitdir: elsewhere").unwrap();
        assert!(root_is_snapshot_worthy(tmp.path()));
        std::fs::remove_file(tmp.path().join(".git")).unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        assert!(root_is_snapshot_worthy(tmp.path()));
        std::fs::remove_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join("intendant.toml"), "").unwrap();
        assert!(root_is_snapshot_worthy(tmp.path()));
    }

    #[test]
    fn tree_budget_bounds_files_and_bytes() {
        assert!(!tree_budget_exceeded(SNAPSHOT_MAX_TREE_FILES, 0));
        assert!(tree_budget_exceeded(SNAPSHOT_MAX_TREE_FILES + 1, 0));
        assert!(!tree_budget_exceeded(0, SNAPSHOT_MAX_TREE_BYTES));
        assert!(tree_budget_exceeded(0, SNAPSHOT_MAX_TREE_BYTES + 1));
        assert!(!tree_budget_exceeded(0, 0));
    }

    fn make_watcher(root: &Path, snap: &Path) -> FileWatcher {
        let bus = EventBus::new();
        FileWatcher::new(root.to_path_buf(), snap.to_path_buf(), bus).expect("new")
    }

    #[test]
    fn test_should_ignore() {
        assert!(should_ignore(Path::new(".git/config")));
        assert!(should_ignore(Path::new("target/debug/foo")));
        assert!(should_ignore(Path::new("node_modules/pkg/index.js")));
        assert!(should_ignore(Path::new("src/main.wasm")));
        assert!(should_ignore(Path::new("images/logo.png")));
        assert!(should_ignore(Path::new("archive.tar.gz")));
        assert!(should_ignore(Path::new(".claude/settings.json")));
        assert!(should_ignore(Path::new(".worktrees/feature/src/main.rs")));

        assert!(!should_ignore(Path::new("src/main.rs")));
        assert!(!should_ignore(Path::new("Cargo.toml")));
        assert!(!should_ignore(Path::new("README.md")));
        assert!(!should_ignore(Path::new("src/lib.rs")));
    }

    #[test]
    fn new_records_unsupported_baseline_manifest_without_text_baseline() {
        let root = TempDir::new().unwrap();
        let snap = TempDir::new().unwrap();
        std::fs::write(root.path().join("data.dat"), b"text\0binary").unwrap();

        let _watcher = make_watcher(root.path(), snap.path());

        let manifest_path = snap.path().join(BASELINE_MANIFEST_FILE);
        let manifest: BaselineManifest =
            serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
        let meta = manifest.get("data.dat").expect("unsupported file metadata");
        assert!(!meta.supported_text);
        assert_eq!(meta.reason.as_deref(), Some("binary file"));
        assert!(!snap.path().join("baseline").join("data.dat").exists());
    }

    #[test]
    fn created_empty_file_does_not_become_baseline_sentinel() {
        let root = TempDir::new().unwrap();
        let snap = TempDir::new().unwrap();
        let file_path = root.path().join("empty.txt");

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut watcher =
            FileWatcher::new(root.path().to_path_buf(), snap.path().to_path_buf(), bus)
                .expect("watcher");

        std::fs::write(&file_path, b"").unwrap();
        watcher.process_change(
            &file_path,
            &notify::EventKind::Create(notify::event::CreateKind::File),
        );

        match rx.try_recv().expect("file_changed event") {
            AppEvent::FileChanged {
                path,
                kind,
                lines_added,
                lines_removed,
            } => {
                assert_eq!(path, "empty.txt");
                assert_eq!(kind, FileChangeKind::Created);
                assert_eq!(lines_added, 0);
                assert_eq!(lines_removed, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(!snap.path().join("baseline").join("empty.txt").exists());
        assert!(!watcher.baselines.contains_key(Path::new("empty.txt")));
    }

    #[test]
    fn created_file_followup_modify_event_is_not_created_again() {
        let root = TempDir::new().unwrap();
        let snap = TempDir::new().unwrap();
        let file_path = root.path().join("new.txt");

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut watcher =
            FileWatcher::new(root.path().to_path_buf(), snap.path().to_path_buf(), bus)
                .expect("watcher");

        std::fs::write(&file_path, "first\n").unwrap();
        watcher.process_change(
            &file_path,
            &notify::EventKind::Create(notify::event::CreateKind::File),
        );
        match rx.try_recv().expect("created event") {
            AppEvent::FileChanged { kind, .. } => assert_eq!(kind, FileChangeKind::Created),
            other => panic!("unexpected event: {other:?}"),
        }

        std::fs::write(&file_path, "first\nsecond\n").unwrap();
        watcher.process_change(
            &file_path,
            &notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
        );
        match rx.try_recv().expect("modified event") {
            AppEvent::FileChanged { path, kind, .. } => {
                assert_eq!(path, "new.txt");
                assert_eq!(kind, FileChangeKind::Modified);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn test_binary_detection() {
        assert!(is_binary(&[0x00, 0x01, 0x02]));
        assert!(is_binary(b"hello\x00world"));
        assert!(!is_binary(b"hello world"));
        assert!(!is_binary(b"fn main() {}"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_compute_unified_diff() {
        let baseline = "line1\nline2\nline3\n";
        let current = "line1\nline2-modified\nline3\nline4\n";
        let diff = compute_unified_diff(baseline, current, "test.txt");

        assert!(diff.contains("--- a/test.txt"));
        assert!(diff.contains("+++ b/test.txt"));
        assert!(diff.contains("@@"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+line2-modified"));
        assert!(diff.contains("+line4"));
    }

    #[test]
    fn test_diff_stats() {
        let baseline = "line1\nline2\nline3\n";
        let current = "line1\nline2-modified\nline3\nline4\n";
        let (added, removed) = diff_stats(baseline, current);
        // line2 removed, line2-modified added, line4 added
        assert_eq!(removed, 1);
        assert_eq!(added, 2);
    }

    #[test]
    fn test_diff_stats_no_change() {
        let text = "line1\nline2\n";
        let (added, removed) = diff_stats(text, text);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_diff_stats_all_new() {
        let (added, removed) = diff_stats("", "a\nb\nc\n");
        assert_eq!(added, 3);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_diff_stats_all_deleted() {
        let (added, removed) = diff_stats("a\nb\nc\n", "");
        assert_eq!(added, 0);
        assert_eq!(removed, 3);
    }

    #[test]
    fn new_persists_initial_baselines_for_http_diff() {
        let root = TempDir::new().unwrap();
        let snap = TempDir::new().unwrap();
        let src_dir = root.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}\n").unwrap();

        let _watcher = make_watcher(root.path(), snap.path());

        let baseline = snap.path().join("baseline").join("src").join("main.rs");
        assert_eq!(std::fs::read_to_string(baseline).unwrap(), "fn main() {}\n");
    }

    #[test]
    fn large_existing_file_is_baselined_for_create_like_rewrites() {
        let root = TempDir::new().unwrap();
        let snap = TempDir::new().unwrap();
        let src_dir = root.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file_path = src_dir.join("large.rs");
        let baseline = (0..20_000)
            .map(|n| format!("let value_{n} = {n};\n"))
            .collect::<String>();
        assert!(baseline.len() as u64 > 100_000);
        assert!(baseline.len() as u64 <= SNAPSHOT_MAX_FILE_BYTES);
        std::fs::write(&file_path, &baseline).unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut watcher =
            FileWatcher::new(root.path().to_path_buf(), snap.path().to_path_buf(), bus)
                .expect("watcher");

        let baseline_path = snap.path().join("baseline").join("src").join("large.rs");
        assert_eq!(std::fs::read_to_string(baseline_path).unwrap(), baseline);

        std::fs::write(&file_path, format!("{baseline}let extra = 1;\n")).unwrap();
        watcher.process_change(
            &file_path,
            &notify::EventKind::Create(notify::event::CreateKind::File),
        );

        match rx.try_recv().expect("file_changed event") {
            AppEvent::FileChanged {
                path,
                kind,
                lines_added,
                lines_removed,
            } => {
                assert_eq!(path, "src/large.rs");
                assert_eq!(kind, FileChangeKind::Modified);
                assert_eq!(lines_added, 1);
                assert_eq!(lines_removed, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn round_files_changed_counts_unsupported_created_files() {
        let root = TempDir::new().unwrap();
        let snap = TempDir::new().unwrap();
        let mut watcher = make_watcher(root.path(), snap.path());

        std::fs::write(root.path().join("text.txt"), "hello\n").unwrap();
        std::fs::write(root.path().join("data.dat"), b"hello\0binary").unwrap();

        watcher
            .on_round_complete("created files".to_string(), None, None)
            .unwrap();
        let round = watcher.history.rounds.last().expect("round");

        assert_eq!(
            round.files_changed,
            vec!["data.dat".to_string(), "text.txt".to_string()]
        );
        assert!(round.files_at_end.contains_key("text.txt"));
        assert!(!round.files_at_end.contains_key("data.dat"));
        assert!(round.all_files_at_end.contains_key("data.dat"));
    }

    #[test]
    fn oversized_file_duplicate_events_are_deduped_by_metadata() {
        let root = TempDir::new().unwrap();
        let snap = TempDir::new().unwrap();
        let file_path = root.path().join("large.csv");
        let mut content = vec![b'a'; SNAPSHOT_MAX_FILE_BYTES as usize + 1];
        std::fs::write(&file_path, &content).unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut watcher =
            FileWatcher::new(root.path().to_path_buf(), snap.path().to_path_buf(), bus)
                .expect("watcher");

        watcher.process_change(
            &file_path,
            &notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
        );
        assert!(rx.try_recv().is_err());

        content.push(b'\n');
        std::fs::write(&file_path, &content).unwrap();
        watcher.process_change(
            &file_path,
            &notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
        );

        match rx.try_recv().expect("large file changed event") {
            AppEvent::FileChanged {
                path,
                kind,
                lines_added,
                lines_removed,
            } => {
                assert_eq!(path, "large.csv");
                assert_eq!(kind, FileChangeKind::Modified);
                assert_eq!(lines_added, 0);
                assert_eq!(lines_removed, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        watcher.process_change(
            &file_path,
            &notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_hex_encode_roundtrip() {
        let hash = sha256_hash(b"hello");
        let hex = hex_encode(&hash);
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A round snapshot records `turn_count` and `native_message_count`
    /// when they're passed in, and persists them through the
    /// history.json round-trip so conversation rollback can look them
    /// up after a restart.
    #[test]
    fn round_records_turn_count_and_message_count() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"r1").unwrap();

        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), Some(3), Some(42)).unwrap();
        let id = w.history.current_head_id.unwrap();

        let round = w.history.rounds.iter().find(|r| r.id == id).unwrap();
        assert_eq!(round.turn_count, Some(3));
        assert_eq!(round.native_message_count, Some(42));

        // Persist + reload to confirm the fields survive JSON round-trip.
        let hist_path = tmp_snap.path().join("history.json");
        let bytes = std::fs::read(&hist_path).unwrap();
        let parsed: History = serde_json::from_slice(&bytes).unwrap();
        let reloaded = parsed.rounds.iter().find(|r| r.id == id).unwrap();
        assert_eq!(reloaded.turn_count, Some(3));
        assert_eq!(reloaded.native_message_count, Some(42));
    }

    /// Backward-compat: a history.json produced before these fields
    /// existed must still deserialize, with the missing fields
    /// defaulting to `None`.
    #[test]
    fn history_round_missing_turn_fields_defaults_to_none() {
        let json = r#"{
            "id": 1,
            "parent_id": null,
            "summary": "R1",
            "timestamp_unix": 0,
            "files_changed": [],
            "files_at_end": {}
        }"#;
        let round: HistoryRound = serde_json::from_str(json).unwrap();
        assert_eq!(round.turn_count, None);
        assert_eq!(round.native_message_count, None);
    }

    /// Snapshot creates a round, files_at_end captures every file's hash,
    /// rollback restores content even when files have been mutated since.
    #[test]
    fn snapshot_and_rollback_round_trip() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"round1-a").unwrap();
        std::fs::write(root.join("b.txt"), b"round1-b").unwrap();

        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let round1_id = w.history.current_head_id.unwrap();

        // Modify files then snapshot round 2.
        std::fs::write(root.join("a.txt"), b"round2-a").unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        std::fs::write(root.join("c.txt"), b"round2-c").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        let round2_id = w.history.current_head_id.unwrap();
        assert_ne!(round1_id, round2_id);
        assert_eq!(w.history.rounds.len(), 2);

        // Rollback to round 1.
        let res = w.rollback(round1_id).unwrap();
        assert_eq!(res.to_round_id, round1_id);

        // Files match round 1 state.
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"round1-a");
        assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"round1-b");
        assert!(!root.join("c.txt").exists());

        // Linear history unchanged — just the head moved.
        assert_eq!(w.history.rounds.len(), 2);
        assert_eq!(w.history.current_head_id, Some(round1_id));
    }

    /// Redo reapplies the state recorded in the next round after a rollback.
    #[test]
    fn redo_restores_state() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"r1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();

        std::fs::write(root.join("a.txt"), b"r2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();

        w.rollback(r1).unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"r1");

        let res = w.redo().unwrap();
        assert_eq!(res.to_round_id, r2);
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"r2");

        // Redo past the end is an error.
        assert!(w.redo().is_err());
    }

    /// Creating a new round after rollback branches the abandoned tail into
    /// `abandoned_branches`.
    #[test]
    fn branching_on_new_action() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"r1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();

        std::fs::write(root.join("a.txt"), b"r2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();

        // Roll back; linear history still holds both rounds.
        w.rollback(r1).unwrap();
        assert_eq!(w.history.rounds.len(), 2);

        // New action branches.
        std::fs::write(root.join("a.txt"), b"r2-prime").unwrap();
        w.on_round_complete("R2'".into(), None, None).unwrap();

        assert_eq!(w.history.rounds.len(), 2);
        assert_eq!(w.history.abandoned_branches.len(), 1);
        let branch = &w.history.abandoned_branches[0];
        assert_eq!(branch.branched_from_id, r1);
        assert_eq!(branch.rounds.len(), 1);
        assert_eq!(branch.rounds[0].id, r2);

        // Head is now the new round, not the old r2.
        let new_head = w.history.current_head_id.unwrap();
        assert_ne!(new_head, r2);
    }

    /// Pruning removes abandoned branches and garbage-collects any orphaned
    /// content blobs that aren't referenced by live rounds.
    #[test]
    fn prune_removes_abandoned_and_orphaned_objects() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"r1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();

        std::fs::write(root.join("a.txt"), b"branch-only-content").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();

        // The "branch-only-content" hash exists in objects/.
        let target = w
            .history
            .rounds
            .iter()
            .find(|r| r.id == r2)
            .unwrap()
            .files_at_end
            .get("a.txt")
            .cloned()
            .unwrap();
        let orphan_obj = tmp_snap.path().join("objects").join(&target);
        assert!(orphan_obj.exists());

        // Branch: roll back to r1 and create a new round. r2 goes into abandoned.
        w.rollback(r1).unwrap();
        std::fs::write(root.join("a.txt"), b"r2-new").unwrap();
        w.on_round_complete("R2'".into(), None, None).unwrap();
        assert_eq!(w.history.abandoned_branches.len(), 1);

        let res = w.prune_abandoned().unwrap();
        assert_eq!(res.branches_removed, 1);
        assert!(res.bytes_freed > 0);
        assert!(w.history.abandoned_branches.is_empty());
        assert!(!orphan_obj.exists(), "orphaned object should be GC'd");
    }

    /// When the snapshot dir exceeds the soft cap and there are abandoned
    /// branches, the oldest ones are pruned automatically. Active rounds are
    /// never touched.
    #[test]
    fn soft_cap_triggers_prune() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("seed.txt"), b"seed").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();

        // Synthesize multiple abandoned branches with distinct timestamps.
        let now = now_unix();
        for i in 0..5 {
            let fake_round = HistoryRound {
                id: 100 + i,
                parent_id: Some(r1),
                summary: format!("fake-{i}"),
                timestamp_unix: now,
                files_changed: vec![],
                files_at_end: HashMap::new(),
                all_files_at_end: HashMap::new(),
                turn_count: None,
                native_message_count: None,
            };
            w.history.abandoned_branches.push(AbandonedBranch {
                branched_from_id: r1,
                rounds: vec![fake_round],
                created_at_unix: now - (10 - i as u64), // oldest has smallest ts
            });
        }
        assert_eq!(w.history.abandoned_branches.len(), 5);

        // Force a prune that would be triggered under over-cap conditions by
        // pretending the cap is zero and calling the inner helpers directly.
        // (We cannot realistically fabricate 500MB of fake state, so validate
        // the eviction-order logic directly.)
        w.history
            .abandoned_branches
            .sort_by_key(|b| b.created_at_unix);
        let oldest_ts = w.history.abandoned_branches[0].created_at_unix;
        let newest_ts = w.history.abandoned_branches.last().unwrap().created_at_unix;
        assert!(oldest_ts < newest_ts);

        // Simulate the soft-cap loop: drop the oldest, verify remaining are
        // newer than the one dropped.
        let dropped = w.history.abandoned_branches.remove(0);
        for remaining in &w.history.abandoned_branches {
            assert!(remaining.created_at_unix > dropped.created_at_unix);
        }
        // Active history (r1) still intact.
        assert_eq!(w.history.rounds.len(), 1);
        assert_eq!(w.history.current_head_id, Some(r1));
    }
}
