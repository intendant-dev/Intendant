//! The non-HTTP session catalog: list/index caches and their
//! fingerprints, external (codex/claude/gemini) session-file parsing,
//! transcripts and activity replay assembly, context-snapshot replay,
//! session search, worktree observed-session hints, usage accounting,
//! and the sort/merge/stream core behind the sessions API.

use super::*;

pub(crate) static SESSION_SEARCH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub(crate) static EXTERNAL_TRANSCRIPT_CACHE: OnceLock<
    Mutex<HashMap<String, ExternalTranscriptCacheEntry>>,
> = OnceLock::new();

pub(crate) static SESSION_LIST_RESPONSE_CACHE: OnceLock<
    Mutex<Option<SessionListResponseCacheEntry>>,
> = OnceLock::new();

pub(crate) const EXTERNAL_SESSION_SCAN_LIMIT: usize = 2_000;

pub(crate) const EXTERNAL_SESSION_READ_LIMIT: u64 = 512 * 1024;

pub(crate) const CODEX_SESSION_INDEX_TAIL_READ_LIMIT: u64 = 2 * 1024 * 1024;

pub(crate) const SESSION_LIST_STREAM_QUICK_LIMIT: usize = 600;

pub(crate) const CODEX_SESSION_LIST_PREFIX_READ_LIMIT: u64 = 8 * 1024 * 1024;

pub(crate) const CODEX_SESSION_LIST_PREFIX_LINE_LIMIT: usize = 64;

pub(crate) const CODEX_PARENT_BASELINE_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub(crate) const CODEX_PARENT_BASELINE_SCAN_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) const WORKTREE_OBSERVED_SESSION_FILE_LIMIT: usize = 1_000;

pub(crate) const WORKTREE_OBSERVED_HINT_LIMIT: usize = 1_000;

pub(crate) const WORKTREE_OBSERVED_PATHS_PER_SESSION: usize = 32;

pub(crate) const EXTERNAL_TRANSCRIPT_CACHE_LIMIT: usize = 32;

pub(crate) const SESSION_LIST_ROW_CACHE_LIMIT: usize = 8_192;

pub(crate) const SESSION_LIST_LIMIT: usize = 5_000;

pub(crate) const SESSION_LIST_RESPONSE_CACHE_TTL_SECS: u64 = 30;

pub(crate) const SESSION_DETAIL_ENTRY_LIMIT_MAX: usize = 1_000;

pub(crate) const WEBSOCKET_BOOTSTRAP_REPLAY_ENTRY_LIMIT: usize = 250;

pub(crate) const WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES: usize = 16 * 1024;

pub(crate) const SESSION_SOURCE_FLOOR: usize = 100;

pub(crate) const SESSION_LOG_SEARCH_SNIPPETS_PER_SESSION: usize = 3;

pub(crate) const SESSION_LOG_SEARCH_SNIPPET_CHARS: usize = 220;

pub(crate) const DELETED_EXTERNAL_SESSIONS_FILE: &str = "deleted_external_sessions.json";

pub(crate) const MANAGED_CONTEXT_ANCHOR_TRACE_LIMIT: usize = 64;

pub(crate) const MANAGED_CONTEXT_ANCHOR_LIMIT: usize = 40;

pub(crate) const MANAGED_CONTEXT_FISSION_GROUP_LIMIT: usize = 50;

pub(crate) const MANAGED_CONTEXT_FISSION_BRANCH_LIMIT: usize = 50;

pub(crate) const EXTERNAL_CONTEXT_REPLAY_LOG_SCAN_LIMIT: usize = 16;

pub(crate) const CONTEXT_REPLAY_RAW_SUMMARY_MAX_BYTES: u64 = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalTranscriptCacheKey {
    source: String,
    session_id: String,
    path: String,
    len: u64,
    mtime_nanos: u128,
}

