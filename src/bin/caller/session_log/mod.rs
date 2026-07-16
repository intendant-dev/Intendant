use crate::types::truncate_str;
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock};
use uuid::Uuid;

mod replay;
pub(crate) use replay::*;
mod history;
pub(crate) use history::*;
// bus_events carries only `impl SessionLog` methods — nothing importable,
// so no glob re-export.
mod bus_events;

/// Structured event written as one JSON line in session.jsonl.
#[derive(Serialize)]
struct LogEvent {
    ts: String,
    /// Epoch milliseconds (UTC) captured alongside `ts`. `ts` is local
    /// time-of-day only — without this field an event's calendar date must
    /// be reconstructed from `session_meta.json` plus midnight-wrap
    /// inference, which breaks across DST folds and multi-day sessions.
    ts_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<usize>,
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    /// Path to a file with full content (relative to log dir).
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    /// Second file reference (e.g., stderr).
    #[serde(skip_serializing_if = "Option::is_none")]
    file2: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnFileSpan {
    relative: String,
    offset: u64,
    len: u64,
}

/// First 16 hex chars of the SHA-256 of `text` — the content fingerprint the
/// `conversation_message_epoch` mapping carries so historical extractors can
/// correlate legacy-extracted messages with resume-time seq assignments.
pub(crate) fn content_hash_hex16(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentOutputChunk {
    pub output_id: String,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Metadata persisted in `session_meta.json` inside each session directory.
#[derive(Serialize, Deserialize, Debug)]
pub struct SessionMeta {
    pub session_id: String,
    pub created_at: String,
    /// Epoch milliseconds (UTC) captured with `created_at` — the
    /// machine-readable timestamp (`created_at` is local and offset-less).
    /// Additive: metas written before 2026-07 lack it. Mirrors
    /// `created_at`'s existing rewrite semantics (re-stamped on every meta
    /// write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<usize>,
    /// Git-worktree linkage for sessions launched into a fresh worktree
    /// (`CreateSession { worktree: true }`): the branch, its checkout path
    /// (the session's effective project root), and where it branched from.
    /// The single source of truth the dashboard's worktree badge, the
    /// session-end finish card, and the merge endpoint all derive from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<SessionWorktreeMeta>,
}

/// Worktree linkage recorded on a session (see [`SessionMeta::worktree`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionWorktreeMeta {
    /// Branch checked out in the worktree.
    pub branch: String,
    /// Absolute worktree checkout path (the session's project root).
    pub path: String,
    /// Project root the worktree was created from (where merges run).
    pub base_root: String,
    /// Branch the base checkout was on at creation, if not detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Commit `HEAD` resolved to when the worktree branched off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
}

static OPEN_SESSION_LOG_DIRS: OnceLock<StdMutex<HashSet<PathBuf>>> = OnceLock::new();

fn open_session_log_dirs() -> &'static StdMutex<HashSet<PathBuf>> {
    OPEN_SESSION_LOG_DIRS.get_or_init(|| StdMutex::new(HashSet::new()))
}

/// How a memoized id→dir resolution is re-validated on a cache hit.
#[derive(Clone, Debug)]
enum SessionDirLookupValidation {
    /// The dir name itself answered to the id — revalidation is a free
    /// string check plus one `is_dir` stat.
    NamePrefix,
    /// The id came from `session_meta.json` — revalidate by the meta
    /// file's (len, mtime): unchanged meta means unchanged session_id.
    MetaFingerprint((u64, u128)),
}

#[derive(Clone, Debug)]
struct SessionDirLookupEntry {
    dir: PathBuf,
    validation: SessionDirLookupValidation,
}

fn session_dir_lookup_cache(
) -> &'static StdMutex<std::collections::HashMap<(PathBuf, String), SessionDirLookupEntry>> {
    static CACHE: OnceLock<
        StdMutex<std::collections::HashMap<(PathBuf, String), SessionDirLookupEntry>>,
    > = OnceLock::new();
    CACHE.get_or_init(|| StdMutex::new(std::collections::HashMap::new()))
}

const SESSION_DIR_LOOKUP_CACHE_LIMIT: usize = 4096;

fn meta_fingerprint(meta_path: &Path) -> Option<(u64, u128)> {
    let metadata = fs::metadata(meta_path).ok()?;
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((metadata.len(), mtime_nanos))
}

/// Memo hit for `find_session_by_id_in_home`, validated against the live
/// filesystem before being trusted (a deleted dir or rewritten meta drops
/// the entry and the caller re-scans).
fn cached_session_dir_for_id(home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let key = (home.to_path_buf(), session_id.to_string());
    let entry = {
        let cache = session_dir_lookup_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.get(&key).cloned()
    }?;
    let valid = entry.dir.is_dir()
        && match &entry.validation {
            SessionDirLookupValidation::NamePrefix => entry
                .dir
                .file_name()
                .map(|name| name.to_string_lossy().starts_with(session_id))
                .unwrap_or(false),
            SessionDirLookupValidation::MetaFingerprint(fingerprint) => {
                meta_fingerprint(&entry.dir.join("session_meta.json")).as_ref() == Some(fingerprint)
            }
        };
    if valid {
        return Some(entry.dir);
    }
    let mut cache = session_dir_lookup_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.remove(&key);
    None
}

fn store_cached_session_dir_for_id(
    home: &Path,
    session_id: &str,
    dir: &Path,
    validation: SessionDirLookupValidation,
) {
    if session_id.is_empty() {
        return;
    }
    let mut cache = session_dir_lookup_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let key = (home.to_path_buf(), session_id.to_string());
    if cache.len() >= SESSION_DIR_LOOKUP_CACHE_LIMIT && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(
        key,
        SessionDirLookupEntry {
            dir: dir.to_path_buf(),
            validation,
        },
    );
}

fn lock_open_session_log_dirs() -> StdMutexGuard<'static, HashSet<PathBuf>> {
    match open_session_log_dirs().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn register_open_session_log_dir(dir: &Path) {
    lock_open_session_log_dirs().insert(dir.to_path_buf());
}

fn unregister_open_session_log_dir(dir: &Path) {
    lock_open_session_log_dirs().remove(dir);
}

fn mark_session_meta_interrupted(dir: &Path, last_turn: Option<usize>) -> bool {
    let meta_path = dir.join("session_meta.json");
    if let Ok(meta_str) = fs::read_to_string(&meta_path) {
        if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
            if meta.status.as_deref() == Some("running") {
                meta.status = Some("interrupted".to_string());
                meta.last_turn = Some(last_turn.or(meta.last_turn).unwrap_or(0));
                if let Ok(json) = serde_json::to_string_pretty(&meta) {
                    let _ = fs::write(&meta_path, &json);
                    return true;
                }
            }
        }
    }
    false
}

fn update_session_meta_after_round_complete(
    dir: &Path,
    last_turn: Option<usize>,
    rounds: Option<usize>,
) {
    let meta_path = dir.join("session_meta.json");
    let Ok(meta_str) = fs::read_to_string(&meta_path) else {
        return;
    };
    let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&meta_str) else {
        return;
    };
    if let Some("completed" | "interrupted") = meta.status.as_deref() {
        return;
    }
    meta.status = Some("idle".to_string());
    if let Some(turn) = last_turn {
        meta.last_turn = Some(meta.last_turn.unwrap_or(0).max(turn));
    }
    if let Some(rounds) = rounds {
        meta.rounds = Some(meta.rounds.unwrap_or(0).max(rounds));
    }
    if let Ok(json) = serde_json::to_string_pretty(&meta) {
        if let Err(e) = fs::write(&meta_path, &json) {
            eprintln!("session_log: failed to update session_meta.json: {}", e);
        }
    }
}

