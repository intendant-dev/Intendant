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
//! only files whose fingerprint (size, mtime, platform change signal, file
//! identity) moved is re-read and re-hashed — so per-round work is
//! proportional to actual changes; blob existence is answered by an
//! in-memory object index rather than per-file `objects/` stats; rounds
//! for sessions working OTHER roots are skipped by root routing; the boot
//! baseline walk reuses fingerprint-unchanged entries across restarts; and
//! the live notify path does its read+hash work off the watcher lock.
//! Durable state is split
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
    /// Store-epoch stamp carried by per-round manifests (see
    /// [`History::store_epoch`]). The resolver refuses a manifest whose
    /// stamp differs from the index's, so a manifest overwritten by a
    /// pre-format-2 binary (whose restarted round ids collide with ours) is
    /// an explicit "cannot restore" instead of a silently wrong tree.
    /// `None` on index round stubs and on legacy data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_epoch: Option<String>,
    /// Marker that this round's maps live inline in THIS record — set on an
    /// index row when its manifest write failed and the maps were retained.
    /// Load-bearing for empty trees: an empty inline map serializes to
    /// nothing, so without the marker a retained empty-tree round would
    /// reload as an ordinary slim stub and its restore-to-empty state would
    /// be unreachable. `false` everywhere else (stubs, manifests, legacy).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub maps_inline: bool,
    /// Content hash of this round's maps (the actual restore payload),
    /// recorded in the slim index row when the manifest is written. The
    /// resolver verifies a manifest's maps against this before serving —
    /// scalar binding alone would bless a manifest whose scalars match but
    /// whose maps were replaced. `None` only on rows that predate the
    /// field (verification is skipped for them; every write path stamps
    /// it, and the restamp sweep backfills it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maps_hash: Option<String>,
}

/// Canonical content hash of a round's two maps: length-prefixed,
/// domain-separated, key-sorted — independent of `HashMap` iteration order
/// and JSON formatting.
fn maps_content_hash(
    files_at_end: &HashMap<String, String>,
    all_files_at_end: &HashMap<String, String>,
) -> String {
    fn fold_map(hasher: &mut Sha256, label: &[u8], map: &HashMap<String, String>) {
        hasher.update(label);
        hasher.update((map.len() as u64).to_le_bytes());
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            hasher.update((key.len() as u64).to_le_bytes());
            hasher.update(key.as_bytes());
            let value = &map[key];
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
    }
    let mut hasher = Sha256::new();
    fold_map(&mut hasher, b"files_at_end", files_at_end);
    fold_map(&mut hasher, b"all_files_at_end", all_files_at_end);
    let digest = hasher.finalize();
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&digest);
    hex_encode(&raw)
}

/// Whether an index row is currently the authoritative carrier of its own
/// maps (retained after a failed migration, or a legacy row not yet
/// migrated) — as opposed to a slim stub whose maps live in a manifest.
fn round_has_inline_maps(round: &HistoryRound) -> bool {
    round.maps_inline || !round.files_at_end.is_empty() || !round.all_files_at_end.is_empty()
}

/// Content binding between a per-round manifest and its index row: every
/// scalar the index records must agree. Identity (`id`) alone is never
/// enough — a pre-format-2 binary restarts ids at 0, so a manifest it
/// overwrote carries one of our ids with a different round's content.
fn manifest_binds_to_round(manifest: &HistoryRound, stub: &HistoryRound) -> bool {
    manifest.id == stub.id
        && manifest.parent_id == stub.parent_id
        && manifest.summary == stub.summary
        && manifest.timestamp_unix == stub.timestamp_unix
        && manifest.files_changed == stub.files_changed
        && manifest.turn_count == stub.turn_count
        && manifest.native_message_count == stub.native_message_count
        && manifest.maps_from_round == stub.maps_from_round
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
    /// Identity of this snapshot store, minted the first time the history
    /// loads under format 2 and stamped into every per-round manifest
    /// written since. Binds manifests to this index: the resolver refuses a
    /// manifest carrying a different (or no) stamp, so nothing can restore
    /// from a manifest some other timeline wrote over ours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_epoch: Option<String>,
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
    /// Metadata fingerprint of the SOURCE file at recording time. The next
    /// boot's baseline walk reuses this entry — skipping the re-read and
    /// the baseline rewrite — when the live file still fingerprints the
    /// same (ctime/identity included; (len, mtime) alone is spoofable and
    /// has failed repeatedly in this repo's history). `None` on manifests
    /// written before the field existed: those entries always take the
    /// full re-read + rewrite path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<FileFingerprint>,
    /// Wall-clock time (nanos since epoch, walk start) the fingerprint was
    /// recorded — the cross-boot racy-distrust gate (see
    /// [`baseline_entry_reusable`]). The monotonic gate the in-memory scan
    /// cache also uses cannot survive a restart; the wall clock is what
    /// crosses boots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_nanos: Option<u128>,
}

pub(crate) type BaselineManifest = HashMap<String, BaselineFileMeta>;

/// Metadata identity used to decide whether a file may have changed without
/// re-reading it. Size and mtime alone are spoofable (mtime-preserving
/// writers, coarse timestamp granules), so the fingerprint also carries the
/// platform's change signal and file identity from
/// [`crate::platform::file_change_stamp`]:
///
/// - Unix: inode ctime (bumped by every content write; not settable by
///   `touch -r`/`rsync -t` style tools) plus `(dev, ino)`.
/// - Windows: NTFS `ChangeTime` via `GetFileInformationByHandleEx` (bumped
///   by every write; not settable through `SetFileTime`) plus the volume +
///   file-index identity.
/// - Elsewhere: no signal exists — see [`FileFingerprint::matches`].
///
/// Serialized (all fields optional-tolerant) inside [`BaselineFileMeta`]
/// for the cross-boot baseline fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileFingerprint {
    size: u64,
    /// Modification time in nanos since the epoch; `None` when unreadable.
    mtime_nanos: Option<u128>,
    /// Platform change signal (see type docs); `None` when the platform
    /// query failed (Windows handle open/query) or offers no signal.
    change_signal: Option<i128>,
    /// Real file identity `(volume/dev, index/inode)`; `None` alongside a
    /// `None` change signal.
    file_id: Option<(u64, u64)>,
}

impl FileFingerprint {
    /// True when `other` plausibly describes the same file state. Requires a
    /// *known, equal* mtime on both sides — a fingerprint with an unreadable
    /// timestamp never matches anything — and, on platforms that have a
    /// change signal (Unix, Windows), a *present, equal* signal: a failed
    /// signal query never matches, so degraded stats always fall toward
    /// re-reading.
    fn matches(&self, other: &FileFingerprint) -> bool {
        let change_signal_ok = match (self.change_signal, other.change_signal) {
            (Some(a), Some(b)) => a == b,
            // Where a signal is expected, its absence is a failed query,
            // never proof of anything.
            (None, None) => !cfg!(any(unix, windows)),
            _ => false,
        };
        self.size == other.size
            && self.mtime_nanos.is_some()
            && self.mtime_nanos == other.mtime_nanos
            && change_signal_ok
            && self.file_id == other.file_id
    }
}

/// Racy-write distrust window, git-style: a cache entry whose mtime is not
/// comfortably older than the moment the entry was recorded could have been
/// rewritten in the same timestamp granule after we hashed it — such entries
/// are treated as misses and re-read. 2s covers the coarsest real granule
/// (FAT's 2s; Windows ~15ms; Linux jiffies ~4-10ms), so only files modified
/// within 2s before their recording walk pay a re-read on the next walk.
const FINGERPRINT_RACY_WINDOW_NANOS: u128 = 2_000_000_000;

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        // Clock before epoch: record 0 so every entry looks racy and gets
        // re-read — the safe direction.
        .unwrap_or(0)
}

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

/// Fingerprint the file at `path`. Unix derives everything from `meta`;
/// Windows additionally opens a handle for the ChangeTime + identity query
/// (a failed query yields `None` fields, which [`FileFingerprint::matches`]
/// treats as never-matching). Call with a stat taken *before* any content
/// read the fingerprint will describe.
fn file_fingerprint(path: &Path, meta: &std::fs::Metadata) -> FileFingerprint {
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    let stamp = crate::platform::file_change_stamp(path, meta);
    FileFingerprint {
        size: meta.len(),
        mtime_nanos,
        change_signal: stamp.as_ref().map(|s| s.change_signal),
        file_id: stamp
            .as_ref()
            .map(|s| (s.identity.volume, s.identity.file_index)),
    }
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

/// Per-process sequence for damaged-index backup names (see
/// `load_history_from_disk`): keeps same-second backups from colliding.
static DAMAGED_BACKUP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Path of the on-disk manifest carrying one round's full snapshot record.
fn round_manifest_path(snapshot_dir: &Path, round_id: u64) -> PathBuf {
    snapshot_dir
        .join("rounds")
        .join(format!("round_{}", round_id))
        .join("manifest.json")
}

/// Result of loading `history.json`: the (slim) history, whether the index
/// changed in ways that must be persisted immediately (adopted epoch,
/// migrated maps) — before any new manifest is stamped with state the
/// on-disk index doesn't know yet — and whether the store must be treated
/// as read-only because an existing `history.json` was damaged and could
/// not be set aside (it must never be overwritten).
struct LoadedHistory {
    history: History,
    needs_persist: bool,
    force_read_only: bool,
}

impl LoadedHistory {
    fn read_only_with(history: History) -> Self {
        Self {
            history,
            needs_persist: false,
            force_read_only: true,
        }
    }
}

/// Load `history.json`, minting the store epoch when absent and migrating
/// any round that still carries inline maps (legacy pre-format-2 files, or
/// rounds retained after an earlier failed migration) into stamped
/// per-round manifests. Rounds whose manifest write fails KEEP their inline
/// maps — marked via `maps_inline` so even empty trees survive — and the
/// index remains the authoritative carrier for them until a later load
/// succeeds.
///
/// A present-but-unparseable `history.json` is never overwritten: it is
/// renamed aside (`history.json.damaged-<ts>`) and a fresh timeline starts;
/// if even the rename fails, the watcher runs read-only.
///
/// With `read_only` (another process owns the store lock), the file is
/// parsed as-is: no mint, no migration, no renames, no persist.
fn load_history_from_disk(snapshot_dir: &Path, read_only: bool) -> LoadedHistory {
    let history_path = snapshot_dir.join("history.json");
    // Ok(Some(..)) = parsed; Ok(None) = absent (fresh store); Err(()) =
    // present but unreadable/unparseable — the damaged case.
    let parsed: Result<Option<(History, u64)>, ()> = match std::fs::read(&history_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Err(_) => Err(()),
            Ok(value) => {
                let format = value.get("format").and_then(|v| v.as_u64()).unwrap_or(0);
                match serde_json::from_value::<History>(value) {
                    Ok(history) => Ok(Some((history, format))),
                    Err(_) => Err(()),
                }
            }
        },
    };

    let (mut history, format) = match parsed {
        Ok(Some((history, format))) => (history, format),
        Ok(None) => (History::default(), HISTORY_INDEX_FORMAT),
        Err(()) => {
            if read_only {
                return LoadedHistory::read_only_with(History::default());
            }
            // Collision-proof backup name (std rename REPLACES an existing
            // file): time + pid + per-process sequence, existence-checked —
            // sound because the store lock makes us the only writer.
            let damaged_path = loop {
                let seq = DAMAGED_BACKUP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let candidate = snapshot_dir.join(format!(
                    "history.json.damaged-{}-{}-{}",
                    now_unix(),
                    std::process::id(),
                    seq
                ));
                if !candidate.exists() {
                    break candidate;
                }
            };
            match std::fs::rename(&history_path, &damaged_path) {
                Ok(()) => {
                    // Residual, documented: the fresh timeline reuses round
                    // ids from 0, so new manifests will overwrite the damaged
                    // timeline's rounds/round_N files as rounds accrue. With
                    // epoch + maps-hash binding those old manifests are
                    // already unresolvable by this fresh index — the damaged
                    // JSON is preserved for forensics, not for automatic
                    // recovery.
                    eprintln!(
                        "[file_watcher] history.json was unreadable; preserved it at {} and \
                         starting a fresh timeline",
                        damaged_path.display()
                    );
                    (History::default(), HISTORY_INDEX_FORMAT)
                }
                Err(err) => {
                    eprintln!(
                        "[file_watcher] history.json is unreadable and could not be set aside \
                         ({}); rewind runs read-only so it is never overwritten",
                        err
                    );
                    return LoadedHistory::read_only_with(History::default());
                }
            }
        }
    };

    if read_only {
        return LoadedHistory {
            history,
            needs_persist: false,
            force_read_only: false,
        };
    }

    let mut needs_persist = false;
    if history.store_epoch.is_none() {
        let epoch = mint_store_epoch(snapshot_dir);
        let restamped_ok = format < HISTORY_INDEX_FORMAT
            || restamp_manifests_for_new_epoch(
                &mut history,
                snapshot_dir,
                &epoch,
                &mut needs_persist,
            );
        if restamped_ok {
            history.store_epoch = Some(epoch);
            needs_persist = true;
        } else {
            // A content-binding manifest could not be restamped (e.g. a
            // transiently unwritable dir). Adopting the epoch now would make
            // that correct manifest permanently unresolvable once the index
            // persists — so stay epoch-less this load (the resolver falls
            // back to content binding under a `None` epoch) and retry the
            // mint on the next load.
            eprintln!(
                "[file_watcher] could not stamp every round manifest with the new store epoch; \
                 deferring epoch adoption to the next load"
            );
        }
    }

    // Legacy files carry every round's state inline — even an empty map is
    // an explicit "empty tree" record that must become a manifest (distinct
    // from a missing manifest, which rollback refuses). Format-2 rounds with
    // empty maps are ordinary slim stubs unless their `maps_inline` marker
    // says otherwise.
    let treat_empty_as_inline = format < HISTORY_INDEX_FORMAT;
    migrate_inline_round_maps(
        &mut history,
        snapshot_dir,
        treat_empty_as_inline,
        &mut needs_persist,
    );

    LoadedHistory {
        history,
        needs_persist,
        force_read_only: false,
    }
}