#[derive(Clone, Debug)]
pub(crate) struct ExternalTranscriptCacheEntry {
    key: ExternalTranscriptCacheKey,
    entries: Vec<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionListResponseCacheEntry {
    generated_at: std::time::Instant,
    body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionListCacheKey {
    namespace: &'static str,
    path: String,
    len: u64,
    mtime_nanos: u128,
    ctime_nanos: i128,
    dev: u64,
    ino: u64,
    extra: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionListRowCacheEntry {
    key: SessionListCacheKey,
    row: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SessionLineageMetadata {
    parent_id: Option<String>,
    relationship: Option<String>,
    thread_source: Option<String>,
    agent_nickname: Option<String>,
}

impl SessionLineageMetadata {
    pub(crate) fn merge_missing_from(&mut self, other: SessionLineageMetadata) {
        if self.parent_id.is_none() {
            self.parent_id = other.parent_id;
        }
        if self.relationship.is_none() {
            self.relationship = other.relationship;
        }
        if self.thread_source.is_none() {
            self.thread_source = other.thread_source;
        }
        if self.agent_nickname.is_none() {
            self.agent_nickname = other.agent_nickname;
        }
    }

    pub(crate) fn apply_to_session_json(&self, session: &mut serde_json::Value) {
        let Some(obj) = session.as_object_mut() else {
            return;
        };
        if let Some(parent_id) = self.parent_id.as_deref().filter(|s| !s.is_empty()) {
            obj.insert(
                "parent_session_id".to_string(),
                serde_json::Value::String(parent_id.to_string()),
            );
            obj.insert(
                "parent_id".to_string(),
                serde_json::Value::String(parent_id.to_string()),
            );
        }
        if let Some(relationship) = self.relationship.as_deref().filter(|s| !s.is_empty()) {
            obj.insert(
                "relationship_kind".to_string(),
                serde_json::Value::String(relationship.to_string()),
            );
            obj.insert(
                "relationship".to_string(),
                serde_json::Value::String(relationship.to_string()),
            );
        }
        if let Some(thread_source) = self.thread_source.as_deref().filter(|s| !s.is_empty()) {
            obj.insert(
                "thread_source".to_string(),
                serde_json::Value::String(thread_source.to_string()),
            );
        }
        if let Some(agent_nickname) = self.agent_nickname.as_deref().filter(|s| !s.is_empty()) {
            obj.insert(
                "agent_nickname".to_string(),
                serde_json::Value::String(agent_nickname.to_string()),
            );
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CodexSessionListSummary {
    id: String,
    created_at: Option<String>,
    session_cwd: Option<String>,
    effective_cwd: Option<String>,
    model: Option<String>,
    lineage: SessionLineageMetadata,
    provider: Option<String>,
    usage: SessionUsage,
    // First usage event after the last in-file counter reset. For a forked
    // session its cumulative reading still contains the parent's history;
    // keeping just this event lets daily buckets be re-baselined without
    // retaining the full per-request event history (which made the resident
    // summary cache scale with transcript length, not session count).
    #[serde(default)]
    first_usage_event: Option<CodexUsageEvent>,
    daily_usage: BTreeMap<String, SessionUsage>,
    goal: Option<SessionGoal>,
    task: Option<String>,
    turns: u64,
    file_updated_at: Option<String>,
    bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CodexUsageEvent {
    timestamp: Option<String>,
    usage: SessionUsage,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexSessionListCacheEntry {
    key: SessionListCacheKey,
    summary: CodexSessionListSummary,
}

#[derive(Clone, Debug)]
pub(crate) struct CodexParentUsageBaselineCacheEntry {
    key: SessionListCacheKey,
    usage: Option<SessionUsage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionDirFingerprint {
    path: String,
    // SHA-256 over the sorted per-file stat records of the session dir
    // (see `session_file_fingerprints_digest`). Only equality is ever
    // needed for validation, and busy session dirs hold thousands of turn
    // files — retaining the full record list made the resident row cache
    // scale with turn count instead of session count.
    digest: String,
}

#[derive(Debug)]
pub(crate) struct SessionFileFingerprint {
    rel: String,
    mtime_nanos: u128,
    ctime_nanos: i128,
    len: u64,
    dev: u64,
    ino: u64,
    is_dir: bool,
}

mod string_u128 {
    pub fn serialize<S: serde::Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let raw = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

mod string_i128 {
    pub fn serialize<S: serde::Serializer>(v: &i128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i128, D::Error> {
        let raw = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct IntendantSessionListCacheEntry {
    fingerprint: SessionDirFingerprint,
    row: serde_json::Value,
}

pub(crate) const EXTERNAL_ACTIVITY_REPLAY_LIMIT: usize = WEBSOCKET_BOOTSTRAP_REPLAY_ENTRY_LIMIT;

pub(crate) const EXTERNAL_SESSION_DETAIL_DEFAULT_ENTRY_LIMIT: usize =
    SESSION_DETAIL_ENTRY_LIMIT_MAX;

pub(crate) const EXTERNAL_TRANSCRIPT_SEMANTICS: &str = "full_audit_transcript";

/// Spawn the web gateway HTTP/WebSocket server.
///
/// - `GET /config` returns a JSON `WebGatewayConfig` (voice/runtime only).
/// - `GET /.well-known/agent-card.json` returns a JSON `AgentCard` with
///   this daemon's identity, capabilities, transports, and auth scheme.
/// - `GET /icon-128.png` and `GET /favicon.ico` return the dashboard icon.
/// - `GET /` (and any other path) returns the web TUI page.
/// - WebSocket connections are bridged to the EventBus (inbound control
///   messages) and broadcast channel (outbound events), mirroring the
///   Unix control socket in `control.rs`.
///
/// Scan session.jsonl for persisted provider/model/autonomy values.
///
/// The agent loop writes these as plain log entries at startup
/// (`Provider: X`, `Model: Y`, `Autonomy: Z`).  Today the writer uses
/// `l.debug(...)`, so event_type is `debug` for newer sessions and
/// `info` for older ones — scan both.  Replay uses the result to seed
/// the status bar before any events are rendered, replacing the old
/// prefix-based parsing inside `handle_log_replay`.
pub(crate) fn scan_replay_status(
    contents: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut autonomy: Option<String> = None;
    for line in contents.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ev = v.get("event").and_then(|x| x.as_str()).unwrap_or("");
        if !matches!(ev, "info" | "debug" | "warn" | "error") {
            continue;
        }
        let Some(msg) = v.get("message").and_then(|x| x.as_str()) else {
            continue;
        };
        if provider.is_none() {
            if let Some(rest) = msg.strip_prefix("Provider: ") {
                provider = Some(rest.split_whitespace().next().unwrap_or("").to_string());
            }
        }
        if model.is_none() {
            if let Some(rest) = msg.strip_prefix("Model: ") {
                model = Some(rest.to_string());
            }
        }
        if autonomy.is_none() {
            if let Some(rest) = msg.strip_prefix("Autonomy: ") {
                autonomy = Some(rest.to_string());
            }
        }
        if provider.is_some() && model.is_some() && autonomy.is_some() {
            break;
        }
    }
    (provider, model, autonomy)
}

/// Convert session.jsonl contents into a stream of OutboundEvent-shaped
/// JSON objects ready to be sent as a `log_replay` message.
///
/// The first entry is always a `replay_start` marker carrying
/// provider/model/autonomy so the WASM `handle_log_replay` can seed the
/// status bar.  Subsequent entries are the result of running each JSONL
/// row through `session_log_entry_to_app_event` → `app_event_to_outbound`
/// and injecting the original `ts` field, so replay drives the exact
/// same rendering path as live broadcast.
#[allow(dead_code)]
pub(crate) fn replay_jsonl_to_outbound_entries(
    contents: &str,
    log_dir: &std::path::Path,
) -> Vec<serde_json::Value> {
    replay_jsonl_to_outbound_entries_inner(contents, log_dir, false)
}

pub(crate) fn replay_jsonl_to_browser_entries(
    contents: &str,
    log_dir: &std::path::Path,
) -> Vec<serde_json::Value> {
    replay_jsonl_to_outbound_entries_inner(contents, log_dir, true)
}

pub(crate) fn replay_jsonl_to_outbound_entries_inner(
    contents: &str,
    log_dir: &std::path::Path,
    compact_historical_context: bool,
) -> Vec<serde_json::Value> {
    let (provider, model, autonomy) = scan_replay_status(contents);
    let external_replay_session = external_backend_session_from_replay(contents);
    let external_replay_session_id = external_replay_session
        .as_ref()
        .map(|(_, session_id)| session_id.clone());
    let wrapper_replay_session_id = replay_session_id_from_dir(log_dir);
    let replay_session_id = external_replay_session_id
        .clone()
        .or_else(|| wrapper_replay_session_id.clone());
    let context_files_to_load = if compact_historical_context {
        latest_context_snapshot_files_by_session(contents, replay_session_id.as_deref())
    } else {
        HashSet::new()
    };

    let mut entries: Vec<serde_json::Value> = Vec::new();
    entries.push(serde_json::json!({
        "event": "replay_start",
        "provider": provider,
        "model": model,
        "autonomy": autonomy,
        "event_id": format!(
            "session-log:{}:replay_start",
            replay_session_id.as_deref().unwrap_or("unknown")
        ),
        "delivery": "state",
    }));
    if let (Some((source, backend_session_id)), Some(wrapper_session_id)) = (
        external_replay_session.as_ref(),
        wrapper_replay_session_id.as_ref(),
    ) {
        if !source.is_empty()
            && source != "intendant"
            && !backend_session_id.is_empty()
            && backend_session_id != wrapper_session_id
        {
            entries.push(serde_json::json!({
                "event": "session_identity",
                "session_id": wrapper_session_id,
                "source": source,
                "backend_session_id": backend_session_id,
                "event_id": format!(
                    "session-log:{wrapper_session_id}:session_identity:{backend_session_id}"
                ),
                "delivery": "state",
            }));
        }
    }

    let legacy_model_spans = validated_legacy_model_response_spans(contents, log_dir);
    let mut legacy_model_indices: HashMap<String, usize> = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(mut entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        infer_legacy_model_response_span(
            &mut entry_json,
            &legacy_model_spans,
            &mut legacy_model_indices,
        );
        if compact_historical_context
            && entry_json.get("event").and_then(|v| v.as_str()) == Some("context_snapshot")
        {
            let file = entry_json.get("file").and_then(|v| v.as_str());
            if !file.is_some_and(|file| context_files_to_load.contains(file))
                || context_snapshot_raw_file_too_large_for_replay(&entry_json, log_dir)
            {
                let Some(mut value) = context_snapshot_replay_entry_without_raw(
                    &entry_json,
                    replay_session_id.as_deref(),
                ) else {
                    continue;
                };
                inject_replay_entry_metadata(
                    &mut value,
                    &entry_json,
                    replay_session_id.as_deref(),
                    external_replay_session_id.as_deref(),
                    wrapper_replay_session_id.as_deref(),
                );
                entries.push(value);
                continue;
            }
        }

        let Some(app_event) =
            crate::session_log::session_log_entry_to_app_event(&entry_json, log_dir)
        else {
            continue;
        };
        let Some(outbound) = crate::event::app_event_to_outbound(&app_event) else {
            continue;
        };
        let Ok(mut value) = serde_json::to_value(&outbound) else {
            continue;
        };
        if compact_historical_context {
            compact_context_snapshot_raw_for_replay(&mut value);
        }
        inject_replay_entry_metadata(
            &mut value,
            &entry_json,
            replay_session_id.as_deref(),
            external_replay_session_id.as_deref(),
            wrapper_replay_session_id.as_deref(),
        );
        entries.push(value);
    }

    entries
}

pub(crate) fn legacy_model_response_file_and_len(
    entry_json: &serde_json::Value,
) -> Option<(String, u64)> {
    if entry_json.get("event").and_then(|v| v.as_str()) != Some("model_response") {
        return None;
    }
    let rel = entry_json.get("file")?.as_str()?.to_string();
    let data = entry_json.get("data")?;
    if data
        .get("model_offset")
        .and_then(|value| value.as_u64())
        .is_some()
        || data
            .get("model_bytes")
            .and_then(|value| value.as_u64())
            .is_some()
    {
        return None;
    }
    let len = data.get("content_length")?.as_u64()?;
    Some((rel, len))
}

pub(crate) fn validated_legacy_model_response_spans(
    contents: &str,
    log_dir: &std::path::Path,
) -> HashMap<String, Vec<(u64, u64)>> {
    let mut lengths_by_file: HashMap<String, Vec<u64>> = HashMap::new();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some((rel, len)) = legacy_model_response_file_and_len(&entry_json) {
            lengths_by_file.entry(rel).or_default().push(len);
        }
    }

    let mut spans_by_file: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
    for (rel, lengths) in lengths_by_file {
        let Ok(meta) = std::fs::metadata(log_dir.join(&rel)) else {
            continue;
        };
        let mut expected_len = lengths.len().saturating_sub(1) as u64;
        let mut overflowed = false;
        for len in &lengths {
            let Some(next) = expected_len.checked_add(*len) else {
                overflowed = true;
                break;
            };
            expected_len = next;
        }
        if overflowed || expected_len != meta.len() {
            continue;
        }

        let mut offset = 0_u64;
        let mut spans = Vec::with_capacity(lengths.len());
        for len in lengths {
            spans.push((offset, len));
            offset = offset.saturating_add(len).saturating_add(1);
        }
        spans_by_file.insert(rel, spans);
    }
    spans_by_file
}

pub(crate) fn infer_legacy_model_response_span(
    entry_json: &mut serde_json::Value,
    spans_by_file: &HashMap<String, Vec<(u64, u64)>>,
    indices: &mut HashMap<String, usize>,
) {
    let Some((rel, _len)) = legacy_model_response_file_and_len(entry_json) else {
        return;
    };
    let index = indices.entry(rel.clone()).or_insert(0);
    let Some((offset, len)) = spans_by_file
        .get(&rel)
        .and_then(|spans| spans.get(*index))
        .copied()
    else {
        return;
    };
    *index += 1;
    let Some(data) = entry_json
        .get_mut("data")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    data.insert("model_offset".to_string(), serde_json::Value::from(offset));
    data.insert("model_bytes".to_string(), serde_json::Value::from(len));
}

pub(crate) fn inject_replay_entry_metadata(
    value: &mut serde_json::Value,
    entry_json: &serde_json::Value,
    replay_session_id: Option<&str>,
    external_replay_session_id: Option<&str>,
    wrapper_replay_session_id: Option<&str>,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let ts = entry_json
        .get("ts")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    obj.insert("ts".to_string(), serde_json::Value::String(ts));
    if let Some(ts_ms) = timestamp_millis_from_str(
        obj.get("ts")
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
    ) {
        obj.insert("ts_ms".to_string(), serde_json::Value::from(ts_ms));
    }
    if !obj.contains_key("event_id") {
        obj.insert(
            "event_id".to_string(),
            serde_json::Value::String(session_log_replay_event_id(entry_json, replay_session_id)),
        );
    }
    if !obj.contains_key("delivery") {
        let delivery = delivery_class_for_replay_object(obj);
        obj.insert(
            "delivery".to_string(),
            serde_json::Value::String(delivery.to_string()),
        );
    }
    if obj.get("event").and_then(|v| v.as_str()) == Some("context_snapshot") {
        if let Some(file) = entry_json.get("file").and_then(|v| v.as_str()) {
            obj.insert(
                "snapshot_file".to_string(),
                serde_json::Value::String(file.to_string()),
            );
            obj.insert(
                "exact_replay_available".to_string(),
                serde_json::Value::Bool(true),
            );
            if let Some(raw) = obj.get_mut("raw") {
                annotate_context_snapshot_raw_value_exact_replay(raw, file);
            }
        }
    }
    if !obj.contains_key("session_id") {
        if let Some(session_id) = replay_session_id {
            obj.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
        }
    } else if let (Some(external_id), Some(wrapper_id)) =
        (external_replay_session_id, wrapper_replay_session_id)
    {
        let event = obj.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = obj.get("session_id").and_then(|v| v.as_str());
        if event != "session_identity" && session_id == Some(wrapper_id) {
            obj.insert(
                "session_id".to_string(),
                serde_json::Value::String(external_id.to_string()),
            );
        }
    }
}

pub(crate) fn timestamp_millis_from_str(value: &str) -> Option<i64> {
    let raw = value.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp_millis());
    }

    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, fmt) {
            use chrono::TimeZone as _;
            return chrono::Local
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.timestamp_millis())
                .or_else(|| Some(naive.and_utc().timestamp_millis()));
        }
    }

    let raw_with_year = format!("{}-{raw}", chrono::Local::now().format("%Y"));
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&raw_with_year, fmt) {
            use chrono::TimeZone as _;
            return chrono::Local
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.timestamp_millis())
                .or_else(|| Some(naive.and_utc().timestamp_millis()));
        }
    }

    for fmt in ["%H:%M:%S%.f", "%H:%M"] {
        if let Ok(naive_time) = chrono::NaiveTime::parse_from_str(raw, fmt) {
            let date = chrono::Local::now().date_naive();
            let naive = date.and_time(naive_time);
            use chrono::TimeZone as _;
            return chrono::Local
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.timestamp_millis())
                .or_else(|| Some(naive.and_utc().timestamp_millis()));
        }
    }

    None
}

pub(crate) fn short_stable_hash(parts: &[&str]) -> String {
    let mut input = String::new();
    for part in parts {
        input.push_str(part);
        input.push('\0');
    }
    let hash = crate::file_watcher::hex_encode(&crate::file_watcher::sha256_hash(input.as_bytes()));
    hash.chars().take(16).collect()
}

pub(crate) fn delivery_class_for_replay_object(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> &'static str {
    let event = obj.get("event").and_then(|v| v.as_str()).unwrap_or("");
    let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let item_type = obj
        .get("item_type")
        .or_else(|| obj.get("itemType"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match (event, kind, item_type) {
        (
            "log_entry"
            | "model_response"
            | "agent_output"
            | "agent_started"
            | "presence_log"
            | "context_snapshot"
            | "thread_history_change",
            _,
            _,
        ) => "lossless",
        (_, "agent_output" | "rollback_marker", _) => "lossless",
        (_, _, "command_execution" | "user_message" | "agent_message") => "lossless",
        _ => "state",
    }
}

pub(crate) fn session_log_replay_event_id(
    entry_json: &serde_json::Value,
    replay_session_id: Option<&str>,
) -> String {
    if let Some(id) = entry_json
        .get("event_id")
        .or_else(|| entry_json.get("eventId"))
        .or_else(|| entry_json.pointer("/data/event_id"))
        .or_else(|| entry_json.pointer("/data/eventId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return id.to_string();
    }

    let event = entry_json
        .get("event")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let ts = entry_json
        .get("ts")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let file = entry_json
        .get("file")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let model_offset = entry_json
        .pointer("/data/model_offset")
        .and_then(|value| value.as_u64())
        .map(|value| value.to_string())
        .unwrap_or_default();
    let model_bytes = entry_json
        .pointer("/data/model_bytes")
        .and_then(|value| value.as_u64())
        .map(|value| value.to_string())
        .unwrap_or_default();
    let output_id = entry_json
        .pointer("/data/output_id")
        .or_else(|| entry_json.pointer("/data/outputId"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let seq = entry_json
        .get("seq")
        .or_else(|| entry_json.get("sequence"))
        .and_then(|value| value.as_u64())
        .map(|value| value.to_string())
        .unwrap_or_default();

    if !file.is_empty() || !model_offset.is_empty() || !output_id.is_empty() || !seq.is_empty() {
        return format!(
            "session-log:{}",
            short_stable_hash(&[
                replay_session_id.unwrap_or(""),
                event,
                ts,
                file,
                &model_offset,
                &model_bytes,
                output_id,
                &seq,
            ])
        );
    }

    let normalized = serde_json::to_string(entry_json).unwrap_or_default();
    format!(
        "session-log:{}",
        short_stable_hash(&[replay_session_id.unwrap_or(""), event, ts, &normalized])
    )
}

pub(crate) fn latest_context_snapshot_files_by_session(
    contents: &str,
    replay_session_id: Option<&str>,
) -> HashSet<String> {
    let mut latest_by_session = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry_json.get("event").and_then(|v| v.as_str()) != Some("context_snapshot") {
            continue;
        }
        let Some(file) = entry_json.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let session_id = entry_json
            .get("data")
            .and_then(|data| data.get("session_id"))
            .and_then(|v| v.as_str())
            .or(replay_session_id)
            .unwrap_or("__global__");
        latest_by_session.insert(session_id.to_string(), file.to_string());
    }
    latest_by_session.into_values().collect()
}

pub(crate) fn context_snapshot_raw_file_size(
    entry_json: &serde_json::Value,
    log_dir: &Path,
) -> Option<u64> {
    let file = entry_json.get("file").and_then(|v| v.as_str())?;
    if file.trim().is_empty() {
        return None;
    }
    let relative = Path::new(file);
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    std::fs::metadata(log_dir.join(relative))
        .ok()
        .map(|m| m.len())
}

pub(crate) fn context_snapshot_raw_file_too_large_for_replay(
    entry_json: &serde_json::Value,
    log_dir: &Path,
) -> bool {
    context_snapshot_raw_file_size(entry_json, log_dir)
        .is_some_and(|bytes| bytes > CONTEXT_REPLAY_RAW_SUMMARY_MAX_BYTES)
}

pub(crate) fn context_snapshot_replay_entry_from_log_entry(
    entry_json: &serde_json::Value,
    log_dir: &Path,
    replay_session_id: Option<&str>,
    external_replay_session_id: Option<&str>,
    wrapper_replay_session_id: Option<&str>,
) -> Option<serde_json::Value> {
    if context_snapshot_raw_file_too_large_for_replay(entry_json, log_dir) {
        let mut value = context_snapshot_replay_entry_without_raw(entry_json, replay_session_id)?;
        inject_replay_entry_metadata(
            &mut value,
            entry_json,
            replay_session_id,
            external_replay_session_id,
            wrapper_replay_session_id,
        );
        return Some(value);
    }

    let app_event = crate::session_log::session_log_entry_to_app_event(entry_json, log_dir)?;
    let outbound = crate::event::app_event_to_outbound(&app_event)?;
    let mut value = serde_json::to_value(&outbound).ok()?;
    compact_context_snapshot_raw_for_replay(&mut value);
    inject_replay_entry_metadata(
        &mut value,
        entry_json,
        replay_session_id,
        external_replay_session_id,
        wrapper_replay_session_id,
    );
    Some(value)
}

pub(crate) fn context_snapshot_replay_entry_without_raw(
    entry_json: &serde_json::Value,
    replay_session_id: Option<&str>,
) -> Option<serde_json::Value> {
    let data = entry_json.get("data");
    let source = data
        .and_then(|d| d.get("source"))
        .and_then(|v| v.as_str())
        .unwrap_or("model")
        .to_string();
    let label = data
        .and_then(|d| d.get("label"))
        .and_then(|v| v.as_str())
        .unwrap_or("Model context")
        .to_string();
    let format = data
        .and_then(|d| d.get("format"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let request_id = data
        .and_then(|d| d.get("request_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let request_index = data
        .and_then(|d| d.get("request_index"))
        .and_then(|v| v.as_u64());
    let token_count = data
        .and_then(|d| d.get("token_count"))
        .and_then(|v| v.as_u64());
    let token_count_kind = data
        .and_then(|d| d.get("token_count_kind"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let context_window = data
        .and_then(|d| d.get("context_window"))
        .and_then(|v| v.as_u64());
    let hard_context_window = data
        .and_then(|d| d.get("hard_context_window"))
        .and_then(|v| v.as_u64());
    let item_count = data
        .and_then(|d| d.get("item_count"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let session_id = data
        .and_then(|d| d.get("session_id"))
        .and_then(|v| v.as_str())
        .or(replay_session_id)
        .map(|s| s.to_string());
    let turn = entry_json
        .get("turn")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let raw = context_snapshot_omitted_raw(
        request_id.as_deref(),
        request_index,
        format.as_str(),
        item_count,
        entry_json.get("file").and_then(|v| v.as_str()),
    );
    serde_json::to_value(crate::types::OutboundEvent::ContextSnapshot {
        session_id,
        source,
        label,
        request_id,
        request_index,
        turn,
        format,
        token_count,
        token_count_kind,
        context_window,
        hard_context_window,
        item_count,
        raw,
    })
    .ok()
}

pub(crate) fn context_snapshot_omitted_raw(
    request_id: Option<&str>,
    request_index: Option<u64>,
    format: &str,
    item_count: Option<usize>,
    snapshot_file: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "_intendant_context": {
            "archive_mode": "summary",
            "raw_archived": true,
            "raw_omitted": true,
            "exact_replay_available": snapshot_file.is_some(),
            "snapshot_file": snapshot_file,
            "request_id": request_id,
            "request_index": request_index,
            "format": format,
        },
        "summary": {
            "kind": "compact_context_snapshot",
            "raw_omitted": true,
            "exact_replay_available": snapshot_file.is_some(),
            "part_count": 0,
            "item_count": item_count,
        },
        "summary_parts": [],
    })
}

pub(crate) fn external_backend_session_from_replay(contents: &str) -> Option<(String, String)> {
    let mut found_source: Option<String> = None;
    let mut found_id: Option<String> = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry_json.get("event").and_then(|v| v.as_str()) == Some("session_identity") {
            let data = entry_json.get("data");
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .map(crate::session_names::normalize_source)
                .unwrap_or_default();
            if !source.is_empty() && source != "intendant" {
                if let Some(id) = data
                    .and_then(|d| d.get("backend_session_id"))
                    .and_then(|v| v.as_str())
                    .and_then(clean_external_thread_id)
                {
                    return Some((source, id));
                }
            }
        }
        if let Some(message) = entry_json.get("message").and_then(|v| v.as_str()) {
            if found_source.is_none() {
                found_source = external_agent_source_from_message(message);
            }
            if found_id.is_none() {
                found_id = external_agent_thread_id_from_message(message);
            }
            if let (Some(source), Some(id)) = (found_source.as_ref(), found_id.as_ref()) {
                return Some((source.clone(), id.clone()));
            }
        }
    }
    None
}

/// Debug lines log placeholder thread ids (Claude Code's
/// `claude-code-session` before the stream announces the real one).
/// Scraping those into a replay session id stamps every session-less row
/// with a session that never exists — frontends then materialize a ghost
/// window for it and can even hand it the prompt target.
pub(crate) fn scraped_external_thread_id_is_canonical(id: &str) -> bool {
    crate::external_agent::AgentBackend::ClaudeCode.thread_id_is_canonical(id)
}

pub(crate) fn external_backend_session_id_from_replay(contents: &str) -> Option<String> {
    if let Some((_, id)) = external_backend_session_from_replay(contents) {
        return Some(id);
    }
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(message) = entry_json.get("message").and_then(|v| v.as_str()) {
            if let Some(id) = external_agent_thread_id_from_message(message) {
                return Some(id);
            }
        }
    }
    None
}

pub(crate) fn home_from_intendant_log_dir(log_dir: &std::path::Path) -> Option<PathBuf> {
    let logs_dir = log_dir.parent()?;
    if logs_dir.file_name().and_then(|name| name.to_str()) != Some("logs") {
        return None;
    }
    let intendant_dir = logs_dir.parent()?;
    if intendant_dir.file_name().and_then(|name| name.to_str()) != Some(".intendant") {
        return None;
    }
    intendant_dir.parent().map(Path::to_path_buf)
}

pub(crate) fn annotate_replay_user_turns_from_external_transcript(
    entries: &mut [serde_json::Value],
    home: &Path,
    source: &str,
    session_id: &str,
) {
    let Some(transcript) = external_session_entries_from_home(home, source, session_id) else {
        return;
    };
    let user_turns: Vec<serde_json::Value> = transcript
        .into_iter()
        .filter(|entry| entry.get("source").and_then(|v| v.as_str()) == Some("user"))
        .filter(|entry| {
            entry
                .get("user_turn_index")
                .and_then(|v| v.as_u64())
                .is_some()
                && entry
                    .get("user_turn_revision")
                    .and_then(|v| v.as_u64())
                    .is_some()
        })
        .collect();
    if user_turns.is_empty() {
        return;
    }

    let mut next_user_turn = 0usize;
    for entry in entries {
        if entry.get("event").and_then(|v| v.as_str()) != Some("log_entry") {
            continue;
        }
        if entry.get("session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        let source = entry
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if source != "user" {
            continue;
        }
        let content = entry
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            continue;
        }

        let Some((matched_offset, turn)) =
            user_turns[next_user_turn..]
                .iter()
                .enumerate()
                .find(|(_, turn)| {
                    turn.get("content")
                        .and_then(|v| v.as_str())
                        .map(|candidate| candidate.trim() == content)
                        .unwrap_or(false)
                })
        else {
            continue;
        };
        next_user_turn += matched_offset + 1;

        if let Some(obj) = entry.as_object_mut() {
            for key in [
                "user_turn_index",
                "user_turn_revision",
                "replacement_for_user_turn_index",
                "superseded",
                "superseded_reason",
            ] {
                if let Some(value) = turn.get(key) {
                    obj.insert(key.to_string(), value.clone());
                }
            }
        }
    }
}

pub(crate) fn session_log_replay_entries_from_dir(
    log_dir: &std::path::Path,
) -> Option<(Vec<serde_json::Value>, Option<String>)> {
    let session_jsonl = log_dir.join("session.jsonl");
    let contents = std::fs::read_to_string(&session_jsonl).ok()?;
    let external_session = external_backend_session_from_replay(&contents);
    let external_session_id = external_session
        .as_ref()
        .map(|(_, id)| id.clone())
        .or_else(|| external_backend_session_id_from_replay(&contents));
    let mut entries = replay_jsonl_to_browser_entries(&contents, log_dir);
    if let Some((source, session_id)) = external_session.as_ref() {
        let home = home_from_intendant_log_dir(log_dir).unwrap_or_else(crate::platform::home_dir);
        annotate_replay_user_turns_from_external_transcript(
            &mut entries,
            &home,
            source,
            session_id,
        );
    }
    Some((entries, external_session_id))
}

pub(crate) fn context_snapshot_raw_is_compact(raw: &serde_json::Value) -> bool {
    raw.pointer("/_intendant_context/archive_mode")
        .and_then(|v| v.as_str())
        == Some("summary")
        || raw.pointer("/summary/kind").and_then(|v| v.as_str()) == Some("compact_context_snapshot")
        || raw.get("summary_parts").is_some()
}

pub(crate) fn context_snapshot_raw_replay_size(raw: &serde_json::Value) -> usize {
    serde_json::to_vec(raw)
        .map(|bytes| bytes.len())
        .unwrap_or_else(|_| raw.to_string().len())
}

pub(crate) fn compact_context_snapshot_raw_for_replay(entry: &mut serde_json::Value) {
    if entry.get("event").and_then(|v| v.as_str()) != Some("context_snapshot") {
        return;
    }
    let snapshot_file = entry
        .get("snapshot_file")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let request_id = entry
        .get("request_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("replay")
        .to_string();
    let request_index = entry
        .get("request_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let format = entry
        .get("format")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();
    let item_count = entry
        .get("item_count")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let should_omit_compact_raw = snapshot_file.is_some()
        && entry.get("raw").is_some_and(|raw| {
            !raw.is_null()
                && context_snapshot_raw_is_compact(raw)
                && context_snapshot_raw_replay_size(raw)
                    > CONTEXT_REPLAY_RAW_SUMMARY_MAX_BYTES as usize
        });
    if should_omit_compact_raw {
        if let Some(raw) = entry.get_mut("raw") {
            *raw = context_snapshot_omitted_raw(
                Some(request_id.as_str()),
                Some(request_index),
                format.as_str(),
                item_count,
                snapshot_file.as_deref(),
            );
        }
        if let Some(snapshot_file) = snapshot_file.as_deref() {
            annotate_context_snapshot_raw_exact_replay(entry, snapshot_file);
        }
        return;
    }

    let Some(raw) = entry.get_mut("raw") else {
        return;
    };
    if raw.is_null() || context_snapshot_raw_is_compact(raw) {
        return;
    }

    let raw_payload = std::mem::take(raw);
    *raw = crate::external_agent::codex::codex_context_archive_payload(
        raw_payload,
        &request_id,
        request_index,
        &format,
        false,
    );
    if let Some(snapshot_file) = snapshot_file.as_deref() {
        annotate_context_snapshot_raw_exact_replay(entry, snapshot_file);
    }
}

pub(crate) fn compact_context_snapshot_entries_for_replay(entries: &mut [serde_json::Value]) {
    for entry in entries {
        compact_context_snapshot_raw_for_replay(entry);
    }
}

pub(crate) fn truncate_string_to_utf8_byte_limit(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = value[..end].to_string();
    out.push_str("...");
    out
}

pub(crate) fn compact_replay_entry_text_fields_for_websocket(entry: &mut serde_json::Value) {
    let Some(obj) = entry.as_object_mut() else {
        return;
    };
    let is_agent_output = obj.get("event").and_then(|v| v.as_str()) == Some("agent_output")
        || obj.get("kind").and_then(|v| v.as_str()) == Some("agent_output")
        || obj.contains_key("output_id")
        || obj.contains_key("outputId");
    let mut truncated_fields = Vec::new();
    let mut full_output_bytes = 0usize;
    let mut full_output_lines = 0usize;
    for key in [
        "content",
        "stdout",
        "stderr",
        "message",
        "summary",
        "reasoning_summary",
        "reasoningSummary",
    ] {
        let Some(value) = obj.get_mut(key) else {
            continue;
        };
        let Some(text) = value.as_str() else {
            continue;
        };
        if text.len() > WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES {
            truncated_fields.push(key);
            if is_agent_output && matches!(key, "content" | "stdout" | "stderr") {
                full_output_bytes = full_output_bytes.saturating_add(text.len());
                full_output_lines = full_output_lines.saturating_add(text.lines().count().max(1));
            }
            *value = serde_json::Value::String(truncate_string_to_utf8_byte_limit(
                text,
                WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES,
            ));
        }
    }
    if truncated_fields.is_empty() {
        return;
    }
    obj.insert("text_truncated".to_string(), serde_json::json!(true));
    obj.insert(
        "truncated_fields".to_string(),
        serde_json::json!(truncated_fields),
    );
    if is_agent_output {
        obj.insert("full_output_available".to_string(), serde_json::json!(true));
        if full_output_bytes > 0 {
            obj.insert(
                "full_output_bytes".to_string(),
                serde_json::json!(full_output_bytes),
            );
        }
        if full_output_lines > 0 {
            obj.insert(
                "full_output_lines".to_string(),
                serde_json::json!(full_output_lines),
            );
        }
    }
}

pub(crate) fn prepare_websocket_bootstrap_replay_entries(
    mut entries: Vec<serde_json::Value>,
    limit: usize,
) -> Vec<serde_json::Value> {
    entries.retain(|entry| entry.get("event").and_then(|v| v.as_str()) != Some("context_snapshot"));
    entries = limited_session_detail_entries(entries, Some(limit));
    for entry in &mut entries {
        compact_replay_entry_text_fields_for_websocket(entry);
    }
    entries
}

pub(crate) fn annotate_context_snapshot_raw_exact_replay(
    entry: &mut serde_json::Value,
    snapshot_file: &str,
) {
    let Some(raw) = entry.get_mut("raw") else {
        return;
    };
    annotate_context_snapshot_raw_value_exact_replay(raw, snapshot_file);
}

pub(crate) fn annotate_context_snapshot_raw_value_exact_replay(
    raw: &mut serde_json::Value,
    snapshot_file: &str,
) {
    if let Some(context) = raw
        .get_mut("_intendant_context")
        .and_then(|value| value.as_object_mut())
    {
        context.insert(
            "snapshot_file".to_string(),
            serde_json::Value::String(snapshot_file.to_string()),
        );
        context.insert(
            "exact_replay_available".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if let Some(summary) = raw
        .get_mut("summary")
        .and_then(|value| value.as_object_mut())
    {
        summary.insert(
            "exact_replay_available".to_string(),
            serde_json::Value::Bool(true),
        );
    }
}

#[allow(dead_code)]
pub(crate) fn session_log_replay_payload_from_dir(
    log_dir: &std::path::Path,
) -> Option<(String, Option<String>)> {
    session_log_replay_payload_from_dir_with_limit(log_dir, None)
}

pub(crate) fn session_log_replay_payload_from_dir_with_limit(
    log_dir: &std::path::Path,
    limit: Option<usize>,
) -> Option<(String, Option<String>)> {
    let (mut entries, external_session_id) = session_log_replay_entries_from_dir(log_dir)?;
    if let Some(limit) = limit {
        entries = prepare_websocket_bootstrap_replay_entries(entries, limit);
    }
    compact_context_snapshot_entries_for_replay(&mut entries);
    Some((
        serde_json::json!({
            "t": "log_replay",
            "entries": entries,
        })
        .to_string(),
        external_session_id,
    ))
}

pub(crate) fn session_log_replay_payload_for_websocket_bootstrap(
    log_dir: &std::path::Path,
) -> Option<(String, Option<String>)> {
    session_log_replay_payload_from_dir_with_limit(
        log_dir,
        Some(WEBSOCKET_BOOTSTRAP_REPLAY_ENTRY_LIMIT),
    )
}

pub(crate) fn limited_session_detail_entries(
    entries: Vec<serde_json::Value>,
    limit: Option<usize>,
) -> Vec<serde_json::Value> {
    session_detail_page_entries(entries, limit, None).entries
}

pub(crate) struct SessionDetailPageEntries {
    entries: Vec<serde_json::Value>,
    total_entries: usize,
    page_start: usize,
    page_end: usize,
}

pub(crate) fn session_detail_page_entries(
    entries: Vec<serde_json::Value>,
    limit: Option<usize>,
    before: Option<usize>,
) -> SessionDetailPageEntries {
    let total_entries = entries.len();
    let Some(limit) = limit else {
        if before.is_none() {
            return SessionDetailPageEntries {
                entries,
                total_entries,
                page_start: 0,
                page_end: total_entries,
            };
        }
        let end = before.unwrap_or(total_entries).min(total_entries);
        return SessionDetailPageEntries {
            entries: entries.into_iter().take(end).collect(),
            total_entries,
            page_start: 0,
            page_end: end,
        };
    };
    let limit = limit.clamp(1, SESSION_DETAIL_ENTRY_LIMIT_MAX);
    let page_end = before.unwrap_or(total_entries).min(total_entries);
    let page_start = page_end.saturating_sub(limit);
    if total_entries <= limit && before.is_none() {
        return SessionDetailPageEntries {
            entries,
            total_entries,
            page_start: 0,
            page_end: total_entries,
        };
    }

    let mut keep = BTreeSet::new();
    let mut latest_goal_by_session: HashMap<String, usize> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        let event = entry.get("event").and_then(|v| v.as_str()).unwrap_or("");
        match event {
            "replay_start"
            | "session_identity"
            | "session_relationship"
            | "session_capabilities" => {
                keep.insert(idx);
            }
            "session_goal" => {
                let session_id = entry
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| entry.pointer("/data/session_id").and_then(|v| v.as_str()))
                    .unwrap_or("__global__");
                latest_goal_by_session.insert(session_id.to_string(), idx);
            }
            _ => {}
        }
    }
    keep.extend(latest_goal_by_session.into_values());
    keep.extend(page_start..page_end);

    let entries = entries
        .into_iter()
        .enumerate()
        .filter_map(|(idx, entry)| keep.contains(&idx).then_some(entry))
        .collect();

    SessionDetailPageEntries {
        entries,
        total_entries,
        page_start,
        page_end,
    }
}

pub(crate) fn session_detail_entry_limit_from_request(request_line: &str) -> Option<usize> {
    query_param(request_line, "limit")
        .or_else(|| query_param(request_line, "entry_limit"))
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .map(|limit| limit.min(SESSION_DETAIL_ENTRY_LIMIT_MAX))
}

pub(crate) fn session_detail_before_from_request(request_line: &str) -> Option<usize> {
    query_param(request_line, "before")
        .or_else(|| query_param(request_line, "page_before"))
        .or_else(|| query_param(request_line, "pageBefore"))
        .and_then(|raw| raw.trim().parse::<usize>().ok())
}

pub(crate) fn replay_session_id_from_dir(log_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(log_dir.join("session_meta.json"))
        .ok()
        .and_then(|meta| serde_json::from_str::<crate::session_log::SessionMeta>(&meta).ok())
        .map(|meta| meta.session_id)
        .filter(|session_id| !session_id.trim().is_empty())
        .or_else(|| {
            log_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|session_id| !session_id.trim().is_empty())
        })
}

pub(crate) fn session_log_id(
    session_log: &Arc<Mutex<crate::session_log::SessionLog>>,
) -> Option<String> {
    session_log
        .lock()
        .ok()
        .map(|log| log.session_id().to_string())
        .filter(|id| !id.trim().is_empty())
}

#[allow(dead_code)]
pub(crate) fn session_log_replay_from_dir(log_dir: &std::path::Path) -> Option<String> {
    session_log_replay_payload_from_dir(log_dir).map(|(payload, _)| payload)
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub(crate) fn session_log_mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path.join("session.jsonl"))
        .or_else(|_| std::fs::metadata(path))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

pub(crate) fn external_session_mtime(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<std::time::SystemTime> {
    let path = session_log_search_file_path(home, source, session_id, None)?;
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub(crate) fn external_session_newer_than_wrapper(
    home: &Path,
    wrapper_log_dir: &Path,
    source: &str,
    session_id: &str,
) -> bool {
    external_session_mtime(home, source, session_id)
        .is_some_and(|external_mtime| external_mtime > session_log_mtime(wrapper_log_dir))
}

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

    let logs_dir = home.join(".intendant").join("logs");
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
    project_root: Option<String>,
    cwd: Option<String>,
    source: Option<String>,
    source_label: Option<String>,
    name: Option<String>,
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
    if wrappers.is_empty() {
        return;
    }
    let Some(obj) = session.as_object_mut() else {
        return;
    };
    let latest = &wrappers[0];
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

    let logs_dir = home.join(".intendant").join("logs");

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

pub(crate) fn resolve_session_dir(session_id: &str) -> Option<PathBuf> {
    resolve_bare_session_dir_from_home(&crate::platform::home_dir(), session_id)
}

pub(crate) fn deleted_external_sessions_path(home: &Path) -> PathBuf {
    home.join(".intendant").join(DELETED_EXTERNAL_SESSIONS_FILE)
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

pub(crate) async fn recordings_list_response_body(
    recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
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

    let daemon_dir = crate::debug::daemon_recordings_dir();
    for entry in list_recording_streams(&daemon_dir) {
        all_entries.push(entry);
    }

    serde_json::to_string(&all_entries).unwrap_or("[]".to_string())
}

pub(crate) fn session_recordings_list_response_body(session_id: &str) -> (&'static str, String) {
    if !session_lookup_id_is_safe(session_id) {
        return (
            "400 Bad Request",
            serde_json::json!({ "error": "invalid session id" }).to_string(),
        );
    }
    let body = if let Some(session_dir) = resolve_session_dir(session_id) {
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
    let home = crate::platform::home_dir();
    if source == "intendant" {
        get_session_detail_from_home_with_page(&home, session_id, limit, before)
    } else {
        external_session_detail_from_home_with_page(&home, source, session_id, limit, before)
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

    let mut entries = session_log_replay_entries_from_dir(&session_dir)
        .map(|(entries, _)| entries)
        .unwrap_or_default();
    compact_context_snapshot_entries_for_replay(&mut entries);
    let page = session_detail_page_entries(entries, limit, before);
    let entries = page.entries;

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

    serde_json::json!({
        "session_id": session_dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "entries": entries,
        "total_entries": page.total_entries,
        "page_start": page.page_start,
        "page_end": page.page_end,
        "has_older": page.page_start > 0,
        "frames": frames,
        "relationships": session_relationships_from_log_dir(&session_dir),
    }).to_string()
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

pub(crate) fn context_snapshot_selector_from_request(
    request_line: &str,
) -> Result<ContextSnapshotSelector, String> {
    let file = query_param(request_line, "file").filter(|value| !value.trim().is_empty());
    let request_index =
        match query_param(request_line, "request_index").filter(|value| !value.trim().is_empty()) {
            Some(value) => Some(
                value
                    .parse::<u64>()
                    .map_err(|_| "invalid request_index".to_string())?,
            ),
            None => None,
        };
    context_snapshot_selector_from_parts(
        file,
        query_param(request_line, "request_id").filter(|value| !value.trim().is_empty()),
        request_index,
        query_param(request_line, "ts").filter(|value| !value.trim().is_empty()),
    )
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

pub(crate) fn get_session_context_snapshot_from_home(
    home: &Path,
    session_id: &str,
    source: &str,
    request_line: &str,
) -> (&'static str, String) {
    if !session_lookup_id_is_safe(session_id) {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "invalid session id"}).to_string(),
        );
    }
    let selector = match context_snapshot_selector_from_request(request_line) {
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

pub(crate) async fn sessions_search_response_body(
    query: String,
    source_filter: String,
    mode: String,
    project_filter: Vec<String>,
) -> String {
    sessions_search_response_body_with_cancel(
        query,
        source_filter,
        mode,
        project_filter,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
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
    let mut results = Vec::new();
    let mut searched = 0usize;

    for session in sessions {
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
    collect_recent_files(
        &home.join(".claude").join("projects"),
        ".jsonl",
        EXTERNAL_SESSION_SCAN_LIMIT,
    )
    .into_iter()
    .find(|path| path.file_stem().and_then(|n| n.to_str()) == Some(session_id))
}

pub(crate) fn find_gemini_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    collect_recent_files(
        &home.join(".gemini").join("tmp"),
        ".json",
        EXTERNAL_SESSION_SCAN_LIMIT,
    )
    .into_iter()
    .filter(|path| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("chats")
    })
    .find(|path| {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&contents)
            .ok()
            .and_then(|obj| value_str(&obj, "sessionId"))
            .as_deref()
            == Some(session_id)
    })
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
        .filter_map(Result::ok)
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

pub(crate) fn value_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

pub(crate) fn codex_exec_command_workdir(payload: &serde_json::Value) -> Option<String> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("function_call")
        || payload.get("name").and_then(|v| v.as_str()) != Some("exec_command")
    {
        return None;
    }

    let arguments = payload.get("arguments")?;
    let parsed_arguments;
    let arguments = if let Some(raw) = arguments.as_str() {
        parsed_arguments = serde_json::from_str::<serde_json::Value>(raw).ok()?;
        &parsed_arguments
    } else {
        arguments
    };

    value_str(arguments, "workdir")
        .or_else(|| value_str(arguments, "cwd"))
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn compact_text(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let mut out = one_line
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>();
        out.push_str("...");
        out
    }
}

pub(crate) fn preview_text(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

pub(crate) fn message_content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("content").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

#[derive(Deserialize)]
pub(crate) struct ExternalJsonLineKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    payload: Option<ExternalJsonPayloadKind<'a>>,
}

#[derive(Deserialize)]
pub(crate) struct ExternalJsonPayloadKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
}

pub(crate) fn codex_line_may_affect_replay(line: &str) -> bool {
    let Ok(kind) = serde_json::from_str::<ExternalJsonLineKind<'_>>(line) else {
        return true;
    };
    let payload_kind = kind
        .payload
        .as_ref()
        .and_then(|payload| payload.kind.as_deref());
    match (kind.kind.as_deref(), payload_kind) {
        (Some("session_meta"), _) => true,
        (
            _,
            Some(
                "thread_rolled_back"
                | "thread_goal_updated"
                | "thread_goal_cleared"
                | "user_message"
                | "agent_message"
                | "message",
            ),
        ) => true,
        // All response items must reach the parser so item-anchor rewinds can locate
        // their anchor — including non-message items (function_call, reasoning) that
        // render no transcript entry but are the usual targets of a noise-trim rewind.
        (Some("response_item"), _) => true,
        (Some("event_msg"), None) => true,
        (None, _) => true,
        _ => false,
    }
}

/// Item id carried by a `response_item` rollout line (the id an item-anchor rewind
/// targets), or `None` for other line kinds / items without an id.
pub(crate) fn codex_response_item_id(obj: &serde_json::Value) -> Option<String> {
    if obj.get("type").and_then(|v| v.as_str()) != Some("response_item") {
        return None;
    }
    obj.get("payload")
        .and_then(|payload| payload.get("id"))
        .or_else(|| obj.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_line_may_affect_session_list(line: &str) -> bool {
    let Ok(kind) = serde_json::from_str::<ExternalJsonLineKind<'_>>(line) else {
        return true;
    };
    let payload_kind = kind
        .payload
        .as_ref()
        .and_then(|payload| payload.kind.as_deref());
    matches!(
        (kind.kind.as_deref(), payload_kind),
        (Some("session_meta" | "turn_context"), _)
            | (Some("event_msg"), _)
            | (Some("response_item"), Some("message" | "function_call"))
            | (Some("response_item"), None)
            | (None, _)
    )
}

pub(crate) fn codex_payload_text(payload: &serde_json::Value) -> Option<(String, String)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    let role = payload
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("message")
        .to_string();
    let content = payload.get("content")?;
    message_content_text(content).map(|text| (role, text))
}

pub(crate) fn codex_function_call_id(payload: &serde_json::Value) -> Option<String> {
    value_str(payload, "call_id")
        .or_else(|| value_str(payload, "callId"))
        .or_else(|| value_str(payload, "id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_function_call_arguments(
    payload: &serde_json::Value,
) -> Option<serde_json::Value> {
    let arguments = payload.get("arguments")?;
    if let Some(raw) = arguments.as_str() {
        serde_json::from_str::<serde_json::Value>(raw).ok()
    } else {
        Some(arguments.clone())
    }
}

pub(crate) fn codex_function_call_command(payload: &serde_json::Value) -> Option<String> {
    let name = value_str(payload, "name")?;
    if name != "exec_command" {
        return Some(name);
    }
    let arguments = codex_function_call_arguments(payload)?;
    value_str(&arguments, "cmd")
        .or_else(|| value_str(&arguments, "command"))
        .or_else(|| value_str(&arguments, "script"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or(Some(name))
}

pub(crate) fn codex_function_call_projection(
    payload: &serde_json::Value,
    response_item_id: Option<&str>,
    current_turn_id: Option<&str>,
) -> Option<(String, serde_json::Value)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("function_call") {
        return None;
    }
    let call_id = codex_function_call_id(payload)?;
    let item_id = response_item_id
        .map(str::to_string)
        .or_else(|| value_str(payload, "id"))
        .unwrap_or_else(|| call_id.clone());
    let command = codex_function_call_command(payload);
    let cwd = codex_exec_command_workdir(payload);
    let turn_id = current_turn_id.map(str::to_string);
    Some((
        call_id.clone(),
        serde_json::json!({
            "id": item_id,
            "call_id": call_id,
            "type": "command_execution",
            "status": "started",
            "command": command,
            "cwd": cwd,
            "turn_id": turn_id,
        }),
    ))
}

pub(crate) fn codex_function_call_output(
    payload: &serde_json::Value,
) -> Option<(Option<String>, String)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("function_call_output") {
        return None;
    }
    let raw_output = value_str(payload, "output")?;
    let output = crate::external_agent::codex::strip_codex_tool_output_envelope(&raw_output);
    if output.trim().is_empty() {
        return None;
    }
    let output_id = codex_function_call_id(payload);
    Some((output_id, output))
}

pub(crate) fn codex_session_goal_from_value(goal: &serde_json::Value) -> Option<SessionGoal> {
    if goal.is_null() || goal.as_bool() == Some(false) {
        return None;
    }
    let objective = value_str(goal, "objective")
        .or_else(|| value_str(goal, "goal"))
        .or_else(|| value_str(goal, "title"))?
        .trim()
        .to_string();
    if objective.is_empty() {
        return None;
    }
    Some(SessionGoal {
        objective,
        status: value_str(goal, "status").filter(|s| !s.trim().is_empty()),
        elapsed_seconds: goal
            .get("timeUsedSeconds")
            .or_else(|| goal.get("elapsedSeconds"))
            .or_else(|| goal.get("elapsed_seconds"))
            .or_else(|| goal.get("time_used_seconds"))
            .and_then(|v| v.as_u64()),
        tokens_used: goal
            .get("tokensUsed")
            .or_else(|| goal.get("tokens_used"))
            .and_then(|v| v.as_u64()),
        token_budget: goal
            .get("tokenBudget")
            .or_else(|| goal.get("token_budget"))
            .and_then(|v| v.as_u64()),
    })
}

pub(crate) fn codex_session_goal_from_thread_payload(
    payload: &serde_json::Value,
) -> Option<SessionGoal> {
    payload
        .get("goal")
        .and_then(codex_session_goal_from_value)
        .or_else(|| codex_session_goal_from_value(payload))
}

pub(crate) fn codex_thread_goal_session_id(
    payload: &serde_json::Value,
    fallback_session_id: Option<&str>,
) -> Option<String> {
    value_str(payload, "threadId")
        .or_else(|| value_str(payload, "thread_id"))
        .or_else(|| fallback_session_id.map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_session_goal_entry(
    ts: &str,
    session_id: &str,
    goal: Option<SessionGoal>,
) -> serde_json::Value {
    serde_json::json!({
        "event": "session_goal",
        "session_id": session_id,
        "ts": ts,
        "data": {
            "session_id": session_id,
            "goal": goal,
        },
    })
}

pub(crate) fn codex_event_message_text(payload: &serde_json::Value) -> Option<(String, String)> {
    match payload.get("type").and_then(|v| v.as_str())? {
        "user_message" => value_str(payload, "message").map(|text| ("user".to_string(), text)),
        "agent_message" => {
            value_str(payload, "message").map(|text| ("assistant".to_string(), text))
        }
        _ => None,
    }
}

pub(crate) fn codex_thread_rollback_anchor(
    payload: &serde_json::Value,
) -> Option<(String, String)> {
    let anchor = payload.get("anchor")?;
    let item_id = value_str(anchor, "itemId")
        .or_else(|| value_str(anchor, "item_id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let position = value_str(anchor, "position")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s == "before" || s == "after")
        .unwrap_or_else(|| "after".to_string());
    Some((item_id, position))
}

pub(crate) fn is_codex_injected_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<turn_aborted>")
        || trimmed.starts_with("<subagent_notification>")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<task-notification>")
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<command-message>")
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<bash-input>")
        || trimmed.starts_with("<bash-stdout>")
        || trimmed.starts_with("<bash-stderr>")
        || trimmed.starts_with("<user_shell_command>")
}

pub(crate) fn codex_thread_display_name(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| !is_codex_injected_user_text(s))
        .map(|s| compact_text(&s, 180))
}

pub(crate) fn normalize_session_thread_source(value: &str) -> Option<String> {
    let normalized = value.trim().to_lowercase().replace('_', "-");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(crate) fn normalize_session_relationship_kind(value: &str) -> Option<String> {
    match value.trim().to_lowercase().replace('_', "-").as_str() {
        "side" => Some("side".to_string()),
        "fork" => Some("fork".to_string()),
        "subagent" | "sub-agent" => Some("subagent".to_string()),
        _ => None,
    }
}

pub(crate) fn relationship_from_thread_source(
    thread_source: Option<&str>,
    parent_id: Option<&str>,
) -> Option<String> {
    let source = thread_source.and_then(normalize_session_thread_source)?;
    match source.as_str() {
        "subagent" => Some("subagent".to_string()),
        "side" => Some("side".to_string()),
        "fork" => Some("fork".to_string()),
        _ if parent_id.is_some_and(|id| !id.trim().is_empty()) => Some("fork".to_string()),
        _ => None,
    }
}

pub(crate) fn codex_thread_source_from_payload(payload: &serde_json::Value) -> Option<String> {
    if let Some(source) =
        value_str(payload, "thread_source").and_then(|s| normalize_session_thread_source(&s))
    {
        return Some(source);
    }
    if payload.pointer("/source/subagent").is_some() {
        return Some("subagent".to_string());
    }
    None
}

pub(crate) fn session_lineage_from_codex_payload(
    payload: &serde_json::Value,
) -> SessionLineageMetadata {
    let subagent_spawn = payload.pointer("/source/subagent/thread_spawn");
    let parent_id = value_str(payload, "forked_from_id")
        .or_else(|| value_str(payload, "parent_thread_id"))
        .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "parent_thread_id")))
        .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "parent_session_id")))
        .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "parent_id")));
    let thread_source = codex_thread_source_from_payload(payload);
    let relationship = value_str(payload, "relationship")
        .or_else(|| value_str(payload, "relationship_kind"))
        .and_then(|value| normalize_session_relationship_kind(&value))
        .or_else(|| {
            relationship_from_thread_source(thread_source.as_deref(), parent_id.as_deref())
        });
    SessionLineageMetadata {
        parent_id,
        relationship,
        thread_source,
        agent_nickname: value_str(payload, "agent_nickname")
            .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "agent_nickname"))),
    }
}

pub(crate) fn push_external_transcript_entry(
    entries: &mut Vec<serde_json::Value>,
    provider_source: &str,
    ts: &str,
    role: &str,
    text: String,
) -> bool {
    let role = match role.trim().to_lowercase().as_str() {
        "model" => "assistant".to_string(),
        other => other.to_string(),
    };
    if role != "user" && role != "assistant" {
        return false;
    }
    if text.trim().is_empty() {
        return false;
    }
    if role == "user" && is_codex_injected_user_text(&text) {
        return false;
    }
    entries.push(serde_json::json!({
        "ts": ts,
        "level": if role == "assistant" || role == "model" { "model" } else { "info" },
        "source": external_transcript_source(provider_source, &role),
        "content": text,
    }));
    true
}

pub(crate) fn external_transcript_entry_role(entry: &serde_json::Value) -> Option<&'static str> {
    if entry.get("source").and_then(|v| v.as_str()) == Some("user") {
        Some("user")
    } else if entry.get("level").and_then(|v| v.as_str()) == Some("model") {
        Some("assistant")
    } else {
        None
    }
}

/// Mark every transcript entry at or after `start_index` superseded (skipping
/// rollback markers and already-superseded entries). Used for item-anchor rewinds,
/// which discard the tail after the anchored item. Returns the user-turn indices
/// that were superseded so caller can keep revision bookkeeping consistent.
pub(crate) fn mark_external_entries_superseded_from(
    entries: &mut [serde_json::Value],
    start_index: usize,
    rollback_ts: &str,
) -> Vec<u32> {
    let mut superseded_user_turns = Vec::new();
    for entry in entries.iter_mut().skip(start_index) {
        if entry
            .get("superseded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if entry.get("kind").and_then(|v| v.as_str()) == Some("rollback_marker") {
            continue;
        }
        let Some(role) = external_transcript_entry_role(entry) else {
            continue;
        };
        let user_turn_index = entry
            .get("user_turn_index")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("superseded".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "superseded_at".to_string(),
                serde_json::Value::String(rollback_ts.to_string()),
            );
            obj.insert(
                "superseded_reason".to_string(),
                serde_json::Value::String("thread_rollback".to_string()),
            );
        }
        if role == "user" {
            if let Some(turn) = user_turn_index {
                superseded_user_turns.push(turn);
            }
        }
    }
    superseded_user_turns
}

pub(crate) fn mark_latest_external_turn_superseded(
    entries: &mut [serde_json::Value],
    rollback_ts: &str,
) -> Option<u32> {
    for idx in (0..entries.len()).rev() {
        let entry = &entries[idx];
        if entry
            .get("superseded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if entry.get("kind").and_then(|v| v.as_str()) == Some("rollback_marker") {
            continue;
        }
        let Some(role) = external_transcript_entry_role(entry) else {
            continue;
        };
        let user_turn_index = entry
            .get("user_turn_index")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        if let Some(obj) = entries[idx].as_object_mut() {
            obj.insert("superseded".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "superseded_at".to_string(),
                serde_json::Value::String(rollback_ts.to_string()),
            );
            obj.insert(
                "superseded_reason".to_string(),
                serde_json::Value::String("thread_rollback".to_string()),
            );
        }
        if role == "user" {
            return user_turn_index;
        }
    }
    None
}

pub(crate) fn collect_files(root: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, suffix, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(suffix))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

pub(crate) fn file_mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|m| metadata_mtime_secs(&m))
        .unwrap_or(0)
}

pub(crate) fn metadata_mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn metadata_mtime_nanos(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(crate) fn metadata_ctime_nanos(metadata: &std::fs::Metadata) -> i128 {
    crate::platform::metadata_ctime_nanos(metadata)
}

pub(crate) fn session_list_path_key(path: &Path) -> String {
    let normalized = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalized.to_string_lossy().to_string()
}

pub(crate) fn file_dependency_fingerprint(path: &Path) -> String {
    let path_key = session_list_path_key(path);
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let (dev, ino) = crate::platform::metadata_dev_ino(&metadata);
            format!(
                "{path_key}\0{}\0{}\0{}\0{}\0{}",
                metadata.len(),
                metadata_mtime_nanos(&metadata),
                metadata_ctime_nanos(&metadata),
                dev,
                ino
            )
        }
        Err(_) => format!("{path_key}\0missing"),
    }
}

pub(crate) fn session_list_cache_key(
    namespace: &'static str,
    path: &Path,
    extra: impl Into<String>,
) -> Option<SessionListCacheKey> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let (dev, ino) = crate::platform::metadata_dev_ino(&metadata);
    Some(SessionListCacheKey {
        namespace,
        path: session_list_path_key(path),
        len: metadata.len(),
        mtime_nanos: metadata_mtime_nanos(&metadata),
        ctime_nanos: metadata_ctime_nanos(&metadata),
        dev,
        ino,
        extra: extra.into(),
    })
}

pub(crate) fn session_list_cache_slot(key: &SessionListCacheKey) -> String {
    format!("{}\0{}\0{}", key.namespace, key.path, key.extra)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedSessionCacheKey {
    namespace: String,
    path: String,
    len: u64,
    #[serde(with = "string_u128")]
    mtime_nanos: u128,
    #[serde(with = "string_i128")]
    ctime_nanos: i128,
    dev: u64,
    ino: u64,
    extra: String,
}

impl PersistedSessionCacheKey {
    pub(crate) fn of(key: &SessionListCacheKey) -> Self {
        Self {
            namespace: key.namespace.to_string(),
            path: key.path.clone(),
            len: key.len,
            mtime_nanos: key.mtime_nanos,
            ctime_nanos: key.ctime_nanos,
            dev: key.dev,
            ino: key.ino,
            extra: key.extra.clone(),
        }
    }

    pub(crate) fn matches(&self, key: &SessionListCacheKey) -> bool {
        self.namespace == key.namespace
            && self.path == key.path
            && self.len == key.len
            && self.mtime_nanos == key.mtime_nanos
            && self.ctime_nanos == key.ctime_nanos
            && self.dev == key.dev
            && self.ino == key.ino
            && self.extra == key.extra
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedSessionCacheEntry<T> {
    #[serde(default)]
    schema: u32,
    key: PersistedSessionCacheKey,
    value: T,
}

/// Schema stamp for a namespace's persisted entries. Old entries (schema 0
/// predates the field) mismatch after a bump and read as cache misses.
pub(crate) fn persisted_namespace_schema(namespace: &str) -> u32 {
    match namespace {
        // v1: summaries persist `first_usage_event` instead of the full
        // `usage_events` history; pre-v1 entries would deserialize with a
        // defaulted first event and mis-baseline forked sessions.
        "codex" => 1,
        _ => 0,
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedIntendantSessionEntry {
    fingerprint: SessionDirFingerprint,
    row: serde_json::Value,
}

pub(crate) fn session_index_dir() -> PathBuf {
    crate::platform::home_dir()
        .join(".intendant")
        .join("cache")
        .join("session_index")
}

pub(crate) fn session_index_entry_path_in(base: &Path, namespace: &str, slot: &str) -> PathBuf {
    let digest = ring::digest::digest(&ring::digest::SHA256, slot.as_bytes());
    let mut name = String::with_capacity(digest.as_ref().len() * 2 + 5);
    for byte in digest.as_ref() {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".json");
    base.join(namespace).join(name)
}

pub(crate) fn session_index_entry_path(namespace: &str, slot: &str) -> PathBuf {
    session_index_entry_path_in(&session_index_dir(), namespace, slot)
}

pub(crate) fn write_session_index_entry(path: &Path, body: &[u8]) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = crate::file_watcher::atomic_write(path, body);
}

pub(crate) fn load_persisted_session_entry_in<T: serde::de::DeserializeOwned>(
    base: &Path,
    key: &SessionListCacheKey,
) -> Option<T> {
    let path = session_index_entry_path_in(base, key.namespace, &session_list_cache_slot(key));
    let bytes = std::fs::read(path).ok()?;
    let entry: PersistedSessionCacheEntry<T> = serde_json::from_slice(&bytes).ok()?;
    if entry.schema != persisted_namespace_schema(key.namespace) {
        return None;
    }
    entry.key.matches(key).then_some(entry.value)
}

pub(crate) fn load_persisted_session_entry<T: serde::de::DeserializeOwned>(
    key: &SessionListCacheKey,
) -> Option<T> {
    load_persisted_session_entry_in(&session_index_dir(), key)
}

pub(crate) fn store_persisted_session_entry_in<T: Serialize>(
    base: &Path,
    key: &SessionListCacheKey,
    value: &T,
) {
    let entry = PersistedSessionCacheEntry {
        schema: persisted_namespace_schema(key.namespace),
        key: PersistedSessionCacheKey::of(key),
        value,
    };
    let Ok(body) = serde_json::to_vec(&entry) else {
        return;
    };
    let path = session_index_entry_path_in(base, key.namespace, &session_list_cache_slot(key));
    write_session_index_entry(&path, &body);
}

pub(crate) fn store_persisted_session_entry<T: Serialize>(key: &SessionListCacheKey, value: &T) {
    store_persisted_session_entry_in(&session_index_dir(), key, value);
}

pub(crate) fn load_persisted_intendant_row(
    fingerprint: &SessionDirFingerprint,
) -> Option<serde_json::Value> {
    let path = session_index_entry_path("intendant-row", &fingerprint.path);
    let bytes = std::fs::read(path).ok()?;
    let entry: PersistedIntendantSessionEntry = serde_json::from_slice(&bytes).ok()?;
    (&entry.fingerprint == fingerprint).then_some(entry.row)
}

pub(crate) fn store_persisted_intendant_row(
    fingerprint: &SessionDirFingerprint,
    row: &serde_json::Value,
) {
    let entry = PersistedIntendantSessionEntry {
        fingerprint: fingerprint.clone(),
        row: row.clone(),
    };
    let Ok(body) = serde_json::to_vec(&entry) else {
        return;
    };
    let path = session_index_entry_path("intendant-row", &fingerprint.path);
    write_session_index_entry(&path, &body);
}

pub(crate) fn remove_persisted_intendant_row(dir: &Path) {
    let path = session_index_entry_path("intendant-row", &session_list_path_key(dir));
    let _ = std::fs::remove_file(path);
}

/// Bulk-load the on-disk session index into the in-memory caches once per
/// process. Thousands of lazy per-entry reads during the first list scan
/// cost seconds sequentially; one parallel sweep up front costs a fraction
/// of that. Entries land exactly as `store_*` would have put them — the
/// normal lookup path still validates every fingerprint against the live
/// filesystem, so a stale preloaded entry can never be served.
pub(crate) type PreloadApply = fn(&'static str, &[u8]) -> PreloadOutcome;

pub(crate) fn preload_session_index() {
    static PRELOADED: std::sync::Once = std::sync::Once::new();
    PRELOADED.call_once(|| {
        let base = session_index_dir();
        let namespaces: [(&'static str, PreloadApply); 4] = [
            ("codex", preload_codex_entry),
            ("claude-code", preload_row_entry),
            ("gemini", preload_row_entry),
            ("codex-parent-baseline", preload_baseline_entry),
        ];
        std::thread::scope(|scope| {
            for (namespace, apply) in namespaces {
                let dir = base.join(namespace);
                scope.spawn(move || preload_namespace_dir(&dir, namespace, apply));
            }
            let intendant_dir = base.join("intendant-row");
            scope.spawn(move || {
                preload_namespace_dir(&intendant_dir, "intendant-row", preload_intendant_entry)
            });
        });
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PreloadOutcome {
    /// Entry landed in the in-memory cache.
    Loaded,
    /// The session file/dir the entry indexes is gone — the entry is dead
    /// weight and its index file should be deleted.
    TargetMissing,
    /// Not loadable by this build (schema mismatch, unreadable JSON). Keep
    /// the file: an older or newer daemon sharing this HOME may still own
    /// it, and refreshes overwrite the same slot anyway.
    Skipped,
}

pub(crate) fn preload_namespace_dir(dir: &Path, namespace: &'static str, apply: PreloadApply) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp.")
            || (name.starts_with(".intendant-write-") && name.ends_with(".tmp"))
        {
            // Writers rename these away within the same call; anything
            // older than a minute is litter from a crashed daemon.
            let aged = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age > std::time::Duration::from_secs(60))
                .unwrap_or(false);
            if aged {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return;
    }
    // The largest namespace holds thousands of entries; a few reader
    // threads keep the preload in the hundreds of milliseconds.
    let chunk = paths.len().div_ceil(4).max(1);
    std::thread::scope(|scope| {
        for slice in paths.chunks(chunk) {
            scope.spawn(move || {
                for path in slice {
                    if let Ok(bytes) = std::fs::read(path) {
                        if apply(namespace, &bytes) == PreloadOutcome::TargetMissing {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
            });
        }
    });
}

pub(crate) fn runtime_session_cache_key(
    namespace: &'static str,
    key: PersistedSessionCacheKey,
) -> Option<SessionListCacheKey> {
    if key.namespace != namespace {
        return None;
    }
    Some(SessionListCacheKey {
        namespace,
        path: key.path,
        len: key.len,
        mtime_nanos: key.mtime_nanos,
        ctime_nanos: key.ctime_nanos,
        dev: key.dev,
        ino: key.ino,
        extra: key.extra,
    })
}

/// Lenient fallback for entries this build cannot parse (legacy or future
/// shapes): both formats keep the indexed session's path under `key.path`
/// (generic namespaces) or `fingerprint.path` (intendant rows), so a dead
/// target is still detectable — and prunable — without understanding the
/// rest of the entry. Anything else unreadable is left alone.
pub(crate) fn preload_unparsed_entry_outcome(bytes: &[u8]) -> PreloadOutcome {
    #[derive(Deserialize)]
    struct ProbePath {
        path: Option<String>,
    }
    #[derive(Deserialize)]
    struct Probe {
        key: Option<ProbePath>,
        fingerprint: Option<ProbePath>,
    }
    let Ok(probe) = serde_json::from_slice::<Probe>(bytes) else {
        return PreloadOutcome::Skipped;
    };
    let path = probe
        .key
        .and_then(|key| key.path)
        .or_else(|| probe.fingerprint.and_then(|fingerprint| fingerprint.path));
    match path {
        Some(path) if !path.is_empty() && !Path::new(&path).exists() => {
            PreloadOutcome::TargetMissing
        }
        _ => PreloadOutcome::Skipped,
    }
}

pub(crate) fn preload_row_entry(namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) = serde_json::from_slice::<PersistedSessionCacheEntry<serde_json::Value>>(bytes)
    else {
        return preload_unparsed_entry_outcome(bytes);
    };
    let schema_matches = entry.schema == persisted_namespace_schema(namespace);
    let Some(key) = runtime_session_cache_key(namespace, entry.key) else {
        return PreloadOutcome::Skipped;
    };
    if !Path::new(&key.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    if !schema_matches {
        return PreloadOutcome::Skipped;
    }
    let slot = session_list_cache_slot(&key);
    let mut cache = session_list_row_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entry(slot).or_insert(SessionListRowCacheEntry {
        key,
        row: entry.value,
    });
    PreloadOutcome::Loaded
}

pub(crate) fn preload_codex_entry(namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) =
        serde_json::from_slice::<PersistedSessionCacheEntry<CodexSessionListSummary>>(bytes)
    else {
        return preload_unparsed_entry_outcome(bytes);
    };
    let schema_matches = entry.schema == persisted_namespace_schema(namespace);
    let Some(key) = runtime_session_cache_key(namespace, entry.key) else {
        return PreloadOutcome::Skipped;
    };
    if !Path::new(&key.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    if !schema_matches {
        return PreloadOutcome::Skipped;
    }
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entry(slot).or_insert(CodexSessionListCacheEntry {
        key,
        summary: entry.value,
    });
    PreloadOutcome::Loaded
}

pub(crate) fn preload_baseline_entry(namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) =
        serde_json::from_slice::<PersistedSessionCacheEntry<Option<SessionUsage>>>(bytes)
    else {
        return preload_unparsed_entry_outcome(bytes);
    };
    let schema_matches = entry.schema == persisted_namespace_schema(namespace);
    let Some(key) = runtime_session_cache_key(namespace, entry.key) else {
        return PreloadOutcome::Skipped;
    };
    if !Path::new(&key.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    if !schema_matches {
        return PreloadOutcome::Skipped;
    }
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_parent_usage_baseline_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(slot)
        .or_insert(CodexParentUsageBaselineCacheEntry {
            key,
            usage: entry.value,
        });
    PreloadOutcome::Loaded
}

pub(crate) fn preload_intendant_entry(_namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) = serde_json::from_slice::<PersistedIntendantSessionEntry>(bytes) else {
        return preload_unparsed_entry_outcome(bytes);
    };
    if !Path::new(&entry.fingerprint.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    let slot = entry.fingerprint.path.clone();
    let mut cache = intendant_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entry(slot).or_insert(IntendantSessionListCacheEntry {
        fingerprint: entry.fingerprint,
        row: entry.row,
    });
    PreloadOutcome::Loaded
}

pub(crate) fn session_list_row_cache() -> &'static Mutex<HashMap<String, SessionListRowCacheEntry>>
{
    SESSION_LIST_ROW_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_session_list_row(key: &SessionListCacheKey) -> Option<serde_json::Value> {
    let slot = session_list_cache_slot(key);
    {
        let cache = session_list_row_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&slot).filter(|entry| &entry.key == key) {
            return Some(entry.row.clone());
        }
    }
    // Miss in memory (fresh process): try the on-disk index before paying
    // a full re-parse. A hit re-seeds the in-memory tier.
    let row = load_persisted_session_entry::<serde_json::Value>(key)?;
    let mut cache = session_list_row_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        SessionListRowCacheEntry {
            key: key.clone(),
            row: row.clone(),
        },
    );
    Some(row)
}

pub(crate) fn store_session_list_row(key: SessionListCacheKey, row: &serde_json::Value) {
    store_persisted_session_entry(&key, row);
    let slot = session_list_cache_slot(&key);
    let mut cache = session_list_row_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        SessionListRowCacheEntry {
            key,
            row: row.clone(),
        },
    );
}

pub(crate) fn codex_session_list_cache(
) -> &'static Mutex<HashMap<String, CodexSessionListCacheEntry>> {
    CODEX_SESSION_LIST_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_codex_session_list_entry(
    key: &SessionListCacheKey,
) -> Option<CodexSessionListCacheEntry> {
    let slot = session_list_cache_slot(key);
    {
        let cache = codex_session_list_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&slot).filter(|entry| &entry.key == key) {
            return Some(entry.clone());
        }
    }
    let summary = load_persisted_session_entry::<CodexSessionListSummary>(key)?;
    let entry = CodexSessionListCacheEntry {
        key: key.clone(),
        summary,
    };
    let mut cache = codex_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(slot, entry.clone());
    Some(entry)
}

pub(crate) fn store_codex_session_list_entry(
    key: SessionListCacheKey,
    summary: CodexSessionListSummary,
) {
    store_persisted_session_entry(&key, &summary);
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(slot, CodexSessionListCacheEntry { key, summary });
}

pub(crate) fn codex_parent_usage_baseline_cache(
) -> &'static Mutex<HashMap<String, CodexParentUsageBaselineCacheEntry>> {
    CODEX_PARENT_USAGE_BASELINE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_codex_parent_usage_baseline(
    key: &SessionListCacheKey,
) -> Option<Option<SessionUsage>> {
    let slot = session_list_cache_slot(key);
    {
        let cache = codex_parent_usage_baseline_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&slot).filter(|entry| &entry.key == key) {
            return Some(entry.usage);
        }
    }
    let usage = load_persisted_session_entry::<Option<SessionUsage>>(key)?;
    let mut cache = codex_parent_usage_baseline_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        CodexParentUsageBaselineCacheEntry {
            key: key.clone(),
            usage,
        },
    );
    Some(usage)
}

pub(crate) fn store_codex_parent_usage_baseline(
    key: SessionListCacheKey,
    usage: Option<SessionUsage>,
) {
    store_persisted_session_entry(&key, &usage);
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_parent_usage_baseline_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(slot, CodexParentUsageBaselineCacheEntry { key, usage });
}

pub(crate) fn intendant_session_list_cache(
) -> &'static Mutex<HashMap<String, IntendantSessionListCacheEntry>> {
    INTENDANT_SESSION_LIST_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_intendant_session_list_row(
    fingerprint: &SessionDirFingerprint,
) -> Option<serde_json::Value> {
    {
        let cache = intendant_session_list_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache
            .get(&fingerprint.path)
            .filter(|entry| &entry.fingerprint == fingerprint)
        {
            return Some(entry.row.clone());
        }
    }
    let row = load_persisted_intendant_row(fingerprint)?;
    let slot = fingerprint.path.clone();
    let mut cache = intendant_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        IntendantSessionListCacheEntry {
            fingerprint: fingerprint.clone(),
            row: row.clone(),
        },
    );
    Some(row)
}

pub(crate) fn store_intendant_session_list_row(
    fingerprint: SessionDirFingerprint,
    row: &serde_json::Value,
) {
    store_persisted_intendant_row(&fingerprint, row);
    let slot = fingerprint.path.clone();
    let mut cache = intendant_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        IntendantSessionListCacheEntry {
            fingerprint,
            row: row.clone(),
        },
    );
}

pub(crate) fn collect_recent_files(root: &Path, suffix: &str, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, suffix, &mut files);
    let mut seen = HashSet::new();
    files.retain(|path| {
        std::fs::canonicalize(path)
            .map(|canonical| seen.insert(canonical))
            .unwrap_or(true)
    });
    files.sort_by_key(|b| std::cmp::Reverse(file_mtime_secs(b)));
    files.truncate(limit);
    files
}

pub(crate) fn derive_project_root_from_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?.trim();
    if cwd.is_empty() {
        return None;
    }

    let mut current = PathBuf::from(cwd);
    if !current.is_absolute() {
        return Some(cwd.to_string());
    }
    if current.is_file() {
        current.pop();
    }

    loop {
        if current.join(".git").exists() {
            return Some(current.to_string_lossy().to_string());
        }
        if !current.pop() {
            break;
        }
    }

    Some(cwd.to_string())
}

pub(crate) fn read_text_head_tail(path: &Path, head_bytes: u64, tail_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok().map(|m| m.len()).unwrap_or(0);
    if len <= head_bytes.saturating_add(tail_bytes) {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        return Some(String::from_utf8_lossy(&buf).to_string());
    }

    let mut head = vec![0; head_bytes as usize];
    let head_len = file.read(&mut head).ok()?;
    head.truncate(head_len);

    file.seek(SeekFrom::End(-(tail_bytes as i64))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;

    let mut out = String::from_utf8_lossy(&head).to_string();
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&tail));
    Some(out)
}

pub(crate) fn read_text_tail(path: &Path, tail_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok().map(|m| m.len()).unwrap_or(0);
    if len <= tail_bytes {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        return Some(String::from_utf8_lossy(&buf).to_string());
    }

    file.seek(SeekFrom::End(-(tail_bytes as i64))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    Some(String::from_utf8_lossy(&tail).to_string())
}

pub(crate) fn file_mtime_string(path: &Path) -> Option<String> {
    mtime_secs_to_string(file_mtime_secs(path))
}

pub(crate) fn mtime_secs_to_string(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let dt: chrono::DateTime<chrono::Local> = t.into();
    Some(dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

pub(crate) fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionUsage {
    total_tokens: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_creation_tokens: u64,
    cached_tokens: u64,
}

impl SessionUsage {
    pub(crate) fn is_empty(self) -> bool {
        self.total_tokens == 0
            && self.prompt_tokens == 0
            && self.completion_tokens == 0
            && self.cache_creation_tokens == 0
            && self.cached_tokens == 0
    }

    pub(crate) fn add(&mut self, other: SessionUsage) {
        self.total_tokens += other.total_tokens;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.cached_tokens += other.cached_tokens;
    }

    pub(crate) fn saturating_sub(self, baseline: SessionUsage) -> SessionUsage {
        SessionUsage {
            total_tokens: self.total_tokens.saturating_sub(baseline.total_tokens),
            prompt_tokens: self.prompt_tokens.saturating_sub(baseline.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_sub(baseline.completion_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_sub(baseline.cache_creation_tokens),
            cached_tokens: self.cached_tokens.saturating_sub(baseline.cached_tokens),
        }
    }
}

pub(crate) fn value_u64_at(value: &serde_json::Value, paths: &[&str]) -> Option<u64> {
    paths
        .iter()
        .find_map(|path| value.pointer(path).and_then(|v| v.as_u64()))
}

pub(crate) fn usage_day_from_timestamp(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        let local: chrono::DateTime<chrono::Local> = dt.with_timezone(&chrono::Local);
        return Some(local.format("%Y-%m-%d").to_string());
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, format) {
            if let Some(local) = dt.and_local_timezone(chrono::Local).single() {
                return Some(local.format("%Y-%m-%d").to_string());
            }
        }
    }
    value
        .get(0..10)
        .filter(|s| {
            s.len() == 10
                && s.as_bytes()[4] == b'-'
                && s.as_bytes()[7] == b'-'
                && s.chars()
                    .enumerate()
                    .all(|(idx, ch)| idx == 4 || idx == 7 || ch.is_ascii_digit())
        })
        .map(|s| s.to_string())
}

pub(crate) fn apply_session_usage(
    session: &mut serde_json::Value,
    usage: SessionUsage,
    model: Option<&str>,
) {
    if usage.is_empty() {
        return;
    }
    let estimated_cost = model.and_then(|m| {
        crate::app_state_pricing::estimate_session_cost(
            m,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cached_tokens,
            usage.cache_creation_tokens,
        )
    });
    if let Some(obj) = session.as_object_mut() {
        obj.insert(
            "total_tokens".to_string(),
            serde_json::json!(usage.total_tokens),
        );
        obj.insert(
            "prompt_tokens".to_string(),
            serde_json::json!(usage.prompt_tokens),
        );
        obj.insert(
            "completion_tokens".to_string(),
            serde_json::json!(usage.completion_tokens),
        );
        obj.insert(
            "cached_tokens".to_string(),
            serde_json::json!(usage.cached_tokens),
        );
        obj.insert(
            "cache_creation_tokens".to_string(),
            serde_json::json!(usage.cache_creation_tokens),
        );
        obj.insert(
            "estimated_cost".to_string(),
            serde_json::json!(estimated_cost.unwrap_or(0.0)),
        );
        obj.insert(
            "pricing_known".to_string(),
            serde_json::json!(estimated_cost.is_some()),
        );
    }
}

pub(crate) fn apply_session_daily_usage(
    session: &mut serde_json::Value,
    daily_usage: &BTreeMap<String, SessionUsage>,
    model: Option<&str>,
) {
    if daily_usage.is_empty() {
        return;
    }
    let rows = daily_usage
        .iter()
        .filter(|(_, usage)| !usage.is_empty())
        .map(|(day, usage)| {
            let estimated_cost = model.and_then(|m| {
                crate::app_state_pricing::estimate_session_cost(
                    m,
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.cached_tokens,
                    usage.cache_creation_tokens,
                )
            });
            serde_json::json!({
                "day": day,
                "total_tokens": usage.total_tokens,
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "cached_tokens": usage.cached_tokens,
                "cache_creation_tokens": usage.cache_creation_tokens,
                "estimated_cost": estimated_cost.unwrap_or(0.0),
                "pricing_known": estimated_cost.is_some(),
            })
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return;
    }
    if let Some(obj) = session.as_object_mut() {
        obj.insert("daily_usage".to_string(), serde_json::json!(rows));
    }
}

pub(crate) fn session_usage_from_json(session: &serde_json::Value) -> SessionUsage {
    SessionUsage {
        total_tokens: session
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        prompt_tokens: session
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        completion_tokens: session
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_tokens: session
            .get("cached_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_tokens: session
            .get("cache_creation_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    }
}

pub(crate) fn apply_session_model_and_reprice(session: &mut serde_json::Value, model: &str) {
    if let Some(obj) = session.as_object_mut() {
        obj.insert("model".to_string(), serde_json::json!(model));
    }
    apply_session_usage(session, session_usage_from_json(session), Some(model));
}

pub(crate) fn external_session_json(
    source: &str,
    label: &str,
    session_id: String,
    resume_id: String,
    created_at: Option<String>,
    updated_at: Option<String>,
    name: Option<String>,
    task: Option<String>,
    provider: &str,
    model: Option<String>,
    turns: u64,
    project_root: Option<String>,
    cwd: Option<String>,
    path: Option<String>,
    bytes: u64,
) -> serde_json::Value {
    let created_at = created_at.unwrap_or_default();
    let updated_at = updated_at.unwrap_or_else(|| created_at.clone());
    let cwd = cwd.or_else(|| project_root.clone());
    serde_json::json!({
        "source": source,
        "source_label": label,
        "session_id": session_id,
        "resume_id": resume_id,
        "created_at": created_at,
        "updated_at": updated_at,
        "name": name,
        "task": task,
        "provider": provider,
        "model": model,
        "turns": turns,
        "status": "external",
        "total_tokens": 0,
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "cached_tokens": 0,
        "cache_creation_tokens": 0,
        "estimated_cost": 0.0,
        "pricing_known": false,
        "role": null,
        "recordings": 0,
        "recording_bytes": 0,
        "annotations": 0,
        "clips": 0,
        "frames_bytes": 0,
        "turns_bytes": bytes,
        "logs_bytes": bytes,
        "total_bytes": bytes,
        "cwd": cwd,
        "project_root": project_root,
        "path": path,
        "can_delete": false,
        "can_resume": true,
    })
}

pub(crate) fn timestamp_sort_secs(value: &str) -> i64 {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return dt.timestamp();
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, format) {
            if let Some(dt) = dt.and_local_timezone(chrono::Local).single() {
                return dt.timestamp();
            }
        }
    }
    0
}

pub(crate) fn session_created_sort_key(session: &serde_json::Value) -> i64 {
    session
        .get("created_at")
        .and_then(|v| v.as_str())
        .map(timestamp_sort_secs)
        .unwrap_or(0)
}

pub(crate) fn session_changed_sort_key(session: &serde_json::Value) -> i64 {
    session
        .get("updated_at")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(timestamp_sort_secs)
        .unwrap_or_else(|| session_created_sort_key(session))
}

pub(crate) fn sort_sessions_newest_first(sessions: &mut [serde_json::Value]) {
    sessions.sort_by_key(|b| std::cmp::Reverse(session_changed_sort_key(b)));
}

pub(crate) fn session_source(session: &serde_json::Value) -> &str {
    session
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("intendant")
}

pub(crate) fn session_unique_key(session: &serde_json::Value) -> String {
    let source = session_source(session);
    let session_id = session
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    format!("{source}:{session_id}")
}

pub(crate) fn push_unique_session(
    out: &mut Vec<serde_json::Value>,
    seen: &mut HashSet<String>,
    session: &serde_json::Value,
) {
    if seen.insert(session_unique_key(session)) {
        out.push(session.clone());
    }
}

pub(crate) fn truncate_sessions_preserving_sources_to(
    sessions: &mut Vec<serde_json::Value>,
    limit: usize,
) {
    if sessions.len() <= limit {
        return;
    }

    let mut out = Vec::with_capacity(limit);
    let mut seen = HashSet::new();
    let source_floor = SESSION_SOURCE_FLOOR.min((limit / 4).max(1));
    for source in ["intendant", "codex", "claude-code", "gemini"] {
        for session in sessions
            .iter()
            .filter(|session| session_source(session) == source)
            .take(source_floor)
        {
            push_unique_session(&mut out, &mut seen, session);
        }
    }

    for session in sessions.iter() {
        if out.len() >= limit {
            break;
        }
        push_unique_session(&mut out, &mut seen, session);
    }

    sort_sessions_newest_first(&mut out);
    *sessions = out;
}

pub(crate) fn truncate_sessions_preserving_sources(sessions: &mut Vec<serde_json::Value>) {
    truncate_sessions_preserving_sources_to(sessions, SESSION_LIST_LIMIT)
}

pub(crate) fn codex_usage_bucket<'a>(
    value: &'a serde_json::Value,
    names: &[&str],
) -> Option<&'a serde_json::Value> {
    for name in names {
        if let Some(v) = value.get(*name) {
            return Some(v);
        }
        if let Some(info) = value.get("info") {
            if let Some(v) = info.get(*name) {
                return Some(v);
            }
        }
    }
    None
}

pub(crate) fn codex_session_usage_from_payload(
    payload: &serde_json::Value,
) -> Option<SessionUsage> {
    codex_session_usage_from_payload_bucket(
        payload,
        &["total_token_usage", "totalTokenUsage", "total"],
        true,
    )
}

pub(crate) fn codex_session_usage_from_payload_bucket(
    payload: &serde_json::Value,
    bucket_names: &[&str],
    fallback_to_info: bool,
) -> Option<SessionUsage> {
    let info = payload
        .get("info")
        .or_else(|| payload.get("tokenUsage"))
        .unwrap_or(payload);
    if info.is_null() {
        return None;
    }
    let total =
        codex_usage_bucket(info, bucket_names).or_else(|| fallback_to_info.then_some(info))?;
    let prompt_tokens = value_u64_at(total, &["/input_tokens", "/inputTokens"])?;
    let completion_tokens = value_u64_at(total, &["/output_tokens", "/outputTokens"]).unwrap_or(0);
    let cached_tokens = value_u64_at(
        total,
        &[
            "/cached_input_tokens",
            "/cachedInputTokens",
            "/cached_tokens",
            "/cachedTokens",
        ],
    )
    .unwrap_or(0);
    let total_tokens = value_u64_at(total, &["/total_tokens", "/totalTokens"])
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    Some(SessionUsage {
        total_tokens,
        prompt_tokens,
        completion_tokens,
        cache_creation_tokens: 0,
        cached_tokens,
    })
}

pub(crate) fn claude_usage_from_message_usage(usage: &serde_json::Value) -> Option<SessionUsage> {
    let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64())?;
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let completion_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let prompt_tokens = input_tokens + cache_creation + cache_read;
    Some(SessionUsage {
        total_tokens: prompt_tokens + completion_tokens,
        prompt_tokens,
        completion_tokens,
        cache_creation_tokens: cache_creation,
        cached_tokens: cache_read,
    })
}

pub(crate) fn gemini_usage_from_tokens(tokens: &serde_json::Value) -> Option<SessionUsage> {
    let prompt_tokens = value_u64_at(
        tokens,
        &[
            "/input",
            "/input_tokens",
            "/inputTokens",
            "/prompt",
            "/prompt_tokens",
            "/promptTokens",
        ],
    )?;
    let output_tokens = value_u64_at(
        tokens,
        &[
            "/output",
            "/output_tokens",
            "/outputTokens",
            "/completion",
            "/completion_tokens",
            "/completionTokens",
        ],
    )
    .unwrap_or(0);
    let thinking_tokens = value_u64_at(
        tokens,
        &[
            "/thoughts",
            "/thought_tokens",
            "/thoughtTokens",
            "/thinking",
            "/thinking_tokens",
            "/thinkingTokens",
        ],
    )
    .unwrap_or(0);
    let tool_tokens = value_u64_at(tokens, &["/tool", "/tool_tokens", "/toolTokens"]).unwrap_or(0);
    let cached_tokens = value_u64_at(
        tokens,
        &[
            "/cached",
            "/cached_tokens",
            "/cachedTokens",
            "/cached_input_tokens",
            "/cachedInputTokens",
        ],
    )
    .unwrap_or(0);
    let completion_tokens = output_tokens + thinking_tokens + tool_tokens;
    let total_tokens = value_u64_at(tokens, &["/total", "/total_tokens", "/totalTokens"])
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    Some(SessionUsage {
        total_tokens,
        prompt_tokens,
        completion_tokens,
        cache_creation_tokens: 0,
        cached_tokens,
    })
}

pub(crate) fn resolve_codex_inherited_model(
    session_id: &str,
    model_by_id: &HashMap<String, String>,
    parent_by_id: &HashMap<String, String>,
) -> Option<String> {
    let mut seen = HashSet::new();
    let mut current = session_id.to_string();
    while seen.insert(current.clone()) {
        let parent = parent_by_id.get(&current)?;
        if let Some(model) = model_by_id.get(parent) {
            return Some(model.clone());
        }
        current = parent.clone();
    }
    None
}

pub(crate) fn json_compact_string_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("\"{key}\":\"");
    let start = line.find(&marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

pub(crate) fn json_compact_u64_field(object: &str, key: &str) -> Option<u64> {
    let marker = format!("\"{key}\":");
    let start = object.find(&marker)? + marker.len();
    let bytes = object.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    object[digits_start..i].parse().ok()
}

pub(crate) fn json_compact_object_for_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("\"{key}\":{{");
    let object_start = line.find(&marker)? + marker.len() - 1;
    let bytes = line.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for i in object_start..bytes.len() {
        let byte = bytes[i];
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&line[object_start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn codex_usage_from_compact_total_bucket(bucket: &str) -> Option<SessionUsage> {
    let prompt_tokens = json_compact_u64_field(bucket, "input_tokens")
        .or_else(|| json_compact_u64_field(bucket, "inputTokens"))?;
    let completion_tokens = json_compact_u64_field(bucket, "output_tokens")
        .or_else(|| json_compact_u64_field(bucket, "outputTokens"))
        .unwrap_or(0);
    let cached_tokens = json_compact_u64_field(bucket, "cached_input_tokens")
        .or_else(|| json_compact_u64_field(bucket, "cachedInputTokens"))
        .or_else(|| json_compact_u64_field(bucket, "cached_tokens"))
        .or_else(|| json_compact_u64_field(bucket, "cachedTokens"))
        .unwrap_or(0);
    let total_tokens = json_compact_u64_field(bucket, "total_tokens")
        .or_else(|| json_compact_u64_field(bucket, "totalTokens"))
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    Some(SessionUsage {
        total_tokens,
        prompt_tokens,
        completion_tokens,
        cache_creation_tokens: 0,
        cached_tokens,
    })
}

pub(crate) fn codex_token_count_usage_from_line(line: &str) -> Option<(i64, String, SessionUsage)> {
    if line.contains("\"type\":\"event_msg\"") && line.contains("\"type\":\"token_count\"") {
        let timestamp = json_compact_string_field(line, "timestamp")?;
        let event_ts = timestamp_sort_secs(timestamp);
        if event_ts <= 0 {
            return None;
        }
        let bucket = json_compact_object_for_key(line, "total_token_usage")
            .or_else(|| json_compact_object_for_key(line, "totalTokenUsage"))
            .or_else(|| json_compact_object_for_key(line, "total"))?;
        let usage = codex_usage_from_compact_total_bucket(bucket)?;
        return Some((event_ts, timestamp.to_string(), usage));
    }

    let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
        return None;
    };
    if obj.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = obj.get("payload")?;
    if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
        return None;
    }
    let parsed = codex_session_usage_from_payload(payload)?;
    let timestamp = value_str(&obj, "timestamp")?;
    let event_ts = timestamp_sort_secs(&timestamp);
    if event_ts <= 0 {
        return None;
    }
    Some((event_ts, timestamp, parsed))
}

pub(crate) fn codex_usage_baselines_from_file(
    path: &Path,
    cutoff_secs: &[i64],
) -> HashMap<i64, Option<SessionUsage>> {
    let mut cutoffs = cutoff_secs
        .iter()
        .copied()
        .filter(|cutoff| *cutoff > 0)
        .collect::<Vec<_>>();
    cutoffs.sort_unstable();
    cutoffs.dedup();

    let mut baselines = HashMap::new();
    let mut uncached_cutoffs = Vec::new();
    for cutoff in cutoffs {
        let Some(key) = session_list_cache_key("codex-parent-baseline", path, cutoff.to_string())
        else {
            uncached_cutoffs.push(cutoff);
            continue;
        };
        if let Some(usage) = cached_codex_parent_usage_baseline(&key) {
            baselines.insert(cutoff, usage);
        } else {
            uncached_cutoffs.push(cutoff);
        }
    }
    if uncached_cutoffs.is_empty() {
        return baselines;
    }

    let scanned = codex_usage_baselines_from_file_uncached(path, &uncached_cutoffs);
    for cutoff in uncached_cutoffs {
        let usage = scanned.get(&cutoff).copied().unwrap_or(None);
        if let Some(key) = session_list_cache_key("codex-parent-baseline", path, cutoff.to_string())
        {
            store_codex_parent_usage_baseline(key, usage);
        }
        baselines.insert(cutoff, usage);
    }
    baselines
}

pub(crate) fn codex_usage_baselines_from_file_uncached(
    path: &Path,
    cutoff_secs: &[i64],
) -> HashMap<i64, Option<SessionUsage>> {
    let mut cutoffs = cutoff_secs
        .iter()
        .copied()
        .filter(|cutoff| *cutoff > 0)
        .collect::<Vec<_>>();
    cutoffs.sort_unstable();
    cutoffs.dedup();

    let mut baselines = HashMap::new();
    if cutoffs.is_empty() {
        return baselines;
    }

    let Ok(file) = std::fs::File::open(path) else {
        for cutoff in cutoffs {
            baselines.insert(cutoff, None);
        }
        return baselines;
    };

    let reader = std::io::BufReader::new(file);
    let mut cutoff_index = 0usize;
    let mut selected = None;
    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() || !line.contains("\"token_count\"") {
            continue;
        }
        let Some((event_ts, _, parsed)) = codex_token_count_usage_from_line(line) else {
            continue;
        };

        while cutoff_index < cutoffs.len() && cutoffs[cutoff_index] < event_ts {
            baselines.insert(cutoffs[cutoff_index], selected);
            cutoff_index += 1;
        }
        if cutoff_index >= cutoffs.len() {
            break;
        }

        selected = Some(parsed);
    }

    while cutoff_index < cutoffs.len() {
        baselines.insert(cutoffs[cutoff_index], selected);
        cutoff_index += 1;
    }

    baselines
}

pub(crate) fn codex_parent_baseline_for_summary(
    summary: &CodexSessionListSummary,
    exact_parent_baselines: &HashMap<(String, i64), Option<SessionUsage>>,
) -> Option<SessionUsage> {
    let parent_id = summary.lineage.parent_id.as_deref()?;

    let cutoff = summary
        .created_at
        .as_deref()
        .map(timestamp_sort_secs)
        .unwrap_or(0);
    if cutoff > 0 {
        let exact_key = (parent_id.to_string(), cutoff);
        if let Some(exact_baseline) = exact_parent_baselines.get(&exact_key) {
            return Some(exact_baseline.unwrap_or_default());
        }
    }

    None
}

/// Daily usage for a forked session. The parse-time buckets counted the
/// first usage event's cumulative reading from zero, but for a fork that
/// reading still contains the parent's history — remove the parent
/// baseline from the first event's day bucket.
pub(crate) fn codex_daily_usage_with_baseline(
    summary: &CodexSessionListSummary,
    baseline: Option<SessionUsage>,
) -> BTreeMap<String, SessionUsage> {
    let mut daily = summary.daily_usage.clone();
    if let (Some(first), Some(baseline)) = (summary.first_usage_event.as_ref(), baseline) {
        if !baseline.is_empty() {
            let day = usage_day_from_timestamp(first.timestamp.as_deref())
                .or_else(|| usage_day_from_timestamp(summary.file_updated_at.as_deref()));
            if let Some(day) = day {
                if let Some(bucket) = daily.get_mut(&day) {
                    *bucket = bucket.saturating_sub(baseline);
                    if bucket.is_empty() {
                        daily.remove(&day);
                    }
                }
            }
        }
    }
    if daily.is_empty() && !summary.usage.is_empty() {
        let day = usage_day_from_timestamp(summary.created_at.as_deref())
            .or_else(|| usage_day_from_timestamp(summary.file_updated_at.as_deref()));
        if let Some(day) = day {
            let usage = baseline
                .map(|baseline| summary.usage.saturating_sub(baseline))
                .unwrap_or(summary.usage);
            if !usage.is_empty() {
                daily.insert(day, usage);
            }
        }
    }
    daily
}

#[derive(Default)]
pub(crate) struct CodexSessionListAccumulator {
    id: Option<String>,
    created_at: Option<String>,
    session_cwd: Option<String>,
    turn_cwd: Option<String>,
    command_cwd: Option<String>,
    model: Option<String>,
    lineage: SessionLineageMetadata,
    provider: Option<String>,
    usage: SessionUsage,
    first_usage_event: Option<CodexUsageEvent>,
    // Deltas from events without a parseable timestamp; folded into the
    // file-mtime day bucket at finish(), matching how the old
    // event-replay path bucketed undated events.
    undated_usage: SessionUsage,
    daily_usage: BTreeMap<String, SessionUsage>,
    goal: Option<SessionGoal>,
    task_started_turns: u64,
    saw_user_message_event: bool,
    event_user_turns: Vec<Option<String>>,
    fallback_user_turns: Vec<Option<String>>,
}

impl CodexSessionListAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            provider: Some("Codex".to_string()),
            ..Self::default()
        }
    }

    pub(crate) fn process_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() || !codex_line_may_affect_session_list(line) {
            return;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "session_meta" => {
                if let Some(payload) = obj.get("payload") {
                    self.id = self.id.take().or_else(|| value_str(payload, "id"));
                    self.lineage
                        .merge_missing_from(session_lineage_from_codex_payload(payload));
                    self.created_at = self
                        .created_at
                        .take()
                        .or_else(|| value_str(payload, "timestamp"));
                    if let Some(value) = value_str(payload, "cwd") {
                        if self.session_cwd.is_none() {
                            self.session_cwd = Some(value);
                        }
                    }
                    self.model = self.model.take().or_else(|| value_str(payload, "model"));
                    self.provider = value_str(payload, "model_provider").or(self.provider.take());
                }
            }
            "turn_context" => {
                if let Some(payload) = obj.get("payload") {
                    if let Some(value) = value_str(payload, "cwd") {
                        if self.session_cwd.is_none() {
                            self.session_cwd = Some(value.clone());
                        }
                        self.turn_cwd = Some(value);
                    }
                    self.model = self.model.take().or_else(|| value_str(payload, "model"));
                }
            }
            "event_msg" => {
                if let Some(payload) = obj.get("payload") {
                    let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if payload_type.starts_with("exec_command") {
                        if let Some(value) = value_str(payload, "cwd") {
                            self.command_cwd = Some(value);
                        }
                    }
                    match payload_type {
                        "task_started" => {
                            self.task_started_turns += 1;
                        }
                        "token_count" => {
                            if let Some(parsed) = codex_session_usage_from_payload(payload) {
                                self.record_token_usage(value_str(&obj, "timestamp"), parsed);
                            }
                        }
                        "thread_goal_updated" => {
                            self.goal = codex_session_goal_from_thread_payload(payload);
                        }
                        "thread_goal_cleared" => {
                            self.goal = None;
                        }
                        "user_message" => {
                            self.saw_user_message_event = true;
                            let text = value_str(payload, "message")
                                .filter(|s| !s.trim().is_empty())
                                .map(|s| compact_text(&s, 180));
                            self.event_user_turns.push(text);
                        }
                        "thread_rolled_back" => {
                            let num_turns = payload
                                .get("num_turns")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            for _ in 0..num_turns {
                                let _ = self.event_user_turns.pop();
                                let _ = self.fallback_user_turns.pop();
                            }
                            self.task_started_turns =
                                self.task_started_turns.saturating_sub(num_turns);
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Some(payload) = obj.get("payload") {
                    if let Some(value) = codex_exec_command_workdir(payload) {
                        self.command_cwd = Some(value);
                    }
                    if let Some((role, text)) = codex_payload_text(payload) {
                        if role == "user" && !is_codex_injected_user_text(&text) {
                            self.fallback_user_turns
                                .push(Some(compact_text(&text, 180)));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn clear_token_usage(&mut self) {
        self.usage = SessionUsage::default();
        self.first_usage_event = None;
        self.undated_usage = SessionUsage::default();
        self.daily_usage.clear();
    }

    pub(crate) fn record_token_usage(&mut self, timestamp: Option<String>, parsed: SessionUsage) {
        let delta = parsed.saturating_sub(self.usage);
        if !delta.is_empty() {
            if let Some(day) = usage_day_from_timestamp(timestamp.as_deref()) {
                self.daily_usage.entry(day).or_default().add(delta);
            } else {
                self.undated_usage.add(delta);
            }
        }
        self.usage = parsed;
        if self.first_usage_event.is_none() {
            self.first_usage_event = Some(CodexUsageEvent {
                timestamp,
                usage: parsed,
            });
        }
    }

    pub(crate) fn finish(self, path: &Path) -> Option<CodexSessionListSummary> {
        let id = self.id?;
        let task = self
            .event_user_turns
            .iter()
            .find_map(|t| t.clone())
            .or_else(|| self.fallback_user_turns.iter().find_map(|t| t.clone()));
        let turns = if self.saw_user_message_event {
            self.event_user_turns.len() as u64
        } else if self.task_started_turns > 0 {
            self.task_started_turns
        } else if !self.fallback_user_turns.is_empty() {
            self.fallback_user_turns.len() as u64
        } else {
            0
        };
        let effective_cwd = self
            .command_cwd
            .or(self.turn_cwd)
            .or_else(|| self.session_cwd.clone());
        let file_updated_at = file_mtime_string(path);
        let mut daily_usage = self.daily_usage;
        if !self.undated_usage.is_empty() {
            if let Some(day) = usage_day_from_timestamp(file_updated_at.as_deref()) {
                daily_usage.entry(day).or_default().add(self.undated_usage);
            }
        }
        Some(CodexSessionListSummary {
            id,
            created_at: self.created_at,
            session_cwd: self.session_cwd,
            effective_cwd,
            model: self.model,
            lineage: self.lineage,
            provider: self.provider,
            usage: self.usage,
            first_usage_event: self.first_usage_event,
            daily_usage,
            goal: self.goal,
            task,
            turns,
            file_updated_at,
            bytes: file_size(path),
        })
    }
}

pub(crate) fn process_codex_session_list_prefix(
    path: &Path,
    acc: &mut CodexSessionListAccumulator,
) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut bytes_read = 0u64;
    for _ in 0..CODEX_SESSION_LIST_PREFIX_LINE_LIMIT {
        let mut line = String::new();
        let Ok(n) = reader.read_line(&mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(n as u64);
        acc.process_line(&line);
        if bytes_read >= CODEX_SESSION_LIST_PREFIX_READ_LIMIT {
            break;
        }
    }
}

pub(crate) fn process_codex_token_counts_full(path: &Path, acc: &mut CodexSessionListAccumulator) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let reader = std::io::BufReader::new(file);
    let mut saw_usage = false;
    let mut parsed_events = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() || !line.contains("\"token_count\"") {
            continue;
        }
        let Some((_, timestamp, usage)) = codex_token_count_usage_from_line(line) else {
            continue;
        };
        saw_usage = true;
        parsed_events.push((timestamp, usage));
    }
    if !saw_usage {
        return;
    }
    acc.clear_token_usage();
    for (timestamp, usage) in parsed_events {
        acc.record_token_usage(Some(timestamp), usage);
    }
}

pub(crate) fn codex_session_list_summary_from_excerpt(
    path: &Path,
) -> Option<CodexSessionListSummary> {
    let len = file_size(path);
    let mut acc = CodexSessionListAccumulator::new();
    if len <= EXTERNAL_SESSION_READ_LIMIT.saturating_mul(2) {
        let contents = read_text_head_tail(
            path,
            EXTERNAL_SESSION_READ_LIMIT,
            EXTERNAL_SESSION_READ_LIMIT,
        )?;
        for line in contents.lines() {
            acc.process_line(line);
        }
    } else {
        process_codex_session_list_prefix(path, &mut acc);
        process_codex_token_counts_full(path, &mut acc);
        if let Some(tail) = read_text_tail(path, EXTERNAL_SESSION_READ_LIMIT) {
            for line in tail.lines() {
                acc.process_line(line);
            }
        }
    }
    acc.finish(path)
}

pub(crate) fn codex_session_list_summary_from_file(path: &Path) -> Option<CodexSessionListSummary> {
    let key = session_list_cache_key("codex", path, "")?;
    if let Some(entry) = cached_codex_session_list_entry(&key) {
        return Some(entry.summary);
    }

    let summary = codex_session_list_summary_from_excerpt(path)?;
    store_codex_session_list_entry(key, summary.clone());
    Some(summary)
}

/// Resolve Codex's home directory for a home-scoped session scan.
///
/// Codex writes session rollouts under `$CODEX_HOME` when that env var is set
/// (common on managed/headless installs), so the dashboard scan for the active
/// user must honor it rather than assuming `~/.codex`. For explicit alternate
/// homes, keep the scan scoped to that home; otherwise tests and targeted
/// home scans can accidentally read the live user's Codex sessions.
pub(crate) fn codex_dir(home: &Path) -> PathBuf {
    if home != crate::platform::home_dir() {
        return home.join(".codex");
    }

    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".codex"))
}

#[allow(dead_code)]
pub(crate) fn list_codex_sessions(home: &Path) -> Vec<serde_json::Value> {
    list_codex_sessions_with_limit(home, EXTERNAL_SESSION_SCAN_LIMIT)
}

pub(crate) fn read_codex_session_index_for_list(index_path: &Path) -> Option<String> {
    read_text_tail(index_path, CODEX_SESSION_INDEX_TAIL_READ_LIMIT)
}

pub(crate) fn list_codex_index_skeleton_sessions_with_limit(
    home: &Path,
    limit: usize,
) -> Vec<serde_json::Value> {
    let codex = codex_dir(home);
    let index_path = codex.join("session_index.jsonl");
    let Some(contents) = read_codex_session_index_for_list(&index_path) else {
        return Vec::new();
    };
    let mut rows: HashMap<String, serde_json::Value> = HashMap::new();
    for line in contents.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(id) = value_str(&obj, "id") else {
            continue;
        };
        let updated_at = value_str(&obj, "updated_at");
        let name = codex_thread_display_name(value_str(&obj, "thread_name"));
        rows.insert(
            id.clone(),
            external_session_json(
                "codex",
                "Codex",
                id.clone(),
                id,
                None,
                updated_at,
                name,
                None,
                "Codex",
                None,
                0,
                None,
                None,
                None,
                0,
            ),
        );
    }
    let deleted_external_sessions = read_deleted_external_sessions(home);
    let mut rows = rows.into_values().collect::<Vec<_>>();
    if !deleted_external_sessions.is_empty() {
        rows.retain(|session| {
            !session_matches_deleted_external(session, &deleted_external_sessions)
        });
    }
    crate::session_names::apply_session_name_overlays(home, &mut rows);
    crate::session_config::apply_overlays_to_sessions(home, &mut rows);
    sort_sessions_newest_first(&mut rows);
    rows.truncate(limit);
    rows
}

pub(crate) fn list_codex_sessions_with_limit(
    home: &Path,
    scan_limit: usize,
) -> Vec<serde_json::Value> {
    let codex = codex_dir(home);
    let mut rows: HashMap<String, serde_json::Value> = HashMap::new();
    let mut model_by_id: HashMap<String, String> = HashMap::new();
    let mut parent_by_id: HashMap<String, String> = HashMap::new();
    let mut path_by_id: HashMap<String, PathBuf> = HashMap::new();
    let index_path = codex.join("session_index.jsonl");
    if let Some(contents) = read_codex_session_index_for_list(&index_path) {
        for line in contents.lines() {
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(id) = value_str(&obj, "id") else {
                continue;
            };
            let updated_at = value_str(&obj, "updated_at");
            let name = codex_thread_display_name(value_str(&obj, "thread_name"));
            rows.insert(
                id.clone(),
                external_session_json(
                    "codex",
                    "Codex",
                    id.clone(),
                    id,
                    None,
                    updated_at,
                    name,
                    None,
                    "Codex",
                    None,
                    0,
                    None,
                    None,
                    Some(index_path.to_string_lossy().to_string()),
                    file_size(&index_path),
                ),
            );
        }
    }

    let mut files = collect_recent_files(&codex.join("sessions"), ".jsonl", scan_limit);
    files.extend(collect_recent_files(
        &codex.join("archived_sessions"),
        ".jsonl",
        scan_limit,
    ));
    files.sort_by_key(|b| std::cmp::Reverse(file_mtime_secs(b)));
    files.truncate(scan_limit);
    let mut summaries = Vec::new();
    for path in files {
        let Some(summary) = codex_session_list_summary_from_file(&path) else {
            continue;
        };
        let id = summary.id.clone();
        if let Some(model) = summary.model.clone() {
            model_by_id.insert(id.clone(), model);
        }
        if let Some(parent_id) = summary.lineage.parent_id.clone() {
            parent_by_id.insert(id, parent_id);
        }
        path_by_id.insert(summary.id.clone(), path.clone());
        summaries.push((path, summary));
    }

    let mut parent_cutoffs_by_id: HashMap<String, Vec<i64>> = HashMap::new();
    for (_, summary) in &summaries {
        let Some(parent_id) = summary.lineage.parent_id.as_ref() else {
            continue;
        };
        if !path_by_id.contains_key(parent_id) {
            continue;
        }
        let cutoff = summary
            .created_at
            .as_deref()
            .map(timestamp_sort_secs)
            .unwrap_or(0);
        if cutoff > 0 {
            parent_cutoffs_by_id
                .entry(parent_id.clone())
                .or_default()
                .push(cutoff);
        }
    }

    let mut parent_cutoffs = parent_cutoffs_by_id
        .into_iter()
        .filter_map(|(parent_id, cutoffs)| {
            path_by_id
                .get(&parent_id)
                .map(|path| (parent_id, file_size(path), cutoffs))
        })
        .collect::<Vec<_>>();
    parent_cutoffs.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then(a.1.cmp(&b.1)));

    let mut exact_parent_baselines: HashMap<(String, i64), Option<SessionUsage>> = HashMap::new();
    let mut remaining_exact_scan_budget = CODEX_PARENT_BASELINE_SCAN_BUDGET_BYTES;
    for (parent_id, parent_bytes, cutoffs) in parent_cutoffs {
        if parent_bytes > CODEX_PARENT_BASELINE_MAX_FILE_BYTES
            || parent_bytes > remaining_exact_scan_budget
        {
            continue;
        }
        let Some(parent_path) = path_by_id.get(&parent_id) else {
            continue;
        };
        remaining_exact_scan_budget = remaining_exact_scan_budget.saturating_sub(parent_bytes);
        for (cutoff, usage) in codex_usage_baselines_from_file(parent_path, &cutoffs) {
            exact_parent_baselines.insert((parent_id.clone(), cutoff), usage);
        }
    }

    for (path, summary) in summaries {
        let id = summary.id.clone();
        let existing = rows.get(&id);
        let existing_task = existing
            .and_then(|v| value_str(v, "task"))
            .filter(|s| !is_codex_injected_user_text(s));
        let existing_name = existing.and_then(|v| value_str(v, "name"));
        let existing_updated_at = existing.and_then(|v| value_str(v, "updated_at"));
        let created_at = summary
            .created_at
            .clone()
            .or_else(|| summary.file_updated_at.clone());
        let updated_at = summary
            .file_updated_at
            .clone()
            .or(existing_updated_at)
            .or_else(|| created_at.clone());
        let project_root = derive_project_root_from_cwd(
            summary
                .session_cwd
                .as_deref()
                .or(summary.effective_cwd.as_deref()),
        );
        let mut session = external_session_json(
            "codex",
            "Codex",
            id.clone(),
            id.clone(),
            created_at,
            updated_at,
            existing_name,
            summary.task.clone().or(existing_task),
            summary.provider.as_deref().unwrap_or("Codex"),
            summary.model.clone(),
            summary.turns,
            project_root,
            summary.effective_cwd.clone(),
            Some(path.to_string_lossy().to_string()),
            summary.bytes,
        );
        summary.lineage.apply_to_session_json(&mut session);
        let parent_baseline = codex_parent_baseline_for_summary(&summary, &exact_parent_baselines);
        let usage = parent_baseline
            .map(|baseline| summary.usage.saturating_sub(baseline))
            .unwrap_or(summary.usage);
        apply_session_usage(&mut session, usage, summary.model.as_deref());
        let daily_usage = if summary.lineage.parent_id.is_some() {
            codex_daily_usage_with_baseline(&summary, parent_baseline)
        } else {
            summary.daily_usage.clone()
        };
        apply_session_daily_usage(&mut session, &daily_usage, summary.model.as_deref());
        if let Some(goal) = summary.goal.as_ref() {
            if let Some(obj) = session.as_object_mut() {
                obj.insert("goal".to_string(), serde_json::json!(goal));
                obj.insert("session_goal".to_string(), serde_json::json!(goal));
            }
        }
        rows.insert(id, session);
    }

    let ids_missing_model = rows
        .iter()
        .filter_map(|(id, session)| {
            if value_str(session, "model").is_none() {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for id in ids_missing_model {
        let Some(model) = resolve_codex_inherited_model(&id, &model_by_id, &parent_by_id) else {
            continue;
        };
        if let Some(session) = rows.get_mut(&id) {
            apply_session_model_and_reprice(session, &model);
        }
    }

    rows.into_values().collect()
}

pub(crate) fn claude_session_list_row_from_file(path: &Path) -> Option<serde_json::Value> {
    let key = session_list_cache_key("claude-code", path, "")?;
    if let Some(row) = cached_session_list_row(&key) {
        return Some(row);
    }

    let session_id = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut created_at = None;
    let mut updated_at = None;
    let mut session_cwd = None;
    let mut cwd = None;
    let mut task = None;
    let mut model = None;
    let mut usage = SessionUsage::default();
    let mut daily_usage: BTreeMap<String, SessionUsage> = BTreeMap::new();
    let mut seen_usage = HashSet::new();
    let mut turns = 0u64;
    for (line_idx, line_result) in reader.lines().enumerate() {
        let Ok(line) = line_result else {
            continue;
        };
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        created_at = created_at.or_else(|| value_str(&obj, "timestamp"));
        updated_at = value_str(&obj, "timestamp").or(updated_at);
        if let Some(value) = value_str(&obj, "cwd") {
            if session_cwd.is_none() {
                session_cwd = Some(value.clone());
            }
            cwd = Some(value);
        }
        if obj.get("type").and_then(|v| v.as_str()) == Some("user") {
            turns += 1;
            if task.is_none() {
                if let Some(msg) = obj.get("message") {
                    if let Some(content) = msg.get("content").and_then(message_content_text) {
                        // Supervised sessions carry the Intendant bootstrap
                        // addendum on their first prompt; keep it out of the
                        // session title.
                        let user_text = content
                            .split(
                                crate::external_agent::claude_code::CLAUDE_CODE_BOOTSTRAP_ADDENDUM_MARKER,
                            )
                            .next()
                            .unwrap_or(&content)
                            .trim_end();
                        task = Some(compact_text(user_text, 180));
                    }
                }
            }
        }
        if let Some(msg) = obj.get("message") {
            if model.is_none() {
                model = msg
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            if let Some(parsed) = msg.get("usage").and_then(claude_usage_from_message_usage) {
                let key = value_str(&obj, "requestId")
                    .or_else(|| value_str(msg, "id"))
                    .unwrap_or_else(|| format!("line-{line_idx}"));
                if seen_usage.insert(key) {
                    usage.add(parsed);
                    if let Some(day) =
                        usage_day_from_timestamp(value_str(&obj, "timestamp").as_deref())
                    {
                        daily_usage.entry(day).or_default().add(parsed);
                    }
                }
            }
        }
    }
    let effective_cwd = cwd.or_else(|| session_cwd.clone());
    let project_root =
        derive_project_root_from_cwd(session_cwd.as_deref().or(effective_cwd.as_deref()));
    let mut session = external_session_json(
        "claude-code",
        "Claude Code",
        session_id.clone(),
        session_id,
        created_at
            .or_else(|| updated_at.clone())
            .or_else(|| file_mtime_string(path)),
        file_mtime_string(path).or(updated_at),
        None,
        task,
        "Claude Code",
        model.clone(),
        turns,
        project_root,
        effective_cwd,
        Some(path.to_string_lossy().to_string()),
        file_size(path),
    );
    apply_session_usage(&mut session, usage, model.as_deref());
    apply_session_daily_usage(&mut session, &daily_usage, model.as_deref());
    store_session_list_row(key, &session);
    Some(session)
}

#[allow(dead_code)]
pub(crate) fn list_claude_sessions(home: &Path) -> Vec<serde_json::Value> {
    list_claude_sessions_with_limit(home, EXTERNAL_SESSION_SCAN_LIMIT)
}

pub(crate) fn list_claude_sessions_with_limit(
    home: &Path,
    scan_limit: usize,
) -> Vec<serde_json::Value> {
    let files = collect_recent_files(&home.join(".claude").join("projects"), ".jsonl", scan_limit);
    let mut rows = Vec::new();
    for path in files {
        if let Some(session) = claude_session_list_row_from_file(&path) {
            rows.push(session);
        }
    }
    rows
}

pub(crate) fn gemini_project_roots(home: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let path = home.join(".gemini").join("projects.json");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return out;
    };
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return out;
    };
    let Some(projects) = obj.get("projects").and_then(|v| v.as_object()) else {
        return out;
    };
    for (root, alias) in projects {
        if let Some(alias) = alias.as_str() {
            out.insert(alias.to_string(), root.to_string());
        }
    }
    out
}

pub(crate) fn gemini_session_list_row_from_file(
    path: &Path,
    roots: &HashMap<String, String>,
    roots_fingerprint: &str,
) -> Option<serde_json::Value> {
    if path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        != Some("chats")
    {
        return None;
    }
    let key = session_list_cache_key("gemini", path, roots_fingerprint)?;
    if let Some(row) = cached_session_list_row(&key) {
        return Some(row);
    }

    let contents = std::fs::read_to_string(path).ok()?;
    let obj = serde_json::from_str::<serde_json::Value>(&contents).ok()?;
    let session_id = value_str(&obj, "sessionId")?;
    let alias = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());
    let mut task = None;
    let mut turns = 0u64;
    let mut model = value_str(&obj, "model");
    let mut usage = SessionUsage::default();
    let mut daily_usage: BTreeMap<String, SessionUsage> = BTreeMap::new();
    let session_started_at = value_str(&obj, "startTime");
    if let Some(messages) = obj.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            model = model.or_else(|| value_str(msg, "model"));
            if let Some(parsed) = msg.get("tokens").and_then(gemini_usage_from_tokens) {
                usage.add(parsed);
                let timestamp = value_str(msg, "timestamp")
                    .or_else(|| value_str(msg, "createdAt"))
                    .or_else(|| value_str(msg, "time"))
                    .or_else(|| session_started_at.clone());
                if let Some(day) = usage_day_from_timestamp(timestamp.as_deref()) {
                    daily_usage.entry(day).or_default().add(parsed);
                }
            }
            let role = msg
                .get("role")
                .or_else(|| msg.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if role == "user" {
                turns += 1;
                if task.is_none() {
                    let text = msg
                        .get("text")
                        .or_else(|| msg.get("message"))
                        .or_else(|| msg.get("content"))
                        .and_then(message_content_text);
                    if let Some(text) = text {
                        task = Some(compact_text(&text, 180));
                    }
                }
            }
        }
    }
    let project_root = alias.as_ref().and_then(|a| roots.get(a).cloned());
    let cwd = project_root.clone();
    let mut session = external_session_json(
        "gemini",
        "Gemini CLI",
        session_id.clone(),
        session_id,
        value_str(&obj, "startTime").or_else(|| file_mtime_string(path)),
        file_mtime_string(path),
        None,
        task,
        "Gemini CLI",
        model.clone(),
        turns,
        project_root,
        cwd,
        Some(path.to_string_lossy().to_string()),
        file_size(path),
    );
    apply_session_usage(&mut session, usage, model.as_deref());
    apply_session_daily_usage(&mut session, &daily_usage, model.as_deref());
    store_session_list_row(key, &session);
    Some(session)
}

#[allow(dead_code)]
pub(crate) fn list_gemini_sessions(home: &Path) -> Vec<serde_json::Value> {
    list_gemini_sessions_with_limit(home, EXTERNAL_SESSION_SCAN_LIMIT)
}

pub(crate) fn list_gemini_sessions_with_limit(
    home: &Path,
    scan_limit: usize,
) -> Vec<serde_json::Value> {
    let roots = gemini_project_roots(home);
    let roots_fingerprint =
        file_dependency_fingerprint(&home.join(".gemini").join("projects.json"));
    let files = collect_recent_files(&home.join(".gemini").join("tmp"), ".json", scan_limit);
    let mut rows = Vec::new();
    for path in files {
        if let Some(session) = gemini_session_list_row_from_file(&path, &roots, &roots_fingerprint)
        {
            rows.push(session);
        }
    }
    rows
}

pub(crate) fn codex_session_file_id(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).ok()?;
        if bytes == 0 {
            return None;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            return obj
                .get("payload")
                .and_then(|payload| value_str(payload, "id"));
        }
    }
}

pub(crate) fn find_codex_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    let codex = codex_dir(home);
    let mut files = Vec::new();
    collect_files(&codex.join("sessions"), ".jsonl", &mut files);
    collect_files(&codex.join("archived_sessions"), ".jsonl", &mut files);

    if let Some(path) = files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(session_id))
                && codex_session_file_id(path).as_deref() == Some(session_id)
        })
        .cloned()
    {
        return Some(path);
    }

    files
        .into_iter()
        .find(|path| codex_session_file_id(path).as_deref() == Some(session_id))
}

