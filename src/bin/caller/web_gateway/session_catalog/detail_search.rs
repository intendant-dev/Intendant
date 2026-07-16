//! Session detail responses, context-snapshot selectors, and the session
//! log search (terms, candidates, snippets, filters).

use super::*;

pub(crate) fn session_detail_http_status(body: &str) -> &'static str {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return "200 OK";
    };
    if value.get("error").and_then(|v| v.as_str()) == Some("session not found") {
        "404 Not Found"
    } else {
        "200 OK"
    }
}

#[allow(dead_code)]
pub(crate) fn get_session_detail_from_home(home: &Path, session_id: &str) -> String {
    get_session_detail_from_home_with_limit(home, session_id, None)
}

pub(crate) fn session_detail_response_body_with_page(
    home: &Path,
    session_id: &str,
    source: &str,
    limit: Option<usize>,
    before: Option<usize>,
) -> String {
    let session_id = session_id.trim();
    if !session_lookup_id_is_safe(session_id) {
        return serde_json::json!({"error": "invalid session id"}).to_string();
    }
    let source = source.trim();
    let source = if source.is_empty() {
        "intendant"
    } else {
        source
    };
    if source == "intendant" {
        get_session_detail_from_home_with_page(home, session_id, limit, before)
    } else {
        external_session_detail_from_home_with_page(home, source, session_id, limit, before)
            .unwrap_or_else(|| serde_json::json!({"error": "session not found"}).to_string())
    }
}

#[allow(dead_code)]
pub(crate) fn get_session_detail_from_home_with_limit(
    home: &Path,
    session_id: &str,
    limit: Option<usize>,
) -> String {
    get_session_detail_from_home_with_page(home, session_id, limit, None)
}

pub(crate) fn get_session_detail_from_home_with_page(
    home: &Path,
    session_id: &str,
    limit: Option<usize>,
    before: Option<usize>,
) -> String {
    let session_dir = match resolve_bare_session_dir_from_home(home, session_id) {
        Some(d) => d,
        None => return serde_json::json!({"error": "session not found"}).to_string(),
    };

    // Cached full conversion (fingerprint-keyed, already compacted):
    // paging back through a session detail re-used to re-convert the
    // whole log per page; now page N is a slice of the cached entries.
    let entries = cached_session_log_replay_entries(&session_dir)
        .map(|(entries, _)| entries)
        .unwrap_or_default();
    // Entries come out of the cache already compacted (replay_cache
    // compacts before admission), so no per-request compaction pass.
    let page = session_detail_page_entries_ref(&entries, limit, before);
    native_session_detail_body(&session_dir, page, None)
}

/// Shared assembly of the native session-detail body (the historical
/// field set) plus the optional additive `locate` object the anchored
/// read (`locate.rs`) attaches.
pub(crate) fn native_session_detail_body(
    session_dir: &Path,
    page: SessionDetailPageEntries,
    locate: Option<serde_json::Value>,
) -> String {
    // Check for screenshot frames
    let frames_dir = session_dir.join("frames");
    let mut frames: Vec<String> = Vec::new();
    if frames_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&frames_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".png") || name.ends_with(".jpg") {
                    frames.push(name);
                }
            }
        }
        frames.sort();
    }

    let mut body = serde_json::json!({
        "session_id": session_dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "entries": page.entries,
        "total_entries": page.total_entries,
        "page_start": page.page_start,
        "page_end": page.page_end,
        "has_older": page.page_start > 0,
        "frames": frames,
        "relationships": session_relationships_from_log_dir(session_dir),
    });
    if let Some(locate) = locate {
        body["locate"] = locate;
    }
    body.to_string()
}

#[derive(Default)]
pub(crate) struct ContextSnapshotSelector {
    file: Option<String>,
    request_id: Option<String>,
    request_index: Option<u64>,
    ts: Option<String>,
}

impl ContextSnapshotSelector {
    pub(crate) fn is_empty(&self) -> bool {
        self.file.is_none()
            && self.request_id.is_none()
            && self.request_index.is_none()
            && self.ts.is_none()
    }
}

