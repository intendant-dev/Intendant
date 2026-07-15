//! Live filesystem watcher: observes file changes in the project directory,
//! stores copy-on-write baseline snapshots, and emits `AppEvent::FileChanged`
//! events. Works for all agent types (native, Codex, Claude Code, Gemini CLI)
//! by watching the filesystem directly rather than relying on git.
//!
//! Also provides per-round content-addressed snapshots for rollback / redo /
//! branching. On each [`AppEvent::RoundComplete`], a new [`HistoryRound`] is
//! recorded: supported, non-ignored text files under the size cap are stored
//! in `objects/` for restore, while additional inspected non-restorable files
//! contribute hashes to the display/count mirror. Rollback moves
//! `current_head_id` back without truncating the linear history (so redo is
//! available). A new action after rollback branches off the abandoned path and
//! stores it in
//! `abandoned_branches` for later pruning.
//!
//! Cost model: round scans are fingerprint-cached — every file is stat'd,
//! only files whose (size, mtime) moved are re-read and re-hashed — so
//! per-round work is proportional to actual changes. Durable state is split
//! between a slim `history.json` index (round scalars + changed paths,
//! format 2) and one `rounds/round_{id}/manifest.json` per round holding
//! that round's full path→hash maps; a no-op round writes a tiny
//! `maps_from_round` backreference instead of duplicating maps. Only the
//! head round's maps stay in memory.

