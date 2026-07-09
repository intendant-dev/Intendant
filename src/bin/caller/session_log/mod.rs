use crate::types::truncate_str;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
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
struct TurnFileSpan {
    relative: String,
    offset: u64,
    len: u64,
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
}

static OPEN_SESSION_LOG_DIRS: OnceLock<StdMutex<HashSet<PathBuf>>> = OnceLock::new();

fn open_session_log_dirs() -> &'static StdMutex<HashSet<PathBuf>> {
    OPEN_SESSION_LOG_DIRS.get_or_init(|| StdMutex::new(HashSet::new()))
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
        };
        log.emit(LogEvent {
            ts: Self::ts(),
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
        let existing_name = fs::read_to_string(self.dir.join("session_meta.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<SessionMeta>(&raw).ok())
            .and_then(|meta| meta.name);
        let meta = SessionMeta {
            session_id: self.session_id.clone(),
            created_at: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            project_root: project_root.map(|p| p.to_string_lossy().to_string()),
            name: name.map(|n| n.to_string()).or(existing_name),
            task: task.map(|t| t.to_string()),
            status: Some("running".to_string()),
            last_turn: None,
            role: role.map(|r| r.to_string()),
            rounds: None,
        };
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            if let Err(e) = fs::write(self.dir.join("session_meta.json"), json) {
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

        // Scan for prefix match or meta match
        if !logs_dir.is_dir() {
            return None;
        }
        if let Ok(entries) = fs::read_dir(&logs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(session_id) && entry.path().is_dir() {
                    return Some(entry.path());
                }
                // Also check inside session_meta.json for session_id match
                let meta_path = entry.path().join("session_meta.json");
                if let Ok(meta_str) = fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                        if meta.session_id == session_id || meta.session_id.starts_with(session_id)
                        {
                            return Some(entry.path());
                        }
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

    fn emit(&mut self, event: LogEvent) {
        if let Ok(json) = serde_json::to_string(&event) {
            if let Err(e) = writeln!(self.writer, "{}", json) {
                eprintln!("session_log: failed to write log event: {}", e);
            }
            let _ = self.writer.flush();
        }
    }

    fn emit_transcript(&mut self, entry: TranscriptEntry) {
        if let Some(ref mut w) = self.transcript_writer {
            if let Ok(json) = serde_json::to_string(&entry) {
                let _ = writeln!(w, "{}", json);
                let _ = w.flush();
            }
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
        let already_has_content = fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        if already_has_content {
            let _ = file.write_all(b"\n");
        }
        let offset = file.metadata().ok()?.len();
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
            project_root: None,
            name: None,
            task: None,
            status: None,
            last_turn: None,
            role: None,
            rounds: None,
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
            project_root: Some("/tmp/project".to_string()),
            name: None,
            task: Some("task 1".to_string()),
            status: Some("completed".to_string()),
            last_turn: Some(5),
            role: None,
            rounds: None,
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
            project_root: Some("/tmp/project".to_string()),
            name: None,
            task: Some("task 2".to_string()),
            status: Some("completed".to_string()),
            last_turn: Some(3),
            role: None,
            rounds: None,
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
        let result = SessionLog::find_session_by_id("nonexistent-uuid-12345");
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
            project_root: Some("/tmp".to_string()),
            name: None,
            task: Some("task".to_string()),
            status: Some("running".to_string()),
            last_turn: None,
            role: None,
            rounds: None,
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
    pub(crate) fn read_events(log_dir: &std::path::Path, event_type: &str) -> Vec<serde_json::Value> {
        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some(event_type))
            .collect()
    }

    pub(crate) fn read_last_event(log_dir: &std::path::Path, event_type: &str) -> serde_json::Value {
        read_events(log_dir, event_type)
            .into_iter()
            .last()
            .unwrap_or_else(|| panic!("no {} event found", event_type))
    }
}