pub(crate) fn context_snapshot_file_selector_is_safe(file: &str) -> bool {
    if file.is_empty() || file.len() > 512 || file.contains('\\') {
        return false;
    }
    let path = Path::new(file);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// Query-string decode of the snapshot selector parts — the HTTP lane's
/// transport-owned decode (the tunnel decodes the same parts from frame
/// params). Selector validation lives with the shared
/// [`session_context_snapshot_response_body`] core; only the u64 parse
/// can fail here, keeping its historical wording.
pub(crate) type ContextSnapshotSelectorParts =
    (Option<String>, Option<String>, Option<u64>, Option<String>);

pub(crate) fn context_snapshot_selector_parts_from_request(
    request_line: &str,
) -> Result<ContextSnapshotSelectorParts, String> {
    let request_index =
        match query_param(request_line, "request_index").filter(|value| !value.trim().is_empty()) {
            Some(value) => Some(
                value
                    .parse::<u64>()
                    .map_err(|_| "invalid request_index".to_string())?,
            ),
            None => None,
        };
    Ok((
        query_param(request_line, "file").filter(|value| !value.trim().is_empty()),
        query_param(request_line, "request_id").filter(|value| !value.trim().is_empty()),
        request_index,
        query_param(request_line, "ts").filter(|value| !value.trim().is_empty()),
    ))
}

pub(crate) fn context_snapshot_selector_from_parts(
    file: Option<String>,
    request_id: Option<String>,
    request_index: Option<u64>,
    ts: Option<String>,
) -> Result<ContextSnapshotSelector, String> {
    let file = file.filter(|value| !value.trim().is_empty());
    if file
        .as_deref()
        .is_some_and(|file| !context_snapshot_file_selector_is_safe(file))
    {
        return Err("invalid snapshot file".to_string());
    }
    let selector = ContextSnapshotSelector {
        file,
        request_id: request_id.filter(|value| !value.trim().is_empty()),
        request_index,
        ts: ts.filter(|value| !value.trim().is_empty()),
    };
    if selector.is_empty() {
        return Err("missing snapshot selector".to_string());
    }
    Ok(selector)
}

pub(crate) fn context_snapshot_candidate_log_dirs(
    home: &Path,
    session_id: &str,
    source: &str,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    let push = |dirs: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf| {
        let key = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .to_string();
        if seen.insert(key) {
            dirs.push(path);
        }
    };

    if let Some(dir) = resolve_bare_session_dir_from_home(home, session_id) {
        push(&mut dirs, &mut seen, dir);
    }
    let source = crate::session_names::normalize_source(source);
    if source != "intendant" {
        for record in crate::external_wrapper_index::wrappers_for(home, &source, session_id) {
            push(&mut dirs, &mut seen, PathBuf::from(record.log_path));
        }
        for dir in cached_intendant_log_dirs_for_session_id(session_id) {
            push(&mut dirs, &mut seen, dir);
        }
        if dirs.is_empty() {
            for dir in recent_intendant_log_dirs(home, EXTERNAL_CONTEXT_REPLAY_LOG_SCAN_LIMIT) {
                if managed_context_log_dir_mentions_session(&dir, session_id) {
                    push(&mut dirs, &mut seen, dir);
                }
            }
        }
    } else if dirs.is_empty() {
        for dir in managed_context_candidate_log_dirs(home, None, Some(session_id), None) {
            push(&mut dirs, &mut seen, dir);
        }
    }
    dirs
}

pub(crate) fn context_snapshot_log_entry_matches_selector(
    entry: &serde_json::Value,
    selector: &ContextSnapshotSelector,
) -> bool {
    if entry.get("event").and_then(|v| v.as_str()) != Some("context_snapshot") {
        return false;
    }
    if let Some(file) = selector.file.as_deref() {
        if entry.get("file").and_then(|v| v.as_str()) != Some(file) {
            return false;
        }
    }
    if let Some(request_id) = selector.request_id.as_deref() {
        if entry
            .get("data")
            .and_then(|data| data.get("request_id"))
            .and_then(|v| v.as_str())
            != Some(request_id)
        {
            return false;
        }
    }
    if let Some(request_index) = selector.request_index {
        if entry
            .get("data")
            .and_then(|data| data.get("request_index"))
            .and_then(|v| v.as_u64())
            != Some(request_index)
        {
            return false;
        }
    }
    if let Some(ts) = selector.ts.as_deref() {
        if entry.get("ts").and_then(|v| v.as_str()) != Some(ts) {
            return false;
        }
    }
    true
}

pub(crate) fn context_snapshot_log_entry_matches_session(
    entry: &serde_json::Value,
    log_dir: &Path,
    session_id: &str,
    source: &str,
) -> bool {
    let data_session = entry
        .get("data")
        .and_then(|data| data.get("session_id"))
        .and_then(|v| v.as_str());
    if data_session == Some(session_id) {
        return true;
    }
    let source = crate::session_names::normalize_source(source);
    if source == "intendant" {
        return data_session.is_none()
            || replay_session_id_from_dir(log_dir).as_deref() == Some(session_id);
    }
    data_session.is_none() && replay_session_id_from_dir(log_dir).as_deref() == Some(session_id)
}

pub(crate) fn exact_context_snapshot_from_log_entry(
    entry: &serde_json::Value,
    log_dir: &Path,
    contents: &str,
) -> Option<serde_json::Value> {
    // Disk truth first: sidecars rotate to latest-only, and the event
    // converter degrades a missing file to an empty `{}` raw — without
    // this gate a rotated-away row would serve 200-with-{} instead of
    // taking the caller's honest 404 arm.
    if context_snapshot_raw_file_size(entry, log_dir).is_none() {
        return None;
    }
    let app_event = crate::session_log::session_log_entry_to_app_event(entry, log_dir)?;
    let outbound = crate::event::app_event_to_outbound(&app_event)?;
    let mut value = serde_json::to_value(&outbound).ok()?;
    let external_replay_session_id = external_backend_session_id_from_replay(contents);
    let wrapper_replay_session_id = replay_session_id_from_dir(log_dir);
    let replay_session_id = external_replay_session_id
        .clone()
        .or_else(|| wrapper_replay_session_id.clone());
    inject_replay_entry_metadata(
        &mut value,
        entry,
        log_dir,
        replay_session_id.as_deref(),
        external_replay_session_id.as_deref(),
        wrapper_replay_session_id.as_deref(),
    );
    Some(value)
}

pub(crate) fn session_context_snapshot_response_body(
    home: &Path,
    session_id: &str,
    source: &str,
    file: Option<String>,
    request_id: Option<String>,
    request_index: Option<u64>,
    ts: Option<String>,
) -> (&'static str, String) {
    if !session_lookup_id_is_safe(session_id) {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "invalid session id"}).to_string(),
        );
    }
    let selector = match context_snapshot_selector_from_parts(file, request_id, request_index, ts) {
        Ok(selector) => selector,
        Err(error) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": error}).to_string(),
            );
        }
    };
    session_context_snapshot_response_for_selector(home, session_id, source, selector)
}

