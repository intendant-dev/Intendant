//! `GET /api/session/{id}/fork-points` — the unified fork-point catalog
//! (tunnel twin `api_session_fork_points`).
//!
//! Resolution ladder for `{id}`: an Intendant wrapper session resolves to
//! its canonical external identity first (so the catalog is derived from
//! the backend's own transcript), then a plain native log dir, then the
//! backend stores directly (a bare codex rollout id / claude session id).

use super::*;
use crate::session_fork::{
    claude_fork_points, codex_fork_points, native_fork_points, ForkPointCatalog, ForkPointQuery,
    FORK_POINT_DEFAULT_LIMIT,
};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) async fn handle_session_fork_points(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = session_fork_points_response(request_line, &crate::platform::home_dir());
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport edge: resolves `CODEX_HOME`; tests drive the `_with_roots`
/// variant with injected temp roots.
pub(crate) fn session_fork_points_response(request_line: &str, home: &Path) -> ApiResponse {
    let codex_root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".codex"));
    session_fork_points_response_with_roots(request_line, home, &codex_root)
}

pub(crate) fn session_fork_points_response_with_roots(
    request_line: &str,
    home: &Path,
    codex_root: &Path,
) -> ApiResponse {
    let Some(session_id) = fork_points_session_id(request_line) else {
        return ApiResponse::json_error(400, "session id missing in fork-points path");
    };
    let query = fork_point_query_from_request(request_line);
    match resolve_fork_point_catalog(&session_id, home, codex_root, &query) {
        Ok(Some(catalog)) => match serde_json::to_value(&catalog) {
            Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
            Err(err) => {
                ApiResponse::json_error(500, format!("failed to serialize fork points: {err}"))
            }
        },
        Ok(None) => ApiResponse::json_error(
            404,
            format!("session {session_id} not found in any session store"),
        ),
        Err(err) => ApiResponse::json_error(500, format!("failed to derive fork points: {err}")),
    }
}

/// `{id}` from `GET /api/session/{id}/fork-points[?…]`.
fn fork_points_session_id(request_line: &str) -> Option<String> {
    let path = request_line.split_whitespace().nth(1)?;
    let path = path.split('?').next().unwrap_or(path);
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next() != Some("api") || segments.next() != Some("session") {
        return None;
    }
    let id = segments.next()?.trim();
    if id.is_empty() || segments.next() != Some("fork-points") || segments.next().is_some() {
        return None;
    }
    Some(id.to_string())
}

fn fork_point_query_from_request(request_line: &str) -> ForkPointQuery {
    let flag = |key: &str| {
        query_param(request_line, key)
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    };
    let number =
        |key: &str| query_param(request_line, key).and_then(|value| value.parse::<usize>().ok());
    ForkPointQuery {
        include_non_recovery: flag("include_non_recovery"),
        offset: number("offset").unwrap_or(0),
        limit: number("limit").unwrap_or(FORK_POINT_DEFAULT_LIMIT),
    }
}

fn resolve_fork_point_catalog(
    session_id: &str,
    home: &Path,
    codex_root: &Path,
    query: &ForkPointQuery,
) -> io::Result<Option<ForkPointCatalog>> {
    // A wrapper session's catalog comes from its backend's own transcript.
    if let Some((source, backend_id)) =
        crate::session_supervisor::persisted_external_identity_for_session_in_home(home, session_id)
    {
        return external_fork_point_catalog(
            session_id,
            &source,
            &backend_id,
            home,
            codex_root,
            query,
        )
        .map(Some);
    }
    if let Some(log_dir) =
        crate::session_log::SessionLog::find_session_by_id_in_home(home, session_id)
    {
        return native_fork_points(session_id, &log_dir, query).map(Some);
    }
    if let Some(rollout) =
        crate::codex_history::find_codex_session_file_in(codex_root, home, session_id)
    {
        return codex_fork_points(session_id, session_id, &rollout, query).map(Some);
    }
    if let Some(transcript) = find_claude_session_file(home, session_id) {
        return claude_fork_points(session_id, session_id, &transcript, query).map(Some);
    }
    Ok(None)
}

