//! External-session row context: id/path resolution, wrapper-index row
//! merging and application, source labels, and resumed-session activity replay.

use super::*;

/// The PASTE-FRIENDLY policy, used by replay only: accepts a bare session
/// directory name (like everything else) or a full pasted log-dir path,
/// which must canonicalize under `~/.intendant/logs` (anchored by
/// `session_names::intendant_session_dir_from_slash_path`). Every other
/// dashboard endpoint holds the bare-id line — see
/// `session_lookup_id_is_safe` for the policy split.
pub(crate) fn intendant_session_dir_from_id_or_path(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    if crate::session_names::session_id_looks_like_path(session_id) {
        return crate::session_names::intendant_session_dir_from_slash_path(home, session_id);
    }

    // Anything else must be a bare directory name — one normal path
    // component. Windows path shapes never take the validated slash route
    // above, and `logs_dir.join(<absolute or drive-relative>)` REPLACES
    // the logs root, so an id like `C:\evil\dir` would replay a session
    // log from anywhere on disk; `..` likewise walks out a level even on
    // Unix. Refuse every path-shaped id outright (the explicit backslash
    // check keeps Unix — where `\` is a legal filename byte — behaving
    // exactly like Windows).
    {
        use std::path::Component;
        let mut components = Path::new(session_id).components();
        let bare_name = matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        );
        if !bare_name || session_id.contains('\\') {
            return None;
        }
    }

    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let direct = logs_dir.join(session_id);
    if direct.is_dir() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(logs_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(session_id) {
            return Some(path);
        }
        let meta_path = path.join("session_meta.json");
        let Ok(meta_str) = std::fs::read_to_string(meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str) else {
            continue;
        };
        let Some(meta_id) = meta.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if meta_id == session_id || meta_id.starts_with(session_id) {
            return Some(path);
        }
    }

    None
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExternalSessionContext {
    pub(crate) project_root: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) source_label: Option<String>,
    pub(crate) name: Option<String>,
}

pub(crate) fn external_session_context_by_id(
    sessions: &[serde_json::Value],
) -> HashMap<String, ExternalSessionContext> {
    let mut out = HashMap::new();
    for session in sessions {
        let context = ExternalSessionContext {
            project_root: value_str(session, "project_root"),
            cwd: value_str(session, "cwd"),
            source: value_str(session, "source"),
            source_label: value_str(session, "source_label"),
            name: value_str(session, "name"),
        };
        if context.project_root.is_none()
            && context.cwd.is_none()
            && context.source.is_none()
            && context.source_label.is_none()
            && context.name.is_none()
        {
            continue;
        }
        for key in [
            value_str(session, "session_id"),
            value_str(session, "resume_id"),
        ]
        .into_iter()
        .flatten()
        {
            out.entry(key).or_insert_with(|| context.clone());
        }
    }
    out
}

pub(crate) fn session_value_matches_external_id(
    session: &serde_json::Value,
    external_id: &str,
) -> bool {
    ["session_id", "resume_id", "backend_session_id"]
        .into_iter()
        .any(|key| session.get(key).and_then(|v| v.as_str()) == Some(external_id))
}

pub(crate) fn external_session_row_matches(
    session: &serde_json::Value,
    source: &str,
    external_id: &str,
) -> bool {
    let source = crate::session_names::normalize_source(source);
    if !session_value_matches_external_id(session, external_id) {
        return false;
    }
    let row_source = session
        .get("source")
        .and_then(|v| v.as_str())
        .map(crate::session_names::normalize_source);
    let row_backend_source = session
        .get("backend_source")
        .and_then(|v| v.as_str())
        .map(crate::session_names::normalize_source);
    row_source.as_deref() == Some(source.as_str())
        || row_backend_source.as_deref() == Some(source.as_str())
}

pub(crate) fn merge_intendant_wrapper_into_external_session(
    external: &mut serde_json::Value,
    wrapper: &serde_json::Value,
) {
    let Some(obj) = external.as_object_mut() else {
        return;
    };
    let Some(wrapper_obj) = wrapper.as_object() else {
        return;
    };

    for (target_key, wrapper_key) in [
        ("intendant_session_id", "session_id"),
        ("intendant_session_path", "path"),
        ("backend_source", "backend_source"),
        ("backend_source_label", "backend_source_label"),
        ("backend_session_id", "backend_session_id"),
        ("capabilities", "capabilities"),
        ("agent_command", "agent_command"),
        ("codex_command", "codex_command"),
        ("codex_managed_context", "codex_managed_context"),
        // Claude launch pins ride the wrapper row the same way, so the
        // Launch-config modal can prefill from the sessions list.
        ("claude_model", "claude_model"),
        ("claude_permission_mode", "claude_permission_mode"),
        ("claude_allowed_tools", "claude_allowed_tools"),
        ("claude_effort", "claude_effort"),
    ] {
        if let Some(value) = wrapper_obj.get(wrapper_key) {
            obj.insert(target_key.to_string(), value.clone());
        }
    }

    for key in ["name", "task", "project_root", "cwd", "provider", "model"] {
        let current_is_empty = obj
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::is_empty)
            .unwrap_or(true);
        if current_is_empty {
            if let Some(value) = wrapper_obj.get(key).filter(|v| !v.is_null()) {
                obj.insert(key.to_string(), value.clone());
            }
        }
    }

    for key in [
        "recordings",
        "recording_bytes",
        "annotations",
        "clips",
        "frames_bytes",
        "turns_bytes",
        "logs_bytes",
        "total_bytes",
    ] {
        if let Some(value) = wrapper_obj.get(key) {
            obj.insert(format!("intendant_{key}"), value.clone());
        }
    }
    if let Some(value) = wrapper_obj.get("status") {
        obj.insert("intendant_status".to_string(), value.clone());
    }
    obj.insert(
        "can_delete_intendant_log".to_string(),
        serde_json::json!(true),
    );
    if let Some(value) = wrapper_obj.get("relationships") {
        if let Some(existing) = obj.get_mut("relationships").and_then(|v| v.as_array_mut()) {
            if let Some(items) = value.as_array() {
                for item in items {
                    if !existing.contains(item) {
                        existing.push(item.clone());
                    }
                }
            }
        } else {
            obj.insert("relationships".to_string(), value.clone());
        }
    }

    if let (Some(current), Some(wrapper_updated)) = (
        obj.get("updated_at").and_then(|v| v.as_str()),
        wrapper_obj.get("updated_at").and_then(|v| v.as_str()),
    ) {
        if timestamp_sort_secs(wrapper_updated) > timestamp_sort_secs(current) {
            obj.insert(
                "updated_at".to_string(),
                serde_json::Value::String(wrapper_updated.to_string()),
            );
        }
    }
}

pub(crate) fn external_session_source_and_id(
    session: &serde_json::Value,
) -> Option<(String, String)> {
    let source = value_str(session, "backend_source")
        .or_else(|| value_str(session, "source"))
        .map(|source| crate::session_names::normalize_source(&source))?;
    if source.is_empty() || source == "intendant" {
        return None;
    }
    let session_id = value_str(session, "backend_session_id")
        .or_else(|| value_str(session, "resume_id"))
        .or_else(|| value_str(session, "session_id"))?;
    if !crate::external_agent::source_session_id_is_canonical(&source, &session_id) {
        return None;
    }
    Some((source, session_id))
}

