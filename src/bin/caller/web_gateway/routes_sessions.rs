//! The sessions surface of the gateway: session list/detail/delete
//! endpoints and the /api/session sub-router, agent-output serving,
//! context-rewind history endpoints, managed-context anchors/records/
//! fission views, workspace change tracking and diffs, worktree
//! inventory responses, session report zips, and display listing.

use super::*;

/// Serializes tests that drive the session search's single-flight guard
/// (`SESSION_SEARCH_IN_FLIGHT` is process-global): the golden transcripts
/// here and the tunnel/HTTP parity fixtures in `dashboard_control` lock it
/// so a concurrent test never observes a spurious busy response.
#[cfg(test)]
pub(crate) static SESSIONS_SEARCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn agent_output_chunks_with_fallback(
    primary_log_dir: &Path,
    ids: &[String],
    fallback_logs_dir: Option<&Path>,
) -> Vec<crate::session_log::AgentOutputChunk> {
    let mut found: HashMap<String, crate::session_log::AgentOutputChunk> = HashMap::new();

    for chunk in crate::session_log::agent_output_chunks_by_id(primary_log_dir, ids) {
        found.entry(chunk.output_id.clone()).or_insert(chunk);
    }

    if found.len() < ids.len() {
        if let Some(logs_dir) = fallback_logs_dir {
            let mut dirs = Vec::new();
            if let Ok(entries) = std::fs::read_dir(logs_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir()
                        && path.join("session.jsonl").is_file()
                        && !same_path(&path, primary_log_dir)
                    {
                        dirs.push(path);
                    }
                }
            }
            dirs.sort_by_key(|b| std::cmp::Reverse(session_log_mtime(b)));

            for dir in dirs {
                let missing: Vec<String> = ids
                    .iter()
                    .filter(|id| !found.contains_key(id.as_str()))
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    break;
                }
                for chunk in crate::session_log::agent_output_chunks_by_id(&dir, &missing) {
                    found.entry(chunk.output_id.clone()).or_insert(chunk);
                }
            }
        }
    }

    ids.iter().filter_map(|id| found.remove(id)).collect()
}

pub(crate) fn is_valid_agent_output_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
}

/// The wildcard-CORS json envelope several session surfaces answer with
/// (`Cache-Control` + `Access-Control-Allow-Origin: *` + `Connection`
/// tail — the historical `upload_error_response`/hand-rolled shape).
pub(crate) fn session_wildcard_json_response(status: u16, body: String) -> ApiResponse {
    ApiResponse::Json {
        status,
        body: JsonBody::PreSerialized(body),
        headers: vec![
            ("Cache-Control", "no-cache".to_string()),
            ("Access-Control-Allow-Origin", "*".to_string()),
            ("Connection", "close".to_string()),
        ],
    }
}

/// `{"error": message}` under the wildcard-CORS tail.
pub(crate) fn session_wildcard_json_error(status: u16, message: &str) -> ApiResponse {
    session_wildcard_json_response(status, serde_json::json!({ "error": message }).to_string())
}

pub(crate) fn current_agent_output_response_for_ids(
    home: &Path,
    ids: Vec<String>,
    log_dir: &Path,
) -> ApiResponse {
    if ids.is_empty() {
        return session_wildcard_json_error(400, "missing output ids");
    }

    let fallback_logs_dir = Some(crate::platform::intendant_home_in(home).join("logs"));
    let chunks = agent_output_chunks_with_fallback(log_dir, &ids, fallback_logs_dir.as_deref());
    let found: HashSet<&str> = chunks
        .iter()
        .map(|chunk| chunk.output_id.as_str())
        .collect();
    let missing: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !found.contains(id))
        .collect();
    let body = serde_json::json!({
        "outputs": chunks,
        "missing": missing,
    })
    .to_string();
    session_wildcard_json_response(200, body)
}

pub(crate) fn agent_output_ids_from_json_body(body: &str) -> Result<Vec<String>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let Some(ids) = parsed.get("ids").and_then(|ids| ids.as_array()) else {
        return Err("missing output ids".to_string());
    };
    let ids: Vec<String> = ids
        .iter()
        .filter_map(|id| id.as_str())
        .map(str::trim)
        .filter(|id| is_valid_agent_output_id(id))
        .map(ToString::to_string)
        .collect();
    if ids.is_empty() {
        return Err("missing output ids".to_string());
    }
    Ok(ids)
}

/// Transport-neutral core of `POST /api/session/current/agent-output`
/// (tunnel twin `api_session_current_agent_output`): output-id decode
/// from the JSON body, then the persisted-chunk fetch against the active
/// session log dir. The no-active-log 404 stays with each lane's log
/// resolution step; `home` scopes the cross-session fallback sweep (the
/// transport edge resolves the real home, tests inject a temp one).
pub(crate) fn current_agent_output_api_response(
    home: &Path,
    body: &str,
    log_dir: &Path,
) -> ApiResponse {
    match agent_output_ids_from_json_body(body) {
        Ok(ids) => current_agent_output_response_for_ids(home, ids, log_dir),
        Err(e) => session_wildcard_json_error(400, &e),
    }
}

pub(crate) fn external_agent_output_response_for_ids(
    home: &Path,
    source: &str,
    session_id: &str,
    ids: Vec<String>,
) -> ApiResponse {
    let Some(entries) = external_session_entries_from_home(home, source, session_id) else {
        return session_wildcard_json_error(404, "session not found");
    };
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut found: HashMap<String, serde_json::Value> = HashMap::new();
    for entry in entries {
        let output_id = entry
            .get("output_id")
            .or_else(|| entry.get("outputId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if output_id.is_empty() || !wanted.contains(output_id) {
            continue;
        }
        if entry.get("event").and_then(|v| v.as_str()) != Some("agent_output")
            && entry.get("kind").and_then(|v| v.as_str()) != Some("agent_output")
        {
            continue;
        }
        found.entry(output_id.to_string()).or_insert_with(|| {
            serde_json::json!({
                "output_id": output_id,
                "session_id": session_id,
                "source": source,
                "stdout": entry.get("stdout").and_then(|v| v.as_str()).unwrap_or(""),
                "stderr": entry.get("stderr").and_then(|v| v.as_str()).unwrap_or(""),
            })
        });
    }
    let outputs: Vec<_> = ids.iter().filter_map(|id| found.remove(id)).collect();
    let output_ids: HashSet<&str> = outputs
        .iter()
        .filter_map(|output| output.get("output_id").and_then(|v| v.as_str()))
        .collect();
    let missing: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !output_ids.contains(id))
        .collect();
    // Historical asymmetry, preserved: the external-source success rides
    // the canonical json tail (no wildcard CORS header), unlike the
    // intendant-source success above.
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(
            serde_json::json!({
                "outputs": outputs,
                "missing": missing,
            })
            .to_string(),
        ),
    )
}

pub(crate) fn session_agent_output_response_for_ids(
    home: &Path,
    session_id: &str,
    source: &str,
    ids: Vec<String>,
) -> ApiResponse {
    let source = crate::session_names::normalize_source(source);
    if source == "intendant" {
        let Some(session_dir) = resolve_bare_session_dir_from_home(home, session_id) else {
            return session_wildcard_json_error(404, "session not found");
        };
        return current_agent_output_response_for_ids(home, ids, &session_dir);
    }
    external_agent_output_response_for_ids(home, &source, session_id, ids)
}

/// Transport-neutral core of the by-id agent-output read
/// (`POST /api/session/{id}/agent-output`, a POST-shaped read; tunnel twin
/// `api_session_agent_output`): bare-id policy, output-id decode from the
/// JSON body, then the persisted-chunk fetch. The transport edge resolves
/// `home`; tests inject a temp one.
pub(crate) fn session_agent_output_api_response(
    home: &Path,
    body: &str,
    session_id: &str,
    source: &str,
) -> ApiResponse {
    let session_id = session_id.trim();
    if !session_lookup_id_is_safe(session_id) {
        return session_wildcard_json_error(400, "invalid session id");
    }
    match agent_output_ids_from_json_body(body) {
        Ok(ids) => session_agent_output_response_for_ids(home, session_id, source, ids),
        Err(e) => session_wildcard_json_error(400, &e),
    }
}

/// Build a zip containing the current session's text artifacts for the
/// Settings → "Download session report" feature. Includes session.jsonl,
/// session_meta.json, transcript.jsonl, summary.json, daemon.log,
/// panic.log, and everything under `turns/`. Excludes `frames/` and
/// `recordings/` since those can be hundreds of megabytes and are not
/// needed to diagnose controller-side bugs.
pub(crate) fn build_session_report_zip(session_dir: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    let buf = Cursor::new(Vec::<u8>::new());
    let mut zip = zip::ZipWriter::new(buf);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    const FLAT_FILES: &[&str] = &[
        "session.jsonl",
        "session_meta.json",
        "transcript.jsonl",
        "summary.json",
        "daemon.log",
        "panic.log",
    ];

    for name in FLAT_FILES {
        let path = session_dir.join(name);
        if path.is_file() {
            let data = std::fs::read(&path)?;
            zip.start_file(*name, options)
                .map_err(std::io::Error::other)?;
            zip.write_all(&data)?;
        }
    }

    let turns_dir = session_dir.join("turns");
    if turns_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&turns_dir) {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            files.sort();
            for path in files {
                if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                    let zip_name = format!("turns/{}", fname);
                    let data = std::fs::read(&path)?;
                    zip.start_file(&zip_name, options)
                        .map_err(std::io::Error::other)?;
                    zip.write_all(&data)?;
                }
            }
        }
    }

    let cursor = zip.finish().map_err(std::io::Error::other)?;
    Ok(cursor.into_inner())
}

pub(crate) struct SessionReportZip {
    pub filename: String,
    pub bytes: Vec<u8>,
}

pub(crate) enum SessionReportZipError {
    InvalidSessionId,
    NotFound,
    Build(String),
}

pub(crate) fn session_report_zip_for_request(
    home: &Path,
    session_id: &str,
    session_log: Option<&Arc<Mutex<crate::session_log::SessionLog>>>,
    query_ctx: Option<&WebQueryCtx>,
) -> Result<SessionReportZip, SessionReportZipError> {
    let session_id = session_id.trim();
    if session_id != "current" && !session_lookup_id_is_safe(session_id) {
        return Err(SessionReportZipError::InvalidSessionId);
    }
    let resolved_dir: Option<PathBuf> = if session_id == "current" {
        current_session_log_dir(session_log, query_ctx)
    } else {
        resolve_bare_session_dir_from_home(home, session_id)
    };
    let Some(dir) = resolved_dir else {
        return Err(SessionReportZipError::NotFound);
    };
    let bytes =
        build_session_report_zip(&dir).map_err(|e| SessionReportZipError::Build(e.to_string()))?;
    let fname = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".to_string());
    Ok(SessionReportZip {
        filename: format!("intendant-session-{fname}.zip"),
        bytes,
    })
}

pub(crate) fn current_session_log_dir(
    session_log: Option<&Arc<Mutex<crate::session_log::SessionLog>>>,
    query_ctx: Option<&WebQueryCtx>,
) -> Option<PathBuf> {
    session_log
        .and_then(|slog| slog.lock().ok().map(|log| log.dir().to_path_buf()))
        .or_else(|| query_ctx.map(|ctx| ctx.log_dir.clone()))
}

pub(crate) fn empty_worktree_inventory_response() -> String {
    serde_json::to_string(&crate::worktree_inventory::empty_scan())
        .unwrap_or_else(|_| "{}".to_string())
}

pub(crate) fn scan_worktree_inventory_response(home: &Path, project_root: Option<&Path>) -> String {
    let hints = worktree_session_hints_from_home(home);
    let scan = crate::worktree_inventory::scan_worktrees(home, project_root, &hints);
    serde_json::to_string(&scan).unwrap_or_else(|_| "{}".to_string())
}

/// Transport-neutral worktrees cores (tunnel twins `api_worktrees`,
/// `api_worktrees_inspect`, `api_worktrees_scan`, `api_worktrees_remove`):
/// the inventory (status, body) helpers plus the shared cache
/// side-effects, rendered as [`ApiResponse`]s. Spawn placement and
/// task-failure shapes stay transport-owned — and so is `home`: the
/// transport edge resolves the real home dir (like the merge adapters
/// already do), keeping these cores deterministic so tests inject a
/// temp home instead of scanning the machine they run on.
pub(crate) fn worktrees_list_api_response(cache: &Arc<Mutex<Option<String>>>) -> ApiResponse {
    let body = cache
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_else(empty_worktree_inventory_response);
    ApiResponse::json(200, JsonBody::PreSerialized(body))
}

pub(crate) fn worktrees_inspect_api_response(home: &Path, body_text: &str) -> ApiResponse {
    let (status_line, body) = inspect_worktree_inventory_response(home, body_text);
    ApiResponse::json(status_line_code(status_line), JsonBody::PreSerialized(body))
}

pub(crate) fn worktrees_scan_api_response(
    home: &Path,
    project_root: Option<&Path>,
    cache: &Arc<Mutex<Option<String>>>,
) -> ApiResponse {
    let body = scan_worktree_inventory_response(home, project_root);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(body.clone());
    }
    ApiResponse::json(200, JsonBody::PreSerialized(body))
}

pub(crate) fn worktrees_remove_api_response(
    home: &Path,
    body_text: &str,
    cache: &Arc<Mutex<Option<String>>>,
) -> ApiResponse {
    let (status_line, body) = remove_worktree_inventory_response(home, body_text);
    if status_line == "200 OK" {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
    ApiResponse::json(status_line_code(status_line), JsonBody::PreSerialized(body))
}

pub(crate) fn inspect_worktree_inventory_response(
    home: &Path,
    body_text: &str,
) -> (&'static str, String) {
    let request = match serde_json::from_str::<crate::worktree_inventory::WorktreeInspectRequest>(
        body_text,
    ) {
        Ok(request) => request,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({
                    "ok": false,
                    "error": format!("invalid worktree inspect request: {e}")
                })
                .to_string(),
            );
        }
    };
    let hints = worktree_session_hints_from_home(home);
    match crate::worktree_inventory::inspect_worktree(request, &hints) {
        Ok(response) => (
            "200 OK",
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string()),
        ),
        Err(e) => (
            "409 Conflict",
            serde_json::json!({
                "ok": false,
                "error": e
            })
            .to_string(),
        ),
    }
}

pub(crate) fn remove_worktree_inventory_response(
    home: &Path,
    body_text: &str,
) -> (&'static str, String) {
    let request =
        match serde_json::from_str::<crate::worktree_inventory::WorktreeRemoveRequest>(body_text) {
            Ok(request) => request,
            Err(e) => {
                return (
                    "400 Bad Request",
                    serde_json::json!({
                        "ok": false,
                        "error": format!("invalid worktree removal request: {e}")
                    })
                    .to_string(),
                );
            }
        };
    let hints = worktree_session_hints_from_home(home);
    match crate::worktree_inventory::remove_worktree_if_safe(request, &hints) {
        Ok(response) => (
            "200 OK",
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string()),
        ),
        Err(e) => (
            "409 Conflict",
            serde_json::json!({
                "ok": false,
                "error": e
            })
            .to_string(),
        ),
    }
}

