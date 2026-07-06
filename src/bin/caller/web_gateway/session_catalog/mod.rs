//! The non-HTTP session catalog: list/index caches and their
//! fingerprints, external (codex/claude/gemini) session-file parsing,
//! transcripts and activity replay assembly, context-snapshot replay,
//! session search, worktree observed-session hints, usage accounting,
//! and the sort/merge/stream core behind the sessions API.

use super::*;

mod replay;
pub(crate) use replay::*;
mod external_rows;
pub(crate) use external_rows::*;
mod detail_search;
pub(crate) use detail_search::*;
mod codex_values;
pub(crate) use codex_values::*;
mod caches;
pub(crate) use caches::*;
mod rows_usage;
pub(crate) use rows_usage::*;
mod backend_lists;
pub(crate) use backend_lists::*;

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
}