pub(crate) fn index_external_wrapper_session_row(home: &Path, session: &serde_json::Value) {
    let Some(source) = value_str(session, "backend_source") else {
        return;
    };
    let Some(backend_session_id) = value_str(session, "backend_session_id") else {
        return;
    };
    let Some(intendant_session_id) =
        value_str(session, "intendant_session_id").or_else(|| value_str(session, "session_id"))
    else {
        return;
    };
    let Some(log_path) =
        value_str(session, "intendant_session_path").or_else(|| value_str(session, "path"))
    else {
        return;
    };
    let project_root = value_str(session, "project_root").map(PathBuf::from);
    let _ = crate::external_wrapper_index::upsert(
        home,
        &source,
        &backend_session_id,
        &intendant_session_id,
        Path::new(&log_path),
        project_root.as_deref(),
    );
}

pub(crate) fn apply_external_wrapper_index_to_session(
    home: &Path,
    session: &mut serde_json::Value,
) {
    if value_str(session, "source")
        .map(|source| crate::session_names::normalize_source(&source))
        .as_deref()
        == Some("intendant")
    {
        return;
    }
    let Some((source, backend_session_id)) = external_session_source_and_id(session) else {
        return;
    };
    let wrappers = crate::external_wrapper_index::wrappers_for(home, &source, &backend_session_id);
    // "Active wins" selection lives in the wrapper index, not here.
    let Some(latest) = crate::external_wrapper_index::active_wrapper_in(&wrappers) else {
        return;
    };
    let Some(obj) = session.as_object_mut() else {
        return;
    };
    obj.insert(
        "intendant_session_id".to_string(),
        serde_json::Value::String(latest.intendant_session_id.clone()),
    );
    obj.insert(
        "intendant_session_path".to_string(),
        serde_json::Value::String(latest.log_path.clone()),
    );
    obj.insert(
        "intendant_wrappers".to_string(),
        serde_json::Value::Array(
            wrappers
                .iter()
                .map(crate::external_wrapper_index::record_to_json)
                .collect(),
        ),
    );
    obj.insert(
        "can_delete_intendant_log".to_string(),
        serde_json::json!(true),
    );
}

pub(crate) fn apply_external_wrapper_index_to_sessions(
    home: &Path,
    sessions: &mut [serde_json::Value],
) {
    for session in sessions {
        apply_external_wrapper_index_to_session(home, session);
    }
}

/// LEGACY (pre-2026-07 session dirs): scrape a backend thread id from a
/// human log line. Identity is recorded as structured `session_identity`
/// events (see `crate::session_identity`); readers prefer those and fall
/// back here only for dirs that predate them. Frozen grammar — never extend.
pub(crate) fn external_agent_thread_id_from_message(message: &str) -> Option<String> {
    let scraped = if let Some(thread_id) = message.strip_prefix("External agent thread: ") {
        clean_external_thread_id(thread_id)
    } else if message.starts_with("Mode: external agent") {
        message
            .rsplit_once("thread: ")
            .and_then(|(_, thread_id)| clean_external_thread_id(thread_id))
    } else {
        None
    };
    // Debug lines log placeholder thread ids (Claude Code's
    // `claude-code-session` before the stream announces the real one).
    // Treating a placeholder as a session's external id poisons every
    // consumer: the sessions list hydrates dashboard metadata with it,
    // status routing then retargets at a window that never exists, and
    // the ghost window it conjures can steal the prompt target.
    scraped.filter(|id| scraped_external_thread_id_is_canonical(id))
}

/// LEGACY (pre-2026-07 session dirs): scrape the backend source from a
/// `"Mode: external agent (…)"` log line. Structured `session_identity`
/// events are the source of truth; frozen grammar — never extend.
pub(crate) fn external_agent_source_from_message(message: &str) -> Option<String> {
    let mode = message.strip_prefix("Mode: external agent (")?;
    let (source, _) = mode.split_once(')')?;
    let source = crate::session_names::normalize_source(source);
    (!source.is_empty()).then_some(source)
}

pub(crate) fn pretty_external_source_label(source: &str) -> String {
    match crate::session_names::normalize_source(source).as_str() {
        "codex" => "Codex".to_string(),
        "claude-code" => "Claude Code".to_string(),
        "gemini" => "Gemini CLI".to_string(),
        "intendant" => "Intendant".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn clean_external_thread_id(thread_id: &str) -> Option<String> {
    let thread_id = thread_id
        .trim()
        .trim_matches(|ch: char| matches!(ch, '"' | '\'' | ',' | ';'));
    if thread_id.is_empty() || thread_id.chars().any(char::is_whitespace) {
        None
    } else {
        Some(thread_id.to_string())
    }
}

pub(crate) fn resume_session_activity_replay(
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
    task: Option<&str>,
    limit: usize,
) -> Option<String> {
    resume_session_activity_replay_from_home(
        &crate::platform::home_dir(),
        source,
        session_id,
        resume_id,
        task,
        limit,
    )
}

pub(crate) fn resume_session_activity_replay_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
    task: Option<&str>,
    limit: usize,
) -> Option<String> {
    if task.map(str::trim).is_some_and(|task| !task.is_empty()) {
        return None;
    }

    let source_norm = source.trim().to_lowercase();
    if source_norm == "intendant" {
        let log_dir = intendant_session_dir_from_id_or_path(home, session_id)?;
        return session_log_replay_payload_from_dir_with_limit(&log_dir, Some(limit))
            .map(|(payload, _)| payload);
    }

    let replay_id = resume_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(session_id);
    if let Some(log_dir) = intendant_session_dir_from_id_or_path(home, session_id) {
        if let Some((payload, external_id)) =
            session_log_replay_payload_from_dir_with_limit(&log_dir, Some(limit))
        {
            if external_id.as_deref() == Some(replay_id) {
                return Some(payload);
            }
        }
    }
    external_session_activity_replay_from_home_with_attach(
        home,
        &source_norm,
        replay_id,
        limit,
        false,
        true,
        true,
    )
}

/// The BARE-ID policy: dashboard session APIs take a plain directory name
/// (or id prefix) — anything path-shaped is invalid input, full stop.
/// The one deliberate exception is replay's paste-friendly resolver,
/// `intendant_session_dir_from_id_or_path`, which additionally accepts a
/// full log-dir path anchored under `~/.intendant/logs`. Pick one policy
/// per endpoint on purpose; never mix them in one lookup.
pub(crate) fn session_lookup_id_is_safe(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.trim() == session_id
        && session_id != "."
        && !session_id.contains("..")
        && !session_id.contains('/')
        && !session_id.contains('\\')
}

/// Resolve a session directory under `~/.intendant/logs` from a bare id:
/// exact directory, then id-prefix match, then the listed-external-row
/// fallback. Enforces the bare-id policy (`session_lookup_id_is_safe`).
pub(crate) fn resolve_bare_session_dir_from_home(home: &Path, session_id: &str) -> Option<PathBuf> {
    if !session_lookup_id_is_safe(session_id) {
        return None;
    }

    let logs_dir = crate::platform::intendant_home_in(home).join("logs");

    if logs_dir.join(session_id).is_dir() {
        return Some(logs_dir.join(session_id));
    }
    // Prefix match
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(session_id) {
                return Some(entry.path());
            }
        }
    }
    resolve_session_dir_from_listed_external_row(home, session_id)
}