/// Session-end finish-card action: merge a session's linked worktree
/// branch into its base checkout, then remove the checkout via the same
/// safety-checked path `/api/worktrees/remove` uses.
///
/// The request carries only a session id; the branch, checkout path, and
/// base root all come from the session's own recorded linkage in
/// `session_meta.json` — so the endpoint can only ever merge a
/// session-linked worktree branch, never an arbitrary ref.
pub(crate) fn merge_session_worktree_response(
    home: &Path,
    body_text: &str,
) -> (&'static str, String) {
    let session_id = serde_json::from_str::<serde_json::Value>(body_text)
        .ok()
        .and_then(|body| {
            body.get("session_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let Some(session_id) = session_id.filter(|id| session_lookup_id_is_safe(id)) else {
        return (
            "400 Bad Request",
            serde_json::json!({
                "ok": false,
                "error": "worktree merge request needs a session_id"
            })
            .to_string(),
        );
    };
    let Some(session_dir) = resolve_bare_session_dir_from_home(home, &session_id) else {
        return (
            "404 Not Found",
            serde_json::json!({
                "ok": false,
                "error": format!("session '{session_id}' was not found")
            })
            .to_string(),
        );
    };
    let hints = worktree_session_hints_from_home(home);
    match merge_linked_session_worktree(&session_dir, &hints) {
        Ok(body) => ("200 OK", body.to_string()),
        Err(message) => (
            "409 Conflict",
            serde_json::json!({
                "ok": false,
                "error": message
            })
            .to_string(),
        ),
    }
}

/// The merge itself, keyed entirely off the session's recorded linkage.
/// Fails closed on every drifted precondition (unregistered checkout,
/// renamed branch, base checkout moved to another branch or detached);
/// a conflicted merge is aborted by `worktree::merge` and reported. A
/// post-merge removal refusal is reported in the response, not an error —
/// the merge itself already landed.
pub(crate) fn merge_linked_session_worktree(
    session_dir: &Path,
    hints: &[crate::worktree_inventory::WorktreeSessionHint],
) -> Result<serde_json::Value, String> {
    let meta = std::fs::read_to_string(session_dir.join("session_meta.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::session_log::SessionMeta>(&raw).ok())
        .ok_or_else(|| "session metadata was not readable".to_string())?;
    let linkage = meta
        .worktree
        .ok_or_else(|| "this session has no linked git worktree".to_string())?;
    let base_root = PathBuf::from(&linkage.base_root);
    if !base_root.is_dir() {
        return Err(format!(
            "base checkout {} no longer exists",
            base_root.display()
        ));
    }
    let worktree_path = PathBuf::from(&linkage.path);
    // The linked checkout must still be a registered worktree of the base
    // repo, still on the branch the session recorded.
    let registered = crate::worktree::list(&base_root)
        .map_err(|e| format!("could not list worktrees: {e}"))?
        .into_iter()
        .find(|wt| worktree_merge_paths_match(&wt.path, &worktree_path));
    let Some(registered) = registered else {
        return Err(format!(
            "{} is no longer a registered worktree of {}",
            worktree_path.display(),
            base_root.display()
        ));
    };
    if registered.branch_name != linkage.branch {
        return Err(format!(
            "worktree {} is now on branch {:?}, not the session's recorded {:?} — merge manually",
            worktree_path.display(),
            registered.branch_name,
            linkage.branch
        ));
    }
    // Merge into the branch the base checkout is on — and require it to
    // still be the branch the worktree branched from, so "Merge into
    // <base>" can never silently land on a different branch.
    let current = crate::worktree::current_branch(&base_root);
    let merge_target = match (&linkage.base_branch, current) {
        (Some(recorded), Some(current)) if *recorded == current => current,
        (Some(recorded), Some(current)) => {
            return Err(format!(
                "base checkout is now on {current:?} (it was on {recorded:?} when the \
                 worktree was created) — check out {recorded:?} first or merge manually"
            ));
        }
        (Some(recorded), None) => {
            return Err(format!(
                "base checkout is on a detached HEAD — check out {recorded:?} first"
            ));
        }
        (None, _) => {
            return Err(
                "the worktree was created from a detached HEAD; merge manually".to_string(),
            );
        }
    };
    let wt = crate::worktree::Worktree {
        branch_name: linkage.branch.clone(),
        path: worktree_path.clone(),
        base_branch: merge_target.clone(),
    };
    match crate::worktree::merge(&base_root, &wt, &merge_target).map_err(|e| e.to_string())? {
        crate::worktree::MergeResult::Conflict(message) => Err(message),
        crate::worktree::MergeResult::Clean => {
            let repo_root = crate::worktree_inventory::git_repo_root(&base_root)
                .unwrap_or_else(|| base_root.clone());
            let removal = crate::worktree_inventory::remove_worktree_if_safe(
                crate::worktree_inventory::WorktreeRemoveRequest {
                    repo_root,
                    path: worktree_path.clone(),
                    expected_head: None,
                },
                hints,
            );
            let (removed, removal_error) = match removal {
                Ok(_) => (true, None),
                Err(e) => (false, Some(e)),
            };
            Ok(serde_json::json!({
                "ok": true,
                "merged": true,
                "branch": linkage.branch,
                "merged_into": merge_target,
                "base_root": linkage.base_root,
                "worktree_path": linkage.path,
                "removed": removed,
                "removal_error": removal_error,
            }))
        }
    }
}

/// Compare a `git worktree list` path against the session's recorded
/// checkout path, tolerating symlinked tempdirs (macOS `/tmp` vs
/// `/private/tmp`) by canonicalizing when possible.
fn worktree_merge_paths_match(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    a == b || canon(a) == canon(b)
}

/// Handle `/api/session/current/changes[/{path}]` requests.
///
/// - No path suffix: list all changed files (baseline vs current).
/// - With path suffix: return unified diff for a single file.
#[derive(Debug, Clone)]
pub(crate) enum ChangeFileState {
    Text { content: String, hash: String },
    Unsupported { hash: String, reason: String },
}

#[derive(Debug, Clone)]
pub(crate) struct ChangeRecord {
    path: String,
    kind: &'static str,
    lines_added: u32,
    lines_removed: u32,
    diff_available: bool,
    reason: Option<String>,
    diff: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChangesRequestTarget {
    snapshot_dir: PathBuf,
    project_root: PathBuf,
    include_project_external_logs: bool,
}

pub(crate) fn handle_changes_request(
    request_line: &str,
    snapshot_dir: Option<&Path>,
    project_root: Option<&Path>,
) -> (&'static str, String) {
    handle_changes_request_inner(request_line, snapshot_dir, project_root, false)
}

pub(crate) fn handle_changes_request_for_home(
    request_line: &str,
    snapshot_dir: Option<&Path>,
    project_root: Option<&Path>,
    home: &Path,
) -> (&'static str, String) {
    if let Some(target) = changes_request_target_from_home(request_line, home) {
        if should_use_live_changes_for_target(request_line, &target, snapshot_dir, project_root) {
            return handle_changes_request(request_line, snapshot_dir, project_root);
        }
        return handle_changes_request_inner(
            request_line,
            Some(&target.snapshot_dir),
            Some(&target.project_root),
            target.include_project_external_logs,
        );
    }
    handle_changes_request(request_line, snapshot_dir, project_root)
}

pub(crate) fn handle_changes_request_inner(
    request_line: &str,
    snapshot_dir: Option<&Path>,
    project_root: Option<&Path>,
    include_project_external_logs: bool,
) -> (&'static str, String) {
    let (snapshot_dir, project_root) = match (snapshot_dir, project_root) {
        (Some(s), Some(p)) => (s, p),
        _ => {
            return (
                "503 Service Unavailable",
                serde_json::json!({"error": "file watcher not active"}).to_string(),
            );
        }
    };

    let baseline_dir = snapshot_dir.join("baseline");
    // Extract the request target from `GET <target> HTTP/1.1`, then trim the
    // endpoint prefix. The list endpoint has no path suffix.
    let file_path = changes_request_file_path(request_line);

    if !baseline_dir.exists() {
        let records =
            load_external_change_records(snapshot_dir, project_root, !file_path.is_empty(), true);
        if file_path.is_empty() {
            let changes: Vec<_> = records.iter().map(change_record_summary_json).collect();
            return (
                "200 OK",
                serde_json::to_string(&changes).unwrap_or_else(|_| "[]".to_string()),
            );
        }

        let decoded = url_path_decode(file_path);
        if let Some(record) = records.into_iter().find(|record| record.path == decoded) {
            return ("200 OK", change_record_detail_json(&record).to_string());
        }
        return (
            "404 Not Found",
            serde_json::json!({"error": "no changes for path"}).to_string(),
        );
    }

    if file_path.is_empty() {
        // List all changed files.
        (
            "200 OK",
            handle_changes_list(
                snapshot_dir,
                &baseline_dir,
                project_root,
                include_project_external_logs,
            ),
        )
    } else {
        // Single-file diff.
        handle_changes_file_diff(
            file_path,
            snapshot_dir,
            &baseline_dir,
            project_root,
            include_project_external_logs,
        )
    }
}

pub(crate) fn changes_request_file_path(request_line: &str) -> &str {
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    target
        .strip_prefix("/api/session/current/changes")
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .trim_start_matches('/')
}

pub(crate) fn should_use_live_changes_for_target(
    request_line: &str,
    target: &ChangesRequestTarget,
    snapshot_dir: Option<&Path>,
    project_root: Option<&Path>,
) -> bool {
    let (Some(live_snapshot_dir), Some(live_project_root)) = (snapshot_dir, project_root) else {
        return false;
    };
    if target.snapshot_dir.join("baseline").exists()
        || !live_snapshot_dir.join("baseline").exists()
        || !path_keys_match(&target.project_root, live_project_root)
    {
        return false;
    }

    let file_path = changes_request_file_path(request_line);
    let records = load_external_change_records(
        &target.snapshot_dir,
        &target.project_root,
        !file_path.is_empty(),
        target.include_project_external_logs,
    );
    if file_path.is_empty() {
        return records.is_empty();
    }

    let decoded = url_path_decode(file_path);
    !records.iter().any(|record| record.path == decoded)
}

pub(crate) fn changes_request_target_id(request_line: &str) -> Option<String> {
    [
        "session_id",
        "target_session_id",
        "backend_session_id",
        "intendant_session_id",
    ]
    .into_iter()
    .find_map(|key| query_param(request_line, key))
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

pub(crate) fn session_row_matches_changes_target(
    session: &serde_json::Value,
    target_id: &str,
) -> bool {
    [
        "session_id",
        "resume_id",
        "backend_session_id",
        "intendant_session_id",
    ]
    .into_iter()
    .any(|key| session.get(key).and_then(|v| v.as_str()) == Some(target_id))
}

pub(crate) fn changes_project_root_from_session(session: &serde_json::Value) -> Option<PathBuf> {
    ["project_root", "cwd", "workdir", "workDir"]
        .into_iter()
        .find_map(|key| value_str(session, key))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

pub(crate) fn changes_log_dir_from_session(session: &serde_json::Value) -> Option<PathBuf> {
    ["intendant_session_path", "path"]
        .into_iter()
        .filter_map(|key| value_str(session, key).map(PathBuf::from))
        .find(|path| path.is_dir())
}

pub(crate) fn changes_request_target_from_home(
    request_line: &str,
    home: &Path,
) -> Option<ChangesRequestTarget> {
    let target_id = changes_request_target_id(request_line)?;
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let entries = std::fs::read_dir(logs_dir).ok()?;
    let mut candidates = Vec::new();

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().to_string();
        let Some(row) = intendant_session_list_row_from_dir(&dir, &session_id) else {
            continue;
        };
        if !session_row_matches_changes_target(&row, &target_id) {
            continue;
        }
        if let Some(project_root) = changes_project_root_from_session(&row) {
            candidates.push(ChangesRequestTarget {
                snapshot_dir: dir.join("file_snapshots"),
                project_root,
                include_project_external_logs: true,
            });
        }
    }

    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&list_sessions_from_home(home)).unwrap_or_default();
    for session in sessions {
        if !session_row_matches_changes_target(&session, &target_id) {
            continue;
        }
        if let (Some(project_root), Some(log_dir)) = (
            changes_project_root_from_session(&session),
            changes_log_dir_from_session(&session),
        ) {
            candidates.push(ChangesRequestTarget {
                snapshot_dir: log_dir.join("file_snapshots"),
                project_root,
                include_project_external_logs: true,
            });
        }
    }

    best_changes_request_target(candidates)
}

pub(crate) fn best_changes_request_target(
    candidates: Vec<ChangesRequestTarget>,
) -> Option<ChangesRequestTarget> {
    candidates
        .into_iter()
        .max_by_key(changes_request_target_score)
}

pub(crate) fn changes_request_target_score(target: &ChangesRequestTarget) -> usize {
    let baseline_dir = target.snapshot_dir.join("baseline");
    if baseline_dir.exists() {
        let body = handle_changes_list(
            &target.snapshot_dir,
            &baseline_dir,
            &target.project_root,
            target.include_project_external_logs,
        );
        let count = serde_json::from_str::<Vec<serde_json::Value>>(&body)
            .map(|items| items.len())
            .unwrap_or(0);
        if count > 0 {
            return 2_000 + count;
        }
    }

    let external_count = load_external_change_records(
        &target.snapshot_dir,
        &target.project_root,
        false,
        target.include_project_external_logs,
    )
    .len();
    if external_count > 0 {
        return 1_000 + external_count;
    }
    if baseline_dir.exists() {
        10
    } else {
        100
    }
}

pub(crate) fn load_baseline_manifest(baseline_dir: &Path) -> crate::file_watcher::BaselineManifest {
    let Some(snapshot_dir) = baseline_dir.parent() else {
        return HashMap::new();
    };
    let path = snapshot_dir.join(crate::file_watcher::BASELINE_MANIFEST_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub(crate) fn normalize_external_diff_path(path: &str) -> Option<String> {
    let path = path.split('\t').next().unwrap_or(path).trim();
    if path == "/dev/null" {
        return None;
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    (!path.is_empty()).then(|| path.to_string())
}

pub(crate) fn parse_external_diff_file_paths(unified_diff: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in unified_diff.lines() {
        let path = if let Some(rest) = line.strip_prefix("+++ ") {
            rest
        } else if let Some(rest) = line.strip_prefix("--- ") {
            rest
        } else {
            continue;
        };
        if let Some(path) = normalize_external_diff_path(path) {
            if !out.iter().any(|p| p == &path) {
                out.push(path);
            }
        }
    }
    out
}

pub(crate) fn external_diff_line_text(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

pub(crate) fn is_external_diff_file_boundary(lines: &[&str], idx: usize) -> bool {
    let line = external_diff_line_text(lines[idx]);
    line.starts_with("diff --git ")
        || (line.starts_with("--- ")
            && lines
                .get(idx + 1)
                .is_some_and(|next| external_diff_line_text(next).starts_with("+++ ")))
}

pub(crate) fn split_external_unified_diff_by_file(unified_diff: &str) -> Vec<(String, String)> {
    if unified_diff.trim().is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<&str> = unified_diff.split_inclusive('\n').collect();
    if lines.is_empty() {
        lines.push(unified_diff);
    }

    let mut starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            external_diff_line_text(line)
                .starts_with("diff --git ")
                .then_some(idx)
        })
        .collect();
    if starts.is_empty() {
        for idx in 0..lines.len() {
            if is_external_diff_file_boundary(&lines, idx) {
                starts.push(idx);
            }
        }
    }
    if starts.is_empty() {
        return parse_external_diff_file_paths(unified_diff)
            .into_iter()
            .next()
            .map(|path| vec![(path, unified_diff.to_string())])
            .unwrap_or_default();
    }

    let mut out = Vec::new();
    for (i, start) in starts.iter().copied().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(lines.len());
        let block = lines[start..end].concat();
        if let Some(path) = parse_external_diff_file_paths(&block).into_iter().next() {
            out.push((path, block));
        }
    }
    out
}

pub(crate) fn external_diff_log_body(message: &str) -> Option<&str> {
    if !message.starts_with("External agent diff") {
        return None;
    }
    let first_line_end = message.find('\n')?;
    let body = &message[first_line_end + 1..];
    if body.contains("diff --git ") || body.contains("--- ") || body.contains("@@ ") {
        Some(body)
    } else {
        None
    }
}

pub(crate) fn diff_stats_from_unified_diff(diff: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

pub(crate) fn external_diff_project_root(diff: &str) -> Option<String> {
    for line in diff.lines() {
        let line = line.trim();
        if let Some(root) = line.strip_prefix("# intendant-project-root:") {
            let root = root.trim();
            if !root.is_empty() {
                return Some(root.to_string());
            }
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            break;
        }
    }
    None
}

pub(crate) fn path_keys_match(a: &Path, b: &Path) -> bool {
    let clean = |path: &Path| {
        path.canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string()
    };
    clean(a) == clean(b)
}

pub(crate) fn external_diff_kind(diff: &str) -> &'static str {
    if diff
        .lines()
        .any(|line| line.starts_with("new file mode") || line == "--- /dev/null")
    {
        return "created";
    }
    if diff
        .lines()
        .any(|line| line.starts_with("deleted file mode") || line == "+++ /dev/null")
    {
        return "deleted";
    }
    "modified"
}

pub(crate) fn path_is_inside_project_root(project_root: &Path, path: &Path) -> bool {
    if !path.is_absolute() {
        return true;
    }
    if path.starts_with(project_root) {
        return true;
    }
    let Ok(root) = project_root.canonicalize() else {
        return false;
    };
    match path.canonicalize() {
        Ok(resolved) => resolved.starts_with(root),
        Err(_) => false,
    }
}

pub(crate) fn safe_relative_change_path(path: &str) -> Option<String> {
    let rel = Path::new(path);
    if rel.is_absolute() {
        return None;
    }
    if rel
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
        && !crate::file_watcher::should_ignore(rel)
    {
        Some(crate::file_watcher::rel_path_key(rel))
    } else {
        None
    }
}

pub(crate) fn project_relative_external_diff_path(
    project_root: &Path,
    path: &str,
) -> Option<String> {
    let path_obj = Path::new(path);
    if !path_obj.is_absolute() {
        return safe_relative_change_path(path);
    }
    if let Ok(rel) = path_obj.strip_prefix(project_root) {
        return safe_relative_change_path(&crate::file_watcher::rel_path_key(rel));
    }
    if let Ok(root) = project_root.canonicalize() {
        if let Ok(rel) = path_obj.strip_prefix(root) {
            return safe_relative_change_path(&crate::file_watcher::rel_path_key(rel));
        }
    }
    None
}

pub(crate) fn load_external_change_records(
    snapshot_dir: &Path,
    project_root: &Path,
    include_diff: bool,
    include_project_root_paths: bool,
) -> Vec<ChangeRecord> {
    let Some(log_dir) = snapshot_dir.parent() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(log_dir.join("session.jsonl")) else {
        return Vec::new();
    };

    let mut by_path: HashMap<String, ChangeRecord> = HashMap::new();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = value.get("message").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(diff_body) = external_diff_log_body(message) else {
            continue;
        };
        if let Some(log_project_root) = external_diff_project_root(diff_body) {
            if !path_keys_match(Path::new(&log_project_root), project_root) {
                continue;
            }
        }
        for (path, block) in split_external_unified_diff_by_file(diff_body) {
            let path_obj = Path::new(&path);
            let project_relative = project_relative_external_diff_path(project_root, &path);
            let (display_path, kind, reason) = if let Some(rel) = project_relative {
                if !include_project_root_paths {
                    continue;
                }
                (rel, external_diff_kind(&block), None)
            } else {
                if !path_obj.is_absolute() || path_is_inside_project_root(project_root, path_obj) {
                    continue;
                }
                (
                    path.clone(),
                    "external",
                    Some(
                        "outside tracked project root; shown from external agent diff log"
                            .to_string(),
                    ),
                )
            };
            let (lines_added, lines_removed) = diff_stats_from_unified_diff(&block);
            by_path.insert(
                display_path.clone(),
                ChangeRecord {
                    path: display_path,
                    kind,
                    lines_added,
                    lines_removed,
                    diff_available: true,
                    reason,
                    diff: include_diff.then_some(block),
                },
            );
        }
    }

    let mut records: Vec<_> = by_path.into_values().collect();
    records.sort_by(|a, b| a.path.cmp(&b.path));
    records
}

pub(crate) fn collect_baseline_text_paths(baseline_dir: &Path) -> HashSet<String> {
    let mut paths = HashSet::new();
    let mut stack = vec![baseline_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(baseline_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if crate::file_watcher::should_ignore(rel) {
                continue;
            }
            paths.insert(crate::file_watcher::rel_path_key(rel));
        }
    }
    paths
}

pub(crate) fn collect_current_change_states(
    project_root: &Path,
) -> HashMap<String, ChangeFileState> {
    let mut states = HashMap::new();
    let mut stack = vec![project_root.to_path_buf()];
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
                if let Ok(rel) = path.strip_prefix(project_root) {
                    if !crate::file_watcher::should_ignore(rel) {
                        stack.push(path);
                    }
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(project_root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if crate::file_watcher::should_ignore(rel) {
                continue;
            }
            let key = crate::file_watcher::rel_path_key(rel);
            match crate::file_watcher::inspect_file(&path) {
                Ok(crate::file_watcher::InspectedFile::Text(snapshot)) => {
                    states.insert(
                        key,
                        ChangeFileState::Text {
                            content: snapshot.text,
                            hash: snapshot.hash_hex,
                        },
                    );
                }
                Ok(crate::file_watcher::InspectedFile::Unsupported(snapshot)) => {
                    states.insert(
                        key,
                        ChangeFileState::Unsupported {
                            hash: snapshot.hash_hex,
                            reason: snapshot.reason,
                        },
                    );
                }
                Err(_) => continue,
            }
        }
    }
    states
}

pub(crate) fn inspect_current_change_state(
    project_root: &Path,
    rel_key: &str,
) -> Option<ChangeFileState> {
    let path = project_root.join(Path::new(rel_key));
    if !path.exists() {
        return None;
    }
    match crate::file_watcher::inspect_file(&path) {
        Ok(crate::file_watcher::InspectedFile::Text(snapshot)) => Some(ChangeFileState::Text {
            content: snapshot.text,
            hash: snapshot.hash_hex,
        }),
        Ok(crate::file_watcher::InspectedFile::Unsupported(snapshot)) => {
            Some(ChangeFileState::Unsupported {
                hash: snapshot.hash_hex,
                reason: snapshot.reason,
            })
        }
        Err(_) => None,
    }
}

pub(crate) fn read_baseline_text(baseline_dir: &Path, rel_key: &str) -> Option<String> {
    std::fs::read_to_string(baseline_dir.join(Path::new(rel_key))).ok()
}

pub(crate) fn baseline_hash_for(
    baseline_text: Option<&str>,
    baseline_meta: Option<&crate::file_watcher::BaselineFileMeta>,
) -> Option<String> {
    baseline_meta.map(|m| m.hash.clone()).or_else(|| {
        baseline_text.map(|s| {
            crate::file_watcher::hex_encode(&crate::file_watcher::sha256_hash(s.as_bytes()))
        })
    })
}

pub(crate) fn diff_stat_pair(baseline: &str, current: &str) -> (u32, u32) {
    let diff = similar::TextDiff::from_lines(baseline, current);
    let mut added = 0u32;
    let mut removed = 0u32;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

pub(crate) fn unsupported_change_record(
    rel_key: &str,
    kind: &'static str,
    reason: String,
) -> ChangeRecord {
    ChangeRecord {
        path: rel_key.to_string(),
        kind,
        lines_added: 0,
        lines_removed: 0,
        diff_available: false,
        reason: Some(reason),
        diff: None,
    }
}

pub(crate) fn compute_change_record(
    rel_key: &str,
    baseline_dir: &Path,
    current: Option<&ChangeFileState>,
    baseline_manifest: &crate::file_watcher::BaselineManifest,
    include_diff: bool,
) -> Option<ChangeRecord> {
    let baseline_text = read_baseline_text(baseline_dir, rel_key);
    let baseline_meta = baseline_manifest.get(rel_key);
    let baseline_exists = baseline_text.is_some() || baseline_meta.is_some();
    let baseline_supported_text =
        baseline_text.is_some() && baseline_meta.map(|m| m.supported_text).unwrap_or(true);

    match (
        baseline_exists,
        baseline_supported_text,
        baseline_text.as_deref(),
        current,
    ) {
        (false, _, _, None) => None,
        (false, _, _, Some(ChangeFileState::Text { content, .. })) => {
            let (lines_added, lines_removed) = diff_stat_pair("", content);
            let diff = include_diff
                .then(|| crate::file_watcher::compute_unified_diff("", content, rel_key));
            Some(ChangeRecord {
                path: rel_key.to_string(),
                kind: "created",
                lines_added,
                lines_removed,
                diff_available: true,
                reason: None,
                diff,
            })
        }
        (false, _, _, Some(ChangeFileState::Unsupported { reason, .. })) => Some(
            unsupported_change_record(rel_key, "created", reason.clone()),
        ),
        (true, true, Some(base), None) => {
            let diff =
                include_diff.then(|| crate::file_watcher::compute_unified_diff(base, "", rel_key));
            Some(ChangeRecord {
                path: rel_key.to_string(),
                kind: "deleted",
                lines_added: 0,
                lines_removed: base.lines().count() as u32,
                diff_available: true,
                reason: None,
                diff,
            })
        }
        (true, false, _, None) => {
            let reason = baseline_meta
                .and_then(|m| m.reason.clone())
                .unwrap_or_else(|| "baseline file was not text-diffable".to_string());
            Some(unsupported_change_record(rel_key, "deleted", reason))
        }
        (true, true, Some(base), Some(ChangeFileState::Text { content, hash })) => {
            let baseline_hash = baseline_hash_for(Some(base), baseline_meta);
            if baseline_hash.as_ref() == Some(hash) || base == content {
                return None;
            }
            let (lines_added, lines_removed) = diff_stat_pair(base, content);
            let diff = include_diff
                .then(|| crate::file_watcher::compute_unified_diff(base, content, rel_key));
            Some(ChangeRecord {
                path: rel_key.to_string(),
                kind: "modified",
                lines_added,
                lines_removed,
                diff_available: true,
                reason: None,
                diff,
            })
        }
        (true, true, Some(base), Some(ChangeFileState::Unsupported { hash, reason })) => {
            let baseline_hash = baseline_hash_for(Some(base), baseline_meta);
            if baseline_hash.as_ref() == Some(hash) {
                return None;
            }
            Some(unsupported_change_record(
                rel_key,
                "modified",
                reason.clone(),
            ))
        }
        (true, false, _, Some(ChangeFileState::Text { hash, .. }))
        | (true, false, _, Some(ChangeFileState::Unsupported { hash, .. })) => {
            if baseline_meta.map(|m| &m.hash) == Some(hash) {
                return None;
            }
            let reason = baseline_meta
                .and_then(|m| m.reason.clone())
                .unwrap_or_else(|| "baseline file was not text-diffable".to_string());
            Some(unsupported_change_record(rel_key, "modified", reason))
        }
        _ => None,
    }
}

pub(crate) fn change_record_summary_json(record: &ChangeRecord) -> serde_json::Value {
    serde_json::json!({
        "path": record.path.clone(),
        "kind": record.kind,
        "lines_added": record.lines_added,
        "lines_removed": record.lines_removed,
        "diff_available": record.diff_available,
        "reason": record.reason.clone(),
    })
}

pub(crate) fn change_record_detail_json(record: &ChangeRecord) -> serde_json::Value {
    serde_json::json!({
        "path": record.path.clone(),
        "kind": record.kind,
        "diff": record.diff.clone().unwrap_or_default(),
        "lines_added": record.lines_added,
        "lines_removed": record.lines_removed,
        "diff_available": record.diff_available,
        "reason": record.reason.clone(),
    })
}

/// List all files that have changed since the session baseline.
pub(crate) fn handle_changes_list(
    snapshot_dir: &Path,
    baseline_dir: &Path,
    project_root: &Path,
    include_project_external_logs: bool,
) -> String {
    let baseline_manifest = load_baseline_manifest(baseline_dir);
    let baseline_paths = collect_baseline_text_paths(baseline_dir);
    let current_states = collect_current_change_states(project_root);
    let mut keys: HashSet<String> = baseline_manifest.keys().cloned().collect();
    keys.extend(baseline_paths);
    keys.extend(current_states.keys().cloned());

    let mut changes = Vec::new();
    let mut sorted_keys: Vec<String> = keys.into_iter().collect();
    sorted_keys.sort();
    for key in sorted_keys {
        if crate::file_watcher::should_ignore(Path::new(&key)) {
            continue;
        }
        if let Some(record) = compute_change_record(
            &key,
            baseline_dir,
            current_states.get(&key),
            &baseline_manifest,
            false,
        ) {
            changes.push(change_record_summary_json(&record));
        }
    }
    let existing_paths: HashSet<String> = changes
        .iter()
        .filter_map(|value| {
            value
                .get("path")
                .and_then(|path| path.as_str())
                .map(str::to_string)
        })
        .collect();
    for record in load_external_change_records(
        snapshot_dir,
        project_root,
        false,
        include_project_external_logs,
    ) {
        if !existing_paths.contains(&record.path) {
            changes.push(change_record_summary_json(&record));
        }
    }
    serde_json::to_string(&changes).unwrap_or_else(|_| "[]".to_string())
}

/// Return a unified diff for a single file.
pub(crate) fn handle_changes_file_diff(
    file_path: &str,
    snapshot_dir: &Path,
    baseline_dir: &Path,
    project_root: &Path,
    include_project_external_logs: bool,
) -> (&'static str, String) {
    let decoded = url_path_decode(file_path);
    // Reject path traversal.
    let rel = Path::new(&decoded);
    if rel.is_absolute() {
        if let Some(record) = load_external_change_records(
            snapshot_dir,
            project_root,
            true,
            include_project_external_logs,
        )
        .into_iter()
        .find(|record| record.path == decoded)
        {
            return ("200 OK", change_record_detail_json(&record).to_string());
        }
        return (
            "404 Not Found",
            serde_json::json!({"error": "no changes for path"}).to_string(),
        );
    }
    for component in rel.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "invalid path"}).to_string(),
            );
        }
    }
    if crate::file_watcher::should_ignore(rel) {
        return (
            "404 Not Found",
            serde_json::json!({"error": "no changes for path"}).to_string(),
        );
    }

    let baseline_path = baseline_dir.join(rel);
    let current_path = project_root.join(rel);

    // Verify existing resolved paths stay within their roots. Missing paths
    // are safe after the component check above; canonicalizing a missing
    // `baseline/<created-file>` path can otherwise mix `/tmp` and
    // `/private/tmp` spellings on macOS and reject valid created files.
    if baseline_path.exists() {
        if let (Ok(resolved_baseline), Ok(resolved_root)) =
            (baseline_path.canonicalize(), baseline_dir.canonicalize())
        {
            if !resolved_baseline.starts_with(&resolved_root) {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": "invalid path"}).to_string(),
                );
            }
        } else {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "invalid path"}).to_string(),
            );
        }
    }
    if current_path.exists() {
        if let (Ok(resolved_current), Ok(resolved_root)) =
            (current_path.canonicalize(), project_root.canonicalize())
        {
            if !resolved_current.starts_with(&resolved_root) {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": "invalid path"}).to_string(),
                );
            }
        } else {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "invalid path"}).to_string(),
            );
        }
    }

    let baseline_manifest = load_baseline_manifest(baseline_dir);
    let current = inspect_current_change_state(project_root, &decoded);

    match compute_change_record(
        &decoded,
        baseline_dir,
        current.as_ref(),
        &baseline_manifest,
        true,
    ) {
        Some(record) => ("200 OK", change_record_detail_json(&record).to_string()),
        None => {
            if let Some(record) = load_external_change_records(
                snapshot_dir,
                project_root,
                true,
                include_project_external_logs,
            )
            .into_iter()
            .find(|record| record.path == decoded)
            {
                return ("200 OK", change_record_detail_json(&record).to_string());
            }
            (
                "404 Not Found",
                serde_json::json!({"error": "no changes for path"}).to_string(),
            )
        }
    }
}

