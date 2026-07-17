//! `GET /api/session/{id}/background-tasks` and
//! `GET /api/session/{id}/background-tasks/{task}/output` — the
//! background-task inspector (tunnel twins `api_session_background_tasks`
//! / `api_session_background_task_output`).
//!
//! Read-only peek at what a supervised Claude Code session's background
//! commands are doing, served from the adapter-fed registry
//! (`crate::background_tasks`). `{id}` resolves like fork-points: an
//! Intendant wrapper session resolves to its persisted backend identity
//! first, then the id is tried as a backend session id directly.
//!
//! Security invariants (unit-tested below):
//! - Output is served ONLY from the registry's stored path — the client
//!   names a task id, never a path, and an unknown task is a 404.
//! - Reads are tail-capped: `tail_kb` defaults to
//!   [`DEFAULT_TAIL_KB`] and clamps to [`MAX_TAIL_KB`].
//! - The stored path is opened directly and must be a regular file both
//!   by pre-open `symlink_metadata` (a symlink leaf is refused — the
//!   registry recorded a file, not a link to follow) and by post-open
//!   handle metadata (what was actually opened).
//! - A vanished file is a 404, never an empty 200 pretending the output
//!   exists.

use super::*;
use crate::background_tasks::{BackgroundTaskRecord, BackgroundTaskStatus};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Default tail size served when `tail_kb` is absent.
pub(crate) const DEFAULT_TAIL_KB: u64 = 64;
/// Hard ceiling for one output read.
pub(crate) const MAX_TAIL_KB: u64 = 256;

pub(crate) async fn handle_session_background_tasks(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = session_background_tasks_response(request_line, &crate::platform::home_dir());
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_session_background_task_output(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response =
        session_background_task_output_response(request_line, &crate::platform::home_dir());
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// How `{id}` resolved: the registry key to serve, or an honest
/// unsupported/unknown verdict.
enum SessionResolution {
    /// Serve the registry under this backend session id — resolved
    /// through a persisted wrapper identity (which vouches the session
    /// exists even when the registry has no records for it yet), or the
    /// id itself when the registry already knows it.
    ClaudeCode { key: String },
    /// A wrapper session of a backend without a background-task wire.
    Unsupported { source: String },
    /// Nothing known under this id anywhere.
    Unknown,
}

/// The fork-points resolution ladder, specialized: wrapper identity
/// first, then the id as a backend session id the registry knows.
fn resolve_session(session_id: &str, home: &Path) -> SessionResolution {
    if let Some((source, backend_id)) =
        crate::session_supervisor::persisted_external_identity_for_session_in_home(home, session_id)
    {
        return if source == "claude-code" {
            SessionResolution::ClaudeCode { key: backend_id }
        } else {
            SessionResolution::Unsupported { source }
        };
    }
    if crate::background_tasks::session_known(session_id) {
        return SessionResolution::ClaudeCode {
            key: session_id.to_string(),
        };
    }
    SessionResolution::Unknown
}

fn task_json(record: &BackgroundTaskRecord) -> serde_json::Value {
    let mut task = serde_json::json!({
        "taskId": record.task_id,
        "description": record.description,
        "status": record.status.as_str(),
        "startedAtEpoch": record.started_at_epoch,
        // The peek affordance: true only when the wire announced a path
        // (the path itself is deliberately NOT serialized — clients name
        // tasks, the daemon resolves paths).
        "hasOutput": record.output_file.is_some(),
    });
    if let Some(ended) = record.ended_at_epoch {
        task["endedAtEpoch"] = serde_json::json!(ended);
    }
    (record.status == BackgroundTaskStatus::Running)
        .then(|| task["running"] = serde_json::json!(true));
    task
}

/// Transport edge: resolves the real home; tests inject a temp home.
pub(crate) fn session_background_tasks_response(request_line: &str, home: &Path) -> ApiResponse {
    let Some(session_id) = background_tasks_session_id(request_line) else {
        return ApiResponse::json_error(400, "session id missing in background-tasks path");
    };
    match resolve_session(&session_id, home) {
        SessionResolution::ClaudeCode { key } => {
            let tasks: Vec<serde_json::Value> = crate::background_tasks::tasks_for_session(&key)
                .iter()
                .map(task_json)
                .collect();
            ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({
                    "session": session_id,
                    "source": "claude-code",
                    "supported": true,
                    "tasks": tasks,
                })),
            )
        }
        SessionResolution::Unsupported { source } => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "session": session_id,
                "source": source,
                "supported": false,
                "reason": background_tasks_unsupported_reason(&source),
                "tasks": [],
            })),
        ),
        SessionResolution::Unknown => ApiResponse::json_error(
            404,
            format!("session {session_id} has no background-task records"),
        ),
    }
}