use crate::error::CallerError;
use crate::event::{AppEvent, EventBus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
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

/// One recorded round in the session history. Captures the restorable project
/// state at the end of the round (supported, non-ignored text files under the
/// size cap) plus a broader display/count hash mirror and the subset of paths
/// that differ from the previous round.
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
    /// Full restorable state at the end of this round: supported,
    /// non-ignored text files under `SNAPSHOT_MAX_FILE_BYTES`, path → sha256
    /// hex. Rollback restores exactly this map.
    ///
    /// Populated when this struct is a per-round manifest
    /// (`rounds/round_{id}/manifest.json`) with inline maps. The in-memory
    /// `History` and the persisted `history.json` index keep it empty — the
    /// manifests are the durable source; see [`FileWatcher`]'s head-maps
    /// cache and `resolved_round_maps`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub files_at_end: HashMap<String, String>,
    /// Display-state mirror that also includes non-restorable tracked files
    /// that were inspected but not stored as text blobs. Rollback still uses
    /// `files_at_end`; this lets timeline counts match what the Changes tab
    /// reports. Same manifest-only population as `files_at_end`.
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
    /// When this round's tree state is identical to an earlier round's
    /// (a no-op round — e.g. a `RoundComplete` that changed no files), the
    /// maps are not duplicated: this names the round whose manifest holds
    /// them inline. Backreferences are written depth-1 (they point at the
    /// nearest round with inline maps, never at another backreference).
    /// `None` for rounds with inline maps and for legacy manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maps_from_round: Option<u64>,
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

/// `(restorable, display_mirror)` maps from a snapshot object scan: relative
/// path → object hash.
type SnapshotObjectMaps = (HashMap<String, String>, HashMap<String, String>);

#[derive(Debug, Clone)]
pub(crate) struct TextFileSnapshot {
    /// UTF-8 content. The raw bytes are exactly `text.as_bytes()` — the
    /// snapshot deliberately holds one copy, not a bytes+text pair.
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
    // Same marker notion as "does the daemon have a project at all" —
    // one definition, owned by project.rs.
    crate::project::root_has_project_marker(root)
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

    let text = match String::from_utf8(bytes) {
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

/// Compute the list of paths whose content in `current` differs from the
/// parent round's display mirror (or restorable map for legacy rounds
/// recorded before the mirror existed), or from the baseline when there is
/// no resolvable parent round.
fn compute_files_changed(
    parent: Option<&ResolvedRoundMaps>,
    current: &HashMap<String, String>,
    baseline_manifest: &BaselineManifest,
) -> Vec<String> {
    let mut changed = Vec::new();
    let baseline_hex: HashMap<String, String>;
    let prev: &HashMap<String, String> = match parent {
        Some(parent) => {
            if parent.all_files_at_end.is_empty() {
                &parent.files_at_end
            } else {
                &parent.all_files_at_end
            }
        }
        None => {
            // First round (or unresolvable parent): compare against the
            // baseline manifest, which records a hash for every inspected
            // file at session start.
            baseline_hex = baseline_manifest
                .iter()
                .map(|(path, meta)| (path.clone(), meta.hash.clone()))
                .collect();
            &baseline_hex
        }
    };
    let mut keys: HashSet<&String> = prev.keys().collect();
    keys.extend(current.keys());
    for k in keys {
        match (prev.get(k), current.get(k)) {
            (Some(a), Some(b)) if a == b => continue,
            _ => changed.push(k.clone()),
        }
    }
    changed.sort();
    changed
}

/// Persist a named tempfile at `dest_path`, using an atomic rename when the
/// tempfile is already on the destination filesystem and re-staging in the
/// destination directory when it is not.
pub(crate) fn persist_tempfile(
    temp_file: tempfile::NamedTempFile,
    dest_path: &Path,
) -> io::Result<()> {
    match temp_file.persist(dest_path) {
        Ok(_) => Ok(()),
        Err(err) if err.error.kind() == io::ErrorKind::CrossesDevices => {
            copy_tempfile_across_filesystems(err.file, dest_path)
        }
        Err(err) => Err(err.error),
    }
}

fn copy_tempfile_across_filesystems(
    temp_file: tempfile::NamedTempFile,
    dest_path: &Path,
) -> io::Result<()> {
    let dest_dir = path_parent_or_cwd(dest_path);
    let mut source = temp_file.reopen()?;
    let mut dest_temp = tempfile::Builder::new()
        .prefix(".intendant-write-")
        .suffix(".tmp")
        .tempfile_in(dest_dir)?;
    io::copy(&mut source, &mut dest_temp)?;
    dest_temp.flush()?;
    dest_temp.as_file_mut().sync_all()?;
    dest_temp.persist(dest_path).map_err(|err| err.error)?;
    Ok(())
}

fn path_parent_or_cwd(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Write `content` to `path` through a unique tempfile in the destination
/// directory, sync it, then atomically replace the destination.
pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path_parent_or_cwd(path);
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".intendant-write-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    tmp.write_all(content)?;
    tmp.as_file_mut().sync_all()?;
    persist_tempfile(tmp, path)?;
    Ok(())
}

/// Format tag stamped into `history.json` since it became a slim index
/// (round scalars + `files_changed` only; the per-round path→hash maps live
/// in `rounds/round_{id}/manifest.json`). Older binaries fail to parse a
/// round without `files_at_end` and fall back to an empty history — a safe
/// "no rollback offered" downgrade instead of restoring from empty maps.
const HISTORY_INDEX_FORMAT: u64 = 2;

/// Path of the on-disk manifest carrying one round's full snapshot record.
fn round_manifest_path(snapshot_dir: &Path, round_id: u64) -> PathBuf {
    snapshot_dir
        .join("rounds")
        .join(format!("round_{}", round_id))
        .join("manifest.json")
}

/// Load `history.json`, migrating legacy full-fat files (per-round maps
/// inline) to per-round manifests so the maps stay restorable after the
/// in-memory copy goes slim. Returns a `History` whose rounds carry scalars
/// and `files_changed` only.
fn load_history_from_disk(snapshot_dir: &Path) -> History {
    let history_path = snapshot_dir.join("history.json");
    let Ok(bytes) = std::fs::read(&history_path) else {
        return History::default();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return History::default();
    };
    let format = value.get("format").and_then(|v| v.as_u64()).unwrap_or(0);
    let Ok(mut history) = serde_json::from_value::<History>(value) else {
        return History::default();
    };
    if format < HISTORY_INDEX_FORMAT {
        migrate_legacy_history_maps(&mut history, snapshot_dir);
    } else {
        // Defensive: a format-2 index never carries maps, but strip any that
        // slipped in so the in-memory invariant (slim rounds) always holds.
        strip_round_maps(&mut history);
    }
    history
}

/// Write a per-round manifest for every legacy round that has inline maps
/// but no manifest on disk (manifests have been written since the feature
/// shipped, so this normally touches nothing), then drop the inline maps
/// from memory. Rounds inside abandoned branches migrate too.
fn migrate_legacy_history_maps(history: &mut History, snapshot_dir: &Path) {
    let migrate_round = |round: &mut HistoryRound| {
        let manifest_path = round_manifest_path(snapshot_dir, round.id);
        if !manifest_path.exists() {
            // Write even when the maps are empty: an explicit "empty tree"
            // record keeps restore-to-empty semantics distinct from a
            // missing manifest (which rollback refuses).
            if let Ok(bytes) = serde_json::to_vec_pretty(&round) {
                let _ = atomic_write(&manifest_path, &bytes);
            }
        }
        round.files_at_end = HashMap::new();
        round.all_files_at_end = HashMap::new();
    };
    for round in &mut history.rounds {
        migrate_round(round);
    }
    for branch in &mut history.abandoned_branches {
        for round in &mut branch.rounds {
            migrate_round(round);
        }
    }
}

fn strip_round_maps(history: &mut History) {
    for round in &mut history.rounds {
        round.files_at_end = HashMap::new();
        round.all_files_at_end = HashMap::new();
    }
    for branch in &mut history.abandoned_branches {
        for round in &mut branch.rounds {
            round.files_at_end = HashMap::new();
            round.all_files_at_end = HashMap::new();
        }
    }
}

/// Decode a 64-char lowercase/uppercase hex sha256 into raw bytes.
fn hex_decode_hash(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in bytes.chunks_exact(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
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

/// One registry row: (project_root, snapshot_dir, watcher).
type LiveWatcherEntry = (PathBuf, PathBuf, std::sync::Weak<AsyncMutex<FileWatcher>>);

/// Registry of live watchers, keyed by (project_root, snapshot_dir). A
/// daemon runs at most one, but the key keeps concurrent in-process tests
/// isolated. The gateway's changes endpoint uses it to serve change lists
/// from watcher state instead of re-reading the whole project per request;
/// entries are `Weak`, so a dropped watcher unregisters itself.
static LIVE_WATCHERS: std::sync::OnceLock<std::sync::Mutex<Vec<LiveWatcherEntry>>> =
    std::sync::OnceLock::new();

fn live_watchers() -> &'static std::sync::Mutex<Vec<LiveWatcherEntry>> {
    LIVE_WATCHERS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn register_live_watcher(project_root: PathBuf, snapshot_dir: PathBuf, shared: &SharedFileWatcher) {
    let Ok(mut registry) = live_watchers().lock() else {
        return;
    };
    registry.retain(|(_, _, weak)| weak.strong_count() > 0);
    registry.push((project_root, snapshot_dir, Arc::downgrade(shared)));
}

/// Compare paths tolerating symlinked spellings (`/tmp` vs `/private/tmp`
/// on macOS) by canonicalizing when possible.
fn watcher_paths_match(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Look up the live watcher covering exactly this (project_root,
/// snapshot_dir) pair. `None` when no watcher matches — callers fall back
/// to their disk-scan path (external session targets, watcher-less
/// daemons).
pub(crate) fn live_watcher_for(
    project_root: &Path,
    snapshot_dir: &Path,
) -> Option<SharedFileWatcher> {
    let registry = live_watchers().lock().ok()?;
    for (root, snap, weak) in registry.iter() {
        if watcher_paths_match(root, project_root) && watcher_paths_match(snap, snapshot_dir) {
            if let Some(shared) = weak.upgrade() {
                return Some(shared);
            }
        }
    }
    None
}

/// Point-in-time view of the watcher state the changes endpoint needs to
/// compute the changed-key set without touching the project tree: the
/// session-start baseline metadata and the last-known content hash per
/// tracked path.
pub(crate) struct ChangesIndexSnapshot {
    pub(crate) baseline_manifest: BaselineManifest,
    /// Relative path key → lowercase sha256 hex of last-known content.
    pub(crate) current_hashes: HashMap<String, String>,
}

/// Cached result of hashing one tracked file, keyed by its metadata
/// fingerprint. Round scans stat every file (cheap) and only re-read and
/// re-hash the ones whose fingerprint moved, so per-round work is
/// proportional to actual changes instead of the whole tree.
///
/// The fingerprint is always taken from a stat performed *before* the
/// content read it describes, so a write racing the read can only cause a
/// spurious cache miss (extra work), never a stale hit.
#[derive(Debug, Clone)]
struct ScanCacheEntry {
    fingerprint: FileFingerprint,
    hash: [u8; 32],
    hash_hex: String,
    /// True when the file was a supported text file (stored in `objects/`
    /// and restorable); false for inspected-but-unsupported files that only
    /// feed the display mirror.
    restorable: bool,
}

/// The maps of the round `current_head_id` points at, kept in memory so the
/// next round's diff does not have to re-read a manifest. All other rounds'
/// maps live only on disk in `rounds/round_{id}/manifest.json`.
#[derive(Debug, Clone)]
struct HeadMapsCache {
    round_id: u64,
    /// The round whose manifest holds these maps inline (`round_id` itself
    /// for changed rounds; an earlier round for no-op rounds recorded via
    /// `maps_from_round` backreferences).
    source_round_id: u64,
    files_at_end: HashMap<String, String>,
    all_files_at_end: HashMap<String, String>,
}

/// Maps of one round resolved from the head cache or its on-disk manifest.
struct ResolvedRoundMaps {
    /// Round whose manifest carries the maps inline.
    source_round_id: u64,
    files_at_end: HashMap<String, String>,
    all_files_at_end: HashMap<String, String>,
}

pub struct FileWatcher {
    project_root: PathBuf,
    snapshot_dir: PathBuf,
    bus: EventBus,
    /// Metadata for every non-ignored file that existed at session start.
    /// Baseline *content* is not held in memory: the `baseline/` shadow copy
    /// written at startup is read on demand (per changed file), so RSS no
    /// longer scales with project size for the daemon's lifetime.
    baseline_manifest: BaselineManifest,
    /// SHA-256 hashes of last-known content, for change deduplication.
    hashes: HashMap<PathBuf, [u8; 32]>,
    /// Last-known metadata fingerprints for oversized files. These files are
    /// not snapshotted, and duplicate notify events can otherwise re-hash tens
    /// of megabytes repeatedly while an editor writes temp files.
    large_file_fingerprints: HashMap<PathBuf, FileFingerprint>,
    /// Per-file fingerprint → hash cache for round scans and post-restore
    /// re-syncs. Rebuilt on every walk so deleted paths fall out.
    round_scan_cache: HashMap<PathBuf, ScanCacheEntry>,
    /// Maps of the current head round (see [`HeadMapsCache`]).
    head_maps: Option<HeadMapsCache>,
    /// Running estimate of bytes under `snapshot_dir`, maintained from the
    /// writes/GCs this watcher performs. The soft-cap check walks the real
    /// tree only when the estimate crosses the cap.
    snapshot_dir_size_estimate: u64,
    /// Size of the last persisted `history.json`, so rewrites adjust the
    /// estimate by the delta instead of double-counting.
    last_history_index_bytes: u64,
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
                        std::fs::write(&baseline_path, snapshot.text.as_bytes()).map_err(|e| {
                            CallerError::Config(format!(
                                "write baseline {}: {}",
                                baseline_path.display(),
                                e
                            ))
                        })?;
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

        // Load history.json if it exists (session resume / restart). Legacy
        // full-fat files (per-round maps inline) are migrated to per-round
        // manifests + a slim index on the next persist.
        let history = load_history_from_disk(&snapshot_dir);

        // One boot-time walk seeds the size estimate the soft-cap check
        // maintains incrementally afterwards (it used to re-walk per round).
        let snapshot_dir_size_estimate = dir_byte_size(&snapshot_dir);
        let last_history_index_bytes = std::fs::metadata(snapshot_dir.join("history.json"))
            .map(|meta| meta.len())
            .unwrap_or(0);

        Ok(Self {
            project_root,
            snapshot_dir,
            bus,
            baseline_manifest,
            hashes,
            large_file_fingerprints,
            round_scan_cache: HashMap::new(),
            head_maps: None,
            snapshot_dir_size_estimate,
            last_history_index_bytes,
            history,
        })
    }

    /// Read-only accessor for the history state. Callers hold the mutex for
    /// the duration, so callers should clone the result if they need to use
    /// it after releasing the lock. Rounds are slim (scalars +
    /// `files_changed`); the per-round maps live in the round manifests.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Snapshot the state the changes endpoint needs to compute the
    /// changed-key set without walking the project tree. Cheap relative to
    /// a tree read (two O(files) map clones), taken under the watcher lock.
    pub(crate) fn changes_index_snapshot(&self) -> ChangesIndexSnapshot {
        ChangesIndexSnapshot {
            baseline_manifest: self.baseline_manifest.clone(),
            current_hashes: self
                .hashes
                .iter()
                .map(|(rel, hash)| (rel_path_key(rel), hex_encode(hash)))
                .collect(),
        }
    }

    /// Wrap `self` in an async-mutex-backed shared handle and spawn the
    /// filesystem watcher loop + round-complete listener. Returns the handle
    /// plus the two join handles so callers can keep them alive.
    pub fn start_shared(self) -> (SharedFileWatcher, JoinHandle<()>, JoinHandle<()>) {
        let bus = self.bus.clone();
        let project_root = self.project_root.clone();
        let snapshot_dir = self.snapshot_dir.clone();
        let shared = Arc::new(AsyncMutex::new(self));
        register_live_watcher(project_root.clone(), snapshot_dir, &shared);

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
                        // Deliberately unfiltered by `session_id`: the event
                        // carries one, but nothing on the bus maps a session
                        // to its working root, so rounds from sessions in
                        // other roots (worktrees, external backends) also
                        // land here — a known attribution wart. Since the
                        // fingerprint cache + backreference manifests, such
                        // foreign rounds cost a stat-walk and an O(1)
                        // persist, not a tree re-read.
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

    pub(crate) fn process_change(&mut self, abs_path: &Path, kind: &notify::EventKind) {
        // Compute relative path.
        let rel = match abs_path.strip_prefix(&self.project_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => return,
        };

        if should_ignore(&rel) {
            return;
        }

        let rel_key = rel_path_key(&rel);
        let existed_at_baseline = self.baseline_manifest.contains_key(&rel_key);
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
                            if let Some(baseline_str) = self.baseline_text_for(&rel) {
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
                        .baseline_text_for(&rel)
                        .map(|text| text.lines().count() as u32)
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
        // Walk the project and compute file hashes + write any new objects.
        // Fingerprint-cached: unchanged files are stat'd, not re-read.
        let (files_at_end, all_files_at_end) = self.scan_and_store_objects()?;

        // Determine parent + files_changed by diffing against previous round
        // (or baseline if first round).
        let parent_id = self.history.current_head_id;
        let parent_maps = parent_id.and_then(|pid| self.resolved_round_maps(pid));
        let files_changed = compute_files_changed(
            parent_maps.as_ref(),
            &all_files_at_end,
            &self.baseline_manifest,
        );

        // A no-op round (tree identical to the parent's recorded state)
        // records a depth-1 backreference instead of duplicating the maps.
        // Compared directly (not via files_changed) so legacy parents whose
        // display mirror predates `all_files_at_end` never alias.
        let maps_source_id = parent_maps
            .as_ref()
            .filter(|parent| {
                parent.files_at_end == files_at_end && parent.all_files_at_end == all_files_at_end
            })
            .map(|parent| parent.source_round_id);

        // If current head is not the last round, branch: move trailing rounds
        // into abandoned_branches before appending. (Backreference targets
        // are always ancestors of the rounds that reference them, so a
        // drained tail never strands a live round's maps.)
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
        let stub = HistoryRound {
            id,
            parent_id,
            summary,
            timestamp_unix: now_unix(),
            files_changed,
            files_at_end: HashMap::new(),
            all_files_at_end: HashMap::new(),
            turn_count,
            native_message_count,
            maps_from_round: maps_source_id,
        };

        // Write the per-round manifest — the durable home of the maps. A
        // no-op round writes a tiny backreference stub; a changed round
        // inlines its maps.
        let manifest = if maps_source_id.is_some() {
            stub.clone()
        } else {
            HistoryRound {
                files_at_end: files_at_end.clone(),
                all_files_at_end: all_files_at_end.clone(),
                ..stub.clone()
            }
        };
        let manifest_path = round_manifest_path(&self.snapshot_dir, id);
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| CallerError::Config(format!("manifest serialize: {}", e)))?;
        atomic_write(&manifest_path, &manifest_bytes).map_err(CallerError::Io)?;
        self.snapshot_dir_size_estimate = self
            .snapshot_dir_size_estimate
            .saturating_add(manifest_bytes.len() as u64);

        self.history.rounds.push(stub);
        self.history.current_head_id = Some(id);
        self.head_maps = Some(HeadMapsCache {
            round_id: id,
            source_round_id: maps_source_id.unwrap_or(id),
            files_at_end,
            all_files_at_end,
        });

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

        let target_id = self.history.rounds[target_idx].id;
        let from_id = self.history.current_head_id.unwrap_or(target_id);
        let target_maps = self.resolved_round_maps(target_id).ok_or_else(|| {
            CallerError::Config(format!(
                "round {} snapshot manifest is missing or unreadable — cannot restore",
                target_id
            ))
        })?;

        let files_reverted = self.restore_to_state(&target_maps.files_at_end)?;

        // Refresh in-memory hash/baseline mirrors so the watcher doesn't
        // re-emit spurious "modified" events for paths we just rewrote.
        self.refresh_hashes_from_tree();

        self.history.current_head_id = Some(target_id);
        self.head_maps = Some(HeadMapsCache {
            round_id: target_id,
            source_round_id: target_maps.source_round_id,
            files_at_end: target_maps.files_at_end,
            all_files_at_end: target_maps.all_files_at_end,
        });
        self.persist_history()?;

        self.bus.send(AppEvent::RolledBack {
            from_id,
            to_id: target_id,
            files_reverted,
        });

        Ok(RollbackResult {
            to_round_id: target_id,
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

        let next_id = self.history.rounds[pos + 1].id;
        let next_maps = self.resolved_round_maps(next_id).ok_or_else(|| {
            CallerError::Config(format!(
                "round {} snapshot manifest is missing or unreadable — cannot restore",
                next_id
            ))
        })?;
        let files_reverted = self.restore_to_state(&next_maps.files_at_end)?;
        self.refresh_hashes_from_tree();

        self.history.current_head_id = Some(next_id);
        self.head_maps = Some(HeadMapsCache {
            round_id: next_id,
            source_round_id: next_maps.source_round_id,
            files_at_end: next_maps.files_at_end,
            all_files_at_end: next_maps.all_files_at_end,
        });
        self.persist_history()?;

        self.bus.send(AppEvent::Redone { to_id: next_id });

        Ok(RedoResult {
            to_round_id: next_id,
            files_reverted,
        })
    }

    /// Delete all abandoned branches and garbage-collect any orphaned blobs
    /// under `objects/`.
    pub fn prune_abandoned(&mut self) -> Result<PruneResult, CallerError> {
        let branches_removed = self.history.abandoned_branches.len() as u32;
        self.history.abandoned_branches.clear();

        let bytes_freed = self.gc_orphaned_objects();
        self.snapshot_dir_size_estimate =
            self.snapshot_dir_size_estimate.saturating_sub(bytes_freed);

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

    /// Walk the project tree and return `(restorable, display_mirror)`.
    /// Supported text files under the size cap are written to `objects/{hash}`
    /// and appear in both maps; inspected non-restorable files appear only in
    /// the display mirror. Ignored and oversized files are skipped.
    ///
    /// Fingerprint-cached: every file is stat'd, but only files whose
    /// (size, mtime) moved since the last walk are re-read and re-hashed.
    /// A cached restorable entry is only trusted when its object blob still
    /// exists on disk, so every hash recorded in `files_at_end` is
    /// restorable by construction.
    fn scan_and_store_objects(&mut self) -> Result<SnapshotObjectMaps, CallerError> {
        let mut out: HashMap<String, String> = HashMap::new();
        let mut all: HashMap<String, String> = HashMap::new();
        let objects_dir = self.snapshot_dir.join("objects");
        std::fs::create_dir_all(&objects_dir)
            .map_err(|e| CallerError::Config(format!("create objects dir: {}", e)))?;
        let mut next_cache: HashMap<PathBuf, ScanCacheEntry> =
            HashMap::with_capacity(self.round_scan_cache.len());

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
                let meta = match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(_) => continue,
                };
                if meta.len() > SNAPSHOT_MAX_FILE_BYTES {
                    continue;
                }
                // Stat-before-read: this fingerprint describes content no
                // newer than what a subsequent read returns.
                let fingerprint = metadata_fingerprint(&meta);
                if let Some(cached) = self.round_scan_cache.get(&rel) {
                    if cached.fingerprint == fingerprint {
                        let key = rel_path_key(&rel);
                        if cached.restorable {
                            if objects_dir.join(&cached.hash_hex).exists() {
                                all.insert(key.clone(), cached.hash_hex.clone());
                                out.insert(key, cached.hash_hex.clone());
                                next_cache.insert(rel, cached.clone());
                                continue;
                            }
                            // Object missing (fresh objects dir, failed
                            // write): fall through and re-store it.
                        } else {
                            all.insert(key, cached.hash_hex.clone());
                            next_cache.insert(rel, cached.clone());
                            continue;
                        }
                    }
                }
                let snapshot = match inspect_file(&path) {
                    Ok(snapshot) => snapshot,
                    Err(_) => continue,
                };
                match snapshot {
                    InspectedFile::Text(snapshot) => {
                        all.insert(rel_path_key(&rel), snapshot.hash_hex.clone());
                        let obj_path = objects_dir.join(&snapshot.hash_hex);
                        if !obj_path.exists()
                            && atomic_write(&obj_path, snapshot.text.as_bytes()).is_ok()
                        {
                            self.snapshot_dir_size_estimate = self
                                .snapshot_dir_size_estimate
                                .saturating_add(snapshot.size);
                        }
                        next_cache.insert(
                            rel.clone(),
                            ScanCacheEntry {
                                fingerprint,
                                hash: snapshot.hash,
                                hash_hex: snapshot.hash_hex.clone(),
                                restorable: true,
                            },
                        );
                        out.insert(rel_path_key(&rel), snapshot.hash_hex);
                    }
                    InspectedFile::Unsupported(snapshot) => {
                        all.insert(rel_path_key(&rel), snapshot.hash_hex.clone());
                        next_cache.insert(
                            rel,
                            ScanCacheEntry {
                                fingerprint,
                                hash: snapshot.hash,
                                hash_hex: snapshot.hash_hex,
                                restorable: false,
                            },
                        );
                    }
                }
            }
        }
        self.round_scan_cache = next_cache;
        Ok((out, all))
    }

    /// Read one baseline file's text from the on-disk `baseline/` shadow
    /// copy. `None` when the path had no supported-text baseline.
    fn baseline_text_for(&self, rel: &Path) -> Option<String> {
        std::fs::read_to_string(self.snapshot_dir.join("baseline").join(rel)).ok()
    }

    /// Resolve one round's maps: from the head cache when it matches, else
    /// from the round's on-disk manifest, following its (depth-1)
    /// `maps_from_round` backreference. `None` when the round is unknown or
    /// its manifest chain is missing/unreadable.
    fn resolved_round_maps(&self, round_id: u64) -> Option<ResolvedRoundMaps> {
        if let Some(cache) = &self.head_maps {
            if cache.round_id == round_id {
                return Some(ResolvedRoundMaps {
                    source_round_id: cache.source_round_id,
                    files_at_end: cache.files_at_end.clone(),
                    all_files_at_end: cache.all_files_at_end.clone(),
                });
            }
        }
        // Prefer the in-memory stub's backreference (skips one manifest
        // read); fall back to whatever the manifest chain says so maps stay
        // resolvable even if the index was rebuilt from scratch.
        let stub_source = self
            .history
            .rounds
            .iter()
            .find(|r| r.id == round_id)
            .and_then(|r| r.maps_from_round);
        let mut source_id = stub_source.unwrap_or(round_id);
        // Backreferences are written depth-1; the bound is purely defensive.
        for _ in 0..32 {
            let manifest_path = round_manifest_path(&self.snapshot_dir, source_id);
            let bytes = std::fs::read(&manifest_path).ok()?;
            let manifest = serde_json::from_slice::<HistoryRound>(&bytes).ok()?;
            match manifest.maps_from_round {
                Some(next) if next != source_id => source_id = next,
                _ => {
                    return Some(ResolvedRoundMaps {
                        source_round_id: source_id,
                        files_at_end: manifest.files_at_end,
                        all_files_at_end: manifest.all_files_at_end,
                    });
                }
            }
        }
        None
    }

    /// Reconcile the on-disk project tree with the given `target` state:
    /// delete any tracked file absent from `target`, restore any file whose
    /// hash differs (or doesn't exist). Returns the count of paths touched.
    ///
    /// Fingerprint-cached like the round scan: unchanged files contribute
    /// their cached hash from a stat instead of a full read. After each
    /// restore/delete the cache is updated so the follow-up
    /// [`Self::refresh_hashes_from_tree`] walk is also mostly stats.
    fn restore_to_state(&mut self, target: &HashMap<String, String>) -> Result<u32, CallerError> {
        let objects_dir = self.snapshot_dir.join("objects");
        // Build the current tree's path set (restorable text files only,
        // exactly like the round scan's `files_at_end`).
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
                let meta = match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(_) => continue,
                };
                if meta.len() > SNAPSHOT_MAX_FILE_BYTES {
                    continue;
                }
                let fingerprint = metadata_fingerprint(&meta);
                if let Some(cached) = self.round_scan_cache.get(&rel) {
                    if cached.fingerprint == fingerprint {
                        if cached.restorable {
                            current.insert(rel_path_key(&rel), cached.hash);
                        }
                        continue;
                    }
                }
                let snapshot = match inspect_file(&path) {
                    Ok(InspectedFile::Text(snapshot)) => snapshot,
                    Ok(InspectedFile::Unsupported(snapshot)) => {
                        self.round_scan_cache.insert(
                            rel,
                            ScanCacheEntry {
                                fingerprint,
                                hash: snapshot.hash,
                                hash_hex: snapshot.hash_hex,
                                restorable: false,
                            },
                        );
                        continue;
                    }
                    Err(_) => continue,
                };
                self.round_scan_cache.insert(
                    rel.clone(),
                    ScanCacheEntry {
                        fingerprint,
                        hash: snapshot.hash,
                        hash_hex: snapshot.hash_hex,
                        restorable: true,
                    },
                );
                current.insert(rel_path_key(&rel), snapshot.hash);
            }
        }

        let mut touched: u32 = 0;
        let restore_one = |watcher: &mut Self, rel: &str, target_hex: &str| -> bool {
            let obj = objects_dir.join(target_hex);
            let Ok(bytes) = std::fs::read(&obj) else {
                return false;
            };
            let abs = watcher.project_root.join(rel);
            if let Some(parent) = abs.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if atomic_write(&abs, &bytes).is_err() {
                return false;
            }
            // Record the restored content in the scan cache so the
            // post-restore re-sync walk does not re-read it.
            if let (Ok(meta), Some(hash)) = (std::fs::metadata(&abs), hex_decode_hash(target_hex)) {
                watcher.round_scan_cache.insert(
                    PathBuf::from(rel),
                    ScanCacheEntry {
                        fingerprint: metadata_fingerprint(&meta),
                        hash,
                        hash_hex: target_hex.to_string(),
                        restorable: true,
                    },
                );
            }
            true
        };

        // 1. For each current file: if not in target → delete. If hash differs → restore.
        for (rel, cur_hash) in &current {
            let cur_hex = hex_encode(cur_hash);
            match target.get(rel) {
                None => {
                    let abs = self.project_root.join(rel);
                    if std::fs::remove_file(&abs).is_ok() {
                        touched += 1;
                        self.round_scan_cache.remove(Path::new(rel));
                    }
                }
                Some(target_hex) if target_hex != &cur_hex => {
                    touched += u32::from(restore_one(self, rel, target_hex));
                }
                _ => {}
            }
        }

        // 2. For each target file not currently present → restore.
        for (rel, target_hex) in target {
            if !current.contains_key(rel) && restore_one(self, rel, target_hex) {
                touched += 1;
            }
        }

        Ok(touched)
    }

    /// After a bulk restore, walk the tree again to re-sync our in-memory
    /// hash mirror. Prevents the watcher from emitting spurious
    /// `FileChanged` events for paths we just rewrote.
    ///
    /// Cache-aware: `restore_to_state` refreshed the scan cache for every
    /// path it touched, so this walk is stats plus reads of only the files
    /// that changed outside the restore.
    fn refresh_hashes_from_tree(&mut self) {
        let mut new_hashes = HashMap::new();
        let mut large_file_fingerprints = HashMap::new();
        let mut next_cache: HashMap<PathBuf, ScanCacheEntry> =
            HashMap::with_capacity(self.round_scan_cache.len());
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
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if meta.len() > SNAPSHOT_MAX_FILE_BYTES {
                    let fingerprint = metadata_fingerprint(&meta);
                    // Keep the last-known hash when the file is untouched
                    // (its fingerprint matches) so the change index doesn't
                    // treat it as deleted after a restore; a genuinely
                    // changed oversized file gets one streaming re-hash.
                    let hash = match self.hashes.get(&rel) {
                        Some(prev)
                            if self.large_file_fingerprints.get(&rel) == Some(&fingerprint) =>
                        {
                            Some(*prev)
                        }
                        _ => sha256_file(&path).ok(),
                    };
                    if let Some(hash) = hash {
                        new_hashes.insert(rel.clone(), hash);
                    }
                    large_file_fingerprints.insert(rel, fingerprint);
                    continue;
                }
                let fingerprint = metadata_fingerprint(&meta);
                if let Some(cached) = self.round_scan_cache.get(&rel) {
                    if cached.fingerprint == fingerprint {
                        new_hashes.insert(rel.clone(), cached.hash);
                        next_cache.insert(rel, cached.clone());
                        continue;
                    }
                }
                if let Ok(snapshot) = inspect_file(&path) {
                    let (hash, hash_hex, restorable) = match snapshot {
                        InspectedFile::Text(snapshot) => (snapshot.hash, snapshot.hash_hex, true),
                        InspectedFile::Unsupported(snapshot) => {
                            (snapshot.hash, snapshot.hash_hex, false)
                        }
                    };
                    new_hashes.insert(rel.clone(), hash);
                    next_cache.insert(
                        rel,
                        ScanCacheEntry {
                            fingerprint,
                            hash,
                            hash_hex,
                            restorable,
                        },
                    );
                }
            }
        }
        self.hashes = new_hashes;
        self.large_file_fingerprints = large_file_fingerprints;
        self.round_scan_cache = next_cache;
    }

    /// Persist `history.json` atomically via tmp + rename.
    ///
    /// The file is a slim index (format 2): round scalars + `files_changed`,
    /// never the per-round path→hash maps — those live once per round in
    /// `rounds/round_{id}/manifest.json`. The rewrite therefore stays small
    /// and roughly constant per round instead of growing with
    /// rounds × files. Binaries from before format 2 fail to parse a round
    /// without `files_at_end` and fall back to an empty history — no
    /// rollback offered, never a restore from an empty map.
    fn persist_history(&mut self) -> Result<(), CallerError> {
        let path = self.snapshot_dir.join("history.json");
        let mut value = serde_json::to_value(&self.history)
            .map_err(|e| CallerError::Config(format!("history serialize: {}", e)))?;
        value["format"] = serde_json::Value::from(HISTORY_INDEX_FORMAT);
        let bytes = serde_json::to_vec_pretty(&value)
            .map_err(|e| CallerError::Config(format!("history serialize: {}", e)))?;
        atomic_write(&path, &bytes).map_err(CallerError::Io)?;
        let new_len = bytes.len() as u64;
        self.snapshot_dir_size_estimate = self
            .snapshot_dir_size_estimate
            .saturating_sub(self.last_history_index_bytes)
            .saturating_add(new_len);
        self.last_history_index_bytes = new_len;
        Ok(())
    }

    /// Collect the set of hashes that are still referenced by any live round
    /// (active path) plus the baseline, and delete any `objects/{hash}` file
    /// not in that set. Returns bytes freed.
    ///
    /// Referenced hashes resolve through the per-round manifests (deduped by
    /// backreference source). Fail-safe: if any live round's maps cannot be
    /// resolved, nothing is deleted — an unreadable manifest must never
    /// orphan objects a rollback still needs.
    fn gc_orphaned_objects(&self) -> u64 {
        let mut referenced: HashSet<String> = HashSet::new();
        let sources: HashSet<u64> = self
            .history
            .rounds
            .iter()
            .map(|r| r.maps_from_round.unwrap_or(r.id))
            .collect();
        for source_id in sources {
            let Some(maps) = self.resolved_round_maps(source_id) else {
                return 0;
            };
            referenced.extend(maps.files_at_end.into_values());
        }
        for meta in self.baseline_manifest.values() {
            if meta.supported_text {
                referenced.insert(meta.hash.clone());
            }
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
    ///
    /// Gated on the incrementally maintained size estimate: the authoritative
    /// full-tree walk only runs when the estimate crosses the cap (it used to
    /// run on every round).
    fn enforce_soft_cap(&mut self) -> Result<(), CallerError> {
        if self.snapshot_dir_size_estimate <= SNAPSHOT_DIR_SOFT_CAP_BYTES {
            return Ok(());
        }
        let mut size = dir_byte_size(&self.snapshot_dir);
        self.snapshot_dir_size_estimate = size;
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
            self.snapshot_dir_size_estimate = self.snapshot_dir_size_estimate.saturating_sub(freed);
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
        assert!(!watcher.baseline_manifest.contains_key("empty.txt"));
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
        let round_id = watcher.history.rounds.last().expect("round").id;
        let round = watcher.history.rounds.last().expect("round");

        assert_eq!(
            round.files_changed,
            vec!["data.dat".to_string(), "text.txt".to_string()]
        );
        let maps = watcher
            .resolved_round_maps(round_id)
            .expect("resolved maps");
        assert!(maps.files_at_end.contains_key("text.txt"));
        assert!(!maps.files_at_end.contains_key("data.dat"));
        assert!(maps.all_files_at_end.contains_key("data.dat"));
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

    /// Snapshot creates a round, files_at_end captures restorable text-file
    /// hashes, and rollback restores content even when files have been
    /// mutated since.
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
            .resolved_round_maps(r2)
            .expect("resolved maps")
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
                maps_from_round: None,
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

    /// Round scans must not re-read files whose (size, mtime) fingerprint is
    /// unchanged. Proven through the short-circuit's one observable: content
    /// rewritten at identical length with a restored mtime keeps the
    /// previously recorded hash (nothing re-read it), while a rewrite whose
    /// mtime moves is picked up again.
    #[test]
    fn round_scan_short_circuits_unchanged_fingerprints() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let file = root.join("a.txt");
        std::fs::write(&file, b"round-1!").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        let r1_hash = w.resolved_round_maps(r1).unwrap().files_at_end["a.txt"].clone();

        let original_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, b"round-2!").unwrap();
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(original_mtime).unwrap();
        drop(handle);
        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        assert_eq!(
            w.resolved_round_maps(r2).unwrap().files_at_end["a.txt"],
            r1_hash,
            "fingerprint-unchanged file must be served from the cache, not re-hashed"
        );

        std::fs::write(&file, b"round-3!").unwrap();
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle
            .set_modified(original_mtime + std::time::Duration::from_secs(5))
            .unwrap();
        drop(handle);
        w.on_round_complete("R3".into(), None, None).unwrap();
        let r3 = w.history.current_head_id.unwrap();
        let r3_maps = w.resolved_round_maps(r3).unwrap();
        assert_ne!(
            r3_maps.files_at_end["a.txt"], r1_hash,
            "a moved fingerprint must be re-read"
        );
        assert_eq!(
            r3_maps.files_at_end["a.txt"],
            hex_encode(&sha256_hash(b"round-3!"))
        );
    }

    /// A no-op round (tree identical to its parent) records a depth-1
    /// `maps_from_round` backreference instead of duplicating the maps;
    /// chains of no-ops keep pointing at the nearest changed round; and
    /// rollback to a no-op round restores exactly the referenced state.
    #[test]
    fn noop_rounds_backreference_and_stay_restorable() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        w.on_round_complete("R2 noop".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        w.on_round_complete("R3 noop".into(), None, None).unwrap();

        assert_eq!(w.history.rounds[1].maps_from_round, Some(r1));
        assert_eq!(
            w.history.rounds[2].maps_from_round,
            Some(r1),
            "no-op chains compress to the nearest changed round (depth-1)"
        );
        let manifest_bytes =
            std::fs::read(round_manifest_path(tmp_snap.path(), r2)).expect("noop manifest");
        let manifest: HistoryRound = serde_json::from_slice(&manifest_bytes).unwrap();
        assert!(manifest.files_at_end.is_empty());
        assert_eq!(manifest.maps_from_round, Some(r1));

        std::fs::write(root.join("a.txt"), b"v4").unwrap();
        w.on_round_complete("R4".into(), None, None).unwrap();
        w.rollback(r2).unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
    }

    /// history.json is a slim format-2 index: no per-round maps regardless
    /// of round count, while the maps stay resolvable via the round
    /// manifests.
    #[test]
    fn history_json_is_a_slim_format2_index() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"one").unwrap();
        std::fs::write(root.join("b.txt"), b"two").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), Some(1), None).unwrap();
        std::fs::write(root.join("a.txt"), b"one-more").unwrap();
        w.on_round_complete("R2".into(), Some(2), None).unwrap();
        let r2 = w.history.current_head_id.unwrap();

        let text = std::fs::read_to_string(tmp_snap.path().join("history.json")).unwrap();
        assert!(text.contains("\"format\": 2"));
        assert!(
            !text.contains("files_at_end"),
            "the index must not carry per-round maps: {text}"
        );

        let parsed: History = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.rounds.len(), 2);
        assert_eq!(parsed.rounds[1].turn_count, Some(2));

        let maps = w.resolved_round_maps(r2).unwrap();
        assert_eq!(maps.files_at_end.len(), 2);
        assert_eq!(
            maps.files_at_end["a.txt"],
            hex_encode(&sha256_hash(b"one-more"))
        );
    }

    /// A legacy (pre-format-2) history.json with inline maps still rolls
    /// back exactly: loading migrates the maps into per-round manifests.
    #[test]
    fn legacy_history_json_migrates_and_rolls_back() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), Some(1), Some(3)).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), Some(1), Some(5)).unwrap();
        let r2 = w.history.current_head_id.unwrap();

        // Rebuild the pre-format-2 on-disk layout: maps inline in
        // history.json (no format marker), no round manifests.
        let mut legacy = w.history.clone();
        for round in &mut legacy.rounds {
            let maps = w.resolved_round_maps(round.id).unwrap();
            round.files_at_end = maps.files_at_end;
            round.all_files_at_end = maps.all_files_at_end;
            round.maps_from_round = None;
        }
        drop(w);
        std::fs::remove_dir_all(tmp_snap.path().join("rounds")).unwrap();
        let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        atomic_write(&tmp_snap.path().join("history.json"), &legacy_bytes).unwrap();

        let mut resumed = make_watcher(root, tmp_snap.path());
        assert!(
            round_manifest_path(tmp_snap.path(), r1).exists(),
            "legacy load must materialize round manifests"
        );
        assert_eq!(resumed.history.rounds.len(), 2);
        assert!(resumed
            .history
            .rounds
            .iter()
            .all(|r| r.files_at_end.is_empty()));
        assert_eq!(resumed.history.rounds[1].native_message_count, Some(5));
        assert_eq!(resumed.history.current_head_id, Some(r2));

        resumed.rollback(r1).unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
    }

    /// Rollback and redo still work after a restart: a fresh watcher over
    /// the same snapshot dir reloads the slim index and resolves maps from
    /// the manifests (head cache starts cold), and the next round diffs
    /// against the resolved parent.
    #[test]
    fn resumed_watcher_rolls_back_and_redoes_from_manifests() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        std::fs::write(root.join("b.txt"), b"new").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        drop(w);

        let mut resumed = make_watcher(root, tmp_snap.path());
        resumed.rollback(r1).unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
        assert!(!root.join("b.txt").exists());

        let redo = resumed.redo().unwrap();
        assert_eq!(redo.to_round_id, r2);
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v2");
        assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"new");

        std::fs::write(root.join("a.txt"), b"v3").unwrap();
        resumed.on_round_complete("R3".into(), None, None).unwrap();
        let r3_round = resumed.history.rounds.last().unwrap();
        assert_eq!(
            r3_round.files_changed,
            vec!["a.txt".to_string()],
            "post-resume rounds must diff against the resolved parent maps"
        );
    }

    /// Oversized files keep their last-known hash across a rollback's
    /// hash re-sync, so the changes index does not misreport them as
    /// deleted (they are stat'd, not re-hashed, when untouched).
    #[test]
    fn oversized_hash_survives_rollback_resync() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let big = vec![b'x'; SNAPSHOT_MAX_FILE_BYTES as usize + 1];
        std::fs::write(root.join("big.csv"), &big).unwrap();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        let big_hash_before = w
            .changes_index_snapshot()
            .current_hashes
            .get("big.csv")
            .cloned()
            .expect("oversized file hashed at baseline");
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();

        w.rollback(r1).unwrap();
        let index = w.changes_index_snapshot();
        assert_eq!(
            index.current_hashes.get("big.csv"),
            Some(&big_hash_before),
            "rollback re-sync must not drop oversized files from the hash index"
        );
    }

    /// `start_shared` registers the watcher; the registry resolves it by
    /// the exact (project_root, snapshot_dir) pair and nothing else.
    #[tokio::test]
    async fn live_watcher_registry_resolves_exact_pair() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        std::fs::write(tmp_proj.path().join("a.txt"), b"hello").unwrap();
        let w = make_watcher(tmp_proj.path(), tmp_snap.path());
        let (shared, _watcher_handle, _round_handle) = w.start_shared();

        let found = live_watcher_for(tmp_proj.path(), tmp_snap.path()).expect("registered watcher");
        assert!(Arc::ptr_eq(&found, &shared));

        let other = TempDir::new().unwrap();
        assert!(live_watcher_for(other.path(), tmp_snap.path()).is_none());
        assert!(live_watcher_for(tmp_proj.path(), other.path()).is_none());
    }
}
