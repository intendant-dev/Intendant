//! Live registry of backend-announced background tasks, per session — the
//! read side of the background-task inspector.
//!
//! External-agent adapters record only what their own wire proves. Claude
//! Code contributes its announced output-file path; Kimi contributes its
//! native task id and a bounded inline output preview fetched through the
//! session-owned server client. The web gateway's
//! `GET /api/session/{id}/background-tasks[/{task}/output]` routes read
//! this registry — clients name tasks, never paths or server endpoints.
//!
//! Keys are the backend-native session id (the id stamped on every wire
//! line, stable across resumes); routes resolve an Intendant wrapper id
//! to it through the persisted identity ladder, exactly like fork-points.
//! Finished tasks are retained (bounded) so a just-completed command's
//! output stays inspectable; a session's records are cleared when its
//! wrapper shuts down or the id is re-adopted by a fresh process —
//! without a live CLI nobody confirms task state, so keeping the rows
//! would claim knowledge the daemon no longer has.
//!
//! Core operations live on [`Registry`] (tests drive local instances —
//! the process global is shared, and eviction-shaped tests on it would
//! race sibling tests); the `pub(crate)` free functions are the global's
//! transport edge.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Lifecycle state of one recorded task, from the backend's own signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundTaskStatus {
    /// Armed by `task_started`; no notification yet.
    Running,
    Completed,
    Failed,
    /// Killed / cancelled / interrupted (e.g. a TaskStop).
    Stopped,
}

impl BackgroundTaskStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BackgroundTaskStatus::Running => "running",
            BackgroundTaskStatus::Completed => "completed",
            BackgroundTaskStatus::Failed => "failed",
            BackgroundTaskStatus::Stopped => "stopped",
        }
    }

    /// Map a wire status word (`task_notification.status`) to the
    /// terminal record state. Unknown words read as completed — the
    /// notification is authoritative that the task ENDED, and inventing
    /// a status vocabulary beyond the probed one would be guessing.
    pub(crate) fn from_wire_terminal(status: &str) -> Self {
        match status {
            "failed" | "error" | "errored" => BackgroundTaskStatus::Failed,
            "stopped" | "killed" | "cancelled" | "interrupted" => BackgroundTaskStatus::Stopped,
            _ => BackgroundTaskStatus::Completed,
        }
    }
}

/// One background task the backend announced. Output is present only when
/// the backend supplied it: an announced file for Claude Code, or a bounded
/// native task-output preview for Kimi. No backend statement means no peek
/// affordance; paths and loopback credentials never cross this boundary.
#[derive(Debug, Clone)]
pub(crate) struct BackgroundTaskRecord {
    /// Canonical external-agent source (`claude-code`, `kimi`).
    pub(crate) source: String,
    /// Backend task id (`task_started.task_id`) — the public handle the
    /// routes use.
    pub(crate) task_id: String,
    /// The launching tool_use id — the adapter's correlation key.
    pub(crate) tool_use_id: String,
    /// Short human description (the model's `description` input or the
    /// command head, as the adapter derived it).
    pub(crate) description: String,
    pub(crate) started_at_epoch: u64,
    pub(crate) ended_at_epoch: Option<u64>,
    pub(crate) status: BackgroundTaskStatus,
    pub(crate) output_file: Option<PathBuf>,
    /// Backend-returned output tail. This is intentionally data-only: the
    /// registry never retains a Kimi loopback client or bearer token.
    pub(crate) inline_output: Option<Vec<u8>>,
    /// Authoritative total byte count when the backend reports one.
    pub(crate) output_size_bytes: Option<u64>,
}

/// Hard ceiling for one retained inline output preview. It matches the
/// dashboard route's maximum response tail and bounds global registry memory.
pub(crate) const INLINE_OUTPUT_RETAINED_BYTES: usize = 256 * 1024;

/// Retained finished records per session — enough for "what just ran",
/// bounded so a long chatty session can't grow the registry unbounded.
const FINISHED_RETAINED_PER_SESSION: usize = 16;

/// Sessions retained in the registry. Eviction removes the
/// least-recently-updated session with no running tasks; sessions with
/// running tasks are never evicted (their wrapper is alive and will
/// clear them itself).
const SESSIONS_RETAINED: usize = 64;

struct SessionTasks {
    records: Vec<BackgroundTaskRecord>,
    /// Monotonic touch counter value at last update (eviction order).
    touched: u64,
}