/// Honest per-backend absence wording (scope: supervised Claude Code on
/// this daemon only — see the PR that introduced the inspector).
fn background_tasks_unsupported_reason(source: &str) -> String {
    match source {
        "codex" => "codex has no non-blocking shell on today's wire".to_string(),
        "intendant" => {
            "native sessions' background work rides the Monitor/run_in_background lane, \
             not yet surfaced here"
                .to_string()
        }
        other => format!("background-task inspection is not implemented for {other} sessions"),
    }
}

/// Transport edge for the output peek; tests inject a temp home.
pub(crate) fn session_background_task_output_response(
    request_line: &str,
    home: &Path,
) -> ApiResponse {
    let Some((session_id, task_id)) = background_task_output_ids(request_line) else {
        return ApiResponse::json_error(400, "session or task id missing in output path");
    };
    let key = match resolve_session(&session_id, home) {
        SessionResolution::ClaudeCode { key } => key,
        SessionResolution::Unsupported { source } => {
            return ApiResponse::json_error(
                404,
                format!("background-task output is not available for {source} sessions"),
            );
        }
        SessionResolution::Unknown => {
            return ApiResponse::json_error(
                404,
                format!("session {session_id} has no background-task records"),
            );
        }
    };
    // THE path authority: the registry's stored record. A task id the
    // registry doesn't know — whatever path shenanigans the caller
    // imagines — is simply a 404.
    let Some(record) = crate::background_tasks::find_task(&key, &task_id) else {
        return ApiResponse::json_error(404, format!("unknown background task {task_id}"));
    };
    let Some(path) = record.output_file.as_deref() else {
        return ApiResponse::json_error(
            404,
            format!("no output file was announced for background task {task_id}"),
        );
    };
    let tail_bytes = tail_bytes_from_request(request_line);
    match read_output_tail(path, tail_bytes) {
        Ok(tail) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "taskId": record.task_id,
                "description": record.description,
                "status": record.status.as_str(),
                "sizeBytes": tail.size_bytes,
                "offset": tail.offset,
                "truncated": tail.offset > 0,
                "content": tail.content,
            })),
        ),
        Err(TailError::Gone) => ApiResponse::json_error(
            404,
            format!("output file for background task {task_id} is gone"),
        ),
        Err(TailError::NotRegular) => {
            ApiResponse::json_error(403, "refusing to read a non-regular output file")
        }
        Err(TailError::Io(err)) => {
            ApiResponse::json_error(500, format!("failed to read output tail: {err}"))
        }
    }
}

/// `tail_kb` from the query string, defaulted and clamped to the caps.
fn tail_bytes_from_request(request_line: &str) -> u64 {
    let tail_kb = query_param(request_line, "tail_kb")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TAIL_KB)
        .clamp(1, MAX_TAIL_KB);
    tail_kb * 1024
}

struct OutputTail {
    size_bytes: u64,
    offset: u64,
    content: String,
}

enum TailError {
    /// Missing file (or a dangling symlink leaf): the task's output is
    /// no longer where the wire said.
    Gone,
    /// The leaf exists but is not a regular file (symlink, directory,
    /// fifo, device): refused, never followed or read.
    NotRegular,
    Io(std::io::Error),
}