pub(crate) fn resolve_session_dir_from_listed_external_row(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&list_sessions_from_home(home)).unwrap_or_default();
    for session in sessions {
        let matches = [
            "session_id",
            "resume_id",
            "backend_session_id",
            "intendant_session_id",
        ]
        .into_iter()
        .any(|key| session.get(key).and_then(|v| v.as_str()) == Some(session_id));
        if !matches {
            continue;
        }
        for key in ["intendant_session_path", "path"] {
            let Some(path) = session.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            let path = PathBuf::from(path);
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn deleted_external_sessions_path(home: &Path) -> PathBuf {
    crate::platform::intendant_home_in(home).join(DELETED_EXTERNAL_SESSIONS_FILE)
}

pub(crate) fn read_deleted_external_sessions(home: &Path) -> HashSet<(String, String)> {
    let path = deleted_external_sessions_path(home);
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(&contents)
    else {
        return HashSet::new();
    };

    let mut deleted = HashSet::new();
    for (source, ids) in root {
        let source = crate::session_names::normalize_source(&source);
        let Some(ids) = ids.as_array() else {
            continue;
        };
        for id in ids.iter().filter_map(|id| id.as_str()) {
            let id = id.trim();
            if !source.is_empty() && !id.is_empty() {
                deleted.insert((source.clone(), id.to_string()));
            }
        }
    }
    deleted
}

pub(crate) fn write_deleted_external_sessions(
    home: &Path,
    deleted: &HashSet<(String, String)>,
) -> Result<(), String> {
    let mut by_source: HashMap<String, Vec<String>> = HashMap::new();
    for (source, id) in deleted {
        by_source
            .entry(source.clone())
            .or_default()
            .push(id.clone());
    }
    let mut root = serde_json::Map::new();
    for (source, mut ids) in by_source {
        ids.sort();
        ids.dedup();
        root.insert(source, serde_json::json!(ids));
    }

    let path = deleted_external_sessions_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create tombstone dir: {e}"))?;
    }
    let body = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .map_err(|e| format!("serialize tombstones: {e}"))?;
    std::fs::write(path, body).map_err(|e| format!("write tombstones: {e}"))
}

pub(crate) fn mark_external_session_deleted(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Result<(), String> {
    let source = crate::session_names::normalize_source(source);
    let session_id = session_id.trim();
    if source.is_empty() || session_id.is_empty() {
        return Ok(());
    }
    let mut deleted = read_deleted_external_sessions(home);
    if !deleted.insert((source, session_id.to_string())) {
        return Ok(());
    }
    write_deleted_external_sessions(home, &deleted)
}

pub(crate) fn session_matches_deleted_external(
    session: &serde_json::Value,
    deleted: &HashSet<(String, String)>,
) -> bool {
    if deleted.is_empty() {
        return false;
    }
    let sources: Vec<String> = ["source", "backend_source"]
        .into_iter()
        .filter_map(|key| value_str(session, key))
        .map(|source| crate::session_names::normalize_source(&source))
        .filter(|source| !source.is_empty())
        .collect();
    let ids: Vec<String> = ["session_id", "resume_id", "backend_session_id"]
        .into_iter()
        .filter_map(|key| value_str(session, key))
        .filter(|id| !id.is_empty())
        .collect();

    sources.iter().any(|source| {
        ids.iter()
            .any(|id| deleted.contains(&(source.clone(), id.clone())))
    })
}

pub(crate) fn external_delete_target_for_intendant_session_dir(
    dir: &Path,
) -> Option<(String, String)> {
    let session_id = dir.file_name()?.to_string_lossy().to_string();
    let row = intendant_session_list_row_from_dir(dir, &session_id)?;
    let source = value_str(&row, "backend_source")?;
    let external_id = value_str(&row, "backend_session_id")?;
    if !crate::external_agent::source_session_id_is_canonical(&source, &external_id) {
        return None;
    }
    Some((source, external_id))
}

pub(crate) fn invalidate_session_list_response_cache() {
    if let Some(cache) = SESSION_LIST_RESPONSE_CACHE.get() {
        *cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// List recording streams from a recordings directory on disk.
pub(crate) fn list_recording_streams(recordings_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(recordings_dir) {
        for entry in dirs.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let stream_dir = entry.path();
            let manifest = std::fs::read_to_string(stream_dir.join("manifest.json"))
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .unwrap_or(serde_json::json!({}));
            let segments = crate::recording::parse_segment_csv_pub(
                &stream_dir.join("segments.csv"),
                &stream_dir,
            );
            if segments.is_empty()
                || !crate::recording::recording_dir_has_playable_segments(&stream_dir)
            {
                continue;
            }
            let total_duration = segments.last().map(|s| s.end_secs).unwrap_or(0.0);
            let seg_json: Vec<serde_json::Value> = segments
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "filename": s.filename,
                        "start_secs": s.start_secs,
                        "end_secs": s.end_secs,
                    })
                })
                .collect();
            let mut e = manifest;
            e["stream_name"] = serde_json::json!(name);
            e["segments"] = serde_json::Value::Array(seg_json);
            e["total_duration_secs"] = serde_json::json!(total_duration);
            entries.push(e);
        }
    }
    entries.sort_by(|a, b| a["stream_name"].as_str().cmp(&b["stream_name"].as_str()));
    entries
}

pub(crate) async fn recordings_list_response_body_in_daemon_dir(
    recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    daemon_dir: &Path,
) -> String {
    let mut all_entries = Vec::new();

    if let Some(rec_reg) = recording_registry {
        let reg = rec_reg.read().await;
        let streams = reg.all_streams();
        for name in &streams {
            let manifest = reg.manifest(name).unwrap_or(serde_json::json!({}));
            let segments = reg.segments(name);
            let total_duration = segments.last().map(|s| s.end_secs).unwrap_or(0.0);
            let seg_json: Vec<serde_json::Value> = segments
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "filename": s.filename,
                        "start_secs": s.start_secs,
                        "end_secs": s.end_secs,
                    })
                })
                .collect();
            let mut entry = manifest;
            entry["segments"] = serde_json::Value::Array(seg_json);
            entry["total_duration_secs"] = serde_json::json!(total_duration);
            all_entries.push(entry);
        }
    }

    for entry in list_recording_streams(daemon_dir) {
        all_entries.push(entry);
    }

    serde_json::to_string(&all_entries).unwrap_or("[]".to_string())
}

pub(crate) fn session_recordings_list_response_body(
    home: &Path,
    session_id: &str,
) -> (&'static str, String) {
    if !session_lookup_id_is_safe(session_id) {
        return (
            "400 Bad Request",
            serde_json::json!({ "error": "invalid session id" }).to_string(),
        );
    }
    let body = if let Some(session_dir) = resolve_bare_session_dir_from_home(home, session_id) {
        let recordings_dir = session_dir.join("recordings");
        let entries = list_recording_streams(&recordings_dir);
        serde_json::to_string(&entries).unwrap_or("[]".to_string())
    } else {
        "[]".to_string()
    };
    ("200 OK", body)
}

pub(crate) fn recording_playlist_m3u8(segments: &[crate::recording::SegmentInfo]) -> String {
    let mut m3u8 = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:0\n");
    let max_dur = segments
        .iter()
        .map(|s| s.end_secs - s.start_secs)
        .fold(0.0f64, f64::max);
    m3u8.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        max_dur.ceil() as u64
    ));
    for s in segments {
        let dur = s.end_secs - s.start_secs;
        m3u8.push_str(&format!("#EXTINF:{:.3},\n{}\n", dur, s.filename));
    }
    m3u8.push_str("#EXT-X-ENDLIST\n");
    m3u8
}