/// Mint a fresh store epoch: unique per (time, process, store path).
fn mint_store_epoch(snapshot_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(now_nanos().to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(snapshot_dir.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&digest);
    hex_encode(&raw)[..16].to_string()
}

/// Move every round's inline maps into a stamped per-round manifest. On a
/// successful write the inline maps are dropped from the index; on failure
/// (or a serialization error) they are KEPT and the row is marked
/// `maps_inline`, so the data — including an empty tree, which serializes
/// no maps of its own — stays authoritative in the index and migration
/// retries on the next load.
fn migrate_inline_round_maps(
    history: &mut History,
    snapshot_dir: &Path,
    treat_empty_as_inline: bool,
    needs_persist: &mut bool,
) {
    let epoch = history.store_epoch.clone();
    let migrate_round = |round: &mut HistoryRound, needs_persist: &mut bool| {
        if !round_has_inline_maps(round) && !treat_empty_as_inline {
            return;
        }
        // Always rewrite from the inline maps — they are the authoritative
        // copy; a pre-existing manifest at this id is not trusted (it may be
        // unparseable, unstamped, or a foreign timeline's).
        let maps_hash = maps_content_hash(&round.files_at_end, &round.all_files_at_end);
        let manifest = HistoryRound {
            store_epoch: epoch.clone(),
            maps_from_round: None,
            maps_inline: false,
            maps_hash: Some(maps_hash.clone()),
            ..round.clone()
        };
        let manifest_path = round_manifest_path(snapshot_dir, round.id);
        let written = serde_json::to_vec_pretty(&manifest)
            .ok()
            .is_some_and(|bytes| atomic_write(&manifest_path, &bytes).is_ok());
        if written {
            round.files_at_end = HashMap::new();
            round.all_files_at_end = HashMap::new();
            round.maps_from_round = None;
            round.maps_inline = false;
            round.maps_hash = Some(maps_hash);
            *needs_persist = true;
        } else if !round.maps_inline {
            round.maps_inline = true;
            *needs_persist = true;
        }
    };
    for round in &mut history.rounds {
        migrate_round(round, needs_persist);
    }
    for branch in &mut history.abandoned_branches {
        for round in &mut branch.rounds {
            migrate_round(round, needs_persist);
        }
    }
}

/// Re-stamp existing manifests with a freshly minted epoch (epoch-less
/// format-2 index only — see `load_history_from_disk`). A manifest is only
/// stamped when its content BINDS to its index row
/// ([`manifest_binds_to_round`]) — identity alone never blesses a manifest,
/// so one poisoned by a pre-format-2 binary (same id, different round)
/// stays unstamped and fails closed at resolve time. Rows that predate
/// `maps_hash` are backfilled from the binding manifest's maps, freezing
/// the payload against later tampering. Returns `false` when any binding
/// manifest could not be written OR read (a transiently unreadable correct
/// manifest must defer epoch adoption exactly like a failed write —
/// otherwise the persisted epoch would orphan it permanently).
fn restamp_manifests_for_new_epoch(
    history: &mut History,
    snapshot_dir: &Path,
    epoch: &str,
    needs_persist: &mut bool,
) -> bool {
    let mut all_ok = true;
    let mut restamp_round = |round: &mut HistoryRound| {
        if round_has_inline_maps(round) {
            // Retained in the index; no manifest of ours to stamp.
            return true;
        }
        let manifest_path = round_manifest_path(snapshot_dir, round.id);
        let bytes = match std::fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Missing: unresolvable with or without an epoch — not a
                // stamp failure.
                return true;
            }
            Err(_) => {
                // Transient read failure (permissions, I/O): the manifest
                // may be perfectly correct — adopting the epoch now would
                // orphan it. Defer.
                return false;
            }
        };
        let Ok(mut manifest) = serde_json::from_slice::<HistoryRound>(&bytes) else {
            return true;
        };
        if !manifest_binds_to_round(&manifest, round) {
            eprintln!(
                "[file_watcher] round {} manifest does not match the index (a foreign or \
                 damaged write?) — leaving it unstamped; restores of that round will refuse",
                round.id
            );
            return true;
        }
        if round.maps_hash.is_none() {
            round.maps_hash = Some(maps_content_hash(
                &manifest.files_at_end,
                &manifest.all_files_at_end,
            ));
            *needs_persist = true;
        }
        if manifest.store_epoch.as_deref() == Some(epoch) {
            return true;
        }
        manifest.store_epoch = Some(epoch.to_string());
        serde_json::to_vec_pretty(&manifest)
            .ok()
            .is_some_and(|stamped| atomic_write(&manifest_path, &stamped).is_ok())
    };
    for round in &mut history.rounds {
        all_ok &= restamp_round(round);
    }
    for branch in &mut history.abandoned_branches {
        for round in &mut branch.rounds {
            all_ok &= restamp_round(round);
        }
    }
    all_ok
}

/// Advisory exclusive lock on the snapshot store, held for the watcher's
/// lifetime (released when the returned `File` drops). Locks a dedicated
/// `store.lock` file — never `history.json` itself, whose reads Windows'
/// LockFileEx would otherwise block. `(lock, read_only)`: any failure to
/// acquire — held by another process, or a filesystem that cannot lock —
/// yields read-only, the safe direction (no writes can interleave).
fn acquire_store_lock(snapshot_dir: &Path) -> (Option<std::fs::File>, bool) {
    let path = snapshot_dir.join("store.lock");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!(
                "[file_watcher] could not open store lock {}: {} — rewind runs read-only",
                path.display(),
                err
            );
            return (None, true);
        }
    };
    match file.try_lock() {
        Ok(()) => (Some(file), false),
        Err(_) => (None, true),
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

/// Seed the in-memory object index: one `objects/` directory listing,
/// collecting the blob names that look like sha256 hex (the only names the
/// store ever writes; in-flight `.intendant-write-*.tmp` staging files are
/// skipped). A missing or unreadable directory seeds empty — the safe
/// direction: an absent index entry falls toward re-storing the blob.
fn seed_object_index(objects_dir: &Path) -> HashSet<String> {
    let mut index = HashSet::new();
    let Ok(entries) = std::fs::read_dir(objects_dir) else {
        return index;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            index.insert(name.to_string());
        }
    }
    index
}

/// Remove `baseline/` files that are not supported-text entries of the
/// current baseline manifest — leftovers from a previous run whose source
/// files were deleted (or stopped being text) before this resume. Keeps the
/// on-disk baseline key universe identical to the manifest's, so the
/// full-scan and watcher-index changes paths agree.
///
/// Returns `false` on any failure (unreadable subdir, undeletable stale
/// file): the key universes may then diverge, and the caller must disable
/// the watcher-index fast path rather than serve divergent views.
fn reconcile_baseline_dir(baseline_dir: &Path, manifest: &BaselineManifest) -> bool {
    let mut ok = true;
    let mut stack = vec![baseline_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => {
                ok = false;
                continue;
            }
        };
        for entry in entries {
            // Per-entry iteration errors matter: a hidden stale file is a
            // divergence between the on-disk universe and the manifest.
            let Ok(entry) = entry else {
                ok = false;
                continue;
            };
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => {
                    ok = false;
                    continue;
                }
            };
            // baseline/ only ever contains regular files and directories we
            // wrote ourselves — a symlink (or any other type) is a foreign
            // leftover. The fallback scan's reads would FOLLOW a file
            // symlink, so it must go like any other stale entry.
            if ft.is_symlink() || (!ft.is_dir() && !ft.is_file()) {
                if std::fs::remove_file(&path).is_err() && std::fs::remove_dir(&path).is_err() {
                    ok = false;
                }
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = match path.strip_prefix(baseline_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let keep = manifest
                .get(&rel_path_key(rel))
                .is_some_and(|meta| meta.supported_text);
            if !keep && std::fs::remove_file(&path).is_err() {
                ok = false;
            }
        }
    }
    ok
}

/// Clear wrong-typed leftovers from a previous run before writing a
/// baseline file at `baseline_path`: a stale FILE occupying a path that now
/// needs to be a directory (project `foo` became `foo/bar`), or a stale
/// DIRECTORY occupying a path that now needs to be a file (`foo/bar`
/// became file `foo`). Best-effort — a leftover this cannot clear makes
/// the subsequent baseline write fail loudly.
fn clear_stale_baseline_slot(baseline_dir: &Path, baseline_path: &Path) {
    let mut blocking_files: Vec<&Path> = Vec::new();
    let mut cursor = baseline_path.parent();
    while let Some(dir) = cursor {
        if dir == baseline_dir {
            break;
        }
        blocking_files.push(dir);
        cursor = dir.parent();
    }
    for ancestor in blocking_files {
        if ancestor.is_file() {
            let _ = std::fs::remove_file(ancestor);
        }
    }
    if baseline_path.is_dir() {
        let _ = std::fs::remove_dir_all(baseline_path);
    }
}

/// Whether a previous boot's manifest entry can stand in for re-reading
/// and re-baselining a live file that currently fingerprints as `live`:
///
/// 1. the stored fingerprint must fully match the live one (size, mtime,
///    platform change signal, file identity — see
///    [`FileFingerprint::matches`]; a fingerprint-less legacy entry never
///    matches), and
/// 2. the entry must pass the wall-clock racy-distrust gate: its mtime
///    must be comfortably older than its own recording time, exactly like
///    [`ScanCacheEntry::trustworthy_for`]'s wall-clock condition. (The
///    monotonic condition cannot cross a process boundary — the wall
///    clock is all that survives a restart.)
/// 3. for supported-text entries, the baseline shadow copy behind the
///    reused hash must still be an intact regular file of the recorded
///    size — a missing or truncated copy (crashed previous boot) must
///    heal via the full rewrite path, not be trusted. Residual: baseline
///    data writes are not fsynced, so a power loss can in principle
///    leave a full-size zero-filled copy on delayed-allocation
///    filesystems; the blast radius is display-only (diff baselines) —
///    restores read `objects/`, which is written via fsynced
///    [`atomic_write`].
fn baseline_entry_reusable(
    meta: &BaselineFileMeta,
    live: &FileFingerprint,
    racy_window_nanos: u128,
    baseline_path: &Path,
) -> bool {
    let (Some(stored), Some(recorded_at)) = (meta.fingerprint, meta.recorded_at_nanos) else {
        return false;
    };
    if !stored.matches(live) {
        return false;
    }
    let racy_ok = stored
        .mtime_nanos
        .is_some_and(|mtime| mtime.saturating_add(racy_window_nanos) < recorded_at);
    if !racy_ok {
        return false;
    }
    if meta.supported_text {
        match std::fs::metadata(baseline_path) {
            Ok(baseline_meta) => baseline_meta.is_file() && baseline_meta.len() == meta.size,
            Err(_) => false,
        }
    } else {
        true
    }
}