#[allow(dead_code)]
pub(crate) fn external_session_detail_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<String> {
    external_session_detail_from_home_with_limit(home, source, session_id, None)
}

#[allow(dead_code)]
pub(crate) fn external_session_detail_from_home_with_limit(
    home: &Path,
    source: &str,
    session_id: &str,
    limit: Option<usize>,
) -> Option<String> {
    external_session_detail_from_home_with_page(home, source, session_id, limit, None)
}

pub(crate) fn external_session_detail_from_home_with_page(
    home: &Path,
    source: &str,
    session_id: &str,
    limit: Option<usize>,
    before: Option<usize>,
) -> Option<String> {
    let mut entries = external_session_entries_from_home(home, source, session_id)?;
    let effective_limit = limit.or(Some(EXTERNAL_SESSION_DETAIL_DEFAULT_ENTRY_LIMIT));
    let mut page = session_detail_page_entries(entries, effective_limit, before);
    entries = page.entries;
    for entry in &mut entries {
        compact_replay_entry_text_fields_for_websocket(entry);
    }
    page.entries = entries;

    Some(
        serde_json::json!({
            "session_id": session_id,
            "transcript_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
            "entries": page.entries,
            "total_entries": page.total_entries,
            "page_start": page.page_start,
            "page_end": page.page_end,
            "has_older": page.page_start > 0,
            "frames": [],
        })
        .to_string(),
    )
}