pub(crate) fn session_relationships_from_log_dir(session_dir: &Path) -> Vec<serde_json::Value> {
    let Ok(contents) = std::fs::read_to_string(session_dir.join("session.jsonl")) else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("session_relationship"))
        .filter_map(|entry| {
            let data = entry.get("data")?;
            let parent_session_id = data
                .get("parent_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let child_session_id = data
                .get("child_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let relationship = data
                .get("relationship")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if parent_session_id.is_empty()
                || child_session_id.is_empty()
                || parent_session_id == child_session_id
                || !matches!(relationship.as_str(), "side" | "subagent" | "fork")
            {
                return None;
            }
            Some(serde_json::json!({
                "parent_session_id": parent_session_id,
                "child_session_id": child_session_id,
                "relationship": relationship,
                "ephemeral": data.get("ephemeral").and_then(|v| v.as_bool()).unwrap_or(false),
            }))
        })
        .collect()
}

// ── Recording/frame asset content core (moved verbatim from
// dashboard_control/api_media.rs — transport-unification S4b). The
// RecordingAsset vocabulary is the tunnel twin's asset addressing
// ("segments" / "playlist.m3u8" / seg files); the HTTP artifact leaves
// and the tunnel's ranged byte streams resolve through these.

pub(crate) fn recording_stream_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 128
        && name.trim() == name
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

pub(crate) fn recording_asset_name_is_safe(asset: &str) -> bool {
    asset == "segments" || asset == "playlist.m3u8" || (recording_segment_filename_is_safe(asset))
}

pub(crate) fn recording_segment_filename_is_safe(filename: &str) -> bool {
    filename.starts_with("seg_")
        && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
        && filename.len() < 30
        && !filename.contains("..")
        && !filename.contains('/')
        && !filename.contains('\\')
}

pub(crate) enum RecordingAsset {
    Bytes {
        bytes: Vec<u8>,
        content_type: &'static str,
        filename: String,
    },
    File {
        path: PathBuf,
        content_type: &'static str,
        filename: String,
    },
}

pub(crate) fn resolve_session_recording_asset(
    session_dir: Option<PathBuf>,
    stream_name: &str,
    asset: &str,
) -> Result<RecordingAsset, (u16, serde_json::Value)> {
    let stream_dir = session_dir
        .as_ref()
        .map(|dir| dir.join("recordings").join(stream_name));
    let segments = stream_dir
        .as_ref()
        .map(|dir| crate::recording::parse_segment_csv_pub(&dir.join("segments.csv"), dir))
        .unwrap_or_default();
    resolve_recording_asset_from_dir_pair(stream_dir, None, segments, asset)
}

pub(crate) fn resolve_recording_asset_from_dir_pair(
    primary_dir: Option<PathBuf>,
    fallback_dir: Option<PathBuf>,
    segments: Vec<crate::recording::SegmentInfo>,
    asset: &str,
) -> Result<RecordingAsset, (u16, serde_json::Value)> {
    if asset == "segments" {
        let seg_json: Vec<serde_json::Value> = segments
            .iter()
            .map(|s| {
                serde_json::json!({
                    "filename": s.filename,
                    "start_secs": s.start_secs,
                    "end_secs": s.end_secs,
                })
            })
            .collect();
        let bytes = serde_json::to_vec(&seg_json).unwrap_or_else(|_| b"[]".to_vec());
        return Ok(RecordingAsset::Bytes {
            bytes,
            content_type: "application/json",
            filename: "segments.json".to_string(),
        });
    }
    if asset == "playlist.m3u8" {
        return Ok(RecordingAsset::Bytes {
            bytes: crate::web_gateway::recording_playlist_m3u8(&segments).into_bytes(),
            content_type: "application/vnd.apple.mpegurl",
            filename: "playlist.m3u8".to_string(),
        });
    }
    if !recording_segment_filename_is_safe(asset) {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "invalid recording asset" }),
        ));
    }
    let path = primary_dir
        .as_ref()
        .map(|dir| dir.join(asset))
        .filter(|path| path.exists())
        .or_else(|| {
            fallback_dir
                .as_ref()
                .map(|dir| dir.join(asset))
                .filter(|path| path.exists())
        });
    let Some(path) = path else {
        return Err((
            404,
            serde_json::json!({ "ok": false, "error": "recording asset not found" }),
        ));
    };
    let content_type = if asset.ends_with(".ts") {
        "video/mp2t"
    } else {
        "video/mp4"
    };
    Ok(RecordingAsset::File {
        path,
        content_type,
        filename: asset.to_string(),
    })
}

pub(crate) fn read_recording_asset_bytes_range(
    bytes: Vec<u8>,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), (u16, serde_json::Value)> {
    let total_size = bytes.len() as u64;
    let (start, transfer_len, end) = recording_asset_range(total_size, offset, length)?;
    let start = usize::try_from(start).map_err(|_| {
        (
            413,
            serde_json::json!({ "ok": false, "error": "range too large for this platform" }),
        )
    })?;
    Ok((bytes[start..start + transfer_len].to_vec(), total_size, end))
}

pub(crate) fn read_recording_asset_file_range(
    path: &std::path::Path,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), (u16, serde_json::Value)> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("stat recording asset: {e}") }),
        )
    })?;
    let total_size = metadata.len();
    let (start, transfer_len, end) = recording_asset_range(total_size, offset, length)?;
    let mut file = std::fs::File::open(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("open recording asset: {e}") }),
        )
    })?;
    file.seek(std::io::SeekFrom::Start(start)).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("seek recording asset: {e}") }),
        )
    })?;
    let mut bytes = vec![0u8; transfer_len];
    file.read_exact(&mut bytes).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("read recording asset: {e}") }),
        )
    })?;
    Ok((bytes, total_size, end))
}

pub(crate) fn recording_asset_range(
    total_size: u64,
    offset: u64,
    length: Option<u64>,
) -> Result<(u64, usize, u64), (u16, serde_json::Value)> {
    if offset > total_size {
        return Err((
            416,
            serde_json::json!({
                "ok": false,
                "error": "range start beyond recording asset size",
                "total_size": total_size,
            }),
        ));
    }
    let available = total_size.saturating_sub(offset);
    let requested = length.unwrap_or(available).min(available);
    if requested > crate::web_gateway::UPLOAD_MAX_BYTES as u64 {
        return Err((
            413,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "range too large: {} bytes (cap is {})",
                    requested,
                    crate::web_gateway::UPLOAD_MAX_BYTES
                ),
            }),
        ));
    }
    let transfer_len = usize::try_from(requested).map_err(|_| {
        (
            413,
            serde_json::json!({ "ok": false, "error": "range too large for this platform" }),
        )
    })?;
    Ok((offset, transfer_len, offset.saturating_add(requested)))
}

pub(crate) fn session_frame_filename_is_safe(filename: &str) -> bool {
    (filename.ends_with(".jpg") || filename.ends_with(".png"))
        && filename.len() < 80
        && !filename.is_empty()
        && filename.trim() == filename
        && !filename.contains("..")
        && !filename.contains('/')
        && !filename.contains('\\')
}