pub(crate) fn session_context_snapshot_response_for_selector(
    home: &Path,
    session_id: &str,
    source: &str,
    selector: ContextSnapshotSelector,
) -> (&'static str, String) {
    for log_dir in context_snapshot_candidate_log_dirs(home, session_id, source) {
        let Ok(contents) = std::fs::read_to_string(log_dir.join("session.jsonl")) else {
            continue;
        };
        for line in contents.lines() {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if !context_snapshot_log_entry_matches_selector(&entry, &selector)
                || !context_snapshot_log_entry_matches_session(&entry, &log_dir, session_id, source)
            {
                continue;
            }
            let Some(snapshot) = exact_context_snapshot_from_log_entry(&entry, &log_dir, &contents)
            else {
                continue;
            };
            return (
                "200 OK",
                serde_json::json!({
                    "ok": true,
                    "snapshot": snapshot,
                })
                .to_string(),
            );
        }
    }
    (
        "404 Not Found",
        serde_json::json!({"error": "context snapshot not found"}).to_string(),
    )
}

pub(crate) async fn sessions_search_response_body_with_cancel(
    query: String,
    source_filter: String,
    mode: String,
    project_filter: Vec<String>,
    cancel: tokio_util::sync::CancellationToken,
) -> String {
    if SESSION_SEARCH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return serde_json::json!({
            "error": "Another deep session search is already running. Wait for it to finish before starting a new one.",
            "busy": true,
        })
        .to_string();
    }
    let body = match tokio::task::spawn_blocking(move || {
        let home_path = crate::platform::home_dir();
        session_log_search_from_home_with_projects_cancel(
            &home_path,
            &query,
            &source_filter,
            &mode,
            &project_filter,
            &cancel,
        )
    })
    .await
    {
        Ok(body) => body,
        Err(e) => serde_json::json!({
            "error": format!("session search task failed: {e}")
        })
        .to_string(),
    };
    SESSION_SEARCH_IN_FLIGHT.store(false, Ordering::SeqCst);
    body
}

#[allow(dead_code)]
pub(crate) fn session_log_search_from_home(
    home: &Path,
    query: &str,
    source_filter: &str,
    mode: &str,
) -> String {
    session_log_search_from_home_with_projects(home, query, source_filter, mode, &[])
}

#[allow(dead_code)]
pub(crate) fn session_log_search_from_home_with_projects(
    home: &Path,
    query: &str,
    source_filter: &str,
    mode: &str,
    project_filter: &[String],
) -> String {
    session_log_search_from_home_with_projects_cancel(
        home,
        query,
        source_filter,
        mode,
        project_filter,
        &tokio_util::sync::CancellationToken::new(),
    )
}

pub(crate) fn session_log_search_from_home_with_projects_cancel(
    home: &Path,
    query: &str,
    source_filter: &str,
    mode: &str,
    project_filter: &[String],
    cancel: &tokio_util::sync::CancellationToken,
) -> String {
    session_log_search_from_home_with_progress(
        home,
        query,
        source_filter,
        mode,
        project_filter,
        cancel,
        &mut |_progress| {},
    )
}

/// Deep-search scan progress: how far through the candidate list the
/// scan is. `scanned` counts candidate sessions the loop has passed
/// (filtered-out ones included, so it reaches `total`), `matched` counts
/// result rows so far.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeepSearchProgress {
    pub(crate) scanned: usize,
    pub(crate) total: usize,
    pub(crate) matched: usize,
}

/// How often the scan reports progress, in candidate sessions.
pub(crate) const DEEP_SEARCH_PROGRESS_EVERY: usize = 250;

pub(crate) fn session_log_search_from_home_with_progress(
    home: &Path,
    query: &str,
    source_filter: &str,
    mode: &str,
    project_filter: &[String],
    cancel: &tokio_util::sync::CancellationToken,
    progress: &mut dyn FnMut(DeepSearchProgress),
) -> String {
    let mode = SessionLogSearchMode::from_query(mode);
    let terms = session_log_search_terms(query);
    if !mode.has_search_input(query, &terms) {
        return serde_json::json!({
            "query": query,
            "mode": mode.as_str(),
            "source_filter": normalize_session_source_filter(source_filter),
            "searched": 0,
            "truncated": false,
            "exhaustive": true,
            "truncated_files": 0,
            "results": [],
        })
        .to_string();
    }

    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&list_sessions_for_deep_search_from_home(home))
            .unwrap_or_else(|_| Vec::new());
    let deleted_external_sessions = read_deleted_external_sessions(home);
    let source_filter = normalize_session_source_filter(source_filter);
    let project_filter = normalize_session_project_filter(project_filter);
    let total = sessions.len();
    let mut results = Vec::new();
    let mut searched = 0usize;

    for (index, session) in sessions.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return serde_json::json!({
                "query": query,
                "mode": mode.as_str(),
                "source_filter": source_filter,
                "searched": searched,
                "truncated": false,
                "exhaustive": false,
                "cancelled": true,
                "truncated_files": 0,
                "results": results,
            })
            .to_string();
        }
        if index > 0 && index % DEEP_SEARCH_PROGRESS_EVERY == 0 {
            progress(DeepSearchProgress {
                scanned: index,
                total,
                matched: results.len(),
            });
        }
        let source = session
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("intendant");
        if !session_source_matches_filter(source, &source_filter) {
            continue;
        }
        if !session_project_matches_filter(&session, &project_filter) {
            continue;
        }

        let Some(session_id) = session.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };

        let session_path = session
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let Some(search_path) =
            session_log_search_file_path(home, source, session_id, session_path.as_deref())
        else {
            continue;
        };
        let Some((matches, snippets)) = search_session_log_file(
            &search_path,
            query,
            &terms,
            mode,
            &deleted_external_sessions,
        ) else {
            continue;
        };
        searched += 1;
        if matches == 0 {
            continue;
        }
        results.push(serde_json::json!({
            "key": format!("{source}:{session_id}"),
            "source": source,
            "session_id": session_id,
            "matches": matches,
            "snippets": snippets,
            "session": session,
        }));
    }

    serde_json::json!({
        "query": query,
        "mode": mode.as_str(),
        "source_filter": source_filter,
        "searched": searched,
        "truncated": false,
        "exhaustive": true,
        "truncated_files": 0,
        "results": results,
    })
    .to_string()
}