pub(crate) fn external_transcript_source(provider_source: &str, role: &str) -> String {
    let role = role.trim().to_lowercase();
    if role == "user" {
        "user".to_string()
    } else {
        provider_source.to_string()
    }
}

pub(crate) fn external_transcript_cache(
) -> &'static Mutex<HashMap<String, ExternalTranscriptCacheEntry>> {
    EXTERNAL_TRANSCRIPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn external_transcript_path_key(path: &Path) -> String {
    let normalized = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalized.to_string_lossy().to_string()
}

pub(crate) fn external_transcript_cache_key(
    source: &str,
    session_id: &str,
    path: &Path,
) -> Option<ExternalTranscriptCacheKey> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(ExternalTranscriptCacheKey {
        source: source.to_string(),
        session_id: session_id.to_string(),
        path: external_transcript_path_key(path),
        len: metadata.len(),
        mtime_nanos: metadata_mtime_nanos(&metadata),
    })
}

pub(crate) fn external_transcript_cache_slot(key: &ExternalTranscriptCacheKey) -> String {
    format!("{}\0{}\0{}", key.source, key.session_id, key.path)
}

pub(crate) fn cached_external_transcript_entries(
    key: &ExternalTranscriptCacheKey,
) -> Option<Vec<serde_json::Value>> {
    let slot = external_transcript_cache_slot(key);
    let cache = external_transcript_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .get(&slot)
        .filter(|entry| &entry.key == key)
        .map(|entry| entry.entries.clone())
}

pub(crate) fn store_external_transcript_entries(
    key: ExternalTranscriptCacheKey,
    entries: &[serde_json::Value],
) {
    let slot = external_transcript_cache_slot(&key);
    let mut cache = external_transcript_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= EXTERNAL_TRANSCRIPT_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        ExternalTranscriptCacheEntry {
            key,
            entries: entries.to_vec(),
        },
    );
}

pub(crate) fn stable_external_transcript_event_id(
    source: &str,
    session_id: &str,
    entry: &serde_json::Value,
    index: usize,
) -> String {
    if let Some(id) = entry
        .get("event_id")
        .or_else(|| entry.get("eventId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return id.to_string();
    }
    let prefix = format!("external:{source}:{session_id}");
    if let Some(item_id) = entry
        .get("item_id")
        .or_else(|| entry.get("itemId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return format!("{prefix}:item:{item_id}");
    }
    if let Some(output_id) = entry
        .get("output_id")
        .or_else(|| entry.get("outputId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return format!("{prefix}:output:{output_id}");
    }
    if entry.get("kind").and_then(|value| value.as_str()) == Some("rollback_marker") {
        let ts = entry
            .get("ts")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let removed = entry
            .get("removed_turn_ids")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        return format!(
            "{prefix}:rollback:{}",
            short_stable_hash(&[ts, &removed, &index.to_string()])
        );
    }
    let normalized = serde_json::to_string(entry).unwrap_or_default();
    format!(
        "{prefix}:entry:{}",
        short_stable_hash(&[&index.to_string(), &normalized])
    )
}

pub(crate) fn annotate_external_transcript_entries(
    source: &str,
    session_id: &str,
    entries: &mut [serde_json::Value],
) {
    for (index, entry) in entries.iter_mut().enumerate() {
        let ts = entry
            .get("ts")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let event_id = stable_external_transcript_event_id(source, session_id, entry, index);
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        obj.entry("transcript_index".to_string())
            .or_insert_with(|| serde_json::Value::from(index as u64));
        if !ts.is_empty() {
            if let Some(ts_ms) = timestamp_millis_from_str(&ts) {
                obj.entry("ts_ms".to_string())
                    .or_insert_with(|| serde_json::Value::from(ts_ms));
            }
        }
        obj.entry("event_id".to_string())
            .or_insert_with(|| serde_json::Value::String(event_id));
        let delivery = delivery_class_for_replay_object(obj);
        obj.entry("delivery".to_string())
            .or_insert_with(|| serde_json::Value::String(delivery.to_string()));
    }
}

#[derive(Debug, Default)]
pub(crate) struct ReplayUserTurnRevisionState {
    active_count: u32,
    latest_revision_by_turn: HashMap<u32, u32>,
}

impl ReplayUserTurnRevisionState {
    pub(crate) fn record_next_turn(&mut self) -> (u32, u32) {
        let turn = self.active_count.saturating_add(1);
        let revision = self
            .latest_revision_by_turn
            .get(&turn)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.latest_revision_by_turn.insert(turn, revision);
        self.active_count = turn;
        (turn, revision)
    }

    pub(crate) fn rewind_from_turn(&mut self, first_user_turn_index: u32) {
        if first_user_turn_index == 0 || first_user_turn_index > self.active_count {
            return;
        }
        self.active_count = first_user_turn_index.saturating_sub(1);
    }

    pub(crate) fn current_turn(&self) -> Option<u32> {
        (self.active_count > 0).then_some(self.active_count)
    }
}

pub(crate) fn codex_synthetic_turn_id(user_turn_index: u32, user_turn_revision: u32) -> String {
    format!("turn-{user_turn_index}-r{user_turn_revision}")
}

pub(crate) fn codex_next_synthetic_item_id(
    synthetic_item_seq: &mut u64,
    turn_id: &str,
    item_type: &str,
) -> String {
    *synthetic_item_seq = synthetic_item_seq.saturating_add(1);
    format!("synthetic:{turn_id}:{item_type}:{}", *synthetic_item_seq)
}

pub(crate) fn codex_thread_item_value(
    item_id: &str,
    item_type: &str,
    turn_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": item_id,
        "type": item_type,
        "turn_id": turn_id,
        "status": "completed",
    })
}

pub(crate) fn codex_thread_turn_value(
    turn_id: &str,
    user_turn_index: Option<u32>,
    user_turn_revision: Option<u32>,
) -> serde_json::Value {
    serde_json::json!({
        "id": turn_id,
        "user_turn_index": user_turn_index,
        "user_turn_revision": user_turn_revision,
    })
}

pub(crate) fn codex_thread_history_change_value(
    changed_item: Option<serde_json::Value>,
    changed_turn: Option<serde_json::Value>,
    removed_turn_ids: Vec<String>,
) -> serde_json::Value {
    let changed_items = changed_item.into_iter().collect::<Vec<_>>();
    let changed_turns = changed_turn.into_iter().collect::<Vec<_>>();
    serde_json::json!({
        "changed_items": changed_items,
        "changed_turns": changed_turns,
        "removed_turn_ids": removed_turn_ids,
    })
}

pub(crate) fn apply_codex_thread_projection(
    entry: &mut serde_json::Value,
    item_id: &str,
    item_type: &str,
    turn_id: &str,
    user_turn_index: Option<u32>,
    user_turn_revision: Option<u32>,
) {
    let thread_item = codex_thread_item_value(item_id, item_type, turn_id);
    let changed_turn = codex_thread_turn_value(turn_id, user_turn_index, user_turn_revision);
    entry["item_id"] = serde_json::json!(item_id);
    entry["item_type"] = serde_json::json!(item_type);
    entry["turn_id"] = serde_json::json!(turn_id);
    entry["thread_item"] = thread_item.clone();
    entry["thread_history_change"] = codex_thread_history_change_value(
        Some(thread_item.clone()),
        Some(changed_turn.clone()),
        Vec::new(),
    );
    entry["changed_items"] = serde_json::json!([thread_item]);
    entry["changed_turns"] = serde_json::json!([changed_turn]);
    entry["removed_turn_ids"] = serde_json::json!([]);
}

pub(crate) fn codex_removed_turn_ids_for_user_turns(
    entries: &[serde_json::Value],
    user_turns: &[u32],
) -> Vec<String> {
    let wanted = user_turns.iter().copied().collect::<BTreeSet<_>>();
    let mut removed = BTreeSet::new();
    for entry in entries {
        let Some(turn_index) = entry
            .get("user_turn_index")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        if !wanted.contains(&turn_index) {
            continue;
        }
        if let Some(turn_id) = entry
            .get("turn_id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            removed.insert(turn_id.to_string());
        }
    }
    removed.into_iter().collect()
}

pub(crate) fn push_codex_transcript_message(
    entries: &mut Vec<serde_json::Value>,
    user_turn_revisions: &mut ReplayUserTurnRevisionState,
    pending_replacement_for_user_turn: &mut Option<u32>,
    synthetic_item_seq: &mut u64,
    item_id: Option<&str>,
    current_turn_id: Option<&str>,
    ts: &str,
    role: &str,
    text: String,
) {
    let normalized_role = match role.trim().to_lowercase().as_str() {
        "model" => "assistant".to_string(),
        other => other.to_string(),
    };
    if push_external_transcript_entry(entries, "codex", ts, role, text) {
    } else {
        return;
    }
    let mut user_turn_index = None;
    let mut user_turn_revision = None;
    if normalized_role == "user" {
        let (recorded_turn_index, recorded_turn_revision) = user_turn_revisions.record_next_turn();
        user_turn_index = Some(recorded_turn_index);
        user_turn_revision = Some(recorded_turn_revision);
        if let Some(entry) = entries.last_mut() {
            entry["user_turn_index"] = serde_json::json!(recorded_turn_index);
            entry["user_turn_revision"] = serde_json::json!(recorded_turn_revision);
            if let Some(turn) = pending_replacement_for_user_turn.take() {
                entry["replacement_for_user_turn_index"] = serde_json::json!(turn);
            }
        }
    }
    let item_type = if normalized_role == "user" {
        "user_message"
    } else {
        "agent_message"
    };
    let derived_turn_id = current_turn_id
        .map(str::to_string)
        .or_else(|| {
            user_turn_index
                .zip(user_turn_revision)
                .map(|(turn, revision)| codex_synthetic_turn_id(turn, revision))
        })
        .or_else(|| {
            user_turn_revisions
                .current_turn()
                .map(|turn| codex_synthetic_turn_id(turn, 1))
        })
        .unwrap_or_else(|| "turn-unknown".to_string());
    let derived_item_id = item_id.map(str::to_string).unwrap_or_else(|| {
        codex_next_synthetic_item_id(synthetic_item_seq, &derived_turn_id, item_type)
    });
    if let Some(entry) = entries.last_mut() {
        apply_codex_thread_projection(
            entry,
            &derived_item_id,
            item_type,
            &derived_turn_id,
            user_turn_index,
            user_turn_revision,
        );
    }
}

pub(crate) fn parse_codex_session_entries(path: &Path) -> Option<Vec<serde_json::Value>> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut user_turn_revisions = ReplayUserTurnRevisionState::default();
    let mut pending_replacement_for_user_turn: Option<u32> = None;
    let mut rollout_session_id: Option<String> = None;
    let mut current_turn_id: Option<String> = None;
    let mut synthetic_item_seq = 0_u64;
    let mut command_calls: HashMap<String, serde_json::Value> = HashMap::new();
    let canonical_user_message_events = codex_session_has_user_message_events(path);
    let canonical_assistant_response_items = codex_session_has_assistant_response_items(path);
    // (count_before, count_after) entry-count boundaries for each rollout item id,
    // so an item-anchor rewind can supersede exactly the entries after the anchor.
    let mut item_entry_boundaries: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || !codex_line_may_affect_replay(trimmed) {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            if let Some(payload) = obj.get("payload") {
                rollout_session_id = rollout_session_id.or_else(|| value_str(payload, "id"));
            }
        }
        if obj.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
            if let Some(payload) = obj.get("payload") {
                let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if payload_type == "task_started" {
                    current_turn_id = value_str(payload, "turn_id")
                        .or_else(|| value_str(payload, "turnId"))
                        .or(current_turn_id);
                    continue;
                }
                if payload_type == "thread_goal_updated" {
                    if let Some(session_id) =
                        codex_thread_goal_session_id(payload, rollout_session_id.as_deref())
                    {
                        entries.push(codex_session_goal_entry(
                            &value_str(&obj, "timestamp").unwrap_or_default(),
                            &session_id,
                            codex_session_goal_from_thread_payload(payload),
                        ));
                    }
                    continue;
                }
                if payload_type == "thread_goal_cleared" {
                    if let Some(session_id) =
                        codex_thread_goal_session_id(payload, rollout_session_id.as_deref())
                    {
                        entries.push(codex_session_goal_entry(
                            &value_str(&obj, "timestamp").unwrap_or_default(),
                            &session_id,
                            None,
                        ));
                    }
                    continue;
                }
                if payload_type == "thread_rolled_back" {
                    let turns = payload
                        .get("num_turns")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let anchor = codex_thread_rollback_anchor(payload);
                    let ts = value_str(&obj, "timestamp").unwrap_or_default();
                    let mut superseded_user_turns = Vec::new();
                    // `num_turns` is read from an untrusted on-disk rollout; bound the
                    // loop by the actual transcript size by breaking as soon as there
                    // is no further user turn to supersede, so a corrupt/huge count
                    // (e.g. u64::MAX) can't spin the request thread.
                    for _ in 0..turns {
                        match mark_latest_external_turn_superseded(&mut entries, &ts) {
                            Some(turn) => {
                                superseded_user_turns.push(turn);
                                user_turn_revisions.rewind_from_turn(turn);
                            }
                            None => break,
                        }
                    }
                    // Item-anchor rewinds (often num_turns == 0) discard the tail after
                    // the anchored item. If the anchor item was seen in this rollout,
                    // supersede every entry past its boundary so the transcript stops
                    // showing discarded entries as live.
                    if let Some((anchor_item_id, anchor_position)) = anchor.as_ref() {
                        if let Some(&(before, after)) =
                            item_entry_boundaries.get(anchor_item_id.as_str())
                        {
                            let start = if anchor_position == "before" {
                                before
                            } else {
                                after
                            };
                            for turn in
                                mark_external_entries_superseded_from(&mut entries, start, &ts)
                            {
                                user_turn_revisions.rewind_from_turn(turn);
                                superseded_user_turns.push(turn);
                            }
                        }
                    }
                    if let Some(replacement_turn) = superseded_user_turns.iter().copied().min() {
                        pending_replacement_for_user_turn = Some(replacement_turn);
                    }
                    if turns > 0 || anchor.is_some() {
                        let removed_turn_ids =
                            codex_removed_turn_ids_for_user_turns(&entries, &superseded_user_turns);
                        let thread_history_change =
                            codex_thread_history_change_value(None, None, removed_turn_ids.clone());
                        let content = match (turns, anchor.as_ref()) {
                            // Item-anchor rewinds carry num_turns==0 and do not mark
                            // any rendered entry superseded (entries are not item-id
                            // correlated), so the marker must not claim entries were
                            // retired — that would contradict the still-live transcript.
                            (0, Some((item_id, position))) => format!(
                                "Rewound to {position} item {item_id}."
                            ),
                            (1, Some((item_id, position))) => format!(
                                "Rewound 1 user turn and trimmed to {position} item {item_id}; overwritten entries are no longer active context."
                            ),
                            (_, Some((item_id, position))) => format!(
                                "Rewound {turns} user turns and trimmed to {position} item {item_id}; overwritten entries are no longer active context."
                            ),
                            (1, None) => {
                                "Rewound 1 user turn; overwritten entries are no longer active context."
                                    .to_string()
                            }
                            (_, None) => format!(
                                "Rewound {turns} user turns; overwritten entries are no longer active context."
                            ),
                        };
                        let mut marker = serde_json::json!({
                            "ts": ts,
                            "level": "warn",
                            "source": "system",
                            "content": content,
                            "kind": "rollback_marker",
                            "rollback_turns": turns,
                            "turns_removed": turns,
                            "removed_turn_ids": removed_turn_ids,
                            "thread_history_change": thread_history_change,
                            "changed_items": [],
                            "changed_turns": [],
                        });
                        if let Some((item_id, position)) = anchor {
                            marker["rollback_anchor_item_id"] = serde_json::json!(item_id);
                            marker["rollback_anchor_position"] = serde_json::json!(position);
                        }
                        entries.push(marker);
                    }
                    current_turn_id = None;
                    continue;
                }
            }
        }
        let ts = value_str(&obj, "timestamp").unwrap_or_default();
        let response_item_id = codex_response_item_id(&obj);
        let entries_before = entries.len();
        if let Some(payload) = obj.get("payload") {
            if let Some((call_id, command_projection)) = codex_function_call_projection(
                payload,
                response_item_id.as_deref(),
                current_turn_id.as_deref(),
            ) {
                command_calls.insert(call_id, command_projection);
            }
            if let Some((role, text)) = codex_event_message_text(payload) {
                if canonical_assistant_response_items && role == "assistant" {
                    continue;
                }
                push_codex_transcript_message(
                    &mut entries,
                    &mut user_turn_revisions,
                    &mut pending_replacement_for_user_turn,
                    &mut synthetic_item_seq,
                    response_item_id.as_deref(),
                    current_turn_id.as_deref(),
                    &ts,
                    &role,
                    text,
                );
                if role == "user" {
                    current_turn_id = entries
                        .last()
                        .and_then(|entry| entry.get("turn_id"))
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                }
            }
            if let Some((role, text)) = codex_payload_text(payload) {
                if canonical_user_message_events && role == "user" {
                    if let Some(item_id) = response_item_id.as_deref() {
                        item_entry_boundaries
                            .insert(item_id.to_string(), (entries_before, entries.len()));
                    }
                    continue;
                }
                push_codex_transcript_message(
                    &mut entries,
                    &mut user_turn_revisions,
                    &mut pending_replacement_for_user_turn,
                    &mut synthetic_item_seq,
                    response_item_id.as_deref(),
                    current_turn_id.as_deref(),
                    &ts,
                    &role,
                    text,
                );
                if role == "user" {
                    current_turn_id = entries
                        .last()
                        .and_then(|entry| entry.get("turn_id"))
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                }
            }
            if let Some((output_id, output)) = codex_function_call_output(payload) {
                let output_id_for_lookup = output_id.clone();
                let command_projection = output_id_for_lookup
                    .as_deref()
                    .and_then(|id| command_calls.get(id))
                    .cloned();
                let mut entry = serde_json::json!({
                    "ts": ts,
                    "event": "agent_output",
                    "level": "agent",
                    "source": "codex",
                    "kind": "agent_output",
                    "item_type": "command_execution",
                    "stdout": output,
                    "stderr": "",
                });
                if let Some(session_id) = rollout_session_id.as_deref() {
                    entry["session_id"] = serde_json::json!(session_id);
                }
                if let Some(output_id) = output_id.as_deref() {
                    entry["output_id"] = serde_json::json!(output_id);
                }
                let command_item_id = command_projection
                    .as_ref()
                    .and_then(|projection| projection.get("id"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .or_else(|| response_item_id.clone())
                    .or_else(|| output_id.clone())
                    .unwrap_or_else(|| {
                        codex_next_synthetic_item_id(
                            &mut synthetic_item_seq,
                            current_turn_id.as_deref().unwrap_or("turn-unknown"),
                            "command_execution",
                        )
                    });
                let command_turn_id = command_projection
                    .as_ref()
                    .and_then(|projection| projection.get("turn_id"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .or_else(|| current_turn_id.clone())
                    .unwrap_or_else(|| "turn-unknown".to_string());
                let thread_item = codex_thread_item_value(
                    &command_item_id,
                    "command_execution",
                    &command_turn_id,
                );
                let changed_turn = codex_thread_turn_value(&command_turn_id, None, None);
                let mut command_execution = command_projection.unwrap_or_else(|| {
                    serde_json::json!({
                        "id": command_item_id.clone(),
                        "call_id": output_id.clone(),
                        "type": "command_execution",
                    })
                });
                command_execution["id"] = serde_json::json!(command_item_id.clone());
                command_execution["status"] = serde_json::json!("completed");
                command_execution["output_id"] = serde_json::json!(output_id.clone());
                command_execution["completed_at_ms"] =
                    serde_json::json!(timestamp_millis_from_str(&ts));
                entry["item_id"] = serde_json::json!(command_item_id.clone());
                entry["command_item_id"] = serde_json::json!(command_item_id);
                entry["turn_id"] = serde_json::json!(command_turn_id);
                entry["thread_item"] = thread_item.clone();
                entry["thread_history_change"] = codex_thread_history_change_value(
                    Some(thread_item.clone()),
                    Some(changed_turn.clone()),
                    Vec::new(),
                );
                entry["changed_items"] = serde_json::json!([thread_item]);
                entry["changed_turns"] = serde_json::json!([changed_turn]);
                entry["removed_turn_ids"] = serde_json::json!([]);
                entry["command_execution"] = command_execution;
                entries.push(entry);
            }
        }
        // Record where this item sits in the entry stream so a later item-anchor
        // rewind targeting it can supersede entries before/after it precisely.
        if let Some(item_id) = response_item_id {
            item_entry_boundaries.insert(item_id, (entries_before, entries.len()));
        }
    }

    Some(entries)
}

pub(crate) fn codex_session_has_user_message_events(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains("\"user_message\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("event_msg")
            && obj
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(|v| v.as_str())
                == Some("user_message")
        {
            return true;
        }
    }
    false
}

pub(crate) fn codex_session_has_assistant_response_items(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains("\"assistant\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = obj.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        if payload.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if codex_payload_text(payload).is_some() {
            return true;
        }
    }
    false
}

pub(crate) fn parse_claude_session_entries(path: &Path) -> Option<Vec<serde_json::Value>> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(kind) = serde_json::from_str::<ExternalJsonLineKind<'_>>(trimmed) else {
            continue;
        };
        let typ = kind.kind.as_deref().unwrap_or("");
        if typ != "user" && typ != "assistant" {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let text = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(message_content_text)
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        entries.push(serde_json::json!({
            "ts": value_str(&obj, "timestamp").unwrap_or_default(),
            "level": if typ == "assistant" { "model" } else { "info" },
            "source": external_transcript_source("claude", typ),
            "content": text,
        }));
    }

    Some(entries)
}