pub(crate) fn read_frame_asset_file_range(
    path: &std::path::Path,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), (u16, serde_json::Value)> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("stat session frame: {e}") }),
        )
    })?;
    let total_size = metadata.len();
    let (start, transfer_len, end) = recording_asset_range(total_size, offset, length)?;
    let mut file = std::fs::File::open(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("open session frame: {e}") }),
        )
    })?;
    file.seek(std::io::SeekFrom::Start(start)).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("seek session frame: {e}") }),
        )
    })?;
    let mut bytes = vec![0u8; transfer_len];
    file.read_exact(&mut bytes).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("read session frame: {e}") }),
        )
    })?;
    Ok((bytes, total_size, end))
}

// ── Live (daemon-scoped) recordings + HQ-frame byte lanes
// (transport-unification S8). The legacy non-API `/recordings*` and
// `/frames/{id}` chain arms and the tunnel residue methods
// (`api_recordings`, `api_recording_asset`) share these fns: one store
// resolution (registry segments with the daemon-recordings-dir CSV
// fallback, session-dir-first file candidates), rendered per lane — the
// chain's historical full-body shapes here as [`ApiResponse`]s, the
// tunnel's ranged byte-stream carriage in
// `dashboard_control::api_media`. The `_in_daemon_dir` variants are the
// hermetic cores (tests inject a tempdir); the ambient wrappers resolve
// `crate::debug::daemon_recordings_dir()` once at the transport edge.

/// The historical `text/plain` error tail the `/recordings*` and
/// `/frames/*` chain arms answer with (`Connection: close`, nothing
/// else). Byte lane so the content type survives the render.
pub(crate) fn live_media_text_plain_api_response(status: u16, body: &str) -> ApiResponse {
    ApiResponse::Bytes {
        status,
        content_type: "text/plain".to_string(),
        headers: vec![("Connection", "close".to_string())],
        bytes: BytesPayload::InMemory(body.as_bytes().to_vec()),
        meta: serde_json::Value::Null,
    }
}

/// `GET /recordings` (chain) / tunnel `api_recordings`: every stream the
/// live registry knows plus the daemon-scoped store, under the
/// historical `no-cache` json tail.
pub(crate) async fn recordings_list_api_response(
    recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
) -> ApiResponse {
    recordings_list_api_response_in_daemon_dir(
        recording_registry,
        &crate::debug::daemon_recordings_dir(),
    )
    .await
}

pub(crate) async fn recordings_list_api_response_in_daemon_dir(
    recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    daemon_dir: &Path,
) -> ApiResponse {
    let body = recordings_list_response_body_in_daemon_dir(recording_registry, daemon_dir).await;
    ApiResponse::json(200, JsonBody::PreSerialized(body))
}

/// Live-registry recording-asset resolution shared by both lanes: the
/// registry's segments with the daemon-dir CSV fallback when the stream
/// has none, then the dir-pair candidates (session recordings dir first,
/// daemon dir second) through the one asset vocabulary
/// ([`resolve_recording_asset_from_dir_pair`]).
pub(crate) async fn resolve_live_recording_asset_in_daemon_dir(
    registry: Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>,
    daemon_dir: &Path,
    stream_name: &str,
    asset: &str,
) -> Result<RecordingAsset, (u16, serde_json::Value)> {
    let (session_dir, mut segments) = {
        let reg = registry.read().await;
        (reg.session_dir().to_path_buf(), reg.segments(stream_name))
    };
    if segments.is_empty() {
        let stream_dir = daemon_dir.join(stream_name);
        segments =
            crate::recording::parse_segment_csv_pub(&stream_dir.join("segments.csv"), &stream_dir);
    }
    resolve_recording_asset_from_dir_pair(
        Some(session_dir.join("recordings").join(stream_name)),
        Some(daemon_dir.join(stream_name)),
        segments,
        asset,
    )
}