pub(crate) fn managed_context_query_session_id(request_line: &str) -> Option<String> {
    request_query_param(request_line, "backend_session_id")
        .or_else(|| request_query_param(request_line, "session_id"))
        .or_else(|| request_query_param(request_line, "session"))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

pub(crate) fn managed_context_query_wrapper_session_id(request_line: &str) -> Option<String> {
    request_query_param(request_line, "intendant_session_id")
        .or_else(|| request_query_param(request_line, "wrapper_session_id"))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

pub(crate) fn managed_context_safe_log_dir_id(id: &str) -> Option<String> {
    let id = id.trim();
    // Reject anything that isn't a plain single path component. `:` matters on
    // Windows, where `Path::join("C:")` (or `C:foo`) yields a drive-relative path
    // that escapes the logs dir; we also reject path separators, NUL, and the
    // `.`/`..` traversal components on every platform.
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id.contains(':')
        || id.contains('\0')
    {
        return None;
    }
    Some(id.to_string())
}

pub(crate) fn managed_context_named_log_dir(home: &Path, session_id: &str) -> Option<PathBuf> {
    let session_id = managed_context_safe_log_dir_id(session_id)?;
    let path = crate::platform::intendant_home_in(home)
        .join("logs")
        .join(session_id);
    path.is_dir().then_some(path)
}

pub(crate) fn managed_context_line_mentions_session(line: &str, session_id: &str) -> bool {
    if !line.contains(session_id) {
        return false;
    }
    if line.contains("\"session_identity\"")
        || line.contains("External agent thread:")
        || line.contains("Mode: external agent")
        || line.contains("\"backend_session_id\"")
    {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .map(|value| {
            value.pointer("/data/session_id").and_then(|v| v.as_str()) == Some(session_id)
                || value
                    .pointer("/data/backend_session_id")
                    .and_then(|v| v.as_str())
                    == Some(session_id)
                || value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .and_then(external_agent_thread_id_from_message)
                    .as_deref()
                    == Some(session_id)
        })
        .unwrap_or(false)
}

pub(crate) fn managed_context_trace_dirs_mention_session(log_dir: &Path, session_id: &str) -> bool {
    let trace_root = log_dir.join("model-request-traces");
    let Ok(entries) = std::fs::read_dir(trace_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .map(|name| name.contains(session_id))
            .unwrap_or(false)
    })
}

pub(crate) fn managed_context_log_dir_mentions_session(log_dir: &Path, session_id: &str) -> bool {
    if log_dir.file_name().and_then(|name| name.to_str()) == Some(session_id) {
        return true;
    }
    if managed_context_trace_dirs_mention_session(log_dir, session_id) {
        return true;
    }
    let session_path = log_dir.join("session.jsonl");
    let Ok(file) = std::fs::File::open(session_path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    reader
        .lines()
        .map_while(Result::ok)
        .any(|line| managed_context_line_mentions_session(&line, session_id))
}

pub(crate) fn managed_context_candidate_log_dirs(
    home: &Path,
    active_log_dir: Option<&Path>,
    session_id: Option<&str>,
    wrapper_session_id: Option<&str>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen_dirs = HashSet::new();
    let mut push_dir = |path: PathBuf| {
        let key = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .to_string();
        if seen_dirs.insert(key) {
            dirs.push(path);
        }
    };

    if let Some(log_dir) = active_log_dir {
        push_dir(log_dir.to_path_buf());
    }
    if let Some(wrapper_session_id) = wrapper_session_id {
        if let Some(path) = managed_context_named_log_dir(home, wrapper_session_id) {
            push_dir(path);
            if session_id.is_none() || session_id == Some(wrapper_session_id) {
                return dirs;
            }
        }
    }
    if let Some(session_id) = session_id {
        if let Some(path) = managed_context_named_log_dir(home, session_id) {
            push_dir(path);
            if wrapper_session_id.is_none() || wrapper_session_id == Some(session_id) {
                return dirs;
            }
        }
        let logs_dir = crate::platform::intendant_home_in(home).join("logs");
        if let Ok(entries) = std::fs::read_dir(&logs_dir) {
            for entry in entries.flatten() {
                let log_dir = entry.path();
                if log_dir.is_dir()
                    && (managed_context_log_dir_mentions_session(&log_dir, session_id)
                        || crate::context_rewind::records_dir(&log_dir).is_dir())
                {
                    push_dir(log_dir);
                }
            }
        }
    }
    dirs
}

pub(crate) fn managed_context_backend_session_id_from_log_dir(log_dir: &Path) -> Option<String> {
    let session_path = log_dir.join("session.jsonl");
    let file = std::fs::File::open(session_path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("event").and_then(|v| v.as_str()) == Some("session_identity") {
            if let Some(id) = value
                .get("data")
                .and_then(|data| data.get("backend_session_id"))
                .and_then(|v| v.as_str())
                .and_then(clean_external_thread_id)
            {
                return Some(id);
            }
        }
        if let Some(id) = value
            .get("message")
            .and_then(|v| v.as_str())
            .and_then(external_agent_thread_id_from_message)
        {
            return Some(id);
        }
    }
    None
}

pub(crate) fn managed_context_push_filter_session_id(
    filter_session_ids: &mut Vec<String>,
    id: Option<&str>,
) {
    let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    if !filter_session_ids.iter().any(|existing| existing == id) {
        filter_session_ids.push(id.to_string());
    }
}

pub(crate) fn managed_context_extend_candidate_log_dirs(
    dirs: &mut Vec<PathBuf>,
    extra_dirs: Vec<PathBuf>,
) {
    let mut seen_dirs: HashSet<String> = dirs
        .iter()
        .map(|path| {
            std::fs::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .to_string()
        })
        .collect();
    for path in extra_dirs {
        let key = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .to_string();
        if seen_dirs.insert(key) {
            dirs.push(path);
        }
    }
}

pub(crate) fn managed_context_record_matches_session(
    record: &crate::context_rewind::ContextRewindRecord,
    session_id: &str,
) -> bool {
    record.thread_id == session_id || record.session_id.as_deref() == Some(session_id)
}

pub(crate) fn managed_context_record_matches_any_session(
    record: &crate::context_rewind::ContextRewindRecord,
    session_ids: &[String],
) -> bool {
    session_ids
        .iter()
        .any(|session_id| managed_context_record_matches_session(record, session_id))
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ManagedContextAnchor {
    item_id: String,
    session_id: Option<String>,
    intendant_session_id: Option<String>,
    tool_name: String,
    preview: String,
    status: Option<String>,
    created_at: Option<String>,
    trace_path: String,
}

pub(crate) fn managed_context_anchor_timestamp(value: &serde_json::Value) -> Option<String> {
    value
        .get("wall_time_unix_ms")
        .and_then(|v| v.as_i64())
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map(|dt| dt.to_rfc3339())
}

pub(crate) fn managed_context_anchor_tool_name(payload: &serde_json::Value) -> String {
    payload
        .pointer("/summary/label")
        .and_then(|v| v.as_str())
        .or_else(|| payload.pointer("/kind/type").and_then(|v| v.as_str()))
        .unwrap_or("tool")
        .to_string()
}

pub(crate) fn managed_context_anchor_preview(payload: &serde_json::Value) -> String {
    payload
        .pointer("/summary/input_preview")
        .and_then(|v| v.as_str())
        .or_else(|| {
            payload
                .pointer("/summary/output_preview")
                .and_then(|v| v.as_str())
        })
        .map(|s| compact_text(s, 240))
        .unwrap_or_default()
}

pub(crate) fn managed_context_anchor_session_id(value: &serde_json::Value) -> Option<String> {
    value_str(value, "thread_id")
        .or_else(|| {
            value
                .pointer("/payload/thread_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| value_str(value, "rollout_id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn managed_context_anchor_matches_session(
    anchor: &ManagedContextAnchor,
    session_id: &str,
) -> bool {
    anchor.session_id.as_deref() == Some(session_id)
        || anchor.intendant_session_id.as_deref() == Some(session_id)
}

/// Transport-neutral core of `GET /api/managed-context/anchors` (tunnel
/// twin `api_managed_context_anchors`): the deduped, capped anchor
/// catalog from the home-scoped candidate scan, under the canonical
/// json tail.
pub(crate) fn managed_context_anchors_response_from_home(
    request_line: &str,
    active_log_dir: Option<&Path>,
    home: &Path,
) -> ApiResponse {
    let session_id = managed_context_query_session_id(request_line);
    let wrapper_session_id = managed_context_query_wrapper_session_id(request_line);
    let filter_session_id = session_id.as_deref().or(wrapper_session_id.as_deref());
    let mut anchors = Vec::new();
    let mut seen_dirs = HashSet::new();

    let dirs = managed_context_candidate_log_dirs(
        home,
        active_log_dir,
        session_id.as_deref(),
        wrapper_session_id.as_deref(),
    );
    if !dirs.is_empty() {
        for log_dir in dirs {
            if let Err(err) = append_managed_context_anchors_from_dir(
                &mut anchors,
                &mut seen_dirs,
                &log_dir,
                filter_session_id,
            ) {
                return ApiResponse::json_error(
                    500,
                    format!("failed to read managed-context anchors: {err}"),
                );
            }
        }
    } else if active_log_dir.is_some() {
        // Active log was present but unreadable or raced with session teardown.
        return ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "anchors": [] })));
    } else if session_id.is_none() && wrapper_session_id.is_none() {
        return ApiResponse::json_error(
            404,
            "managed-context anchors need an active session log",
        );
    } else {
        let Some(log_dir) = active_log_dir else {
            anchors.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            return ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({ "anchors": anchors })),
            );
        };
        if let Err(err) = append_managed_context_anchors_from_dir(
            &mut anchors,
            &mut seen_dirs,
            log_dir,
            filter_session_id,
        ) {
            return ApiResponse::json_error(
                500,
                format!("failed to read managed-context anchors: {err}"),
            );
        }
    }

    anchors.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let mut seen_items = HashSet::new();
    anchors.retain(|anchor| seen_items.insert(anchor.item_id.clone()));
    anchors.truncate(MANAGED_CONTEXT_ANCHOR_LIMIT);
    ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "anchors": anchors })))
}

pub(crate) fn append_managed_context_records_from_dir(
    records: &mut Vec<crate::context_rewind::ContextRewindRecord>,
    seen_dirs: &mut std::collections::HashSet<String>,
    log_dir: &Path,
    session_ids: &[String],
) -> std::io::Result<()> {
    let key = std::fs::canonicalize(log_dir)
        .unwrap_or_else(|_| log_dir.to_path_buf())
        .to_string_lossy()
        .to_string();
    if !seen_dirs.insert(key) {
        return Ok(());
    }
    let mut from_dir = crate::context_rewind::list_records(log_dir)?;
    if !session_ids.is_empty() {
        from_dir.retain(|record| managed_context_record_matches_any_session(record, session_ids));
    }
    records.extend(from_dir);
    Ok(())
}

pub(crate) fn append_managed_context_anchors_from_trace_file(
    anchors: &mut Vec<ManagedContextAnchor>,
    trace_path: &Path,
    intendant_session_id: Option<&str>,
    session_id: Option<&str>,
) -> std::io::Result<()> {
    let file = std::fs::File::open(trace_path)?;
    let reader = std::io::BufReader::new(file);
    let mut indexes: HashMap<String, usize> = HashMap::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        match payload.get("type").and_then(|v| v.as_str()) {
            Some("tool_call_started") => {
                let Some(item_id) = value_str(payload, "tool_call_id")
                    .or_else(|| value_str(payload, "model_visible_call_id"))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let anchor = ManagedContextAnchor {
                    item_id: item_id.clone(),
                    session_id: managed_context_anchor_session_id(&value),
                    intendant_session_id: intendant_session_id.map(str::to_string),
                    tool_name: managed_context_anchor_tool_name(payload),
                    preview: managed_context_anchor_preview(payload),
                    status: None,
                    created_at: managed_context_anchor_timestamp(&value),
                    trace_path: trace_path.to_string_lossy().to_string(),
                };
                if let Some(session_id) = session_id {
                    if !managed_context_anchor_matches_session(&anchor, session_id) {
                        continue;
                    }
                }
                indexes.insert(item_id, anchors.len());
                anchors.push(anchor);
            }
            Some("tool_call_ended" | "tool_call_runtime_ended") => {
                let Some(item_id) = value_str(payload, "tool_call_id") else {
                    continue;
                };
                let Some(index) = indexes.get(item_id.trim()).copied() else {
                    continue;
                };
                if anchors[index].status.is_none() {
                    anchors[index].status = value_str(payload, "status");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn append_managed_context_anchors_from_dir(
    anchors: &mut Vec<ManagedContextAnchor>,
    seen_dirs: &mut HashSet<String>,
    log_dir: &Path,
    session_id: Option<&str>,
) -> std::io::Result<()> {
    let key = std::fs::canonicalize(log_dir)
        .unwrap_or_else(|_| log_dir.to_path_buf())
        .to_string_lossy()
        .to_string();
    if !seen_dirs.insert(key) {
        return Ok(());
    }
    let intendant_session_id = log_dir.file_name().and_then(|name| name.to_str());
    let trace_root = log_dir.join("model-request-traces");
    for trace_path in collect_recent_files(
        &trace_root,
        "trace.jsonl",
        MANAGED_CONTEXT_ANCHOR_TRACE_LIMIT,
    ) {
        append_managed_context_anchors_from_trace_file(
            anchors,
            &trace_path,
            intendant_session_id,
            session_id,
        )?;
    }
    Ok(())
}

pub(crate) fn append_managed_context_fission_groups_from_dir(
    groups: &mut Vec<ManagedContextFissionGroup>,
    seen_dirs: &mut std::collections::HashSet<String>,
    log_dir: &Path,
    session_ids: &[String],
) -> std::io::Result<()> {
    let key = std::fs::canonicalize(log_dir)
        .unwrap_or_else(|_| log_dir.to_path_buf())
        .to_string_lossy()
        .to_string();
    if !seen_dirs.insert(key) {
        return Ok(());
    }
    // The session-filtered document reader applies the ledger's own
    // connected-component rule per session id; duplicate groups produced by
    // overlapping filter ids are deduplicated by `group_id` after the final
    // newest-first sort.
    let mut documents = Vec::new();
    if session_ids.is_empty() {
        if let Some(document) = crate::fission_ledger::read_fission_ledger_document(log_dir)? {
            documents.push(document);
        }
    } else {
        for session_id in session_ids {
            if let Some(document) = crate::fission_ledger::read_fission_ledger_document_for_session(
                log_dir, session_id,
            )? {
                documents.push(document);
            }
        }
    }
    for document in &documents {
        for group in &document.groups {
            groups.push(managed_context_fission_group_view(document, group));
        }
    }
    Ok(())
}

/// GET /api/session/current/history — returns serialized `History` JSON.
pub(crate) async fn handle_history_get(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    let w = fw.lock().await;
    let body = serde_json::to_string(w.history()).unwrap_or_else(|_| "{}".to_string());
    ("200 OK", body)
}

/// POST /api/session/current/rollback — body:
/// ```json
/// { "round_id": N,
///   "revert_files": true,          // default true (backward-compat)
///   "revert_conversation": false   // default false
/// }
/// ```
///
/// Each boolean is independent. When both are false the endpoint is a
/// validation-only no-op (returns 400). Existing callers passing only
/// `round_id` get a file-only revert, matching prior behavior.
///
/// `revert_conversation` emits an `AppEvent::ConversationRollbackRequested`
/// on the shared bus. The active agent loop subscribes and either
/// truncates its native `Conversation` (native path), issues
/// `thread/rollback` (Codex), or shuts down and re-initializes
/// (session-reset for Claude Code / Gemini). A matching
/// `AppEvent::ConversationRolledBack` is emitted when the work
/// completes. The HTTP response does not wait for that completion —
/// the dashboard observes the event stream.
pub(crate) async fn handle_history_rollback(
    body_text: &str,
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
    bus: &EventBus,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    if let Err((status, body)) = ensure_idle(agent_state) {
        return (status, body);
    }
    let parsed: serde_json::Value = match serde_json::from_str(body_text) {
        Ok(v) => v,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": format!("invalid body: {}", e)}).to_string(),
            );
        }
    };
    let round_id = match parsed.get("round_id").and_then(|v| v.as_u64()) {
        Some(id) => id,
        None => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": "missing round_id"}).to_string(),
            );
        }
    };
    // Backward-compat: old callers pass only `round_id` and expect a
    // file-only revert. New callers supply both flags.
    let revert_files = parsed
        .get("revert_files")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let revert_conversation = parsed
        .get("revert_conversation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !revert_files && !revert_conversation {
        return (
            "400 Bad Request",
            serde_json::json!({
                "error": "at least one of revert_files / revert_conversation must be true"
            })
            .to_string(),
        );
    }

    // Resolve conversation-rollback parameters before we mutate any
    // state so a downstream failure doesn't leave files half-reverted
    // with no event emitted. Reading the history requires the same
    // mutex the rollback writes use, so we briefly acquire and release.
    let conv_params: Option<(Option<u32>, u32)> = if revert_conversation {
        let w = fw.lock().await;
        let hist = w.history();
        let target_idx = hist.rounds.iter().position(|r| r.id == round_id);
        let head_idx = hist
            .current_head_id
            .and_then(|hid| hist.rounds.iter().position(|r| r.id == hid));
        match (target_idx, head_idx) {
            (Some(t), Some(h)) => {
                // Compute turns to drop from the head turn-count sum
                // between (t, h]. This matches Codex's `numTurns`
                // semantics: the number of turns we want to undo.
                let turns_to_drop: u32 = if t < h {
                    hist.rounds[t + 1..=h]
                        .iter()
                        .map(|r| r.turn_count.unwrap_or(0))
                        .sum()
                } else {
                    0
                };
                let target_msg_count = hist.rounds[t].native_message_count;
                Some((target_msg_count, turns_to_drop))
            }
            (Some(_), None) => {
                // No head — rolling back with no active position is a
                // pure file-state restore; nothing to drop from the
                // conversation side.
                Some((hist.rounds[target_idx.unwrap()].native_message_count, 0))
            }
            _ => {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": format!(
                        "round {} not found in active history", round_id
                    )})
                    .to_string(),
                );
            }
        }
    } else {
        None
    };

    // File rollback (may fail for reasons unrelated to the conversation
    // side; bail out before emitting the conversation event so both
    // halves stay consistent from the user's perspective).
    let file_result_json = if revert_files {
        let mut w = fw.lock().await;
        match w.rollback(round_id) {
            Ok(res) => serde_json::json!({
                "to_round_id": res.to_round_id,
                "files_reverted": res.files_reverted,
            }),
            Err(e) => {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": e.to_string()}).to_string(),
                );
            }
        }
    } else {
        serde_json::json!({ "to_round_id": round_id, "files_reverted": 0 })
    };

    // Dispatch the conversation-rollback event; the agent loop picks it
    // up and emits `ConversationRolledBack` when done.
    if let Some((target_msg_count, turns_to_drop)) = conv_params {
        bus.send(AppEvent::ConversationRollbackRequested {
            round_id,
            target_native_message_count: target_msg_count,
            turns_to_drop,
        });
    }

    (
        "200 OK",
        serde_json::json!({
            "to_round_id": file_result_json["to_round_id"],
            "files_reverted": file_result_json["files_reverted"],
            "revert_files": revert_files,
            "revert_conversation": revert_conversation,
        })
        .to_string(),
    )
}