pub(crate) fn session_project_filter_from_request(request_line: &str) -> Vec<String> {
    let Some(raw) = query_param(request_line, "projects") else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(values) => values,
        Err(_) => vec![raw],
    }
}

pub(crate) fn normalize_session_project_filter(project_filter: &[String]) -> HashSet<String> {
    project_filter
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub(crate) fn session_project_directory_value(session: &serde_json::Value) -> Option<String> {
    for key in [
        "project_root",
        "projectRoot",
        "project_dir",
        "projectDir",
        "project",
        "cwd",
        "workdir",
        "workDir",
    ] {
        let Some(value) = value_str(session, key) else {
            continue;
        };
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

pub(crate) fn session_project_matches_filter(
    session: &serde_json::Value,
    project_filter: &HashSet<String>,
) -> bool {
    if project_filter.is_empty() {
        return true;
    }
    match session_project_directory_value(session) {
        Some(path) => project_filter.contains(&path),
        None => false,
    }
}

pub(crate) fn session_log_search_file_path(
    home: &Path,
    source: &str,
    session_id: &str,
    session_path: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = session_path {
        if source == "intendant" && path.is_dir() {
            return Some(path.join("session.jsonl"));
        }
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }

    match source {
        "intendant" => {
            Some(resolve_bare_session_dir_from_home(home, session_id)?.join("session.jsonl"))
        }
        "codex" => find_codex_session_file(home, session_id),
        "claude-code" => find_claude_session_file(home, session_id),
        "gemini" => find_gemini_session_file(home, session_id),
        _ => None,
    }
}

pub(crate) fn find_claude_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    // Exact-name lookup: the direct per-project probe (and walk
    // fallback) replaces a store-wide walk + mtime sort whose ordering
    // never mattered for finding one stem.
    find_claude_session_file_for_transcript(home, session_id)
}

pub(crate) fn find_gemini_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    // Exact-id lookup: the cached transcript resolver verifies one
    // remembered file per repeat fetch instead of read+parsing every
    // chat in the store under an mtime sort that never mattered here.
    find_gemini_session_file_for_transcript(home, session_id)
}

pub(crate) fn search_session_log_file(
    path: &Path,
    query: &str,
    terms: &[String],
    mode: SessionLogSearchMode,
    deleted_external_sessions: &HashSet<(String, String)>,
) -> Option<(usize, Vec<serde_json::Value>)> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let candidates = reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| session_log_search_candidate_from_line(&line));
    Some(search_session_log_candidates(
        candidates,
        query,
        terms,
        mode,
        deleted_external_sessions,
    ))
}

pub(crate) fn normalize_session_source_filter(source_filter: &str) -> String {
    let value = source_filter.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "all" => "all".to_string(),
        "external" => "external".to_string(),
        "intendant" | "codex" | "claude-code" | "gemini" => value,
        "claude" => "claude-code".to_string(),
        _ => "all".to_string(),
    }
}

pub(crate) fn session_source_matches_filter(source: &str, source_filter: &str) -> bool {
    match source_filter {
        "all" => true,
        "external" => source != "intendant",
        "claude" | "claude-code" => source == "claude-code",
        other => source == other,
    }
}

pub(crate) fn session_log_search_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|term| term.trim().to_ascii_lowercase())
        .filter(|term| !term.is_empty())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionLogSearchMode {
    AllKeywords,
    ExactPhrase,
    AnyKeywordSession,
    UserMessageAllKeywords,
}

impl SessionLogSearchMode {
    pub(crate) fn from_query(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "exact" | "exact_phrase" | "phrase" => Self::ExactPhrase,
            "any" | "any_keyword" | "any_keyword_session" => Self::AnyKeywordSession,
            "user" | "user_message" | "user_message_all_keywords" => Self::UserMessageAllKeywords,
            _ => Self::AllKeywords,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AllKeywords => "all_keywords",
            Self::ExactPhrase => "exact_phrase",
            Self::AnyKeywordSession => "any_keyword_session",
            Self::UserMessageAllKeywords => "user_message_all_keywords",
        }
    }

    pub(crate) fn has_search_input(self, query: &str, terms: &[String]) -> bool {
        match self {
            Self::ExactPhrase => !query.trim().is_empty(),
            _ => !terms.is_empty(),
        }
    }
}