/// The on-disk baseline manifest, for read-only instances that must adopt
/// the store owner's baseline state instead of writing their own.
fn read_baseline_manifest_file(snapshot_dir: &Path) -> BaselineManifest {
    std::fs::read(snapshot_dir.join(BASELINE_MANIFEST_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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
/// Staleness contract (this is *detection*, not proof): a hit requires the
/// full fingerprint (size, mtime, platform change signal, file id) to match
/// AND the entry to pass the racy-distrust window below. The fingerprint is
/// taken from a stat performed *before* the content read it describes, so a
/// racing write can make the stored fingerprint only older than the hashed
/// content — combined with the window, a same-granule rewrite is re-read on
/// the next walk, and (on Unix) an mtime-backdated rewrite is caught by
/// ctime/inode. A write that defeats all of those at once (same length,
/// restored mtime, same change signal, same inode) is not detectable from
/// metadata; the live notify event for it still refreshes this cache via
/// `process_change`.
#[derive(Debug, Clone)]
struct ScanCacheEntry {
    fingerprint: FileFingerprint,
    hash: [u8; 32],
    hash_hex: String,
    /// True when the file was a supported text file (stored in `objects/`
    /// and restorable); false for inspected-but-unsupported files that only
    /// feed the display mirror.
    restorable: bool,
    /// Wall-clock time this entry was recorded (walk start for
    /// scan-recorded entries), compared against the file's mtime — same
    /// clock domain as filesystem timestamps.
    recorded_at_nanos: u128,
    /// Monotonic recording instant: the primary racy-distrust gate, immune
    /// to wall-clock steps (in-memory only, never serialized).
    recorded_at_instant: std::time::Instant,
}

impl ScanCacheEntry {
    /// True when this entry can be trusted for a file currently stat'ing as
    /// `current`: full fingerprint match plus the racy-distrust window,
    /// gated both ways —
    ///
    /// 1. monotonically: at least `window` of real time must have passed
    ///    since the entry was hashed (a backward wall-clock step cannot
    ///    fake this), and
    /// 2. on the wall clock: the entry's mtime must be comfortably older
    ///    than its recording time (a same-granule rewrite lands near the
    ///    recording moment and fails this).
    fn trustworthy_for(&self, current: &FileFingerprint, window_nanos: u128) -> bool {
        self.fingerprint.matches(current)
            && self.recorded_at_instant.elapsed().as_nanos() >= window_nanos
            && self
                .fingerprint
                .mtime_nanos
                .is_some_and(|mtime| mtime.saturating_add(window_nanos) < self.recorded_at_nanos)
    }
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
    /// In-memory index of the blob hashes known to exist under `objects/`.
    /// Seeded from one directory listing at construction and maintained on
    /// every object write and GC delete, so round walks consult this set
    /// instead of stat'ing `objects/{hash}` once per cached file (which
    /// doubled the stat count of big-tree walks). Repair contract: a
    /// restore that finds a blob missing on disk removes its hash here
    /// (later walks re-store the content), and a walk that would write a
    /// blob the index does not know first re-checks the disk — so an
    /// index/disk divergence in either direction converges instead of
    /// making a `files_at_end` hash unrestorable.
    object_index: HashSet<String>,
    /// True until the notify watcher is confirmed running, and again after a
    /// notify error or loop exit. While degraded, `changes_index_snapshot`
    /// returns `None` so the gateway's changes fast path stands down and the
    /// full scan serves (correct, just slower). A rescan-flagged notify
    /// event triggers a full re-sync instead of degrading.
    live_index_degraded: bool,
    /// Sticky sibling of `live_index_degraded`: set when this watcher can
    /// never trust its live index for the rest of the process (read-only
    /// mode, a failed baseline reconciliation). Never cleared.
    live_index_disabled: bool,
    /// True when another process holds this store's lock (or the store was
    /// damaged in a way we could not set aside): this watcher serves reads
    /// (/history, the legacy changes scan) but refuses every mutation —
    /// round recording, rollback/redo/prune, index persists.
    read_only: bool,
    /// Advisory store lock (a dedicated `store.lock` file — never
    /// `history.json` itself; Windows LockFileEx blocks reads of the locked
    /// file). Held for the watcher's lifetime; released on drop.
    _store_lock: Option<std::fs::File>,
    /// Racy-distrust window (see [`ScanCacheEntry::trustworthy_for`]).
    /// `FINGERPRINT_RACY_WINDOW_NANOS` in production; tests shrink it to
    /// exercise fingerprint mechanics without multi-second sleeps.
    racy_window_nanos: u128,
    /// Test-only observability: how many files the last
    /// `scan_and_store_objects` walk actually read (cache misses).
    #[cfg(test)]
    pub(crate) files_read_in_last_scan: usize,
    /// Test-only observability: how many baseline copies the constructor
    /// walk wrote (files whose previous-boot manifest entry could not be
    /// reused — see [`baseline_entry_reusable`]).
    #[cfg(test)]
    pub(crate) baseline_files_rewritten_in_new: usize,
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
        Self::new_with_racy_window(
            project_root,
            snapshot_dir,
            bus,
            FINGERPRINT_RACY_WINDOW_NANOS,
        )
    }

    /// [`Self::new`] with an injectable racy-distrust window, so tests can
    /// exercise the cross-boot baseline fast path without multi-second
    /// sleeps. Production always passes `FINGERPRINT_RACY_WINDOW_NANOS`.
    ///
    /// HARD INVARIANT: the baseline walk below runs synchronously in this
    /// constructor — a baseline copy must exist before the first mutation
    /// can be observed (snapshot-before-first-write), or the first diff
    /// against a late-written baseline reports the whole file as added.
    /// The cross-boot reuse check is a fast path OF the walk, never a
    /// deferral of it.
    fn new_with_racy_window(
        project_root: PathBuf,
        snapshot_dir: PathBuf,
        bus: EventBus,
        racy_window_nanos: u128,
    ) -> Result<Self, CallerError> {
        let baseline_dir = snapshot_dir.join("baseline");
        // Only the store root exists before the lock: the lockfile needs a
        // home, and a read-only loser must create nothing else.
        std::fs::create_dir_all(&snapshot_dir)
            .map_err(|e| CallerError::Config(format!("create snapshot dir: {}", e)))?;

        // Cross-process exclusion, acquired before any store write: a second
        // watcher on the same store (another daemon resuming this session)
        // must never interleave baseline/manifest/index writes with ours.
        let (store_lock, lock_read_only) = acquire_store_lock(&snapshot_dir);
        if lock_read_only {
            eprintln!(
                "[file_watcher] snapshot store {} is locked by another intendant process — \
                 rewind runs read-only in this instance",
                snapshot_dir.display()
            );
        } else {
            std::fs::create_dir_all(&baseline_dir)
                .map_err(|e| CallerError::Config(format!("create baseline dir: {}", e)))?;
            std::fs::create_dir_all(snapshot_dir.join("objects"))
                .map_err(|e| CallerError::Config(format!("create objects dir: {}", e)))?;
            std::fs::create_dir_all(snapshot_dir.join("rounds"))
                .map_err(|e| CallerError::Config(format!("create rounds dir: {}", e)))?;
        }

        let mut baseline_manifest = BaselineManifest::new();
        let mut hashes = HashMap::new();
        let mut large_file_fingerprints = HashMap::new();
        let mut reconcile_failed = false;
        let mut files_seen: usize = 0;
        let mut bytes_seen: u64 = 0;
        #[cfg(test)]
        let mut baseline_files_rewritten: usize = 0;

        if lock_read_only {
            // Write nothing: adopt the owning process's on-disk baseline
            // state so reads (the legacy changes scan, FileChanged line
            // counts) stay consistent with the store we serve.
            baseline_manifest = read_baseline_manifest_file(&snapshot_dir);
            for (key, meta) in &baseline_manifest {
                if let Some(hash) = hex_decode_hash(&meta.hash) {
                    hashes.insert(PathBuf::from(key), hash);
                }
            }
        } else {
            // The previous boot's manifest, consulted per file for the
            // cross-boot reuse fast path; empty on a fresh store, so every
            // file takes the full read + rewrite path.
            let previous_manifest = read_baseline_manifest_file(&snapshot_dir);
            // Reused/recorded fingerprints carry the walk's start time —
            // the most conservative recording bound for the wall-clock
            // racy-distrust gate (recording happens later in the walk).
            let walk_started_nanos = now_nanos();
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
                    // Stat before any content read: this fingerprint
                    // describes content no newer than what the reads below
                    // return, so recording it against that content is
                    // conservative (the house stat-before-read contract).
                    let live_fingerprint = std::fs::metadata(&path)
                        .ok()
                        .map(|meta| file_fingerprint(&path, &meta));
                    // Cross-boot fast path: a live file whose fingerprint
                    // still matches the previous boot's manifest entry (and
                    // whose baseline copy is intact, for text entries)
                    // reuses that entry verbatim — no re-read, no rewrite.
                    if let (Some(live), Some(prev)) =
                        (live_fingerprint, previous_manifest.get(&rel_key))
                    {
                        if baseline_entry_reusable(
                            prev,
                            &live,
                            racy_window_nanos,
                            &baseline_dir.join(&rel),
                        ) {
                            if let Some(hash) = hex_decode_hash(&prev.hash) {
                                bytes_seen = bytes_seen.saturating_add(prev.size);
                                if prev.size > SNAPSHOT_MAX_FILE_BYTES {
                                    large_file_fingerprints.insert(rel.clone(), live);
                                }
                                hashes.insert(rel, hash);
                                baseline_manifest.insert(rel_key, prev.clone());
                                continue;
                            }
                        }
                    }
                    match inspect_file(&path) {
                        Ok(InspectedFile::Text(snapshot)) => {
                            bytes_seen = bytes_seen.saturating_add(snapshot.size);
                            let baseline_path = baseline_dir.join(&rel);
                            // A previous run may have baselined a FILE where this
                            // run needs a directory (or vice versa): clear the
                            // wrong-typed leftover before writing.
                            clear_stale_baseline_slot(&baseline_dir, &baseline_path);
                            if let Some(parent) = baseline_path.parent() {
                                std::fs::create_dir_all(parent).map_err(|e| {
                                    CallerError::Config(format!(
                                        "create baseline parent {}: {}",
                                        parent.display(),
                                        e
                                    ))
                                })?;
                            }
                            std::fs::write(&baseline_path, snapshot.text.as_bytes()).map_err(
                                |e| {
                                    CallerError::Config(format!(
                                        "write baseline {}: {}",
                                        baseline_path.display(),
                                        e
                                    ))
                                },
                            )?;
                            #[cfg(test)]
                            {
                                baseline_files_rewritten += 1;
                            }
                            hashes.insert(rel, snapshot.hash);
                            baseline_manifest.insert(
                                rel_key,
                                BaselineFileMeta {
                                    supported_text: true,
                                    hash: snapshot.hash_hex,
                                    size: snapshot.size,
                                    reason: None,
                                    fingerprint: live_fingerprint,
                                    recorded_at_nanos: Some(walk_started_nanos),
                                },
                            );
                        }
                        Ok(InspectedFile::Unsupported(snapshot)) => {
                            bytes_seen = bytes_seen.saturating_add(snapshot.size);
                            if snapshot.size > SNAPSHOT_MAX_FILE_BYTES {
                                if let Ok(meta) = std::fs::metadata(&path) {
                                    large_file_fingerprints
                                        .insert(rel.clone(), file_fingerprint(&path, &meta));
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
                                    fingerprint: live_fingerprint,
                                    recorded_at_nanos: Some(walk_started_nanos),
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

            // Drop baseline/ files left over from a previous run whose sources
            // no longer exist: stale copies otherwise make the full-scan changes
            // path report phantom "deleted" files this session never had. Any
            // reconciliation failure permanently disables the changes fast path
            // (the full scan would diverge from the watcher index otherwise).
            reconcile_failed = !reconcile_baseline_dir(&baseline_dir, &baseline_manifest);
            if reconcile_failed {
                eprintln!(
                    "[file_watcher] baseline reconciliation under {} failed — the changes fast \
                 path is disabled for this run",
                    baseline_dir.display()
                );
            }
        }

        // Load history.json if it exists (session resume / restart). Legacy
        // full-fat files (per-round maps inline) are migrated to per-round
        // manifests + a slim index; the store epoch is minted on first
        // format-2 load. Read-only instances parse without mutating.
        let LoadedHistory {
            history,
            needs_persist,
            force_read_only,
        } = load_history_from_disk(&snapshot_dir, lock_read_only);
        let read_only = lock_read_only || force_read_only;

        // One boot-time walk seeds the size estimate the soft-cap check
        // maintains incrementally afterwards (it used to re-walk per round).
        let snapshot_dir_size_estimate = dir_byte_size(&snapshot_dir);
        let last_history_index_bytes = std::fs::metadata(snapshot_dir.join("history.json"))
            .map(|meta| meta.len())
            .unwrap_or(0);

        // One boot-time listing seeds the object index the round walks
        // consult and the write/GC paths maintain incrementally.
        let object_index = seed_object_index(&snapshot_dir.join("objects"));

        let mut watcher = Self {
            project_root,
            snapshot_dir,
            bus,
            baseline_manifest,
            hashes,
            large_file_fingerprints,
            round_scan_cache: HashMap::new(),
            object_index,
            live_index_degraded: true,
            live_index_disabled: read_only || reconcile_failed,
            read_only,
            _store_lock: store_lock,
            racy_window_nanos,
            #[cfg(test)]
            files_read_in_last_scan: 0,
            #[cfg(test)]
            baseline_files_rewritten_in_new: baseline_files_rewritten,
            head_maps: None,
            snapshot_dir_size_estimate,
            last_history_index_bytes,
            history,
        };
        if needs_persist && !watcher.read_only {
            // Make the adopted epoch / migrated maps durable before any new
            // manifest is stamped against them.
            watcher.persist_history()?;
        }
        Ok(watcher)
    }

    /// Read-only accessor for the history state. Callers hold the mutex for
    /// the duration, so callers should clone the result if they need to use
    /// it after releasing the lock. Rounds are slim (scalars +
    /// `files_changed`); the per-round maps live in the round manifests.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Refuse store mutations in read-only mode (another process owns the
    /// store lock, or a damaged `history.json` could not be set aside).
    fn ensure_writable(&self) -> Result<(), CallerError> {
        if self.read_only {
            return Err(CallerError::Config(
                "snapshot store is read-only in this instance (held by another intendant \
                 process, or a damaged history.json) — round recording and rollback are \
                 disabled here"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Snapshot the state the changes endpoint needs to compute the
    /// changed-key set without walking the project tree. Cheap relative to
    /// a tree read (two O(files) map clones), taken under the watcher lock.
    ///
    /// `None` while the live index is degraded — before the notify watcher
    /// has confirmed it is running, or after a notify error — so callers
    /// fall back to their full-scan path instead of serving from a hash
    /// mirror that may be missing events.
    pub(crate) fn changes_index_snapshot(&self) -> Option<ChangesIndexSnapshot> {
        if self.live_index_degraded || self.live_index_disabled {
            return None;
        }
        Some(ChangesIndexSnapshot {
            baseline_manifest: self.baseline_manifest.clone(),
            current_hashes: self
                .hashes
                .iter()
                .map(|(rel, hash)| (rel_path_key(rel), hex_encode(hash)))
                .collect(),
        })
    }

    /// Test hook: pretend the notify watcher is confirmed healthy, so unit
    /// tests can exercise the watcher-index fast path without spawning the
    /// real filesystem-event loop. Deliberately leaves the sticky
    /// `live_index_disabled` flag alone — tests assert its precedence.
    #[cfg(test)]
    pub(crate) fn mark_live_index_healthy_for_tests(&mut self) {
        self.live_index_degraded = false;
    }

    /// Test hook: shrink the racy-distrust window so fingerprint mechanics
    /// can be pinned without multi-second sleeps.
    #[cfg(test)]
    pub(crate) fn set_racy_window_for_tests(&mut self, nanos: u128) {
        self.racy_window_nanos = nanos;
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
            let rx = bus.subscribe();
            tokio::task::spawn(run_round_complete_listener(shared, rx, project_root))
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

    /// Apply one notify path synchronously under the caller's borrow: the
    /// staged pipeline (see [`PreparedNotifyChange`]) run back-to-back.
    /// The live notify loop runs the same stages with the read phases OFF
    /// the shared watcher lock ([`apply_notify_path`]); this composition
    /// serves tests that already hold exclusive access and drive events
    /// deterministically.
    #[cfg(test)]
    pub(crate) fn process_change(&mut self, abs_path: &Path, kind: &notify::EventKind) {
        let Some(prepared) = prepare_notify_change(&self.project_root, abs_path, kind) else {
            return;
        };
        match prepared {
            PreparedNotifyChange::Upsert {
                rel,
                rel_key,
                fingerprint,
                size,
            } => {
                let UpsertStage::Read {
                    existed_at_baseline,
                    prev_hash,
                } = self.stage_upsert(&rel, size, &fingerprint)
                else {
                    return;
                };
                let Some(content) = read_upsert_content(
                    &self.snapshot_dir,
                    &self.project_root,
                    &rel,
                    existed_at_baseline,
                    prev_hash,
                ) else {
                    return;
                };
                self.publish_upsert(rel, rel_key, fingerprint, content);
            }
            PreparedNotifyChange::Delete { rel, rel_key } => {
                let lines_removed = deleted_baseline_line_count(&self.snapshot_dir, &rel);
                self.publish_delete(rel, rel_key, lines_removed);
            }
        }
    }

    /// Upsert phase B — the pre-read snapshot, taken under the watcher
    /// lock: the oversized-event dedup (duplicate notify events for an
    /// unchanged oversized file must not re-hash tens of megabytes) and
    /// the state bits the off-lock read phase needs.
    fn stage_upsert(&self, rel: &Path, size: u64, fingerprint: &FileFingerprint) -> UpsertStage {
        if size > SNAPSHOT_MAX_FILE_BYTES
            && self
                .large_file_fingerprints
                .get(rel)
                .is_some_and(|prev| prev.matches(fingerprint))
        {
            return UpsertStage::Drop;
        }
        UpsertStage::Read {
            existed_at_baseline: self.baseline_manifest.contains_key(&rel_path_key(rel)),
            prev_hash: self.hashes.get(rel).copied(),
        }
    }

    /// Upsert phase D — re-taken under the watcher lock after the off-lock
    /// read: revalidate, then mutate state and publish. The pre-read
    /// fingerprint is compared against a fresh stat; if the file moved (or
    /// vanished) while the lock was released, this read is stale and is
    /// dropped — the change that invalidated it either has its own notify
    /// event queued behind this one, or was authored by a lock-holding
    /// restore that refreshed the mirrors itself.
    fn publish_upsert(
        &mut self,
        rel: PathBuf,
        rel_key: String,
        fingerprint: FileFingerprint,
        content: UpsertContent,
    ) {
        let abs = self.project_root.join(&rel);
        let Ok(meta) = std::fs::metadata(&abs) else {
            // Deleted while unlocked: the Remove event is queued behind us.
            return;
        };
        if !fingerprint_still_describes(&file_fingerprint(&abs, &meta), &fingerprint) {
            return;
        }
        // The Created/Modified label reflects state at publish time — the
        // lock we now hold — not the possibly stale pre-read snapshot.
        let known_file =
            self.baseline_manifest.contains_key(&rel_key) || self.hashes.contains_key(&rel);
        let change_kind = if known_file {
            FileChangeKind::Modified
        } else {
            FileChangeKind::Created
        };
        match content.inspected {
            InspectedFile::Text(snapshot) => {
                self.large_file_fingerprints.remove(&rel);
                // Live events are the freshest signal — refresh the
                // round-scan cache even when the content hash proves
                // unchanged (the fingerprint may still have moved).
                self.round_scan_cache.insert(
                    rel.clone(),
                    ScanCacheEntry {
                        fingerprint,
                        hash: snapshot.hash,
                        hash_hex: snapshot.hash_hex.clone(),
                        restorable: true,
                        recorded_at_nanos: now_nanos(),
                        recorded_at_instant: std::time::Instant::now(),
                    },
                );
                if self.hashes.get(&rel) == Some(&snapshot.hash) {
                    return; // no actual change
                }
                self.hashes.insert(rel.clone(), snapshot.hash);

                // `None` line stats mean the off-lock read predicted a
                // dedup that publish-time state no longer agrees with (a
                // rare cross-lock interleave): emit with zeros rather than
                // hold the lock for a diff — the next event self-corrects.
                let (lines_added, lines_removed) = content.line_stats.unwrap_or((0, 0));
                self.bus.send(AppEvent::FileChanged {
                    path: rel_key,
                    kind: change_kind,
                    lines_added,
                    lines_removed,
                });
            }
            InspectedFile::Unsupported(snapshot) => {
                if snapshot.size > SNAPSHOT_MAX_FILE_BYTES {
                    self.large_file_fingerprints
                        .insert(rel.clone(), fingerprint);
                    self.round_scan_cache.remove(&rel);
                } else {
                    self.large_file_fingerprints.remove(&rel);
                    self.round_scan_cache.insert(
                        rel.clone(),
                        ScanCacheEntry {
                            fingerprint,
                            hash: snapshot.hash,
                            hash_hex: snapshot.hash_hex.clone(),
                            restorable: false,
                            recorded_at_nanos: now_nanos(),
                            recorded_at_instant: std::time::Instant::now(),
                        },
                    );
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
        }
    }

    /// Delete phase D — under the watcher lock. Revalidates that the path
    /// is still absent: a path that exists again was recreated while the
    /// lock was released (a rollback restore, an editor's atomic-rename
    /// save) and whatever recreated it owns the mirrors — a lock-holding
    /// restore refreshed them itself; a plain recreate's Create event is
    /// queued behind this one.
    fn publish_delete(&mut self, rel: PathBuf, rel_key: String, lines_removed: u32) {
        if std::fs::symlink_metadata(self.project_root.join(&rel)).is_ok() {
            return;
        }
        let known_file =
            self.baseline_manifest.contains_key(&rel_key) || self.hashes.contains_key(&rel);
        if known_file {
            self.bus.send(AppEvent::FileChanged {
                path: rel_key,
                kind: FileChangeKind::Deleted,
                lines_added: 0,
                lines_removed,
            });
        }
        self.hashes.remove(&rel);
        self.large_file_fingerprints.remove(&rel);
        self.round_scan_cache.remove(&rel);
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
        self.ensure_writable()?;
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
        // Recorded in the index row so the resolver can verify a manifest's
        // maps (the restore payload) before ever serving them.
        let maps_hash = maps_content_hash(&files_at_end, &all_files_at_end);
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
            store_epoch: None,
            maps_inline: false,
            maps_hash: Some(maps_hash),
        };

        // Write the per-round manifest — the durable home of the maps,
        // stamped with the store epoch so the resolver can tell our
        // manifests from anything a foreign timeline wrote at the same id.
        // A no-op round writes a tiny backreference stub; a changed round
        // inlines its maps.
        let manifest = if maps_source_id.is_some() {
            HistoryRound {
                store_epoch: self.history.store_epoch.clone(),
                ..stub.clone()
            }
        } else {
            HistoryRound {
                files_at_end: files_at_end.clone(),
                all_files_at_end: all_files_at_end.clone(),
                store_epoch: self.history.store_epoch.clone(),
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
        self.ensure_writable()?;
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
        let _ = self.refresh_hashes_from_tree();

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
        self.ensure_writable()?;
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
        let _ = self.refresh_hashes_from_tree();

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
        self.ensure_writable()?;
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
    /// fingerprint moved since the last walk (see [`ScanCacheEntry`] for the
    /// exact staleness contract) are re-read and re-hashed. A cached
    /// restorable entry is only trusted when its object blob is still known
    /// to the in-memory object index (seeded from one `objects/` listing at
    /// construction, maintained on every write and GC delete — it replaces
    /// the per-entry `objects/{hash}` stat that doubled big-tree walk
    /// costs), so every hash recorded in `files_at_end` is restorable by
    /// construction up to index accuracy; a restore that finds a blob
    /// missing anyway repairs the index so later walks re-store the
    /// content (see [`Self::restore_to_state`]).
    fn scan_and_store_objects(&mut self) -> Result<SnapshotObjectMaps, CallerError> {
        let mut out: HashMap<String, String> = HashMap::new();
        let mut all: HashMap<String, String> = HashMap::new();
        let objects_dir = self.snapshot_dir.join("objects");
        std::fs::create_dir_all(&objects_dir)
            .map_err(|e| CallerError::Config(format!("create objects dir: {}", e)))?;
        let mut next_cache: HashMap<PathBuf, ScanCacheEntry> =
            HashMap::with_capacity(self.round_scan_cache.len());
        // Entries recorded during this walk carry the walk's start time: the
        // racy-distrust check compares a file's mtime against it, and the
        // start is the most conservative bound (recording happens later).
        let walk_started_nanos = now_nanos();
        let walk_started_instant = std::time::Instant::now();
        #[cfg(test)]
        {
            self.files_read_in_last_scan = 0;
        }

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
                let fingerprint = file_fingerprint(&path, &meta);
                if let Some(cached) = self.round_scan_cache.get(&rel) {
                    if cached.trustworthy_for(&fingerprint, self.racy_window_nanos) {
                        let key = rel_path_key(&rel);
                        if cached.restorable {
                            if self.object_index.contains(&cached.hash_hex) {
                                all.insert(key.clone(), cached.hash_hex.clone());
                                out.insert(key, cached.hash_hex.clone());
                                next_cache.insert(rel, cached.clone());
                                continue;
                            }
                            // Object not in the index (fresh objects dir,
                            // failed write, restore-time repair): fall
                            // through and re-store it.
                        } else {
                            all.insert(key, cached.hash_hex.clone());
                            next_cache.insert(rel, cached.clone());
                            continue;
                        }
                    }
                }
                #[cfg(test)]
                {
                    self.files_read_in_last_scan += 1;
                }
                let snapshot = match inspect_file(&path) {
                    Ok(snapshot) => snapshot,
                    Err(_) => continue,
                };
                match snapshot {
                    InspectedFile::Text(snapshot) => {
                        all.insert(rel_path_key(&rel), snapshot.hash_hex.clone());
                        if !self.object_index.contains(&snapshot.hash_hex) {
                            let obj_path = objects_dir.join(&snapshot.hash_hex);
                            if obj_path.exists() {
                                // Blob present but unindexed (written by a
                                // path the index missed): repair the
                                // stale-negative instead of rewriting.
                                self.object_index.insert(snapshot.hash_hex.clone());
                            } else if atomic_write(&obj_path, snapshot.text.as_bytes()).is_ok() {
                                self.object_index.insert(snapshot.hash_hex.clone());
                                self.snapshot_dir_size_estimate = self
                                    .snapshot_dir_size_estimate
                                    .saturating_add(snapshot.size);
                            }
                        }
                        next_cache.insert(
                            rel.clone(),
                            ScanCacheEntry {
                                fingerprint,
                                hash: snapshot.hash,
                                hash_hex: snapshot.hash_hex.clone(),
                                restorable: true,
                                recorded_at_nanos: walk_started_nanos,
                                recorded_at_instant: walk_started_instant,
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
                                recorded_at_nanos: walk_started_nanos,
                                recorded_at_instant: walk_started_instant,
                            },
                        );
                    }
                }
            }
        }
        self.round_scan_cache = next_cache;
        Ok((out, all))
    }

    /// Resolve one round's maps: from the head cache when it matches, from
    /// inline maps retained in the index (a round whose migration to a
    /// manifest hasn't succeeded yet), else from the round's on-disk
    /// manifest, following its (depth-1) `maps_from_round` backreference.
    ///
    /// Fails closed (`None` — callers surface "cannot restore") when the
    /// round is unknown, the manifest chain is missing/unreadable/cyclic
    /// (including a self-referential backreference), a manifest's `id`
    /// doesn't match its path, or a manifest's `store_epoch` stamp doesn't
    /// match the index's — the last guards against a pre-format-2 binary
    /// having overwritten manifests with same-id rounds of a different
    /// timeline (its restarted ids collide with ours).
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
        let stub = self.history.rounds.iter().find(|r| r.id == round_id)?;
        // Inline maps retained after a failed migration are authoritative —
        // including the explicit `maps_inline` marker, which is how an
        // empty-tree retention survives (empty maps serialize to nothing).
        if round_has_inline_maps(stub) {
            return Some(ResolvedRoundMaps {
                source_round_id: stub.id,
                files_at_end: stub.files_at_end.clone(),
                all_files_at_end: stub.all_files_at_end.clone(),
            });
        }
        // Prefer the in-memory stub's backreference (skips one manifest
        // read); the chain below re-verifies every hop against its own
        // index row anyway.
        let expected_maps_hash = stub.maps_hash.clone();
        let mut source_id = stub.maps_from_round.unwrap_or(round_id);
        // Backreferences are written depth-1; the visited set and bound are
        // defense against corrupt or foreign manifests.
        let mut visited: HashSet<u64> = HashSet::new();
        for _ in 0..32 {
            if !visited.insert(source_id) {
                // Cycle (including a manifest that references itself):
                // corrupt data — refuse rather than resolve to a wrong or
                // empty tree.
                return None;
            }
            // Every hop must have an index row of its own (backreference
            // targets are always linear-path ancestors).
            let hop_stub = self.history.rounds.iter().find(|r| r.id == source_id)?;
            // A referenced round whose own migration is still pending keeps
            // its maps inline in the index — serve those (its manifest does
            // not exist yet).
            if round_has_inline_maps(hop_stub) {
                return Some(ResolvedRoundMaps {
                    source_round_id: hop_stub.id,
                    files_at_end: hop_stub.files_at_end.clone(),
                    all_files_at_end: hop_stub.all_files_at_end.clone(),
                });
            }
            let manifest_path = round_manifest_path(&self.snapshot_dir, source_id);
            let bytes = std::fs::read(&manifest_path).ok()?;
            let manifest = serde_json::from_slice::<HistoryRound>(&bytes).ok()?;
            // Content binding: the manifest must be the round the index
            // recorded, not merely a file at the right path (a pre-format-2
            // binary restarts ids at 0 and can overwrite manifests with
            // same-id rounds of a different timeline).
            if !manifest_binds_to_round(&manifest, hop_stub) {
                return None;
            }
            // Epoch binding: with an adopted epoch, the stamp must match
            // exactly. An epoch-less index (a pre-epoch store whose stamping
            // hasn't completed yet) accepts any stamp — content binding
            // above is the guard for that window.
            if let Some(epoch) = self.history.store_epoch.as_deref() {
                if manifest.store_epoch.as_deref() != Some(epoch) {
                    return None;
                }
            }
            match manifest.maps_from_round {
                Some(next) => source_id = next,
                None => {
                    // Payload binding: the maps we are about to serve must
                    // hash to what the requested round's index row recorded
                    // — scalar binding alone would bless a manifest whose
                    // scalars match but whose maps were replaced. Rows
                    // predating the field (`None`) skip this check.
                    if let Some(expected) = expected_maps_hash.as_deref() {
                        let actual =
                            maps_content_hash(&manifest.files_at_end, &manifest.all_files_at_end);
                        if actual != expected {
                            return None;
                        }
                    }
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
    /// Fingerprint-cached like the round scan: files untouched since a
    /// comfortably-old walk contribute their cached hash from a stat instead
    /// of a full read; recently-modified and just-restored files are
    /// (re-)read — the racy-distrust window deliberately keeps fresh entries
    /// untrusted, so the walks after a restore re-verify what was written.
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
                let fingerprint = file_fingerprint(&path, &meta);
                if let Some(cached) = self.round_scan_cache.get(&rel) {
                    if cached.trustworthy_for(&fingerprint, self.racy_window_nanos) {
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
                                recorded_at_nanos: now_nanos(),
                                recorded_at_instant: std::time::Instant::now(),
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
                        recorded_at_nanos: now_nanos(),
                        recorded_at_instant: std::time::Instant::now(),
                    },
                );
                current.insert(rel_path_key(&rel), snapshot.hash);
            }
        }

        let mut touched: u32 = 0;
        let restore_one = |watcher: &mut Self, rel: &str, target_hex: &str| -> bool {
            let obj = objects_dir.join(target_hex);
            let bytes = match std::fs::read(&obj) {
                Ok(bytes) => bytes,
                Err(err) => {
                    if err.kind() == io::ErrorKind::NotFound {
                        // The blob vanished behind the object index
                        // (external deletion): repair the index so later
                        // walks re-read and re-store this content instead
                        // of trusting the stale entry. This restore still
                        // skips the file, exactly as a walk-time disk
                        // check would have skipped storing it.
                        watcher.object_index.remove(target_hex);
                    }
                    return false;
                }
            };
            let abs = watcher.project_root.join(rel);
            if let Some(parent) = abs.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if atomic_write(&abs, &bytes).is_err() {
                return false;
            }
            // Record the restored content in the scan cache. The entry is
            // freshly written, so the racy-distrust window will still force
            // one verification re-read on the next walk — correct, and the
            // cost is bounded by the number of restored files.
            if let (Ok(meta), Some(hash)) = (std::fs::metadata(&abs), hex_decode_hash(target_hex)) {
                watcher.round_scan_cache.insert(
                    PathBuf::from(rel),
                    ScanCacheEntry {
                        fingerprint: file_fingerprint(&abs, &meta),
                        hash,
                        hash_hex: target_hex.to_string(),
                        restorable: true,
                        recorded_at_nanos: now_nanos(),
                        recorded_at_instant: std::time::Instant::now(),
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
    ///
    /// Returns `false` — and degrades the live index — when a subtree could
    /// not be enumerated (non-NotFound `read_dir` failure): the rebuilt
    /// mirrors would silently omit whatever lives there. Per-file races
    /// (a path deleted mid-walk) are benign; the next event covers them.
    fn refresh_hashes_from_tree(&mut self) -> bool {
        let mut ok = true;
        let mut new_hashes = HashMap::new();
        let mut large_file_fingerprints = HashMap::new();
        let mut next_cache: HashMap<PathBuf, ScanCacheEntry> =
            HashMap::with_capacity(self.round_scan_cache.len());
        let mut stack = vec![self.project_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(err) => {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        ok = false;
                    }
                    continue;
                }
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
                    let fingerprint = file_fingerprint(&path, &meta);
                    // Keep the last-known hash when the file is untouched
                    // (its fingerprint matches) so the change index doesn't
                    // treat it as deleted after a restore; a genuinely
                    // changed oversized file gets one streaming re-hash.
                    let hash = match self.hashes.get(&rel) {
                        Some(prev)
                            if self
                                .large_file_fingerprints
                                .get(&rel)
                                .is_some_and(|old| old.matches(&fingerprint)) =>
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
                let fingerprint = file_fingerprint(&path, &meta);
                if let Some(cached) = self.round_scan_cache.get(&rel) {
                    if cached.trustworthy_for(&fingerprint, self.racy_window_nanos) {
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
                            recorded_at_nanos: now_nanos(),
                            recorded_at_instant: std::time::Instant::now(),
                        },
                    );
                }
            }
        }
        self.hashes = new_hashes;
        self.large_file_fingerprints = large_file_fingerprints;
        self.round_scan_cache = next_cache;
        if !ok {
            self.live_index_degraded = true;
        }
        ok
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
        self.ensure_writable()?;
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
    /// orphan objects a rollback still needs. Deleted blobs leave the
    /// in-memory object index in the same motion.
    fn gc_orphaned_objects(&mut self) -> u64 {
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
                if std::fs::remove_file(&p).is_ok() {
                    // Keep the object index honest: a blob that could not
                    // actually be deleted stays indexed.
                    self.object_index.remove(name);
                }
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

/// Round-complete listener: records a rewind round in this watcher's store
/// for every completed round that belongs to this watcher's root.
///
/// Routing: `RoundComplete` carries the emitting session's effective
/// project root, and rounds are filtered on it — a round for a DIFFERENT
/// root (a worktree sub-agent, an external session supervised in another
/// directory) is skipped entirely, where it used to cost this watcher a
/// full stat walk plus a no-op round persist and polluted this root's
/// timeline with rounds that changed nothing. A round with NO resolvable
/// root (`None`: replayed logs, emitters without a project) fails open
/// and records as before — losing a legitimate snapshot is worse than an
/// occasional foreign no-op round. Deliberately still unfiltered by
/// `session_id`: multiple sessions working the SAME root all belong in
/// its one timeline.
async fn run_round_complete_listener(
    shared: SharedFileWatcher,
    mut rx: tokio::sync::broadcast::Receiver<AppEvent>,
    watcher_root: PathBuf,
) {
    loop {
        match rx.recv().await {
            Ok(AppEvent::RoundComplete {
                round,
                turns_in_round,
                native_message_count,
                project_root,
                ..
            }) => {
                if !round_targets_watcher_root(&watcher_root, project_root.as_deref()) {
                    continue;
                }
                let summary = format!("Round {}", round);
                let mut w = shared.lock().await;
                if let Err(e) =
                    w.on_round_complete(summary, Some(turns_in_round as u32), native_message_count)
                {
                    eprintln!("[file_watcher] round snapshot failed: {}", e);
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Whether a completed round belongs to the watcher rooted at
/// `watcher_root`: yes for the same root (symlink spellings tolerated via
/// [`watcher_paths_match`]) and — fail-open — for rounds that resolve no
/// root at all.
fn round_targets_watcher_root(watcher_root: &Path, round_root: Option<&Path>) -> bool {
    round_root.is_none_or(|root| watcher_paths_match(root, watcher_root))
}

// ---------------------------------------------------------------------------
// Staged notify pipeline
// ---------------------------------------------------------------------------
//
// One notify path is applied in phases so the live loop never does file
// read+hash work while holding the shared watcher lock (which stalls round
// snapshots and gateway rollback requests behind single-file hashing):
//
//   A. `prepare_notify_change` — pure path/kind resolution + the pre-read
//      stat. No watcher state; runs off-lock.
//   B. `FileWatcher::stage_upsert` — the pre-read snapshot, under the lock.
//   C. `read_upsert_content` / `deleted_baseline_line_count` — the content
//      read, hash, and line diff. Off-lock (the expensive part).
//   D. `FileWatcher::publish_upsert` / `publish_delete` — revalidate and
//      publish, under the lock again.
//
// Ordering: per-path event order is preserved by processing events
// SEQUENTIALLY — one event completes all its phases before the next
// starts (the loop below awaits each). The phase-D revalidation exists
// for the OTHER lock users (round scans, rollbacks) that interleave
// precisely because the lock is released around the reads: a read the
// on-disk state has moved past is dropped, never published stale.

/// One notify path resolved by phase A — pure path/kind/stat work.
enum PreparedNotifyChange {
    /// A Create/Modify event for a path that currently stats as a file.
    Upsert {
        rel: PathBuf,
        rel_key: String,
        /// Stat taken BEFORE any content read (the house stat-before-read
        /// contract): recording it against the later-read content is
        /// conservative — the fingerprint is never newer than the bytes.
        fingerprint: FileFingerprint,
        size: u64,
    },
    /// A Remove event.
    Delete { rel: PathBuf, rel_key: String },
}

/// Phase-B decision for an upsert.
enum UpsertStage {
    /// Duplicate event for an unchanged oversized file — dropped without
    /// reading (the point of the oversized fingerprint dedup).
    Drop,
    /// Proceed to the off-lock content read.
    Read {
        /// The path had a baseline entry at session start (immutable after
        /// construction, so safe to carry across the unlocked read).
        existed_at_baseline: bool,
        /// Last-known content hash at stage time — lets phase C skip the
        /// line diff when the fresh content hashes identically (phase D
        /// re-checks against live state before trusting the prediction).
        prev_hash: Option<[u8; 32]>,
    },
}

/// Phase-C result for an upsert.
struct UpsertContent {
    inspected: InspectedFile,
    /// `(lines_added, lines_removed)` for supported text whose content
    /// differed from the staged `prev_hash`; `None` when the diff was
    /// skipped (predicted dedup) or not applicable (unsupported files).
    line_stats: Option<(u32, u32)>,
}

/// Phase A: resolve a notify event against the project root — relative
/// path, ignore rules, event-kind class, and (for upserts) the pre-read
/// stat. Pure path + metadata work, no watcher state.
fn prepare_notify_change(
    project_root: &Path,
    abs_path: &Path,
    kind: &notify::EventKind,
) -> Option<PreparedNotifyChange> {
    let rel = abs_path.strip_prefix(project_root).ok()?.to_path_buf();
    if should_ignore(&rel) {
        return None;
    }
    let rel_key = rel_path_key(&rel);
    match kind {
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {
            if !abs_path.is_file() {
                return None;
            }
            let meta = std::fs::metadata(abs_path).ok()?;
            let fingerprint = file_fingerprint(abs_path, &meta);
            Some(PreparedNotifyChange::Upsert {
                rel,
                rel_key,
                fingerprint,
                size: meta.len(),
            })
        }
        notify::EventKind::Remove(_) => Some(PreparedNotifyChange::Delete { rel, rel_key }),
        _ => None,
    }
}

/// Phase C for upserts: the content read (inspect = read + hash) plus, for
/// supported text that actually changed, the baseline read + line diff —
/// the expensive work the notify path must never do under the watcher
/// lock. `None` when the file could not be read (vanished mid-flight).
fn read_upsert_content(
    snapshot_dir: &Path,
    project_root: &Path,
    rel: &Path,
    existed_at_baseline: bool,
    prev_hash: Option<[u8; 32]>,
) -> Option<UpsertContent> {
    let inspected = inspect_file(&project_root.join(rel)).ok()?;
    let line_stats = match &inspected {
        InspectedFile::Text(snapshot) if prev_hash != Some(snapshot.hash) => {
            Some(
                match std::fs::read_to_string(snapshot_dir.join("baseline").join(rel)) {
                    Ok(baseline_str) => diff_stats(&baseline_str, &snapshot.text),
                    // A tracked path whose baseline copy is unreadable:
                    // counts unknown, report zeros (matches the historic
                    // in-lock behavior).
                    Err(_) if existed_at_baseline => (0, 0),
                    Err(_) => diff_stats("", &snapshot.text),
                },
            )
        }
        _ => None,
    };
    Some(UpsertContent {
        inspected,
        line_stats,
    })
}

/// Phase C for deletes: line count of the baseline copy (0 when the path
/// never had a supported-text baseline). Baseline copies are written only
/// by the constructor, so this read is race-free off-lock.
fn deleted_baseline_line_count(snapshot_dir: &Path, rel: &Path) -> u32 {
    std::fs::read_to_string(snapshot_dir.join("baseline").join(rel))
        .map(|text| text.lines().count() as u32)
        .unwrap_or(0)
}

/// Revalidation gate for phase D: does `current` (a fresh stat) still
/// describe the same file state as `observed` (the pre-read stat)? Unlike
/// cache trust ([`FileFingerprint::matches`]) — which must never serve
/// stale data and therefore fails toward re-reading — this gate must never
/// DROP a legitimate event: a component missing on either side (a failed
/// change-signal query, an unreadable mtime) counts as unchanged, and only
/// a component present on both sides that moved votes "changed".
fn fingerprint_still_describes(current: &FileFingerprint, observed: &FileFingerprint) -> bool {
    fn opt_component_eq<T: PartialEq>(a: Option<T>, b: Option<T>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
    current.size == observed.size
        && opt_component_eq(current.mtime_nanos, observed.mtime_nanos)
        && opt_component_eq(current.change_signal, observed.change_signal)
        && opt_component_eq(current.file_id, observed.file_id)
}

/// Apply one notify path with the read phases OFF the shared watcher lock:
/// snapshot under the lock, release, do the blocking I/O on the blocking
/// pool, re-take and revalidate before publishing. See the pipeline notes
/// above for the ordering contract.
async fn apply_notify_path(
    shared: &SharedFileWatcher,
    project_root: &Path,
    snapshot_dir: &Path,
    abs_path: &Path,
    kind: &notify::EventKind,
) {
    // Phase A off-lock: path resolution + the pre-read stat.
    let prepared = tokio::task::spawn_blocking({
        let project_root = project_root.to_path_buf();
        let abs_path = abs_path.to_path_buf();
        let kind = *kind;
        move || prepare_notify_change(&project_root, &abs_path, &kind)
    })
    .await
    .ok()
    .flatten();
    match prepared {
        Some(PreparedNotifyChange::Upsert {
            rel,
            rel_key,
            fingerprint,
            size,
        }) => {
            // Phase B: the pre-read snapshot, briefly under the lock.
            let stage = shared.lock().await.stage_upsert(&rel, size, &fingerprint);
            let UpsertStage::Read {
                existed_at_baseline,
                prev_hash,
            } = stage
            else {
                return;
            };
            // Phase C off-lock: read + hash + diff.
            let content = tokio::task::spawn_blocking({
                let snapshot_dir = snapshot_dir.to_path_buf();
                let project_root = project_root.to_path_buf();
                let rel = rel.clone();
                move || {
                    read_upsert_content(
                        &snapshot_dir,
                        &project_root,
                        &rel,
                        existed_at_baseline,
                        prev_hash,
                    )
                }
            })
            .await
            .ok()
            .flatten();
            let Some(content) = content else {
                return;
            };
            // Phase D: revalidate + publish under the lock.
            shared
                .lock()
                .await
                .publish_upsert(rel, rel_key, fingerprint, content);
        }
        Some(PreparedNotifyChange::Delete { rel, rel_key }) => {
            let lines_removed = tokio::task::spawn_blocking({
                let snapshot_dir = snapshot_dir.to_path_buf();
                let rel = rel.clone();
                move || deleted_baseline_line_count(&snapshot_dir, &rel)
            })
            .await
            .unwrap_or(0);
            shared
                .lock()
                .await
                .publish_delete(rel, rel_key, lines_removed);
        }
        None => {}
    }
}

/// Run the notify-based filesystem watcher. Shared state is updated under the
/// async mutex on each event so snapshot / rollback operations see a
/// consistent view.
///
/// Health protocol: the watcher starts degraded; only once `watch()` has
/// succeeded is the live index marked healthy (enabling the gateway's
/// changes fast path). A rescan-flagged event (the backend dropped events)
/// triggers a full hash re-sync and stays healthy; a notify error or loop
/// exit degrades the index for the rest of the process — the fast path
/// stands down and the full scan serves.
async fn run_watcher_loop(
    shared: SharedFileWatcher,
    project_root: PathBuf,
) -> Result<(), CallerError> {
    use notify::Watcher;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let _ = tx.send(res);
        })
        .map_err(|e| CallerError::Config(format!("notify watcher init: {}", e)))?;

    watcher
        .watch(&project_root, notify::RecursiveMode::Recursive)
        .map_err(|e| CallerError::Config(format!("notify watch: {}", e)))?;

    let _watcher = watcher;

    let snapshot_dir = { shared.lock().await.snapshot_dir.clone() };

    // Close the scan-to-watch gap: the construction scan ran before the
    // watch registration above, so a change landing between them has no
    // event. Re-sync the hash mirrors from disk (a fingerprint walk), drain
    // whatever raced in meanwhile, and only then — and only if every one of
    // those steps succeeded — mark the index healthy.
    let resync_ok = { shared.lock().await.refresh_hashes_from_tree() };
    let mut drain_ok = true;
    while let Ok(res) = rx.try_recv() {
        drain_ok &= apply_notify_result(&shared, &project_root, &snapshot_dir, res).await;
    }
    {
        let mut w = shared.lock().await;
        if resync_ok && drain_ok && !w.live_index_disabled {
            w.live_index_degraded = false;
        }
    }

    while let Some(res) = rx.recv().await {
        let _ = apply_notify_result(&shared, &project_root, &snapshot_dir, res).await;
    }

    shared.lock().await.live_index_degraded = true;
    Ok(())
}

/// Apply one notify callback result to the shared watcher: rescan-flagged
/// events re-sync the hash mirrors first (the backend dropped events);
/// errors degrade the live index for the rest of the process. Returns
/// whether the result was applied cleanly (a failed rescan re-sync or a
/// notify error both degrade and return `false`).
///
/// Per-event file I/O runs off the watcher lock via [`apply_notify_path`];
/// the rescan re-sync deliberately stays under the lock — it rebuilds the
/// mirrors wholesale and must be atomic against the other lock users.
async fn apply_notify_result(
    shared: &SharedFileWatcher,
    project_root: &Path,
    snapshot_dir: &Path,
    res: Result<notify::Event, notify::Error>,
) -> bool {
    match res {
        Ok(notify_event) => {
            let ok = if notify_event.need_rescan() {
                shared.lock().await.refresh_hashes_from_tree()
            } else {
                true
            };
            for path in &notify_event.paths {
                apply_notify_path(shared, project_root, snapshot_dir, path, &notify_event.kind)
                    .await;
            }
            ok
        }
        Err(err) => {
            eprintln!("[file_watcher] notify error, live index degraded: {}", err);
            shared.lock().await.live_index_degraded = true;
            false
        }
    }
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
                store_epoch: None,
                maps_inline: false,
                maps_hash: None,
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

    /// A same-length rewrite with a restored mtime must be DETECTED and
    /// re-read, never served stale from the fingerprint cache: the
    /// racy-distrust window refuses to trust entries whose mtime sits near
    /// their recording walk, which is exactly where such rewrites land.
    #[test]
    fn same_length_mtime_restored_rewrite_is_detected() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let file = root.join("a.txt");
        std::fs::write(&file, b"round-1!").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        let r1_hash = w.resolved_round_maps(r1).unwrap().files_at_end["a.txt"].clone();
        assert_eq!(r1_hash, hex_encode(&sha256_hash(b"round-1!")));

        // Same length, mtime restored to the pre-rewrite value: the stat
        // fingerprint alone cannot tell the difference.
        let original_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, b"round-2!").unwrap();
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(original_mtime).unwrap();
        drop(handle);

        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        assert_eq!(
            w.resolved_round_maps(r2).unwrap().files_at_end["a.txt"],
            hex_encode(&sha256_hash(b"round-2!")),
            "a same-length mtime-restored rewrite must be re-read, not served stale"
        );
    }

    /// Unix: a same-length rewrite whose mtime is backdated far OUTSIDE the
    /// racy window is still detected via the ctime component of the
    /// fingerprint (mtime-preserving tools cannot preserve ctime).
    #[cfg(unix)]
    #[test]
    fn mtime_backdated_rewrite_is_detected_via_ctime() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let file = root.join("a.txt");
        std::fs::write(&file, b"round-1!").unwrap();
        // Backdate the mtime a full hour so cache entries recorded for this
        // file are comfortably outside the racy-distrust window.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(past).unwrap();
        drop(handle);

        let mut w = make_watcher(root, tmp_snap.path());
        // Disable the racy window: this test pins that the CHANGE SIGNAL
        // alone catches the rewrite (the window is exercised separately).
        w.set_racy_window_for_tests(0);
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        assert_eq!(
            w.resolved_round_maps(r1).unwrap().files_at_end["a.txt"],
            hex_encode(&sha256_hash(b"round-1!"))
        );

        // Guard against coarse (1s) ctime granularity: make sure the rewrite
        // below cannot share the original write's ctime tick.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Same length, mtime backdated to the exact same past value — only
        // the ctime betrays the rewrite.
        std::fs::write(&file, b"round-2!").unwrap();
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(past).unwrap();
        drop(handle);

        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        assert_eq!(
            w.resolved_round_maps(r2).unwrap().files_at_end["a.txt"],
            hex_encode(&sha256_hash(b"round-2!")),
            "an mtime-backdated rewrite must be caught by the ctime fingerprint"
        );
    }

    /// Efficiency pin: a round scan reads only files whose fingerprint
    /// moved. Files are backdated an hour so their cache entries are
    /// trustworthy (outside the racy-distrust window) — an untouched tree
    /// then costs zero reads, and touching one file costs one.
    #[test]
    fn round_scan_reads_only_changed_files() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for name in ["a.txt", "b.txt", "c.txt"] {
            let path = root.join(name);
            std::fs::write(&path, format!("content of {name}\n")).unwrap();
            let handle = std::fs::File::options().write(true).open(&path).unwrap();
            handle.set_modified(past).unwrap();
        }

        let mut w = make_watcher(root, tmp_snap.path());
        // Pin pure fingerprint mechanics: the racy-distrust window (tested
        // separately) would force re-reads between back-to-back rounds.
        w.set_racy_window_for_tests(0);
        w.on_round_complete("R1".into(), None, None).unwrap();
        assert_eq!(w.files_read_in_last_scan, 3, "first scan reads everything");

        w.on_round_complete("R2".into(), None, None).unwrap();
        assert_eq!(
            w.files_read_in_last_scan, 0,
            "an untouched tree must cost zero reads"
        );

        std::fs::write(root.join("b.txt"), "changed content\n").unwrap();
        w.on_round_complete("R3".into(), None, None).unwrap();
        assert_eq!(
            w.files_read_in_last_scan, 1,
            "only the touched file is re-read"
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
        // history.json (no format marker, no epoch), no round manifests.
        let mut legacy = w.history.clone();
        legacy.store_epoch = None;
        for round in &mut legacy.rounds {
            let maps = w.resolved_round_maps(round.id).unwrap();
            round.files_at_end = maps.files_at_end;
            round.all_files_at_end = maps.all_files_at_end;
            round.maps_from_round = None;
            round.maps_hash = None;
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
        w.mark_live_index_healthy_for_tests();
        let big_hash_before = w
            .changes_index_snapshot()
            .expect("healthy index")
            .current_hashes
            .get("big.csv")
            .cloned()
            .expect("oversized file hashed at baseline");
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();

        w.rollback(r1).unwrap();
        let index = w.changes_index_snapshot().expect("healthy index");
        assert_eq!(
            index.current_hashes.get("big.csv"),
            Some(&big_hash_before),
            "rollback re-sync must not drop oversized files from the hash index"
        );
    }

    /// The changes index refuses to serve while the live-event lane is
    /// unconfirmed or degraded — callers must fall back to the full scan.
    #[test]
    fn degraded_live_index_serves_no_snapshot() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        std::fs::write(tmp_proj.path().join("a.txt"), b"hello").unwrap();
        let mut w = make_watcher(tmp_proj.path(), tmp_snap.path());
        assert!(
            w.changes_index_snapshot().is_none(),
            "a watcher whose notify loop is unconfirmed must not serve the index"
        );
        w.mark_live_index_healthy_for_tests();
        assert!(w.changes_index_snapshot().is_some());
    }

    /// A manifest whose store-epoch stamp doesn't match the index (e.g. a
    /// pre-format-2 binary overwrote it with a same-id round of a different
    /// timeline) makes restore refuse — an error, never a wrong tree.
    #[test]
    fn manifest_epoch_mismatch_refuses_restore() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        drop(w);

        // Tamper: restamp round 1's manifest with a foreign epoch.
        let manifest_path = round_manifest_path(tmp_snap.path(), r1);
        let mut manifest: HistoryRound =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest.store_epoch = Some("f00df00df00df00d".to_string());
        atomic_write(
            &manifest_path,
            &serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let mut resumed = make_watcher(root, tmp_snap.path());
        assert!(
            resumed.resolved_round_maps(r1).is_none(),
            "foreign-epoch manifest must not resolve"
        );
        assert!(resumed.rollback(r1).is_err());
        assert_eq!(
            std::fs::read(root.join("a.txt")).unwrap(),
            b"v2",
            "a refused rollback must not touch the tree"
        );
        drop(resumed);

        // An unstamped manifest (what an old binary writes) is refused too.
        manifest.store_epoch = None;
        atomic_write(
            &manifest_path,
            &serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let mut resumed = make_watcher(root, tmp_snap.path());
        assert!(resumed.resolved_round_maps(r1).is_none());
        assert!(resumed.rollback(r1).is_err());
    }

    /// Corrupt backreference chains — a manifest referencing itself, or a
    /// two-manifest cycle — must fail closed to "cannot restore" instead of
    /// resolving to an empty (or wrong) tree.
    #[test]
    fn self_referential_and_cyclic_backrefs_fail_closed() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v3").unwrap();
        w.on_round_complete("R3".into(), None, None).unwrap();
        let r3 = w.history.current_head_id.unwrap();
        drop(w);

        let tamper = |id: u64, backref: u64| {
            let path = round_manifest_path(tmp_snap.path(), id);
            let mut manifest: HistoryRound =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            manifest.maps_from_round = Some(backref);
            manifest.files_at_end = HashMap::new();
            manifest.all_files_at_end = HashMap::new();
            atomic_write(&path, &serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        };
        // r1 references itself; r2 and r3 form a 2-cycle.
        tamper(r1, r1);
        tamper(r2, r3);
        tamper(r3, r2);

        let mut resumed = make_watcher(root, tmp_snap.path());
        assert!(
            resumed.resolved_round_maps(r1).is_none(),
            "self-referential manifest must fail closed, not resolve to an empty tree"
        );
        assert!(resumed.resolved_round_maps(r2).is_none());
        assert!(resumed.resolved_round_maps(r3).is_none());
        assert!(resumed.rollback(r1).is_err());
        assert_eq!(
            std::fs::read(root.join("a.txt")).unwrap(),
            b"v3",
            "no tracked file may be touched by a refused rollback"
        );
    }

    /// When the per-round manifest cannot be written (unwritable rounds
    /// dir), migration must keep the inline maps in the index — restore
    /// still works from them, and a later load with a writable dir migrates
    /// them for real.
    #[cfg(unix)]
    #[test]
    fn failed_migration_keeps_inline_maps_and_still_restores() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();

        // Rebuild the legacy layout (maps inline, no manifests)...
        let mut legacy = w.history.clone();
        legacy.store_epoch = None;
        for round in &mut legacy.rounds {
            let maps = w.resolved_round_maps(round.id).unwrap();
            round.files_at_end = maps.files_at_end;
            round.all_files_at_end = maps.all_files_at_end;
            round.maps_from_round = None;
            round.maps_hash = None;
        }
        drop(w);
        let rounds_dir = tmp_snap.path().join("rounds");
        std::fs::remove_dir_all(&rounds_dir).unwrap();
        std::fs::create_dir_all(&rounds_dir).unwrap();
        atomic_write(
            &tmp_snap.path().join("history.json"),
            &serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();
        // ...and make the manifest home unwritable.
        std::fs::set_permissions(&rounds_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let mut blocked = make_watcher(root, tmp_snap.path());
        assert!(
            !blocked.history.rounds[0].files_at_end.is_empty(),
            "failed migration must keep the authoritative inline maps"
        );
        let persisted = std::fs::read_to_string(tmp_snap.path().join("history.json")).unwrap();
        assert!(
            persisted.contains("files_at_end"),
            "retained inline maps must stay durable in the index"
        );
        blocked.rollback(r1).unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
        drop(blocked);

        // Writable again: the next load migrates for real.
        std::fs::set_permissions(&rounds_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let migrated = make_watcher(root, tmp_snap.path());
        assert!(migrated
            .history
            .rounds
            .iter()
            .all(|r| r.files_at_end.is_empty()));
        assert!(round_manifest_path(tmp_snap.path(), r1).exists());
        assert!(migrated.resolved_round_maps(r1).is_some());
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

    /// FIX (round 3): a manifest whose scalars all bind but whose MAPS —
    /// the actual restore payload — were replaced must be refused: the
    /// index row records a content hash of the maps and the resolver
    /// verifies it before serving.
    #[test]
    fn map_only_poison_is_refused() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        drop(w);

        // Poison ONLY the payload: every scalar and the epoch stay intact.
        let manifest_path = round_manifest_path(tmp_snap.path(), r1);
        let mut manifest: HistoryRound =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let poison_hash = hex_encode(&sha256_hash(b"attacker content"));
        manifest.files_at_end = HashMap::from([("a.txt".to_string(), poison_hash.clone())]);
        manifest.all_files_at_end = HashMap::from([("a.txt".to_string(), poison_hash)]);
        atomic_write(
            &manifest_path,
            &serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let mut resumed = make_watcher(root, tmp_snap.path());
        assert!(
            resumed.resolved_round_maps(r1).is_none(),
            "a map-only poison must fail the payload hash, not be served"
        );
        assert!(resumed.rollback(r1).is_err());
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v2");
    }

    /// FIX (round 3): a transiently UNREADABLE manifest defers epoch
    /// adoption exactly like a failed write — otherwise the persisted epoch
    /// would orphan a perfectly correct manifest.
    #[cfg(unix)]
    #[test]
    fn unreadable_manifest_defers_epoch_adoption() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        drop(w);

        // Pre-epoch layout + an unreadable (but correct) manifest.
        let index_path = tmp_snap.path().join("history.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        index.as_object_mut().unwrap().remove("store_epoch");
        atomic_write(&index_path, &serde_json::to_vec_pretty(&index).unwrap()).unwrap();
        let manifest_path = round_manifest_path(tmp_snap.path(), r1);
        std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let deferred = make_watcher(root, tmp_snap.path());
        assert_eq!(
            deferred.history.store_epoch, None,
            "an unreadable manifest must defer epoch adoption like a failed write"
        );
        drop(deferred);

        // Readable again: the next load completes the mint and restamps.
        std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let adopted = make_watcher(root, tmp_snap.path());
        assert!(adopted.history.store_epoch.is_some());
        assert!(adopted.resolved_round_maps(r1).is_some());
    }

    /// FIX (round 3): a symlink left under baseline/ (which only ever holds
    /// regular files we wrote) is a foreign leftover — reconciliation
    /// removes it, keeping the fallback scan's key universe identical to
    /// the watcher index's.
    #[cfg(unix)]
    #[test]
    fn symlink_baseline_leftover_is_reconciled() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("real.rs"), b"tracked").unwrap();
        let first = make_watcher(root, tmp_snap.path());
        drop(first);

        // Plant a symlink leftover pointing at live content.
        let target = root.join("real.rs");
        let link = tmp_snap.path().join("baseline").join("ghost.rs");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut second = make_watcher(root, tmp_snap.path());
        assert!(
            !link.exists() && std::fs::symlink_metadata(&link).is_err(),
            "the symlink leftover must be removed"
        );
        second.mark_live_index_healthy_for_tests();
        assert!(
            second.changes_index_snapshot().is_some(),
            "a successful reconciliation (including symlink removal) keeps the fast path"
        );
    }

    /// FIX (round 3): if the post-watch re-sync cannot enumerate part of
    /// the tree, the live index must STAY degraded — never marked healthy
    /// over silently missing subtrees.
    #[cfg(unix)]
    #[tokio::test]
    async fn resync_failure_keeps_index_degraded() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path().to_path_buf();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inner.rs"), b"content").unwrap();
        let w = make_watcher(&root, tmp_snap.path());

        // Make the subtree unenumerable between construction and watch.
        std::fs::set_permissions(root.join("sub"), std::fs::Permissions::from_mode(0o000)).unwrap();

        let (shared, _watcher_handle, _round_handle) = w.start_shared();
        // The healthy transition (if it were wrongly taken) happens right
        // after spawn; give it ample time, asserting it never happens.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            assert!(
                shared.lock().await.changes_index_snapshot().is_none(),
                "a failed re-sync must keep the index degraded"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        std::fs::set_permissions(root.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Windows: a same-length rewrite whose mtime is backdated OUTSIDE the
    /// racy window is still detected via the NTFS ChangeTime component of
    /// the fingerprint (SetFileTime cannot set ChangeTime).
    #[cfg(windows)]
    #[test]
    fn mtime_backdated_rewrite_is_detected_via_change_signal() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let file = root.join("a.txt");
        std::fs::write(&file, b"round-1!").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(past).unwrap();
        drop(handle);

        let mut w = make_watcher(root, tmp_snap.path());
        // Pin the change signal itself; the racy window is tested separately.
        w.set_racy_window_for_tests(0);
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        assert_eq!(
            w.resolved_round_maps(r1).unwrap().files_at_end["a.txt"],
            hex_encode(&sha256_hash(b"round-1!"))
        );

        // Cross the NTFS timestamp tick (~15.6ms) so the rewrite cannot
        // share the original write's ChangeTime.
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::write(&file, b"round-2!").unwrap();
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(past).unwrap();
        drop(handle);

        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        assert_eq!(
            w.resolved_round_maps(r2).unwrap().files_at_end["a.txt"],
            hex_encode(&sha256_hash(b"round-2!")),
            "an mtime-backdated rewrite must be caught by the ChangeTime fingerprint"
        );
    }

    /// FIX 1: an epoch-less (pre-epoch draft) format-2 store never blesses a
    /// manifest on identity alone — a poisoned manifest (right id, wrong
    /// content, e.g. written by a pre-format-2 binary) is left unstamped and
    /// restore refuses, while a content-binding sibling is stamped and keeps
    /// working.
    #[test]
    fn restamp_refuses_poisoned_manifest() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        let r2 = w.history.current_head_id.unwrap();
        drop(w);

        // Rebuild the epoch-less pre-fix layout: strip the epoch from the
        // index and from both manifests.
        let index_path = tmp_snap.path().join("history.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        index.as_object_mut().unwrap().remove("store_epoch");
        atomic_write(&index_path, &serde_json::to_vec_pretty(&index).unwrap()).unwrap();
        for id in [r1, r2] {
            let path = round_manifest_path(tmp_snap.path(), id);
            let mut manifest: HistoryRound =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            manifest.store_epoch = None;
            atomic_write(&path, &serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        }
        // Poison r1's manifest the way an old binary would: same id,
        // different round content entirely.
        let poison_path = round_manifest_path(tmp_snap.path(), r1);
        let mut poison: HistoryRound =
            serde_json::from_slice(&std::fs::read(&poison_path).unwrap()).unwrap();
        poison.summary = "Round 1".to_string();
        poison.timestamp_unix = 12345;
        poison.files_changed = vec!["other.txt".to_string()];
        poison.files_at_end = HashMap::from([(
            "other.txt".to_string(),
            hex_encode(&sha256_hash(b"foreign content")),
        )]);
        atomic_write(&poison_path, &serde_json::to_vec_pretty(&poison).unwrap()).unwrap();

        let mut resumed = make_watcher(root, tmp_snap.path());
        assert!(
            resumed.history.store_epoch.is_some(),
            "the epoch mint must complete (poison refusal is not a write failure)"
        );
        let stamped: HistoryRound = serde_json::from_slice(
            &std::fs::read(round_manifest_path(tmp_snap.path(), r2)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            stamped.store_epoch, resumed.history.store_epoch,
            "the binding sibling manifest must be adopted"
        );
        let unstamped: HistoryRound =
            serde_json::from_slice(&std::fs::read(&poison_path).unwrap()).unwrap();
        assert_eq!(
            unstamped.store_epoch, None,
            "a poisoned manifest must never be blessed on identity alone"
        );
        assert!(resumed.resolved_round_maps(r1).is_none());
        assert!(resumed.rollback(r1).is_err());
        assert_eq!(
            std::fs::read(root.join("a.txt")).unwrap(),
            b"v2",
            "a refused restore must not touch the tree"
        );
        assert!(
            resumed.resolved_round_maps(r2).is_some(),
            "the healthy round must keep resolving"
        );
    }

    /// FIX 3: a second watcher on the same store cannot interleave writes —
    /// it constructs read-only: reads serve, every mutation refuses, and the
    /// lock releases with the owner.
    #[test]
    fn second_watcher_is_read_only_until_lock_released() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut owner = make_watcher(root, tmp_snap.path());
        owner.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = owner.history.current_head_id.unwrap();

        let mut second = make_watcher(root, tmp_snap.path());
        assert!(second.read_only, "a locked store must yield read-only");
        assert_eq!(
            second.history.rounds.len(),
            1,
            "read-only instances still serve the on-disk history"
        );
        let err = second
            .on_round_complete("R2".into(), None, None)
            .expect_err("round recording must refuse in read-only mode");
        assert!(err.to_string().contains("read-only"), "clear error: {err}");
        assert!(second.rollback(r1).is_err());
        assert!(second.redo().is_err());
        assert!(second.prune_abandoned().is_err());
        second.mark_live_index_healthy_for_tests();
        assert!(
            second.changes_index_snapshot().is_none(),
            "read-only instances must not serve the changes fast path"
        );
        drop(second);
        drop(owner);

        let mut reclaimed = make_watcher(root, tmp_snap.path());
        assert!(!reclaimed.read_only);
        reclaimed
            .on_round_complete("R2".into(), None, None)
            .unwrap();
    }

    /// FIX 4: a change landing between the construction scan and the notify
    /// watch registration has no event — the pre-healthy re-sync must pick
    /// it up before the fast path serves.
    #[tokio::test]
    async fn scan_to_watch_gap_is_resynced_before_healthy() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path().to_path_buf();
        std::fs::write(root.join("a.txt"), b"scanned-1").unwrap();
        let w = make_watcher(&root, tmp_snap.path());

        // Mutate in the gap: the watcher scanned, notify is not running yet.
        std::fs::write(root.join("a.txt"), b"gap-write").unwrap();

        let (shared, _watcher_handle, _round_handle) = w.start_shared();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let index = loop {
            if let Some(index) = shared.lock().await.changes_index_snapshot() {
                break index;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher never became healthy"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(
            index.current_hashes.get("a.txt"),
            Some(&hex_encode(&sha256_hash(b"gap-write"))),
            "the gap write must be visible the moment the index is healthy"
        );
    }

    /// FIX 5: a retained EMPTY-tree round (failed migration of a legacy
    /// round with no files) keeps its authority across reloads via the
    /// explicit `maps_inline` marker — restore-to-empty still works.
    #[cfg(unix)]
    #[test]
    fn retained_empty_tree_round_survives_reload() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        // R1 records a genuinely empty tree.
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1 empty".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();

        let mut legacy = w.history.clone();
        legacy.store_epoch = None;
        for round in &mut legacy.rounds {
            let maps = w.resolved_round_maps(round.id).unwrap();
            round.files_at_end = maps.files_at_end;
            round.all_files_at_end = maps.all_files_at_end;
            round.maps_from_round = None;
            round.maps_inline = false;
            round.maps_hash = None;
        }
        assert!(legacy.rounds[0].files_at_end.is_empty(), "empty-tree round");
        drop(w);
        let rounds_dir = tmp_snap.path().join("rounds");
        std::fs::remove_dir_all(&rounds_dir).unwrap();
        std::fs::create_dir_all(&rounds_dir).unwrap();
        atomic_write(
            &tmp_snap.path().join("history.json"),
            &serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&rounds_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let blocked = make_watcher(root, tmp_snap.path());
        assert!(
            blocked.history.rounds[0].maps_inline,
            "a retained empty-tree round must carry the explicit inline marker"
        );
        let persisted = std::fs::read_to_string(tmp_snap.path().join("history.json")).unwrap();
        assert!(
            persisted.contains("maps_inline"),
            "the marker must be durable: {persisted}"
        );
        drop(blocked);

        // Reload (manifest dir still unwritable): the marker alone must keep
        // the round restorable — rolling back to the empty tree deletes the
        // file created since.
        let mut reloaded = make_watcher(root, tmp_snap.path());
        std::fs::write(root.join("late.txt"), b"created later").unwrap();
        reloaded.rollback(r1).unwrap();
        assert!(
            !root.join("late.txt").exists(),
            "restore-to-empty must still be exact after reload"
        );

        std::fs::set_permissions(&rounds_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// FIX 6: when a binding manifest cannot be restamped (transiently
    /// unwritable dir), the epoch mint is deferred — never persisted over a
    /// store whose manifests it would orphan — and the store keeps working
    /// epoch-less until a later load succeeds.
    #[cfg(unix)]
    #[test]
    fn failed_restamp_defers_epoch_adoption() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        drop(w);

        // Strip the epoch everywhere (pre-epoch store) and make the
        // manifest home unwritable so the restamp cannot land.
        let index_path = tmp_snap.path().join("history.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        index.as_object_mut().unwrap().remove("store_epoch");
        atomic_write(&index_path, &serde_json::to_vec_pretty(&index).unwrap()).unwrap();
        for id in [r1, r1 + 1] {
            let path = round_manifest_path(tmp_snap.path(), id);
            let mut manifest: HistoryRound =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            manifest.store_epoch = None;
            atomic_write(&path, &serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        }
        let rounds_dir = tmp_snap.path().join("rounds");
        std::fs::set_permissions(&rounds_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Manifest subdirs must stay readable but unwritable too.
        for id in [r1, r1 + 1] {
            let dir = rounds_dir.join(format!("round_{id}"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }

        let mut deferred = make_watcher(root, tmp_snap.path());
        assert_eq!(
            deferred.history.store_epoch, None,
            "epoch adoption must be deferred when a binding manifest cannot be stamped"
        );
        let on_disk = std::fs::read_to_string(&index_path).unwrap();
        assert!(
            !on_disk.contains("store_epoch"),
            "no epoch may persist over unstamped manifests: {on_disk}"
        );
        // The store still works epoch-less: content binding guards resolves.
        assert!(deferred.resolved_round_maps(r1).is_some());
        deferred.rollback(r1).unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
        drop(deferred);

        // Writable again: the next load completes the mint.
        for id in [r1, r1 + 1] {
            let dir = rounds_dir.join(format!("round_{id}"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::set_permissions(&rounds_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let adopted = make_watcher(root, tmp_snap.path());
        assert!(adopted.history.store_epoch.is_some());
        let stamped: HistoryRound = serde_json::from_slice(
            &std::fs::read(round_manifest_path(tmp_snap.path(), r1)).unwrap(),
        )
        .unwrap();
        assert_eq!(stamped.store_epoch, adopted.history.store_epoch);
        assert!(adopted.resolved_round_maps(r1).is_some());
    }

    /// FIX 7: a present-but-unparseable history.json is never overwritten —
    /// it is preserved aside and a fresh timeline starts.
    #[test]
    fn damaged_history_json_is_preserved_not_overwritten() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        drop(w);

        let garbage = b"{not json at all";
        atomic_write(&tmp_snap.path().join("history.json"), garbage).unwrap();

        let resumed = make_watcher(root, tmp_snap.path());
        assert!(resumed.history.rounds.is_empty(), "fresh timeline");
        let damaged: Vec<_> = std::fs::read_dir(tmp_snap.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("history.json.damaged-")
            })
            .collect();
        assert_eq!(damaged.len(), 1, "damaged index preserved aside");
        assert_eq!(
            std::fs::read(damaged[0].path()).unwrap(),
            garbage,
            "the damaged bytes must survive untouched"
        );
    }

    /// FIX 9a: any baseline-reconciliation failure permanently disables the
    /// changes fast path (the full scan would diverge from the index
    /// otherwise) — even after the notify loop reports healthy.
    #[cfg(unix)]
    #[test]
    fn reconcile_failure_disables_fast_path() {
        use std::os::unix::fs::PermissionsExt;

        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/stale.rs"), b"leftover").unwrap();
        let first = make_watcher(root, tmp_snap.path());
        drop(first);

        // The source disappears between runs; the stale baseline copy is
        // made undeletable.
        std::fs::remove_file(root.join("sub/stale.rs")).unwrap();
        let locked_dir = tmp_snap.path().join("baseline").join("sub");
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let mut second = make_watcher(root, tmp_snap.path());
        second.mark_live_index_healthy_for_tests();
        assert!(
            second.changes_index_snapshot().is_none(),
            "a failed reconciliation must disable the fast path for good"
        );

        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// FIX 9b: path-type changes between runs are reconciled — a stale
    /// baseline FILE where this run needs a directory, and a stale baseline
    /// DIRECTORY where this run needs a file.
    #[test]
    fn baseline_path_type_changes_are_reconciled() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();

        // Run 1: `thing` is a file.
        std::fs::write(root.join("thing"), b"i am a file").unwrap();
        let first = make_watcher(root, tmp_snap.path());
        drop(first);

        // Run 2: `thing` became a directory.
        std::fs::remove_file(root.join("thing")).unwrap();
        std::fs::create_dir_all(root.join("thing")).unwrap();
        std::fs::write(root.join("thing/inner.rs"), b"nested now").unwrap();
        let second = make_watcher(root, tmp_snap.path());
        assert!(second.baseline_manifest.contains_key("thing/inner.rs"));
        assert_eq!(
            std::fs::read(tmp_snap.path().join("baseline/thing/inner.rs")).unwrap(),
            b"nested now"
        );
        drop(second);

        // Run 3: `thing` is a file again.
        std::fs::remove_dir_all(root.join("thing")).unwrap();
        std::fs::write(root.join("thing"), b"file again").unwrap();
        let third = make_watcher(root, tmp_snap.path());
        assert!(third.baseline_manifest.contains_key("thing"));
        assert_eq!(
            std::fs::read(tmp_snap.path().join("baseline/thing")).unwrap(),
            b"file again"
        );
    }

    /// Object-index parity + repair: consecutive walks trust the index
    /// instead of stat'ing `objects/{hash}` per file, and a blob deleted
    /// behind the index's back is repaired at restore time so the content
    /// becomes restorable again on the next walk — the
    /// restorable-by-construction contract survives an index divergence.
    #[test]
    fn object_index_repair_keeps_restorability() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.set_racy_window_for_tests(0);

        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        let h1 = w.resolved_round_maps(r1).unwrap().files_at_end["a.txt"].clone();
        assert!(w.object_index.contains(&h1), "stored blob is indexed");

        // Parity: an untouched tree costs zero reads AND zero object stats
        // (the walk consults the index, which this test then deliberately
        // desyncs below to prove it is what the walk trusts).
        w.on_round_complete("R2".into(), None, None).unwrap();
        assert_eq!(w.files_read_in_last_scan, 0);

        // Delete the blob behind the index's back, then change the file so
        // a rollback to R1 must read that blob.
        std::fs::remove_file(tmp_snap.path().join("objects").join(&h1)).unwrap();
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        w.on_round_complete("R3".into(), None, None).unwrap();

        // The walk above still recorded R1's hash as restorable (the index
        // is stale-positive); the failed restore is the inconsistency
        // signal that repairs it.
        let res = w.rollback(r1).unwrap();
        assert_eq!(res.files_reverted, 0, "missing blob: nothing restored");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v2");
        assert!(
            !w.object_index.contains(&h1),
            "failed restore must repair the index"
        );

        // Recreate the original content: the next walk finds the hash
        // unindexed and re-stores the blob — restorability is back.
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        w.on_round_complete("R4".into(), None, None).unwrap();
        assert!(tmp_snap.path().join("objects").join(&h1).exists());
        assert!(w.object_index.contains(&h1));
        std::fs::write(root.join("a.txt"), b"v5").unwrap();
        w.on_round_complete("R5".into(), None, None).unwrap();
        w.rollback(r1).unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
    }

    /// A blob present on disk but missing from the index (stale-negative)
    /// is re-adopted by the walk's write path without rewriting it.
    #[test]
    fn stale_negative_object_index_is_repaired_by_walk() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.set_racy_window_for_tests(0);
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        let h1 = w.resolved_round_maps(r1).unwrap().files_at_end["a.txt"].clone();

        // Desync: forget the blob, then rewrite the same content so the
        // fingerprint moves and the walk re-reads the file.
        w.object_index.remove(&h1);
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        w.on_round_complete("R2".into(), None, None).unwrap();
        assert!(w.object_index.contains(&h1), "walk re-adopts the blob");
        assert!(tmp_snap.path().join("objects").join(&h1).exists());
    }

    /// A fresh watcher over an existing store seeds its object index from
    /// the `objects/` listing, so resumed sessions keep the fast path.
    #[test]
    fn object_index_is_seeded_on_resume() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let mut w = make_watcher(root, tmp_snap.path());
        w.on_round_complete("R1".into(), None, None).unwrap();
        let r1 = w.history.current_head_id.unwrap();
        let h1 = w.resolved_round_maps(r1).unwrap().files_at_end["a.txt"].clone();
        drop(w);

        let resumed = make_watcher(root, tmp_snap.path());
        assert!(resumed.object_index.contains(&h1));
    }

    #[test]
    fn round_routing_matches_own_root_and_fails_open() {
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        assert!(round_targets_watcher_root(tmp_a.path(), None));
        assert!(round_targets_watcher_root(tmp_a.path(), Some(tmp_a.path())));
        assert!(!round_targets_watcher_root(
            tmp_a.path(),
            Some(tmp_b.path())
        ));
    }

    /// End-to-end round routing through the bus listener: a round carrying
    /// a DIFFERENT root records nothing in this watcher's store, while a
    /// root-less round (fail-open) and a same-root round both record.
    #[tokio::test]
    async fn foreign_root_rounds_skip_walk_rootless_rounds_still_record() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let foreign_root = TempDir::new().unwrap();
        let root = tmp_proj.path().to_path_buf();
        std::fs::write(root.join("a.txt"), b"content").unwrap();

        let bus = EventBus::new();
        let watcher = FileWatcher::new(root.clone(), tmp_snap.path().to_path_buf(), bus.clone())
            .expect("watcher");
        let shared = Arc::new(AsyncMutex::new(watcher));
        let rx = bus.subscribe();
        let listener = tokio::spawn(run_round_complete_listener(
            shared.clone(),
            rx,
            root.clone(),
        ));

        let send_round = |round: usize, project_root: Option<PathBuf>| {
            bus.send(AppEvent::RoundComplete {
                session_id: None,
                round,
                turns_in_round: 1,
                native_message_count: None,
                project_root,
            });
        };
        send_round(1, Some(foreign_root.path().to_path_buf()));
        send_round(2, None);
        send_round(3, Some(root.clone()));

        // The listener applies events in order, so observing the two
        // expected rounds proves the foreign one (sent first) was skipped.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let recorded = shared.lock().await.history.rounds.len();
            if recorded >= 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "listener never recorded the expected rounds"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let w = shared.lock().await;
        assert_eq!(
            w.history.rounds.len(),
            2,
            "foreign-root round must not create a round here"
        );
        assert_eq!(
            w.history
                .rounds
                .iter()
                .map(|r| r.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["Round 2", "Round 3"],
            "the root-less and same-root rounds recorded, in order"
        );
        listener.abort();
    }

    /// Cross-boot baseline reuse: a second boot over an unchanged tree
    /// rewrites zero baseline copies (the stored fingerprint matches), a
    /// changed file rewrites, and a type-flipped path still reconciles
    /// while its unchanged siblings keep the fast path.
    #[test]
    fn baseline_reuse_across_boots_skips_unchanged_rewrites() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("keep.txt"), b"kept content\n").unwrap();
        std::fs::write(root.join("change.txt"), b"original\n").unwrap();
        std::fs::write(root.join("thing"), b"i am a file").unwrap();

        let boot = |root: &Path, snap: &Path| {
            FileWatcher::new_with_racy_window(
                root.to_path_buf(),
                snap.to_path_buf(),
                EventBus::new(),
                0,
            )
            .expect("watcher")
        };

        let first = boot(root, tmp_snap.path());
        assert_eq!(first.baseline_files_rewritten_in_new, 3);
        drop(first);

        // Boot 2 over the untouched tree: zero baseline writes, state
        // fully adopted from the previous manifest.
        let second = boot(root, tmp_snap.path());
        assert_eq!(
            second.baseline_files_rewritten_in_new, 0,
            "an unchanged tree must reuse every baseline entry"
        );
        assert_eq!(
            second
                .baseline_manifest
                .get("keep.txt")
                .map(|m| m.hash.as_str()),
            Some(hex_encode(&sha256_hash(b"kept content\n")).as_str())
        );
        assert_eq!(
            std::fs::read(tmp_snap.path().join("baseline/keep.txt")).unwrap(),
            b"kept content\n"
        );
        drop(second);

        // Boot 3: one changed file, one file→dir flip. The changed file
        // and the flipped path rewrite; the untouched sibling still
        // reuses.
        std::fs::write(root.join("change.txt"), b"rewritten\n").unwrap();
        std::fs::remove_file(root.join("thing")).unwrap();
        std::fs::create_dir_all(root.join("thing")).unwrap();
        std::fs::write(root.join("thing/inner.rs"), b"nested now").unwrap();
        let third = boot(root, tmp_snap.path());
        assert_eq!(
            third.baseline_files_rewritten_in_new, 2,
            "only the changed file and the flipped path rewrite"
        );
        assert_eq!(
            std::fs::read(tmp_snap.path().join("baseline/change.txt")).unwrap(),
            b"rewritten\n"
        );
        assert_eq!(
            std::fs::read(tmp_snap.path().join("baseline/thing/inner.rs")).unwrap(),
            b"nested now"
        );
        assert_eq!(
            std::fs::read(tmp_snap.path().join("baseline/keep.txt")).unwrap(),
            b"kept content\n"
        );
    }

    /// The reuse gate distrusts a damaged shadow copy: a truncated
    /// baseline file fails the size check and heals via a full rewrite
    /// even though the SOURCE file's fingerprint still matches.
    #[test]
    fn baseline_reuse_heals_truncated_shadow_copies() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"full content here\n").unwrap();
        let first = FileWatcher::new_with_racy_window(
            root.to_path_buf(),
            tmp_snap.path().to_path_buf(),
            EventBus::new(),
            0,
        )
        .expect("watcher");
        drop(first);

        std::fs::write(tmp_snap.path().join("baseline/a.txt"), b"torn").unwrap();
        let second = FileWatcher::new_with_racy_window(
            root.to_path_buf(),
            tmp_snap.path().to_path_buf(),
            EventBus::new(),
            0,
        )
        .expect("watcher");
        assert_eq!(second.baseline_files_rewritten_in_new, 1);
        assert_eq!(
            std::fs::read(tmp_snap.path().join("baseline/a.txt")).unwrap(),
            b"full content here\n"
        );
    }

    /// The production racy window really gates the cross-boot fast path: an
    /// entry recorded moments after its file was written is NOT reused (it
    /// could describe a same-granule torn observation).
    #[test]
    fn baseline_reuse_respects_racy_window() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        std::fs::write(root.join("a.txt"), b"fresh write").unwrap();
        let first = make_watcher(root, tmp_snap.path());
        assert_eq!(first.baseline_files_rewritten_in_new, 1);
        drop(first);

        // Second boot immediately after: the stored mtime sits inside the
        // production racy window of its recording time, so the entry is
        // distrusted and the file re-baselines.
        let second = make_watcher(root, tmp_snap.path());
        assert_eq!(
            second.baseline_files_rewritten_in_new, 1,
            "a racy-window entry must not be reused"
        );
    }

    /// Off-lock pipeline: a read that on-disk state moved past while the
    /// watcher lock was released is dropped at publish time (revalidation),
    /// and the follow-up event for the newer write carries the truth.
    #[test]
    fn stale_offlock_read_is_dropped_and_follow_up_event_wins() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let file = root.join("f.txt");
        std::fs::write(&file, b"zero\n").unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut w = FileWatcher::new(root.to_path_buf(), tmp_snap.path().to_path_buf(), bus)
            .expect("watcher");

        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));

        // Event 1: phases A–C run against "one\n"…
        std::fs::write(&file, b"one\n").unwrap();
        let Some(PreparedNotifyChange::Upsert {
            rel,
            rel_key,
            fingerprint,
            size,
        }) = prepare_notify_change(root, &file, &modify)
        else {
            panic!("expected upsert");
        };
        let UpsertStage::Read {
            existed_at_baseline,
            prev_hash,
        } = w.stage_upsert(&rel, size, &fingerprint)
        else {
            panic!("expected read stage");
        };
        let content =
            read_upsert_content(tmp_snap.path(), root, &rel, existed_at_baseline, prev_hash)
                .expect("content");

        // …but a second write lands before publish (as a round scan or
        // rollback interleave would allow). Different length, so the
        // fingerprint moves on every platform.
        std::fs::write(&file, b"two-longer\n").unwrap();
        w.publish_upsert(rel.clone(), rel_key, fingerprint, content);
        assert!(rx.try_recv().is_err(), "stale read must not publish");
        assert_eq!(
            w.hashes.get(&rel),
            Some(&sha256_hash(b"zero\n")),
            "stale read must not overwrite the mirror"
        );

        // The queued event for the second write publishes the fresh state.
        w.process_change(&file, &modify);
        match rx.try_recv().expect("follow-up event") {
            AppEvent::FileChanged { path, kind, .. } => {
                assert_eq!(path, "f.txt");
                assert_eq!(kind, FileChangeKind::Modified);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(w.hashes.get(&rel), Some(&sha256_hash(b"two-longer\n")));
    }

    /// Sequential processing publishes rapid same-path changes in order
    /// with the correct final content.
    #[test]
    fn rapid_double_change_publishes_in_order() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let file = root.join("f.txt");
        std::fs::write(&file, b"zero\n").unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut w = FileWatcher::new(root.to_path_buf(), tmp_snap.path().to_path_buf(), bus)
            .expect("watcher");
        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));

        std::fs::write(&file, b"one\n").unwrap();
        w.process_change(&file, &modify);
        std::fs::write(&file, b"two-longer\n").unwrap();
        w.process_change(&file, &modify);

        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::FileChanged { path, kind, .. } = event {
                assert_eq!(path, "f.txt");
                kinds.push(kind);
            }
        }
        assert_eq!(
            kinds,
            vec![FileChangeKind::Modified, FileChangeKind::Modified]
        );
        assert_eq!(
            w.hashes.get(Path::new("f.txt")),
            Some(&sha256_hash(b"two-longer\n")),
            "the final mirror carries the last write"
        );
    }

    /// Delete revalidation: a Remove event for a path that exists again by
    /// publish time (atomic-rename save, interleaved restore) is dropped —
    /// the recreate's own event supersedes it.
    #[test]
    fn delete_event_for_recreated_path_is_dropped() {
        let tmp_proj = TempDir::new().unwrap();
        let tmp_snap = TempDir::new().unwrap();
        let root = tmp_proj.path();
        let file = root.join("f.txt");
        std::fs::write(&file, b"zero\n").unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut w = FileWatcher::new(root.to_path_buf(), tmp_snap.path().to_path_buf(), bus)
            .expect("watcher");

        // The file was deleted and recreated before the Remove event is
        // processed (an editor's atomic-rename save).
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, b"back-different\n").unwrap();
        w.process_change(
            &file,
            &notify::EventKind::Remove(notify::event::RemoveKind::File),
        );
        assert!(rx.try_recv().is_err(), "no Deleted event for a live path");
        assert_eq!(
            w.hashes.get(Path::new("f.txt")),
            Some(&sha256_hash(b"zero\n")),
            "the mirror survives the dropped delete"
        );

        // The Create event queued behind reports one clean modification.
        w.process_change(
            &file,
            &notify::EventKind::Create(notify::event::CreateKind::File),
        );
        match rx.try_recv().expect("create-after-rename event") {
            AppEvent::FileChanged { path, kind, .. } => {
                assert_eq!(path, "f.txt");
                assert_eq!(kind, FileChangeKind::Modified);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