/// `GET /recordings/{stream}/{asset}` (chain): the historical full-body
/// shapes over the shared live resolution. `path_part` arrives verbatim
/// from the request line (historically including any query string, which
/// then fails the asset vocabulary — pinned by the goldens):
/// segments listing json and the HLS playlist under `no-cache`, segment
/// files under `public, max-age=3600`, and the `text/plain` error trio
/// (`Recording not available` / `Not found` / `Invalid filename` /
/// `Segment not found`).
pub(crate) async fn live_recordings_path_api_response(
    recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    daemon_dir: &Path,
    path_part: &str,
) -> ApiResponse {
    let Some(registry) = recording_registry else {
        return live_media_text_plain_api_response(404, "Recording not available");
    };
    let parts: Vec<&str> = path_part.split('/').collect();
    if parts.len() != 2 {
        return live_media_text_plain_api_response(404, "Not found");
    }
    let (stream_name, asset) = (parts[0], parts[1]);
    match resolve_live_recording_asset_in_daemon_dir(registry, daemon_dir, stream_name, asset).await
    {
        // Segments listing + HLS playlist: in-memory renders under the
        // historical no-cache tail (content type from the vocabulary).
        Ok(RecordingAsset::Bytes {
            bytes,
            content_type,
            filename: _,
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
        // Segment file: full read under the historical cacheable tail; a
        // vanished-underfoot file keeps the historical 404 wording.
        Ok(RecordingAsset::File {
            path,
            content_type,
            filename: _,
        }) => match tokio::fs::read(&path).await {
            Ok(data) => ApiResponse::Bytes {
                status: 200,
                content_type: content_type.to_string(),
                headers: vec![
                    ("Cache-Control", "public, max-age=3600".to_string()),
                    ("Connection", "close".to_string()),
                ],
                bytes: BytesPayload::InMemory(data),
                meta: serde_json::Value::Null,
            },
            Err(_) => live_media_text_plain_api_response(404, "Segment not found"),
        },
        // The shared vocabulary's json errors map onto the chain's
        // historical text/plain wordings: 400 = the asset name failed the
        // vocabulary ("Invalid filename"), anything else = not found in
        // either dir ("Segment not found").
        Err((400, _)) => live_media_text_plain_api_response(400, "Invalid filename"),
        Err((status, _)) => live_media_text_plain_api_response(status, "Segment not found"),
    }
}

/// `GET /frames/{frame_id}` (chain): one HQ frame from the live frame
/// registry under the historical immutable-cache tail; the registry read
/// stays at the transport edge (async lock), this renders its result.
pub(crate) fn frame_hq_api_response(jpeg: Option<Vec<u8>>) -> ApiResponse {
    match jpeg {
        Some(data) => ApiResponse::Bytes {
            status: 200,
            content_type: "image/jpeg".to_string(),
            headers: vec![
                (
                    "Cache-Control",
                    "public, max-age=31536000, immutable".to_string(),
                ),
                ("Connection", "close".to_string()),
            ],
            bytes: BytesPayload::InMemory(data),
            meta: serde_json::Value::Null,
        },
        None => live_media_text_plain_api_response(404, "Frame not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_playlist_m3u8_formats_segments_for_hls() {
        let segments = vec![
            crate::recording::SegmentInfo {
                filename: "seg_00000.mp4".to_string(),
                start_secs: 0.0,
                end_secs: 1.25,
                path: std::path::PathBuf::from("seg_00000.mp4"),
            },
            crate::recording::SegmentInfo {
                filename: "seg_00001.mp4".to_string(),
                start_secs: 1.25,
                end_secs: 3.0,
                path: std::path::PathBuf::from("seg_00001.mp4"),
            },
        ];

        assert_eq!(
            recording_playlist_m3u8(&segments),
            concat!(
                "#EXTM3U\n",
                "#EXT-X-VERSION:3\n",
                "#EXT-X-MEDIA-SEQUENCE:0\n",
                "#EXT-X-TARGETDURATION:2\n",
                "#EXTINF:1.250,\n",
                "seg_00000.mp4\n",
                "#EXTINF:1.750,\n",
                "seg_00001.mp4\n",
                "#EXT-X-ENDLIST\n",
            )
        );
    }

    // ── S8 goldens: the legacy `/recordings*` + `/frames/*` chain wire
    // bytes. Expected framing is hand-written from the historical inline
    // chain arms (`http_dispatch.rs`) so the S8 re-plumb onto the neutral
    // fns above can be proven byte-identical (design §6 S8, risk R1);
    // store-dependent bodies come from store-layer fns the conversion
    // does not touch, over injected tempdirs — the ambient
    // `daemon_recordings_dir()` is never read.

    fn golden_live_transcript(response: ApiResponse) -> String {
        String::from_utf8_lossy(&crate::web_gateway::api_response_http_bytes(
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        ))
        .into_owned()
    }

    fn golden_live_registry(
        session_dir: &Path,
    ) -> Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>> {
        Arc::new(tokio::sync::RwLock::new(
            crate::recording::RecordingRegistry::new(
                session_dir,
                crate::project::RecordingConfig::default(),
            ),
        ))
    }

    /// One playable stream under `<root>/<stream>`: a csv row plus the
    /// on-disk segment (`list_recording_streams` skips streams without
    /// playable bytes).
    fn golden_seed_stream(root: &Path, stream: &str, filename: &str, bytes: &[u8]) {
        let stream_dir = root.join(stream);
        std::fs::create_dir_all(&stream_dir).unwrap();
        std::fs::write(stream_dir.join(filename), bytes).unwrap();
        std::fs::write(
            stream_dir.join("segments.csv"),
            format!("{filename},0.0,2.0\n"),
        )
        .unwrap();
    }

    fn golden_live_text_plain_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn golden_live_recordings_list_transcript() {
        let session_dir = tempfile::tempdir().unwrap();
        let daemon_dir = tempfile::tempdir().unwrap();
        golden_seed_stream(
            &session_dir.path().join("recordings"),
            "screen",
            "seg_00001.mp4",
            b"live session segment bytes",
        );
        golden_seed_stream(
            daemon_dir.path(),
            "daemon0",
            "seg_00001.mp4",
            b"daemon segment",
        );
        let registry = golden_live_registry(session_dir.path());

        // The framing pin: the body is the untouched store listing
        // (registry streams first, then the daemon-scoped store).
        let body =
            recordings_list_response_body_in_daemon_dir(Some(registry.clone()), daemon_dir.path())
                .await;
        assert!(body.contains("\"stream_name\":\"daemon0\""), "{body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.as_array().map(|entries| entries.len()), Some(2));

        let transcript = golden_live_transcript(
            recordings_list_api_response_in_daemon_dir(Some(registry), daemon_dir.path()).await,
        );
        assert_eq!(
            transcript,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        );

        // No live registry: the daemon-scoped store still lists.
        let body = recordings_list_response_body_in_daemon_dir(None, daemon_dir.path()).await;
        let transcript = golden_live_transcript(
            recordings_list_api_response_in_daemon_dir(None, daemon_dir.path()).await,
        );
        assert!(transcript.ends_with(&body), "{transcript}");
        assert!(
            transcript.starts_with("HTTP/1.1 200 OK\r\n"),
            "{transcript}"
        );
    }

    #[tokio::test]
    async fn golden_live_recording_segments_and_playlist_transcripts() {
        // Registry with no segments for the stream: the daemon-dir csv
        // fallback resolves (the historical chain's fallback branch).
        let session_dir = tempfile::tempdir().unwrap();
        let daemon_dir = tempfile::tempdir().unwrap();
        golden_seed_stream(
            daemon_dir.path(),
            "display_0",
            "seg_00001.mp4",
            b"daemon bytes",
        );
        let registry = golden_live_registry(session_dir.path());

        let expected_body = serde_json::to_string(&vec![serde_json::json!({
            "filename": "seg_00001.mp4",
            "start_secs": 0.0,
            "end_secs": 2.0,
        })])
        .unwrap();
        let transcript = golden_live_transcript(
            live_recordings_path_api_response(
                Some(registry.clone()),
                daemon_dir.path(),
                "display_0/segments",
            )
            .await,
        );
        assert_eq!(
            transcript,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{expected_body}",
                expected_body.len()
            )
        );

        // HLS playlist over the same segments (store-layer body).
        let segments = crate::recording::parse_segment_csv_pub(
            &daemon_dir.path().join("display_0").join("segments.csv"),
            &daemon_dir.path().join("display_0"),
        );
        let expected_playlist = recording_playlist_m3u8(&segments);
        let transcript = golden_live_transcript(
            live_recordings_path_api_response(
                Some(registry),
                daemon_dir.path(),
                "display_0/playlist.m3u8",
            )
            .await,
        );
        assert_eq!(
            transcript,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{expected_playlist}",
                expected_playlist.len()
            )
        );
    }

    #[tokio::test]
    async fn golden_live_recording_segment_file_transcripts() {
        // The session recordings dir wins over the daemon dir when both
        // hold the filename (the historical candidate order), and the
        // `.ts` leaf serves under `video/mp2t`.
        let session_dir = tempfile::tempdir().unwrap();
        let daemon_dir = tempfile::tempdir().unwrap();
        let session_bytes = b"session segment bytes".to_vec();
        golden_seed_stream(
            &session_dir.path().join("recordings"),
            "display_0",
            "seg_00001.mp4",
            &session_bytes,
        );
        golden_seed_stream(
            daemon_dir.path(),
            "display_0",
            "seg_00001.mp4",
            b"daemon copy",
        );
        std::fs::write(
            daemon_dir.path().join("display_0").join("seg_00002.ts"),
            b"transport stream bytes",
        )
        .unwrap();
        let registry = golden_live_registry(session_dir.path());

        let transcript = golden_live_transcript(
            live_recordings_path_api_response(
                Some(registry.clone()),
                daemon_dir.path(),
                "display_0/seg_00001.mp4",
            )
            .await,
        );
        let mut expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nCache-Control: public, max-age=3600\r\nConnection: close\r\n\r\n",
            session_bytes.len()
        );
        expected.push_str(&String::from_utf8(session_bytes).unwrap());
        assert_eq!(transcript, expected);

        let transcript = golden_live_transcript(
            live_recordings_path_api_response(
                Some(registry),
                daemon_dir.path(),
                "display_0/seg_00002.ts",
            )
            .await,
        );
        assert!(
            transcript.starts_with(
                "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 22\r\nCache-Control: public, max-age=3600\r\nConnection: close\r\n\r\n"
            ),
            "{transcript}"
        );
        assert!(
            transcript.ends_with("transport stream bytes"),
            "{transcript}"
        );
    }

    #[tokio::test]
    async fn golden_live_recording_error_transcripts() {
        let session_dir = tempfile::tempdir().unwrap();
        let daemon_dir = tempfile::tempdir().unwrap();
        let registry = golden_live_registry(session_dir.path());

        // No registry wired: everything under /recordings/ answers 404.
        let transcript = golden_live_transcript(
            live_recordings_path_api_response(None, daemon_dir.path(), "display_0/segments").await,
        );
        assert_eq!(
            transcript,
            golden_live_text_plain_transcript("404 Not Found", "Recording not available")
        );

        // Path shapes other than {stream}/{asset} answer 404.
        for path_part in ["display_0", "a/b/c"] {
            let transcript = golden_live_transcript(
                live_recordings_path_api_response(
                    Some(registry.clone()),
                    daemon_dir.path(),
                    path_part,
                )
                .await,
            );
            assert_eq!(
                transcript,
                golden_live_text_plain_transcript("404 Not Found", "Not found"),
                "{path_part}"
            );
        }

        // Asset names outside the vocabulary answer 400 — including the
        // historical query-string rider (never split off the request
        // line) and traversal shapes.
        for path_part in [
            "display_0/evil.mp4",
            "display_0/seg_00001.mp4?t=5",
            "display_0/..",
        ] {
            let transcript = golden_live_transcript(
                live_recordings_path_api_response(
                    Some(registry.clone()),
                    daemon_dir.path(),
                    path_part,
                )
                .await,
            );
            assert_eq!(
                transcript,
                golden_live_text_plain_transcript("400 Bad Request", "Invalid filename"),
                "{path_part}"
            );
        }

        // A valid name that exists in neither dir answers 404.
        let transcript = golden_live_transcript(
            live_recordings_path_api_response(
                Some(registry),
                daemon_dir.path(),
                "display_0/seg_09999.mp4",
            )
            .await,
        );
        assert_eq!(
            transcript,
            golden_live_text_plain_transcript("404 Not Found", "Segment not found")
        );
    }

    #[test]
    fn golden_frame_hq_transcripts() {
        let jpeg = b"hq frame jpeg bytes".to_vec();
        let transcript = golden_live_transcript(frame_hq_api_response(Some(jpeg.clone())));
        let mut expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nCache-Control: public, max-age=31536000, immutable\r\nConnection: close\r\n\r\n",
            jpeg.len()
        );
        expected.push_str(&String::from_utf8(jpeg).unwrap());
        assert_eq!(transcript, expected);

        let transcript = golden_live_transcript(frame_hq_api_response(None));
        assert_eq!(
            transcript,
            golden_live_text_plain_transcript("404 Not Found", "Frame not found")
        );
    }

    #[test]
    fn external_agent_thread_id_is_extracted_from_log_messages() {
        assert_eq!(
            external_agent_thread_id_from_message(
                "External agent thread: 019e41de-e785-7581-85dd-8e74bb464c6c"
            )
            .as_deref(),
            Some("019e41de-e785-7581-85dd-8e74bb464c6c")
        );
        assert_eq!(
            external_agent_thread_id_from_message(
                "Mode: external agent (Codex) via presence, thread: codex-session-1"
            )
            .as_deref(),
            Some("codex-session-1")
        );
        assert_eq!(
            external_agent_source_from_message(
                "Mode: external agent (Claude Code) via presence, thread: claude-session-1"
            )
            .as_deref(),
            Some("claude-code")
        );
    }

    #[test]
    fn external_session_context_indexes_session_and_resume_ids() {
        let sessions = vec![serde_json::json!({
            "session_id": "display-id",
            "resume_id": "resume-id",
            "project_root": "/repo",
            "cwd": "/repo/.worktrees/feature",
            "source": "codex",
            "source_label": "Codex",
            "name": "Dashboard task"
        })];

        let context = external_session_context_by_id(&sessions);
        assert_eq!(
            context
                .get("display-id")
                .and_then(|ctx| ctx.project_root.as_deref()),
            Some("/repo")
        );
        assert_eq!(
            context.get("resume-id").and_then(|ctx| ctx.cwd.as_deref()),
            Some("/repo/.worktrees/feature")
        );
        assert_eq!(
            context
                .get("resume-id")
                .and_then(|ctx| ctx.source.as_deref()),
            Some("codex")
        );
        assert_eq!(
            context
                .get("resume-id")
                .and_then(|ctx| ctx.source_label.as_deref()),
            Some("Codex")
        );
        assert_eq!(
            context.get("resume-id").and_then(|ctx| ctx.name.as_deref()),
            Some("Dashboard task")
        );
    }

    #[test]
    fn session_log_search_filters_deleted_external_references_from_parent_logs() {
        let home = tempfile::tempdir().unwrap();
        let parent_id = "intendant-parent-search-session";
        let deleted_external_id = "019e37ae-deleted-search";
        let deleted_marker = "deleted-parent-search-token";
        let visible_marker = "visible-parent-search-token";
        let log_dir = home.path().join(".intendant").join("logs").join(parent_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": parent_id,
                "created_at": "2026-05-17T20:44:00",
                "task": "parent daemon session",
                "status": "completed"
            })
            .to_string(),
        )
        .unwrap();
        let lines = [
            serde_json::json!({
                "ts": "2026-05-17T20:44:01",
                "event": "presence_log",
                "level": "debug",
                "message": format!("[ws] ControlMsg: \"CreateSession {{ task: \\\"{deleted_marker}\\\" }}\"")
            }),
            serde_json::json!({
                "ts": "2026-05-17T20:44:02",
                "event": "session_started",
                "message": format!("Session started: {deleted_external_id} {deleted_marker}"),
                "data": {
                    "source": "codex",
                    "session_id": deleted_external_id,
                    "task": deleted_marker,
                }
            }),
            serde_json::json!({
                "ts": "2026-05-17T20:44:03",
                "event": "info",
                "message": visible_marker
            }),
        ];
        std::fs::write(
            log_dir.join("session.jsonl"),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        mark_external_session_deleted(home.path(), "codex", deleted_external_id).unwrap();

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            deleted_marker,
            "all",
            "exact_phrase",
        ))
        .unwrap();
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert!(
            results.is_empty(),
            "deleted external child references should not leak through parent log search: {results:?}"
        );

        let response: serde_json::Value = serde_json::from_str(&session_log_search_from_home(
            home.path(),
            visible_marker,
            "all",
            "exact_phrase",
        ))
        .unwrap();
        let results = response.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("session_id").and_then(|v| v.as_str()),
            Some(parent_id)
        );
    }

    #[test]
    fn list_sessions_filters_deleted_external_session_tombstones() {
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-deleted-external";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": id,
                "updated_at": "2026-05-17T20:44:33Z"
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/Users/vm/projects/intendant"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Delete me"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        assert!(
            sessions
                .iter()
                .any(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id)),
            "codex session should be listed before tombstone"
        );

        mark_external_session_deleted(home.path(), "codex", id).unwrap();

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        assert!(
            !sessions
                .iter()
                .any(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id)),
            "tombstoned codex session should be hidden"
        );
    }

    #[test]
    fn resume_session_open_limits_external_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-full-activity-replay";
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        })];
        for n in 1..=300 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-05-17T16:{:02}:00Z", 49 + (n / 60)),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": if n % 2 == 0 { "assistant" } else { "user" },
                    "content": [{ "type": "text", "text": format!("turn message {n}") }]
                }
            }));
        }
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            lines
                .into_iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let replay = resume_session_activity_replay_from_home(
            dir.path(),
            "codex",
            session_id,
            None,
            None,
            EXTERNAL_ACTIVITY_REPLAY_LIMIT,
        )
        .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let contents: Vec<_> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "log_entry")
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(contents.len(), EXTERNAL_ACTIVITY_REPLAY_LIMIT);
        assert_eq!(contents.first(), Some(&"turn message 51"));
        assert_eq!(contents.last(), Some(&"turn message 300"));
        assert!(replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "log_entry")
            .all(|entry| entry["session_id"] == session_id));
    }

    #[test]
    fn resume_session_open_compacts_large_external_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-compact-activity-replay";
        let large_output = "x".repeat(WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 100);
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_large",
                        "output": large_output
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay = resume_session_activity_replay_from_home(
            dir.path(),
            "codex",
            session_id,
            None,
            None,
            EXTERNAL_ACTIVITY_REPLAY_LIMIT,
        )
        .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let content = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["kind"] == "agent_output")
            .and_then(|entry| entry["content"].as_str())
            .expect("large tool output should replay as compact log entry");

        assert_eq!(
            content.len(),
            WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + "...".len()
        );
        assert!(content.ends_with("..."));
        let replay_output = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["kind"] == "agent_output")
            .expect("large tool output should replay");
        assert_eq!(replay_output["full_output_available"], true);
        assert_eq!(
            replay_output["full_output_bytes"],
            WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 100
        );
    }

    #[test]
    fn resume_session_open_replays_external_transcript_without_attach_marker() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-e756-7461-9946-34b639448717";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Open this from Sessions" }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay = resume_session_activity_replay_from_home(
            dir.path(),
            "codex",
            session_id,
            None,
            None,
            80,
        )
        .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        assert_eq!(entries[0]["event"], "replay_start");
        assert!(
            entries
                .iter()
                .all(|entry| entry["event"] != "session_attached"),
            "Sessions-tab open replay should let the live attach event render the attach line"
        );
        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["content"] == "Open this from Sessions"
        }));
    }

    #[test]
    fn resume_session_open_does_not_replay_when_task_is_submitted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resume_session_activity_replay_from_home(
            dir.path(),
            "codex",
            "session-1",
            None,
            Some("continue the task"),
            80,
        )
        .is_none());
    }

    #[test]
    fn resume_session_open_replays_intendant_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("session-1");
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.model_response("internal history", 0, 0, 0, 0, None);
        drop(log);

        let replay = resume_session_activity_replay_from_home(
            dir.path(),
            "intendant",
            "session-1",
            None,
            None,
            80,
        )
        .expect("intendant session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();

        assert!(replay["entries"].as_array().unwrap().iter().any(|entry| {
            entry["event"] == "model_response" && entry["summary"] == "internal history"
        }));
    }

    #[test]
    fn resume_session_open_rejects_intendant_slash_path_outside_logs_root() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let log_dir = outside.path().join("session-escape");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.model_response("outside history", 0, 0, 0, 0, None);
        drop(log);

        assert!(resume_session_activity_replay_from_home(
            home.path(),
            "intendant",
            &log_dir.to_string_lossy(),
            None,
            None,
            80,
        )
        .is_none());
    }

    #[test]
    fn intendant_session_dir_refuses_path_shaped_session_ids() {
        // Non-slash ids join under the logs root, and join() with an
        // absolute / drive-relative / parent path REPLACES or escapes it
        // — the Windows shapes never reach the validated slash route, so
        // every path-shaped id must be refused outright, on every OS.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".intendant").join("logs")).unwrap();
        for id in [
            "..",
            r"..\..",
            r"C:\outside\dir",
            r"C:evil",
            r"logs\x",
            ".",
            "",
        ] {
            assert!(
                intendant_session_dir_from_id_or_path(home.path(), id).is_none(),
                "path-shaped session id {id:?} must be refused"
            );
        }
    }

    #[test]
    fn merged_external_session_preserves_wrapper_relationships() {
        let mut external = serde_json::json!({
            "source": "codex",
            "session_id": "parent",
            "resume_id": "parent",
        });
        let wrapper = serde_json::json!({
            "session_id": "wrapper",
            "backend_source": "codex",
            "backend_session_id": "parent",
            "relationships": [{
                "parent_session_id": "parent",
                "child_session_id": "child",
                "relationship": "subagent",
                "ephemeral": false,
            }],
        });

        merge_intendant_wrapper_into_external_session(&mut external, &wrapper);

        let relationships = external["relationships"].as_array().unwrap();
        assert_eq!(relationships.len(), 1);
        assert_eq!(relationships[0]["parent_session_id"], "parent");
        assert_eq!(relationships[0]["child_session_id"], "child");
    }

    #[test]
    fn resolve_session_dir_accepts_external_backend_id() {
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
                "task": "external report"
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

        assert_eq!(
            resolve_bare_session_dir_from_home(home.path(), backend_id).as_deref(),
            Some(wrapper_dir.as_path())
        );
    }

    #[test]
    fn resolve_session_dir_rejects_unsafe_session_ids() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".intendant/logs/safe-session")).unwrap();

        for session_id in [
            "",
            ".",
            "..",
            "../logs",
            "safe/session",
            "safe\\session",
            " safe",
        ] {
            assert!(
                resolve_bare_session_dir_from_home(home.path(), session_id).is_none(),
                "unsafe session id resolved: {session_id:?}"
            );
        }

        let expected = home.path().join(".intendant/logs/safe-session");
        assert_eq!(
            resolve_bare_session_dir_from_home(home.path(), "safe").as_deref(),
            Some(expected.as_path())
        );
    }

    #[test]
    fn resume_external_wrapper_replays_full_log_with_editable_user_turns() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let log_dir = home.join(".intendant").join("logs").join("wrapper-session");
        let backend_id = "019e598b-editable-wrapper-replay";
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.session_started("wrapper-session", Some("external task"));
        log.session_identity("wrapper-session", "codex", backend_id);
        log.info("Mode: external agent (Codex)");
        log.info("[user] first prompt");
        log.info("full wrapper-only event");
        log.info("[user] second prompt");
        drop(log);

        let codex_dir = home.join(".codex").join("sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join(format!("rollout-2026-05-17T16-48-52-{backend_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": backend_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "first prompt" }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:50:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "assistant reply" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:51:00Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "second prompt" }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay = resume_session_activity_replay_from_home(
            home,
            "codex",
            "wrapper-session",
            Some(backend_id),
            None,
            EXTERNAL_ACTIVITY_REPLAY_LIMIT,
        )
        .expect("wrapper session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry" && entry["content"] == "full wrapper-only event"
        }));
        let first_prompt = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "first prompt")
            .expect("first prompt should replay from wrapper log");
        assert_eq!(first_prompt["session_id"], backend_id);
        assert_eq!(first_prompt["user_turn_index"], 1);
        assert_eq!(first_prompt["user_turn_revision"], 1);
        let second_prompt = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "second prompt")
            .expect("second prompt should replay from wrapper log");
        assert_eq!(second_prompt["user_turn_index"], 2);
        assert_eq!(second_prompt["user_turn_revision"], 1);

        let detail: serde_json::Value =
            serde_json::from_str(&get_session_detail_from_home(home, "wrapper-session")).unwrap();
        let detail_entries = detail["entries"].as_array().unwrap();
        let detail_prompt = detail_entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "first prompt")
            .expect("session detail should expose editable wrapper prompt");
        assert_eq!(detail_prompt["session_id"], backend_id);
        assert_eq!(detail_prompt["user_turn_index"], 1);
        assert_eq!(detail_prompt["user_turn_revision"], 1);
    }
}