fn external_fork_point_catalog(
    session_id: &str,
    source: &str,
    backend_id: &str,
    home: &Path,
    codex_root: &Path,
    query: &ForkPointQuery,
) -> io::Result<ForkPointCatalog> {
    match source {
        "codex" => {
            match crate::codex_history::find_codex_session_file_in(codex_root, home, backend_id) {
                Some(rollout) => codex_fork_points(session_id, backend_id, &rollout, query),
                None => Ok(ForkPointCatalog::unsupported(
                    session_id,
                    "codex",
                    Some(backend_id),
                    "rollout file not found under the codex session store",
                )),
            }
        }
        "claude-code" => match find_claude_session_file(home, backend_id) {
            Some(transcript) => claude_fork_points(session_id, backend_id, &transcript, query),
            None => Ok(ForkPointCatalog::unsupported(
                session_id,
                "claude-code",
                Some(backend_id),
                "transcript not found under the claude session store",
            )),
        },
        other => Ok(ForkPointCatalog::unsupported(
            session_id,
            other,
            Some(backend_id),
            &format!("fork points are not supported for {other} sessions yet"),
        )),
    }
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

    fn request_line(id: &str, query: &str) -> String {
        format!("GET /api/session/{id}/fork-points{query} HTTP/1.1")
    }

    fn seed_native_session(home: &Path, session_id: &str) {
        let dir = crate::platform::intendant_home_in(home)
            .join("logs")
            .join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::json!({ "session_id": session_id }).to_string(),
        )
        .expect("meta");
        let lines = [
            serde_json::json!({"role":"user","content":"round one","seq":1}),
            serde_json::json!({"role":"assistant","content":"done","seq":2}),
            serde_json::json!({"role":"user","content":"round two","seq":3}),
        ];
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(dir.join("conversation.jsonl"), body).expect("conversation");
    }

    fn seed_codex_rollout(codex_root: &Path, backend_id: &str) {
        let dir = codex_root
            .join("sessions")
            .join("2026")
            .join("01")
            .join("02");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let lines = [
            serde_json::json!({"timestamp":"t","type":"session_meta","payload":{"id":backend_id,"cwd":"/tmp"}}),
            serde_json::json!({"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"do the thing"}}),
        ];
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(
            dir.join(format!("rollout-2026-01-02T00-00-00-{backend_id}.jsonl")),
            body,
        )
        .expect("rollout");
    }

    #[test]
    fn native_session_resolves_to_round_catalog() {
        let home = tempfile::tempdir().expect("home");
        let codex_root = home.path().join(".codex");
        seed_native_session(home.path(), "native-session");
        let (status, body) = response_json(session_fork_points_response_with_roots(
            &request_line("native-session", ""),
            home.path(),
            &codex_root,
        ));
        assert_eq!(status, 200);
        assert_eq!(body["source"], "intendant");
        assert_eq!(body["supported"], true);
        assert!(body["fork_points"]
            .as_array()
            .is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn bare_codex_id_resolves_via_rollout_store() {
        let home = tempfile::tempdir().expect("home");
        let codex_root = home.path().join("codex-home");
        seed_codex_rollout(&codex_root, "0000aaaa-1111-2222-3333-444455556666");
        let (status, body) = response_json(session_fork_points_response_with_roots(
            &request_line("0000aaaa-1111-2222-3333-444455556666", "?limit=10"),
            home.path(),
            &codex_root,
        ));
        assert_eq!(status, 200);
        assert_eq!(body["source"], "codex");
        assert_eq!(body["supported"], true);
        let kinds: Vec<&str> = body["fork_points"]
            .as_array()
            .expect("points")
            .iter()
            .filter_map(|point| point["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"turn-boundary"));
    }

    #[test]
    fn claude_session_resolves_to_message_catalog() {
        use crate::session_fork::test_fixtures::message_line;
        let home = tempfile::tempdir().expect("home");
        let codex_root = home.path().join(".codex");
        let project = home.path().join(".claude").join("projects").join("-tmp-x");
        std::fs::create_dir_all(&project).expect("mkdir");
        let lines = [
            message_line("u1", None, "user", "round one", false),
            message_line("a1", Some("u1"), "assistant", "answer", false),
            message_line("u2", Some("a1"), "user", "round two", false),
        ];
        std::fs::write(
            project.join("cc11cc11-0000-0000-0000-000000000000.jsonl"),
            lines.join("\n") + "\n",
        )
        .expect("transcript");
        let (status, body) = response_json(session_fork_points_response_with_roots(
            &request_line("cc11cc11-0000-0000-0000-000000000000", ""),
            home.path(),
            &codex_root,
        ));
        assert_eq!(status, 200);
        assert_eq!(body["source"], "claude-code");
        assert_eq!(body["supported"], true);
        let ids: Vec<&str> = body["fork_points"]
            .as_array()
            .expect("points")
            .iter()
            .filter_map(|point| point["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["head", "msg:a1"]);
    }

    #[test]
    fn unknown_session_is_404() {
        let home = tempfile::tempdir().expect("home");
        let codex_root = home.path().join(".codex");
        let (status, body) = response_json(session_fork_points_response_with_roots(
            &request_line("does-not-exist", ""),
            home.path(),
            &codex_root,
        ));
        assert_eq!(status, 404);
        assert!(body["error"].as_str().is_some());
    }

    #[test]
    fn session_id_extraction_rejects_malformed_paths() {
        assert_eq!(
            fork_points_session_id("GET /api/session/abc/fork-points HTTP/1.1").as_deref(),
            Some("abc")
        );
        assert_eq!(
            fork_points_session_id("GET /api/session/abc/fork-points?limit=5 HTTP/1.1").as_deref(),
            Some("abc")
        );
        assert!(fork_points_session_id("GET /api/session//fork-points HTTP/1.1").is_none());
        assert!(fork_points_session_id("GET /api/session/abc/frames/x.png HTTP/1.1").is_none());
        assert!(fork_points_session_id("GET /api/other/abc/fork-points HTTP/1.1").is_none());
    }
}