pub(crate) fn parse_gemini_session_entries(
    path: &Path,
    session_id: &str,
) -> Option<Vec<serde_json::Value>> {
    let contents = std::fs::read_to_string(path).ok()?;
    let obj = serde_json::from_str::<serde_json::Value>(&contents).ok()?;
    if value_str(&obj, "sessionId").as_deref() != Some(session_id) {
        return None;
    }

    let mut entries = Vec::new();
    if let Some(messages) = obj.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg
                .get("role")
                .or_else(|| msg.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("message");
            let text = msg
                .get("text")
                .or_else(|| msg.get("message"))
                .or_else(|| msg.get("content"))
                .and_then(message_content_text)
                .unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            entries.push(serde_json::json!({
                "ts": value_str(msg, "timestamp").unwrap_or_default(),
                "level": if role == "assistant" || role == "model" { "model" } else { "info" },
                "source": external_transcript_source("gemini", role),
                "content": text,
            }));
        }
    }

    Some(entries)
}

pub(crate) fn find_claude_session_file_for_transcript(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_files(&home.join(".claude").join("projects"), ".jsonl", &mut files);
    files
        .into_iter()
        .find(|path| path.file_stem().and_then(|n| n.to_str()) == Some(session_id))
}

pub(crate) fn find_gemini_session_file_for_transcript(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_files(&home.join(".gemini").join("tmp"), ".json", &mut files);
    files.into_iter().find(|path| {
        if path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            != Some("chats")
        {
            return false;
        }
        let Ok(contents) = std::fs::read_to_string(path) else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&contents)
            .ok()
            .and_then(|obj| value_str(&obj, "sessionId"))
            .as_deref()
            == Some(session_id)
    })
}

pub(crate) fn parse_external_session_entries_from_file(
    source: &str,
    session_id: &str,
    path: &Path,
) -> Option<Vec<serde_json::Value>> {
    match source {
        "codex" => parse_codex_session_entries(path),
        "claude-code" => parse_claude_session_entries(path),
        "gemini" => parse_gemini_session_entries(path, session_id),
        _ => None,
    }
}

pub(crate) fn external_session_entries_from_file(
    source: &str,
    session_id: &str,
    path: &Path,
) -> Option<Vec<serde_json::Value>> {
    let key = external_transcript_cache_key(source, session_id, path)?;
    if let Some(entries) = cached_external_transcript_entries(&key) {
        return Some(entries);
    }

    let mut entries = parse_external_session_entries_from_file(source, session_id, path)?;
    annotate_external_transcript_entries(source, session_id, &mut entries);
    store_external_transcript_entries(key, &entries);
    Some(entries)
}

pub(crate) fn external_session_entries_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<Vec<serde_json::Value>> {
    let source = crate::session_names::normalize_source(source);
    let path = match source.as_str() {
        "codex" => find_codex_session_file(home, session_id),
        "claude-code" => find_claude_session_file_for_transcript(home, session_id),
        "gemini" => find_gemini_session_file_for_transcript(home, session_id),
        _ => None,
    }?;

    external_session_entries_from_file(&source, session_id, &path)
}

#[allow(dead_code)]
pub(crate) fn external_session_activity_replay_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
    limit: usize,
) -> Option<String> {
    external_session_activity_replay_from_home_with_attach(
        home, source, session_id, limit, true, true, false,
    )
}

pub(crate) fn external_session_activity_replay_for_websocket(
    source: &str,
    session_id: &str,
) -> Option<String> {
    external_session_activity_replay_from_home_with_attach(
        &crate::platform::home_dir(),
        source,
        session_id,
        WEBSOCKET_BOOTSTRAP_REPLAY_ENTRY_LIMIT,
        true,
        false,
        true,
    )
}

pub(crate) fn external_session_activity_replay_from_home_with_attach(
    home: &Path,
    source: &str,
    session_id: &str,
    limit: usize,
    include_attached: bool,
    include_context_snapshots: bool,
    compact_for_websocket: bool,
) -> Option<String> {
    let source = crate::session_names::normalize_source(source);
    let mut transcript = external_session_entries_from_home(home, &source, session_id)?;
    if compact_for_websocket {
        transcript.retain(|entry| {
            entry.get("event").and_then(|v| v.as_str()) != Some("context_snapshot")
        });
    }
    if limit > 0 {
        transcript = limited_session_detail_entries(transcript, Some(limit));
    }

    let mut entries = Vec::with_capacity(transcript.len() + 2);
    entries.push(serde_json::json!({
        "event": "replay_start",
        "session_id": session_id,
        "source": source,
        "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
        "event_id": format!("external:{source}:{session_id}:replay_start"),
        "delivery": "state",
    }));
    if include_attached {
        entries.push(serde_json::json!({
            "event": "session_attached",
            "session_id": session_id,
            "source": source,
            "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
            "event_id": format!("external:{source}:{session_id}:session_attached"),
            "delivery": "state",
        }));
    }

    for entry in transcript {
        if entry.get("event").and_then(|v| v.as_str()) == Some("session_goal") {
            let mut event = entry;
            if let Some(obj) = event.as_object_mut() {
                obj.insert(
                    "replay_semantics".to_string(),
                    serde_json::json!(EXTERNAL_TRANSCRIPT_SEMANTICS),
                );
            }
            entries.push(event);
            continue;
        }
        let content = entry
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|content| !content.is_empty())
            .or_else(|| {
                if entry.get("kind").and_then(|v| v.as_str()) == Some("agent_output") {
                    entry.get("stdout").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("");
        if content.is_empty() {
            continue;
        }
        let mut replay_entry = serde_json::json!({
            "event": "log_entry",
            "session_id": session_id,
            "ts": entry.get("ts").and_then(|v| v.as_str()).unwrap_or(""),
            "ts_ms": entry.get("ts_ms").and_then(|v| v.as_i64()),
            "event_id": entry.get("event_id").and_then(|v| v.as_str()),
            "delivery": entry.get("delivery").and_then(|v| v.as_str()),
            "level": entry.get("level").and_then(|v| v.as_str()).unwrap_or("info"),
            "source": entry.get("source").and_then(|v| v.as_str()).unwrap_or(source.as_str()),
            "content": content,
            "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
            "transcript_index": entry.get("transcript_index").and_then(|v| v.as_u64()),
            "user_turn_index": entry.get("user_turn_index").and_then(|v| v.as_u64()),
            "user_turn_revision": entry
                .get("user_turn_revision")
                .and_then(|v| v.as_u64()),
            "replacement_for_user_turn_index": entry
                .get("replacement_for_user_turn_index")
                .and_then(|v| v.as_u64()),
            "superseded": entry
                .get("superseded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "superseded_reason": entry
                .get("superseded_reason")
                .and_then(|v| v.as_str()),
            "kind": entry.get("kind").and_then(|v| v.as_str()),
            "item_id": entry.get("item_id").and_then(|v| v.as_str()),
            "item_type": entry.get("item_type").and_then(|v| v.as_str()),
            "turn_id": entry.get("turn_id").and_then(|v| v.as_str()),
            "output_id": entry.get("output_id").and_then(|v| v.as_str()),
            "command_item_id": entry
                .get("command_item_id")
                .and_then(|v| v.as_str()),
            "command_execution": entry.get("command_execution").cloned(),
            "thread_item": entry.get("thread_item").cloned(),
            "thread_history_change": entry.get("thread_history_change").cloned(),
            "changed_items": entry.get("changed_items").cloned(),
            "changed_turns": entry.get("changed_turns").cloned(),
            "removed_turn_ids": entry.get("removed_turn_ids").cloned(),
            "rollback_turns": entry.get("rollback_turns").and_then(|v| v.as_u64()),
            "turns_removed": entry.get("turns_removed").and_then(|v| v.as_u64()),
            "rollback_anchor_item_id": entry
                .get("rollback_anchor_item_id")
                .and_then(|v| v.as_str()),
            "rollback_anchor_position": entry
                .get("rollback_anchor_position")
                .and_then(|v| v.as_str()),
        });
        if compact_for_websocket {
            compact_replay_entry_text_fields_for_websocket(&mut replay_entry);
        }
        entries.push(replay_entry);
    }
    if include_context_snapshots {
        append_external_context_snapshot_replay_entries(home, &source, session_id, &mut entries);
    }
    compact_context_snapshot_entries_for_replay(&mut entries);

    Some(
        serde_json::json!({
            "t": "log_replay",
            "replay_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
            "entries": entries,
        })
        .to_string(),
    )
}

pub(crate) fn append_external_context_snapshot_replay_entries(
    home: &Path,
    source: &str,
    session_id: &str,
    entries: &mut Vec<serde_json::Value>,
) {
    if crate::session_names::normalize_source(source) != "codex" {
        return;
    }
    let mut seen = HashSet::new();
    let mut snapshot_entries = Vec::new();
    for dir in external_context_snapshot_replay_log_dirs(home, source, session_id) {
        let Ok(contents) = std::fs::read_to_string(dir.join("session.jsonl")) else {
            if seen.is_empty() {
                append_external_context_trace_replay_entries(
                    &dir,
                    session_id,
                    &mut seen,
                    &mut snapshot_entries,
                );
            }
            continue;
        };
        let external_replay_session_id = external_backend_session_id_from_replay(&contents);
        let wrapper_replay_session_id = replay_session_id_from_dir(&dir);
        let replay_session_id = external_replay_session_id
            .clone()
            .or_else(|| wrapper_replay_session_id.clone());
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if entry_json.get("event").and_then(|v| v.as_str()) != Some("context_snapshot") {
                continue;
            }
            let Some(entry) = context_snapshot_replay_entry_from_log_entry(
                &entry_json,
                &dir,
                replay_session_id.as_deref(),
                external_replay_session_id.as_deref(),
                wrapper_replay_session_id.as_deref(),
            ) else {
                continue;
            };
            if entry.get("event").and_then(|v| v.as_str()) != Some("context_snapshot") {
                continue;
            }
            if !context_snapshot_replay_entry_matches_session(&entry, session_id) {
                continue;
            }
            let key = context_snapshot_replay_entry_key(&entry);
            if seen.insert(key) {
                snapshot_entries.push(entry);
            }
        }
        if seen.is_empty() {
            append_external_context_trace_replay_entries(
                &dir,
                session_id,
                &mut seen,
                &mut snapshot_entries,
            );
        }
    }
    snapshot_entries.sort_by_key(context_snapshot_replay_entry_sort_key);
    entries.extend(snapshot_entries);
}

pub(crate) fn external_context_snapshot_replay_log_dirs(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen_dirs = HashSet::new();

    if let Some(path) = managed_context_named_log_dir(home, session_id) {
        push_external_context_replay_log_dir(&mut dirs, &mut seen_dirs, path);
    }
    for record in crate::external_wrapper_index::wrappers_for(home, source, session_id) {
        push_external_context_replay_log_dir(
            &mut dirs,
            &mut seen_dirs,
            PathBuf::from(record.log_path),
        );
    }
    for path in cached_intendant_log_dirs_for_session_id(session_id) {
        push_external_context_replay_log_dir(&mut dirs, &mut seen_dirs, path);
    }

    if dirs.is_empty() {
        for path in recent_intendant_log_dirs(home, EXTERNAL_CONTEXT_REPLAY_LOG_SCAN_LIMIT) {
            if managed_context_log_dir_mentions_session(&path, session_id) {
                push_external_context_replay_log_dir(&mut dirs, &mut seen_dirs, path);
            }
        }
    }

    dirs
}

pub(crate) fn push_external_context_replay_log_dir(
    dirs: &mut Vec<PathBuf>,
    seen_dirs: &mut HashSet<String>,
    path: PathBuf,
) {
    let key = std::fs::canonicalize(&path)
        .unwrap_or_else(|_| path.clone())
        .to_string_lossy()
        .to_string();
    if seen_dirs.insert(key) {
        dirs.push(path);
    }
}

pub(crate) fn recent_intendant_log_dirs(home: &Path, limit: usize) -> Vec<PathBuf> {
    if limit == 0 {
        return Vec::new();
    }
    let logs_dir = home.join(".intendant").join("logs");
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let mtime = file_mtime_secs(&path.join("session.jsonl")).max(file_mtime_secs(&path));
            Some((mtime, path))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    dirs.truncate(limit);
    dirs.into_iter().map(|(_, path)| path).collect()
}

pub(crate) fn append_external_context_trace_replay_entries(
    log_dir: &Path,
    session_id: &str,
    seen: &mut HashSet<String>,
    entries: &mut Vec<serde_json::Value>,
) {
    let trace_root = log_dir.join("model-request-traces");
    if !trace_root.is_dir() {
        return;
    }
    let Ok(snapshots) = crate::external_agent::codex::context_snapshots_from_trace_archive(
        &trace_root,
        session_id,
        false,
    ) else {
        return;
    };
    for snapshot in snapshots {
        let entry = external_context_snapshot_replay_entry_from_trace(session_id, snapshot);
        let key = context_snapshot_replay_entry_key(&entry);
        if seen.insert(key) {
            entries.push(entry);
        }
    }
}

pub(crate) fn external_context_snapshot_replay_entry_from_trace(
    session_id: &str,
    snapshot: crate::external_agent::AgentContextSnapshot,
) -> serde_json::Value {
    serde_json::to_value(crate::types::OutboundEvent::ContextSnapshot {
        session_id: Some(session_id.to_string()),
        source: snapshot.source,
        label: snapshot.label,
        request_id: snapshot.request_id,
        request_index: snapshot.request_index,
        turn: None,
        format: snapshot.format,
        token_count: snapshot.token_count,
        token_count_kind: snapshot
            .token_count_kind
            .map(|kind| kind.as_str().to_string()),
        context_window: snapshot.context_window,
        hard_context_window: snapshot.hard_context_window,
        item_count: snapshot.item_count,
        raw: snapshot.raw,
    })
    .unwrap_or_else(|_| {
        serde_json::json!({
            "event": "context_snapshot",
            "session_id": session_id,
            "source": "codex",
            "label": "Codex request payload",
            "format": "codex.inference_request_payload.v1",
            "raw": serde_json::json!({}),
        })
    })
}

pub(crate) fn context_snapshot_replay_entry_matches_session(
    entry: &serde_json::Value,
    session_id: &str,
) -> bool {
    entry.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
        || entry
            .pointer("/raw/_intendant_context/thread_id")
            .and_then(|v| v.as_str())
            == Some(session_id)
}

pub(crate) fn context_snapshot_replay_entry_key(entry: &serde_json::Value) -> String {
    if let Some(request_id) = entry.get("request_id").and_then(|v| v.as_str()) {
        return format!("request:{request_id}");
    }
    if let Some(request_index) = entry.get("request_index").and_then(|v| v.as_u64()) {
        let session_id = entry
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        return format!("index:{session_id}:{request_index}");
    }
    serde_json::to_string(entry).unwrap_or_else(|_| format!("{entry:?}"))
}

pub(crate) fn context_snapshot_replay_entry_sort_key(entry: &serde_json::Value) -> (u64, String) {
    (
        entry
            .get("request_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX),
        context_snapshot_replay_entry_key(entry),
    )
}

pub(crate) fn external_attached_session_from_wire(line: &str) -> Option<(String, String)> {
    let parsed = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if parsed.get("event").and_then(|v| v.as_str()) != Some("session_attached") {
        return None;
    }
    let session_id = parsed.get("session_id").and_then(|v| v.as_str())?;
    let source = parsed
        .get("source")
        .and_then(|v| v.as_str())
        .map(crate::session_names::normalize_source)?;
    if session_id.trim().is_empty() || source.is_empty() || source == "intendant" {
        return None;
    }
    Some((session_id.to_string(), source))
}

pub(crate) fn external_identity_session_from_wire(line: &str) -> Option<(String, String)> {
    let parsed = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if parsed.get("event").and_then(|v| v.as_str()) != Some("session_identity") {
        return None;
    }
    let data = parsed.get("data").filter(|value| value.is_object());
    let backend_session_id = parsed
        .get("backend_session_id")
        .or_else(|| data.and_then(|data| data.get("backend_session_id")))
        .and_then(|v| v.as_str())
        .and_then(clean_external_thread_id)?;
    let source = parsed
        .get("source")
        .or_else(|| data.and_then(|data| data.get("source")))
        .and_then(|v| v.as_str())
        .map(crate::session_names::normalize_source)?;
    if source.is_empty() || source == "intendant" {
        return None;
    }
    Some((backend_session_id, source))
}

pub(crate) fn update_external_attached_sessions_from_wire(
    sessions: &mut HashMap<String, String>,
    line: &str,
) {
    if let Some((session_id, source)) = external_attached_session_from_wire(line) {
        sessions.insert(session_id, source);
        return;
    }
    if let Some((session_id, source)) = external_identity_session_from_wire(line) {
        sessions.insert(session_id, source);
        return;
    }
    if let Some(ended_id) = session_ended_id_from_wire(line) {
        sessions.remove(&ended_id);
    }
}

pub(crate) fn session_ended_id_from_wire(line: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if parsed.get("event").and_then(|v| v.as_str()) != Some("session_ended") {
        return None;
    }
    parsed
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub(crate) fn list_sessions() -> String {
    list_sessions_from_home(&crate::platform::home_dir())
}

pub(crate) fn cached_limited_session_list_cache(
) -> &'static Mutex<HashMap<usize, SessionListResponseCacheEntry>> {
    SESSION_LIST_LIMITED_RESPONSE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) const SESSION_LIST_RESPONSE_STALE_MAX_SECS: u64 = 15 * 60;

pub(crate) fn session_list_refresh_inflight() -> &'static Mutex<HashSet<usize>> {
    static INFLIGHT: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Slot key for the single-flight guard: real limits are 1..SESSION_LIST_LIMIT,
/// the unlimited list uses SESSION_LIST_LIMIT itself.
pub(crate) fn spawn_session_list_refresh(limit_slot: usize) {
    {
        let mut inflight = session_list_refresh_inflight()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !inflight.insert(limit_slot) {
            return;
        }
    }
    std::thread::spawn(move || {
        let body = if limit_slot >= SESSION_LIST_LIMIT {
            list_sessions()
        } else {
            list_sessions_from_home_with_limit(&crate::platform::home_dir(), Some(limit_slot))
        };
        store_session_list_response(limit_slot, body);
        let mut inflight = session_list_refresh_inflight()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        inflight.remove(&limit_slot);
    });
}

pub(crate) fn store_session_list_response(limit_slot: usize, body: String) {
    let entry = SessionListResponseCacheEntry {
        generated_at: std::time::Instant::now(),
        body,
    };
    if limit_slot >= SESSION_LIST_LIMIT {
        let cache = SESSION_LIST_RESPONSE_CACHE.get_or_init(|| Mutex::new(None));
        *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(entry);
    } else {
        let mut guard = cached_limited_session_list_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.len() >= 16 && !guard.contains_key(&limit_slot) {
            guard.clear();
        }
        guard.insert(limit_slot, entry);
    }
}

/// fresh -> serve; stale-but-usable -> serve + background refresh;
/// too stale/absent -> rebuild inline.
pub(crate) fn serve_session_list_cache_entry(
    limit_slot: usize,
    entry: Option<&SessionListResponseCacheEntry>,
) -> Option<String> {
    let entry = entry?;
    let age = entry.generated_at.elapsed();
    if age <= std::time::Duration::from_secs(SESSION_LIST_RESPONSE_CACHE_TTL_SECS) {
        return Some(entry.body.clone());
    }
    if age <= std::time::Duration::from_secs(SESSION_LIST_RESPONSE_STALE_MAX_SECS) {
        let body = entry.body.clone();
        spawn_session_list_refresh(limit_slot);
        return Some(body);
    }
    None
}

pub(crate) fn cached_list_sessions() -> String {
    let cache = SESSION_LIST_RESPONSE_CACHE.get_or_init(|| Mutex::new(None));
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(body) = serve_session_list_cache_entry(SESSION_LIST_LIMIT, guard.as_ref()) {
            return body;
        }
    }

    let body = list_sessions();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(SessionListResponseCacheEntry {
        generated_at: std::time::Instant::now(),
        body: body.clone(),
    });
    body
}

pub(crate) fn cached_list_sessions_with_limit(limit: usize) -> String {
    let limit = limit.clamp(1, SESSION_LIST_LIMIT);
    if limit >= SESSION_LIST_LIMIT {
        return cached_list_sessions();
    }

    let cache = cached_limited_session_list_cache();
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(body) = serve_session_list_cache_entry(limit, guard.get(&limit)) {
            return body;
        }
    }

    let body = list_sessions_from_home_with_limit(&crate::platform::home_dir(), Some(limit));
    store_session_list_response(limit, body.clone());
    body
}

pub(crate) fn cached_session_list_snapshot() -> Option<String> {
    let cache = SESSION_LIST_RESPONSE_CACHE.get()?;
    let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|entry| entry.body.clone())
}

pub(crate) fn session_ids_filter_from_request(request_line: &str) -> Option<Vec<String>> {
    query_param(request_line, "ids").map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty() && session_lookup_id_is_safe(id))
            .map(ToString::to_string)
            .collect()
    })
}

pub(crate) fn session_row_matches_any_id(row: &serde_json::Value, ids: &HashSet<String>) -> bool {
    if ids.is_empty() {
        return true;
    }
    [
        "session_id",
        "resume_id",
        "backend_session_id",
        "intendant_session_id",
    ]
    .into_iter()
    .filter_map(|key| row.get(key).and_then(|v| v.as_str()))
    .any(|id| session_id_matches_any_requested(id, ids))
}

pub(crate) fn session_id_matches_any_requested(
    candidate: &str,
    requested_ids: &HashSet<String>,
) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return false;
    }
    requested_ids.iter().any(|requested| {
        let requested = requested.trim();
        !requested.is_empty() && (candidate == requested || candidate.starts_with(requested))
    })
}

#[allow(dead_code)]
pub(crate) fn filter_session_list_by_ids(body: &str, ids: &[String]) -> String {
    if ids.is_empty() {
        return body.to_string();
    }
    let wanted: HashSet<String> = ids.iter().cloned().collect();
    let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(body) else {
        return body.to_string();
    };
    let filtered: Vec<serde_json::Value> = rows
        .into_iter()
        .filter(|row| session_row_matches_any_id(row, &wanted))
        .collect();
    serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".to_string())
}

pub(crate) fn session_row_source_is_codex(row: &serde_json::Value) -> bool {
    value_str(row, "backend_source")
        .or_else(|| value_str(row, "source"))
        .map(|source| crate::session_names::normalize_source(&source) == "codex")
        .unwrap_or(false)
}

pub(crate) fn session_row_id_values(row: &serde_json::Value) -> Vec<String> {
    [
        "backend_session_id",
        "resume_id",
        "session_id",
        "intendant_session_id",
    ]
    .into_iter()
    .filter_map(|key| value_str(row, key))
    .filter(|id| !id.trim().is_empty())
    .collect()
}

pub(crate) fn latest_session_goal_from_entries(
    entries: &[serde_json::Value],
    session_id: &str,
) -> Option<Option<SessionGoal>> {
    let mut latest = None;
    for entry in entries {
        if entry.get("event").and_then(|v| v.as_str()) != Some("session_goal") {
            continue;
        }
        let entry_session_id = entry
            .get("session_id")
            .and_then(|v| v.as_str())
            .or_else(|| entry.pointer("/data/session_id").and_then(|v| v.as_str()));
        if entry_session_id.is_some_and(|id| id != session_id) {
            continue;
        }
        let data = entry.get("data").unwrap_or(entry);
        let has_goal = data.get("goal").is_some()
            || data.get("session_goal").is_some()
            || data.get("sessionGoal").is_some();
        let goal = data
            .get("goal")
            .or_else(|| data.get("session_goal"))
            .or_else(|| data.get("sessionGoal"));
        if has_goal {
            latest = Some(goal.and_then(codex_session_goal_from_value));
        } else {
            latest = Some(codex_session_goal_from_value(data));
        }
    }
    latest
}

pub(crate) fn hydrate_codex_session_goal_for_row(
    home: &Path,
    row: &mut serde_json::Value,
    requested_ids: &HashSet<String>,
) {
    if !session_row_source_is_codex(row) {
        return;
    }
    let row_ids = session_row_id_values(row);
    if !row_ids
        .iter()
        .any(|id| session_id_matches_any_requested(id, requested_ids))
    {
        return;
    }
    for id in row_ids {
        let Some(entries) = external_session_entries_from_home(home, "codex", &id) else {
            continue;
        };
        let Some(goal) = latest_session_goal_from_entries(&entries, &id) else {
            continue;
        };
        if let Some(obj) = row.as_object_mut() {
            obj.insert("goal".to_string(), serde_json::json!(goal));
            obj.insert("session_goal".to_string(), serde_json::json!(goal));
        }
        return;
    }
}

pub(crate) fn hydrate_codex_session_goals_for_ids(
    home: &Path,
    body: &str,
    ids: &[String],
) -> String {
    if ids.is_empty() {
        return body.to_string();
    }
    let Ok(mut rows) = serde_json::from_str::<Vec<serde_json::Value>>(body) else {
        return body.to_string();
    };
    let requested_ids: HashSet<String> = ids.iter().cloned().collect();
    for row in &mut rows {
        hydrate_codex_session_goal_for_row(home, row, &requested_ids);
    }
    serde_json::to_string(&rows).unwrap_or_else(|_| body.to_string())
}

#[allow(dead_code)]
pub(crate) fn filter_session_list_by_ids_with_codex_goal_hydration(
    home: &Path,
    body: &str,
    ids: &[String],
) -> String {
    let filtered = filter_session_list_by_ids(body, ids);
    hydrate_codex_session_goals_for_ids(home, &filtered, ids)
}

pub(crate) fn session_list_limit_from_request(request_line: &str) -> Option<usize> {
    let raw = query_param(request_line, "limit")
        .or_else(|| query_param(request_line, "max"))
        .or_else(|| query_param(request_line, "count"))?;
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("all")
        || trimmed.eq_ignore_ascii_case("full")
        || trimmed.eq_ignore_ascii_case("unlimited")
    {
        return None;
    }
    trimmed
        .parse::<usize>()
        .ok()
        .filter(|limit| *limit > 0)
        .map(|limit| limit.min(SESSION_LIST_LIMIT))
}

pub(crate) fn limit_session_list_body(body: &str, limit: Option<usize>) -> String {
    let Some(limit) = limit else {
        return body.to_string();
    };
    let Ok(mut rows) = serde_json::from_str::<Vec<serde_json::Value>>(body) else {
        return body.to_string();
    };
    if rows.len() <= limit {
        return body.to_string();
    }
    rows.truncate(limit);
    serde_json::to_string(&rows).unwrap_or_else(|_| body.to_string())
}

pub(crate) fn cached_list_sessions_for_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        return "[]".to_string();
    }
    cached_list_sessions_for_ids_from_home(&crate::platform::home_dir(), ids)
}

pub(crate) fn sessions_list_response_body(limit: Option<usize>, ids: &[String]) -> String {
    if !ids.is_empty() {
        cached_list_sessions_for_ids(ids)
    } else if let Some(limit) = limit {
        cached_list_sessions_with_limit(limit)
    } else {
        cached_list_sessions()
    }
}

/// Strip session rows down to what the Stats tab folds: usage, costs,
/// per-day buckets, and disk sizes. Full rows carry tasks, paths, goals,
/// and lineage that make a whole-corpus fetch megabytes; the usage view
/// is the same cached data at ~a tenth of the payload.
pub(crate) fn session_list_body_usage_view(body: &str) -> String {
    const KEEP: [&str; 19] = [
        "id",
        "session_id",
        "source",
        "turns",
        "total_tokens",
        "prompt_tokens",
        "completion_tokens",
        "cached_tokens",
        "cache_creation_tokens",
        "estimated_cost",
        "pricing_known",
        "created_at",
        "updated_at",
        "daily_usage",
        "recording_bytes",
        "frames_bytes",
        "turns_bytes",
        "logs_bytes",
        "total_bytes",
    ];
    let Ok(mut rows) = serde_json::from_str::<Vec<serde_json::Value>>(body) else {
        return body.to_string();
    };
    for row in rows.iter_mut() {
        if let Some(obj) = row.as_object_mut() {
            obj.retain(|key, _| KEEP.contains(&key.as_str()));
        }
    }
    serde_json::to_string(&rows).unwrap_or_else(|_| body.to_string())
}

pub(crate) fn session_list_usage_view_from_request(request_line: &str) -> bool {
    let Some(path) = request_line.split_whitespace().nth(1) else {
        return false;
    };
    let Some(query) = path.split('?').nth(1) else {
        return false;
    };
    query
        .split('&')
        .any(|pair| pair == "view=usage" || pair.starts_with("view=usage&"))
}

pub(crate) fn push_unique_session_row_for_ids(
    rows: &mut Vec<serde_json::Value>,
    seen: &mut HashSet<String>,
    row: serde_json::Value,
    requested_ids: &HashSet<String>,
) {
    if !session_row_matches_any_id(&row, requested_ids) {
        return;
    }
    let key = session_unique_key(&row);
    if seen.insert(key) {
        rows.push(row);
    }
}

pub(crate) fn targeted_intendant_session_rows_from_home(
    home: &Path,
    requested_ids: &HashSet<String>,
    rows: &mut Vec<serde_json::Value>,
    seen: &mut HashSet<String>,
) {
    let logs_dir = home.join(".intendant").join("logs");
    let mut seen_dirs = HashSet::new();
    for requested_id in requested_ids {
        if !session_lookup_id_is_safe(requested_id) {
            continue;
        }
        let exact = logs_dir.join(requested_id);
        if exact.is_dir() {
            seen_dirs.insert(session_list_path_key(&exact));
        }
    }
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if requested_ids
                .iter()
                .any(|requested| !requested.is_empty() && name.starts_with(requested))
            {
                let path = entry.path();
                if path.is_dir() {
                    seen_dirs.insert(session_list_path_key(&path));
                }
            }
        }
    }

    for dir_key in seen_dirs {
        let dir = PathBuf::from(&dir_key);
        let Some(session_id) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if let Some(row) = intendant_session_list_row_from_dir(&dir, &session_id) {
            push_unique_session_row_for_ids(rows, seen, row, requested_ids);
        }
    }
}

pub(crate) fn targeted_external_session_rows_from_home(
    home: &Path,
    requested_ids: &HashSet<String>,
    rows: &mut Vec<serde_json::Value>,
    seen: &mut HashSet<String>,
) {
    let mut external_sessions = Vec::new();
    external_sessions.extend(list_codex_sessions_with_limit(
        home,
        EXTERNAL_SESSION_SCAN_LIMIT,
    ));
    external_sessions.extend(list_claude_sessions_with_limit(
        home,
        EXTERNAL_SESSION_SCAN_LIMIT,
    ));
    external_sessions.extend(list_gemini_sessions_with_limit(
        home,
        EXTERNAL_SESSION_SCAN_LIMIT,
    ));
    let deleted_external_sessions = read_deleted_external_sessions(home);
    if !deleted_external_sessions.is_empty() {
        external_sessions.retain(|session| {
            !session_matches_deleted_external(session, &deleted_external_sessions)
        });
    }
    crate::session_names::apply_session_name_overlays(home, &mut external_sessions);
    crate::session_config::apply_overlays_to_sessions(home, &mut external_sessions);
    for row in external_sessions {
        push_unique_session_row_for_ids(rows, seen, row, requested_ids);
    }
}

pub(crate) fn targeted_session_list_for_ids_from_home(home: &Path, ids: &[String]) -> String {
    let requested_ids: HashSet<String> = ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| session_lookup_id_is_safe(id))
        .map(ToString::to_string)
        .collect();
    if requested_ids.is_empty() {
        return "[]".to_string();
    }

    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    targeted_intendant_session_rows_from_home(home, &requested_ids, &mut rows, &mut seen);
    targeted_external_session_rows_from_home(home, &requested_ids, &mut rows, &mut seen);
    apply_external_wrapper_index_to_sessions(home, &mut rows);
    sort_sessions_newest_first(&mut rows);
    let body = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string());
    hydrate_codex_session_goals_for_ids(home, &body, ids)
}

pub(crate) fn cached_list_sessions_for_ids_from_home(home: &Path, ids: &[String]) -> String {
    if ids.is_empty() {
        return "[]".to_string();
    }
    targeted_session_list_for_ids_from_home(home, ids)
}

pub(crate) fn cached_intendant_log_dirs_for_session_id(session_id: &str) -> Vec<PathBuf> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Vec::new();
    }
    let Some(body) = cached_session_list_snapshot() else {
        return Vec::new();
    };
    let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&body) else {
        return Vec::new();
    };
    let wanted = std::iter::once(session_id.to_string()).collect::<HashSet<_>>();
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        if !session_row_matches_any_id(&row, &wanted) {
            continue;
        }
        for key in ["intendant_session_path", "path"] {
            let Some(path) = row.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            let path = PathBuf::from(path);
            if !path.is_dir() {
                continue;
            }
            let fingerprint = std::fs::canonicalize(&path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .to_string();
            if seen.insert(fingerprint) {
                dirs.push(path);
            }
        }
    }
    dirs
}

pub(crate) fn worktree_session_hints_from_home(
    home: &Path,
) -> Vec<crate::worktree_inventory::WorktreeSessionHint> {
    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&list_sessions_from_home(home)).unwrap_or_default();
    let mut hints = Vec::new();
    let mut status_by_session = HashMap::new();
    for session in &sessions {
        let Some(hint) = worktree_session_hint_from_json(session) else {
            continue;
        };
        status_by_session.insert(
            (hint.source.clone(), hint.session_id.clone()),
            (hint.status.clone(), hint.updated_at.clone()),
        );
        hints.push(hint);
    }

    let mut observed =
        agent_observed_worktree_session_hints_from_home(home, &sessions, &status_by_session);
    observed.extend(hints);
    dedupe_worktree_session_hints(observed)
}

pub(crate) type WorktreeHintStatusMap = HashMap<(String, String), (String, Option<String>)>;

pub(crate) fn worktree_session_hint_from_json(
    session: &serde_json::Value,
) -> Option<crate::worktree_inventory::WorktreeSessionHint> {
    let session_id = session.get("session_id")?.as_str()?.to_string();
    let source = session
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("intendant")
        .to_string();
    let status = session
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let project_root = session
        .get("project_root")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let cwd = session
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let updated_at = session
        .get("updated_at")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    Some(crate::worktree_inventory::WorktreeSessionHint {
        session_id,
        source,
        status,
        project_root,
        cwd,
        updated_at,
    })
}

pub(crate) fn agent_observed_worktree_session_hints_from_home(
    home: &Path,
    sessions: &[serde_json::Value],
    status_by_session: &WorktreeHintStatusMap,
) -> Vec<crate::worktree_inventory::WorktreeSessionHint> {
    let mut hints = Vec::new();
    extend_codex_observed_worktree_session_hints(home, sessions, status_by_session, &mut hints);
    extend_claude_observed_worktree_session_hints(home, sessions, status_by_session, &mut hints);
    extend_gemini_observed_worktree_session_hints(home, &mut hints);
    hints
}