pub(crate) fn search_session_log_candidates<I>(
    candidates: I,
    query: &str,
    terms: &[String],
    mode: SessionLogSearchMode,
    deleted_external_sessions: &HashSet<(String, String)>,
) -> (usize, Vec<serde_json::Value>)
where
    I: IntoIterator<Item = SessionLogSearchCandidate>,
{
    let mut matches = 0usize;
    let mut snippets = Vec::new();
    let snippet_needles = if mode == SessionLogSearchMode::ExactPhrase {
        vec![query.trim().to_ascii_lowercase()]
    } else {
        terms.to_vec()
    };

    for candidate in candidates {
        if session_log_candidate_is_deleted_external_reference(
            &candidate,
            deleted_external_sessions,
        ) {
            continue;
        }
        if candidate.text.trim().is_empty()
            || !session_log_candidate_matches(&candidate, query, terms, mode)
        {
            continue;
        }
        matches += 1;
        if snippets.len() < SESSION_LOG_SEARCH_SNIPPETS_PER_SESSION {
            snippets.push(serde_json::json!({
                "ts": candidate.ts,
                "source": candidate.source,
                "level": candidate.level,
                "event": candidate.event,
                "content": session_log_match_snippet(
                    &candidate.text,
                    &snippet_needles,
                    SESSION_LOG_SEARCH_SNIPPET_CHARS
                ),
            }));
        }
    }

    (matches, snippets)
}

pub(crate) fn session_log_candidate_is_deleted_external_reference(
    candidate: &SessionLogSearchCandidate,
    deleted_external_sessions: &HashSet<(String, String)>,
) -> bool {
    if deleted_external_sessions.is_empty() {
        return false;
    }

    if candidate.event == "presence_log"
        && candidate.text.contains("ControlMsg:")
        && candidate.text.contains("CreateSession")
    {
        return true;
    }

    deleted_external_sessions
        .iter()
        .any(|(_source, id)| !id.is_empty() && candidate.text.contains(id))
}

pub(crate) fn session_log_candidate_matches(
    candidate: &SessionLogSearchCandidate,
    query: &str,
    terms: &[String],
    mode: SessionLogSearchMode,
) -> bool {
    match mode {
        SessionLogSearchMode::AllKeywords => text_matches_session_terms(&candidate.text, terms),
        SessionLogSearchMode::ExactPhrase => text_contains_session_phrase(&candidate.text, query),
        SessionLogSearchMode::AnyKeywordSession => {
            text_matches_any_session_term(&candidate.text, terms)
        }
        SessionLogSearchMode::UserMessageAllKeywords => {
            candidate.is_user && text_matches_session_terms(&candidate.text, terms)
        }
    }
}

pub(crate) struct SessionLogSearchCandidate {
    ts: String,
    source: String,
    level: String,
    event: String,
    text: String,
    is_user: bool,
}