/// Read the last `tail_bytes` of `path`, refusing non-regular leaves.
/// Both checks matter: `symlink_metadata` refuses a symlink at the leaf
/// BEFORE any open could follow it, and the opened handle's metadata
/// re-verifies what was actually opened (a swap between the two shows
/// up here as a non-file).
fn read_output_tail(path: &Path, tail_bytes: u64) -> Result<OutputTail, TailError> {
    let leaf = std::fs::symlink_metadata(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            TailError::Gone
        } else {
            TailError::Io(err)
        }
    })?;
    if !leaf.file_type().is_file() {
        return Err(TailError::NotRegular);
    }
    let mut file = std::fs::File::open(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            TailError::Gone
        } else {
            TailError::Io(err)
        }
    })?;
    let opened = file.metadata().map_err(TailError::Io)?;
    if !opened.is_file() {
        return Err(TailError::NotRegular);
    }
    let size_bytes = opened.len();
    let offset = size_bytes.saturating_sub(tail_bytes);
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).map_err(TailError::Io)?;
    }
    // `take` bounds the read even if the file grows mid-read.
    let mut raw = Vec::with_capacity(tail_bytes.min(size_bytes) as usize);
    file.take(tail_bytes)
        .read_to_end(&mut raw)
        .map_err(TailError::Io)?;
    Ok(OutputTail {
        size_bytes,
        offset,
        content: String::from_utf8_lossy(&raw).into_owned(),
    })
}

/// `{id}` from `GET /api/session/{id}/background-tasks[?…]`.
fn background_tasks_session_id(request_line: &str) -> Option<String> {
    let mut segments = api_session_path_segments(request_line)?;
    let id = segments.next()?.trim();
    if id.is_empty() || segments.next() != Some("background-tasks") || segments.next().is_some() {
        return None;
    }
    Some(id.to_string())
}

/// `({id}, {task})` from
/// `GET /api/session/{id}/background-tasks/{task}/output[?…]`.
fn background_task_output_ids(request_line: &str) -> Option<(String, String)> {
    let mut segments = api_session_path_segments(request_line)?;
    let id = segments.next()?.trim();
    if id.is_empty() || segments.next() != Some("background-tasks") {
        return None;
    }
    let task = segments.next()?.trim();
    if task.is_empty() || segments.next() != Some("output") || segments.next().is_some() {
        return None;
    }
    Some((id.to_string(), task.to_string()))
}