pub(crate) fn extend_codex_observed_worktree_session_hints(
    home: &Path,
    sessions: &[serde_json::Value],
    status_by_session: &WorktreeHintStatusMap,
    hints: &mut Vec<crate::worktree_inventory::WorktreeSessionHint>,
) {
    let mut files = agent_session_files_from_rows(
        sessions,
        "codex",
        ".jsonl",
        WORKTREE_OBSERVED_SESSION_FILE_LIMIT,
    );
    if files.is_empty() {
        let codex = codex_dir(home);
        files = collect_recent_files(
            &codex.join("sessions"),
            ".jsonl",
            WORKTREE_OBSERVED_SESSION_FILE_LIMIT,
        );
        files.extend(collect_recent_files(
            &codex.join("archived_sessions"),
            ".jsonl",
            WORKTREE_OBSERVED_SESSION_FILE_LIMIT,
        ));
        files.sort_by_key(|b| std::cmp::Reverse(file_mtime_secs(b)));
        files.truncate(WORKTREE_OBSERVED_SESSION_FILE_LIMIT);
    }

    for path in files {
        if hints.len() >= WORKTREE_OBSERVED_HINT_LIMIT {
            break;
        }
        hints.extend(codex_observed_worktree_session_hints_from_file(
            home,
            &path,
            status_by_session,
        ));
    }
}

pub(crate) fn extend_claude_observed_worktree_session_hints(
    home: &Path,
    sessions: &[serde_json::Value],
    status_by_session: &WorktreeHintStatusMap,
    hints: &mut Vec<crate::worktree_inventory::WorktreeSessionHint>,
) {
    let mut files = agent_session_files_from_rows(
        sessions,
        "claude-code",
        ".jsonl",
        WORKTREE_OBSERVED_SESSION_FILE_LIMIT,
    );
    if files.is_empty() {
        files = collect_recent_files(
            &home.join(".claude").join("projects"),
            ".jsonl",
            WORKTREE_OBSERVED_SESSION_FILE_LIMIT,
        );
    }
    for path in files {
        if hints.len() >= WORKTREE_OBSERVED_HINT_LIMIT {
            break;
        }
        hints.extend(claude_observed_worktree_session_hints_from_file(
            home,
            &path,
            status_by_session,
        ));
    }
}

pub(crate) fn agent_session_files_from_rows(
    sessions: &[serde_json::Value],
    source: &str,
    suffix: &str,
    limit: usize,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for session in sessions {
        if session.get("source").and_then(|v| v.as_str()) != Some(source) {
            continue;
        }
        let Some(path) = session
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| s.ends_with(suffix))
            .map(PathBuf::from)
        else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let key = worktree_hint_path_key(&path);
        if seen.insert(key) {
            files.push(path);
        }
    }
    files.sort_by_key(|b| std::cmp::Reverse(file_mtime_secs(b)));
    files.truncate(limit);
    files
}

pub(crate) fn extend_gemini_observed_worktree_session_hints(
    home: &Path,
    hints: &mut Vec<crate::worktree_inventory::WorktreeSessionHint>,
) {
    for (alias, root) in gemini_project_roots(home) {
        if hints.len() >= WORKTREE_OBSERVED_HINT_LIMIT {
            break;
        }
        let mut seen_paths = HashSet::new();
        push_agent_observed_worktree_hint(
            hints,
            home,
            "gemini",
            &format!("gemini-project:{alias}"),
            "external",
            None,
            &root,
            &mut seen_paths,
        );
    }
}

pub(crate) fn codex_observed_worktree_session_hints_from_file(
    home: &Path,
    path: &Path,
    status_by_session: &WorktreeHintStatusMap,
) -> Vec<crate::worktree_inventory::WorktreeSessionHint> {
    let contents = match read_text_head_tail(
        path,
        EXTERNAL_SESSION_READ_LIMIT,
        EXTERNAL_SESSION_READ_LIMIT,
    ) {
        Some(contents) => contents,
        None => return Vec::new(),
    };
    let mut session_id = None;
    let mut updated_at = file_mtime_string(path);
    let mut observed_paths = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || !codex_line_may_affect_session_list(line) {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        updated_at = value_str(&obj, "timestamp").or(updated_at);
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "session_meta" => {
                if let Some(payload) = obj.get("payload") {
                    session_id = session_id.or_else(|| value_str(payload, "id"));
                    if let Some(value) = value_str(payload, "cwd") {
                        observed_paths.push(value);
                    }
                }
            }
            "turn_context" => {
                if let Some(payload) = obj.get("payload") {
                    if let Some(value) = value_str(payload, "cwd") {
                        observed_paths.push(value);
                    }
                }
            }
            "event_msg" => {
                if let Some(payload) = obj.get("payload") {
                    let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if payload_type.starts_with("exec_command") {
                        if let Some(value) =
                            value_str(payload, "workdir").or_else(|| value_str(payload, "cwd"))
                        {
                            observed_paths.push(value);
                        }
                    }
                }
            }
            "response_item" => {
                if let Some(payload) = obj.get("payload") {
                    if let Some(value) = codex_exec_command_workdir(payload) {
                        observed_paths.push(value);
                    }
                }
            }
            _ => {}
        }
    }

    let session_id = session_id
        .or_else(|| codex_session_file_id(path))
        .or_else(|| {
            path.file_stem()
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
        });
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    let (status, updated_at) =
        worktree_hint_status("codex", &session_id, status_by_session, updated_at);
    let mut hints = Vec::new();
    let mut seen_paths = HashSet::new();
    for observed_path in observed_paths {
        if seen_paths.len() >= WORKTREE_OBSERVED_PATHS_PER_SESSION {
            break;
        }
        push_agent_observed_worktree_hint(
            &mut hints,
            home,
            "codex",
            &session_id,
            &status,
            updated_at.as_deref(),
            &observed_path,
            &mut seen_paths,
        );
    }
    hints
}

pub(crate) fn claude_observed_worktree_session_hints_from_file(
    home: &Path,
    path: &Path,
    status_by_session: &WorktreeHintStatusMap,
) -> Vec<crate::worktree_inventory::WorktreeSessionHint> {
    let contents = match read_text_head_tail(
        path,
        EXTERNAL_SESSION_READ_LIMIT,
        EXTERNAL_SESSION_READ_LIMIT,
    ) {
        Some(contents) => contents,
        None => return Vec::new(),
    };
    let Some(session_id) = path
        .file_stem()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
    else {
        return Vec::new();
    };
    let mut updated_at = file_mtime_string(path);
    let mut observed_paths = Vec::new();

    for line in contents.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        updated_at = value_str(&obj, "timestamp").or(updated_at);
        if let Some(value) = value_str(&obj, "cwd") {
            observed_paths.push(value);
        }
        if let Some(message) = obj.get("message") {
            if let Some(value) = value_str(message, "cwd") {
                observed_paths.push(value);
            }
        }
    }

    let (status, updated_at) =
        worktree_hint_status("claude-code", &session_id, status_by_session, updated_at);
    let mut hints = Vec::new();
    let mut seen_paths = HashSet::new();
    for observed_path in observed_paths {
        if seen_paths.len() >= WORKTREE_OBSERVED_PATHS_PER_SESSION {
            break;
        }
        push_agent_observed_worktree_hint(
            &mut hints,
            home,
            "claude-code",
            &session_id,
            &status,
            updated_at.as_deref(),
            &observed_path,
            &mut seen_paths,
        );
    }
    hints
}

pub(crate) fn worktree_hint_status(
    source: &str,
    session_id: &str,
    status_by_session: &WorktreeHintStatusMap,
    fallback_updated_at: Option<String>,
) -> (String, Option<String>) {
    if let Some((status, updated_at)) =
        status_by_session.get(&(source.to_string(), session_id.to_string()))
    {
        (status.clone(), updated_at.clone().or(fallback_updated_at))
    } else {
        ("external".to_string(), fallback_updated_at)
    }
}

pub(crate) fn push_agent_observed_worktree_hint(
    hints: &mut Vec<crate::worktree_inventory::WorktreeSessionHint>,
    home: &Path,
    source: &str,
    session_id: &str,
    status: &str,
    updated_at: Option<&str>,
    observed_path: &str,
    seen_paths: &mut HashSet<String>,
) {
    let Some((project_root, cwd)) = normalize_agent_observed_git_path(home, observed_path) else {
        return;
    };
    let key = format!(
        "{}\0{}",
        worktree_hint_path_key(&project_root),
        worktree_hint_path_key(&cwd)
    );
    if !seen_paths.insert(key) {
        return;
    }
    hints.push(crate::worktree_inventory::WorktreeSessionHint {
        session_id: session_id.to_string(),
        source: source.to_string(),
        status: status.to_string(),
        project_root: Some(project_root),
        cwd: Some(cwd),
        updated_at: updated_at.map(ToString::to_string),
    });
}

pub(crate) fn normalize_agent_observed_git_path(
    home: &Path,
    raw_path: &str,
) -> Option<(PathBuf, PathBuf)> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() || should_skip_agent_observed_path(home, &path) {
        return None;
    }

    let mut cwd = path;
    while !cwd.exists() {
        if !cwd.pop() {
            return None;
        }
    }
    if cwd.is_file() {
        cwd.pop();
    }
    let mut project_root = cwd.clone();
    loop {
        if project_root.join(".git").exists() {
            return Some((project_root, cwd));
        }
        if !project_root.pop() {
            return None;
        }
    }
}

pub(crate) fn should_skip_agent_observed_path(home: &Path, path: &Path) -> bool {
    if worktree_hint_path_key(home) == worktree_hint_path_key(path) {
        return true;
    }
    if path.parent().is_none() {
        return true;
    }
    matches!(
        path.to_string_lossy().as_ref(),
        "/" | "/tmp" | "/private/tmp" | "/var/tmp"
    )
}

pub(crate) fn dedupe_worktree_session_hints(
    hints: Vec<crate::worktree_inventory::WorktreeSessionHint>,
) -> Vec<crate::worktree_inventory::WorktreeSessionHint> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for hint in hints {
        let project_root = hint
            .project_root
            .as_ref()
            .map(|path| worktree_hint_path_key(path))
            .unwrap_or_default();
        let cwd = hint
            .cwd
            .as_ref()
            .map(|path| worktree_hint_path_key(path))
            .unwrap_or_default();
        let key = format!(
            "{}\0{}\0{}\0{}",
            hint.source, hint.session_id, project_root, cwd
        );
        if seen.insert(key) {
            out.push(hint);
        }
    }
    out
}

pub(crate) fn worktree_hint_path_key(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string()
}

pub(crate) fn push_session_file_fingerprint(
    entries: &mut Vec<SessionFileFingerprint>,
    base: &Path,
    path: &Path,
    is_dir: bool,
) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() != is_dir {
        return;
    }
    let rel = path
        .strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let (dev, ino) = crate::platform::metadata_dev_ino(&metadata);
    entries.push(SessionFileFingerprint {
        rel,
        len: metadata.len(),
        mtime_nanos: metadata_mtime_nanos(&metadata),
        ctime_nanos: metadata_ctime_nanos(&metadata),
        dev,
        ino,
        is_dir,
    });
}

pub(crate) fn intendant_session_dir_fingerprint(dir: &Path) -> Option<SessionDirFingerprint> {
    let mut entries = Vec::new();
    push_session_file_fingerprint(&mut entries, dir, dir, true);

    for name in [
        "session.jsonl",
        "session_meta.json",
        crate::session_config::SESSION_AGENT_CONFIG_FILE,
        "summary.json",
        "conversation.jsonl",
    ] {
        let path = dir.join(name);
        if path.is_file() {
            push_session_file_fingerprint(&mut entries, dir, &path, false);
        }
    }

    let recordings_dir = dir.join("recordings");
    if let Ok(rd) = std::fs::read_dir(&recordings_dir) {
        for re in rd.flatten() {
            let recording_dir = re.path();
            if !recording_dir.is_dir() {
                continue;
            }
            push_session_file_fingerprint(&mut entries, dir, &recording_dir, true);
            if let Ok(files) = std::fs::read_dir(&recording_dir) {
                for file in files.flatten() {
                    let path = file.path();
                    let name = file.file_name().to_string_lossy().to_string();
                    if name.starts_with("seg_") && path.is_file() {
                        push_session_file_fingerprint(&mut entries, dir, &path, false);
                    }
                }
            }
        }
    }

    let frames_dir = dir.join("frames");
    if let Ok(fd) = std::fs::read_dir(&frames_dir) {
        for fe in fd.flatten() {
            let path = fe.path();
            if path.is_file() {
                push_session_file_fingerprint(&mut entries, dir, &path, false);
            }
        }
    }

    let turns_dir = dir.join("turns");
    if let Ok(td) = std::fs::read_dir(&turns_dir) {
        for te in td.flatten() {
            let path = te.path();
            if path.is_file() {
                push_session_file_fingerprint(&mut entries, dir, &path, false);
            }
        }
    }

    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|a, b| a.rel.cmp(&b.rel).then_with(|| a.is_dir.cmp(&b.is_dir)));
    Some(SessionDirFingerprint {
        path: session_list_path_key(dir),
        digest: session_file_fingerprints_digest(&entries),
    })
}