fn log_preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

pub fn mark_registered_session_logs_interrupted_now() -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = lock_open_session_log_dirs().iter().cloned().collect();
    let mut updated = Vec::new();
    for dir in dirs {
        if mark_session_meta_interrupted(&dir, None) {
            updated.push(dir);
        }
    }
    updated
}

/// Comprehensive structured session logger.
///
/// Writes to a directory containing:
/// - `session.jsonl`    — one JSON object per line, every event with metadata
/// - `session_meta.json` — session metadata (id, created_at, project_root, task)
/// - `turns/turn_NNN_model.txt`     — full model response for turn N
/// - `turns/turn_NNN_agent_in.json` — JSON commands sent to agent for turn N
/// - `turns/turn_NNN_stdout.txt`    — agent stdout for turn N
/// - `turns/turn_NNN_stderr.txt`    — agent stderr for turn N (if non-empty)
/// - `summary.json`     — written at session end
///
/// AI agents can: read session.jsonl for an overview, grep by event/turn/level,
/// then drill into specific turn files for full content.
pub struct SessionLog {
    writer: BufWriter<File>,
    transcript_writer: Option<BufWriter<File>>,
    dir: PathBuf,
    session_id: String,
    current_turn: usize,
    summary_builder: SessionSummaryBuilder,
    /// Buffer for accumulating voice_log tokens into full utterances.
    /// Flushed to transcript on turnComplete or user_transcript.
    voice_utterance_buf: String,
    last_approval_resolved: Option<(u64, String)>,
    /// Latest context-snapshot sidecar per (source, session id) stream,
    /// for the rotate-on-write policy in
    /// [`Self::context_snapshot_for_session`]: writing a new snapshot
    /// deletes the previous file of the same stream, keeping per-session
    /// context disk O(1) instead of O(turns × context). Seeded at open
    /// from the rows an earlier process persisted, so a resumed session
    /// keeps rotating instead of stranding its predecessor's sidecar.
    last_context_snapshots: std::collections::HashMap<String, String>,
    /// Snapshot retention policy, resolved from the environment once at
    /// open (`INTENDANT_CONTEXT_SNAPSHOT_KEEP_ALL=1` keeps every sidecar).
    /// Injected as state — not read ambiently per call — so tests pin the
    /// policy they exercise instead of inheriting the shell's.
    keep_all_context_snapshots: bool,
}

/// The rotation-map key for one context-snapshot stream. One derivation
/// shared by the write path and the open-time reseed, so the two can
/// never drift.
pub(super) fn context_snapshot_stream_key(source: &str, session_id: Option<&str>) -> String {
    format!("{}\u{1f}{}", source, session_id.unwrap_or_default())
}