/// POST /api/session/current/redo — no body. Advances `current_head_id`.
pub(crate) async fn handle_history_redo(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    if let Err((status, body)) = ensure_idle(agent_state) {
        return (status, body);
    }
    let mut w = fw.lock().await;
    match w.redo() {
        Ok(res) => (
            "200 OK",
            serde_json::json!({
                "to_round_id": res.to_round_id,
                "files_reverted": res.files_reverted,
            })
            .to_string(),
        ),
        Err(e) => (
            "400 Bad Request",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// POST /api/session/current/prune — drop abandoned branches and GC orphaned
/// content-addressed blobs.
pub(crate) async fn handle_history_prune(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> (&'static str, String) {
    let Some(fw) = file_watcher else {
        return (
            "503 Service Unavailable",
            serde_json::json!({"error": "file watcher not active"}).to_string(),
        );
    };
    let mut w = fw.lock().await;
    match w.prune_abandoned() {
        Ok(res) => (
            "200 OK",
            serde_json::json!({
                "branches_removed": res.branches_removed,
                "bytes_freed": res.bytes_freed,
            })
            .to_string(),
        ),
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// Transport-neutral core of `GET /api/session/current/changes[/…]`
/// (tunnel twin `api_session_current_changes`): the change list / one
/// file's unified diff under the wildcard-CORS tail. The request line
/// arrives transport-decoded — HTTP passes it verbatim, the tunnel
/// synthesizes it from its path/query params.
pub(crate) fn session_current_changes_api_response(
    request_line: &str,
    snapshot_dir: Option<&Path>,
    project_root: Option<&Path>,
    home: &Path,
) -> ApiResponse {
    let (status, body) =
        handle_changes_request_for_home(request_line, snapshot_dir, project_root, home);
    session_wildcard_json_response(status_line_code(status), body)
}

pub(crate) async fn handle_session_current_changes(
    stream: DemuxStream,
    request_line: &str,
    project_root_for_changes: Option<PathBuf>,
    snapshot_dir: Option<PathBuf>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // File change tracking endpoints:
    //   GET /api/session/current/changes        — list all changed files
    //   GET /api/session/current/changes/{path} — unified diff for one file
    let response = session_current_changes_api_response(
        request_line,
        snapshot_dir.as_deref(),
        project_root_for_changes.as_deref(),
        &crate::platform::home_dir(),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `GET /api/session/current/history` (tunnel
/// twin `api_session_current_history`): the serialized rewind History —
/// or the 503 watcher-absent shape — under the wildcard-CORS tail.
pub(crate) async fn current_history_api_response(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> ApiResponse {
    let (status, body) = handle_history_get(file_watcher).await;
    session_wildcard_json_response(status_line_code(status), body)
}

pub(crate) async fn handle_current_history(
    stream: DemuxStream,
    file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // GET /api/session/current/history — serialized History.
    let response = current_history_api_response(file_watcher.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/session/current/rollback`
/// (tunnel twin `api_session_current_rollback`): the shared rollback
/// core — validation, file revert, conversation-rollback event — under
/// the wildcard-CORS tail.
pub(crate) async fn current_rollback_api_response(
    body_text: &str,
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
    bus: &EventBus,
) -> ApiResponse {
    let (status, body) =
        handle_history_rollback(body_text, file_watcher, agent_state, bus).await;
    session_wildcard_json_response(status_line_code(status), body)
}

pub(crate) async fn handle_current_rollback(
    stream: DemuxStream,
    body_text: String,
    bus: EventBus,
    query_ctx: Option<WebQueryCtx>,
    file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // POST /api/session/current/rollback body:
    //   {"round_id": N,
    //    "revert_files": bool (default true),
    //    "revert_conversation": bool (default false)}
    let agent_state = query_ctx.as_ref().map(|ctx| ctx.agent_state.clone());
    let response = current_rollback_api_response(
        &body_text,
        file_watcher.as_ref(),
        agent_state.as_ref(),
        &bus,
    )
    .await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/session/current/redo` (tunnel
/// twin `api_session_current_redo`), under the wildcard-CORS tail.
pub(crate) async fn current_redo_api_response(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> ApiResponse {
    let (status, body) = handle_history_redo(file_watcher, agent_state).await;
    session_wildcard_json_response(status_line_code(status), body)
}

pub(crate) async fn handle_current_redo(
    stream: DemuxStream,
    query_ctx: Option<WebQueryCtx>,
    file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // POST /api/session/current/redo — no body required (dispatch
    // drains any body sent anyway).
    let agent_state = query_ctx.as_ref().map(|ctx| ctx.agent_state.clone());
    let response = current_redo_api_response(file_watcher.as_ref(), agent_state.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/session/current/prune` (tunnel
/// twin `api_session_current_prune`), under the wildcard-CORS tail.
pub(crate) async fn current_prune_api_response(
    file_watcher: Option<&crate::file_watcher::SharedFileWatcher>,
) -> ApiResponse {
    let (status, body) = handle_history_prune(file_watcher).await;
    session_wildcard_json_response(status_line_code(status), body)
}

pub(crate) async fn handle_current_prune(
    stream: DemuxStream,
    file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // POST /api/session/current/prune — no body required (dispatch
    // drains any body sent anyway).
    let response = current_prune_api_response(file_watcher.as_ref()).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_current_agent_output(
    stream: DemuxStream,
    body_text: String,
    query_ctx: Option<WebQueryCtx>,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let log_dir = current_session_log_dir(session_log.as_ref(), query_ctx.as_ref());
    let response = match log_dir {
        // The transport edge resolves the real home for the fallback
        // sweep — only when there is an active log to serve from.
        Some(dir) => {
            current_agent_output_api_response(&crate::platform::home_dir(), &body_text, &dir)
        }
        None => session_wildcard_json_error(404, "no active session log"),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `GET /api/sessions` (tunnel twin
/// `api_sessions`; the hot list path — `PreSerialized` keeps it
/// allocation-identical, risk R8). Params arrive transport-decoded; the
/// composition below is the row's semantics: the ids filter wins, then
/// the limit truncation, then the usage projection. Answers 200 with the
/// wildcard-CORS tail so the multi-host Stats tab can fetch sibling
/// daemons' session lists for its "All Sessions" / "Disk Usage" cards.
pub(crate) fn sessions_list_api_response(
    home: &Path,
    ids_filter: Option<Vec<String>>,
    limit: Option<usize>,
    usage_view: bool,
) -> ApiResponse {
    let body = match ids_filter {
        Some(ids) => cached_list_sessions_for_ids(home, &ids),
        None => match limit {
            Some(limit) => cached_list_sessions_with_limit(limit),
            None => cached_list_sessions(),
        },
    };
    let body = limit_session_list_body(&body, limit);
    let body = if usage_view {
        session_list_body_usage_view(&body)
    } else {
        body
    };
    session_wildcard_json_response(200, body)
}

pub(crate) async fn handle_sessions_list(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let ids_filter = session_ids_filter_from_request(request_line);
    let limit = session_list_limit_from_request(request_line);
    let usage_view = session_list_usage_view_from_request(request_line);
    let home = crate::platform::home_dir();
    let response = match tokio::task::spawn_blocking(move || {
        sessions_list_api_response(&home, ids_filter, limit, usage_view)
    })
    .await
    {
        Ok(response) => response,
        Err(e) => session_wildcard_json_response(
            200,
            serde_json::json!({
                "error": format!("session list task failed: {e}")
            })
            .to_string(),
        ),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of session deletion (all five HTTP wire
/// shapes; tunnel twin `api_session_delete`): the bare-id policy, target
/// resolution, and store removal live in `delete_session_data`; the
/// delete tail historically orders the wildcard CORS header before
/// `Cache-Control`, unlike the list/search tail.
pub(crate) fn session_delete_api_response(home: &Path, session_id: &str, target: &str) -> ApiResponse {
    ApiResponse::Json {
        status: 200,
        body: JsonBody::PreSerialized(delete_session_data(home, session_id, target)),
        headers: vec![
            ("Access-Control-Allow-Origin", "*".to_string()),
            ("Cache-Control", "no-cache".to_string()),
            ("Connection", "close".to_string()),
        ],
    }
}

pub(crate) async fn handle_session_delete(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Transport edge: resolve the real home once; the golden transcripts
    // drive the `_from_home` variant with an injected temp home.
    handle_session_delete_from_home(
        stream,
        request_line,
        cors,
        fleet_origin,
        &crate::platform::home_dir(),
    )
    .await;
}

pub(crate) async fn handle_session_delete_from_home(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
    home: &Path,
) {
    // DELETE /api/session/{id}[/{target}]  (native DELETE)
    // POST  /api/session/{id}/delete[/{target}]  (WKWebView fallback)
    let rest = request_line
        .split("/api/session/")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("");
    let rest_parts: Vec<&str> = rest
        .split('/')
        .filter(|s| !s.is_empty() && *s != "delete")
        .collect();
    let session_id = rest_parts.first().copied().unwrap_or("");
    let target = rest_parts.get(1).copied().unwrap_or("session");
    let response = session_delete_api_response(home, session_id, target);
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_session_agent_output(
    stream: DemuxStream,
    body_text: String,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Transport edge: resolve the real home once; the golden transcripts
    // drive the `_from_home` variant with an injected temp home.
    handle_session_agent_output_from_home(
        stream,
        body_text,
        request_line,
        cors,
        fleet_origin,
        &crate::platform::home_dir(),
    )
    .await;
}

pub(crate) async fn handle_session_agent_output_from_home(
    stream: DemuxStream,
    body_text: String,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
    home: &Path,
) {
    let rest = request_line
        .split("/api/session/")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("");
    let path = rest.split('?').next().unwrap_or(rest);
    let rest_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let session_id = rest_parts.first().copied().unwrap_or("");
    let is_agent_output_route = rest_parts.get(1).copied() == Some("agent-output");
    let source = query_param(request_line, "source").unwrap_or_else(|| "intendant".to_string());
    let response = if is_agent_output_route {
        session_agent_output_api_response(home, &body_text, session_id, &source)
    } else {
        session_wildcard_json_error(404, "unknown session output route")
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_session_sub_router(
    stream: DemuxStream,
    request_line: &str,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    query_ctx: Option<WebQueryCtx>,
) {
    // Transport edge: resolve the real home once; the golden transcripts
    // drive the `_from_home` variant with an injected temp home.
    handle_session_sub_router_from_home(
        stream,
        request_line,
        session_log,
        query_ctx,
        &crate::platform::home_dir(),
    )
    .await;
}

pub(crate) async fn handle_session_sub_router_from_home(
    mut stream: DemuxStream,
    request_line: &str,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    query_ctx: Option<WebQueryCtx>,
    home: &Path,
) {
    use tokio::io::AsyncWriteExt;
    // Extract the rest after /api/session/ and split into parts
    let rest = request_line
        .split("/api/session/")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("");
    let rest_parts: Vec<&str> = rest.split('/').collect();

    let route_name = rest_parts
        .get(1)
        .map(|part| part.split('?').next().unwrap_or(part))
        .unwrap_or("");

    if rest_parts.len() >= 2 && route_name == "context-snapshot" {
        // GET /api/session/{id}/context-snapshot?file=...
        // Replays exactly one archived context snapshot
        // on demand so historical session replay can stay
        // lightweight by default.
        let raw_id = rest_parts[0];
        let session_id = raw_id.split('?').next().unwrap_or(raw_id);
        let source = query_param(request_line, "source").unwrap_or_else(|| "intendant".to_string());
        // Historical HTTP precedence: the bare-id check answers before
        // the selector decode (the tunnel's transport-owned decode keeps
        // its own historical index-error-first order).
        let response = if !session_lookup_id_is_safe(session_id) {
            ApiResponse::json_error(400, "invalid session id")
        } else {
            match context_snapshot_selector_parts_from_request(request_line) {
                Ok((file, request_id, request_index, ts)) => {
                    session_context_snapshot_api_response(
                        home,
                        session_id,
                        &source,
                        file,
                        request_id,
                        request_index,
                        ts,
                    )
                }
                Err(error) => ApiResponse::json_error(400, error),
            }
        };
        let bytes = api_response_http_bytes(
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        );
        let _ = stream.write_all(&bytes).await;
    } else if rest_parts.len() >= 2 && route_name == "recordings" {
        // Session recording sub-routes: /api/session/{id}/recordings[/...]
        let session_id = rest_parts[0];
        let rec_rest = &rest_parts[2..]; // parts after "recordings"

        if !session_lookup_id_is_safe(session_id) {
            let response = upload_error_response("400 Bad Request", "invalid session id");
            let _ = stream.write_all(response.as_bytes()).await;
        } else if rec_rest.len() == 2 && (rec_rest[1] == "segments" || rec_rest[1] == "playlist.m3u8")
        {
            // GET /api/session/{id}/recordings/{stream}/{segments|playlist.m3u8}
            // — the tunnel twin's listing-asset vocabulary, resolved
            // through the shared content core.
            let response = session_recording_listing_asset_api_response(
                home,
                session_id,
                rec_rest[0],
                rec_rest[1],
            );
            let bytes = api_response_http_bytes(
                response,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            );
            let _ = stream.write_all(&bytes).await;
        } else if rec_rest.len() == 2 {
            // GET /api/session/{id}/recordings/{stream}/{filename}
            let stream_name = rec_rest[0];
            let filename = rec_rest[1];
            let valid = filename.starts_with("seg_")
                && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                && filename.len() < 30
                && !filename.contains("..");
            if valid {
                let seg_ct = if filename.ends_with(".ts") {
                    "video/mp2t"
                } else {
                    "video/mp4"
                };
                let seg_path = resolve_bare_session_dir_from_home(home, session_id)
                    .map(|d| d.join("recordings").join(stream_name).join(filename));
                if let Some(path) = seg_path.filter(|p| p.exists()) {
                    match tokio::fs::read(&path).await {
                        Ok(data) => {
                            let header = HttpResponse::new("200 OK")
                                .header("Content-Type", seg_ct)
                                .header("Content-Length", data.len().to_string())
                                .header("Cache-Control", "public, max-age=3600")
                                .header("Connection", "close")
                                .into_string();
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&data).await;
                        }
                        Err(_) => {
                            let body = "Failed to read segment";
                            let response = HttpResponse::with_content(
                                "500 Internal Server Error",
                                "text/plain",
                                body,
                            )
                            .header("Connection", "close")
                            .into_string();
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    }
                } else {
                    let body = "Segment not found";
                    let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                        .header("Connection", "close")
                        .into_string();
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            } else {
                let body = "Invalid filename";
                let response = HttpResponse::with_content("400 Bad Request", "text/plain", body)
                    .header("Connection", "close")
                    .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
            }
        } else {
            // GET /api/session/{id}/recordings — list streams
            let response = session_recordings_api_response(home, session_id);
            let bytes = api_response_http_bytes(
                response,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            );
            let _ = stream.write_all(&bytes).await;
        }
    } else if rest_parts.len() >= 2 && route_name == "report" {
        // GET /api/session/{id}/report — download a zip of
        // the current session's text artifacts for sharing
        // with the dev. Pass id="current" to target the
        // live daemon's own session via WebQueryCtx.
        let session_id = rest_parts[0];
        let response = match session_report_zip_for_request(
            home,
            session_id,
            session_log.as_ref(),
            query_ctx.as_ref(),
        ) {
            Ok(report) => session_report_api_response(report),
            // Per-lane error framing, historical: the id-policy failure
            // answers json under the wildcard tail; the resolution and
            // build failures answer text/plain.
            Err(SessionReportZipError::InvalidSessionId) => {
                session_wildcard_json_error(400, "invalid session id")
            }
            Err(SessionReportZipError::NotFound) => {
                session_text_plain_response(404, "Session not found".to_string())
            }
            Err(SessionReportZipError::Build(e)) => {
                session_text_plain_response(500, format!("Failed to build report: {}", e))
            }
        };
        let bytes = api_response_http_bytes(
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        );
        let _ = stream.write_all(&bytes).await;
    } else if rest_parts.len() >= 2 && route_name == "frames" {
        // Session frame sub-routes: /api/session/{id}/frames[/{filename}]
        use tokio::io::AsyncWriteExt;
        let session_id = rest_parts[0];
        let frame_rest = &rest_parts[2..];

        if !session_lookup_id_is_safe(session_id) {
            let response = upload_error_response("400 Bad Request", "invalid session id");
            let _ = stream.write_all(response.as_bytes()).await;
        } else if frame_rest.len() == 1 {
            // GET /api/session/{id}/frames/{filename}
            let filename = frame_rest[0];
            let valid = (filename.ends_with(".jpg") || filename.ends_with(".png"))
                && filename.len() < 80
                && !filename.contains("..");
            if valid {
                let ct = if filename.ends_with(".png") {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                let frame_path = resolve_bare_session_dir_from_home(home, session_id)
                    .map(|d| d.join("frames").join(filename));
                if let Some(path) = frame_path.filter(|p| p.exists()) {
                    match tokio::fs::read(&path).await {
                        Ok(data) => {
                            let header = HttpResponse::new("200 OK")
                                .header("Content-Type", ct)
                                .header("Content-Length", data.len().to_string())
                                .header("Cache-Control", "public, max-age=3600")
                                .header("Connection", "close")
                                .into_string();
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&data).await;
                        }
                        Err(_) => {
                            let body = "Failed to read frame";
                            let response = HttpResponse::with_content(
                                "500 Internal Server Error",
                                "text/plain",
                                body,
                            )
                            .header("Connection", "close")
                            .into_string();
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    }
                } else {
                    let body = "Frame not found";
                    let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                        .header("Connection", "close")
                        .into_string();
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            } else {
                let body = "Invalid filename";
                let response = HttpResponse::with_content("400 Bad Request", "text/plain", body)
                    .header("Connection", "close")
                    .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
            }
        } else {
            // GET /api/session/{id}/frames — list frame filenames
            let body = if let Some(session_dir) = resolve_bare_session_dir_from_home(home, session_id)
            {
                let frames_dir = session_dir.join("frames");
                let mut names: Vec<String> = Vec::new();
                if frames_dir.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&frames_dir) {
                        for e in entries.flatten() {
                            let n = e.file_name().to_string_lossy().to_string();
                            if n.ends_with(".jpg") || n.ends_with(".png") {
                                names.push(n);
                            }
                        }
                    }
                    names.sort();
                }
                serde_json::to_string(&names).unwrap_or("[]".to_string())
            } else {
                "[]".to_string()
            };
            let response = HttpResponse::with_content("200 OK", "application/json", body)
                .header("Cache-Control", "no-cache")
                .header("Connection", "close")
                .into_string();
            let _ = stream.write_all(response.as_bytes()).await;
        }
    } else {
        // GET /api/session/{id} — session detail
        let raw_id = rest_parts[0];
        let session_id = raw_id.split('?').next().unwrap_or(raw_id);
        let source = query_param(request_line, "source").unwrap_or_else(|| "intendant".to_string());
        let entry_limit = session_detail_entry_limit_from_request(request_line);
        let entry_before = session_detail_before_from_request(request_line);
        let session_id_owned = session_id.to_string();
        let home = home.to_path_buf();
        let response = match tokio::task::spawn_blocking(move || {
            session_detail_api_response(&home, &session_id_owned, &source, entry_limit, entry_before)
        })
        .await
        {
            Ok(response) => response,
            // Historical shape: a failed detail task answers 200 with the
            // error body (the status mapping only knows "not found").
            Err(e) => ApiResponse::json(
                200,
                JsonBody::PreSerialized(
                    serde_json::json!({
                        "error": format!("session detail task failed: {e}")
                    })
                    .to_string(),
                ),
            ),
        };
        let bytes = api_response_http_bytes(
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        );
        let _ = stream.write_all(&bytes).await;
    }
    finalize_http_stream(&mut stream).await;
}

/// Transport-neutral core of session detail (`GET /api/session/{id}`,
/// tunnel twin `api_session_detail`): the bare-id policy check (the raw,
/// untrimmed id — the HTTP lane's historical strictness; the tunnel
/// trims before delegating), then the paged replay body with its
/// historical status mapping (404 only for "session not found").
pub(crate) fn session_detail_api_response(
    home: &Path,
    session_id: &str,
    source: &str,
    limit: Option<usize>,
    before: Option<usize>,
) -> ApiResponse {
    if !session_lookup_id_is_safe(session_id) {
        return ApiResponse::json_error(400, "invalid session id");
    }
    let body = session_detail_response_body_with_page(home, session_id, source, limit, before);
    let status = if session_detail_http_status(&body) == "404 Not Found" {
        404
    } else {
        200
    };
    ApiResponse::json(status, JsonBody::PreSerialized(body))
}

/// Transport-neutral core of the context-snapshot replay
/// (`GET /api/session/{id}/context-snapshot`, tunnel twin
/// `api_session_context_snapshot`): selector parts arrive transport-
/// decoded (query string vs frame params); the bare-id policy, selector
/// validation, and log-dir scan are the shared
/// `session_context_snapshot_response_body` core.
/// text/plain rendering for the session artifact error bodies (report
/// and asset leaves keep their historical plain-text wordings on the
/// HTTP lane; the tunnel answers its own json envelopes).
pub(crate) fn session_text_plain_response(status: u16, body: String) -> ApiResponse {
    ApiResponse::Bytes {
        status,
        content_type: "text/plain".to_string(),
        headers: vec![("Connection", "close".to_string())],
        bytes: BytesPayload::InMemory(body.into_bytes()),
        meta: serde_json::Value::Null,
    }
}

/// Bytes-lane rendering of a built session report (tunnel twin
/// `api_session_report`): the zip under its historical attachment tail;
/// the meta sidecar is the tunnel's `byte_stream_end.result` object.
pub(crate) fn session_report_api_response(report: SessionReportZip) -> ApiResponse {
    let size = report.bytes.len();
    ApiResponse::Bytes {
        status: 200,
        content_type: "application/zip".to_string(),
        headers: vec![
            (
                "Content-Disposition",
                format!("attachment; filename=\"{}\"", report.filename),
            ),
            ("Cache-Control", "no-cache".to_string()),
            ("Connection", "close".to_string()),
        ],
        meta: serde_json::json!({
            "ok": true,
            "filename": report.filename,
            "content_type": "application/zip",
            "size": size,
        }),
        bytes: BytesPayload::InMemory(report.bytes),
    }
}

/// Transport-neutral core of the recordings stream list
/// (`GET /api/session/{id}/recordings`, tunnel twin
/// `api_session_recordings`).
pub(crate) fn session_recordings_api_response(home: &Path, session_id: &str) -> ApiResponse {
    let (status_line, body) = session_recordings_list_response_body(home, session_id);
    ApiResponse::json(status_line_code(status_line), JsonBody::PreSerialized(body))
}

/// Neutral core of the recordings listing-asset leaves (the tunnel
/// twin's "segments" / "playlist.m3u8" asset vocabulary): the shared
/// resolver supplies the bytes; the HTTP tail is the canonical no-cache
/// pair. Segment files stay on their own transport-owned carriage (the
/// tunnel streams ranged and capped; HTTP serves the whole file).
pub(crate) fn session_recording_listing_asset_api_response(
    home: &Path,
    session_id: &str,
    stream_name: &str,
    asset: &str,
) -> ApiResponse {
    match resolve_session_recording_asset(
        resolve_bare_session_dir_from_home(home, session_id),
        stream_name,
        asset,
    ) {
        Ok(RecordingAsset::Bytes {
            bytes,
            content_type,
            ..
        }) => ApiResponse::Bytes {
            status: 200,
            content_type: content_type.to_string(),
            headers: vec![
                ("Cache-Control", "no-cache".to_string()),
                ("Connection", "close".to_string()),
            ],
            bytes: BytesPayload::InMemory(bytes),
            meta: serde_json::Value::Null,
        },
        // The listing assets never resolve to files or errors; a wiring
        // bug fails closed.
        _ => ApiResponse::json_error(500, "unexpected recording asset resolution"),
    }
}

pub(crate) fn session_context_snapshot_api_response(
    home: &Path,
    session_id: &str,
    source: &str,
    file: Option<String>,
    request_id: Option<String>,
    request_index: Option<u64>,
    ts: Option<String>,
) -> ApiResponse {
    let (status_line, body) = session_context_snapshot_response_body(
        home,
        session_id,
        source,
        file,
        request_id,
        request_index,
        ts,
    );
    ApiResponse::json(
        status_line_code(status_line),
        JsonBody::PreSerialized(body),
    )
}

pub(crate) async fn handle_mc_anchors(
    stream: DemuxStream,
    request_line: &str,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match session_log.as_ref() {
        Some(log) => match log.lock() {
            Ok(log) => {
                let active_log_dir = log.dir().to_path_buf();
                managed_context_anchors_response_from_home(
                    request_line,
                    Some(active_log_dir.as_path()),
                    &crate::platform::home_dir(),
                )
            }
            Err(_) => ApiResponse::json_error(500, "session log lock poisoned"),
        },
        None => managed_context_anchors_response_from_home(
            request_line,
            None,
            &crate::platform::home_dir(),
        ),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_mc_records(
    stream: DemuxStream,
    request_line: &str,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match session_log.as_ref() {
        Some(log) => match log.lock() {
            Ok(log) => {
                let active_log_dir = log.dir().to_path_buf();
                managed_context_records_response_from_home(
                    request_line,
                    Some(active_log_dir.as_path()),
                    &crate::platform::home_dir(),
                )
            }
            Err(_) => ApiResponse::json_error(500, "session log lock poisoned"),
        },
        None => managed_context_records_response_from_home(
            request_line,
            None,
            &crate::platform::home_dir(),
        ),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_mc_fission(
    stream: DemuxStream,
    request_line: &str,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match session_log.as_ref() {
        Some(log) => match log.lock() {
            Ok(log) => {
                let active_log_dir = log.dir().to_path_buf();
                managed_context_fission_response_from_home(
                    request_line,
                    Some(active_log_dir.as_path()),
                    &crate::platform::home_dir(),
                )
            }
            Err(_) => ApiResponse::json_error(500, "session log lock poisoned"),
        },
        None => managed_context_fission_response_from_home(
            request_line,
            None,
            &crate::platform::home_dir(),
        ),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_sessions_stream(mut stream: DemuxStream, request_line: &str) {
    let request_line_for_stream = request_line.to_string();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let stream_task = tokio::task::spawn_blocking(move || {
        stream_sessions_from_request(&request_line_for_stream, tx);
    });
    let response = HttpResponse::new("200 OK")
        .header("Content-Type", "application/x-ndjson")
        .header("Cache-Control", "no-cache")
        .header("Access-Control-Allow-Origin", "*")
        .header("Connection", "close")
        .into_string();
    use tokio::io::AsyncWriteExt;
    if stream.write_all(response.as_bytes()).await.is_ok() {
        while let Some(line) = rx.recv().await {
            if stream.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    }
    let _ = stream_task.await;
    finalize_http_stream(&mut stream).await;
}

/// Transport-neutral core of `GET /api/sessions/search` (tunnel twin
/// `api_sessions_search`): one search composition — the single-flight
/// guard plus the blocking store scan — under the caller's cancellation
/// token, answered 200 under the wildcard-CORS tail.
pub(crate) async fn sessions_search_api_response(
    query: String,
    source_filter: String,
    mode: String,
    project_filter: Vec<String>,
    cancel: tokio_util::sync::CancellationToken,
) -> ApiResponse {
    let body = sessions_search_response_body_with_cancel(
        query,
        source_filter,
        mode,
        project_filter,
        cancel,
    )
    .await;
    session_wildcard_json_response(200, body)
}

pub(crate) async fn handle_sessions_search(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let query = query_param(request_line, "q").unwrap_or_default();
    let source_filter = query_param(request_line, "source").unwrap_or_else(|| "all".to_string());
    let mode = query_param(request_line, "mode").unwrap_or_default();
    let project_filter = session_project_filter_from_request(request_line);
    let response = sessions_search_api_response(
        query,
        source_filter,
        mode,
        project_filter,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_worktrees_inspect(
    stream: DemuxStream,
    body_text: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let response =
        match tokio::task::spawn_blocking(move || worktrees_inspect_api_response(&home, &body_text))
            .await
        {
            Ok(response) => response,
            Err(e) => ApiResponse::json(
                500,
                JsonBody::PreSerialized(
                    serde_json::json!({
                        "ok": false,
                        "error": format!("worktree inspect task failed: {e}")
                    })
                    .to_string(),
                ),
            ),
        };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_worktrees_remove(
    stream: DemuxStream,
    body_text: String,
    worktree_inventory_cache: Arc<Mutex<Option<String>>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let response = match tokio::task::spawn_blocking(move || {
        worktrees_remove_api_response(&home, &body_text, &worktree_inventory_cache)
    })
    .await
    {
        Ok(response) => response,
        Err(e) => ApiResponse::json(
            500,
            JsonBody::PreSerialized(
                serde_json::json!({
                    "ok": false,
                    "error": format!("worktree removal task failed: {e}")
                })
                .to_string(),
            ),
        ),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_worktrees_merge(
    mut stream: DemuxStream,
    body_text: String,
    worktree_inventory_cache: Arc<Mutex<Option<String>>>,
) {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let cache = worktree_inventory_cache.clone();
    let (status, body) = match tokio::task::spawn_blocking(move || {
        let result = merge_session_worktree_response(&home, &body_text);
        if result.0 == "200 OK" {
            // The merge (and usually the removal) changed the inventory;
            // drop the cached scan like the remove handler does.
            if let Ok(mut guard) = cache.lock() {
                *guard = None;
            }
        }
        result
    })
    .await
    {
        Ok(result) => result,
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({
                "ok": false,
                "error": format!("worktree merge task failed: {e}")
            })
            .to_string(),
        ),
    };
    let response = json_response(status, body);
    use tokio::io::AsyncWriteExt;
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_worktrees_scan(
    stream: DemuxStream,
    project_root: Option<PathBuf>,
    worktree_inventory_cache: Arc<Mutex<Option<String>>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let response = match tokio::task::spawn_blocking(move || {
        worktrees_scan_api_response(&home, project_root.as_deref(), &worktree_inventory_cache)
    })
    .await
    {
        Ok(response) => response,
        // Historical shape: a failed scan task answers 200 with the
        // error body.
        Err(e) => ApiResponse::json(
            200,
            JsonBody::PreSerialized(
                serde_json::json!({
                    "error": format!("worktree scan task failed: {e}")
                })
                .to_string(),
            ),
        ),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_worktrees_list(
    stream: DemuxStream,
    worktree_inventory_cache: Arc<Mutex<Option<String>>>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = worktrees_list_api_response(&worktree_inventory_cache);
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_displays(
    mut stream: DemuxStream,
    session_registry: Option<crate::display::SharedSessionRegistry>,
) {
    // Display enumeration endpoint
    use tokio::io::AsyncWriteExt;
    let body = displays_response_body(&session_registry).await;
    let response = HttpResponse::with_content("200 OK", "application/json", body)
        .header("Cache-Control", "no-cache")
        .header("Access-Control-Allow-Origin", "*")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

#[cfg(test)]
pub(crate) fn managed_context_anchors_response(
    request_line: &str,
    log_dir: &Path,
    home: &Path,
) -> String {
    test_render_api_response(managed_context_anchors_response_from_home(
        request_line,
        Some(log_dir),
        home,
    ))
}

/// Transport-neutral core of `GET /api/managed-context/records` (tunnel
/// twin `api_managed_context_records`): the rewind-record index from the
/// home-scoped candidate scan, under the canonical json tail.
pub(crate) fn managed_context_records_response_from_home(
    request_line: &str,
    active_log_dir: Option<&Path>,
    home: &Path,
) -> ApiResponse {
    let session_id = managed_context_query_session_id(request_line);
    let wrapper_session_id = managed_context_query_wrapper_session_id(request_line);
    let mut filter_session_ids = Vec::new();
    managed_context_push_filter_session_id(&mut filter_session_ids, session_id.as_deref());
    managed_context_push_filter_session_id(&mut filter_session_ids, wrapper_session_id.as_deref());
    let mut records = Vec::new();
    let mut seen_dirs = std::collections::HashSet::new();

    let mut dirs = managed_context_candidate_log_dirs(
        home,
        active_log_dir,
        session_id.as_deref(),
        wrapper_session_id.as_deref(),
    );
    let mut query_log_ids = Vec::new();
    managed_context_push_filter_session_id(&mut query_log_ids, wrapper_session_id.as_deref());
    managed_context_push_filter_session_id(&mut query_log_ids, session_id.as_deref());
    for query_log_id in query_log_ids {
        let Some(log_dir) = managed_context_named_log_dir(home, &query_log_id) else {
            continue;
        };
        let Some(backend_session_id) = managed_context_backend_session_id_from_log_dir(&log_dir)
        else {
            continue;
        };
        if filter_session_ids
            .iter()
            .any(|existing| existing == &backend_session_id)
        {
            continue;
        }
        managed_context_push_filter_session_id(
            &mut filter_session_ids,
            Some(backend_session_id.as_str()),
        );
        managed_context_extend_candidate_log_dirs(
            &mut dirs,
            managed_context_candidate_log_dirs(
                home,
                active_log_dir,
                Some(backend_session_id.as_str()),
                Some(query_log_id.as_str()),
            ),
        );
    }
    if !dirs.is_empty() {
        for log_dir in dirs {
            if let Err(err) = append_managed_context_records_from_dir(
                &mut records,
                &mut seen_dirs,
                &log_dir,
                &filter_session_ids,
            ) {
                return ApiResponse::json_error(
                    500,
                    format!("failed to read managed-context records: {err}"),
                );
            }
        }
    } else if active_log_dir.is_some() {
        return ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "records": [] })));
    } else if session_id.is_none() && wrapper_session_id.is_none() {
        return ApiResponse::json_error(
            404,
            "managed-context records need an active session log",
        );
    } else {
        let Some(log_dir) = active_log_dir else {
            records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            return ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({ "records": records })),
            );
        };
        if let Err(err) = append_managed_context_records_from_dir(
            &mut records,
            &mut seen_dirs,
            log_dir,
            &filter_session_ids,
        ) {
            return ApiResponse::json_error(
                500,
                format!("failed to read managed-context records: {err}"),
            );
        }
    }

    records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "records": records })))
}

#[cfg(test)]
pub(crate) fn managed_context_records_response(
    request_line: &str,
    log_dir: &Path,
    home: &Path,
) -> String {
    test_render_api_response(managed_context_records_response_from_home(
        request_line,
        Some(log_dir),
        home,
    ))
}

/// Test-only wire render of a neutral response through the real HTTP
/// adapter under the managed-context rows' own-origin posture.
#[cfg(test)]
pub(crate) fn test_render_api_response(response: ApiResponse) -> String {
    String::from_utf8_lossy(&api_response_http_bytes(
        response,
        crate::gateway_routes::CorsPosture::OwnOrigin,
        None,
    ))
    .into_owned()
}

/// Merged dashboard view of one fission group: the wire ledger fields from
/// [`crate::fission_ledger::FissionGroup`] plus the group-level extension
/// state (detach markers) from the same
/// [`crate::fission_ledger::FissionLedgerDocument`]. Served by
/// `GET /api/managed-context/fission`.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ManagedContextFissionGroup {
    group_id: String,
    parent_session_id: String,
    anchor_item_id: String,
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    objective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_session_id: Option<String>,
    detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detached_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detach_reason: Option<String>,
    branches: Vec<ManagedContextFissionBranch>,
}

/// One branch inside [`ManagedContextFissionGroup`]: the flattened wire
/// branch plus its per-branch extension state (charter, import marker, work
/// metadata).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ManagedContextFissionBranch {
    #[serde(flatten)]
    branch: crate::fission_ledger::FissionBranch,
    #[serde(skip_serializing_if = "Option::is_none")]
    charter: Option<crate::fission_ledger::BranchCharter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported_at: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    changed_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tests_run: Vec<String>,
}

pub(crate) fn managed_context_fission_group_view(
    document: &crate::fission_ledger::FissionLedgerDocument,
    group: &crate::fission_ledger::FissionGroup,
) -> ManagedContextFissionGroup {
    let group_ext = document.group_ext(&group.group_id);
    let branches = group
        .branches
        .iter()
        .take(MANAGED_CONTEXT_FISSION_BRANCH_LIMIT)
        .map(|branch| {
            let branch_ext = group_ext.and_then(|ext| ext.branch(&branch.session_id));
            ManagedContextFissionBranch {
                branch: branch.clone(),
                charter: branch_ext.and_then(|ext| ext.charter.clone()),
                imported_at: branch_ext.and_then(|ext| ext.imported_at.clone()),
                changed_files: branch_ext
                    .map(|ext| ext.changed_files.clone())
                    .unwrap_or_default(),
                tests_run: branch_ext
                    .map(|ext| ext.tests_run.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();
    ManagedContextFissionGroup {
        group_id: group.group_id.clone(),
        parent_session_id: group.parent_session_id.clone(),
        anchor_item_id: group.anchor_item_id.clone(),
        tool: group.tool.clone(),
        objective: group.objective.clone(),
        prompt: group.prompt.clone(),
        created_at: group.created_at.clone(),
        updated_at: group.updated_at.clone(),
        canonical_session_id: group.canonical_session_id.clone(),
        detached: group_ext.is_some_and(crate::fission_ledger::FissionGroupExt::is_detached),
        detached_at: group_ext.and_then(|ext| ext.detached_at.clone()),
        detach_reason: group_ext.and_then(|ext| ext.detach_reason.clone()),
        branches,
    }
}

/// Transport-neutral core of `GET /api/managed-context/fission` (tunnel
/// twin `api_managed_context_fission`): the deduped, capped fission
/// groups from the home-scoped candidate scan, under the canonical json
/// tail.
pub(crate) fn managed_context_fission_response_from_home(
    request_line: &str,
    active_log_dir: Option<&Path>,
    home: &Path,
) -> ApiResponse {
    let session_id = managed_context_query_session_id(request_line);
    let wrapper_session_id = managed_context_query_wrapper_session_id(request_line);
    let mut filter_session_ids = Vec::new();
    managed_context_push_filter_session_id(&mut filter_session_ids, session_id.as_deref());
    managed_context_push_filter_session_id(&mut filter_session_ids, wrapper_session_id.as_deref());
    let mut groups: Vec<ManagedContextFissionGroup> = Vec::new();
    let mut seen_dirs = std::collections::HashSet::new();

    let mut dirs = managed_context_candidate_log_dirs(
        home,
        active_log_dir,
        session_id.as_deref(),
        wrapper_session_id.as_deref(),
    );
    let mut query_log_ids = Vec::new();
    managed_context_push_filter_session_id(&mut query_log_ids, wrapper_session_id.as_deref());
    managed_context_push_filter_session_id(&mut query_log_ids, session_id.as_deref());
    for query_log_id in query_log_ids {
        let Some(log_dir) = managed_context_named_log_dir(home, &query_log_id) else {
            continue;
        };
        let Some(backend_session_id) = managed_context_backend_session_id_from_log_dir(&log_dir)
        else {
            continue;
        };
        if filter_session_ids
            .iter()
            .any(|existing| existing == &backend_session_id)
        {
            continue;
        }
        managed_context_push_filter_session_id(
            &mut filter_session_ids,
            Some(backend_session_id.as_str()),
        );
        managed_context_extend_candidate_log_dirs(
            &mut dirs,
            managed_context_candidate_log_dirs(
                home,
                active_log_dir,
                Some(backend_session_id.as_str()),
                Some(query_log_id.as_str()),
            ),
        );
    }
    if !dirs.is_empty() {
        for log_dir in dirs {
            if let Err(err) = append_managed_context_fission_groups_from_dir(
                &mut groups,
                &mut seen_dirs,
                &log_dir,
                &filter_session_ids,
            ) {
                return ApiResponse::json_error(
                    500,
                    format!("failed to read managed-context fission groups: {err}"),
                );
            }
        }
    } else if active_log_dir.is_some() {
        return ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "groups": [] })));
    } else if session_id.is_none() && wrapper_session_id.is_none() {
        return ApiResponse::json_error(
            404,
            "managed-context fission groups need an active session log",
        );
    } else {
        let Some(log_dir) = active_log_dir else {
            return ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({ "groups": groups })),
            );
        };
        if let Err(err) = append_managed_context_fission_groups_from_dir(
            &mut groups,
            &mut seen_dirs,
            log_dir,
            &filter_session_ids,
        ) {
            return ApiResponse::json_error(
                500,
                format!("failed to read managed-context fission groups: {err}"),
            );
        }
    }

    groups.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let mut seen_groups = HashSet::new();
    groups.retain(|group| seen_groups.insert(group.group_id.clone()));
    groups.truncate(MANAGED_CONTEXT_FISSION_GROUP_LIMIT);
    ApiResponse::json(200, JsonBody::Value(serde_json::json!({ "groups": groups })))
}

/// Delete session data: entire session, media, recordings, frames, or turns.
/// Returns a JSON result with `ok` and `bytes_freed`.
pub(crate) fn delete_session_data(home: &Path, session_id: &str, target: &str) -> String {
    // Path traversal protection
    if !session_lookup_id_is_safe(session_id) {
        return serde_json::json!({"ok": false, "error": "invalid session id"}).to_string();
    }

    let dir = match resolve_bare_session_dir_from_home(home, session_id) {
        Some(d) => d,
        None => return serde_json::json!({"ok": false, "error": "session not found"}).to_string(),
    };

    let dir_byte_size = |path: &std::path::Path| -> u64 {
        let mut total = 0u64;
        if path.is_dir() {
            // On-disk allocation (512-byte blocks) with hardlinked inodes
            // counted once, matching `du` — so bytes_freed reflects the space
            // actually reclaimed, not apparent size.
            fn walk(dir: &std::path::Path, total: &mut u64, seen: &mut HashSet<(u64, u64)>) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for e in entries.flatten() {
                        let p = e.path();
                        if p.is_dir() {
                            walk(&p, total, seen);
                        } else if let Ok(m) = p.metadata() {
                            if crate::platform::metadata_is_multiply_linked(&m)
                                && !seen.insert(crate::platform::metadata_dev_ino(&m))
                            {
                                continue;
                            }
                            *total =
                                total.saturating_add(crate::platform::metadata_on_disk_bytes(&m));
                        }
                    }
                }
            }
            let mut seen: HashSet<(u64, u64)> = HashSet::new();
            walk(path, &mut total, &mut seen);
        }
        total
    };

    match target {
        "session" => {
            let bytes = dir_byte_size(&dir);
            let external_delete_target = external_delete_target_for_intendant_session_dir(&dir);
            match std::fs::remove_dir_all(&dir) {
                Ok(_) => {
                    let mut body =
                        serde_json::json!({"ok": true, "deleted": "session", "bytes_freed": bytes});
                    if let Some((source, external_id)) = external_delete_target {
                        match mark_external_session_deleted(home, &source, &external_id) {
                            Ok(()) => {
                                body["external_session_hidden"] = serde_json::json!(true);
                            }
                            Err(e) => {
                                body["external_session_hidden"] = serde_json::json!(false);
                                body["external_session_hide_error"] = serde_json::json!(e);
                            }
                        }
                    }
                    invalidate_session_list_response_cache();
                    remove_persisted_intendant_row(&dir);
                    body.to_string()
                }
                Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
            }
        }
        "media" => {
            let rec_dir = dir.join("recordings");
            let frames_dir = dir.join("frames");
            let bytes = dir_byte_size(&rec_dir) + dir_byte_size(&frames_dir);
            let mut errors = Vec::new();
            if rec_dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&rec_dir) {
                    errors.push(format!("recordings: {}", e));
                }
            }
            if frames_dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&frames_dir) {
                    errors.push(format!("frames: {}", e));
                }
            }
            if errors.is_empty() {
                serde_json::json!({"ok": true, "deleted": "media", "bytes_freed": bytes})
                    .to_string()
            } else {
                serde_json::json!({"ok": false, "error": errors.join("; "), "bytes_freed": bytes})
                    .to_string()
            }
        }
        "recordings" | "frames" | "turns" => {
            let target_dir = dir.join(target);
            let bytes = dir_byte_size(&target_dir);
            if !target_dir.is_dir() {
                serde_json::json!({"ok": true, "deleted": target, "bytes_freed": 0}).to_string()
            } else {
                match std::fs::remove_dir_all(&target_dir) {
                    Ok(_) => {
                        serde_json::json!({"ok": true, "deleted": target, "bytes_freed": bytes})
                            .to_string()
                    }
                    Err(e) => {
                        serde_json::json!({"ok": false, "error": e.to_string(), "bytes_freed": 0})
                            .to_string()
                    }
                }
            }
        }
        _ => serde_json::json!({"ok": false, "error": "invalid target"}).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_session_log_dir_uses_live_log_without_query_context() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("headless-session");
        let session_log = Arc::new(Mutex::new(
            crate::session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));

        assert_eq!(
            current_session_log_dir(Some(&session_log), None).unwrap(),
            log_dir
        );
    }

    fn response_json_body(response: &str) -> serde_json::Value {
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .expect("response body");
        serde_json::from_str(body).expect("json response")
    }

    mod worktree_merge {
        use super::*;
        use std::process::Command;

        fn git(repo: &Path, args: &[&str]) {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn init_repo(dir: &Path) {
            git(dir, &["init", "-b", "main"]);
            git(dir, &["config", "user.email", "test@test.com"]);
            git(dir, &["config", "user.name", "Test"]);
            std::fs::write(dir.join("README.md"), "# base\n").unwrap();
            git(dir, &["add", "README.md"]);
            git(dir, &["commit", "-m", "initial"]);
        }

        /// A repo + linked worktree with one committed change, and the
        /// session dir whose meta records the linkage — the state a
        /// worktree session leaves behind when it ends.
        fn linked_session(
            repo: &Path,
            session_dir: &Path,
            branch: &str,
        ) -> crate::session_log::SessionWorktreeMeta {
            init_repo(repo);
            let wt = crate::worktree::create(repo, branch, "HEAD").unwrap();
            std::fs::write(wt.path.join("feature.txt"), "from the worktree\n").unwrap();
            git(&wt.path, &["add", "feature.txt"]);
            git(&wt.path, &["commit", "-m", "worktree change"]);

            let linkage = crate::session_log::SessionWorktreeMeta {
                branch: branch.to_string(),
                path: wt.path.to_string_lossy().to_string(),
                base_root: repo.to_string_lossy().to_string(),
                base_branch: Some("main".to_string()),
                base_sha: crate::worktree::head_commit(repo).ok(),
            };
            std::fs::create_dir_all(session_dir).unwrap();
            let meta = serde_json::json!({
                "session_id": "wt-session",
                "created_at": "2026-07-09T00:00:00",
                "project_root": linkage.path,
                "worktree": linkage,
            });
            std::fs::write(
                session_dir.join("session_meta.json"),
                serde_json::to_string_pretty(&meta).unwrap(),
            )
            .unwrap();
            linkage
        }

        #[test]
        fn clean_merge_lands_and_removes_the_worktree() {
            let repo_dir = tempfile::tempdir().unwrap();
            let session_dir = tempfile::tempdir().unwrap();
            let linkage = linked_session(repo_dir.path(), session_dir.path(), "wt-clean");

            let body = merge_linked_session_worktree(session_dir.path(), &[]).unwrap();
            assert_eq!(body["ok"], true);
            assert_eq!(body["merged"], true);
            assert_eq!(body["merged_into"], "main");
            assert_eq!(body["branch"], "wt-clean");
            assert_eq!(body["removed"], true, "{body}");
            // The change landed in the base checkout; the checkout is gone
            // but the branch ref survives (the work product).
            assert!(repo_dir.path().join("feature.txt").exists());
            assert!(!PathBuf::from(&linkage.path).exists());
            assert!(crate::worktree::branch_exists(repo_dir.path(), "wt-clean"));
        }

        #[test]
        fn conflicted_merge_aborts_and_keeps_everything() {
            let repo_dir = tempfile::tempdir().unwrap();
            let session_dir = tempfile::tempdir().unwrap();
            let linkage = linked_session(repo_dir.path(), session_dir.path(), "wt-conflict");
            // Divergent edit to the same file in the base checkout.
            std::fs::write(repo_dir.path().join("feature.txt"), "from main\n").unwrap();
            git(repo_dir.path(), &["add", "feature.txt"]);
            git(repo_dir.path(), &["commit", "-m", "conflicting main change"]);

            let err = merge_linked_session_worktree(session_dir.path(), &[]).unwrap_err();
            assert!(err.contains("wt-conflict"), "{err}");
            // The merge was aborted: no merge in progress, worktree intact.
            assert!(!repo_dir.path().join(".git").join("MERGE_HEAD").exists());
            assert!(PathBuf::from(&linkage.path).exists());
        }

        #[test]
        fn merge_refuses_when_base_checkout_moved_branches() {
            let repo_dir = tempfile::tempdir().unwrap();
            let session_dir = tempfile::tempdir().unwrap();
            linked_session(repo_dir.path(), session_dir.path(), "wt-moved");
            git(repo_dir.path(), &["checkout", "-b", "elsewhere"]);

            let err = merge_linked_session_worktree(session_dir.path(), &[]).unwrap_err();
            assert!(err.contains("elsewhere"), "{err}");
            assert!(err.contains("main"), "{err}");
        }

        #[test]
        fn merge_requires_a_recorded_linkage() {
            let session_dir = tempfile::tempdir().unwrap();
            std::fs::write(
                session_dir.path().join("session_meta.json"),
                serde_json::json!({
                    "session_id": "plain",
                    "created_at": "2026-07-09T00:00:00",
                })
                .to_string(),
            )
            .unwrap();
            let err = merge_linked_session_worktree(session_dir.path(), &[]).unwrap_err();
            assert!(err.contains("no linked git worktree"), "{err}");
        }

        #[test]
        fn merge_response_rejects_bad_and_unknown_session_ids() {
            let home = tempfile::tempdir().unwrap();
            let (status, body) = merge_session_worktree_response(home.path(), "{}");
            assert_eq!(status, "400 Bad Request", "{body}");
            let (status, body) = merge_session_worktree_response(
                home.path(),
                &serde_json::json!({"session_id": "../escape"}).to_string(),
            );
            assert_eq!(status, "400 Bad Request", "{body}");
            let (status, body) = merge_session_worktree_response(
                home.path(),
                &serde_json::json!({"session_id": "does-not-exist"}).to_string(),
            );
            assert_eq!(status, "404 Not Found", "{body}");
        }

        #[test]
        fn merge_refuses_a_session_linked_to_a_missing_worktree() {
            let repo_dir = tempfile::tempdir().unwrap();
            let session_dir = tempfile::tempdir().unwrap();
            let linkage = linked_session(repo_dir.path(), session_dir.path(), "wt-gone");
            // Simulate the checkout being removed out from under the card.
            git(
                repo_dir.path(),
                &["worktree", "remove", "--force", &linkage.path],
            );
            let err = merge_linked_session_worktree(session_dir.path(), &[]).unwrap_err();
            assert!(err.contains("no longer a registered worktree"), "{err}");
        }
    }

    fn managed_context_test_record(
        record_id: &str,
        session_id: Option<&str>,
        thread_id: &str,
        created_at: &str,
    ) -> crate::context_rewind::ContextRewindRecord {
        crate::context_rewind::ContextRewindRecord {
            record_id: record_id.to_string(),
            created_at: created_at.to_string(),
            session_id: session_id.map(str::to_string),
            thread_id: thread_id.to_string(),
            item_id: "call-1".to_string(),
            position: "after".to_string(),
            reason: Some("crystallize branch".to_string()),
            primer: Some("dense state".to_string()),
            preserve: Vec::new(),
            discard: Vec::new(),
            artifacts: Vec::new(),
            next_steps: Vec::new(),
            source_rollout_path: None,
            recovery_rollout_path: None,
            fission_snapshot: None,
            lineage_ledger: None,
            fission_ledger: None,
            detached_fission_group_ids: Vec::new(),
            used_tokens_at_rewind: None,
            context_window_at_rewind: None,
            pressure_band_at_rewind: None,
            surgical: false,
        }
    }

    #[test]
    fn managed_context_records_response_filters_by_session_or_thread_id() {
        let dir = tempfile::tempdir().unwrap();
        crate::context_rewind::persist_record(
            dir.path(),
            &managed_context_test_record(
                "visible-by-session",
                Some("dashboard session"),
                "thread-a",
                "2026-05-26T00:00:00Z",
            ),
        )
        .unwrap();
        crate::context_rewind::persist_record(
            dir.path(),
            &managed_context_test_record(
                "visible-by-thread",
                Some("other-session"),
                "dashboard session",
                "2026-05-25T00:00:00Z",
            ),
        )
        .unwrap();
        crate::context_rewind::persist_record(
            dir.path(),
            &managed_context_test_record(
                "hidden",
                Some("other-session"),
                "other-thread",
                "2026-05-24T00:00:00Z",
            ),
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let response = managed_context_records_response(
            "GET /api/managed-context/records?session_id=dashboard%20session HTTP/1.1",
            dir.path(),
            home.path(),
        );
        let body = response_json_body(&response);
        let ids: Vec<_> = body["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["record_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["visible-by-session", "visible-by-thread"]);
    }

    #[test]
    fn managed_context_records_response_passes_surgical_and_pressure_fields_through() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = managed_context_test_record(
            "surgical-1",
            Some("dashboard session"),
            "thread-a",
            "2026-06-12T00:00:00Z",
        );
        record.surgical = true;
        record.used_tokens_at_rewind = Some(26_000);
        record.context_window_at_rewind = Some(23_800);
        record.pressure_band_at_rewind = Some("high".to_string());
        crate::context_rewind::persist_record(dir.path(), &record).unwrap();

        let home = tempfile::tempdir().unwrap();
        let response = managed_context_records_response(
            "GET /api/managed-context/records?session_id=dashboard%20session HTTP/1.1",
            dir.path(),
            home.path(),
        );
        let body = response_json_body(&response);
        // The Managed tab renders the SURGICAL badge and the pressure chip
        // straight off these record fields: the endpoint must pass records
        // through whole, not reshape them.
        let served = &body["records"][0];
        assert_eq!(served["record_id"], "surgical-1");
        assert_eq!(served["surgical"], true);
        assert_eq!(served["used_tokens_at_rewind"], 26_000);
        assert_eq!(served["context_window_at_rewind"], 23_800);
        assert_eq!(served["pressure_band_at_rewind"], "high");
    }

    #[test]
    fn managed_context_records_response_accepts_session_alias() {
        let dir = tempfile::tempdir().unwrap();
        crate::context_rewind::persist_record(
            dir.path(),
            &managed_context_test_record(
                "visible",
                Some("session-a"),
                "thread-a",
                "2026-05-26T00:00:00Z",
            ),
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let response = managed_context_records_response(
            "GET /api/managed-context/records?session=session-a HTTP/1.1",
            dir.path(),
            home.path(),
        );
        let body = response_json_body(&response);
        assert_eq!(body["records"].as_array().unwrap().len(), 1);
        assert_eq!(body["records"][0]["record_id"], "visible");
    }

    #[test]
    fn managed_context_records_response_scans_historical_logs_for_session_id() {
        let home = tempfile::tempdir().unwrap();
        let active = tempfile::tempdir().unwrap();
        let old_log = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-a");
        crate::context_rewind::persist_record(
            &old_log,
            &managed_context_test_record(
                "historical",
                Some("wrapper-a"),
                "codex-thread-a",
                "2026-05-27T00:00:00Z",
            ),
        )
        .unwrap();
        crate::context_rewind::persist_record(
            active.path(),
            &managed_context_test_record(
                "active-other",
                Some("active-session"),
                "active-thread",
                "2026-05-28T00:00:00Z",
            ),
        )
        .unwrap();

        let response = test_render_api_response(managed_context_records_response_from_home(
            "GET /api/managed-context/records?session_id=codex-thread-a HTTP/1.1",
            Some(active.path()),
            home.path(),
        ));
        let body = response_json_body(&response);
        let ids: Vec<_> = body["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["record_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["historical"]);
    }

    #[test]
    fn managed_context_records_response_resolves_wrapper_to_backend_thread() {
        let home = tempfile::tempdir().unwrap();
        let active = tempfile::tempdir().unwrap();
        let logs_dir = home.path().join(".intendant").join("logs");
        let wrapper_log = logs_dir.join("wrapper-session");
        std::fs::create_dir_all(&wrapper_log).unwrap();
        std::fs::write(
            wrapper_log.join("session.jsonl"),
            serde_json::json!({
                "event": "debug",
                "message": "External agent thread: codex-thread-a"
            })
            .to_string(),
        )
        .unwrap();
        let managed_host_log = logs_dir.join("managed-host");
        crate::context_rewind::persist_record(
            &managed_host_log,
            &managed_context_test_record(
                "historical-by-thread",
                Some("managed-host"),
                "codex-thread-a",
                "2026-05-27T00:00:00Z",
            ),
        )
        .unwrap();
        crate::context_rewind::persist_record(
            &managed_host_log,
            &managed_context_test_record(
                "hidden",
                Some("managed-host"),
                "other-thread",
                "2026-05-28T00:00:00Z",
            ),
        )
        .unwrap();

        let response = test_render_api_response(managed_context_records_response_from_home(
            "GET /api/managed-context/records?session_id=wrapper-session HTTP/1.1",
            Some(active.path()),
            home.path(),
        ));
        let body = response_json_body(&response);
        let ids: Vec<_> = body["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["record_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["historical-by-thread"]);
    }

    #[test]
    fn managed_context_fission_response_merges_ledger_and_ext_state() {
        use crate::fission_ledger::{BranchCharter, NewSpawnedBranch};

        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Group 1: a chartered spawn that reported work and was imported.
        let chartered = crate::fission_ledger::register_spawned_branch(
            dir.path(),
            "fission-web-parent",
            "anchor-chartered",
            BranchCharter {
                objective: "polish the docs".to_string(),
                write_scope: Some("docs/**".to_string()),
                worktree_requested: true,
            },
            NewSpawnedBranch {
                session_id: "branch-chartered".to_string(),
                worktree_path: Some(std::path::PathBuf::from("/tmp/wt-chartered")),
                ..Default::default()
            },
        )
        .unwrap();
        crate::fission_ledger::update_branch_work(
            dir.path(),
            &chartered.group_id,
            "branch-chartered",
            &["docs/src/a.md".to_string(), "docs/src/b.md".to_string()],
            &["cargo test --bins".to_string()],
            Some("docs polished"),
        )
        .unwrap();
        crate::fission_ledger::mark_branch_imported(
            dir.path(),
            &chartered.group_id,
            "branch-chartered",
            None,
        )
        .unwrap();

        // Group 2: registered later, then detached (newest by updated_at).
        let detached = crate::fission_ledger::register_spawned_branch(
            dir.path(),
            "fission-web-parent",
            "anchor-detached",
            BranchCharter {
                objective: "explore the alternative".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            NewSpawnedBranch {
                session_id: "branch-detached".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        crate::fission_ledger::detach_group(dir.path(), &detached.group_id, "anchor rewound away")
            .unwrap();

        let response = test_render_api_response(managed_context_fission_response_from_home(
            "GET /api/managed-context/fission?session_id=fission-web-parent HTTP/1.1",
            Some(dir.path()),
            home.path(),
        ));
        let body = response_json_body(&response);
        let groups = body["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);

        // Newest first by updated_at: the detach bumped group 2 last.
        let first = &groups[0];
        assert_eq!(first["group_id"], detached.group_id.as_str());
        assert_eq!(first["parent_session_id"], "fission-web-parent");
        assert_eq!(first["anchor_item_id"], "anchor-detached");
        assert_eq!(first["tool"], "spawn_agent");
        assert_eq!(first["detached"], true);
        assert!(first["detached_at"].is_string());
        assert_eq!(first["detach_reason"], "anchor rewound away");
        assert_eq!(first["branches"][0]["session_id"], "branch-detached");
        assert_eq!(first["branches"][0]["status"], "detached");
        assert_eq!(
            first["branches"][0]["charter"]["objective"],
            "explore the alternative"
        );

        let second = &groups[1];
        assert_eq!(second["group_id"], chartered.group_id.as_str());
        assert_eq!(second["detached"], false);
        assert!(second["detached_at"].is_null());
        let branch = &second["branches"][0];
        assert_eq!(branch["session_id"], "branch-chartered");
        assert_eq!(branch["status"], "running");
        assert_eq!(branch["worktree_path"], "/tmp/wt-chartered");
        assert_eq!(branch["summary"], "docs polished");
        assert_eq!(branch["charter"]["objective"], "polish the docs");
        assert_eq!(branch["charter"]["write_scope"], "docs/**");
        assert_eq!(branch["charter"]["worktree_requested"], true);
        assert!(branch["imported_at"].is_string());
        assert_eq!(
            branch["changed_files"],
            serde_json::json!(["docs/src/a.md", "docs/src/b.md"])
        );
        assert_eq!(
            branch["tests_run"],
            serde_json::json!(["cargo test --bins"])
        );

        // A session id outside the connected component sees no groups.
        let unrelated = test_render_api_response(managed_context_fission_response_from_home(
            "GET /api/managed-context/fission?session_id=unrelated-session HTTP/1.1",
            Some(dir.path()),
            home.path(),
        ));
        let unrelated_body = response_json_body(&unrelated);
        assert_eq!(unrelated_body["groups"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn managed_context_anchors_response_reads_trace_anchors_by_backend_session() {
        let dir = tempfile::tempdir().unwrap();
        let trace_dir = dir.path().join("model-request-traces").join("trace-a");
        std::fs::create_dir_all(&trace_dir).unwrap();
        let trace_path = trace_dir.join("trace.jsonl");
        let lines = [
            serde_json::json!({
                "wall_time_unix_ms": 1779944111933i64,
                "rollout_id": "codex-thread-a",
                "thread_id": "codex-thread-a",
                "payload": {
                    "type": "tool_call_started",
                    "tool_call_id": "call-visible",
                    "kind": { "type": "exec_command" },
                    "summary": {
                        "label": "exec_command",
                        "input_preview": "{\"cmd\":\"pwd\"}"
                    }
                }
            }),
            serde_json::json!({
                "wall_time_unix_ms": 1779944112003i64,
                "rollout_id": "codex-thread-a",
                "thread_id": "codex-thread-a",
                "payload": {
                    "type": "tool_call_ended",
                    "tool_call_id": "call-visible",
                    "status": "completed"
                }
            }),
            serde_json::json!({
                "wall_time_unix_ms": 1779944113000i64,
                "rollout_id": "other-thread",
                "thread_id": "other-thread",
                "payload": {
                    "type": "tool_call_started",
                    "tool_call_id": "call-hidden",
                    "kind": { "type": "exec_command" }
                }
            }),
        ];
        std::fs::write(
            trace_path,
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let response = managed_context_anchors_response(
            "GET /api/managed-context/anchors?session_id=codex-thread-a HTTP/1.1",
            dir.path(),
            home.path(),
        );
        let body = response_json_body(&response);
        let anchors = body["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0]["item_id"], "call-visible");
        assert_eq!(anchors[0]["session_id"], "codex-thread-a");
        assert_eq!(anchors[0]["tool_name"], "exec_command");
        assert_eq!(anchors[0]["status"], "completed");
        assert!(anchors[0]["preview"]
            .as_str()
            .unwrap()
            .contains("\"cmd\":\"pwd\""));
    }

    #[test]
    fn managed_context_anchors_response_accepts_wrapper_session_alias() {
        let home = tempfile::tempdir().unwrap();
        let active = tempfile::tempdir().unwrap();
        let old_log = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-a");
        let trace_dir = old_log.join("model-request-traces").join("trace-a");
        std::fs::create_dir_all(&trace_dir).unwrap();
        std::fs::write(
            trace_dir.join("trace.jsonl"),
            serde_json::json!({
                "wall_time_unix_ms": 1779944111933i64,
                "rollout_id": "codex-thread-a",
                "thread_id": "codex-thread-a",
                "payload": {
                    "type": "tool_call_started",
                    "tool_call_id": "call-wrapper",
                    "kind": { "type": "exec_command" }
                }
            })
            .to_string(),
        )
        .unwrap();

        let response = test_render_api_response(managed_context_anchors_response_from_home(
            "GET /api/managed-context/anchors?session_id=wrapper-a HTTP/1.1",
            Some(active.path()),
            home.path(),
        ));
        let body = response_json_body(&response);
        let anchors = body["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0]["item_id"], "call-wrapper");
        assert_eq!(anchors[0]["intendant_session_id"], "wrapper-a");
    }

    #[test]
    fn managed_context_anchors_response_scans_backend_when_wrapper_alias_is_stale() {
        let home = tempfile::tempdir().unwrap();
        let active = tempfile::tempdir().unwrap();
        let logs_dir = home.path().join(".intendant").join("logs");
        let stale_log = logs_dir.join("wrapper-stale");
        let resumed_log = logs_dir.join("wrapper-resumed");
        std::fs::create_dir_all(stale_log.join("model-request-traces")).unwrap();
        let trace_dir = resumed_log
            .join("model-request-traces")
            .join("codex-thread-a-trace");
        std::fs::create_dir_all(&trace_dir).unwrap();
        std::fs::write(
            trace_dir.join("trace.jsonl"),
            serde_json::json!({
                "wall_time_unix_ms": 1779944111933i64,
                "rollout_id": "codex-thread-a",
                "thread_id": "codex-thread-a",
                "payload": {
                    "type": "tool_call_started",
                    "tool_call_id": "call-resumed",
                    "kind": { "type": "exec_command" }
                }
            })
            .to_string(),
        )
        .unwrap();

        let response = test_render_api_response(managed_context_anchors_response_from_home(
            "GET /api/managed-context/anchors?session_id=codex-thread-a&backend_session_id=codex-thread-a&intendant_session_id=wrapper-stale HTTP/1.1",
            Some(active.path()),
            home.path(),
        ));
        let body = response_json_body(&response);
        let anchors = body["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0]["item_id"], "call-resumed");
        assert_eq!(anchors[0]["session_id"], "codex-thread-a");
        assert_eq!(anchors[0]["intendant_session_id"], "wrapper-resumed");
    }

    #[test]
    fn changes_request_decodes_nested_file_path() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/main.rs");
        let current_path = project.path().join("src/main.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "old\nsame\n").unwrap();
        std::fs::write(&current_path, "new\nsame\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/src%2Fmain.rs HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], "src/main.rs");
        assert!(json["diff"].as_str().unwrap().contains("-old"));
        assert!(json["diff"].as_str().unwrap().contains("+new"));
    }

    #[test]
    fn changes_request_without_path_lists_files() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/main.rs");
        let current_path = project.path().join("src/main.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "old\n").unwrap();
        std::fs::write(&current_path, "new\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert!(
            json.as_array().is_some(),
            "list endpoint should return an array"
        );
        assert_eq!(json[0]["path"], "src/main.rs");
    }

    #[test]
    fn changes_request_lists_current_only_created_file() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();
        let current_path = project.path().join("src/new.rs");
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&current_path, "new\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["path"], "src/new.rs");
        assert_eq!(json[0]["kind"], "created");
        assert_eq!(json[0]["lines_added"], 1);
        assert_eq!(json[0]["diff_available"], true);
    }

    #[cfg(unix)]
    #[test]
    fn changes_request_created_file_detail_accepts_symlinked_snapshot_root() {
        use std::os::unix::fs::symlink;

        let holder = tempfile::TempDir::new().unwrap();
        let real_log = holder.path().join("real-log");
        let linked_log = holder.path().join("linked-log");
        std::fs::create_dir_all(real_log.join("file_snapshots/baseline")).unwrap();
        symlink(&real_log, &linked_log).unwrap();

        let snapshot_dir = linked_log.join("file_snapshots");
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(project.path().join("new file.txt"), "created\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/new%20file.txt HTTP/1.1",
            Some(&snapshot_dir),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], "new file.txt");
        assert_eq!(json["kind"], "created");
        assert!(json["diff"].as_str().unwrap().contains("+created"));
    }

    #[test]
    fn changes_request_surfaces_external_absolute_diff_from_session_log() {
        let log = tempfile::TempDir::new().unwrap();
        let snapshot_dir = log.path().join("file_snapshots");
        let project = tempfile::TempDir::new().unwrap();
        let external = tempfile::TempDir::new().unwrap();
        let external_path = external.path().join("outside file.txt");
        let external_display = external_path.to_string_lossy();
        std::fs::create_dir_all(snapshot_dir.join("baseline")).unwrap();
        let diff = format!(
            "External agent diff: {external_display}\n# intendant-project-root: {}\n--- a/{external_display}\n+++ b/{external_display}\n@@ -0,0 +1,2 @@\n+alpha\n+beta\n",
            project.path().display()
        );
        let entry = serde_json::json!({
            "event": "info",
            "message": diff,
        });
        std::fs::write(log.path().join("session.jsonl"), format!("{entry}\n")).unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(&snapshot_dir),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["path"], external_display.as_ref());
        assert_eq!(json[0]["kind"], "external");
        assert_eq!(json[0]["lines_added"], 2);
        assert!(json[0]["reason"]
            .as_str()
            .unwrap()
            .contains("outside tracked project root"));

        let encoded = external_display
            .replace('%', "%25")
            .replace('/', "%2F")
            .replace(' ', "%20");
        let request = format!("GET /api/session/current/changes/{encoded} HTTP/1.1");
        let (status, body) =
            handle_changes_request(&request, Some(&snapshot_dir), Some(project.path()));
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], external_display.as_ref());
        assert_eq!(json["kind"], "external");
        assert!(json["diff"].as_str().unwrap().contains("+alpha"));
    }

    #[test]
    fn changes_request_targets_external_wrapper_diff_without_baseline() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let logs_dir = home.path().join(".intendant/logs");
        let wrapper_id = "wrapper-session";
        let backend_id = "backend-session";
        let wrapper_dir = logs_dir.join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(
            wrapper_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": wrapper_id,
                "created_at": "2026-05-29T06:11:20",
                "project_root": project.path().to_string_lossy(),
                "task": "external diff"
            })
            .to_string(),
        )
        .unwrap();
        let diff = format!(
            "External agent diff: 2 files (created.txt, tracked.txt)\n# intendant-project-root: {}\ndiff --git a/created.txt b/created.txt\nnew file mode 100644\n--- /dev/null\n+++ b/created.txt\n@@ -0,0 +1 @@\n+created\ndiff --git a/tracked.txt b/tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1 +1 @@\n-old\n+new\n",
            project.path().display()
        );
        let session_lines = vec![
            serde_json::json!({"event": "info", "message": "Mode: external agent (Codex)"}),
            serde_json::json!({"event": "debug", "message": format!("External agent thread: {backend_id}")}),
            serde_json::json!({"event": "info", "message": diff}),
        ];
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            session_lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let default_snapshot = tempfile::TempDir::new().unwrap();
        let default_project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(default_snapshot.path().join("baseline")).unwrap();

        let request = format!("GET /api/session/current/changes?session_id={backend_id} HTTP/1.1");
        let (status, body) = handle_changes_request_for_home(
            &request,
            Some(default_snapshot.path()),
            Some(default_project.path()),
            home.path(),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let paths: Vec<_> = json
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("path").and_then(|path| path.as_str()))
            .collect();

        assert_eq!(status, "200 OK");
        assert_eq!(paths, vec!["created.txt", "tracked.txt"]);
        assert_eq!(json[0]["kind"], "created");
        assert_eq!(json[1]["kind"], "modified");

        let request = format!(
            "GET /api/session/current/changes/created.txt?session_id={backend_id} HTTP/1.1"
        );
        let (status, body) = handle_changes_request_for_home(
            &request,
            Some(default_snapshot.path()),
            Some(default_project.path()),
            home.path(),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], "created.txt");
        assert!(json["diff"].as_str().unwrap().contains("+created"));
    }

    #[test]
    fn changes_request_target_without_snapshot_falls_back_to_live_project() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let logs_dir = home.path().join(".intendant/logs");
        let wrapper_id = "wrapper-session";
        let backend_id = "backend-session";
        let wrapper_dir = logs_dir.join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(
            wrapper_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": wrapper_id,
                "created_at": "2026-05-29T06:11:20",
                "project_root": project.path().to_string_lossy(),
                "task": "external session without watcher snapshots"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            [
                serde_json::json!({"event": "info", "message": "Mode: external agent (Codex)"})
                    .to_string(),
                serde_json::json!({"event": "debug", "message": format!("External agent thread: {backend_id}")})
                    .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let live_snapshot = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(live_snapshot.path().join("baseline")).unwrap();
        std::fs::write(project.path().join("created.txt"), "created\n").unwrap();

        let request = format!("GET /api/session/current/changes?session_id={backend_id} HTTP/1.1");
        let (status, body) = handle_changes_request_for_home(
            &request,
            Some(live_snapshot.path()),
            Some(project.path()),
            home.path(),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["path"], "created.txt");
        assert_eq!(json[0]["kind"], "created");

        let request = format!(
            "GET /api/session/current/changes/created.txt?session_id={backend_id} HTTP/1.1"
        );
        let (status, body) = handle_changes_request_for_home(
            &request,
            Some(live_snapshot.path()),
            Some(project.path()),
            home.path(),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], "created.txt");
        assert!(json["diff"].as_str().unwrap().contains("+created"));
    }

    #[test]
    fn changes_request_prefers_matching_external_log_with_changes() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(project.path().join("created.txt"), "created\n").unwrap();
        std::fs::write(project.path().join("tracked.txt"), "new\n").unwrap();
        let logs_dir = home.path().join(".intendant/logs");
        let backend_id = "backend-session";

        let attach_dir = logs_dir.join("attach-wrapper");
        std::fs::create_dir_all(attach_dir.join("file_snapshots/baseline")).unwrap();
        std::fs::write(
            attach_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "attach-wrapper",
                "project_root": project.path().to_string_lossy(),
                "task": "reattach"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            attach_dir.join("session.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"event": "info", "message": "Mode: external agent (Codex)"}),
                serde_json::json!({"event": "debug", "message": format!("External agent thread: {backend_id}")})
            ),
        )
        .unwrap();
        std::fs::write(
            attach_dir.join("file_snapshots/baseline/created.txt"),
            "created\n",
        )
        .unwrap();
        std::fs::write(
            attach_dir.join("file_snapshots/baseline/tracked.txt"),
            "new\n",
        )
        .unwrap();

        let wrapper_dir = logs_dir.join("original-wrapper");
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(
            wrapper_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "original-wrapper",
                "project_root": project.path().to_string_lossy(),
                "task": "external diff"
            })
            .to_string(),
        )
        .unwrap();
        let diff = format!(
            "External agent diff: 2 files (created.txt, tracked.txt)\n# intendant-project-root: {}\ndiff --git a/created.txt b/created.txt\nnew file mode 100644\n--- /dev/null\n+++ b/created.txt\n@@ -0,0 +1 @@\n+created\ndiff --git a/tracked.txt b/tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1 +1 @@\n-old\n+new\n",
            project.path().display()
        );
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            [
                serde_json::json!({"event": "info", "message": "Mode: external agent (Codex)"})
                    .to_string(),
                serde_json::json!({"event": "debug", "message": format!("External agent thread: {backend_id}")})
                    .to_string(),
                serde_json::json!({"event": "info", "message": diff}).to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let default_snapshot = tempfile::TempDir::new().unwrap();
        let default_project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(default_snapshot.path().join("baseline")).unwrap();

        let request = format!("GET /api/session/current/changes?session_id={backend_id} HTTP/1.1");
        let (status, body) = handle_changes_request_for_home(
            &request,
            Some(default_snapshot.path()),
            Some(default_project.path()),
            home.path(),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let paths: Vec<_> = json
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("path").and_then(|path| path.as_str()))
            .collect();

        assert_eq!(status, "200 OK");
        assert_eq!(paths, vec!["created.txt", "tracked.txt"]);
    }

    #[test]
    fn changes_request_lists_created_empty_file() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();
        std::fs::write(project.path().join("empty.txt"), "").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["path"], "empty.txt");
        assert_eq!(json[0]["kind"], "created");
        assert_eq!(json[0]["lines_added"], 0);
        assert_eq!(json[0]["lines_removed"], 0);
    }

    #[test]
    fn changes_request_empty_baseline_file_modified_is_not_created() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/empty.txt");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "").unwrap();
        std::fs::write(project.path().join("empty.txt"), "now has text\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json[0]["path"], "empty.txt");
        assert_eq!(json[0]["kind"], "modified");
        assert_eq!(json[0]["lines_added"], 1);
    }

    #[test]
    fn changes_request_created_then_deleted_net_zero_is_absent() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[test]
    fn changes_request_ignores_nested_worktrees() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();
        let worktree_file = project.path().join(".worktrees/feature/src/main.rs");
        std::fs::create_dir_all(worktree_file.parent().unwrap()).unwrap();
        std::fs::write(worktree_file, "fn main() {}\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[test]
    fn changes_request_reports_unsupported_current_for_text_baseline() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/main.rs");
        let current_path = project.path().join("src/main.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "fn main() {}\n").unwrap();
        std::fs::write(&current_path, b"fn\0main").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json[0]["path"], "src/main.rs");
        assert_eq!(json[0]["kind"], "modified");
        assert_eq!(json[0]["diff_available"], false);
        assert_eq!(json[0]["reason"], "binary file");

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/src/main.rs HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, "200 OK");
        assert_eq!(json["diff_available"], false);
        assert_eq!(json["diff"], "");
    }

    #[test]
    fn changes_request_decodes_segment_escaped_file_path() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let baseline_path = snapshot.path().join("baseline/src/file name.rs");
        let current_path = project.path().join("src/file name.rs");
        std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current_path.parent().unwrap()).unwrap();
        std::fs::write(&baseline_path, "before\n").unwrap();
        std::fs::write(&current_path, "after\n").unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/src/file%20name.rs HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "200 OK");
        assert_eq!(json["path"], "src/file name.rs");
        assert!(json["diff"].as_str().unwrap().contains("-before"));
        assert!(json["diff"].as_str().unwrap().contains("+after"));
    }

    #[test]
    fn changes_request_rejects_decoded_path_traversal() {
        let snapshot = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snapshot.path().join("baseline")).unwrap();

        let (status, body) = handle_changes_request(
            "GET /api/session/current/changes/%2E%2E/Cargo.toml HTTP/1.1",
            Some(snapshot.path()),
            Some(project.path()),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, "400 Bad Request");
        assert_eq!(json["error"], "invalid path");
    }

    #[test]
    fn agent_output_chunks_falls_back_to_other_logs_by_output_id() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        let primary_dir = logs_dir.join("primary");
        let fallback_dir = logs_dir.join("fallback");

        let mut primary = crate::session_log::SessionLog::open(primary_dir.clone()).unwrap();
        primary.agent_output_with_id("primary output", "", Some("Codex"), Some("primary-out"));
        drop(primary);

        let mut fallback = crate::session_log::SessionLog::open(fallback_dir.clone()).unwrap();
        fallback.agent_output_with_id("fallback output", "", Some("Codex"), Some("fallback-out"));
        drop(fallback);

        let chunks = agent_output_chunks_with_fallback(
            &primary_dir,
            &["fallback-out".to_string(), "primary-out".to_string()],
            Some(&logs_dir),
        );

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].output_id, "fallback-out");
        assert_eq!(chunks[0].stdout, "fallback output");
        assert_eq!(chunks[1].output_id, "primary-out");
        assert_eq!(chunks[1].stdout, "primary output");
    }

    #[test]
    fn agent_output_post_response_reads_json_ids() {
        // The injected temp home scopes the missing-id fallback sweep: an
        // empty `<home>/.intendant/logs` instead of the machine's real
        // store.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.agent_output_with_id("first output", "", Some("Codex"), Some("out-1"));
        drop(log);

        let response = test_render_api_response(current_agent_output_api_response(
            dir.path(),
            r#"{"ids":["out-1","missing-out"]}"#,
            &log_dir,
        ));
        assert!(response.starts_with("HTTP/1.1 200 OK"));

        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["outputs"][0]["output_id"], "out-1");
        assert_eq!(json["outputs"][0]["stdout"], "first output");
        assert_eq!(json["missing"][0], "missing-out");
    }

    #[test]
    fn agent_output_post_response_rejects_empty_json_ids() {
        let dir = tempfile::tempdir().unwrap();
        let response = test_render_api_response(current_agent_output_api_response(
            dir.path(),
            r#"{"ids":[""]}"#,
            dir.path(),
        ));
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("missing output ids"));
    }

    #[test]
    fn agent_output_json_body_accepts_large_id_lists() {
        let ids: Vec<String> = (0..700).map(|n| format!("ao-19e4f985a17-{n:x}")).collect();
        let body = serde_json::json!({ "ids": ids }).to_string();

        let parsed = agent_output_ids_from_json_body(&body).unwrap();

        assert_eq!(parsed.len(), 700);
        assert_eq!(parsed[0], "ao-19e4f985a17-0");
        assert_eq!(parsed[699], "ao-19e4f985a17-2bb");
    }

    // ── Golden HTTP transcripts: the sessions read-core wire contract ──
    //
    // Byte-exact pins of the session list / search / detail /
    // agent-output / context-snapshot HTTP responses, captured before the
    // transport-neutral conversion (transport-unification design §6 S4,
    // risk R1) and kept as the conversion's proof. The expected framing
    // is hand-written below — never built through the response helpers
    // under conversion. Store-dependent bodies (detail success,
    // agent-output success) come from the store-layer fns the conversion
    // does not touch, over fixtures written into an injected tempdir
    // home's `.intendant/logs` store — the handlers' `_from_home`
    // variants take the same home, so no golden ever reads or writes the
    // machine's real store (tests-are-hermetic convention; the public
    // handler wrappers resolve the real home at the transport edge).

    /// Run one stream-consuming handler and collect every byte it wrote.
    async fn collect_session_handler_response<Fut>(
        run: impl FnOnce(DemuxStream) -> Fut,
    ) -> Vec<u8>
    where
        Fut: std::future::Future<Output = ()>,
    {
        use tokio::io::AsyncReadExt;
        let (mut client, server) = tokio::io::duplex(1 << 20);
        run(Box::pin(server)).await;
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("collect handler response");
        response
    }

    /// The canonical JSON framing (`Cache-Control` + `Connection` tail):
    /// detail and context-snapshot responses, spelled out literally.
    fn golden_session_json_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The wildcard-CORS JSON framing (`Cache-Control` +
    /// `Access-Control-Allow-Origin: *` + `Connection` tail): the session
    /// list, search, and agent-output shapes, spelled out literally.
    fn golden_session_wildcard_json_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn golden_transcript(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// A fixture session's log dir under an injected tempdir home
    /// (`<home>/.intendant/logs/<session_id>`). Fixed ids are fine: each
    /// test owns its whole temp store.
    fn golden_home_log_dir(home: &tempfile::TempDir, session_id: &str) -> std::path::PathBuf {
        crate::platform::intendant_home_in(home.path())
            .join("logs")
            .join(session_id)
    }

    #[tokio::test]
    async fn golden_sessions_list_empty_ids_filter_transcript() {
        // A present-but-empty ids filter answers the empty list without
        // touching the session stores — fully deterministic.
        let request_line = "GET /api/sessions?ids= HTTP/1.1";
        let response =
            collect_session_handler_response(|stream| {
            handle_sessions_list(
                stream,
                request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
                .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript("200 OK", "[]")
        );
    }

    #[tokio::test]
    async fn golden_sessions_list_usage_view_and_limit_transcript() {
        // The limit and view=usage knobs ride the same empty-filter body:
        // pins the query-parameter plumbing end to end.
        let request_line = "GET /api/sessions?ids=&limit=3&view=usage HTTP/1.1";
        let response =
            collect_session_handler_response(|stream| {
            handle_sessions_list(
                stream,
                request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
                .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript("200 OK", "[]")
        );
    }

    #[tokio::test]
    async fn golden_sessions_search_no_input_transcript() {
        let _guard = SESSIONS_SEARCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // An empty query short-circuits before any store scan.
        let request_line = "GET /api/sessions/search?q= HTTP/1.1";
        let response =
            collect_session_handler_response(|stream| {
            handle_sessions_search(
                stream,
                request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
                .await;
        let body = serde_json::json!({
            "query": "",
            "mode": "all_keywords",
            "source_filter": "all",
            "searched": 0,
            "truncated": false,
            "exhaustive": true,
            "truncated_files": 0,
            "results": [],
        })
        .to_string();
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_sessions_search_busy_transcript() {
        let _guard = SESSIONS_SEARCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The single-flight guard answers 200 with the busy body.
        assert!(!SESSION_SEARCH_IN_FLIGHT.swap(true, Ordering::SeqCst));
        let request_line = "GET /api/sessions/search?q=anything HTTP/1.1";
        let response =
            collect_session_handler_response(|stream| {
            handle_sessions_search(
                stream,
                request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
                .await;
        SESSION_SEARCH_IN_FLIGHT.store(false, Ordering::SeqCst);
        let body = serde_json::json!({
            "error": "Another deep session search is already running. Wait for it to finish before starting a new one.",
            "busy": true,
        })
        .to_string();
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_session_detail_invalid_id_transcript() {
        // `..` fails the bare-id policy (session_lookup_id_is_safe).
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/.. HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("400 Bad Request", r#"{"error":"invalid session id"}"#)
        );
    }

    #[tokio::test]
    async fn golden_session_detail_missing_transcript() {
        // The empty temp home makes the miss deterministic.
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/golden-detail-missing HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("404 Not Found", r#"{"error":"session not found"}"#)
        );
    }

    #[tokio::test]
    async fn golden_session_detail_success_transcript() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "golden-detail";
        let log_dir = golden_home_log_dir(&home, session_id);
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.agent_output_with_id("golden detail stdout", "", Some("Codex"), Some("gd-out-1"));
        drop(log);

        let request_line = format!("GET /api/session/{session_id}?limit=5 HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        // Body from the store layer (untouched by the conversion); the
        // framing around it is the golden contract.
        let body = get_session_detail_from_home_with_page(home.path(), session_id, Some(5), None);
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_session_agent_output_missing_ids_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line = "POST /api/session/abc123/agent-output HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_agent_output_from_home(
                stream,
                "{}".to_string(),
                request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
                home.path(),
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "400 Bad Request",
                r#"{"error":"missing output ids"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_session_agent_output_missing_session_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line = "POST /api/session/golden-output-missing/agent-output HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_agent_output_from_home(
                stream,
                r#"{"ids":["out-1"]}"#.to_string(),
                request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
                home.path(),
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "404 Not Found",
                r#"{"error":"session not found"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_session_agent_output_success_transcript() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "golden-output";
        let log_dir = golden_home_log_dir(&home, session_id);
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.agent_output_with_id("golden stdout", "", Some("Codex"), Some("go-out-1"));
        drop(log);

        let request_line = format!("POST /api/session/{session_id}/agent-output HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_agent_output_from_home(
                stream,
                r#"{"ids":["go-out-1","go-missing"]}"#.to_string(),
                &request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
                home.path(),
            )
        })
        .await;
        // Body from the chunk store layer (untouched by the conversion).
        let chunks = agent_output_chunks_with_fallback(
            &log_dir,
            &["go-out-1".to_string(), "go-missing".to_string()],
            None,
        );
        let body = serde_json::json!({
            "outputs": chunks,
            "missing": ["go-missing"],
        })
        .to_string();
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_session_context_snapshot_missing_selector_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/abc123/context-snapshot HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript(
                "400 Bad Request",
                r#"{"error":"missing snapshot selector"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_session_context_snapshot_invalid_index_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/abc123/context-snapshot?request_index=abc HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript(
                "400 Bad Request",
                r#"{"error":"invalid request_index"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_session_context_snapshot_invalid_id_transcript() {
        // `..` fails the bare-id policy (session_lookup_id_is_safe).
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/../context-snapshot?file=snapshot.json HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("400 Bad Request", r#"{"error":"invalid session id"}"#)
        );
    }

    #[tokio::test]
    async fn golden_session_context_snapshot_double_error_precedence_transcript() {
        // Invalid id AND invalid request_index: the bare-id check answers
        // first on the HTTP lane (historical precedence, kept through the
        // S4a conversion; the tunnel's decode keeps index-error-first).
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/../context-snapshot?request_index=abc HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("400 Bad Request", r#"{"error":"invalid session id"}"#)
        );
    }

    #[tokio::test]
    async fn golden_session_context_snapshot_not_found_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line =
            "GET /api/session/golden-snapshot-missing/context-snapshot?file=snapshot.json HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript(
                "404 Not Found",
                r#"{"error":"context snapshot not found"}"#
            )
        );
    }

    // ── S4b golden transcripts: session artifacts / deletes / worktrees ──
    //
    // Same discipline as the S4a set above: byte-exact pins of the HTTP
    // wire bytes captured before the transport-neutral conversion
    // (design §6 S4, risk R1). The five session-delete wire shapes have
    // routing pins in gateway_routes; these pin the response bytes.

    /// The session-delete json framing — its wildcard tail historically
    /// orders `Access-Control-Allow-Origin` BEFORE `Cache-Control`,
    /// unlike the list/search/upload-error tail.
    fn golden_delete_json_transcript(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The text/plain framing (report/asset error bodies), spelled out.
    fn golden_text_plain_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The immutable-asset framing (recording segments, frame images).
    fn golden_public_asset_transcript(content_type: &str, bytes: &[u8]) -> Vec<u8> {
        let mut expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: public, max-age=3600\r\nConnection: close\r\n\r\n",
            bytes.len()
        )
        .into_bytes();
        expected.extend_from_slice(bytes);
        expected
    }

    #[tokio::test]
    async fn golden_session_delete_five_wire_shapes_transcripts() {
        // All five accepted shapes answer 200 with the wildcard-CORS tail;
        // `..` fails the bare-id policy so the body is deterministic.
        let home = tempfile::tempdir().unwrap();
        for request_line in [
            "DELETE /api/session/.. HTTP/1.1",
            "DELETE /api/session/../recordings HTTP/1.1",
            "DELETE /api/session/../recordings/delete HTTP/1.1",
            "POST /api/session/../delete HTTP/1.1",
            "POST /api/session/../recordings/delete HTTP/1.1",
        ] {
            let response = collect_session_handler_response(|stream| {
                handle_session_delete_from_home(
                    stream,
                    request_line,
                    crate::gateway_routes::CorsPosture::OwnOrigin,
                    None,
                    home.path(),
                )
            })
            .await;
            assert_eq!(
                golden_transcript(&response),
                golden_delete_json_transcript(r#"{"error":"invalid session id","ok":false}"#),
                "{request_line}"
            );
        }
    }

    #[tokio::test]
    async fn golden_session_delete_missing_session_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line = "DELETE /api/session/golden-delete-missing HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_delete_from_home(
                stream,
                request_line,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
                home.path(),
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_delete_json_transcript(r#"{"error":"session not found","ok":false}"#)
        );
    }

    #[tokio::test]
    async fn golden_session_report_invalid_id_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/../report HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "400 Bad Request",
                r#"{"error":"invalid session id"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_session_report_missing_transcript() {
        let home = tempfile::tempdir().unwrap();
        let request_line = "GET /api/session/golden-report-missing/report HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_text_plain_transcript("404 Not Found", "Session not found")
        );
    }

    #[tokio::test]
    async fn golden_session_report_success_transcript() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "golden-report";
        let log_dir = golden_home_log_dir(&home, session_id);
        std::fs::create_dir_all(log_dir.join("turns")).unwrap();
        std::fs::write(log_dir.join("summary.json"), "{\"ok\":true}\n").unwrap();
        std::fs::write(log_dir.join("turns").join("turn_001_stdout.txt"), "hi\n").unwrap();

        let request_line = format!("GET /api/session/{session_id}/report HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        // Zip bytes from the store layer (same-run mtimes make the two
        // builds byte-identical); the framing around them is the pin.
        let bytes = build_session_report_zip(&log_dir).unwrap();
        let mut expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nContent-Disposition: attachment; filename=\"intendant-session-{session_id}.zip\"\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
            bytes.len()
        )
        .into_bytes();
        expected.extend_from_slice(&bytes);
        assert_eq!(golden_transcript(&response), golden_transcript(&expected));
    }

    /// Temp-home recordings fixture: one stream with one playable
    /// segment (csv row + on-disk file) under the injected home's store.
    fn golden_recordings_fixture(
        home: &tempfile::TempDir,
        session_id: &str,
    ) -> (std::path::PathBuf, Vec<u8>) {
        let log_dir = golden_home_log_dir(home, session_id);
        let stream_dir = log_dir.join("recordings").join("screen");
        std::fs::create_dir_all(&stream_dir).unwrap();
        let seg_bytes = b"golden fake mp4 segment bytes".to_vec();
        std::fs::write(stream_dir.join("seg_00001.mp4"), &seg_bytes).unwrap();
        std::fs::write(stream_dir.join("segments.csv"), "seg_00001.mp4,0.0,2.0\n").unwrap();
        (log_dir, seg_bytes)
    }

    #[tokio::test]
    async fn golden_session_recordings_list_transcripts() {
        // Invalid id: the branch precheck answers under the wildcard tail.
        let home = tempfile::tempdir().unwrap();
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(
                stream,
                "GET /api/session/../recordings HTTP/1.1",
                None,
                None,
                home.path(),
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "400 Bad Request",
                r#"{"error":"invalid session id"}"#
            )
        );

        // Missing session: empty list under the canonical tail.
        let request_line = "GET /api/session/golden-recordings-missing/recordings HTTP/1.1";
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("200 OK", "[]")
        );

        // Fixture stream: body from the store layer.
        let session_id = "golden-recordings";
        let (_log_dir, _seg) = golden_recordings_fixture(&home, session_id);
        let request_line = format!("GET /api/session/{session_id}/recordings HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        let (status, body) = session_recordings_list_response_body(home.path(), session_id);
        assert_eq!(status, "200 OK");
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_recording_segments_and_playlist_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "golden-rec-assets";
        let (_log_dir, _seg) = golden_recordings_fixture(&home, session_id);

        // Segments listing: json array under the canonical tail.
        let request_line =
            format!("GET /api/session/{session_id}/recordings/screen/segments HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        let body = r#"[{"end_secs":2.0,"filename":"seg_00001.mp4","start_secs":0.0}]"#;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("200 OK", body)
        );

        // Playlist: HLS body under the mpegurl content type.
        let request_line =
            format!("GET /api/session/{session_id}/recordings/screen/playlist.m3u8 HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        let m3u8 = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.000,\nseg_00001.mp4\n#EXT-X-ENDLIST\n";
        let expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{m3u8}",
            m3u8.len()
        );
        assert_eq!(golden_transcript(&response), expected);
    }

    #[tokio::test]
    async fn golden_recording_segment_file_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "golden-seg-file";
        let (_log_dir, seg_bytes) = golden_recordings_fixture(&home, session_id);

        // Success: video content type under the immutable-asset tail.
        let request_line =
            format!("GET /api/session/{session_id}/recordings/screen/seg_00001.mp4 HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_transcript(&golden_public_asset_transcript("video/mp4", &seg_bytes))
        );

        // Invalid filename / missing segment: text/plain errors.
        let request_line =
            format!("GET /api/session/{session_id}/recordings/screen/evil.txt HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_text_plain_transcript("400 Bad Request", "Invalid filename")
        );

        let request_line =
            format!("GET /api/session/{session_id}/recordings/screen/seg_09999.mp4 HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_text_plain_transcript("404 Not Found", "Segment not found")
        );
    }

    #[tokio::test]
    async fn golden_session_frame_asset_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "golden-frame";
        let log_dir = golden_home_log_dir(&home, session_id);
        std::fs::create_dir_all(log_dir.join("frames")).unwrap();
        let frame_bytes = b"golden fake jpeg bytes".to_vec();
        std::fs::write(log_dir.join("frames").join("frame_0001.jpg"), &frame_bytes).unwrap();

        let request_line = format!("GET /api/session/{session_id}/frames/frame_0001.jpg HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_transcript(&golden_public_asset_transcript("image/jpeg", &frame_bytes))
        );

        let request_line = format!("GET /api/session/{session_id}/frames/evil.exe HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_text_plain_transcript("400 Bad Request", "Invalid filename")
        );

        let request_line = format!("GET /api/session/{session_id}/frames/frame_9.jpg HTTP/1.1");
        let response = collect_session_handler_response(|stream| {
            handle_session_sub_router_from_home(stream, &request_line, None, None, home.path())
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_text_plain_transcript("404 Not Found", "Frame not found")
        );
    }

    #[tokio::test]
    async fn golden_worktrees_transcripts() {
        // List with a cold cache: the empty inventory scan body.
        let cache: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let response = collect_session_handler_response(|stream| {
            handle_worktrees_list(
                stream,
                cache.clone(),
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("200 OK", &empty_worktree_inventory_response())
        );

        // List with a warm cache: served verbatim.
        {
            let mut guard = cache.lock().unwrap();
            *guard = Some(r#"{"worktrees":[],"cached":true}"#.to_string());
        }
        let response = collect_session_handler_response(|stream| {
            handle_worktrees_list(
                stream,
                cache.clone(),
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("200 OK", r#"{"worktrees":[],"cached":true}"#)
        );

        // Inspect / remove with invalid bodies: serde error wordings.
        let response = collect_session_handler_response(|stream| {
            handle_worktrees_inspect(
                stream,
                "not json".to_string(),
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        let body = serde_json::json!({
            "ok": false,
            "error": format!(
                "invalid worktree inspect request: {}",
                serde_json::from_str::<crate::worktree_inventory::WorktreeInspectRequest>("not json")
                    .unwrap_err()
            ),
        })
        .to_string();
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("400 Bad Request", &body)
        );

        let cache2: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let response = collect_session_handler_response(|stream| {
            handle_worktrees_remove(
                stream,
                "not json".to_string(),
                cache2,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        let body = serde_json::json!({
            "ok": false,
            "error": format!(
                "invalid worktree removal request: {}",
                serde_json::from_str::<crate::worktree_inventory::WorktreeRemoveRequest>("not json")
                    .unwrap_err()
            ),
        })
        .to_string();
        assert_eq!(
            golden_transcript(&response),
            golden_session_json_transcript("400 Bad Request", &body)
        );
    }
    // ── S4c golden transcripts: current-session + managed-context ──
    // Same discipline as the S4a/S4b sets: byte-exact pins captured
    // before the transport-neutral conversion (design §6 S4, risk R1).

    #[tokio::test]
    async fn golden_current_history_and_mutations_without_watcher_transcripts() {
        for (make, expect_status) in [
            ("GET", "503 Service Unavailable"),
        ] {
            let _ = make;
            let response = collect_session_handler_response(|stream| {
                handle_current_history(
                    stream,
                    None,
                    crate::gateway_routes::CorsPosture::OwnOrigin,
                    None,
                )
            })
            .await;
            assert_eq!(
                golden_transcript(&response),
                golden_session_wildcard_json_transcript(
                    expect_status,
                    r#"{"error":"file watcher not active"}"#
                )
            );
        }
        let response = collect_session_handler_response(|stream| {
            handle_current_redo(
                stream,
                None,
                None,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "503 Service Unavailable",
                r#"{"error":"file watcher not active"}"#
            )
        );
        let response = collect_session_handler_response(|stream| {
            handle_current_prune(
                stream,
                None,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "503 Service Unavailable",
                r#"{"error":"file watcher not active"}"#
            )
        );
        let bus = crate::event::EventBus::new();
        let response = collect_session_handler_response(|stream| {
            handle_current_rollback(
                stream,
                "{}".to_string(),
                bus,
                None,
                None,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "503 Service Unavailable",
                r#"{"error":"file watcher not active"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_current_changes_without_context_transcript() {
        let response = collect_session_handler_response(|stream| {
            handle_session_current_changes(
                stream,
                "GET /api/session/current/changes HTTP/1.1",
                None,
                None,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "503 Service Unavailable",
                r#"{"error":"file watcher not active"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_current_agent_output_without_log_transcript() {
        let response = collect_session_handler_response(|stream| {
            handle_current_agent_output(
                stream,
                r#"{"ids":["x"]}"#.to_string(),
                None,
                None,
                crate::gateway_routes::CorsPosture::OwnOrigin,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_transcript(&response),
            golden_session_wildcard_json_transcript(
                "404 Not Found",
                r#"{"error":"no active session log"}"#
            )
        );
    }

    #[tokio::test]
    async fn golden_managed_context_empty_home_transcripts() {
        // A tempdir home: the candidate scan finds nothing — the empty
        // bodies and framing are the pin (query names one session id so
        // the scan stays home-scoped).
        let home = tempfile::tempdir().unwrap();
        let anchors = test_render_api_response(managed_context_anchors_response_from_home(
            "GET /api/managed-context/anchors?session_id=abc123 HTTP/1.1",
            None,
            home.path(),
        ));
        assert_eq!(
            anchors,
            golden_session_json_transcript("200 OK", r#"{"anchors":[]}"#)
        );
        let records = test_render_api_response(managed_context_records_response_from_home(
            "GET /api/managed-context/records?session_id=abc123 HTTP/1.1",
            None,
            home.path(),
        ));
        assert_eq!(
            records,
            golden_session_json_transcript("200 OK", r#"{"records":[]}"#)
        );
        let fission = test_render_api_response(managed_context_fission_response_from_home(
            "GET /api/managed-context/fission?session_id=abc123 HTTP/1.1",
            None,
            home.path(),
        ));
        assert_eq!(
            fission,
            golden_session_json_transcript("200 OK", r#"{"groups":[]}"#)
        );
    }

}