/// The path segments after `/api/session/` (query stripped), or `None`
/// when the request is not under it.
fn api_session_path_segments<'a>(
    request_line: &'a str,
) -> Option<impl Iterator<Item = &'a str> + 'a> {
    let path = request_line.split_whitespace().nth(1)?;
    let path = path.split('?').next().unwrap_or(path);
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next() != Some("api") || segments.next() != Some("session") {
        return None;
    }
    Some(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_json(response: ApiResponse) -> (u16, serde_json::Value) {
        match response {
            ApiResponse::Json { status, body, .. } => (
                status,
                serde_json::from_str(&body.into_string()).expect("json body"),
            ),
            _ => panic!("expected a JSON response"),
        }
    }

    fn list_line(id: &str) -> String {
        format!("GET /api/session/{id}/background-tasks HTTP/1.1")
    }

    fn output_line(id: &str, task: &str, query: &str) -> String {
        format!("GET /api/session/{id}/background-tasks/{task}/output{query} HTTP/1.1")
    }

    /// Seed one finished + one running record under a unique registry
    /// key (the registry is process-global; unique ids keep tests
    /// hermetic against each other).
    fn seed_session(sid: &str, output: Option<&Path>) {
        crate::background_tasks::clear_session(sid);
        crate::background_tasks::record_started(sid, "task-run", "toolu_run", "long build", 100);
        if let Some(path) = output {
            crate::background_tasks::record_output_file(sid, "toolu_run", path.to_path_buf());
        }
        crate::background_tasks::record_started(sid, "task-done", "toolu_done", "quick check", 90);
        crate::background_tasks::record_finished(
            sid,
            "toolu_done",
            crate::background_tasks::BackgroundTaskStatus::Completed,
            None,
            95,
        );
    }

    #[test]
    fn list_serves_registry_records_without_paths() {
        let home = tempfile::tempdir().expect("home");
        let out = tempfile::NamedTempFile::new().expect("output file");
        let sid = "bgroute-list-0001";
        seed_session(sid, Some(out.path()));

        let (status, body) = response_json(session_background_tasks_response(
            &list_line(sid),
            home.path(),
        ));
        assert_eq!(status, 200);
        assert_eq!(body["supported"], true);
        assert_eq!(body["source"], "claude-code");
        let tasks = body["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["taskId"], "task-run");
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["hasOutput"], true);
        assert_eq!(tasks[0]["startedAtEpoch"], 100);
        assert_eq!(tasks[1]["taskId"], "task-done");
        assert_eq!(tasks[1]["status"], "completed");
        assert_eq!(tasks[1]["hasOutput"], false);
        assert_eq!(tasks[1]["endedAtEpoch"], 95);
        // The invariant in the wire shape itself: no path ever leaves.
        assert!(
            !serde_json::to_string(&body)
                .expect("body")
                .contains("output_file")
                && !serde_json::to_string(&body)
                    .expect("body")
                    .contains("outputFile"),
            "paths never serialize"
        );
        crate::background_tasks::clear_session(sid);
    }

    #[test]
    fn unknown_session_is_404_on_both_routes() {
        let home = tempfile::tempdir().expect("home");
        let (status, _) = response_json(session_background_tasks_response(
            &list_line("bgroute-never-seen"),
            home.path(),
        ));
        assert_eq!(status, 404);
        let (status, body) = response_json(session_background_task_output_response(
            &output_line("bgroute-never-seen", "task-x", ""),
            home.path(),
        ));
        assert_eq!(status, 404);
        assert!(body["error"].as_str().is_some());
    }

    #[test]
    fn output_tail_defaults_and_caps() {
        let home = tempfile::tempdir().expect("home");
        let dir = tempfile::tempdir().expect("outputs");
        let path = dir.path().join("task.output");
        // 300 KiB of 'x' with a distinctive tail marker.
        let mut body = vec![b'x'; 300 * 1024];
        let marker = b"TAIL-MARKER-END";
        let len = body.len();
        body[len - marker.len()..].copy_from_slice(marker);
        std::fs::write(&path, &body).expect("write output");

        let sid = "bgroute-caps-0001";
        seed_session(sid, Some(&path));

        // Default: 64 KiB from the end.
        let (status, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-run", ""),
            home.path(),
        ));
        assert_eq!(status, 200);
        assert_eq!(json["sizeBytes"], 300 * 1024);
        assert_eq!(json["offset"], (300 - 64) * 1024);
        assert_eq!(json["truncated"], true);
        let content = json["content"].as_str().expect("content");
        assert_eq!(content.len(), 64 * 1024);
        assert!(content.ends_with("TAIL-MARKER-END"));

        // Requests above the ceiling clamp to 256 KiB.
        let (status, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-run", "?tail_kb=100000"),
            home.path(),
        ));
        assert_eq!(status, 200);
        assert_eq!(json["offset"], (300 - 256) * 1024);
        assert_eq!(json["content"].as_str().expect("content").len(), 256 * 1024);

        // A small explicit tail is honored; zero clamps up to 1 KiB.
        let (_, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-run", "?tail_kb=1"),
            home.path(),
        ));
        assert_eq!(json["content"].as_str().expect("content").len(), 1024);
        let (_, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-run", "?tail_kb=0"),
            home.path(),
        ));
        assert_eq!(json["content"].as_str().expect("content").len(), 1024);

        // A file smaller than the tail arrives whole, untruncated.
        std::fs::write(&path, b"short output").expect("rewrite");
        let (_, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-run", ""),
            home.path(),
        ));
        assert_eq!(json["offset"], 0);
        assert_eq!(json["truncated"], false);
        assert_eq!(json["content"], "short output");
        crate::background_tasks::clear_session(sid);
    }

    #[test]
    fn output_unknown_task_vanished_file_and_no_path_are_404() {
        let home = tempfile::tempdir().expect("home");
        let dir = tempfile::tempdir().expect("outputs");
        let path = dir.path().join("gone.output");
        std::fs::write(&path, b"soon gone").expect("write");

        let sid = "bgroute-404s-0001";
        seed_session(sid, Some(&path));

        // Unknown task id: 404 — the registry is the only path source.
        let (status, _) = response_json(session_background_task_output_response(
            &output_line(sid, "task-imaginary", ""),
            home.path(),
        ));
        assert_eq!(status, 404);

        // A task the wire never announced a path for: 404, no guessing.
        let (status, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-done", ""),
            home.path(),
        ));
        assert_eq!(status, 404);
        assert!(json["error"]
            .as_str()
            .expect("error")
            .contains("no output file was announced"));

        // The file vanishing after registration: 404, not an empty 200.
        std::fs::remove_file(&path).expect("remove");
        let (status, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-run", ""),
            home.path(),
        ));
        assert_eq!(status, 404);
        assert!(json["error"].as_str().expect("error").contains("gone"));
        crate::background_tasks::clear_session(sid);
    }

    /// A symlink at the recorded leaf is refused outright — the registry
    /// recorded a file the CLI announced, and following links from there
    /// would let a task swap its output for any readable path.
    #[cfg(unix)]
    #[test]
    fn output_refuses_symlink_and_non_regular_leaves() {
        let home = tempfile::tempdir().expect("home");
        let dir = tempfile::tempdir().expect("outputs");
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, b"not for the inspector").expect("write secret");
        let link = dir.path().join("task.output");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");

        let sid = "bgroute-symlink-0001";
        seed_session(sid, Some(&link));
        let (status, json) = response_json(session_background_task_output_response(
            &output_line(sid, "task-run", ""),
            home.path(),
        ));
        assert_eq!(status, 403);
        assert!(json["error"]
            .as_str()
            .expect("error")
            .contains("non-regular"));

        // A directory leaf is refused the same way.
        let sid_dir = "bgroute-dirleaf-0001";
        seed_session(sid_dir, Some(dir.path()));
        let (status, _) = response_json(session_background_task_output_response(
            &output_line(sid_dir, "task-run", ""),
            home.path(),
        ));
        assert_eq!(status, 403);
        crate::background_tasks::clear_session(sid);
        crate::background_tasks::clear_session(sid_dir);
    }

    #[test]
    fn id_extraction_rejects_malformed_paths() {
        assert_eq!(
            background_tasks_session_id("GET /api/session/abc/background-tasks HTTP/1.1")
                .as_deref(),
            Some("abc")
        );
        assert!(
            background_tasks_session_id("GET /api/session//background-tasks HTTP/1.1").is_none()
        );
        assert!(background_tasks_session_id("GET /api/session/abc/frames HTTP/1.1").is_none());
        assert!(background_tasks_session_id(
            "GET /api/session/abc/background-tasks/extra HTTP/1.1"
        )
        .is_none());
        assert_eq!(
            background_task_output_ids(
                "GET /api/session/abc/background-tasks/t9/output?tail_kb=4 HTTP/1.1"
            ),
            Some(("abc".to_string(), "t9".to_string()))
        );
        assert!(background_task_output_ids(
            "GET /api/session/abc/background-tasks//output HTTP/1.1"
        )
        .is_none());
        assert!(
            background_task_output_ids("GET /api/session/abc/background-tasks/t9 HTTP/1.1")
                .is_none()
        );
        assert!(background_task_output_ids(
            "GET /api/session/abc/background-tasks/t9/output/extra HTTP/1.1"
        )
        .is_none());
    }
}