/// The task store proper. All mutation and lookup semantics live here;
/// the module's free functions apply them to the process global.
pub(crate) struct Registry {
    sessions: HashMap<String, SessionTasks>,
    /// Monotonic counter backing `SessionTasks::touched`.
    clock: u64,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            clock: 0,
        }
    }

    fn touch(&mut self, session_id: &str) -> &mut SessionTasks {
        self.clock += 1;
        let clock = self.clock;
        let entry = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionTasks {
                records: Vec::new(),
                touched: 0,
            });
        entry.touched = clock;
        entry
    }

    /// Drop the oldest evictable sessions once past the cap.
    fn evict_stale_sessions(&mut self) {
        while self.sessions.len() > SESSIONS_RETAINED {
            let evictee = self
                .sessions
                .iter()
                .filter(|(_, tasks)| {
                    !tasks
                        .records
                        .iter()
                        .any(|record| record.status == BackgroundTaskStatus::Running)
                })
                .min_by_key(|(_, tasks)| tasks.touched)
                .map(|(id, _)| id.clone());
            match evictee {
                Some(id) => {
                    self.sessions.remove(&id);
                }
                // Every retained session has running tasks: nothing is
                // safely evictable, live wrappers own their cleanup.
                None => return,
            }
        }
    }

    /// A `task_started` for a main-thread background command: register
    /// it running. Re-announcing an id already registered running is a
    /// no-op (arming is idempotent, mirroring the adapter's armed set).
    pub(crate) fn record_started(
        &mut self,
        session_id: &str,
        task_id: &str,
        tool_use_id: &str,
        description: &str,
        started_at_epoch: u64,
    ) {
        self.record_started_for_source(
            session_id,
            "claude-code",
            task_id,
            tool_use_id,
            description,
            started_at_epoch,
        );
    }

    /// Backend-qualified form of [`Self::record_started`]. New adapters use
    /// this so shared read surfaces can report and route the task honestly.
    pub(crate) fn record_started_for_source(
        &mut self,
        session_id: &str,
        source: &str,
        task_id: &str,
        tool_use_id: &str,
        description: &str,
        started_at_epoch: u64,
    ) {
        let session_id = session_id.trim();
        let source = source.trim();
        let tool_use_id = tool_use_id.trim();
        if session_id.is_empty() || source.is_empty() || tool_use_id.is_empty() {
            return;
        }
        let entry = self.touch(session_id);
        if entry.records.iter().any(|record| {
            record.tool_use_id == tool_use_id && record.status == BackgroundTaskStatus::Running
        }) {
            return;
        }
        entry.records.push(BackgroundTaskRecord {
            source: source.to_string(),
            task_id: task_id.trim().to_string(),
            tool_use_id: tool_use_id.to_string(),
            description: description.to_string(),
            started_at_epoch,
            ended_at_epoch: None,
            status: BackgroundTaskStatus::Running,
            output_file: None,
            inline_output: None,
            output_size_bytes: None,
        });
        self.evict_stale_sessions();
    }

    /// Attach the launch-ack's announced output path to the running
    /// record for `tool_use_id`. First writer wins — the ack is the
    /// earliest statement and re-parses must not churn it; the
    /// notification's authoritative path lands via [`Self::record_finished`].
    pub(crate) fn record_output_file(
        &mut self,
        session_id: &str,
        tool_use_id: &str,
        output_file: PathBuf,
    ) {
        let Some(entry) = self.sessions.get_mut(session_id.trim()) else {
            return;
        };
        if let Some(record) = entry.records.iter_mut().find(|record| {
            record.tool_use_id == tool_use_id && record.status == BackgroundTaskStatus::Running
        }) {
            if record.output_file.is_none() {
                record.output_file = Some(output_file);
            }
        }
    }

    /// Cache a backend-returned output preview under its public task id. The
    /// tail is retained, never a prefix, so the dashboard's tail semantics
    /// stay useful when a backend returns more than the memory ceiling.
    pub(crate) fn record_inline_output(
        &mut self,
        session_id: &str,
        task_id: &str,
        output: &[u8],
        output_size_bytes: Option<u64>,
    ) {
        let Some(entry) = self.sessions.get_mut(session_id.trim()) else {
            return;
        };
        let task_id = task_id.trim();
        let Some(record) = entry
            .records
            .iter_mut()
            .find(|record| record.task_id == task_id)
        else {
            return;
        };
        let start = output.len().saturating_sub(INLINE_OUTPUT_RETAINED_BYTES);
        record.inline_output = Some(output[start..].to_vec());
        record.output_size_bytes = Some(
            output_size_bytes
                .unwrap_or(output.len() as u64)
                .max(output.len() as u64),
        );
    }

    /// The `task_notification` end: mark the record finished, adopting
    /// the notification's `output_file` when it names one (the
    /// authoritative final statement, so it overrides an ack-parsed
    /// path). Trims retained finished records past the per-session
    /// bound.
    pub(crate) fn record_finished(
        &mut self,
        session_id: &str,
        tool_use_id: &str,
        status: BackgroundTaskStatus,
        output_file: Option<PathBuf>,
        ended_at_epoch: u64,
    ) {
        let Some(entry) = self.sessions.get_mut(session_id.trim()) else {
            return;
        };
        if let Some(record) = entry.records.iter_mut().find(|record| {
            record.tool_use_id == tool_use_id && record.status == BackgroundTaskStatus::Running
        }) {
            record.status = status;
            record.ended_at_epoch = Some(ended_at_epoch);
            if let Some(path) = output_file {
                record.output_file = Some(path);
            }
        }
        // Retention: keep every running record; drop the oldest finished
        // ones (finishes land in wire order, so list order is age order)
        // past the bound.
        let finished = entry
            .records
            .iter()
            .filter(|record| record.status != BackgroundTaskStatus::Running)
            .count();
        if finished > FINISHED_RETAINED_PER_SESSION {
            let mut to_drop = finished - FINISHED_RETAINED_PER_SESSION;
            entry.records.retain(|record| {
                if to_drop > 0 && record.status != BackgroundTaskStatus::Running {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Forget a session's records — the wrapper shut down, or a fresh
    /// process re-adopted the id (a new CLI process does not own the old
    /// process's background children, so their state is unknowable).
    pub(crate) fn clear_session(&mut self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        self.sessions.remove(session_id);
    }

    /// Snapshot of a session's records, launch order. Empty when unknown.
    pub(crate) fn tasks_for_session(&self, session_id: &str) -> Vec<BackgroundTaskRecord> {
        self.sessions
            .get(session_id.trim())
            .map(|entry| entry.records.clone())
            .unwrap_or_default()
    }

    /// Whether the registry knows this session at all (distinguishes "no
    /// tasks recorded" from "session never seen").
    pub(crate) fn session_known(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id.trim())
    }

    /// Canonical source of a known session, if at least one task established
    /// it. Mixed-source rows under one backend-native id are refused instead
    /// of guessing which adapter owns the id.
    pub(crate) fn session_source(&self, session_id: &str) -> Option<String> {
        let records = &self.sessions.get(session_id.trim())?.records;
        let first = records.first()?.source.as_str();
        records
            .iter()
            .all(|record| record.source == first)
            .then(|| first.to_string())
    }

    /// The record for `task_id` in `session_id`, if any. THE lookup the
    /// output route serves paths from — the client's task id resolves to
    /// the registry's stored path or nothing.
    pub(crate) fn find_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Option<BackgroundTaskRecord> {
        let task_id = task_id.trim();
        if task_id.is_empty() {
            return None;
        }
        self.sessions
            .get(session_id.trim())?
            .records
            .iter()
            .find(|record| record.task_id == task_id)
            .cloned()
    }
}

fn global() -> std::sync::MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Mutex::new(Registry::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn record_started(
    session_id: &str,
    task_id: &str,
    tool_use_id: &str,
    description: &str,
    started_at_epoch: u64,
) {
    global().record_started(
        session_id,
        task_id,
        tool_use_id,
        description,
        started_at_epoch,
    );
}

pub(crate) fn record_started_for_source(
    session_id: &str,
    source: &str,
    task_id: &str,
    tool_use_id: &str,
    description: &str,
    started_at_epoch: u64,
) {
    global().record_started_for_source(
        session_id,
        source,
        task_id,
        tool_use_id,
        description,
        started_at_epoch,
    );
}

pub(crate) fn record_output_file(session_id: &str, tool_use_id: &str, output_file: PathBuf) {
    global().record_output_file(session_id, tool_use_id, output_file);
}

pub(crate) fn record_inline_output(
    session_id: &str,
    task_id: &str,
    output: &[u8],
    output_size_bytes: Option<u64>,
) {
    global().record_inline_output(session_id, task_id, output, output_size_bytes);
}

pub(crate) fn record_finished(
    session_id: &str,
    tool_use_id: &str,
    status: BackgroundTaskStatus,
    output_file: Option<PathBuf>,
    ended_at_epoch: u64,
) {
    global().record_finished(session_id, tool_use_id, status, output_file, ended_at_epoch);
}

pub(crate) fn clear_session(session_id: &str) {
    global().clear_session(session_id);
}

pub(crate) fn tasks_for_session(session_id: &str) -> Vec<BackgroundTaskRecord> {
    global().tasks_for_session(session_id)
}

pub(crate) fn session_known(session_id: &str) -> bool {
    global().session_known(session_id)
}

pub(crate) fn session_source(session_id: &str) -> Option<String> {
    global().session_source(session_id)
}

pub(crate) fn find_task(session_id: &str, task_id: &str) -> Option<BackgroundTaskRecord> {
    global().find_task(session_id, task_id)
}

/// Parse the CLI's launch-ack sentence for the announced output path:
/// `… Output is being written to: <path>. You will be notified …`
/// (live-probed shape, Claude Code 2.1.211). The path is cut at the
/// known follow-on sentences so embedded spaces survive; a final lone
/// period is trimmed when the ack ends at the path. Only absolute paths
/// are accepted — a relative path would be ambiguous about whose cwd
/// anchors it, and guessing is exactly what this feature refuses to do.
pub(crate) fn parse_output_path_from_ack(text: &str) -> Option<PathBuf> {
    const MARKER: &str = "Output is being written to: ";
    let at = text.find(MARKER)? + MARKER.len();
    // Wire-first: paths never span lines — only the marker's own line
    // can carry one (a newline right after the marker means no path).
    let line = text[at..].lines().next().unwrap_or("");
    let cut = [". You will be notified", ". To check interim output"]
        .iter()
        .filter_map(|terminator| line.find(terminator))
        .min();
    let candidate = match cut {
        Some(cut) => line[..cut].trim(),
        // The ack ended at the path: trim its sentence-final period.
        None => line.trim().trim_end_matches('.').trim_end(),
    };
    if candidate.is_empty() {
        return None;
    }
    let path = PathBuf::from(candidate);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_ack_finished_round_trip() {
        let mut reg = Registry::new();
        let sid = "session-a";
        reg.record_started(sid, "task-a", "toolu_a", "sleep 8 && echo done", 100);
        let tasks = reg.tasks_for_session(sid);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "task-a");
        assert_eq!(tasks[0].source, "claude-code");
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Running);
        assert_eq!(tasks[0].started_at_epoch, 100);
        assert!(tasks[0].output_file.is_none());
        // Re-announcing a running id is a no-op (idempotent arming).
        reg.record_started(sid, "task-a", "toolu_a", "sleep 8 && echo done", 101);
        assert_eq!(reg.tasks_for_session(sid).len(), 1);

        // Ack path attaches once; a second parse never churns it.
        reg.record_output_file(sid, "toolu_a", PathBuf::from("/tmp/tasks/a.output"));
        reg.record_output_file(sid, "toolu_a", PathBuf::from("/tmp/tasks/other.output"));
        let task = reg.find_task(sid, "task-a").expect("registered");
        assert_eq!(
            task.output_file.as_deref(),
            Some(std::path::Path::new("/tmp/tasks/a.output"))
        );

        // The notification finishes the record and its path wins.
        reg.record_finished(
            sid,
            "toolu_a",
            BackgroundTaskStatus::Completed,
            Some(PathBuf::from("/tmp/tasks/a-final.output")),
            160,
        );
        let task = reg.find_task(sid, "task-a").expect("retained after finish");
        assert_eq!(task.status, BackgroundTaskStatus::Completed);
        assert_eq!(task.ended_at_epoch, Some(160));
        assert_eq!(
            task.output_file.as_deref(),
            Some(std::path::Path::new("/tmp/tasks/a-final.output"))
        );
        assert!(reg.session_known(sid));
        reg.clear_session(sid);
        assert!(!reg.session_known(sid));
        assert!(reg.find_task(sid, "task-a").is_none());
    }

    #[test]
    fn kimi_inline_output_is_source_qualified_and_tail_bounded() {
        let mut reg = Registry::new();
        let sid = "session-kimi";
        reg.record_started_for_source(sid, "kimi", "task-k", "task-k", "native process", 100);
        let mut output = vec![b'a'; INLINE_OUTPUT_RETAINED_BYTES + 3];
        output[INLINE_OUTPUT_RETAINED_BYTES] = b'X';
        output[INLINE_OUTPUT_RETAINED_BYTES + 1] = b'Y';
        output[INLINE_OUTPUT_RETAINED_BYTES + 2] = b'Z';
        reg.record_inline_output(sid, "task-k", &output, Some(900_000));

        let record = reg.find_task(sid, "task-k").expect("Kimi task");
        assert_eq!(record.source, "kimi");
        assert_eq!(reg.session_source(sid).as_deref(), Some("kimi"));
        assert_eq!(
            record.inline_output.as_ref().map(Vec::len),
            Some(INLINE_OUTPUT_RETAINED_BYTES)
        );
        assert_eq!(
            record
                .inline_output
                .as_deref()
                .and_then(|bytes| bytes.last()),
            Some(&b'Z')
        );
        assert_eq!(record.output_size_bytes, Some(900_000));
        assert!(record.output_file.is_none());
    }

    #[test]
    fn finished_retention_is_bounded_and_running_survive() {
        let mut reg = Registry::new();
        let sid = "session-retention";
        reg.record_started(sid, "task-live", "toolu_live", "long job", 1);
        for index in 0..(FINISHED_RETAINED_PER_SESSION + 5) {
            let tool = format!("toolu_{index}");
            reg.record_started(sid, &format!("task-{index}"), &tool, "short job", 2);
            reg.record_finished(sid, &tool, BackgroundTaskStatus::Completed, None, 3);
        }
        let tasks = reg.tasks_for_session(sid);
        let running: Vec<_> = tasks
            .iter()
            .filter(|record| record.status == BackgroundTaskStatus::Running)
            .collect();
        assert_eq!(running.len(), 1, "the running record is never trimmed");
        assert_eq!(running[0].task_id, "task-live");
        assert_eq!(tasks.len() - 1, FINISHED_RETAINED_PER_SESSION);
        // The oldest finished rows were dropped, the newest survive.
        assert!(reg.find_task(sid, "task-0").is_none());
        assert!(reg
            .find_task(sid, &format!("task-{}", FINISHED_RETAINED_PER_SESSION + 4))
            .is_some());
    }

    #[test]
    fn session_eviction_spares_running_sessions() {
        let mut reg = Registry::new();
        reg.record_started("session-live", "task-r", "toolu_r", "still running", 1);
        for index in 0..(SESSIONS_RETAINED + 8) {
            let sid = format!("session-flood-{index:04}");
            reg.record_started(&sid, "task-f", "toolu_f", "quick", 2);
            reg.record_finished(&sid, "toolu_f", BackgroundTaskStatus::Completed, None, 3);
        }
        assert!(
            reg.session_known("session-live"),
            "a session with running tasks is never evicted"
        );
        assert!(
            !reg.session_known("session-flood-0000"),
            "the least-recently-updated finished session evicts first"
        );
        assert!(reg.sessions.len() <= SESSIONS_RETAINED + 1);
    }

    #[test]
    fn ack_path_parses_probed_shape_and_refuses_relative() {
        // The CLI emits host-native absolute paths, and `is_absolute` is
        // a host judgment ("/tmp/x" is NOT absolute on Windows) — so the
        // fixtures are host-shaped too.
        let abs_spaced = if cfg!(windows) {
            "C:\\claude\\tasks dir\\b9lkjn0bv.output"
        } else {
            "/tmp/claude/tasks dir/b9lkjn0bv.output"
        };
        let abs_plain = if cfg!(windows) {
            "C:\\t\\x.output"
        } else {
            "/tmp/t/x.output"
        };
        let ack = format!(
            "Command running in background with ID: b9lkjn0bv. \
             Output is being written to: {abs_spaced}. \
             You will be notified when it completes. To check interim output, \
             use Read on that file path."
        );
        assert_eq!(
            parse_output_path_from_ack(&ack).as_deref(),
            Some(std::path::Path::new(abs_spaced)),
            "embedded spaces survive the sentence cut"
        );
        // Ack ending at the path: the lone trailing period trims.
        assert_eq!(
            parse_output_path_from_ack(&format!("Output is being written to: {abs_plain}."))
                .as_deref(),
            Some(std::path::Path::new(abs_plain))
        );
        // No marker, relative path, or empty remainder: no path, no guess.
        assert!(parse_output_path_from_ack("Command running in background with ID: x.").is_none());
        assert!(
            parse_output_path_from_ack("Output is being written to: tasks/x.output.").is_none()
        );
        assert!(parse_output_path_from_ack("Output is being written to: ").is_none());
        assert!(
            parse_output_path_from_ack(&format!("Output is being written to: \n{abs_plain}"))
                .is_none(),
            "a newline before any path text means no path on the marker line"
        );
    }

    #[test]
    fn wire_terminal_status_mapping_is_pinned() {
        use BackgroundTaskStatus as S;
        for (wire, status) in [
            ("completed", S::Completed),
            ("success", S::Completed),
            ("failed", S::Failed),
            ("error", S::Failed),
            ("errored", S::Failed),
            ("stopped", S::Stopped),
            ("killed", S::Stopped),
            ("cancelled", S::Stopped),
            ("interrupted", S::Stopped),
            ("some_future_word", S::Completed),
        ] {
            assert_eq!(S::from_wire_terminal(wire), status, "{wire}");
        }
    }
}