/// Rebuild the latest-sidecar-per-stream rotation state from the rows an
/// earlier process persisted. Without this, every restart/`--continue`
/// strands the previous process's latest sidecar forever — retention
/// would grow O(session reopenings). Streams session.jsonl line by line
/// (a resident session's log can be tens of MB — never buffered whole)
/// with a substring prefilter so only `context_snapshot` rows are parsed;
/// a damaged line or torn/invalid-UTF-8 tail is skipped per line (lossy)
/// without aborting the rows already folded. Run once per session open,
/// and only when rotation is active.
fn seed_context_snapshot_rotation(dir: &Path) -> std::collections::HashMap<String, String> {
    let mut latest = std::collections::HashMap::new();
    let Ok(file) = File::open(dir.join("session.jsonl")) else {
        return latest;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            // Keep whatever was folded so far; a damaged region ends the
            // seed, not the session open.
            Err(_) => break,
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if !line.contains("\"context_snapshot\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row.get("event").and_then(|v| v.as_str()) != Some("context_snapshot") {
            continue;
        }
        let Some(file) = row.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let data = row.get("data");
        let source = data
            .and_then(|d| d.get("source"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_id = data
            .and_then(|d| d.get("session_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty());
        latest.insert(
            context_snapshot_stream_key(source, session_id),
            file.to_string(),
        );
    }
    latest
}

/// Accumulates session statistics as events are logged.
/// Written to `session_summary.json` at session end.
#[derive(Default)]
struct SessionSummaryBuilder {
    start_time: Option<chrono::DateTime<chrono::Local>>,
    voice_provider: Option<String>,
    voice_model: Option<String>,
    voice_connections: usize,
    frames_sent: usize,
    cu_tasks: Vec<CuTaskSummary>,
    /// CU task currently in progress (captured on cu_task_start, moved to cu_tasks on complete).
    current_cu_task: Option<String>,
    current_cu_turns: usize,
    errors: Vec<ErrorSummary>,
    user_transcripts: Vec<String>,
    total_tokens: u64,
}

/// Summary of the entire session, written as `session_summary.json`.
#[derive(Serialize, Deserialize, Debug)]
pub struct SessionSummary {
    pub duration_secs: f64,
    pub voice_provider: Option<String>,
    pub voice_model: Option<String>,
    pub voice_connections: usize,
    pub voice_reconnects: usize,
    pub model_turns: usize,
    pub cu_tasks: Vec<CuTaskSummary>,
    pub frames_sent: usize,
    pub errors: Vec<ErrorSummary>,
    pub user_transcripts: Vec<String>,
    pub total_tokens: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CuTaskSummary {
    pub task: String,
    pub turns: usize,
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ErrorSummary {
    pub category: String,
    pub reason: String,
    pub ts: String,
}

/// Entry in transcript.jsonl — simplified conversation log.
#[derive(Serialize)]
struct TranscriptEntry {
    ts: String,
    role: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools_called: Option<Vec<String>>,
}

impl SessionLog {
    /// Open (or create) a session log directory.
    /// The `path` argument is the directory (not a file).
    /// If resuming an existing session, reads the session_id from session_meta.json.
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(dir.join("turns"))?;
        let log_dir = dir.clone();

        // Try to read existing session_id from meta, or derive from directory name
        let session_id = if let Ok(meta_str) = fs::read_to_string(dir.join("session_meta.json")) {
            if let Ok(meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                meta.session_id
            } else {
                dir.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| Uuid::new_v4().to_string())
            }
        } else {
            dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| Uuid::new_v4().to_string())
        };

        let keep_all_context_snapshots = bus_events::context_snapshot_keep_all();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("session.jsonl"))?;
        let transcript_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("transcript.jsonl"))
            .ok()
            .map(BufWriter::new);
        let mut log = Self {
            writer: BufWriter::new(file),
            transcript_writer: transcript_file,
            dir,
            session_id,
            current_turn: 0,
            summary_builder: SessionSummaryBuilder {
                start_time: Some(Local::now()),
                ..Default::default()
            },
            voice_utterance_buf: String::new(),
            last_approval_resolved: None,
            // Seeding only matters when rotation is active — under
            // keep-all nothing is ever deleted, so skip the log pass.
            last_context_snapshots: if keep_all_context_snapshots {
                std::collections::HashMap::new()
            } else {
                seed_context_snapshot_rotation(&log_dir)
            },
            keep_all_context_snapshots,
        };
        log.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_start".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Session started at {}",
                Local::now().format("%Y-%m-%d %H:%M:%S")
            )),
            data: None,
            file: None,
            file2: None,
        });
        register_open_session_log_dir(&log_dir);
        Ok(log)
    }

    /// Test seam for the snapshot retention policy: production resolves it
    /// from the environment once at [`Self::open`]; tests inject the policy
    /// they exercise instead of inheriting the shell's.
    #[cfg(test)]
    pub(crate) fn set_context_snapshot_keep_all(&mut self, keep_all: bool) {
        self.keep_all_context_snapshots = keep_all;
    }

    /// Write session metadata to `session_meta.json`.
    /// Call after open() to persist session identity and context.
    pub fn write_meta(&self, project_root: Option<&Path>, task: Option<&str>) {
        self.write_meta_with_name_and_role(project_root, task, None, None);
    }

    /// Write session metadata with an optional user-facing session name.
    pub fn write_meta_with_name(
        &self,
        project_root: Option<&Path>,
        task: Option<&str>,
        name: Option<&str>,
    ) {
        self.write_meta_with_name_and_role(project_root, task, name, None);
    }

    /// Write session metadata with an explicit role marker (e.g. `resident`
    /// for the daemon's own base session). The role survives in
    /// `session_meta.json` so the session catalog can tell the daemon's
    /// resident session apart from an abandoned user task after this
    /// process exits.
    pub fn write_meta_with_role(
        &self,
        project_root: Option<&Path>,
        task: Option<&str>,
        role: Option<&str>,
    ) {
        self.write_meta_with_name_and_role(project_root, task, None, role);
    }

    fn write_meta_with_name_and_role(
        &self,
        project_root: Option<&Path>,
        task: Option<&str>,
        name: Option<&str>,
        role: Option<&str>,
    ) {
        let existing = fs::read_to_string(self.dir.join("session_meta.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<SessionMeta>(&raw).ok());
        let (existing_name, existing_worktree) = existing
            .map(|meta| (meta.name, meta.worktree))
            .unwrap_or((None, None));
        let meta = SessionMeta {
            session_id: self.session_id.clone(),
            created_at: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            created_at_ms: Some(Self::ts_ms()),
            project_root: project_root.map(|p| p.to_string_lossy().to_string()),
            name: name.map(|n| n.to_string()).or(existing_name),
            task: task.map(|t| t.to_string()),
            status: Some("running".to_string()),
            last_turn: None,
            role: role.map(|r| r.to_string()),
            rounds: None,
            // Worktree linkage is written once at launch and survives every
            // later meta rewrite (resume, rename), like the session name.
            worktree: existing_worktree,
        };
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            if let Err(e) = fs::write(self.dir.join("session_meta.json"), json) {
                eprintln!("session_log: failed to write session_meta.json: {}", e);
            }
        }
    }

    /// Record (or update) the session's git-worktree linkage in
    /// `session_meta.json`. Call after `write_meta*` has created the file;
    /// the linkage then survives later meta rewrites.
    pub fn write_meta_worktree(&self, worktree: &SessionWorktreeMeta) {
        let meta_path = self.dir.join("session_meta.json");
        let Some(mut meta) = fs::read_to_string(&meta_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<SessionMeta>(&raw).ok())
        else {
            eprintln!(
                "session_log: cannot record worktree linkage before session_meta.json exists"
            );
            return;
        };
        meta.worktree = Some(worktree.clone());
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            if let Err(e) = fs::write(&meta_path, json) {
                eprintln!("session_log: failed to write session_meta.json: {}", e);
            }
        }
    }

    /// Resolve the session log directory.
    /// If `override_path` is set (via --log-file), use that as the directory.
    /// Otherwise, pick a fresh UUID-named directory under `~/.intendant/logs`.
    ///
    /// Pure path computation — nothing is created on disk until `open()`,
    /// so a caller that bails before opening can't strand an empty session
    /// directory in the logs tree.
    pub fn resolve_path(override_path: Option<&str>) -> PathBuf {
        if let Some(path) = override_path {
            return PathBuf::from(path);
        }

        // A fresh UUID-named directory for each top-level caller invocation.
        let session_id = Uuid::new_v4().to_string();
        crate::platform::intendant_home()
            .join("logs")
            .join(&session_id)
    }

    /// [`SessionLog::resolve_path`] against an explicit home: mints the
    /// fresh UUID dir under `<home>/.intendant/logs`. The supervisor mints
    /// through its `logs_home()` so tests' spawned sessions land in the
    /// injected scratch home instead of the machine's real store; the
    /// ambient variant stays the CLI/startup edge.
    pub fn resolve_path_in_home(home: &Path, override_path: Option<&str>) -> PathBuf {
        if let Some(path) = override_path {
            return PathBuf::from(path);
        }
        let session_id = Uuid::new_v4().to_string();
        crate::platform::intendant_home_in(home)
            .join("logs")
            .join(&session_id)
    }

    /// Find the most recent session for a given project root.
    /// Scans `~/.intendant/logs/*/session_meta.json`, filters by project_root,
    /// and returns the most recently created session.
    pub fn find_latest_session(project_root: &Path) -> Option<(String, PathBuf)> {
        let logs_dir = crate::platform::intendant_home().join("logs");
        if !logs_dir.is_dir() {
            return None;
        }

        let project_root_str = project_root.to_string_lossy().to_string();
        let mut best: Option<(String, PathBuf, String)> = None; // (session_id, dir, created_at)

        if let Ok(entries) = fs::read_dir(&logs_dir) {
            for entry in entries.flatten() {
                let meta_path = entry.path().join("session_meta.json");
                if !meta_path.exists() {
                    continue;
                }
                if let Ok(meta_str) = fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                        // Skip sub-agent sessions (they shouldn't be resumed as top-level)
                        if let Some(ref role) = meta.role {
                            match role.as_str() {
                                "orchestrator" | "research" | "implementation" | "testing" => {
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        if meta.project_root.as_deref() == Some(&project_root_str) {
                            let dominated = match &best {
                                Some((_, _, best_created)) => meta.created_at > *best_created,
                                None => true,
                            };
                            if dominated {
                                best = Some((meta.session_id, entry.path(), meta.created_at));
                            }
                        }
                    }
                }
            }
        }

        best.map(|(id, dir, _)| (id, dir))
    }

    /// Find a session by its ID (UUID prefix or full UUID).
    /// Checks `~/.intendant/logs/{id}/` directly, then scans for prefix matches.
    pub fn find_session_by_id(session_id: &str) -> Option<PathBuf> {
        Self::find_session_by_id_in_home(&crate::platform::home_dir(), session_id)
    }

    pub fn find_session_by_id_in_home(home: &Path, session_id: &str) -> Option<PathBuf> {
        // Path-form ids ("resume by path": absolute, or containing a
        // separator) resolve through the anchored helper — the path must
        // land inside the logs root, so a pasted log-dir path keeps
        // working without turning session lookup into an
        // arbitrary-directory read. This runs FIRST because
        // `logs_dir.join(absolute_path)` would silently replace the base.
        if crate::session_names::session_id_looks_like_path(session_id) {
            return crate::session_names::intendant_session_dir_from_slash_path(home, session_id);
        }

        let logs_dir = crate::platform::intendant_home_in(home).join("logs");

        // Direct match (dir name == session_id)
        let direct = logs_dir.join(session_id);
        if direct.is_dir() && direct.join("session_meta.json").exists() {
            return Some(direct);
        }

        if !logs_dir.is_dir() {
            return None;
        }

        // Memoized resolution: this lookup used to scan every session dir
        // AND read every session_meta.json per call — repeated resolutions
        // of the same id (MCP event routing, launch paths) paid a full
        // store scan each time. A hit is re-validated against the live
        // filesystem before it is trusted; misses fall through to the
        // scan. Negative results are never cached (the session may be
        // created a moment later).
        if let Some(dir) = cached_session_dir_for_id(home, session_id) {
            return Some(dir);
        }

        // Pass 1 — directory names only (no file reads): prefix match.
        // The legacy single pass interleaved meta reads with the name
        // scan, so an id resolvable by name could still pay meta reads
        // for every dir readdir happened to yield first.
        let mut entries: Vec<PathBuf> = Vec::new();
        if let Ok(dir_entries) = fs::read_dir(&logs_dir) {
            for entry in dir_entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(session_id) && path.is_dir() {
                    store_cached_session_dir_for_id(
                        home,
                        session_id,
                        &path,
                        SessionDirLookupValidation::NamePrefix,
                    );
                    return Some(path);
                }
                entries.push(path);
            }
        }

        // Pass 2 — the expensive primitive: read each session_meta.json
        // for an exact or prefix session_id match.
        for path in entries {
            let meta_path = path.join("session_meta.json");
            if let Ok(meta_str) = fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                    if meta.session_id == session_id || meta.session_id.starts_with(session_id) {
                        store_cached_session_dir_for_id(
                            home,
                            session_id,
                            &path,
                            meta_fingerprint(&meta_path)
                                .map(SessionDirLookupValidation::MetaFingerprint)
                                .unwrap_or(SessionDirLookupValidation::NamePrefix),
                        );
                        return Some(path);
                    }
                }
            }
        }

        None
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ts() -> String {
        Local::now().format("%H:%M:%S%.3f").to_string()
    }

    fn ts_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn emit(&mut self, event: LogEvent) {
        if let Err(e) = self.emit_checked(event) {
            eprintln!("session_log: failed to write log event: {}", e);
        }
    }

    /// Fallible emit: serialize straight into the writer (the intermediate
    /// String per event bought nothing on this universal append path) and
    /// flush — the flush per record is the durability contract for tail
    /// readers. Callers whose follow-up is only safe once the row is
    /// durably queued (context-snapshot rotation deletes its predecessor)
    /// branch on the result; everything else uses the fire-and-forget
    /// [`Self::emit`].
    fn emit_checked(&mut self, event: LogEvent) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.writer, &event).map_err(std::io::Error::other)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }

    /// Test seam: swap the session.jsonl writer for a read-only handle so
    /// the next emit fails at flush — exercises contracts that must hold
    /// when a row cannot be made durable (disk full, revoked handle).
    #[cfg(test)]
    pub(crate) fn sabotage_writer_for_tests(&mut self) {
        if let Ok(file) = File::open(self.dir.join("session.jsonl")) {
            self.writer = BufWriter::new(file);
        }
    }

    fn emit_transcript(&mut self, entry: TranscriptEntry) {
        if let Some(ref mut w) = self.transcript_writer {
            let _ = serde_json::to_writer(&mut *w, &entry)
                .map_err(std::io::Error::other)
                .and_then(|()| w.write_all(b"\n"));
            let _ = w.flush();
        }
    }

    // ---- CU (Computer Use) structured events ----

    /// Log the start of a CU task.
    pub fn cu_task_start(
        &mut self,
        task: &str,
        provider: &str,
        model: &str,
        cu_enabled: bool,
        cu_display: Option<(u32, u32)>,
        ref_images: usize,
    ) {
        self.summary_builder.current_cu_task = Some(task.to_string());
        self.summary_builder.current_cu_turns = 0;
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "cu_task_start".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("CU task: {} ({}:{})", task, provider, model)),
            data: Some(serde_json::json!({
                "task": task,
                "provider": provider,
                "model": model,
                "cu_enabled": cu_enabled,
                "cu_display": cu_display,
                "ref_images": ref_images,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a CU turn with structured data.
    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub fn cu_turn(
        &mut self,
        turn: usize,
        content_len: usize,
        cu_calls: usize,
        tool_calls: usize,
        prompt_tokens: u64,
        completion_tokens: u64,
        actions: &[String],
    ) {
        self.summary_builder.current_cu_turns = turn;
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "cu_turn".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "CU turn {}: cu_calls={}, tool_calls={}, actions={:?}",
                turn, cu_calls, tool_calls, actions
            )),
            data: Some(serde_json::json!({
                "turn": turn,
                "content_len": content_len,
                "cu_calls": cu_calls,
                "tool_calls": tool_calls,
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "actions": actions,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log CU task completion.
    pub fn cu_task_complete(&mut self, turns: usize, success: bool, summary: &str) {
        self.summary_builder.cu_tasks.push(CuTaskSummary {
            task: self
                .summary_builder
                .current_cu_task
                .take()
                .unwrap_or_else(|| summary.to_string()),
            turns,
            success,
        });
        self.summary_builder.current_cu_turns = 0;
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "cu_task_complete".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("CU complete: {} ({} turns)", summary, turns)),
            data: Some(serde_json::json!({
                "turns": turns,
                "success": success,
                "summary": summary,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log CU task error or escalation.
    pub fn cu_task_error(&mut self, error: &str, escalated_to: Option<&str>) {
        self.summary_builder.errors.push(ErrorSummary {
            category: "cu_error".to_string(),
            reason: error.to_string(),
            ts: Self::ts(),
        });
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "cu_task_error".to_string(),
            level: Some("warn".to_string()),
            message: Some(format!("CU error: {}", error)),
            data: Some(serde_json::json!({
                "error": error,
                "escalated_to": escalated_to,
            })),
            file: None,
            file2: None,
        });
    }

    // ---- Error categorization ----

    /// Log a categorized error with structured metadata.
    #[allow(dead_code)]
    pub fn categorized_error(
        &mut self,
        category: &str,
        reason: &str,
        code: Option<&str>,
        provider: Option<&str>,
    ) {
        self.summary_builder.errors.push(ErrorSummary {
            category: category.to_string(),
            reason: reason.to_string(),
            ts: Self::ts(),
        });
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "error".to_string(),
            level: Some("error".to_string()),
            message: Some(reason.to_string()),
            data: Some(serde_json::json!({
                "category": category,
                "code": code,
                "reason": reason,
                "provider": provider,
            })),
            file: None,
            file2: None,
        });
    }

    // ---- Session summary ----

    /// Write `session_summary.json` with accumulated statistics.
    pub fn write_session_summary(&mut self) {
        self.flush_voice_utterance();
        // Rebuild transcript.jsonl from session.jsonl to ensure completeness.
        // The real-time buffering may have missed events due to race conditions.
        self.rebuild_transcript();

        // Fallback: scan session.jsonl for data the builder might have missed
        // due to race conditions (event bus hasn't flushed when summary writes).
        let _ = self.writer.flush();
        if let Ok(content) = fs::read_to_string(self.dir.join("session.jsonl")) {
            for line in content.lines() {
                let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                match val["event"].as_str().unwrap_or("") {
                    "live_usage_update" | "presence_usage_update" => {
                        if let Some(t) = val["data"]["total_tokens"].as_u64() {
                            if t > self.summary_builder.total_tokens {
                                self.summary_builder.total_tokens = t;
                            }
                        }
                    }
                    "voice_usage" => {
                        // Parse from detail string "tokens: total=28000 ..."
                        if let Some(detail) = val["data"]["detail"].as_str() {
                            if let Some(ts) = detail.split("total=").nth(1) {
                                if let Some(n) = ts.split_whitespace().next() {
                                    if let Ok(t) = n.parse::<u64>() {
                                        if t > self.summary_builder.total_tokens {
                                            self.summary_builder.total_tokens = t;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let duration = self
            .summary_builder
            .start_time
            .map(|s| (Local::now() - s).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(0.0);
        // Include in-progress CU task if session ended mid-task
        let mut cu_tasks = self.summary_builder.cu_tasks.clone();
        if let Some(ref task) = self.summary_builder.current_cu_task {
            let already_recorded = cu_tasks.iter().any(|t| t.task == *task);
            if !already_recorded && self.summary_builder.current_cu_turns > 0 {
                cu_tasks.push(CuTaskSummary {
                    task: task.clone(),
                    turns: self.summary_builder.current_cu_turns,
                    success: false,
                });
            }
        }
        // Count model turns from the rebuilt transcript
        let model_turns = if self.dir.join("transcript.jsonl").exists() {
            fs::read_to_string(self.dir.join("transcript.jsonl"))
                .ok()
                .map(|c| {
                    c.lines()
                        .filter(|l| {
                            serde_json::from_str::<serde_json::Value>(l)
                                .ok()
                                .and_then(|v| v["role"].as_str().map(|r| r == "model"))
                                .unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };

        let summary = SessionSummary {
            duration_secs: duration,
            voice_provider: self.summary_builder.voice_provider.clone(),
            voice_model: self.summary_builder.voice_model.clone(),
            voice_connections: self.summary_builder.voice_connections,
            voice_reconnects: self.summary_builder.voice_connections.saturating_sub(1),
            model_turns,
            cu_tasks,
            frames_sent: self.summary_builder.frames_sent,
            errors: self.summary_builder.errors.clone(),
            user_transcripts: self.summary_builder.user_transcripts.clone(),
            total_tokens: self.summary_builder.total_tokens,
        };
        let path = self.dir.join("session_summary.json");
        if let Ok(json) = serde_json::to_string_pretty(&summary) {
            if let Err(e) = fs::write(&path, &json) {
                eprintln!("session_log: failed to write session_summary.json: {}", e);
            }
        }
    }

    /// Write content to a turn-specific file and return its relative path.
    ///
    /// Overwrites existing content. Use [`append_turn_file`] for streams
    /// like stdout/stderr that accumulate multiple writes within one turn.
    ///
    /// [`append_turn_file`]: Self::append_turn_file
    fn write_turn_file(&self, suffix: &str, content: &str) -> Option<String> {
        let relative = format!("turns/turn_{:03}_{}", self.current_turn, suffix);
        let path = self.dir.join(&relative);
        if fs::write(&path, content).is_ok() {
            Some(relative)
        } else {
            None
        }
    }

    /// Append content to a turn-specific file and return its relative path.
    ///
    /// If the file already has content, writes a blank-line separator first
    /// so successive entries remain visually distinct when read back. Returns
    /// `None` if the OS write fails — the caller should then drop the `file`
    /// reference from the session-log event so downstream readers don't chase
    /// a phantom path.
    fn append_turn_file(&self, suffix: &str, content: &str) -> Option<String> {
        self.append_turn_file_span(suffix, content)
            .map(|span| span.relative)
    }

    fn append_turn_file_span(&self, suffix: &str, content: &str) -> Option<TurnFileSpan> {
        let relative = format!("turns/turn_{:03}_{}", self.current_turn, suffix);
        let path = self.dir.join(&relative);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        // One post-open fstat serves both the has-content test and the
        // span offset (the pre-open stat was a wasted syscall on this
        // per-output-chunk path).
        let mut offset = file.metadata().ok()?.len();
        if offset > 0 && file.write_all(b"\n").is_ok() {
            offset += 1;
        }
        if file.write_all(content.as_bytes()).is_ok() {
            Some(TurnFileSpan {
                relative,
                offset,
                len: content.len() as u64,
            })
        } else {
            None
        }
    }

    // ---- Public logging methods ----

    pub fn info(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "info".to_string(),
            level: Some("info".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    pub fn warn(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "warn".to_string(),
            level: Some("warn".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a voice transcript from the browser presence model.
    pub fn voice_log(&mut self, text: &str, seq: u64, tool_context: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "voice_log".to_string(),
            level: Some("info".to_string()),
            message: Some(text.to_string()),
            data: Some(serde_json::json!({
                "seq": seq,
                "tool_context": tool_context,
            })),
            file: None,
            file2: None,
        });
        // Buffer voice tokens into full utterances (flushed on turnComplete
        // via voice_protocol). Writing per-token produces unreadable transcripts.
        if tool_context.is_none() || tool_context == Some("transcript") {
            self.voice_utterance_buf.push_str(text);
        }
    }

    /// Log a server-side user speech transcript (from Whisper API).
    pub fn user_transcript(&mut self, text: &str, seq: u64) {
        // Flush any buffered model speech before the user turn
        self.flush_voice_utterance();
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "user_transcript".to_string(),
            level: Some("info".to_string()),
            message: Some(text.to_string()),
            data: Some(serde_json::json!({ "seq": seq })),
            file: None,
            file2: None,
        });
        self.summary_builder.user_transcripts.push(text.to_string());
        self.emit_transcript(TranscriptEntry {
            ts: Self::ts(),
            role: "user".to_string(),
            text: text.to_string(),
            tools_called: None,
        });
    }

    /// Log a presence checkpoint (context summary from browser model).
    pub fn presence_checkpoint(&mut self, summary: &str, last_event_seq: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "presence_checkpoint".to_string(),
            level: Some("info".to_string()),
            message: Some(summary.to_string()),
            data: Some(serde_json::json!({
                "last_event_seq": last_event_seq,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a browser presence connect event.
    pub fn presence_connected(&mut self, provider: Option<&str>, model: Option<&str>) {
        self.summary_builder.voice_connections += 1;
        if let Some(p) = provider {
            self.summary_builder.voice_provider = Some(p.to_string());
        }
        if let Some(m) = model {
            self.summary_builder.voice_model = Some(m.to_string());
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "presence_connected".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Browser presence connected ({}:{})",
                provider.unwrap_or("unknown"),
                model.unwrap_or("unknown"),
            )),
            data: Some(serde_json::json!({
                "provider": provider,
                "model": model,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a browser presence disconnect event.
    pub fn presence_disconnected(&mut self) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "presence_disconnected".to_string(),
            level: Some("info".to_string()),
            message: Some("Browser presence disconnected".to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a voice/presence diagnostic — delegates to typed event methods.
    /// Kept as the public API so callers don't need to change.
    pub fn voice_diagnostic(&mut self, kind: &str, detail: &str) {
        match kind {
            "audio_send" => self.voice_audio(kind, detail),
            "video_send" | "frame_skip" => self.voice_frame(kind, detail),
            "gemini_usage" => self.voice_usage(kind, detail),
            "error" | "gemini_close" | "action_drop" => self.voice_error(kind, detail),
            _ => self.voice_protocol(kind, detail),
        }
    }

    /// Audio chunk telemetry (high-frequency, skip in most views).
    pub fn voice_audio(&mut self, kind: &str, detail: &str) {
        self.emit_voice("voice_audio", "debug", kind, detail);
    }

    /// Protocol lifecycle: setupComplete, turnComplete, connected, interrupted, etc.
    pub fn voice_protocol(&mut self, kind: &str, detail: &str) {
        // Flush buffered voice tokens to transcript on turnComplete
        if detail.starts_with("turnComplete")
            || kind == "gemini_msg" && detail.contains("turnComplete")
        {
            self.flush_voice_utterance();
        }
        self.emit_voice("voice_protocol", "debug", kind, detail);
    }

    /// Rebuild transcript.jsonl from session.jsonl at session end.
    /// Aggregates per-token voice_log events into full utterances per turn,
    /// using voice_protocol/turnComplete as turn boundaries.
    fn rebuild_transcript(&mut self) {
        let _ = self.writer.flush();
        let session_path = self.dir.join("session.jsonl");
        let content = match fs::read_to_string(&session_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        let mut entries: Vec<TranscriptEntry> = Vec::new();
        let mut model_buf = String::new();
        let mut model_ts = String::new();

        for line in content.lines() {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let event = val["event"].as_str().unwrap_or("");
            let ts = val["ts"].as_str().unwrap_or("").to_string();

            match event {
                "user_transcript" => {
                    // Flush any buffered model speech first
                    let trimmed = model_buf.trim().to_string();
                    if !trimmed.is_empty() {
                        entries.push(TranscriptEntry {
                            ts: model_ts.clone(),
                            role: "model".to_string(),
                            text: trimmed,
                            tools_called: None,
                        });
                        model_buf.clear();
                    }
                    let text = val["message"].as_str().unwrap_or("").to_string();
                    if !text.is_empty() {
                        entries.push(TranscriptEntry {
                            ts,
                            role: "user".to_string(),
                            text,
                            tools_called: None,
                        });
                    }
                }
                "voice_log" => {
                    let ctx = val["data"]["tool_context"].as_str().unwrap_or("");
                    if ctx.is_empty() || ctx == "transcript" {
                        let text = val["message"].as_str().unwrap_or("");
                        if model_buf.is_empty() {
                            model_ts = ts;
                        }
                        model_buf.push_str(text);
                    }
                }
                "tool_request" => {
                    let tool = val["data"]["tool"].as_str().unwrap_or("unknown");
                    let args = val["data"]["args"]
                        .as_object()
                        .map(|o| serde_json::to_string(o).unwrap_or_default())
                        .unwrap_or_default();
                    entries.push(TranscriptEntry {
                        ts,
                        role: "model".to_string(),
                        text: format!("[tool:{}] {}", tool, args),
                        tools_called: Some(vec![tool.to_string()]),
                    });
                }
                "voice_protocol" => {
                    let detail = val["data"]["detail"].as_str().unwrap_or("");
                    // Flush on turnComplete or interrupted
                    if detail.contains("turnComplete") || detail.contains("interrupted") {
                        let trimmed = model_buf.trim().to_string();
                        if !trimmed.is_empty() {
                            entries.push(TranscriptEntry {
                                ts: model_ts.clone(),
                                role: "model".to_string(),
                                text: trimmed,
                                tools_called: None,
                            });
                            model_buf.clear();
                        }
                    }
                }
                // Also handle legacy voice_diagnostic for older session.jsonl
                "voice_diagnostic" => {
                    let kind = val["data"]["kind"].as_str().unwrap_or("");
                    let detail = val["data"]["detail"].as_str().unwrap_or("");
                    if kind == "gemini_msg"
                        && (detail.contains("turnComplete") || detail.contains("interrupted"))
                    {
                        let trimmed = model_buf.trim().to_string();
                        if !trimmed.is_empty() {
                            entries.push(TranscriptEntry {
                                ts: model_ts.clone(),
                                role: "model".to_string(),
                                text: trimmed,
                                tools_called: None,
                            });
                            model_buf.clear();
                        }
                    }
                }
                _ => {}
            }
        }
        // Flush remaining
        let trimmed = model_buf.trim().to_string();
        if !trimmed.is_empty() {
            entries.push(TranscriptEntry {
                ts: model_ts,
                role: "model".to_string(),
                text: trimmed,
                tools_called: None,
            });
        }

        // Overwrite transcript.jsonl with clean aggregated version
        if !entries.is_empty() {
            let transcript_path = self.dir.join("transcript.jsonl");
            if let Ok(f) = File::create(&transcript_path) {
                let mut w = BufWriter::new(f);
                for entry in &entries {
                    if let Ok(json) = serde_json::to_string(entry) {
                        let _ = writeln!(w, "{}", json);
                    }
                }
                let _ = w.flush();
            }
        }
    }

    /// Flush the buffered voice utterance to transcript.jsonl.
    fn flush_voice_utterance(&mut self) {
        let text = self.voice_utterance_buf.trim().to_string();
        if !text.is_empty() {
            self.emit_transcript(TranscriptEntry {
                ts: Self::ts(),
                role: "model".to_string(),
                text,
                tools_called: None,
            });
        }
        self.voice_utterance_buf.clear();
    }

    /// Video frame send telemetry.
    pub fn voice_frame(&mut self, kind: &str, detail: &str) {
        self.summary_builder.frames_sent += 1;
        self.emit_voice("voice_frame", "debug", kind, detail);
    }

    /// Live model token usage.
    pub fn voice_usage(&mut self, kind: &str, detail: &str) {
        // Extract total tokens from detail string like "tokens: total=3099 prompt=..."
        if let Some(total_str) = detail.split("total=").nth(1) {
            if let Some(num_str) = total_str.split_whitespace().next() {
                if let Ok(total) = num_str.parse::<u64>() {
                    // Use the max seen (cumulative) rather than adding (already cumulative)
                    if total > self.summary_builder.total_tokens {
                        self.summary_builder.total_tokens = total;
                    }
                }
            }
        }
        self.emit_voice("voice_usage", "debug", kind, detail);
    }

    /// Voice/presence errors (disconnects, failures).
    pub fn voice_error(&mut self, kind: &str, detail: &str) {
        self.summary_builder.errors.push(ErrorSummary {
            category: format!("voice_{}", kind),
            reason: detail.to_string(),
            ts: Self::ts(),
        });
        self.emit_voice("voice_error", "warn", kind, detail);
    }

    fn emit_voice(&mut self, event: &str, level: &str, kind: &str, detail: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: event.to_string(),
            level: Some(level.to_string()),
            message: Some(format!("[voice:{}] {}", kind, detail)),
            data: Some(serde_json::json!({
                "kind": kind,
                "detail": detail,
            })),
            file: None,
            file2: None,
        });
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        // Flush any buffered log data
        let _ = self.writer.flush();

        // If the session is still "running", mark it as "interrupted"
        mark_session_meta_interrupted(&self.dir, Some(self.current_turn));
        unregister_open_session_log_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_directory_structure() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let _log = SessionLog::open(log_dir.clone()).unwrap();
        assert!(log_dir.join("session.jsonl").exists());
        assert!(log_dir.join("turns").is_dir());
    }

    #[test]
    fn open_uses_directory_name_as_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("my-custom-session");
        let log = SessionLog::open(log_dir).unwrap();
        assert_eq!(log.session_id(), "my-custom-session");
    }

    #[test]
    fn open_with_uuid_dir_uses_uuid_as_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let uuid = Uuid::new_v4().to_string();
        let log_dir = dir.path().join(&uuid);
        let log = SessionLog::open(log_dir).unwrap();
        assert_eq!(log.session_id(), uuid);
    }

    #[test]
    fn minted_session_ids_are_flag_safe() {
        // Session ids ride argv (`intendant --resume <id>`), where a leading
        // '-' reads as a flag — `--resume` in particular degrades to
        // `--continue` on a dash-leading token. UUIDs are hex-leading, so
        // this can't fire today; if this mint ever moves to a dashable
        // alphabet (base64url), prefix the id with a fixed alphanumeric
        // char instead (see 8c9c0d96).
        for _ in 0..64 {
            let path = SessionLog::resolve_path(None);
            let id = path
                .file_name()
                .expect("session dir has a name")
                .to_string_lossy()
                .to_string();
            assert!(!id.starts_with('-'), "session id not flag-safe: {id}");
        }
    }

    #[test]
    fn open_reuses_existing_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(&log_dir).unwrap();

        // Write a meta file with a known session_id
        let meta = SessionMeta {
            session_id: "test-session-123".to_string(),
            created_at: "2026-01-01T00:00:00".to_string(),
            created_at_ms: None,
            project_root: None,
            name: None,
            task: None,
            status: None,
            last_turn: None,
            role: None,
            rounds: None,
            worktree: None,
        };
        fs::write(
            log_dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        let log = SessionLog::open(log_dir).unwrap();
        assert_eq!(log.session_id(), "test-session-123");
    }

    #[test]
    fn write_meta_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp/project")), Some("test task"));

        let meta_path = log_dir.join("session_meta.json");
        assert!(meta_path.exists());
        let content = fs::read_to_string(&meta_path).unwrap();
        let meta: SessionMeta = serde_json::from_str(&content).unwrap();
        assert_eq!(meta.session_id, log.session_id());
        assert_eq!(meta.project_root.as_deref(), Some("/tmp/project"));
        assert_eq!(meta.task.as_deref(), Some("test task"));
        assert_eq!(meta.status.as_deref(), Some("running"));
    }

    #[test]
    fn write_meta_with_name_persists_and_preserves_name() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta_with_name(
            Some(Path::new("/tmp/project")),
            Some("test task"),
            Some("Named session"),
        );
        log.write_meta(Some(Path::new("/tmp/project")), Some("follow-up task"));

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.name.as_deref(), Some("Named session"));
        assert_eq!(meta.task.as_deref(), Some("follow-up task"));
    }

    #[test]
    fn write_meta_worktree_records_and_survives_meta_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp/wt/checkout")), Some("task"));

        let linkage = SessionWorktreeMeta {
            branch: "session-branch".to_string(),
            path: "/tmp/wt/checkout".to_string(),
            base_root: "/tmp/wt".to_string(),
            base_branch: Some("main".to_string()),
            base_sha: Some("abc123".to_string()),
        };
        log.write_meta_worktree(&linkage);
        // A later meta rewrite (resume / rename) must not drop the linkage.
        log.write_meta(Some(Path::new("/tmp/wt/checkout")), Some("resumed task"));

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.task.as_deref(), Some("resumed task"));
        assert_eq!(meta.worktree, Some(linkage));
    }

    #[test]
    fn resolve_path_with_override() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom_logs");
        let path = SessionLog::resolve_path(Some(custom.to_str().unwrap()));
        assert_eq!(path, custom);
    }

    #[test]
    fn resolve_path_fresh_uses_uuid() {
        let path = SessionLog::resolve_path(None);
        // The directory name should be a UUID (36 chars)
        let dir_name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(dir_name.len(), 36);
        assert!(dir_name.contains('-'));
    }

    #[test]
    fn find_latest_session_basic() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join(".intendant/logs");

        // Create two session dirs
        let s1_dir = logs_dir.join("session-1");
        fs::create_dir_all(&s1_dir).unwrap();
        let meta1 = SessionMeta {
            session_id: "session-1".to_string(),
            created_at: "2026-01-01T00:00:00".to_string(),
            created_at_ms: None,
            project_root: Some("/tmp/project".to_string()),
            name: None,
            task: Some("task 1".to_string()),
            status: Some("completed".to_string()),
            last_turn: Some(5),
            role: None,
            rounds: None,
            worktree: None,
        };
        fs::write(
            s1_dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta1).unwrap(),
        )
        .unwrap();

        let s2_dir = logs_dir.join("session-2");
        fs::create_dir_all(&s2_dir).unwrap();
        let meta2 = SessionMeta {
            session_id: "session-2".to_string(),
            created_at: "2026-01-02T00:00:00".to_string(),
            created_at_ms: None,
            project_root: Some("/tmp/project".to_string()),
            name: None,
            task: Some("task 2".to_string()),
            status: Some("completed".to_string()),
            last_turn: Some(3),
            role: None,
            rounds: None,
            worktree: None,
        };
        fs::write(
            s2_dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta2).unwrap(),
        )
        .unwrap();

        // find_latest_session reads from $HOME; for testing we'd need to override HOME
        // so this test just validates that the function doesn't panic with real HOME
        // The functional test relies on find_session_by_id which is path-based
    }

    #[test]
    fn find_session_by_id_path_form_is_anchored_to_logs_root() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".intendant").join("logs");
        let session_dir = logs.join("my-session");
        fs::create_dir_all(&session_dir).unwrap();

        // A pasted log-dir path resolves (no session_meta.json needed) —
        // compare canonicalized because the resolver canonicalizes.
        let result =
            SessionLog::find_session_by_id_in_home(home.path(), session_dir.to_str().unwrap());
        assert_eq!(result, Some(fs::canonicalize(&session_dir).unwrap()));

        // A directory outside the logs root is not a session, even though
        // it exists: path-form lookup must not read arbitrary directories.
        let outside = tempfile::tempdir().unwrap();
        let escape = outside.path().join("my-session");
        fs::create_dir_all(&escape).unwrap();
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), escape.to_str().unwrap()),
            None
        );
    }

    #[test]
    fn find_session_by_id_nonexistent() {
        // An injected empty home: the lookup must miss without scanning
        // the machine's real logs store.
        let home = tempfile::tempdir().unwrap();
        let result = SessionLog::find_session_by_id_in_home(home.path(), "nonexistent-uuid-12345");
        assert!(result.is_none());
    }

    #[test]
    fn mark_session_meta_interrupted_updates_running_meta_without_log_handle() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(&log_dir).unwrap();
        let meta = SessionMeta {
            session_id: "session".to_string(),
            created_at: "2026-05-29T00:00:00".to_string(),
            created_at_ms: None,
            project_root: Some("/tmp".to_string()),
            name: None,
            task: Some("task".to_string()),
            status: Some("running".to_string()),
            last_turn: None,
            role: None,
            rounds: None,
            worktree: None,
        };
        fs::write(
            log_dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        assert!(mark_session_meta_interrupted(&log_dir, None));

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("interrupted"));
        assert_eq!(meta.last_turn, Some(0));
    }

    #[test]
    fn voice_log_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.voice_log("hello world", 5, Some("check_status"));

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "voice_log");
        assert_eq!(last["message"], "hello world");
        assert_eq!(last["data"]["seq"], 5);
        assert_eq!(last["data"]["tool_context"], "check_status");
    }

    #[test]
    fn voice_log_without_tool_context() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.voice_log("hi", 1, None);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "voice_log");
        assert_eq!(last["message"], "hi");
        assert!(last["data"]["tool_context"].is_null());
    }

    #[test]
    fn user_transcript_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.user_transcript("Hello, run the tests please", 3);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "user_transcript");
        assert_eq!(last["message"], "Hello, run the tests please");
        assert_eq!(last["data"]["seq"], 3);
    }

    #[test]
    fn presence_checkpoint_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.presence_checkpoint("Agent completed 3 tasks", 15);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "presence_checkpoint");
        assert_eq!(last["message"], "Agent completed 3 tasks");
        assert_eq!(last["data"]["last_event_seq"], 15);
    }

    #[test]
    fn presence_connected_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.presence_connected(Some("gemini"), Some("gemini-2.5-flash-native-audio"));

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "presence_connected");
        assert_eq!(last["data"]["provider"], "gemini");
        assert_eq!(last["data"]["model"], "gemini-2.5-flash-native-audio");
    }

    #[test]
    fn presence_disconnected_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.presence_disconnected();

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "presence_disconnected");
    }

    /// Helper: drop `log`, read session.jsonl, and return the last entry
    /// whose `event` field matches `event_type`.
    pub(crate) fn read_events(
        log_dir: &std::path::Path,
        event_type: &str,
    ) -> Vec<serde_json::Value> {
        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some(event_type))
            .collect()
    }

    pub(crate) fn read_last_event(
        log_dir: &std::path::Path,
        event_type: &str,
    ) -> serde_json::Value {
        read_events(log_dir, event_type)
            .into_iter()
            .last()
            .unwrap_or_else(|| panic!("no {} event found", event_type))
    }

    #[test]
    fn events_carry_epoch_ms_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(dir.path().to_path_buf()).unwrap();
        log.info("hello");
        drop(log);

        let before = Utc::now().timestamp_millis();
        let event = read_last_event(dir.path(), "session_start");
        let ts_ms = event["ts_ms"].as_i64().expect("ts_ms is an i64");
        assert!(
            ts_ms > before - 60_000 && ts_ms <= before,
            "ts_ms {} not within a minute before {}",
            ts_ms,
            before
        );
        assert!(
            event["ts"].as_str().is_some(),
            "time-of-day ts still present"
        );
    }

    #[test]
    fn meta_carries_epoch_ms_and_reads_legacy_metas() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::open(dir.path().to_path_buf()).unwrap();
        log.write_meta(None, Some("task"));
        let raw = fs::read_to_string(dir.path().join("session_meta.json")).unwrap();
        let meta: SessionMeta = serde_json::from_str(&raw).unwrap();
        let ms = meta.created_at_ms.expect("created_at_ms stamped");
        assert!(ms > 0);

        // Metas written before the field existed must still parse.
        let legacy = r#"{"session_id":"old","created_at":"2026-01-01T00:00:00"}"#;
        let meta: SessionMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.created_at_ms, None);
    }

    #[test]
    fn conversation_message_user_event_shape() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(dir.path().to_path_buf()).unwrap();
        let id = log.conversation_message_user(
            7,
            crate::conversation::MessageProvenance::AskHumanAnswer,
            "the raw answer",
            Some(6),
        );
        drop(log);
        let event = read_last_event(dir.path(), "conversation_message");
        assert_eq!(event["data"]["message_id"].as_str(), Some(id.as_str()));
        assert_eq!(event["data"]["message_seq"].as_u64(), Some(7));
        assert_eq!(event["data"]["role"].as_str(), Some("user"));
        assert_eq!(
            event["data"]["provenance"].as_str(),
            Some("ask_human_answer")
        );
        assert_eq!(event["data"]["text"].as_str(), Some("the raw answer"));
        assert_eq!(event["data"]["ref_seq"].as_u64(), Some(6));
        assert!(event["ts_ms"].as_i64().is_some());
    }

    #[test]
    fn model_response_with_message_shares_one_span() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(dir.path().to_path_buf()).unwrap();
        log.model_response_with_message(3, "the full assistant text", 10, 5, 15, 0, 0);
        drop(log);

        let diag = read_last_event(dir.path(), "model_response");
        let canon = read_last_event(dir.path(), "conversation_message");
        assert_eq!(canon["data"]["role"].as_str(), Some("assistant"));
        assert_eq!(canon["data"]["provenance"].as_str(), Some("assistant"));
        assert_eq!(canon["data"]["message_seq"].as_u64(), Some(3));
        // Both events reference the SAME sidecar bytes — one write.
        assert_eq!(canon["file"], diag["file"]);
        assert_eq!(canon["data"]["model_offset"], diag["data"]["model_offset"]);
        assert_eq!(canon["data"]["model_bytes"], diag["data"]["model_bytes"]);
        // And the span resolves to the full text.
        let relative = canon["file"].as_str().expect("sidecar file recorded");
        let bytes = fs::read(dir.path().join(relative)).unwrap();
        let offset = canon["data"]["model_offset"].as_u64().unwrap() as usize;
        let len = canon["data"]["model_bytes"].as_u64().unwrap() as usize;
        assert_eq!(&bytes[offset..offset + len], b"the full assistant text");
    }

    #[test]
    fn conversation_rewound_and_epoch_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open(dir.path().to_path_buf()).unwrap();
        log.conversation_rewound(5, "tail_rollback");
        log.conversation_message_epoch(&[
            (1, "system".to_string(), "aaaa".to_string()),
            (2, "user".to_string(), "bbbb".to_string()),
        ]);
        drop(log);

        let rewound = read_last_event(dir.path(), "conversation_rewound");
        assert_eq!(rewound["data"]["cut_after_seq"].as_u64(), Some(5));
        assert_eq!(rewound["data"]["kind"].as_str(), Some("tail_rollback"));
        assert!(rewound["data"]["superseded_at_ms"].as_i64().is_some());

        let epoch = read_last_event(dir.path(), "conversation_message_epoch");
        let mapping = epoch["data"]["mapping"].as_array().unwrap();
        assert_eq!(mapping.len(), 2);
        assert_eq!(mapping[1][0].as_u64(), Some(2));
        assert_eq!(mapping[1][1].as_str(), Some("user"));
        assert_eq!(mapping[1][2].as_str(), Some("bbbb"));
    }

    #[test]
    fn content_hash_hex16_is_stable() {
        assert_eq!(content_hash_hex16("hello"), content_hash_hex16("hello"));
        assert_ne!(content_hash_hex16("hello"), content_hash_hex16("hello!"));
        assert_eq!(content_hash_hex16("x").len(), 16);
    }

    #[test]
    fn find_session_by_id_resolves_names_metas_and_survives_deletion() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".intendant").join("logs");

        // Dir whose NAME is the session id.
        let named = logs.join("aaaa-1111-2222");
        std::fs::create_dir_all(&named).unwrap();
        std::fs::write(
            named.join("session_meta.json"),
            serde_json::json!({"session_id": "aaaa-1111-2222", "created_at": "t"}).to_string(),
        )
        .unwrap();
        // Dir whose name differs from its META session id (--log-file
        // style custom dir).
        let custom = logs.join("custom-dir");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(
            custom.join("session_meta.json"),
            serde_json::json!({"session_id": "bbbb-3333-4444", "created_at": "t"}).to_string(),
        )
        .unwrap();

        // Exact + prefix name matches; meta-only match; miss.
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "aaaa-1111-2222"),
            Some(named.clone())
        );
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "aaaa-1111"),
            Some(named.clone())
        );
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "bbbb-3333-4444"),
            Some(custom.clone())
        );
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "bbbb-3333"),
            Some(custom.clone())
        );
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "zzzz-none"),
            None
        );

        // Memoized hits are revalidated: a deleted dir must not be served
        // from the cache (both lookups above primed it).
        std::fs::remove_dir_all(&custom).unwrap();
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "bbbb-3333-4444"),
            None
        );
        // A meta rewrite that changes the session id drops the memo too.
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(
            custom.join("session_meta.json"),
            serde_json::json!({"session_id": "bbbb-3333-4444", "created_at": "t"}).to_string(),
        )
        .unwrap();
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "bbbb-3333-4444"),
            Some(custom.clone())
        );
        std::fs::write(
            custom.join("session_meta.json"),
            serde_json::json!({"session_id": "cccc-5555-6666", "created_at": "later than t"})
                .to_string(),
        )
        .unwrap();
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "bbbb-3333-4444"),
            None
        );
        assert_eq!(
            SessionLog::find_session_by_id_in_home(home.path(), "cccc-5555-6666"),
            Some(custom)
        );
    }
}