/// Canonical digest over sorted per-file stat records. The byte layout is
/// part of the persisted intendant-row format: changing it (or the record
/// fields) invalidates every persisted row, which then rebuilds on the
/// next list pass.
pub(crate) fn session_file_fingerprints_digest(entries: &[SessionFileFingerprint]) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for entry in entries {
        ctx.update(entry.rel.as_bytes());
        ctx.update(&[0]);
        ctx.update(&entry.len.to_le_bytes());
        ctx.update(&entry.mtime_nanos.to_le_bytes());
        ctx.update(&entry.ctime_nanos.to_le_bytes());
        ctx.update(&entry.dev.to_le_bytes());
        ctx.update(&entry.ino.to_le_bytes());
        ctx.update(&[entry.is_dir as u8]);
    }
    let digest = ctx.finish();
    let mut out = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub(crate) fn intendant_session_list_row_from_dir(
    dir: &Path,
    session_id: &str,
) -> Option<serde_json::Value> {
    let fingerprint = intendant_session_dir_fingerprint(dir)?;
    if let Some(row) = cached_intendant_session_list_row(&fingerprint) {
        return Some(row);
    }

    let meta_path = dir.join("session_meta.json");
    let mut name: Option<String> = None;
    let mut task: Option<String> = None;
    let mut created_at: Option<String> = None;
    let mut project_root: Option<String> = None;
    let cwd: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut status = "in_progress".to_string();
    let mut turns: u64 = 0;
    let mut total_tokens: u64 = 0;
    let mut prompt_tokens: u64 = 0;
    let mut completion_tokens: u64 = 0;
    let mut cached_tokens: u64 = 0;
    let mut daily_usage: BTreeMap<String, SessionUsage> = BTreeMap::new();
    let mut role: Option<String> = None;
    let mut external_resume_id: Option<String> = None;
    let mut external_source: Option<String> = None;
    let mut canonical_session_id: Option<String> = None;
    let mut capabilities: Option<serde_json::Value> = None;
    let mut session_agent_config = crate::session_config::read_log_dir_config(dir);
    let mut updated_at_secs = file_mtime_secs(dir);

    if let Ok(meta_str) = std::fs::read_to_string(&meta_path) {
        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str) {
            task = meta
                .get("task")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            created_at = meta
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            project_root = meta
                .get("project_root")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            name = meta
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| compact_text(s, 180));
            if let Some(s) = meta.get("status").and_then(|v| v.as_str()) {
                status = s.to_string();
            }
            if let Some(t) = meta.get("last_turn").and_then(|v| v.as_u64()) {
                turns = t;
            }
            role = meta
                .get("role")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            canonical_session_id = meta
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }

    let jsonl_path = dir.join("session.jsonl");
    if let Ok(contents) = std::fs::read_to_string(&jsonl_path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let event = obj.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let message = obj.get("message").and_then(|v| v.as_str()).unwrap_or("");

            match event {
                "session_start" => {
                    if created_at.is_none() {
                        created_at = obj
                            .get("ts")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                }
                "session_identity" => {
                    // Structured identity beats the prose scrape below; later
                    // events supersede earlier ones (placeholder → native-id
                    // upgrades append). Wrapper matching keeps identities the
                    // bus tee copied into the daemon-main log from stamping
                    // the daemon session's row with a child's backend id, and
                    // the canonical filter keeps placeholder ids in pre-guard
                    // logs from conjuring ghost windows (see
                    // `scraped_external_thread_id_is_canonical`).
                    if let Some(data) = obj.get("data") {
                        if crate::session_identity::wrapper_matches(
                            data.get("session_id").and_then(|v| v.as_str()),
                            session_id,
                            canonical_session_id.as_deref(),
                        ) {
                            if let Some(source) = data
                                .get("source")
                                .and_then(|v| v.as_str())
                                .map(crate::session_names::normalize_source)
                                .filter(|s| !s.is_empty() && s != "intendant")
                            {
                                external_source = Some(source);
                            }
                            if let Some(id) = data
                                .get("backend_session_id")
                                .and_then(|v| v.as_str())
                                .and_then(clean_external_thread_id)
                                .filter(|id| scraped_external_thread_id_is_canonical(id))
                            {
                                external_resume_id = Some(id);
                            }
                        }
                    }
                }
                "info" | "debug" => {
                    if event == "info" {
                        if message.starts_with("Provider: ") && provider.is_none() {
                            provider = Some(message.trim_start_matches("Provider: ").to_string());
                        } else if message.starts_with("Model: ") && model.is_none() {
                            model = Some(message.trim_start_matches("Model: ").to_string());
                        } else if message.starts_with("Task: ") && task.is_none() {
                            task = Some(message.trim_start_matches("Task: ").to_string());
                        } else if message.starts_with("Interrupted: ")
                            || message.starts_with("External agent interrupted: ")
                        {
                            status = "interrupted".to_string();
                        }
                    }
                    if external_resume_id.is_none() {
                        external_resume_id = external_agent_thread_id_from_message(message);
                    }
                    if external_source.is_none() {
                        external_source = external_agent_source_from_message(message);
                    }
                }
                "turn_start" => {
                    status = "in_progress".to_string();
                    if let Some(t) = obj.get("turn").and_then(|v| v.as_u64()) {
                        if t > turns {
                            turns = t;
                        }
                    }
                }
                "model_response" => {
                    if let Some(tok) = obj.get("data").and_then(|d| d.get("tokens")) {
                        let mut event_usage = SessionUsage::default();
                        if let Some(t) = tok.get("total").and_then(|v| v.as_u64()) {
                            total_tokens += t;
                            event_usage.total_tokens = t;
                        }
                        if let Some(p) = tok.get("prompt").and_then(|v| v.as_u64()) {
                            prompt_tokens += p;
                            event_usage.prompt_tokens = p;
                        }
                        if let Some(c) = tok.get("completion").and_then(|v| v.as_u64()) {
                            completion_tokens += c;
                            event_usage.completion_tokens = c;
                        }
                        if let Some(cached) = tok.get("cached").and_then(|v| v.as_u64()) {
                            cached_tokens += cached;
                            event_usage.cached_tokens = cached;
                        }
                        if event_usage.total_tokens == 0 {
                            event_usage.total_tokens =
                                event_usage.prompt_tokens + event_usage.completion_tokens;
                        }
                        if !event_usage.is_empty() {
                            let day = usage_day_from_timestamp(
                                obj.get("ts")
                                    .or_else(|| obj.get("timestamp"))
                                    .and_then(|v| v.as_str()),
                            );
                            if let Some(day) = day {
                                daily_usage.entry(day).or_default().add(event_usage);
                            }
                        }
                    }
                }
                "task_complete" | "session_end" | "session_ended" => {
                    status = "completed".to_string();
                }
                "session_capabilities" => {
                    capabilities = obj
                        .get("data")
                        .and_then(|data| data.get("capabilities"))
                        .cloned();
                    if session_agent_config.is_none() {
                        let source = external_source.as_deref().or(Some("codex"));
                        let command = capabilities
                            .as_ref()
                            .and_then(|caps| caps.get("codex_command"))
                            .and_then(|v| v.as_str());
                        let mode = capabilities
                            .as_ref()
                            .and_then(|caps| caps.get("codex_managed_context"))
                            .and_then(|v| v.as_str());
                        let archive = capabilities
                            .as_ref()
                            .and_then(|caps| caps.get("codex_context_archive"))
                            .and_then(|v| v.as_str());
                        let service_tier = capabilities
                            .as_ref()
                            .and_then(|caps| caps.get("codex_service_tier"))
                            .and_then(|v| v.as_str());
                        session_agent_config = Some(crate::session_config::from_wire(
                            source,
                            command,
                            None,
                            None,
                            mode,
                            archive,
                            service_tier,
                        ));
                    }
                }
                "round_complete" => {
                    if status != "interrupted" {
                        status = "idle".to_string();
                    }
                }
                _ => {}
            }
        }
    }

    if status != "completed" && dir.join("summary.json").exists() {
        status = "completed".to_string();
    }

    let mut recording_count: u64 = 0;
    let mut recording_bytes: u64 = 0;
    let mut annotation_count: u64 = 0;
    let mut clip_count: u64 = 0;
    let mut frames_bytes: u64 = 0;
    let mut turns_bytes: u64 = 0;
    let mut logs_bytes: u64 = 0;

    let recordings_dir = dir.join("recordings");
    if recordings_dir.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&recordings_dir) {
            for re in rd.flatten() {
                if re.path().is_dir() {
                    recording_count += 1;
                    if let Ok(files) = std::fs::read_dir(re.path()) {
                        for f in files.flatten() {
                            let name = f.file_name().to_string_lossy().to_string();
                            if name.starts_with("seg_") {
                                if let Ok(m) = f.metadata() {
                                    if m.is_file() {
                                        updated_at_secs =
                                            updated_at_secs.max(metadata_mtime_secs(&m));
                                        recording_bytes += m.len();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let frames_dir = dir.join("frames");
    if frames_dir.is_dir() {
        if let Ok(fd) = std::fs::read_dir(&frames_dir) {
            let mut clip_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            for fe in fd.flatten() {
                let name = fe.file_name().to_string_lossy().to_string();
                if name.starts_with("ann-") && name.ends_with(".jpg") {
                    annotation_count += 1;
                } else if name.starts_with("clip-") && name.ends_with(".jpg") {
                    if let Some(pos) = name.rfind("-f") {
                        clip_ids.insert(name[..pos].to_string());
                    }
                }
                if let Ok(m) = fe.metadata() {
                    if m.is_file() {
                        updated_at_secs = updated_at_secs.max(metadata_mtime_secs(&m));
                        frames_bytes += m.len();
                    }
                }
            }
            clip_count = clip_ids.len() as u64;
        }
    }

    let turns_dir = dir.join("turns");
    if turns_dir.is_dir() {
        if let Ok(td) = std::fs::read_dir(&turns_dir) {
            for te in td.flatten() {
                if let Ok(m) = te.metadata() {
                    if m.is_file() {
                        updated_at_secs = updated_at_secs.max(metadata_mtime_secs(&m));
                        turns_bytes += m.len();
                    }
                }
            }
        }
    }

    for name in [
        "session.jsonl",
        "session_meta.json",
        crate::session_config::SESSION_AGENT_CONFIG_FILE,
        "summary.json",
        "conversation.jsonl",
    ] {
        if let Ok(m) = std::fs::metadata(dir.join(name)) {
            if m.is_file() {
                updated_at_secs = updated_at_secs.max(metadata_mtime_secs(&m));
                logs_bytes += m.len();
            }
        }
    }

    let total_bytes = recording_bytes + frames_bytes + turns_bytes + logs_bytes;

    if status != "completed" {
        let has_model_work = turns > 0 || total_tokens > 0;
        if !has_model_work {
            let has_media = recording_count > 0 || annotation_count > 0 || clip_count > 0;
            if task.is_some() || has_media {
                status = "idle".to_string();
            } else {
                status = "abandoned".to_string();
            }
        }
    }

    if created_at.is_none() {
        created_at = mtime_secs_to_string(file_mtime_secs(dir));
    }

    let estimated_cost = model.as_deref().and_then(|m| {
        crate::app_state_pricing::estimate_session_cost(
            m,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            0,
        )
    });

    let created_at = created_at.unwrap_or_default();
    let updated_at = mtime_secs_to_string(updated_at_secs).unwrap_or_else(|| created_at.clone());
    let backend_source_label: Option<String> = None;
    let relationships = session_relationships_from_log_dir(dir);

    let mut wrapper_session = serde_json::json!({
        "source": "intendant",
        "source_label": "Intendant",
        "session_id": session_id,
        "resume_id": session_id,
        "backend_source": external_source.clone(),
        "backend_source_label": backend_source_label,
        "backend_session_id": external_resume_id.clone(),
        "capabilities": capabilities,
        "created_at": created_at,
        "updated_at": updated_at,
        "name": name,
        "task": task,
        "provider": provider,
        "model": model,
        "turns": turns,
        "status": status,
        "total_tokens": total_tokens,
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "cached_tokens": cached_tokens,
        "cache_creation_tokens": 0,
        "estimated_cost": estimated_cost.unwrap_or(0.0),
        "pricing_known": estimated_cost.is_some(),
        "role": role,
        "recordings": recording_count,
        "recording_bytes": recording_bytes,
        "annotations": annotation_count,
        "clips": clip_count,
        "frames_bytes": frames_bytes,
        "turns_bytes": turns_bytes,
        "logs_bytes": logs_bytes,
        "total_bytes": total_bytes,
        "cwd": cwd.clone().or_else(|| project_root.clone()),
        "project_root": project_root.clone(),
        "path": dir.to_string_lossy().to_string(),
        "can_delete": true,
        "can_resume": true,
        "relationships": relationships,
    });
    if let Some(config) = session_agent_config.as_ref() {
        crate::session_config::apply_config_to_session_json(&mut wrapper_session, config);
    }
    apply_session_daily_usage(&mut wrapper_session, &daily_usage, model.as_deref());

    store_intendant_session_list_row(fingerprint, &wrapper_session);
    Some(wrapper_session)
}

pub(crate) fn json_string_missing_or_empty(session: &serde_json::Value, key: &str) -> bool {
    session
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::is_empty)
        .unwrap_or(true)
}

pub(crate) fn insert_optional_string(
    session: &mut serde_json::Value,
    key: &str,
    value: Option<String>,
) {
    if let Some(obj) = session.as_object_mut() {
        obj.insert(
            key.to_string(),
            value
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
    }
}

pub(crate) fn apply_external_context_to_intendant_wrapper(
    wrapper_session: &mut serde_json::Value,
    external_context_by_id: &HashMap<String, ExternalSessionContext>,
) {
    let external_resume_id = value_str(wrapper_session, "backend_session_id");
    if let Some(external_id) = external_resume_id.as_deref() {
        if let Some(context) = external_context_by_id.get(external_id) {
            if json_string_missing_or_empty(wrapper_session, "project_root") {
                insert_optional_string(
                    wrapper_session,
                    "project_root",
                    context.project_root.clone(),
                );
            }
            if json_string_missing_or_empty(wrapper_session, "cwd") {
                insert_optional_string(
                    wrapper_session,
                    "cwd",
                    context.cwd.clone().or_else(|| context.project_root.clone()),
                );
            }
            if json_string_missing_or_empty(wrapper_session, "name") {
                insert_optional_string(wrapper_session, "name", context.name.clone());
            }
            if json_string_missing_or_empty(wrapper_session, "backend_source") {
                insert_optional_string(wrapper_session, "backend_source", context.source.clone());
            }
        }
    }

    let backend_source_label = value_str(wrapper_session, "backend_source").and_then(|source| {
        external_resume_id
            .as_deref()
            .and_then(|external_id| external_context_by_id.get(external_id))
            .and_then(|context| context.source_label.clone())
            .or_else(|| Some(pretty_external_source_label(&source)))
    });
    insert_optional_string(
        wrapper_session,
        "backend_source_label",
        backend_source_label,
    );
}

pub(crate) fn list_sessions_from_home(home_path: &Path) -> String {
    list_sessions_from_home_impl(home_path, true, EXTERNAL_SESSION_SCAN_LIMIT, None)
}

pub(crate) fn list_sessions_from_home_with_limit(home_path: &Path, limit: Option<usize>) -> String {
    let limit = limit.map(|limit| limit.clamp(1, SESSION_LIST_LIMIT));
    let external_scan_limit = limit
        .map(|limit| {
            limit
                .saturating_add(SESSION_SOURCE_FLOOR * 3)
                .clamp(SESSION_SOURCE_FLOOR, EXTERNAL_SESSION_SCAN_LIMIT)
        })
        .unwrap_or(EXTERNAL_SESSION_SCAN_LIMIT);
    list_sessions_from_home_impl(home_path, true, external_scan_limit, limit)
}

pub(crate) fn intendant_session_skeleton_from_dir(
    dir: &Path,
    session_id: &str,
) -> serde_json::Value {
    let meta_path = dir.join("session_meta.json");
    let mut name: Option<String> = None;
    let mut task: Option<String> = None;
    let mut created_at: Option<String> = None;
    let mut project_root: Option<String> = None;
    let mut status = "in_progress".to_string();
    let mut role: Option<String> = None;
    if let Ok(meta_str) = std::fs::read_to_string(&meta_path) {
        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str) {
            task = value_str(&meta, "task");
            created_at = value_str(&meta, "created_at");
            project_root = value_str(&meta, "project_root");
            name = value_str(&meta, "name").map(|s| compact_text(&s, 180));
            if let Some(value) = value_str(&meta, "status") {
                status = value;
            }
            role = value_str(&meta, "role");
        }
    }
    if status != "completed" && dir.join("summary.json").exists() {
        status = "completed".to_string();
    }
    if created_at.is_none() {
        created_at = mtime_secs_to_string(file_mtime_secs(dir));
    }
    let created_at = created_at.unwrap_or_default();
    let updated_at = file_mtime_string(dir).unwrap_or_else(|| created_at.clone());
    serde_json::json!({
        "source": "intendant",
        "source_label": "Intendant",
        "session_id": session_id,
        "resume_id": session_id,
        "created_at": created_at,
        "updated_at": updated_at,
        "name": name,
        "task": task,
        "provider": null,
        "model": null,
        "turns": 0,
        "status": status,
        "total_tokens": 0,
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "cached_tokens": 0,
        "cache_creation_tokens": 0,
        "estimated_cost": 0.0,
        "pricing_known": false,
        "role": role,
        "recordings": 0,
        "recording_bytes": 0,
        "annotations": 0,
        "clips": 0,
        "frames_bytes": 0,
        "turns_bytes": 0,
        "logs_bytes": 0,
        "total_bytes": 0,
        "cwd": project_root.clone(),
        "project_root": project_root,
        "path": dir.to_string_lossy().to_string(),
        "can_delete": true,
        "can_resume": true,
        "partial": true,
    })
}

pub(crate) fn list_intendant_skeleton_sessions_with_limit(
    home_path: &Path,
    limit: usize,
) -> Vec<serde_json::Value> {
    let logs_dir = home_path.join(".intendant").join("logs");
    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return Vec::new();
    };
    let mut dirs = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            if !dir.is_dir() {
                return None;
            }
            let mtime = file_mtime_secs(&dir);
            Some((dir, mtime))
        })
        .collect::<Vec<_>>();
    dirs.sort_by(|a, b| b.1.cmp(&a.1));
    dirs.truncate(limit);
    dirs.into_iter()
        .filter_map(|(dir, _)| {
            let session_id = dir.file_name()?.to_string_lossy().to_string();
            Some(intendant_session_skeleton_from_dir(&dir, &session_id))
        })
        .collect()
}

pub(crate) fn merge_quick_session_rows_with_wrapper_index(
    home: &Path,
    rows: &mut Vec<serde_json::Value>,
) {
    apply_external_wrapper_index_to_sessions(home, rows);
    let wrapped_intendant_ids = rows
        .iter()
        .filter(|session| {
            value_str(session, "source")
                .map(|source| crate::session_names::normalize_source(&source))
                .as_deref()
                != Some("intendant")
        })
        .filter_map(|session| value_str(session, "intendant_session_id"))
        .collect::<HashSet<_>>();
    if wrapped_intendant_ids.is_empty() {
        return;
    }
    rows.retain(|session| {
        if value_str(session, "source")
            .map(|source| crate::session_names::normalize_source(&source))
            .as_deref()
            != Some("intendant")
        {
            return true;
        }
        value_str(session, "session_id")
            .map(|id| !wrapped_intendant_ids.contains(&id))
            .unwrap_or(true)
    });
}

pub(crate) fn list_sessions_for_deep_search_from_home(home_path: &Path) -> String {
    list_sessions_from_home_impl(home_path, false, usize::MAX, None)
}

pub(crate) fn list_sessions_from_home_impl(
    home_path: &Path,
    truncate_for_list_view: bool,
    external_scan_limit: usize,
    requested_limit: Option<usize>,
) -> String {
    preload_session_index();
    let logs_dir = home_path.join(".intendant").join("logs");
    let mut external_sessions = Vec::new();
    external_sessions.extend(list_codex_sessions_with_limit(
        home_path,
        external_scan_limit,
    ));
    external_sessions.extend(list_claude_sessions_with_limit(
        home_path,
        external_scan_limit,
    ));
    external_sessions.extend(list_gemini_sessions_with_limit(
        home_path,
        external_scan_limit,
    ));
    let deleted_external_sessions = read_deleted_external_sessions(home_path);
    if !deleted_external_sessions.is_empty() {
        external_sessions.retain(|session| {
            !session_matches_deleted_external(session, &deleted_external_sessions)
        });
    }
    crate::session_names::apply_session_name_overlays(home_path, &mut external_sessions);
    crate::session_config::apply_overlays_to_sessions(home_path, &mut external_sessions);
    if !logs_dir.is_dir() {
        sort_sessions_newest_first(&mut external_sessions);
        if truncate_for_list_view {
            truncate_sessions_preserving_sources(&mut external_sessions);
        }
        return serde_json::to_string(&external_sessions).unwrap_or_else(|_| "[]".to_string());
    }
    let external_context_by_id = external_session_context_by_id(&external_sessions);

    let mut sessions: Vec<serde_json::Value> = Vec::new();

    let entries = match std::fs::read_dir(&logs_dir) {
        Ok(e) => e,
        Err(_) => return "[]".to_string(),
    };

    let mut dirs = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            if !dir.is_dir() {
                return None;
            }
            let mtime = file_mtime_secs(&dir);
            Some((dir, mtime))
        })
        .collect::<Vec<_>>();
    if truncate_for_list_view {
        dirs.sort_by(|a, b| b.1.cmp(&a.1));
        if let Some(limit) = requested_limit {
            let scan_limit = limit
                .saturating_add(SESSION_SOURCE_FLOOR * 3)
                .clamp(limit, SESSION_LIST_LIMIT);
            dirs.truncate(scan_limit);
        }
    }

    for (dir, _) in dirs {
        let session_id = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if let Some(mut wrapper_session) = intendant_session_list_row_from_dir(&dir, &session_id) {
            index_external_wrapper_session_row(home_path, &wrapper_session);
            apply_external_context_to_intendant_wrapper(
                &mut wrapper_session,
                &external_context_by_id,
            );
            let external_source = value_str(&wrapper_session, "backend_source");
            let external_resume_id = value_str(&wrapper_session, "backend_session_id");
            let merged_into_external = external_source
                .as_deref()
                .zip(external_resume_id.as_deref())
                .filter(|(source, external_id)| {
                    crate::external_agent::source_session_id_is_canonical(source, external_id)
                })
                .and_then(|(source, external_id)| {
                    external_sessions
                        .iter_mut()
                        .find(|session| external_session_row_matches(session, source, external_id))
                })
                .map(|external| {
                    merge_intendant_wrapper_into_external_session(external, &wrapper_session);
                })
                .is_some();

            if !merged_into_external {
                sessions.push(wrapper_session);
            }
            continue;
        }
    }

    sessions.extend(external_sessions);
    apply_external_wrapper_index_to_sessions(home_path, &mut sessions);

    sort_sessions_newest_first(&mut sessions);
    if let Some(limit) = requested_limit {
        truncate_sessions_preserving_sources_to(&mut sessions, limit);
    } else if truncate_for_list_view {
        truncate_sessions_preserving_sources(&mut sessions);
    }

    serde_json::to_string(&sessions).unwrap_or_else(|_| "[]".to_string())
}

pub(crate) fn send_session_stream_event(
    tx: &tokio::sync::mpsc::Sender<String>,
    event: serde_json::Value,
) -> bool {
    let mut line = event.to_string();
    line.push('\n');
    tx.blocking_send(line).is_ok()
}

pub(crate) fn send_session_stream_rows(
    tx: &tokio::sync::mpsc::Sender<String>,
    rows: Vec<serde_json::Value>,
    partial: bool,
) -> bool {
    for mut row in rows {
        if let Some(obj) = row.as_object_mut() {
            obj.insert("partial".to_string(), serde_json::Value::Bool(partial));
        }
        if !send_session_stream_event(
            tx,
            serde_json::json!({
                "type": "session",
                "partial": partial,
                "session": row,
            }),
        ) {
            return false;
        }
    }
    true
}

pub(crate) fn stream_sessions_from_request(
    request_line: &str,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    let requested_limit = session_list_limit_from_request(request_line);
    let quick_limit = requested_limit
        .unwrap_or(SESSION_LIST_LIMIT)
        .min(SESSION_LIST_STREAM_QUICK_LIMIT);
    let home = crate::platform::home_dir();
    if !send_session_stream_event(
        &tx,
        serde_json::json!({
            "type": "start",
            "limit": requested_limit,
            "quick_limit": quick_limit,
        }),
    ) {
        return;
    }

    let mut quick_rows = Vec::new();
    quick_rows.extend(list_intendant_skeleton_sessions_with_limit(
        &home,
        quick_limit,
    ));
    quick_rows.extend(list_codex_index_skeleton_sessions_with_limit(
        &home,
        quick_limit,
    ));
    merge_quick_session_rows_with_wrapper_index(&home, &mut quick_rows);
    sort_sessions_newest_first(&mut quick_rows);
    truncate_sessions_preserving_sources_to(&mut quick_rows, quick_limit);
    if !send_session_stream_rows(&tx, quick_rows, true) {
        return;
    }
    if !send_session_stream_event(
        &tx,
        serde_json::json!({
            "type": "phase",
            "phase": "hydrating",
        }),
    ) {
        return;
    }

    let body = requested_limit
        .map(cached_list_sessions_with_limit)
        .unwrap_or_else(cached_list_sessions);
    let rows = serde_json::from_str::<Vec<serde_json::Value>>(&body).unwrap_or_default();
    let _ = send_session_stream_event(
        &tx,
        serde_json::json!({
            "type": "replace",
            "sessions": rows,
        }),
    );
    let _ = send_session_stream_event(
        &tx,
        serde_json::json!({
            "type": "done",
        }),
    );
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

    #[test]
    fn preview_text_truncates_on_char_boundary() {
        let text = "Wait, the `CONCURRENT AGENTS (n)` indicator is at the top — where";

        assert_eq!(
            preview_text(text, 60),
            "Wait, the `CONCURRENT AGENTS (n)` indicator is at the top — ..."
        );
    }

    #[test]
    fn preview_text_leaves_short_unicode_unchanged() {
        assert_eq!(preview_text("top — where", 60), "top — where");
    }

    #[test]
    fn external_session_json_falls_back_to_created_at_for_updated_at() {
        let session = external_session_json(
            "codex",
            "Codex",
            "session-1".to_string(),
            "session-1".to_string(),
            Some("2026-05-17T10:00:00Z".to_string()),
            None,
            Some("name".to_string()),
            Some("task".to_string()),
            "Codex",
            None,
            1,
            None,
            None,
            None,
            0,
        );

        assert_eq!(session["created_at"], "2026-05-17T10:00:00Z");
        assert_eq!(session["updated_at"], "2026-05-17T10:00:00Z");
        assert_eq!(session["name"], "name");
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
    fn codex_observed_worktree_hints_include_exec_workdirs() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("projects").join("codex");
        let worktree = repo.join(".worktrees").join("vanilla-upstream");
        let worktree_src = worktree.join("src");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&worktree_src).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../.git/worktrees/vanilla\n",
        )
        .unwrap();

        let session_id = "019e37ae-worktree-hints";
        let rollout = home.path().join("rollout.jsonl");
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-27T13:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-27T13:01:00Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": serde_json::json!({
                        "workdir": worktree_src.to_string_lossy().to_string()
                    }).to_string()
                }
            }),
        ];
        std::fs::write(
            &rollout,
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let mut status_by_session = HashMap::new();
        status_by_session.insert(
            ("codex".to_string(), session_id.to_string()),
            (
                "running".to_string(),
                Some("2026-05-27T13:02:00Z".to_string()),
            ),
        );

        let hints = codex_observed_worktree_session_hints_from_file(
            home.path(),
            &rollout,
            &status_by_session,
        );

        assert!(hints.iter().any(|hint| {
            hint.project_root
                .as_ref()
                .map(|root| worktree_hint_path_key(root) == worktree_hint_path_key(&repo))
                .unwrap_or(false)
        }));
        let worktree_hint = hints
            .iter()
            .find(|hint| {
                hint.project_root
                    .as_ref()
                    .map(|root| worktree_hint_path_key(root) == worktree_hint_path_key(&worktree))
                    .unwrap_or(false)
            })
            .expect("exec workdir hint should resolve to linked worktree root");
        assert_eq!(worktree_hint.source, "codex");
        assert_eq!(worktree_hint.session_id, session_id);
        assert_eq!(worktree_hint.status, "running");
        assert_eq!(
            worktree_hint.updated_at.as_deref(),
            Some("2026-05-27T13:02:00Z")
        );
    }

    #[test]
    fn list_sessions_cache_invalidates_intendant_log_changes() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "intendant-cache-session";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": session_id,
                "created_at": "2026-05-17T20:44:00",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();

        let write_task = |task: &str| {
            std::fs::write(
                log_dir.join("session.jsonl"),
                serde_json::json!({
                    "ts": "2026-05-17T20:45:00",
                    "event": "info",
                    "message": format!("Task: {task}")
                })
                .to_string(),
            )
            .unwrap();
        };
        write_task("First cache task");
        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        let row = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .unwrap();
        assert_eq!(
            row.get("task").and_then(|v| v.as_str()),
            Some("First cache task")
        );

        write_task("Second cache invalidated task");
        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        let row = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .unwrap();
        assert_eq!(
            row.get("task").and_then(|v| v.as_str()),
            Some("Second cache invalidated task")
        );
    }

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
    fn list_codex_sessions_uses_first_real_user_message() {
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-f523-73b0-8bb4-01be02f30ebd";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": id,
                "updated_at": "2026-05-17T20:44:33Z",
                "thread_name": "# AGENTS.md instructions for /Users/vm/projects/intendant <INSTRUCTIONS>"
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
                    "cwd": "/Users/vm/projects/intendant",
                    "model": "gpt-5.5",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /Users/vm/projects/intendant <INSTRUCTIONS>"}]
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Fix the Sessions tab"}]
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix the Sessions tab"}
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

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix the Sessions tab")
        );
        assert_eq!(session.get("name").and_then(|v| v.as_str()), None);
        assert_eq!(session.get("turns").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn targeted_intendant_session_list_accepts_prefix() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "abcdef12-3456-7890-abcd-ef1234567890";
        let log_dir = home.path().join(".intendant").join("logs").join(session_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "created_at": "2026-06-07T15:00:00Z",
                "task": "targeted prefix task",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-06-07T15:00:00Z",
                "event": "session_start"
            })
            .to_string(),
        )
        .unwrap();

        let body = cached_list_sessions_for_ids_from_home(home.path(), &["abcdef12".to_string()]);
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].get("session_id").and_then(|v| v.as_str()),
            Some(session_id)
        );
        assert_eq!(
            sessions[0].get("task").and_then(|v| v.as_str()),
            Some("targeted prefix task")
        );
    }

    #[test]
    fn list_codex_sessions_cache_invalidates_when_file_changes() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let id = "019e37ae-cache-invalidates";
        let session_path = sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl"));

        let write_task = |task: &str| {
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
                    "timestamp": "2026-05-17T20:45:21Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "user_message",
                        "message": task
                    }
                }),
            ];
            std::fs::write(
                &session_path,
                lines
                    .iter()
                    .map(serde_json::Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
        };

        write_task("First cached Codex task");
        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .unwrap();
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("First cached Codex task")
        );

        write_task("Second invalidated Codex task");
        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .unwrap();
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Second invalidated Codex task")
        );
    }

    #[test]
    fn list_sessions_applies_external_session_name_overlay() {
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-overlay-name";
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
                "payload": {"type": "user_message", "message": "Fix naming"}
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
        crate::session_names::rename_session(home.path(), "codex", id, "Overlay name").unwrap();

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("name").and_then(|v| v.as_str()),
            Some("Overlay name")
        );
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix naming")
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
    fn list_codex_sessions_separates_project_root_from_latest_command_cwd() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let command_cwd = repo.join(".worktrees").join("feature");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&command_cwd).unwrap();

        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-project-cwd-split";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {
                    "type": "exec_command_end",
                    "cwd": command_cwd.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:22Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Inspect cwd"}
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

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        let expected_project_root = repo.to_string_lossy().to_string();
        let expected_cwd = command_cwd.to_string_lossy().to_string();
        assert_eq!(
            session.get("project_root").and_then(|v| v.as_str()),
            Some(expected_project_root.as_str())
        );
        assert_eq!(
            session.get("cwd").and_then(|v| v.as_str()),
            Some(expected_cwd.as_str())
        );
    }

    #[test]
    fn list_codex_sessions_uses_function_call_workdir_as_latest_cwd() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let command_cwd = repo.join(".worktrees").join("live-cwd");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&command_cwd).unwrap();

        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-function-call-workdir";
        let arguments = serde_json::json!({
            "cmd": "pwd",
            "workdir": command_cwd.to_string_lossy()
        })
        .to_string();
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": arguments
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:22Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Inspect cwd"}
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

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        let expected_project_root = repo.to_string_lossy().to_string();
        let expected_cwd = command_cwd.to_string_lossy().to_string();
        assert_eq!(
            session.get("project_root").and_then(|v| v.as_str()),
            Some(expected_project_root.as_str())
        );
        assert_eq!(
            session.get("cwd").and_then(|v| v.as_str()),
            Some(expected_cwd.as_str())
        );
    }

    #[test]
    fn list_codex_sessions_applies_thread_rollback_to_turns_and_task() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37b2-e756-7461-9946-34b639448717";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:48:52Z",
                "type": "session_meta",
                "payload": {"id": id, "timestamp": "2026-05-17T20:48:52Z"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "old-turn"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Old prompt"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Old prompt"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "turn_aborted", "turn_id": "old-turn"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "thread_rolled_back", "num_turns": 1}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "new-turn"}
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "New prompt"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "New prompt"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-48-52-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("New prompt")
        );
        assert_eq!(session.get("turns").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn list_codex_sessions_parses_token_count_usage() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37c5-9d93-76f0-a395-f5b28bd54a74";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T21:10:00Z",
                    "cwd": "/Users/vm/projects/intendant",
                    "model": "gpt-5.4",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:01Z",
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:02Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix stats usage"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:03Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        },
                        "last_token_usage": {
                            "input_tokens": 200,
                            "cached_input_tokens": 50,
                            "output_tokens": 25,
                            "total_tokens": 225
                        },
                        "model_context_window": 258400
                    }
                }
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-10-00-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(session["prompt_tokens"].as_u64(), Some(1000));
        assert_eq!(session["completion_tokens"].as_u64(), Some(250));
        assert_eq!(session["cached_tokens"].as_u64(), Some(400));
        assert_eq!(session["total_tokens"].as_u64(), Some(1250));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.00535).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    fn list_codex_sessions_subtracts_parent_usage_from_forks() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_id = "019e37c5-parent-cost-thread";
        let child_id = "019e37c5-child-cost-thread";
        let parent_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:09:00Z",
                "type": "session_meta",
                "payload": {
                    "id": parent_id,
                    "timestamp": "2026-05-17T21:09:00Z",
                    "model": "gpt-5.4",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:09:30Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:11:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 2000,
                            "cached_input_tokens": 600,
                            "output_tokens": 400,
                            "total_tokens": 2400
                        }
                    }
                }
            }),
        ];
        let child_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:00Z",
                "type": "session_meta",
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": "2026-05-17T21:10:00Z",
                    "model": "gpt-5.4",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:30Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1300,
                            "cached_input_tokens": 550,
                            "output_tokens": 300,
                            "total_tokens": 1600
                        }
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-09-00-{parent_id}.jsonl")),
            parent_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-10-00-{child_id}.jsonl")),
            child_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let parent = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(parent_id))
            .expect("parent codex session should be listed");
        let child = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(child_id))
            .expect("child codex session should be listed");

        assert_eq!(parent["total_tokens"].as_u64(), Some(2400));
        assert_eq!(child["prompt_tokens"].as_u64(), Some(300));
        assert_eq!(child["completion_tokens"].as_u64(), Some(50));
        assert_eq!(child["cached_tokens"].as_u64(), Some(150));
        assert_eq!(child["total_tokens"].as_u64(), Some(350));
        let cost = child["estimated_cost"].as_f64().unwrap();
        assert!(
            (cost - 0.0011625).abs() < 1e-12,
            "unexpected child cost {cost}"
        );
    }

    #[test]
    fn list_codex_sessions_full_scans_large_parent_for_fork_baseline() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_id = "019e37c5-large-parent-thread";
        let child_id = "019e37c5-child-large-parent";
        let large_padding = "x".repeat(EXTERNAL_SESSION_READ_LIMIT as usize + 1024);
        let parent_head = serde_json::json!({
            "timestamp": "2026-05-17T21:09:00Z",
            "type": "session_meta",
            "payload": {
                "id": parent_id,
                "timestamp": "2026-05-17T21:09:00Z",
                "model": "gpt-5.4",
                "model_provider": "openai"
            }
        })
        .to_string();
        let parent_early_usage = serde_json::json!({
            "timestamp": "2026-05-17T21:09:10Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 1000,
                        "cached_input_tokens": 400,
                        "output_tokens": 250,
                        "total_tokens": 1250
                    }
                }
            }
        })
        .to_string();
        let parent_middle_usage = serde_json::json!({
            "timestamp": "2026-05-17T21:09:59Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 2000,
                        "cached_input_tokens": 700,
                        "output_tokens": 500,
                        "total_tokens": 2500
                    }
                }
            }
        })
        .to_string();
        let parent_late_usage = serde_json::json!({
            "timestamp": "2026-05-17T21:12:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 6000,
                        "cached_input_tokens": 1200,
                        "output_tokens": 900,
                        "total_tokens": 6900
                    }
                }
            }
        })
        .to_string();
        let parent_contents = [
            parent_head,
            parent_early_usage,
            large_padding.clone(),
            parent_middle_usage,
            large_padding,
            parent_late_usage,
        ]
        .join("\n");
        let child_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:00Z",
                "type": "session_meta",
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": "2026-05-17T21:10:00Z",
                    "model": "gpt-5.4",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:30Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 2400,
                            "cached_input_tokens": 850,
                            "output_tokens": 550,
                            "total_tokens": 2950
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:11:30Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 2700,
                            "cached_input_tokens": 950,
                            "output_tokens": 650,
                            "total_tokens": 3350
                        }
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-09-00-{parent_id}.jsonl")),
            parent_contents,
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-10-00-{child_id}.jsonl")),
            child_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let child = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(child_id))
            .expect("child codex session should be listed");

        assert_eq!(child["prompt_tokens"].as_u64(), Some(700));
        assert_eq!(child["completion_tokens"].as_u64(), Some(150));
        assert_eq!(child["cached_tokens"].as_u64(), Some(250));
        assert_eq!(child["total_tokens"].as_u64(), Some(850));
    }

    #[test]
    fn list_codex_sessions_keeps_cumulative_usage_after_thread_rollback() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37c5-rollback-cost-thread";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T21:10:00Z",
                    "model": "gpt-5.4",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        },
                        "last_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:02Z",
                "type": "event_msg",
                "payload": {"type": "thread_rolled_back", "num_turns": 1}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:03Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        },
                        "last_token_usage": {
                            "input_tokens": 0,
                            "cached_input_tokens": 0,
                            "output_tokens": 0,
                            "total_tokens": 120
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:04Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1300,
                            "cached_input_tokens": 550,
                            "output_tokens": 300,
                            "total_tokens": 1600
                        },
                        "last_token_usage": {
                            "input_tokens": 300,
                            "cached_input_tokens": 150,
                            "output_tokens": 50,
                            "total_tokens": 350
                        }
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-10-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(session["prompt_tokens"].as_u64(), Some(1300));
        assert_eq!(session["completion_tokens"].as_u64(), Some(300));
        assert_eq!(session["cached_tokens"].as_u64(), Some(550));
        assert_eq!(session["total_tokens"].as_u64(), Some(1600));
    }

    #[test]
    fn list_codex_sessions_inherits_model_from_parent_thread() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_id = "019e37c5-parent-model-thread";
        let child_id = "019e37c5-child-forked-thread";
        let parent_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:09:00Z",
                "type": "session_meta",
                "payload": {
                    "id": parent_id,
                    "timestamp": "2026-05-17T21:09:00Z",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:09:01Z",
                "type": "turn_context",
                "payload": {"model": "gpt-5.5"}
            }),
        ];
        let child_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:00Z",
                "type": "session_meta",
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": "2026-05-17T21:10:00Z",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:01Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Use inherited model"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:10:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 400,
                            "output_tokens": 250,
                            "total_tokens": 1250
                        }
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-09-00-{parent_id}.jsonl")),
            parent_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T21-10-00-{child_id}.jsonl")),
            child_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(child_id))
            .expect("child codex session should be listed");
        assert_eq!(session["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(session["pricing_known"].as_bool(), Some(true));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.0107).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    fn list_claude_sessions_parses_and_deduplicates_usage() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-Users-vm-projects-intendant");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_id = "019e37cf-34ad-7b08-8a1e-7ad5086eb39f";
        let assistant = serde_json::json!({
            "timestamp": "2026-05-17T21:20:02Z",
            "type": "assistant",
            "cwd": "/Users/vm/projects/intendant",
            "requestId": "req-usage-1",
            "message": {
                "id": "msg-usage-1",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 10,
                    "cache_creation_input_tokens": 20,
                    "cache_read_input_tokens": 30,
                    "output_tokens": 40
                }
            }
        });
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:20:00Z",
                "type": "user",
                "cwd": "/Users/vm/projects/intendant",
                "message": {"content": "Fix stats usage"}
            }),
            assistant.clone(),
            assistant,
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(project_dir.join(format!("{session_id}.jsonl")), contents).unwrap();

        let sessions = list_claude_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .expect("claude session should be listed");
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix stats usage")
        );
        assert_eq!(session["prompt_tokens"].as_u64(), Some(60));
        assert_eq!(session["completion_tokens"].as_u64(), Some(40));
        assert_eq!(session["cached_tokens"].as_u64(), Some(30));
        assert_eq!(session["cache_creation_tokens"].as_u64(), Some(20));
        assert_eq!(session["total_tokens"].as_u64(), Some(100));
        assert_eq!(session["turns"].as_u64(), Some(1));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.000714).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    fn list_claude_sessions_counts_usage_in_large_file_middle() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-Users-vm-projects-intendant");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_id = "019e37cf-large-middle-usage";
        let user = serde_json::json!({
            "timestamp": "2026-05-17T21:20:00Z",
            "type": "user",
            "cwd": "/Users/vm/projects/intendant",
            "message": {"content": "Fix stats usage"}
        });
        let assistant = serde_json::json!({
            "timestamp": "2026-05-17T21:20:02Z",
            "type": "assistant",
            "cwd": "/Users/vm/projects/intendant",
            "requestId": "req-middle",
            "message": {
                "id": "msg-middle",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 1000,
                    "cache_creation_input_tokens": 2000,
                    "cache_read_input_tokens": 3000,
                    "output_tokens": 4000
                }
            }
        });
        let filler = "x".repeat(EXTERNAL_SESSION_READ_LIMIT as usize + 64);
        let contents = format!("{}\n{}\n{}\n{}\n", user, filler, assistant, filler);
        std::fs::write(project_dir.join(format!("{session_id}.jsonl")), contents).unwrap();

        let sessions = list_claude_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .expect("claude session should be listed");
        assert_eq!(session["prompt_tokens"].as_u64(), Some(6000));
        assert_eq!(session["completion_tokens"].as_u64(), Some(4000));
        assert_eq!(session["cached_tokens"].as_u64(), Some(3000));
        assert_eq!(session["cache_creation_tokens"].as_u64(), Some(2000));
        assert_eq!(session["total_tokens"].as_u64(), Some(10000));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.0714).abs() < 1e-12, "unexpected cost {cost}");
    }

    #[test]
    #[cfg(unix)]
    fn list_claude_sessions_deduplicates_symlinked_project_dirs() {
        let home = tempfile::tempdir().unwrap();
        let projects_dir = home.path().join(".claude").join("projects");
        let project_dir = projects_dir.join("-Users-vm-projects-intendant");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_id = "019e37cf-symlink-dedupe";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:20:00Z",
                "type": "user",
                "cwd": "/Users/vm/projects/intendant",
                "message": {"content": "Fix stats usage"}
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:20:02Z",
                "type": "assistant",
                "cwd": "/Users/vm/projects/intendant",
                "requestId": "req-usage",
                "message": {
                    "id": "msg-usage",
                    "model": "claude-sonnet-4-6",
                    "usage": {"input_tokens": 10, "output_tokens": 20}
                }
            }),
        ];
        std::fs::write(
            project_dir.join(format!("{session_id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            &project_dir,
            projects_dir.join("-Volumes-Untitled-projects-intendant"),
        )
        .unwrap();

        let sessions = list_claude_sessions(home.path());
        let matching = sessions
            .iter()
            .filter(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .count();
        assert_eq!(matching, 1);
    }

    #[test]
    fn list_gemini_sessions_parses_token_usage() {
        let home = tempfile::tempdir().unwrap();
        let chats_dir = home
            .path()
            .join(".gemini")
            .join("tmp")
            .join("sample-project")
            .join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();

        let session_id = "session-2026-05-18T09-30-gemini";
        let session = serde_json::json!({
            "sessionId": session_id,
            "startTime": "2026-05-18T09:30:00Z",
            "messages": [
                {
                    "type": "user",
                    "timestamp": "2026-05-18T09:30:01Z",
                    "content": "Fix stats usage"
                },
                {
                    "type": "assistant",
                    "timestamp": "2026-05-18T09:30:02Z",
                    "model": "gemini-2.5-flash",
                    "tokens": {
                        "input": 1000,
                        "cached": 100,
                        "output": 20,
                        "thoughts": 30,
                        "tool": 5,
                        "total": 1055
                    },
                    "content": "Done"
                }
            ]
        });
        std::fs::write(
            chats_dir.join(format!("{session_id}.json")),
            serde_json::to_string(&session).unwrap(),
        )
        .unwrap();

        let sessions = list_gemini_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(session_id))
            .expect("gemini session should be listed");
        assert_eq!(session["prompt_tokens"].as_u64(), Some(1000));
        assert_eq!(session["completion_tokens"].as_u64(), Some(55));
        assert_eq!(session["cached_tokens"].as_u64(), Some(100));
        assert_eq!(session["total_tokens"].as_u64(), Some(1055));
        assert_eq!(session["turns"].as_u64(), Some(1));
        assert_eq!(session["model"].as_str(), Some("gemini-2.5-flash"));
        assert_eq!(session["pricing_known"].as_bool(), Some(true));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.0004105).abs() < 1e-12, "unexpected cost {cost}");
        let daily = session["daily_usage"].as_array().expect("daily usage");
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0]["day"].as_str(), Some("2026-05-18"));
        assert_eq!(daily[0]["total_tokens"].as_u64(), Some(1055));
        assert_eq!(daily[0]["estimated_cost"].as_f64(), Some(cost));
    }

    #[test]
    fn sort_sessions_newest_first_uses_updated_at() {
        let mut sessions = vec![
            serde_json::json!({
                "session_id": "newer-created",
                "created_at": "2026-05-17T11:00:00Z",
                "updated_at": "2026-05-17T11:00:00Z",
            }),
            serde_json::json!({
                "session_id": "recently-changed",
                "created_at": "2026-05-17T08:00:00Z",
                "updated_at": "2026-05-17T12:00:00Z",
            }),
            serde_json::json!({
                "session_id": "fallback-created",
                "created_at": "2026-05-17T10:30:00Z",
            }),
        ];

        sort_sessions_newest_first(&mut sessions);
        let ids: Vec<_> = sessions
            .iter()
            .filter_map(|s| s.get("session_id").and_then(|v| v.as_str()))
            .collect();

        assert_eq!(
            ids,
            vec!["recently-changed", "newer-created", "fallback-created"]
        );
    }

    #[test]
    fn filter_session_list_by_ids_matches_session_and_backend_ids() {
        let body = serde_json::json!([
            {
                "session_id": "wrapper-a",
                "backend_session_id": "backend-a",
                "intendant_session_id": "intendant-a",
                "source": "codex"
            },
            {
                "session_id": "standalone-b",
                "resume_id": "resume-b",
                "source": "intendant"
            },
            {
                "session_id": "other-c",
                "source": "codex"
            }
        ])
        .to_string();

        let filtered =
            filter_session_list_by_ids(&body, &["backend-a".to_string(), "resume-b".to_string()]);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&filtered).unwrap();
        let ids: Vec<_> = rows
            .iter()
            .filter_map(|row| row.get("session_id").and_then(|v| v.as_str()))
            .collect();

        assert_eq!(ids, vec!["wrapper-a", "standalone-b"]);
    }

    #[test]
    fn session_ids_filter_from_request_distinguishes_absent_and_empty_filters() {
        assert!(session_ids_filter_from_request("GET /api/sessions HTTP/1.1").is_none());
        assert_eq!(
            session_ids_filter_from_request("GET /api/sessions?ids=ok-id,%2E%2E%2Fbad HTTP/1.1"),
            Some(vec!["ok-id".to_string()])
        );
        assert_eq!(
            session_ids_filter_from_request("GET /api/sessions?ids=..%2Fbad HTTP/1.1"),
            Some(Vec::new())
        );
    }

    #[test]
    fn session_list_limit_from_request_parses_bounded_limits() {
        assert_eq!(
            session_list_limit_from_request("GET /api/sessions?limit=250 HTTP/1.1"),
            Some(250)
        );
        assert_eq!(
            session_list_limit_from_request("GET /api/sessions?max=7 HTTP/1.1"),
            Some(7)
        );
        assert_eq!(
            session_list_limit_from_request("GET /api/sessions?limit=all HTTP/1.1"),
            None
        );
        assert_eq!(
            session_list_limit_from_request("GET /api/sessions?limit=0 HTTP/1.1"),
            None
        );
        assert_eq!(
            session_list_limit_from_request("GET /api/sessions?limit=999999 HTTP/1.1"),
            Some(SESSION_LIST_LIMIT)
        );
    }

    #[test]
    fn limit_session_list_body_keeps_recent_prefix() {
        let body = serde_json::json!([
            { "session_id": "newest" },
            { "session_id": "middle" },
            { "session_id": "oldest" }
        ])
        .to_string();

        let limited = limit_session_list_body(&body, Some(2));
        let rows: Vec<serde_json::Value> = serde_json::from_str(&limited).unwrap();
        let ids: Vec<_> = rows
            .iter()
            .filter_map(|row| row.get("session_id").and_then(|v| v.as_str()))
            .collect();

        assert_eq!(ids, vec!["newest", "middle"]);
        assert_eq!(limit_session_list_body(&body, None), body);
    }

    #[test]
    fn list_sessions_from_home_with_limit_returns_requested_count() {
        let home = tempfile::tempdir().unwrap();
        let logs_dir = home.path().join(".intendant").join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        for idx in 0..3 {
            let session_id = format!("limit-session-{idx}");
            let log_dir = logs_dir.join(&session_id);
            std::fs::create_dir_all(&log_dir).unwrap();
            std::fs::write(
                log_dir.join("session_meta.json"),
                serde_json::json!({
                    "created_at": format!("2026-06-07T15:0{idx}:00Z"),
                    "task": format!("limit task {idx}"),
                    "status": "idle"
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                log_dir.join("session.jsonl"),
                serde_json::json!({
                    "ts": format!("2026-06-07T15:0{idx}:00Z"),
                    "event": "session_start"
                })
                .to_string(),
            )
            .unwrap();
        }

        let body = list_sessions_from_home_with_limit(home.path(), Some(2));
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn codex_detail_uses_session_meta_id_not_substring_mentions() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let target_id = "019e36b9-fffa-7b42-9070-e06db38b2abd";
        let other_id = "019e37ea-1ace-7091-ad2a-7805190330fa";

        std::fs::write(
            sessions_dir.join("a-other-session.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "timestamp": "2026-05-17T21:49:12.197Z",
                    "type": "session_meta",
                    "payload": { "id": other_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T21:49:16.518Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": format!("mentions {target_id} but is the wrong file")
                            }
                        ]
                    }
                })
            ),
        )
        .unwrap();

        let target_path = sessions_dir.join("z-target-session.jsonl");
        std::fs::write(
            &target_path,
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "timestamp": "2026-05-17T18:16:59.898Z",
                    "type": "session_meta",
                    "payload": { "id": target_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T18:17:01.000Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": "Implement a new subtab for the dashboard in the Activity tab"
                            }
                        ]
                    }
                })
            ),
        )
        .unwrap();

        assert_eq!(
            find_codex_session_file(dir.path(), target_id).as_deref(),
            Some(target_path.as_path())
        );

        let detail = external_session_detail_from_home(dir.path(), "codex", target_id)
            .expect("target session should resolve");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail
            .get("entries")
            .and_then(|v| v.as_array())
            .expect("entries should be present");
        let contents: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry.get("content").and_then(|v| v.as_str()))
            .collect();

        assert!(
            contents
                .iter()
                .any(|content| content.contains("Implement a new subtab")),
            "target session content missing: {contents:?}"
        );
        assert!(
            contents
                .iter()
                .all(|content| !content.contains("wrong file")),
            "detail included content from a substring match: {contents:?}"
        );
        assert!(entries.iter().any(|entry| {
            entry.get("source").and_then(|v| v.as_str()) == Some("user")
                && entry
                    .get("content")
                    .and_then(|v| v.as_str())
                    .is_some_and(|content| content.contains("Implement a new subtab"))
        }));
    }

    #[test]
    fn codex_transcript_filters_and_deduplicates_human_assistant_messages() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-transcript-filter";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:53Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": "internal developer context" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:54Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "# AGENTS.md instructions for /Users/vm/projects/intendant\n<INSTRUCTIONS>"
                        }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:55Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "<subagent_notification>\n{\"agent_path\":\"child\",\"status\":{\"completed\":\"done\"}}\n</subagent_notification>"
                        }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:56Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"
                        }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:57Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "<user_shell_command>\n<command>\nhtop\n</command>\n</user_shell_command>"
                        }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:58Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "<task-notification>\n<task-id>child</task-id>\n</task-notification>"
                        }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Visible prompt" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00.013Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "Visible prompt" }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04.276Z",
                    "type": "event_msg",
                    "payload": { "type": "agent_message", "message": "Visible answer" }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04.289Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Visible answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:05Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"echo hidden\"}"
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let detail = external_session_detail_from_home(dir.path(), "codex", session_id)
            .expect("codex session should resolve");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail["entries"].as_array().unwrap();
        let contents: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(contents, vec!["Visible prompt", "Visible answer"]);
        assert_eq!(entries[0]["source"], "user");
        assert_eq!(entries[0]["user_turn_index"], 1);
        assert_eq!(entries[1]["source"], "codex");
    }

    #[test]
    fn external_transcript_cache_invalidates_when_source_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-cache-invalidation";
        let path = sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl"));
        let session_meta = serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        });
        let first_message = serde_json::json!({
            "timestamp": "2026-05-17T16:49:00Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "first cached message" }]
            }
        });
        let second_message = serde_json::json!({
            "timestamp": "2026-05-17T16:49:01Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "second uncached message" }]
            }
        });

        std::fs::write(&path, format!("{session_meta}\n{first_message}\n")).unwrap();
        let first = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("first load should resolve");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["content"], "first cached message");

        std::fs::write(
            &path,
            format!("{session_meta}\n{first_message}\n{second_message}\n"),
        )
        .unwrap();
        let second = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("second load should resolve");
        let contents: Vec<_> = second
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(
            contents,
            vec!["first cached message", "second uncached message"]
        );
    }

    #[test]
    fn codex_transcript_keeps_repeated_messages_outside_dedupe_window() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-transcript-repeat";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "event_msg",
                    "payload": { "type": "agent_message", "message": "Still working" }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:10Z",
                    "type": "event_msg",
                    "payload": { "type": "agent_message", "message": "Still working" }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let entries = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("codex session should resolve");
        let contents: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(contents, vec!["Still working", "Still working"]);
    }

    #[test]
    fn external_activity_replay_marks_rolled_back_context() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-overwritten-activity";
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
                        "content": [{ "type": "input_text", "text": "Old prompt" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Old answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "event_msg",
                    "payload": { "type": "thread_rolled_back", "num_turns": 1 }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:03Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "New prompt" }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        let old_prompt = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "Old prompt")
            .expect("old prompt should remain visible");
        assert_eq!(old_prompt["user_turn_index"], 1);
        assert_eq!(old_prompt["user_turn_revision"], 1);
        assert_eq!(old_prompt["superseded"], true);

        let old_answer = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "Old answer")
            .expect("old answer should remain visible");
        assert_eq!(old_answer["superseded"], true);

        let marker = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["kind"] == "rollback_marker")
            .expect("rollback marker should replay");
        assert!(marker["content"]
            .as_str()
            .is_some_and(|content| content.contains("Rewound 1 user turn")));
        assert_eq!(marker["removed_turn_ids"], serde_json::json!(["turn-1-r1"]));
        assert_eq!(
            marker["thread_history_change"]["removed_turn_ids"],
            serde_json::json!(["turn-1-r1"])
        );
        assert_eq!(marker["delivery"], "lossless");

        let new_prompt = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "New prompt")
            .expect("replacement prompt should replay");
        assert_eq!(new_prompt["user_turn_index"], 1);
        assert_eq!(new_prompt["user_turn_revision"], 2);
        assert_eq!(new_prompt["replacement_for_user_turn_index"], 1);
        assert_ne!(new_prompt["superseded"], true);
    }

    #[test]
    fn external_activity_replay_preserves_anchor_rollback_marker() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-anchor-rewind-activity";
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
                        "content": [{ "type": "input_text", "text": "Prompt" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Kept answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "thread_rolled_back",
                        "num_turns": 0,
                        "anchor": { "itemId": "call-keep", "position": "after" }
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        let marker = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["kind"] == "rollback_marker")
            .expect("anchor rollback marker should replay");
        assert_eq!(marker["rollback_turns"], 0);
        assert_eq!(marker["rollback_anchor_item_id"], "call-keep");
        assert_eq!(marker["rollback_anchor_position"], "after");
        assert!(marker["content"]
            .as_str()
            .is_some_and(|content| content.contains("Rewound to after item call-keep")));
    }

    #[test]
    fn external_activity_replay_supersedes_entries_after_item_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-item-anchor-dimming";
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
                        "id": "msg-prompt",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Prompt" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "id": "msg-keep",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Kept answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "id": "msg-noise",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Discarded noise" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:03Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "thread_rolled_back",
                        "num_turns": 0,
                        "anchor": { "itemId": "msg-keep", "position": "after" }
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        let entry_for = |needle: &str| {
            entries
                .iter()
                .find(|entry| entry["content"].as_str() == Some(needle))
                .unwrap_or_else(|| panic!("missing entry {needle}"))
                .clone()
        };
        // Everything up to and including the "after" anchor stays live; the tail is dimmed.
        assert_ne!(entry_for("Prompt")["superseded"], true);
        assert_ne!(entry_for("Kept answer")["superseded"], true);
        assert_eq!(entry_for("Discarded noise")["superseded"], true);
    }

    #[test]
    fn external_activity_replay_tracks_double_rewind_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-double-rewind-activity";
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
                        "content": [{ "type": "input_text", "text": "Original prompt" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Original answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "event_msg",
                    "payload": { "type": "thread_rolled_back", "num_turns": 1 }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:03Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Replacement one" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Replacement answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:05Z",
                    "type": "event_msg",
                    "payload": { "type": "thread_rolled_back", "num_turns": 1 }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:06Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "Replacement two" }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();

        let original = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "Original prompt")
            .expect("original prompt should replay");
        assert_eq!(original["user_turn_index"], 1);
        assert_eq!(original["user_turn_revision"], 1);
        assert_eq!(original["superseded"], true);

        let first_replacement = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "Replacement one")
            .expect("first replacement should replay");
        assert_eq!(first_replacement["user_turn_index"], 1);
        assert_eq!(first_replacement["user_turn_revision"], 2);
        assert_eq!(first_replacement["replacement_for_user_turn_index"], 1);
        assert_eq!(first_replacement["superseded"], true);

        let second_replacement = entries
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["content"] == "Replacement two")
            .expect("second replacement should replay");
        assert_eq!(second_replacement["user_turn_index"], 1);
        assert_eq!(second_replacement["user_turn_revision"], 3);
        assert_eq!(second_replacement["replacement_for_user_turn_index"], 1);
        assert_ne!(second_replacement["superseded"], true);
    }

    #[test]
    fn codex_transcript_keeps_distinct_identical_user_messages() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-identical-user-turns";
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
                        "content": [{ "type": "input_text", "text": "continue" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "continue" }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "first answer" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "continue" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "continue" }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let entries = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("codex session should parse");
        let user_entries: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.get("source").and_then(|v| v.as_str()) == Some("user")
                    && entry.get("content").and_then(|v| v.as_str()) == Some("continue")
            })
            .collect();

        assert_eq!(user_entries.len(), 2);
        assert_eq!(user_entries[0]["user_turn_index"], 1);
        assert_eq!(user_entries[1]["user_turn_index"], 2);
    }

    #[test]
    fn codex_transcript_uses_user_message_events_as_editable_turns_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-event-user-canonical";
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
                        "content": [{ "type": "input_text", "text": "provider request context" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "human prompt" }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let entries = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("codex session should parse");
        assert!(!entries
            .iter()
            .any(|entry| entry.get("content").and_then(|v| v.as_str())
                == Some("provider request context")));

        let prompt = entries
            .iter()
            .find(|entry| entry.get("content").and_then(|v| v.as_str()) == Some("human prompt"))
            .expect("event user message should be rendered");
        assert_eq!(prompt["user_turn_index"], 1);
        assert_eq!(prompt["user_turn_revision"], 1);
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
    fn external_activity_replay_limits_transcript_entries() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-e756-7461-9946-34b639448717";
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        })];
        for n in 1..=3 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-05-17T16:49:0{n}Z"),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("message {n}") }]
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

        let replay = external_session_activity_replay_from_home(dir.path(), "codex", session_id, 2)
            .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let contents: Vec<_> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "log_entry")
            .filter_map(|entry| entry["content"].as_str())
            .collect();

        assert_eq!(contents, vec!["message 2", "message 3"]);
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
    fn external_session_detail_defaults_to_bounded_compact_entries() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-compact-detail";
        let large_output = "x".repeat(WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 100);
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        })];
        for n in 1..=1005 {
            lines.push(serde_json::json!({
                "timestamp": "2026-05-17T16:49:00Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": if n % 2 == 0 { "assistant" } else { "user" },
                    "content": [{ "type": "text", "text": format!("detail message {n}") }]
                }
            }));
        }
        lines.push(serde_json::json!({
            "timestamp": "2026-05-17T16:50:00Z",
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_large",
                "output": large_output
            }
        }));
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            lines
                .into_iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let detail = external_session_detail_from_home(dir.path(), "codex", session_id)
            .expect("codex session detail should load");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail["entries"].as_array().unwrap();

        assert_eq!(entries.len(), EXTERNAL_SESSION_DETAIL_DEFAULT_ENTRY_LIMIT);
        assert_eq!(entries[0]["content"], "detail message 7");
        let stdout = entries
            .last()
            .and_then(|entry| entry["stdout"].as_str())
            .expect("large tool output should be retained in compact form");
        assert_eq!(
            stdout.len(),
            WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + "...".len()
        );
        assert!(stdout.ends_with("..."));
        let output = entries
            .last()
            .expect("large tool output entry should be retained");
        assert_eq!(output["full_output_available"], true);
        assert_eq!(
            output["full_output_bytes"],
            WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 100
        );
    }

    #[test]
    fn session_agent_output_response_loads_full_external_output() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-full-output-fetch";
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

        let response = session_agent_output_response_for_ids(
            dir.path(),
            session_id,
            "codex",
            vec!["call_large".to_string()],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["missing"].as_array().unwrap().len(), 0);
        let stdout = json["outputs"][0]["stdout"].as_str().unwrap();
        assert_eq!(
            stdout.len(),
            WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 100
        );
        assert!(!stdout.ends_with("..."));
    }

    #[test]
    fn external_session_detail_pages_before_tail() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-page-before";
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-05-17T16:48:52Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        })];
        for n in 1..=12 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-05-17T16:49:{n:02}Z"),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("paged message {n}") }]
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

        let tail = external_session_detail_from_home_with_page(
            dir.path(),
            "codex",
            session_id,
            Some(5),
            None,
        )
        .expect("tail page should load");
        let tail: serde_json::Value = serde_json::from_str(&tail).unwrap();
        assert_eq!(tail["total_entries"], 12);
        assert_eq!(tail["page_start"], 7);
        assert_eq!(tail["page_end"], 12);
        assert_eq!(tail["has_older"], true);
        let tail_contents: Vec<_> = tail["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();
        assert_eq!(
            tail_contents,
            vec![
                "paged message 8",
                "paged message 9",
                "paged message 10",
                "paged message 11",
                "paged message 12"
            ]
        );

        let previous = external_session_detail_from_home_with_page(
            dir.path(),
            "codex",
            session_id,
            Some(5),
            Some(7),
        )
        .expect("previous page should load");
        let previous: serde_json::Value = serde_json::from_str(&previous).unwrap();
        assert_eq!(previous["page_start"], 2);
        assert_eq!(previous["page_end"], 7);
        assert_eq!(previous["has_older"], true);
        let previous_contents: Vec<_> = previous["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["content"].as_str())
            .collect();
        assert_eq!(
            previous_contents,
            vec![
                "paged message 3",
                "paged message 4",
                "paged message 5",
                "paged message 6",
                "paged message 7"
            ]
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
    fn list_sessions_exposes_persisted_relationships() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("parent");
        let mut log = crate::session_log::SessionLog::open(log_dir).unwrap();
        log.session_relationship("parent", "child", "subagent", false);
        drop(log);

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(dir.path())).unwrap();
        let parent = sessions
            .iter()
            .find(|session| session["session_id"] == "parent")
            .expect("parent session should be listed");
        let relationships = parent["relationships"].as_array().unwrap();

        assert_eq!(relationships.len(), 1);
        assert_eq!(relationships[0]["parent_session_id"], "parent");
        assert_eq!(relationships[0]["child_session_id"], "child");
        assert_eq!(relationships[0]["relationship"], "subagent");
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
    fn external_attached_session_cache_ignores_internal_sessions() {
        assert_eq!(
            external_attached_session_from_wire(
                &serde_json::json!({
                    "event": "session_attached",
                    "session_id": "internal",
                    "source": "intendant"
                })
                .to_string()
            ),
            None
        );
        assert_eq!(
            external_attached_session_from_wire(
                &serde_json::json!({
                    "event": "session_attached",
                    "session_id": "external",
                    "source": "codex"
                })
                .to_string()
            ),
            Some(("external".to_string(), "codex".to_string()))
        );
        assert_eq!(
            external_identity_session_from_wire(
                &serde_json::json!({
                    "event": "session_identity",
                    "session_id": "wrapper",
                    "source": "intendant",
                    "backend_session_id": "backend",
                })
                .to_string()
            ),
            None
        );
    }

    #[test]
    fn external_attached_session_cache_tracks_all_live_external_sessions() {
        let mut sessions = HashMap::new();
        for (session_id, source) in [("codex-a", "codex"), ("claude-b", "claude_code")] {
            update_external_attached_sessions_from_wire(
                &mut sessions,
                &serde_json::json!({
                    "event": "session_attached",
                    "session_id": session_id,
                    "source": source,
                })
                .to_string(),
            );
        }

        update_external_attached_sessions_from_wire(
            &mut sessions,
            &serde_json::json!({
                "event": "session_started",
                "session_id": "internal-wrapper",
            })
            .to_string(),
        );

        assert_eq!(sessions.get("codex-a").map(String::as_str), Some("codex"));
        assert_eq!(
            sessions.get("claude-b").map(String::as_str),
            Some("claude-code")
        );

        update_external_attached_sessions_from_wire(
            &mut sessions,
            &serde_json::json!({
                "event": "session_ended",
                "session_id": "codex-a",
            })
            .to_string(),
        );

        assert!(!sessions.contains_key("codex-a"));
        assert_eq!(
            sessions.get("claude-b").map(String::as_str),
            Some("claude-code")
        );
    }

    #[test]
    fn external_attached_session_cache_tracks_identity_backend_session() {
        let mut sessions = HashMap::new();
        update_external_attached_sessions_from_wire(
            &mut sessions,
            &serde_json::json!({
                "event": "session_identity",
                "session_id": "wrapper-session",
                "source": "codex",
                "backend_session_id": "019eab28-008c-7e21-af02-9da557405f6f",
            })
            .to_string(),
        );

        assert!(!sessions.contains_key("wrapper-session"));
        assert_eq!(
            sessions
                .get("019eab28-008c-7e21-af02-9da557405f6f")
                .map(String::as_str),
            Some("codex")
        );
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
    fn test_scan_replay_status_extracts_provider_model_autonomy() {
        let contents = concat!(
            r#"{"ts":"10:00:00","event":"session_start","level":"info"}"#,
            "\n",
            r#"{"ts":"10:00:01","event":"info","level":"info","message":"Provider: openai"}"#,
            "\n",
            r#"{"ts":"10:00:02","event":"info","level":"info","message":"Model: gpt-5"}"#,
            "\n",
            r#"{"ts":"10:00:03","event":"info","level":"info","message":"Autonomy: High"}"#,
            "\n",
        );
        let (p, m, a) = scan_replay_status(contents);
        assert_eq!(p.as_deref(), Some("openai"));
        assert_eq!(m.as_deref(), Some("gpt-5"));
        assert_eq!(a.as_deref(), Some("High"));
    }

    #[test]
    fn test_scan_replay_status_reads_debug_level_entries() {
        // Newer sessions write Provider/Model/Autonomy as `l.debug(...)`
        // so the event_type is "debug", not "info".  scan_replay_status
        // must pick those up too.
        let contents = concat!(
            r#"{"ts":"10:00:00","event":"debug","level":"debug","message":"Provider: anthropic"}"#,
            "\n",
            r#"{"ts":"10:00:01","event":"debug","level":"debug","message":"Model: claude-sonnet-4-6"}"#,
            "\n",
            r#"{"ts":"10:00:02","event":"debug","level":"debug","message":"Autonomy: Medium"}"#,
            "\n",
        );
        let (p, m, a) = scan_replay_status(contents);
        assert_eq!(p.as_deref(), Some("anthropic"));
        assert_eq!(m.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(a.as_deref(), Some("Medium"));
    }

    #[test]
    fn test_replay_jsonl_produces_replay_start_marker_first() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.info("Provider: openai");
        log.info("Model: gpt-5");
        log.info("Autonomy: Medium");
        log.turn_start(1, 0.0, 100_000);
        log.auto_approved("exec: ls");
        log.round_complete(1, 3);
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        // First entry is the replay_start marker.
        assert_eq!(
            entries[0].get("event").and_then(|v| v.as_str()),
            Some("replay_start")
        );
        assert_eq!(
            entries[0].get("provider").and_then(|v| v.as_str()),
            Some("openai")
        );

        // Each OutboundEvent entry has its historical `ts` injected.
        // Find the turn_started entry and verify it carries the original ts.
        let turn_started = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("turn_started"))
            .expect("turn_started should be present");
        assert!(
            turn_started.get("ts").is_some(),
            "ts should be injected into each outbound entry"
        );
        assert_eq!(
            turn_started.get("session_id").and_then(|v| v.as_str()),
            Some("session")
        );

        // auto_approved preview preserved.
        let auto_approved = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("auto_approved"))
            .expect("auto_approved should be present");
        assert_eq!(
            auto_approved.get("preview").and_then(|v| v.as_str()),
            Some("exec: ls")
        );
        assert_eq!(
            auto_approved.get("session_id").and_then(|v| v.as_str()),
            Some("session")
        );

        // round_complete fields propagated.
        let round = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("round_complete"))
            .expect("round_complete should be present");
        assert_eq!(round.get("round").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            round.get("turns_in_round").and_then(|v| v.as_u64()),
            Some(3)
        );
    }

    #[test]
    fn session_list_round_complete_is_idle_not_completed() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.round_complete(1, 3);
        drop(log);

        let row = intendant_session_list_row_from_dir(&log_dir, "session").unwrap();
        assert_eq!(row["status"].as_str(), Some("idle"));
    }

    #[test]
    fn session_list_preserves_user_interrupted_status_after_round_complete() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.info("External agent interrupted: user requested");
        log.round_complete(1, 2);
        drop(log);

        let row = intendant_session_list_row_from_dir(&log_dir, "session").unwrap();
        assert_eq!(row["status"].as_str(), Some("interrupted"));
    }

    #[test]
    fn session_list_new_turn_after_interrupt_returns_to_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.info("Interrupted: user requested");
        log.round_complete(1, 1);
        log.turn_start(2, 0.0, 100_000);
        drop(log);

        let row = intendant_session_list_row_from_dir(&log_dir, "session").unwrap();
        assert_eq!(row["status"].as_str(), Some("in_progress"));
    }

    #[test]
    fn test_external_wrapper_replay_uses_backend_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("wrapper-session");
        let backend_id = "019e598b-256e-7b61-8816-22908ece438a";
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.session_started("wrapper-session", Some("external task"));
        log.session_identity("wrapper-session", "codex", backend_id);
        log.info("Mode: external agent (Codex)");
        log.debug(&format!("External agent thread: {backend_id}"));
        log.info("[user] continue here");
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);
        let started_row = entries
            .iter()
            .find(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("session_started"))
            .expect("wrapper session_started should replay");
        let identity_row = entries
            .iter()
            .find(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("session_identity"))
            .expect("wrapper session_identity should replay");
        let user_row = entries
            .iter()
            .find(|entry| {
                entry.get("event").and_then(|v| v.as_str()) == Some("log_entry")
                    && entry.get("content").and_then(|v| v.as_str()) == Some("continue here")
            })
            .expect("wrapper log entry should replay");

        assert_eq!(
            started_row.get("session_id").and_then(|v| v.as_str()),
            Some(backend_id)
        );
        assert_eq!(
            identity_row.get("session_id").and_then(|v| v.as_str()),
            Some("wrapper-session")
        );
        assert_eq!(
            user_row.get("session_id").and_then(|v| v.as_str()),
            Some(backend_id)
        );
    }

    #[test]
    fn test_external_wrapper_replay_synthesizes_missing_identity() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("wrapper-session");
        let backend_id = "019e99d5-b9b0-7ff1-a8b4-bdf0a7aade61";
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.session_started("wrapper-session", Some("external task"));
        log.debug(&format!("External agent thread: {backend_id}"));
        log.debug(&format!(
            "Mode: external agent (Codex) via presence, thread: {backend_id}"
        ));
        log.info("[user] continue here");
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);
        let identity_row = entries
            .iter()
            .find(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("session_identity"))
            .expect("missing wrapper session_identity should be synthesized");
        let user_row = entries
            .iter()
            .find(|entry| {
                entry.get("event").and_then(|v| v.as_str()) == Some("log_entry")
                    && entry.get("content").and_then(|v| v.as_str()) == Some("continue here")
            })
            .expect("wrapper log entry should replay");

        assert_eq!(
            identity_row.get("session_id").and_then(|v| v.as_str()),
            Some("wrapper-session")
        );
        assert_eq!(
            identity_row.get("source").and_then(|v| v.as_str()),
            Some("codex")
        );
        assert_eq!(
            identity_row
                .get("backend_session_id")
                .and_then(|v| v.as_str()),
            Some(backend_id)
        );
        assert_eq!(
            user_row.get("session_id").and_then(|v| v.as_str()),
            Some(backend_id)
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

    #[test]
    fn test_session_log_replay_from_dir_reads_active_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.info("Provider: openai");
        log.model_response("still here after refresh", 0, 0, 0, 0, None);
        drop(log);

        let replay = session_log_replay_from_dir(&log_dir).expect("session log should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();

        assert_eq!(replay["t"], "log_replay");
        assert!(replay["entries"].as_array().unwrap().iter().any(|entry| {
            entry["event"] == "model_response" && entry["summary"] == "still here after refresh"
        }));
    }

    #[test]
    fn session_log_replay_infers_legacy_model_response_spans() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        std::fs::create_dir_all(log_dir.join("turns")).unwrap();
        std::fs::write(
            log_dir.join("turns/turn_000_model.txt"),
            "first response\nsecond response",
        )
        .unwrap();
        let first = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 0,
            "event": "model_response",
            "level": "info",
            "message": "first response",
            "file": "turns/turn_000_model.txt",
            "data": {
                "content_length": "first response".len(),
                "tokens": {"prompt": 1, "completion": 2, "total": 3, "cached": 0}
            },
        });
        let second = serde_json::json!({
            "ts": "01:00:01.000",
            "turn": 0,
            "event": "model_response",
            "level": "info",
            "message": "second response",
            "file": "turns/turn_000_model.txt",
            "data": {
                "content_length": "second response".len(),
                "tokens": {"prompt": 4, "completion": 5, "total": 6, "cached": 0}
            },
        });
        std::fs::write(
            log_dir.join("session.jsonl"),
            format!("{first}\n{second}\n"),
        )
        .unwrap();

        let replay = session_log_replay_from_dir(&log_dir).expect("session log should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let summaries: Vec<&str> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "model_response")
            .filter_map(|entry| entry["summary"].as_str())
            .collect();

        assert_eq!(summaries, vec!["first response", "second response"]);
    }

    #[test]
    fn session_log_replay_payload_compacts_context_snapshot_raw() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let large = "x".repeat(8_000);
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
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
                "instructions": large,
                "input": [{"role": "user", "content": "inspect dashboard lag"}]
            }),
        );
        log.context_snapshot(
            "codex",
            "Codex resolved request payload",
            Some(2),
            "openai.responses.resolved_request.v1",
            Some(1_100),
            Some("backend_reported"),
            Some(128_000),
            Some(272_000),
            Some(1),
            &serde_json::json!({
                "input": [{"role": "user", "content": "latest context survives as compact summary"}]
            }),
        );
        drop(log);

        let replay = session_log_replay_from_dir(&log_dir).expect("session log should replay");
        assert!(
            !replay.contains(&"x".repeat(1_000)),
            "replay should not include exact historical context payloads"
        );
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let contexts: Vec<&serde_json::Value> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "context_snapshot")
            .collect();
        assert_eq!(contexts.len(), 2);
        assert_eq!(
            contexts
                .iter()
                .filter(|entry| entry.pointer("/raw/_intendant_context/raw_omitted")
                    == Some(&serde_json::json!(true)))
                .count(),
            1
        );
        let context = contexts
            .iter()
            .find(|entry| {
                entry
                    .pointer("/raw/_intendant_context/raw_omitted")
                    .and_then(|v| v.as_bool())
                    != Some(true)
            })
            .expect("latest context snapshot should keep a compact summary");
        assert_eq!(
            context.pointer("/raw/_intendant_context/archive_mode"),
            Some(&serde_json::json!("summary"))
        );
        assert!(context
            .get("snapshot_file")
            .and_then(|v| v.as_str())
            .is_some_and(|file| file.contains("_context_")));
        assert_eq!(context["exact_replay_available"], true);
        assert!(context
            .pointer("/raw/summary_parts")
            .and_then(|v| v.as_array())
            .is_some_and(|parts| !parts.is_empty()));
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
        let (status, body) = get_session_context_snapshot_from_home(
            dir.path(),
            "lazy-session",
            "intendant",
            &request,
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
    fn test_replay_jsonl_skips_internal_events() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.messages_input(r#"[{"role":"user","content":"hi"}]"#); // -> skip
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#); // -> skip
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        // Entries are: [replay_start, turn_started].  messages_input,
        // agent_input, and session_start all return None.
        assert_eq!(entries.len(), 2, "unexpected entries: {:#?}", entries);
        assert_eq!(
            entries[0].get("event").and_then(|v| v.as_str()),
            Some("replay_start")
        );
        assert_eq!(
            entries[1].get("event").and_then(|v| v.as_str()),
            Some("turn_started")
        );
    }

    #[test]
    fn test_replay_jsonl_includes_context_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.context_snapshot(
            "native",
            "Internal agent messages",
            Some(1),
            "intendant.conversation.messages.v1",
            None,
            None,
            Some(200_000),
            Some(200_000),
            Some(1),
            &serde_json::json!([{"role": "user", "content": "hi"}]),
        );
        drop(log);

        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries = replay_jsonl_to_outbound_entries(&contents, &log_dir);

        let context = entries
            .iter()
            .find(|e| e.get("event").and_then(|v| v.as_str()) == Some("context_snapshot"))
            .expect("context_snapshot should replay");
        assert_eq!(
            context.get("format").and_then(|v| v.as_str()),
            Some("intendant.conversation.messages.v1")
        );
        assert_eq!(
            context.pointer("/raw/0/role").and_then(|v| v.as_str()),
            Some("user")
        );
    }

    #[test]
    fn external_activity_replay_includes_persisted_context_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "019e37b2-context-replay";
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
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
                        "content": [{ "type": "input_text", "text": "show context" }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let wrapper_log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-session");
        let mut log = crate::session_log::SessionLog::open(wrapper_log_dir).unwrap();
        log.context_snapshot_for_session(
            Some(session_id),
            "codex",
            "Codex resolved request payload",
            Some("req-context-1"),
            Some(1),
            Some(4),
            "openai.responses.resolved_request.v1",
            Some(1200),
            Some("backend_reported"),
            Some(128_000),
            Some(272_000),
            Some(1),
            &serde_json::json!({
                "_intendant_context": {
                    "thread_id": session_id,
                    "request_id": "req-context-1",
                    "request_index": 1
                },
                "input": [{"role": "user", "content": "show context"}]
            }),
        );
        drop(log);

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();
        let context = entries
            .iter()
            .find(|entry| entry["event"] == "context_snapshot")
            .expect("persisted context snapshot should replay with external transcript");
        assert_eq!(context["session_id"], session_id);
        assert_eq!(context["request_id"], "req-context-1");
        assert_eq!(context["request_index"], 1);
        assert_eq!(
            context.pointer("/raw/_intendant_context/archive_mode"),
            Some(&serde_json::json!("summary"))
        );
        assert!(context
            .pointer("/raw/summary_parts")
            .and_then(|v| v.as_array())
            .is_some_and(|parts| parts.iter().any(|part| part
                .get("preview")
                .and_then(|v| v.as_str())
                .is_some_and(|preview| preview.contains("show context")))));
    }

    #[test]
    fn external_activity_replay_omits_oversized_persisted_context_snapshot_raw() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "019e37b2-context-replay-large";
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
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
                        "content": [{ "type": "input_text", "text": "show large context" }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let oversized = "large-context-sentinel "
            .repeat((CONTEXT_REPLAY_RAW_SUMMARY_MAX_BYTES as usize / 23) + 4096);
        let wrapper_log_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-session");
        let mut log = crate::session_log::SessionLog::open(wrapper_log_dir).unwrap();
        log.context_snapshot_for_session(
            Some(session_id),
            "codex",
            "Codex resolved request payload",
            Some("req-context-large"),
            Some(1),
            Some(4),
            "openai.responses.resolved_request.v1",
            Some(1200),
            Some("backend_reported"),
            Some(128_000),
            Some(272_000),
            Some(1),
            &serde_json::json!({
                "_intendant_context": {
                    "thread_id": session_id,
                    "request_id": "req-context-large",
                    "request_index": 1
                },
                "input": [{"role": "user", "content": oversized}]
            }),
        );
        drop(log);

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        assert!(
            !replay.contains("large-context-sentinel large-context-sentinel"),
            "external attach replay should not inline oversized raw context"
        );
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let context = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "context_snapshot")
            .expect("persisted context snapshot should replay with external transcript");
        assert_eq!(context["session_id"], session_id);
        assert_eq!(context["request_id"], "req-context-large");
        assert_eq!(
            context.pointer("/raw/_intendant_context/raw_omitted"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(context["exact_replay_available"], true);
        assert!(context
            .get("snapshot_file")
            .and_then(|v| v.as_str())
            .is_some_and(|file| file.contains("_context_")));
    }

    #[test]
    fn external_activity_replay_synthesizes_context_snapshots_from_codex_trace_archive() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "019e7adb-raw-replay";
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-31T02-45-00-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-31T02:45:00Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-31T02:45:02Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "show raw trace context" }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let trace_dir = dir
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-session")
            .join("model-request-traces")
            .join(format!("trace-aa6e5cc0-{session_id}"));
        std::fs::create_dir_all(trace_dir.join("payloads")).unwrap();
        std::fs::write(
            trace_dir.join("payloads/request-1.json"),
            serde_json::json!({
                "model": "gpt-test",
                "input": [{"role": "user", "content": "from raw trace"}]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            trace_dir.join("trace.jsonl"),
            serde_json::json!({
                "schema_version": 1,
                "wall_time_unix_ms": 1780182481847_u64,
                "payload": {
                    "type": "inference_started",
                    "provider_name": "OpenAI",
                    "thread_id": session_id,
                    "inference_call_id": "call-1",
                    "request_payload": {
                        "kind": {"type": "inference_request"},
                        "path": "payloads/request-1.json"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let entries = replay["entries"].as_array().unwrap();
        let context = entries
            .iter()
            .find(|entry| entry["event"] == "context_snapshot")
            .expect("trace archive context snapshot should replay with external transcript");
        assert_eq!(context["session_id"], session_id);
        assert_eq!(context["source"], "codex");
        assert_eq!(context["label"], "Codex resolved request payload");
        assert_eq!(context["format"], "openai.responses.resolved_request.v1");
        assert_eq!(context["request_index"], 1);
        assert!(context["request_id"]
            .as_str()
            .is_some_and(|request_id| request_id.starts_with("codex-request-")));
        assert_eq!(
            context.pointer("/raw/_intendant_context/archive_mode"),
            Some(&serde_json::json!("summary"))
        );
        assert!(context
            .pointer("/raw/summary_parts")
            .and_then(|v| v.as_array())
            .is_some_and(|parts| parts.iter().any(|part| part
                .get("preview")
                .and_then(|v| v.as_str())
                .is_some_and(|preview| preview.contains("from raw trace")))));
    }

    fn persisted_test_key(extra: &str) -> SessionListCacheKey {
        SessionListCacheKey {
            namespace: "test-rows",
            path: "/tmp/example/session.jsonl".to_string(),
            len: 1234,
            mtime_nanos: 111_222_333_444_555_666_777,
            ctime_nanos: -42,
            dev: 7,
            ino: 99,
            extra: extra.to_string(),
        }
    }

    #[test]
    fn persisted_session_entry_round_trips_and_validates_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let key = persisted_test_key("v1");
        let row = serde_json::json!({"session_id": "s-1", "total_tokens": 42});

        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &key).is_none());
        store_persisted_session_entry_in(dir.path(), &key, &row);
        assert_eq!(
            load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &key),
            Some(row.clone())
        );

        // Any fingerprint drift (here: file length) must invalidate the entry.
        let mut stale = key.clone();
        stale.len += 1;
        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &stale).is_none());
        // A different `extra` is a different slot entirely.
        let other = persisted_test_key("v2");
        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &other).is_none());
    }

    #[test]
    fn persisted_session_entry_survives_128_bit_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let mut key = persisted_test_key("wide");
        key.mtime_nanos = u128::MAX;
        key.ctime_nanos = i128::MIN;
        let usage: Option<SessionUsage> = Some(SessionUsage {
            total_tokens: 10,
            prompt_tokens: 6,
            completion_tokens: 4,
            cache_creation_tokens: 0,
            cached_tokens: 2,
        });
        store_persisted_session_entry_in(dir.path(), &key, &usage);
        assert_eq!(
            load_persisted_session_entry_in::<Option<SessionUsage>>(dir.path(), &key),
            Some(usage)
        );
    }

    #[test]
    fn usage_view_strips_heavy_row_fields() {
        let body = serde_json::json!([{
            "session_id": "s-1",
            "source": "codex",
            "task": "a very long task description",
            "cwd": "/somewhere/deep",
            "goal": {"objective": "x"},
            "turns": 3,
            "total_tokens": 100,
            "estimated_cost": 1.25,
            "daily_usage": [{"day": "2026-07-01", "total_tokens": 100}],
            "total_bytes": 2048,
        }])
        .to_string();
        let slim = session_list_body_usage_view(&body);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&slim).unwrap();
        let row = rows[0].as_object().unwrap();
        assert!(row.contains_key("session_id"));
        assert!(row.contains_key("daily_usage"));
        assert!(row.contains_key("total_bytes"));
        assert!(!row.contains_key("task"));
        assert!(!row.contains_key("cwd"));
        assert!(!row.contains_key("goal"));
    }

    #[test]
    fn usage_view_request_detection() {
        assert!(session_list_usage_view_from_request(
            "GET /api/sessions?limit=all&view=usage HTTP/1.1"
        ));
        assert!(!session_list_usage_view_from_request(
            "GET /api/sessions?limit=all HTTP/1.1"
        ));
        assert!(!session_list_usage_view_from_request(
            "GET /api/sessions?view=usagex HTTP/1.1"
        ));
    }

    #[test]
    fn persisted_entry_rejects_corrupt_body() {
        let dir = tempfile::tempdir().unwrap();
        let key = persisted_test_key("corrupt");
        let path =
            session_index_entry_path_in(dir.path(), key.namespace, &session_list_cache_slot(&key));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &key).is_none());
    }

    fn total_usage(total_tokens: u64) -> SessionUsage {
        SessionUsage {
            total_tokens,
            ..Default::default()
        }
    }

    /// Pre-schema "codex" entries carried the full usage_events history and
    /// no schema stamp; they must read as misses (a defaulted
    /// first_usage_event would mis-baseline forked sessions), while
    /// current-schema entries round-trip.
    #[test]
    fn persisted_codex_entry_schema_mismatch_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut key = persisted_test_key("schema");
        key.namespace = "codex";

        let legacy = serde_json::json!({
            "key": {
                "namespace": key.namespace,
                "path": key.path,
                "len": key.len,
                "mtime_nanos": key.mtime_nanos.to_string(),
                "ctime_nanos": key.ctime_nanos.to_string(),
                "dev": key.dev,
                "ino": key.ino,
                "extra": key.extra,
            },
            "value": {
                "id": "codex-1",
                "created_at": null,
                "session_cwd": null,
                "effective_cwd": null,
                "model": null,
                "lineage": {},
                "provider": "Codex",
                "usage": total_usage(10),
                "usage_events": [{"timestamp": null, "usage": total_usage(10)}],
                "daily_usage": {},
                "goal": null,
                "task": null,
                "turns": 1,
                "file_updated_at": null,
                "bytes": 5,
            },
        });
        let path =
            session_index_entry_path_in(dir.path(), key.namespace, &session_list_cache_slot(&key));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(
            load_persisted_session_entry_in::<CodexSessionListSummary>(dir.path(), &key).is_none()
        );

        // A freshly stored entry (current schema) round-trips.
        let summary = CodexSessionListSummary {
            id: "codex-1".to_string(),
            created_at: None,
            session_cwd: None,
            effective_cwd: None,
            model: None,
            lineage: SessionLineageMetadata::default(),
            provider: Some("Codex".to_string()),
            usage: total_usage(10),
            first_usage_event: Some(CodexUsageEvent {
                timestamp: None,
                usage: total_usage(10),
            }),
            daily_usage: BTreeMap::new(),
            goal: None,
            task: None,
            turns: 1,
            file_updated_at: None,
            bytes: 5,
        };
        store_persisted_session_entry_in(dir.path(), &key, &summary);
        let loaded = load_persisted_session_entry_in::<CodexSessionListSummary>(dir.path(), &key)
            .expect("current-schema entry loads");
        assert_eq!(loaded.id, "codex-1");
        assert_eq!(
            loaded.first_usage_event.map(|event| event.usage),
            Some(total_usage(10))
        );
    }

    #[test]
    fn codex_accumulator_tracks_first_event_and_undated_usage() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("rollout.jsonl");
        std::fs::write(&log, b"{}\n").unwrap();

        let mut acc = CodexSessionListAccumulator::new();
        acc.id = Some("codex-acc".to_string());
        acc.record_token_usage(Some("2026-07-01T09:00:00Z".to_string()), total_usage(100));
        acc.record_token_usage(None, total_usage(130));
        acc.record_token_usage(Some("2026-07-02T09:00:00Z".to_string()), total_usage(150));

        let summary = acc.finish(&log).expect("summary");
        assert_eq!(summary.usage, total_usage(150));
        let first = summary.first_usage_event.as_ref().expect("first event");
        assert_eq!(first.usage, total_usage(100));
        assert_eq!(first.timestamp.as_deref(), Some("2026-07-01T09:00:00Z"));
        // Dated deltas land on their own days; the undated delta folds into
        // the file-mtime day.
        assert_eq!(
            summary.daily_usage.get("2026-07-01"),
            Some(&total_usage(100))
        );
        let mtime_day =
            usage_day_from_timestamp(summary.file_updated_at.as_deref()).expect("file mtime day");
        let daily_total: u64 = summary
            .daily_usage
            .values()
            .map(|usage| usage.total_tokens)
            .sum();
        assert_eq!(daily_total, 150);
        assert!(summary.daily_usage.contains_key(&mtime_day));

        // A counter reset discards prior history, including the first event.
        let mut reset = CodexSessionListAccumulator::new();
        reset.id = Some("codex-reset".to_string());
        reset.record_token_usage(Some("2026-07-01T09:00:00Z".to_string()), total_usage(100));
        reset.clear_token_usage();
        reset.record_token_usage(Some("2026-07-03T09:00:00Z".to_string()), total_usage(40));
        let summary = reset.finish(&log).expect("summary");
        let first = summary.first_usage_event.as_ref().expect("first event");
        assert_eq!(first.timestamp.as_deref(), Some("2026-07-03T09:00:00Z"));
        assert_eq!(first.usage, total_usage(40));
        assert_eq!(
            summary.daily_usage.get("2026-07-03"),
            Some(&total_usage(40))
        );
        assert!(!summary.daily_usage.contains_key("2026-07-01"));
    }

    #[test]
    fn codex_daily_usage_with_baseline_rebaselines_first_day() {
        let mut daily = BTreeMap::new();
        daily.insert("2026-07-01".to_string(), total_usage(100));
        daily.insert("2026-07-02".to_string(), total_usage(20));
        let summary = CodexSessionListSummary {
            id: "codex-fork".to_string(),
            created_at: Some("2026-07-01T10:00:00Z".to_string()),
            session_cwd: None,
            effective_cwd: None,
            model: None,
            lineage: SessionLineageMetadata::default(),
            provider: Some("Codex".to_string()),
            usage: total_usage(120),
            first_usage_event: Some(CodexUsageEvent {
                timestamp: Some("2026-07-01T10:05:00Z".to_string()),
                usage: total_usage(100),
            }),
            daily_usage: daily,
            goal: None,
            task: None,
            turns: 2,
            file_updated_at: Some("2026-07-02T09:30:00Z".to_string()),
            bytes: 64,
        };

        // The fork baseline comes out of the first event's day only.
        let rebased = codex_daily_usage_with_baseline(&summary, Some(total_usage(40)));
        assert_eq!(rebased.get("2026-07-01"), Some(&total_usage(60)));
        assert_eq!(rebased.get("2026-07-02"), Some(&total_usage(20)));

        // A baseline covering the whole first day removes that bucket.
        let rebased = codex_daily_usage_with_baseline(&summary, Some(total_usage(100)));
        assert!(!rebased.contains_key("2026-07-01"));
        assert_eq!(rebased.get("2026-07-02"), Some(&total_usage(20)));

        // No baseline → parse-time buckets pass through untouched.
        let rebased = codex_daily_usage_with_baseline(&summary, None);
        assert_eq!(rebased.get("2026-07-01"), Some(&total_usage(100)));
    }

    #[test]
    fn intendant_fingerprint_digest_tracks_dir_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("session.jsonl"), b"{\"a\":1}\n").unwrap();

        let first = intendant_session_dir_fingerprint(dir.path()).expect("fingerprint");
        assert_eq!(first.digest.len(), 64);
        let second = intendant_session_dir_fingerprint(dir.path()).expect("fingerprint");
        assert_eq!(first, second);

        // Content growth (length change) must change the digest.
        std::fs::write(dir.path().join("session.jsonl"), b"{\"a\":1,\"b\":2}\n").unwrap();
        let third = intendant_session_dir_fingerprint(dir.path()).expect("fingerprint");
        assert_eq!(first.path, third.path);
        assert_ne!(first.digest, third.digest);
    }

    #[test]
    fn preload_prunes_entries_for_deleted_targets() {
        let base = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let live_target = target_dir.path().join("live.jsonl");
        std::fs::write(&live_target, b"{}\n").unwrap();

        let entry_for = |path: &Path, extra: &str| -> (PathBuf, Vec<u8>) {
            let key = SessionListCacheKey {
                namespace: "claude-code",
                path: path.to_string_lossy().to_string(),
                len: 2,
                mtime_nanos: 1,
                ctime_nanos: 1,
                dev: 1,
                ino: 1,
                extra: extra.to_string(),
            };
            let entry = PersistedSessionCacheEntry {
                schema: persisted_namespace_schema(key.namespace),
                key: PersistedSessionCacheKey::of(&key),
                value: serde_json::json!({"session_id": extra}),
            };
            let file = session_index_entry_path_in(
                base.path(),
                key.namespace,
                &session_list_cache_slot(&key),
            );
            (file, serde_json::to_vec(&entry).unwrap())
        };

        let (live_file, live_bytes) = entry_for(&live_target, "live");
        let missing_target = target_dir.path().join("deleted.jsonl");
        let (dead_file, dead_bytes) = entry_for(&missing_target, "dead");
        std::fs::create_dir_all(live_file.parent().unwrap()).unwrap();
        std::fs::write(&live_file, &live_bytes).unwrap();
        std::fs::write(&dead_file, &dead_bytes).unwrap();

        preload_namespace_dir(
            &base.path().join("claude-code"),
            "claude-code",
            preload_row_entry,
        );

        assert!(live_file.exists(), "entry for a live session is kept");
        assert!(!dead_file.exists(), "entry for a deleted session is pruned");

        // Outcome-level checks: schema drift is skipped (kept on disk for
        // whichever daemon owns it), a missing target reports prunable.
        assert_eq!(
            preload_row_entry("claude-code", &dead_bytes),
            PreloadOutcome::TargetMissing
        );
        let mut wrong_schema: serde_json::Value = serde_json::from_slice(&live_bytes).unwrap();
        wrong_schema["schema"] = serde_json::json!(99);
        assert_eq!(
            preload_row_entry("claude-code", &serde_json::to_vec(&wrong_schema).unwrap()),
            PreloadOutcome::Skipped
        );

        // Legacy-shape entries no build of this daemon can parse are still
        // prunable through the path probe once their session is gone, and
        // kept while it is alive.
        let legacy = |target: &Path| {
            serde_json::to_vec(&serde_json::json!({
                "fingerprint": {"path": target.to_string_lossy(), "entries": []},
                "row": {"session_id": "legacy"},
            }))
            .unwrap()
        };
        assert_eq!(
            preload_intendant_entry("intendant-row", &legacy(&missing_target)),
            PreloadOutcome::TargetMissing
        );
        assert_eq!(
            preload_intendant_entry("intendant-row", &legacy(&live_target)),
            PreloadOutcome::Skipped
        );
    }
}