pub(crate) fn session_log_search_candidate_from_line(
    line: &str,
) -> Option<SessionLogSearchCandidate> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Some(SessionLogSearchCandidate {
            ts: String::new(),
            source: String::new(),
            level: String::new(),
            event: String::new(),
            text: trimmed.to_string(),
            is_user: false,
        });
    };

    let mut parts = Vec::new();
    collect_session_log_search_strings(&value, &mut parts);
    let text = if parts.is_empty() {
        trimmed.to_string()
    } else {
        parts.join("\n")
    };

    Some(SessionLogSearchCandidate {
        ts: value
            .get("ts")
            .or_else(|| value.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        source: value
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        level: value
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        event: value
            .get("event")
            .or_else(|| value.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        text,
        is_user: session_log_json_is_user_message(&value),
    })
}

pub(crate) fn session_log_json_is_user_message(value: &serde_json::Value) -> bool {
    [
        value.get("source"),
        value.get("role"),
        value.get("type"),
        value.pointer("/payload/source"),
        value.pointer("/payload/role"),
        value.pointer("/payload/type"),
        value.pointer("/message/role"),
        value.pointer("/message/type"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|v| v.as_str())
    .any(|value| matches!(value.to_ascii_lowercase().as_str(), "user" | "user_message"))
}

pub(crate) fn collect_session_log_search_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value) => {
            if value.trim().is_empty() {
                return;
            }
            out.push(value.to_string());
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_session_log_search_strings(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_session_log_search_strings(value, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn text_matches_session_terms(text: &str, terms: &[String]) -> bool {
    let haystack = text.to_ascii_lowercase();
    terms.iter().all(|term| haystack.contains(term))
}

pub(crate) fn text_matches_any_session_term(text: &str, terms: &[String]) -> bool {
    let haystack = text.to_ascii_lowercase();
    terms.iter().any(|term| haystack.contains(term))
}

pub(crate) fn text_contains_session_phrase(text: &str, phrase: &str) -> bool {
    let phrase = phrase.trim().to_ascii_lowercase();
    !phrase.is_empty() && text.to_ascii_lowercase().contains(&phrase)
}

pub(crate) fn session_log_match_snippet(text: &str, terms: &[String], max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let total_chars = compact.chars().count();
    if total_chars <= max_chars {
        return compact;
    }

    let lower = compact.to_ascii_lowercase();
    let match_byte = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let match_char = compact[..match_byte].chars().count();
    let start_char = match_char.saturating_sub(max_chars / 3);
    let end_char = (start_char + max_chars).min(total_chars);
    let mut snippet: String = compact
        .chars()
        .skip(start_char)
        .take(end_char - start_char)
        .collect();
    if start_char > 0 {
        snippet.insert_str(0, "...");
    }
    if end_char < total_chars {
        snippet.push_str("...");
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_log_search_finds_intendant_log_content_not_summary() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "intendant-search-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00",
                "task": "ordinary dashboard task",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-05-17T20:45:00",
                "event": "info",
                "message": "Detailed log contains alpha-search-token"
            })
            .to_string(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha-search-token",
            "all",
            "",
        ))
        .unwrap();
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("session_id").and_then(|v| v.as_str()),
            Some(session_id)
        );
        assert_eq!(
            results[0].get("source").and_then(|v| v.as_str()),
            Some("intendant")
        );
    }

    #[test]
    fn session_log_search_can_filter_external_agent_sessions() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let id = "019e37ae-search-filter";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:40Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "ordinary request"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:50Z",
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": "external-only beta-search-token"
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "beta-search-token",
            "external",
            "",
        ))
        .unwrap();
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("source").and_then(|v| v.as_str()),
            Some("codex")
        );

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "beta-search-token",
            "intendant",
            "",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_log_search_prefilters_by_project_directory() {
        let home = tempfile::tempdir().unwrap();
        for (session_id, project_root) in [
            ("project-search-target", "/repo/target"),
            ("project-search-other", "/repo/other"),
        ] {
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            std::fs::create_dir_all(&log_dir).unwrap();
            std::fs::write(
                log_dir.join("session_meta.json"),
                serde_json::json!({
                    "session_id": session_id,
                    "created_at": "2026-05-17T20:44:00",
                    "task": "project scoped task",
                    "status": "completed",
                    "project_root": project_root,
                    "cwd": project_root
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                log_dir.join("session.jsonl"),
                serde_json::json!({
                    "ts": "2026-05-17T20:45:00",
                    "event": "info",
                    "message": "shared-project-filter-token"
                })
                .to_string(),
            )
            .unwrap();
        }

        let project_filter = vec!["/repo/target".to_string()];
        let response: serde_json::Value =
            serde_json::from_str(&session_log_search_from_home_with_projects(
                home.path(),
                "shared-project-filter-token",
                "all",
                "",
                &project_filter,
            ))
            .unwrap();
        assert_eq!(response.get("searched").and_then(|v| v.as_u64()), Some(1));
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("session_id").and_then(|v| v.as_str()),
            Some("project-search-target")
        );

        let missing_filter = vec!["/repo/missing".to_string()];
        let response: serde_json::Value =
            serde_json::from_str(&session_log_search_from_home_with_projects(
                home.path(),
                "shared-project-filter-token",
                "all",
                "",
                &missing_filter,
            ))
            .unwrap();
        assert_eq!(response.get("searched").and_then(|v| v.as_u64()), Some(0));
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_log_search_scans_beyond_recent_session_window() {
        let home = tempfile::tempdir().unwrap();
        for idx in 0..160 {
            let session_id = format!("exhaustive-window-{idx:03}");
            let log_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(&session_id);
            std::fs::create_dir_all(&log_dir).unwrap();
            std::fs::write(
                log_dir.join("session_meta.json"),
                serde_json::json!({
                    "session_id": session_id,
                    "created_at": format!("2026-05-17T{:02}:{:02}:00Z", 20 + idx / 60, idx % 60),
                    "updated_at": format!("2026-05-17T{:02}:{:02}:00Z", 20 + idx / 60, idx % 60),
                    "task": "exhaustive deep search window",
                    "status": "completed"
                })
                .to_string(),
            )
            .unwrap();
            let message = if idx == 0 {
                "oldest-session-only exhaustive-window-token"
            } else {
                "ordinary session log"
            };
            std::fs::write(
                log_dir.join("session.jsonl"),
                serde_json::json!({
                    "ts": "2026-05-17T20:45:00",
                    "event": "info",
                    "message": message
                })
                .to_string(),
            )
            .unwrap();
        }

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "exhaustive-window-token",
            "all",
            "exact_phrase",
        ))
        .unwrap();
        assert_eq!(response.get("searched").and_then(|v| v.as_u64()), Some(160));
        assert_eq!(
            response.get("truncated").and_then(|v| v.as_bool()),
            Some(false)
        );
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("session_id").and_then(|v| v.as_str()),
            Some("exhaustive-window-000")
        );
    }

    #[test]
    fn session_log_search_scans_full_log_file_and_full_fields() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "exhaustive-full-file-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00Z",
                "task": "full file search task",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        let mut contents = String::new();
        for _ in 0..50_000 {
            contents.push_str("{\"event\":\"info\",\"message\":\"prefix filler line\"}\n");
        }
        let mut long_field = "x".repeat(10_000);
        long_field.push_str(" exhaustive-full-field-token");
        contents.push_str(
            &serde_json::json!({
                "ts": "2026-05-17T20:45:00",
                "event": "info",
                "message": long_field
            })
            .to_string(),
        );
        std::fs::write(log_dir.join("session.jsonl"), contents).unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "exhaustive-full-field-token",
            "all",
            "exact_phrase",
        ))
        .unwrap();
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("session_id").and_then(|v| v.as_str()),
            Some(session_id)
        );
    }

    #[test]
    fn session_log_search_supports_exact_phrase_mode() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "exact-phrase-search-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00",
                "task": "exact phrase task",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-05-17T20:45:00",
                "event": "info",
                "message": "Needle words appear as alpha phrase token"
            })
            .to_string(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha phrase",
            "all",
            "exact_phrase",
        ))
        .unwrap();
        assert_eq!(
            response
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha token",
            "all",
            "exact_phrase",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_log_search_supports_any_keyword_session_mode() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "any-keyword-search-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00",
                "task": "any keyword task",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-05-17T20:45:00",
                "event": "info",
                "message": "This line contains only one-side-token"
            })
            .to_string(),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "one-side-token absent-token",
            "all",
            "all_keywords",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "one-side-token absent-token",
            "all",
            "any_keyword_session",
        ))
        .unwrap();
        assert_eq!(
            response
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn session_log_search_supports_user_message_mode() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let id = "019e37ae-user-message-search";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:40Z",
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": "user-only alpha-token beta-token"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:50Z",
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": "assistant-only gamma-token delta-token"
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "alpha-token beta-token",
            "codex",
            "user_message_all_keywords",
        ))
        .unwrap();
        assert_eq!(
            response
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            "gamma-token delta-token",
            "codex",
            "user_message_all_keywords",
        ))
        .unwrap();
        assert!(response
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_detail_exposes_persisted_relationships() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("parent");
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.session_relationship("parent", "child", "subagent", false);
        drop(log);

        let detail: serde_json::Value =
            serde_json::from_str(&get_session_detail_from_home(dir.path(), "parent")).unwrap();
        let relationships = detail["relationships"].as_array().unwrap();

        assert_eq!(relationships.len(), 1);
        assert_eq!(relationships[0]["parent_session_id"], "parent");
        assert_eq!(relationships[0]["child_session_id"], "child");
        assert_eq!(relationships[0]["relationship"], "subagent");
        assert_eq!(relationships[0]["ephemeral"], false);
    }

    #[test]
    fn session_detail_http_status_marks_missing_sessions_not_found() {
        let missing = serde_json::json!({"error": "session not found"}).to_string();
        assert_eq!(session_detail_http_status(&missing), "404 Not Found");
        assert_eq!(
            session_detail_http_status(&serde_json::json!({"entries": []}).to_string()),
            "200 OK"
        );
    }

    #[test]
    fn session_detail_compacts_context_snapshot_raw() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("detail-session");
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.context_snapshot(
            "codex",
            "Codex resolved request payload",
            Some(1),
            "openai.responses.resolved_request.v1",
            Some(1_000),
            Some("backend_reported"),
            Some(128_000),
            Some(272_000),
            Some(1),
            &serde_json::json!({
                "instructions": "y".repeat(8_000),
                "input": [{"role": "user", "content": "open session detail"}]
            }),
        );
        drop(log);

        let detail = get_session_detail_from_home(dir.path(), "detail-session");
        assert!(
            !detail.contains(&"y".repeat(1_000)),
            "session detail should not include exact historical context payloads"
        );
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let context = detail["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "context_snapshot")
            .expect("context snapshot should be present");
        assert_eq!(
            context.pointer("/raw/_intendant_context/archive_mode"),
            Some(&serde_json::json!("summary"))
        );
        assert!(context
            .get("snapshot_file")
            .and_then(|v| v.as_str())
            .is_some_and(|file| file.contains("_context_")));
    }

    #[test]
    fn session_detail_omits_oversized_compact_context_summary_parts() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("large-compact-detail-session");
        let input: Vec<serde_json::Value> = (0..620)
            .map(|idx| {
                serde_json::json!({
                    "role": "user",
                    "content": format!("compact-sentinel-{idx} {}", "x".repeat(220)),
                })
            })
            .collect();
        let compact = crate::external_agent::codex::codex_context_archive_payload(
            serde_json::json!({ "input": input }),
            "req-large-compact",
            1,
            "openai.responses.resolved_request.v1",
            false,
        );
        let compact_size = serde_json::to_vec(&compact).unwrap().len();
        assert!(compact_size > CONTEXT_REPLAY_RAW_SUMMARY_MAX_BYTES as usize);
        assert!(
            compact_size < 512 * 1024,
            "regression fixture should cover compact summaries that used to be replayed inline"
        );

        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.context_snapshot(
            "codex",
            "Codex resolved request payload",
            Some(1),
            "openai.responses.resolved_request.v1",
            Some(120_000),
            Some("backend_reported"),
            Some(128_000),
            Some(272_000),
            Some(620),
            &compact,
        );
        drop(log);

        let detail = get_session_detail_from_home(dir.path(), "large-compact-detail-session");
        assert!(
            !detail.contains("compact-sentinel-"),
            "session detail replay should not inline oversized compact summary parts"
        );
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let context = detail["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "context_snapshot")
            .expect("context snapshot should be present");
        assert_eq!(
            context.pointer("/raw/_intendant_context/raw_omitted"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            context.pointer("/raw/summary/raw_omitted"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(context["exact_replay_available"], true);
        assert!(context
            .pointer("/raw/summary_parts")
            .and_then(|v| v.as_array())
            .is_some_and(|parts| parts.is_empty()));
    }

    #[test]
    fn session_detail_limit_keeps_metadata_and_recent_entries() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("detail-limit-session");
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.session_identity("detail-limit-session", "codex", "backend-session");
        for idx in 1..=5 {
            log.model_response_for_session(
                Some("backend-session"),
                &format!("response {idx}"),
                0,
                0,
                0,
                0,
                0,
                Some("Codex"),
            );
        }
        drop(log);

        let detail =
            get_session_detail_from_home_with_limit(dir.path(), "detail-limit-session", Some(2));
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail["entries"].as_array().unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry["event"] == "session_identity"));
        let summaries: Vec<_> = entries
            .iter()
            .filter(|entry| entry["event"] == "model_response")
            .filter_map(|entry| entry["summary"].as_str())
            .collect();
        assert_eq!(summaries, vec!["response 4", "response 5"]);
    }

    #[test]
    fn session_detail_omits_oversized_latest_context_snapshot_raw() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("oversized-detail-session");
        let oversized = "z".repeat(CONTEXT_REPLAY_RAW_SUMMARY_MAX_BYTES as usize + 16_384);
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.context_snapshot(
            "codex",
            "Codex resolved request payload",
            Some(1),
            "openai.responses.resolved_request.v1",
            Some(1_000),
            Some("backend_reported"),
            Some(128_000),
            Some(272_000),
            Some(1),
            &serde_json::json!({
                "instructions": oversized,
                "input": [{"role": "user", "content": "open session detail"}]
            }),
        );
        drop(log);

        let detail = get_session_detail_from_home(dir.path(), "oversized-detail-session");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let context = detail["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "context_snapshot")
            .expect("context snapshot should be present");
        assert_eq!(
            context.pointer("/raw/_intendant_context/raw_omitted"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            context.pointer("/raw/summary/raw_omitted"),
            Some(&serde_json::json!(true))
        );
        assert!(context
            .get("snapshot_file")
            .and_then(|v| v.as_str())
            .is_some_and(|file| file.contains("_context_")));
    }

    #[test]
    fn session_context_snapshot_endpoint_loads_exact_raw_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("lazy-session");
        let exact_text = "selected tool call payload survives lazy load";
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.context_snapshot(
            "codex",
            "Codex resolved request payload",
            Some(1),
            "openai.responses.resolved_request.v1",
            Some(1_000),
            Some("backend_reported"),
            Some(128_000),
            Some(272_000),
            Some(1),
            &serde_json::json!({
                "input": [{
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": { "cmd": exact_text }
                }]
            }),
        );
        drop(log);

        let detail = get_session_detail_from_home(dir.path(), "lazy-session");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let context = detail["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "context_snapshot")
            .expect("context snapshot should be present");
        assert_eq!(
            context.pointer("/raw/_intendant_context/archive_mode"),
            Some(&serde_json::json!("summary"))
        );
        assert!(
            context.pointer("/raw/input").is_none(),
            "session detail replay should not carry exact raw input"
        );
        let snapshot_file = context["snapshot_file"]
            .as_str()
            .expect("context snapshot should carry a lazy-load file pointer");
        let encoded_file = snapshot_file.replace('/', "%2F");
        let request = format!(
            "GET /api/session/lazy-session/context-snapshot?source=intendant&file={encoded_file} HTTP/1.1"
        );
        // The HTTP lane's decode (url-decoding the file selector) feeding
        // the shared (status, body) core, over this test's own home.
        let (file, request_id, request_index, ts) =
            context_snapshot_selector_parts_from_request(&request).unwrap();
        assert_eq!(file.as_deref(), Some(snapshot_file));
        let (status, body) = session_context_snapshot_response_body(
            dir.path(),
            "lazy-session",
            "intendant",
            file,
            request_id,
            request_index,
            ts,
        );
        assert_eq!(status, "200 OK");
        let loaded: serde_json::Value = serde_json::from_str(&body).unwrap();
        let snapshot = &loaded["snapshot"];
        assert_eq!(snapshot["snapshot_file"], snapshot_file);
        assert_eq!(snapshot["exact_replay_available"], true);
        assert_eq!(
            snapshot.pointer("/raw/input/0/arguments/cmd"),
            Some(&serde_json::json!(exact_text))
        );
    }

    #[test]
    fn rotated_away_context_snapshot_is_unavailable_and_fetches_404() {
        // Golden for latest-only rotation: the historical row must (a) stop
        // advertising exact replay and (b) take the honest 404 arm on
        // fetch — never 200 with the converter's `{}` degrade.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("rotated-session");
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.set_context_snapshot_keep_all(false);
        let snapshot_raw = |turn: usize| {
            serde_json::json!({
                "input": [{ "type": "message", "content": format!("turn {turn}") }]
            })
        };
        for turn in 1..=2 {
            log.turn_start(turn, 0.0, 0);
            log.context_snapshot(
                "native",
                "Internal agent request payload",
                Some(turn),
                "test.v1",
                None,
                None,
                None,
                None,
                Some(1),
                &snapshot_raw(turn),
            );
        }
        drop(log);

        let detail = get_session_detail_from_home(dir.path(), "rotated-session");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let snapshots: Vec<&serde_json::Value> = detail["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "context_snapshot")
            .collect();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            snapshots[0]["exact_replay_available"], false,
            "rotated-away row must not advertise exact replay"
        );
        assert_eq!(snapshots[1]["exact_replay_available"], true);

        let fetch = |file: &str| {
            session_context_snapshot_response_body(
                dir.path(),
                "rotated-session",
                "intendant",
                Some(file.to_string()),
                None,
                None,
                None,
            )
        };
        let rotated_file = snapshots[0]["snapshot_file"].as_str().unwrap();
        let (status, body) = fetch(rotated_file);
        assert_eq!(status, "404 Not Found", "body: {body}");

        let latest_file = snapshots[1]["snapshot_file"].as_str().unwrap();
        let (status, body) = fetch(latest_file);
        assert_eq!(status, "200 OK", "body: {body}");
        let loaded: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            loaded["snapshot"].pointer("/raw/input/0/content"),
            Some(&serde_json::json!("turn 2"))
        );
    }
}
