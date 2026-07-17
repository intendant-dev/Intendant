//! The non-HTTP session catalog: list/index caches and their
//! fingerprints, external (codex/claude/gemini) session-file parsing,
//! transcripts and activity replay assembly, context-snapshot replay,
//! session search, worktree observed-session hints, usage accounting,
//! and the sort/merge/stream core behind the sessions API.

use super::*;

mod replay;
pub(crate) use replay::*;
mod replay_cache;
pub(crate) use replay_cache::*;
mod external_rows;
pub(crate) use external_rows::*;
mod detail_search;
pub(crate) use detail_search::*;
mod locate;
pub(crate) use locate::*;
mod codex_values;
pub(crate) use codex_values::*;
mod caches;
pub(crate) use caches::*;
mod rows_usage;
pub(crate) use rows_usage::*;
pub(crate) mod backend_lists;
pub(crate) use backend_lists::*;
mod transcripts;
pub(crate) use transcripts::*;

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

/// Ceiling for explicit numeric `limit=` requests, and the cache-slot key the
/// unlimited list is stored under. The unlimited (`limit=all`) list itself is
/// NOT capped by this: Stats and the Sessions header fold it for lifetime
/// usage totals, so it must serve every row.
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
    /// Shared: a cache hit hands out the Arc instead of deep-cloning the
    /// whole parsed transcript per bootstrap/detail request.
    entries: std::sync::Arc<Vec<serde_json::Value>>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionListResponseCacheEntry {
    generated_at: std::time::Instant,
    /// Monotonic id minted per stored body. The projection cache
    /// (limit-slice / usage-view bodies) keys on it, so projections are
    /// valid exactly as long as the body they were derived from.
    generation: u64,
    /// Shared: a cache hit hands out the Arc — and the HTTP write
    /// boundary writes straight from it — instead of cloning the
    /// multi-hundred-KB serialized list per request (same reason the
    /// external-transcript cache above shares its parsed entries).
    body: Arc<str>,
}

pub(crate) fn next_session_list_body_generation() -> u64 {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    GENERATION.fetch_add(1, Ordering::Relaxed) + 1
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
    /// Conversation preview entries (same shape as the row `preview`
    /// field); absent on entries persisted before the field existed.
    #[serde(default)]
    preview: Option<serde_json::Value>,
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

pub(crate) fn list_sessions() -> String {
    list_sessions_from_home(&crate::platform::home_dir())
}

pub(crate) fn cached_limited_session_list_cache(
) -> &'static Mutex<HashMap<usize, SessionListResponseCacheEntry>> {
    SESSION_LIST_LIMITED_RESPONSE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Serve-stale-while-revalidate ceiling for the session-list response
/// caches. Native-session membership changes no longer ride this bound at
/// all: the bus-driven invalidator (`spawn_session_list_cache_invalidator`
/// in `startup/wiring.rs`) drops both cache tiers on session lifecycle
/// events, so this window only limits staleness for changes the bus can't
/// see — chiefly EXTERNAL backend session dirs (codex / claude) written by
/// other processes. Three minutes keeps those reasonably fresh; the hard
/// TTL (`SESSION_LIST_RESPONSE_CACHE_TTL_SECS`) stays at 30s as the storm
/// shield for the 2026-07-05 relationship-hydration incident.
pub(crate) const SESSION_LIST_RESPONSE_STALE_MAX_SECS: u64 = 3 * 60;

pub(crate) fn session_list_refresh_inflight() -> &'static Mutex<HashSet<usize>> {
    static INFLIGHT: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Global ceiling on concurrent background session-list refreshes. The
/// per-slot single-flight below bounds each limit slot to one refresh,
/// but a stampede of stale hits across DISTINCT slots could still spawn
/// one native rebuild thread per slot. Past this many in-flight
/// refreshes a stale hit keeps serving stale without spawning; the
/// refresh happens when a later stale hit finds capacity free (or the
/// hard TTL forces an inline rebuild).
pub(crate) const SESSION_LIST_REFRESH_MAX_CONCURRENT: usize = 4;

/// Claim the single-flight slot for a background refresh of
/// `limit_slot`. False — the caller must not spawn — when that slot
/// already has a refresh in flight or the global ceiling is reached. A
/// successful claim must be released with
/// [`end_session_list_refresh`].
pub(crate) fn try_begin_session_list_refresh(limit_slot: usize) -> bool {
    let mut inflight = session_list_refresh_inflight()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if inflight.len() >= SESSION_LIST_REFRESH_MAX_CONCURRENT {
        return false;
    }
    inflight.insert(limit_slot)
}

/// Release a claim made by [`try_begin_session_list_refresh`].
pub(crate) fn end_session_list_refresh(limit_slot: usize) {
    session_list_refresh_inflight()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&limit_slot);
}

/// Slot key for the single-flight guard: real limits are 1..SESSION_LIST_LIMIT,
/// the unlimited list uses SESSION_LIST_LIMIT itself.
pub(crate) fn spawn_session_list_refresh(limit_slot: usize) {
    if !try_begin_session_list_refresh(limit_slot) {
        return;
    }
    std::thread::spawn(move || {
        let body = if limit_slot >= SESSION_LIST_LIMIT {
            list_sessions()
        } else {
            list_sessions_from_home_with_limit(&crate::platform::home_dir(), Some(limit_slot))
        };
        store_session_list_response(limit_slot, body);
        end_session_list_refresh(limit_slot);
    });
}

/// Store a rebuilt list body and return the generation minted for it —
/// atomically paired with the body, so a caller that goes on to serve
/// `body` keys its projections on the generation of THAT body (the
/// historical store-then-re-read could pair a concurrent rebuild's
/// generation with this caller's body).
pub(crate) fn store_session_list_response(limit_slot: usize, body: impl Into<Arc<str>>) -> u64 {
    let generation = next_session_list_body_generation();
    let entry = SessionListResponseCacheEntry {
        generated_at: std::time::Instant::now(),
        generation,
        body: body.into(),
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
    generation
}

/// fresh -> serve; stale-but-usable -> serve + background refresh;
/// too stale/absent -> rebuild inline.
pub(crate) fn serve_session_list_cache_entry(
    limit_slot: usize,
    entry: Option<&SessionListResponseCacheEntry>,
) -> Option<Arc<str>> {
    serve_session_list_cache_entry_with_generation(limit_slot, entry).map(|(body, _)| body)
}

/// [`serve_session_list_cache_entry`] plus the served body's generation,
/// for callers that key derived projections on it. Hits hand out
/// refcount clones of the stored body, never text copies.
pub(crate) fn serve_session_list_cache_entry_with_generation(
    limit_slot: usize,
    entry: Option<&SessionListResponseCacheEntry>,
) -> Option<(Arc<str>, u64)> {
    let entry = entry?;
    let age = entry.generated_at.elapsed();
    if age <= std::time::Duration::from_secs(SESSION_LIST_RESPONSE_CACHE_TTL_SECS) {
        return Some((Arc::clone(&entry.body), entry.generation));
    }
    if age <= std::time::Duration::from_secs(SESSION_LIST_RESPONSE_STALE_MAX_SECS) {
        let body = Arc::clone(&entry.body);
        let generation = entry.generation;
        spawn_session_list_refresh(limit_slot);
        return Some((body, generation));
    }
    None
}

/// Drop every cached session-list response. Called on session lifecycle
/// events (create/end/attach/identity/relationship): the daemon KNOWS the
/// list just changed, and serving out the 30s hard-TTL / 15-minute SWR
/// window after a membership change hands ghosts to the dashboard's own
/// post-create refresh and to every parameterless caller (MCP, ctl,
/// peers). Status-only drift still rides the TTLs — that freshness
/// trade-off is the cache's reason to exist on 50GB log trees.
pub(crate) fn invalidate_session_list_response_caches() {
    if let Some(cache) = SESSION_LIST_RESPONSE_CACHE.get() {
        *cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    cached_limited_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    // Projections derive from those bodies — invalidated together. (Their
    // generation keys could never match a future body, but dropping them
    // now frees the memory with the bodies.)
    session_list_projection_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

pub(crate) fn cached_list_sessions() -> Arc<str> {
    cached_list_sessions_with_generation().0
}

/// [`cached_list_sessions`] plus the served body's generation for
/// projection-cache keying (`None` only for exotic paths that bypass the
/// entry store).
pub(crate) fn cached_list_sessions_with_generation() -> (Arc<str>, Option<u64>) {
    {
        let cache = SESSION_LIST_RESPONSE_CACHE.get_or_init(|| Mutex::new(None));
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((body, generation)) =
            serve_session_list_cache_entry_with_generation(SESSION_LIST_LIMIT, guard.as_ref())
        {
            return (body, Some(generation));
        }
    }

    let body: Arc<str> = list_sessions().into();
    let generation = store_session_list_response(SESSION_LIST_LIMIT, Arc::clone(&body));
    (body, Some(generation))
}

pub(crate) fn cached_list_sessions_with_limit(limit: usize) -> Arc<str> {
    cached_list_sessions_with_limit_and_generation(limit).0
}

pub(crate) fn cached_list_sessions_with_limit_and_generation(
    limit: usize,
) -> (Arc<str>, Option<u64>) {
    let limit = limit.clamp(1, SESSION_LIST_LIMIT);
    if limit >= SESSION_LIST_LIMIT {
        return cached_list_sessions_with_generation();
    }

    let cache = cached_limited_session_list_cache();
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((body, generation)) =
            serve_session_list_cache_entry_with_generation(limit, guard.get(&limit))
        {
            return (body, Some(generation));
        }
    }

    let body: Arc<str> =
        list_sessions_from_home_with_limit(&crate::platform::home_dir(), Some(limit)).into();
    let generation = store_session_list_response(limit, Arc::clone(&body));
    (body, Some(generation))
}

pub(crate) fn cached_session_list_snapshot() -> Option<Arc<str>> {
    let cache = SESSION_LIST_RESPONSE_CACHE.get()?;
    let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|entry| Arc::clone(&entry.body))
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
        let Some(entries) = external_session_entries_from_home_arc(home, "codex", &id) else {
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

/// Whether a serialized session row answers to `id` on any identity field
/// (mirrors the SPA's `sessionRowMatchesId`).
pub(crate) fn session_row_answers_to_id(row: &serde_json::Value, id: &str) -> bool {
    [
        "session_id",
        "resume_id",
        "backend_session_id",
        "intendant_session_id",
    ]
    .iter()
    .any(|field| row.get(*field).and_then(|v| v.as_str()) == Some(id))
}

/// The cached full-list body when the cache can serve one under its normal
/// fresh/stale policy. No inline rebuild — callers fall back to their own
/// path on a cold cache.
fn cached_session_list_body_if_serveable() -> Option<Arc<str>> {
    let cache = SESSION_LIST_RESPONSE_CACHE.get_or_init(|| Mutex::new(None));
    let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    serve_session_list_cache_entry(SESSION_LIST_LIMIT, guard.as_ref())
}

pub(crate) fn cached_list_sessions_for_ids(home: &Path, ids: &[String]) -> String {
    if ids.is_empty() {
        return "[]".to_string();
    }
    // Storm shield (2026-07-05 incident): relationship hydration fires a
    // request per unknown id pair, and the targeted path below re-scans
    // all three session stores per call — behind a Connection:-close mTLS
    // handshake each. When the full-list cache can serve, answer from it;
    // only requests naming an id the cached list doesn't know fall through
    // to the fresh targeted scan (a genuinely new session deserves one).
    if let Some(body) = cached_session_list_body_if_serveable() {
        if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
            let hits: Vec<serde_json::Value> = rows
                .into_iter()
                .filter(|row| ids.iter().any(|id| session_row_answers_to_id(row, id)))
                .collect();
            let all_found = ids
                .iter()
                .all(|id| hits.iter().any(|row| session_row_answers_to_id(row, id)));
            if all_found {
                return serde_json::to_string(&hits).unwrap_or_else(|_| "[]".to_string());
            }
        }
    }
    cached_list_sessions_for_ids_from_home(home, ids)
}

/// Strip session rows down to what the Stats tab folds: usage, costs,
/// per-day buckets, disk sizes, and the model name (the ui-v2 Usage
/// screen aggregates cost by model client-side). Full rows carry tasks,
/// paths, goals, and lineage that make a whole-corpus fetch megabytes;
/// the usage view is the same cached data at ~a tenth of the payload.
pub(crate) fn session_list_body_usage_view(body: &str) -> String {
    const KEEP: [&str; 20] = [
        "id",
        "session_id",
        "source",
        "model",
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

/// Projection-cache key: (source body generation, limit, usage_view).
type SessionListProjectionKey = (u64, Option<usize>, bool);

/// Cache of derived session-list projections (limit slices, the usage
/// view), keyed by the source body's generation + the projection params.
/// A projection is valid exactly while its source generation is being
/// served, so entries from replaced bodies simply stop matching; the
/// size cap ages them out. Shared bodies for the same reason as the
/// response cache: a hit hands out the Arc, not a text copy.
fn session_list_projection_cache() -> &'static Mutex<HashMap<SessionListProjectionKey, Arc<str>>> {
    static CACHE: OnceLock<Mutex<HashMap<SessionListProjectionKey, Arc<str>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const SESSION_LIST_PROJECTION_CACHE_LIMIT: usize = 16;

#[cfg(test)]
pub(crate) fn session_list_projection_cache_len() -> usize {
    session_list_projection_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

/// The `/api/sessions` response projection (limit truncation, then the
/// usage view), computed once per source-body generation instead of
/// re-parsing + re-serializing the multi-megabyte cached list on every
/// request. `generation: None` (uncachable source) computes directly.
/// The identity projection (no limit, no usage view) and every cache
/// hit are refcount clones — the hot no-projection serve path never
/// copies the body text.
pub(crate) fn projected_session_list_body(
    generation: Option<u64>,
    body: Arc<str>,
    limit: Option<usize>,
    usage_view: bool,
) -> Arc<str> {
    if limit.is_none() && !usage_view {
        return body;
    }
    let key = generation.map(|generation| (generation, limit, usage_view));
    if let Some(key) = &key {
        let cache = session_list_projection_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(projected) = cache.get(key) {
            return Arc::clone(projected);
        }
    }
    let projected = limit_session_list_body(&body, limit);
    let projected = if usage_view {
        session_list_body_usage_view(&projected)
    } else {
        projected
    };
    let projected: Arc<str> = projected.into();
    if let Some(key) = key {
        let mut cache = session_list_projection_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.len() >= SESSION_LIST_PROJECTION_CACHE_LIMIT && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&projected));
    }
    projected
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
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
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
        let mut keyed = collect_recent_files_keyed(
            &codex.join("sessions"),
            ".jsonl",
            WORKTREE_OBSERVED_SESSION_FILE_LIMIT,
        );
        keyed.extend(collect_recent_files_keyed(
            &codex.join("archived_sessions"),
            ".jsonl",
            WORKTREE_OBSERVED_SESSION_FILE_LIMIT,
        ));
        // Keys carried out of the walks: no re-stat per comparison.
        keyed.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
        keyed.truncate(WORKTREE_OBSERVED_SESSION_FILE_LIMIT);
        files = keyed.into_iter().map(|(_, path)| path).collect();
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
    // Decorate once: `sort_by_key(file_mtime_secs)` re-stats per
    // comparison.
    let mut keyed: Vec<(u64, PathBuf)> = files
        .into_iter()
        .map(|path| (file_mtime_secs(&path), path))
        .collect();
    keyed.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    keyed.truncate(limit);
    keyed.into_iter().map(|(_, path)| path).collect()
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

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
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
    push_session_file_fingerprint_with_metadata(entries, base, path, &metadata, is_dir);
}

/// [`push_session_file_fingerprint`] for callers already holding the
/// metadata (dir-entry walks) — the fingerprint sweep covers every
/// session dir per rebuild, and busy dirs hold thousands of turn/seg
/// files, so the redundant second stat per file was most of its cost.
pub(crate) fn push_session_file_fingerprint_with_metadata(
    entries: &mut Vec<SessionFileFingerprint>,
    base: &Path,
    path: &Path,
    metadata: &std::fs::Metadata,
    is_dir: bool,
) {
    if metadata.is_dir() != is_dir {
        return;
    }
    let rel = path
        .strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let (dev, ino) = crate::platform::metadata_dev_ino(metadata);
    entries.push(SessionFileFingerprint {
        rel,
        len: metadata.len(),
        mtime_nanos: metadata_mtime_nanos(metadata),
        ctime_nanos: metadata_ctime_nanos(metadata),
        dev,
        ino,
        is_dir,
    });
}

/// Dir-entry metadata with the platform-standard follow semantics:
/// symlinks resolve through `fs::metadata`, everything else reuses the
/// readdir-adjacent stat.
fn dir_entry_metadata_following(entry: &std::fs::DirEntry) -> std::io::Result<std::fs::Metadata> {
    match entry.file_type() {
        Ok(file_type) if file_type.is_symlink() => std::fs::metadata(entry.path()),
        _ => entry.metadata(),
    }
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
        // No is_file() pre-check: the push validates kind from the one
        // stat it takes anyway (missing or dir-shaped entries no-op).
        push_session_file_fingerprint(&mut entries, dir, &dir.join(name), false);
    }

    let recordings_dir = dir.join("recordings");
    if let Ok(rd) = std::fs::read_dir(&recordings_dir) {
        for re in rd.flatten() {
            let recording_dir = re.path();
            let Ok(dir_meta) = dir_entry_metadata_following(&re) else {
                continue;
            };
            if !dir_meta.is_dir() {
                continue;
            }
            push_session_file_fingerprint_with_metadata(
                &mut entries,
                dir,
                &recording_dir,
                &dir_meta,
                true,
            );
            if let Ok(files) = std::fs::read_dir(&recording_dir) {
                for file in files.flatten() {
                    let name = file.file_name().to_string_lossy().to_string();
                    if !name.starts_with("seg_") {
                        continue;
                    }
                    let Ok(metadata) = dir_entry_metadata_following(&file) else {
                        continue;
                    };
                    if metadata.is_file() {
                        push_session_file_fingerprint_with_metadata(
                            &mut entries,
                            dir,
                            &file.path(),
                            &metadata,
                            false,
                        );
                    }
                }
            }
        }
    }

    let frames_dir = dir.join("frames");
    if let Ok(fd) = std::fs::read_dir(&frames_dir) {
        for fe in fd.flatten() {
            let Ok(metadata) = dir_entry_metadata_following(&fe) else {
                continue;
            };
            if metadata.is_file() {
                push_session_file_fingerprint_with_metadata(
                    &mut entries,
                    dir,
                    &fe.path(),
                    &metadata,
                    false,
                );
            }
        }
    }

    let turns_dir = dir.join("turns");
    if let Ok(td) = std::fs::read_dir(&turns_dir) {
        for te in td.flatten() {
            let Ok(metadata) = dir_entry_metadata_following(&te) else {
                continue;
            };
            if metadata.is_file() {
                push_session_file_fingerprint_with_metadata(
                    &mut entries,
                    dir,
                    &te.path(),
                    &metadata,
                    false,
                );
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
    // Format v4: native rows carry cumulative cache-write usage and price it
    // separately. Bumping the layout invalidates persisted v3 rows once, so
    // their old zero-write cost does not linger until the session dir changes.
    // (v3 added conversation `preview`; v2 moved `updated_at` to transcript
    // activity.)
    ctx.update(&[4u8]);
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

/// Status for a session whose `summary.json` exists: the session ended,
/// but not necessarily well. A backend that died ("external agent event
/// channel closed", spawn failures, `error: …`) must not present as
/// `completed` — that flattening hid real backend deaths from operators.
/// Row statuses that mean a session has ENDED — the catalog's terminal
/// vocabulary (everything else a row can read — `running`, `in_progress`,
/// `idle`, `resident`, `external` — describes a session that may still be
/// alive). The bare-dir `"abandoned"` classification applied in
/// `intendant_session_list_row_from_dir` is terminal for consumers too.
/// The dashboard's Claude sign-in reload panel mirrors this set
/// (`CLAUDE_SIGNIN_TERMINAL_STATUSES` in `static/app/32-vault-custody.js`)
/// — pinned by the parity test below, so a vocabulary change here fails
/// the suite instead of shipping as drift.
pub(crate) const SESSION_ENDED_STATUSES: [&str; 3] = ["completed", "failed", "interrupted"];

fn summary_json_status(dir: &Path) -> Option<&'static str> {
    let raw = std::fs::read_to_string(dir.join("summary.json")).ok()?;
    let summary: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let outcome = summary
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("completed");
    let normalized = outcome.trim().to_ascii_lowercase();
    Some(
        if normalized.is_empty() || normalized == "completed" || normalized == "done" {
            "completed"
        } else if normalized.contains("stopped by user") || normalized.contains("interrupt") {
            "interrupted"
        } else {
            "failed"
        },
    )
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
    let mut cache_creation_tokens: u64 = 0;
    let mut daily_usage: BTreeMap<String, SessionUsage> = BTreeMap::new();
    let mut role: Option<String> = None;
    let mut external_resume_id: Option<String> = None;
    let mut external_source: Option<String> = None;
    let mut canonical_session_id: Option<String> = None;
    let mut capabilities: Option<serde_json::Value> = None;
    let mut worktree = serde_json::Value::Null;
    let mut session_agent_config = crate::session_config::read_log_dir_config(dir);
    let mut updated_at_secs = session_activity_mtime_secs(dir);

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
            if let Some(value) = meta.get("worktree").filter(|v| v.is_object()) {
                worktree = value.clone();
            }
        }
    }

    // Conversation preview: the meta task is the first user message; the
    // event loop below adds steer follow-ups and assistant text.
    let mut preview = SessionPreviewBuilder::default();
    if let Some(task_text) = task.as_deref() {
        preview.push_user(task_text);
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

            // A row describes ONE session, but not every event in a dir's
            // log is that session's: the daemon session's log carries the
            // bus tee's copies of every session's lifecycle events
            // (capabilities, vitals, task_complete, …). Honor an event that
            // names a session only when it names this dir's session — the
            // dir id, its meta-canonical id, or the backend id this dir's
            // own identity events established (post-collapse events are
            // keyed by the backend id). Events without a session id are the
            // dir's own runtime lines. Without this, the daemon session's
            // row absorbed whichever session's capabilities the tee copied
            // in first and the merge below stamped that config onto another
            // session's row (the codex-for-claude-code incident,
            // 2026-07-16).
            if let Some(event_session_id) = obj
                .get("data")
                .and_then(|data| data.get("session_id"))
                .and_then(|v| v.as_str())
            {
                let belongs = crate::session_identity::event_names_session(
                    Some(event_session_id),
                    session_id,
                    canonical_session_id.as_deref(),
                ) || external_resume_id.as_deref() == Some(event_session_id);
                if !belongs {
                    continue;
                }
            }

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
                    // upgrades append). Attribution matching keeps identities
                    // the bus tee copied into the daemon-main log from
                    // stamping the daemon session's row with a child's
                    // backend id (`event_names_session` — the addressing
                    // predicate `wrapper_matches` accepted ANY event once
                    // meta and dir agreed on the id), and the canonical
                    // filter keeps placeholder ids in pre-guard logs from
                    // conjuring ghost windows (see
                    // `scraped_external_thread_id_is_canonical`).
                    if let Some(data) = obj.get("data") {
                        if crate::session_identity::event_names_session(
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
                            let task_text = message.trim_start_matches("Task: ");
                            preview.push_user(task_text);
                            task = Some(task_text.to_string());
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
                "steer_requested" => {
                    if let Some(text) = obj
                        .get("data")
                        .and_then(|d| d.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        preview.push_user(text);
                    }
                }
                "model_response" => {
                    // The event's message is the model TEXT preview (first
                    // 200 chars); tool calls live in other event kinds.
                    if !message.is_empty() {
                        preview.push_assistant(message);
                    }
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
                        if let Some(created) = tok.get("cache_creation").and_then(|v| v.as_u64()) {
                            cache_creation_tokens += created;
                            event_usage.cache_creation_tokens = created;
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
                }
                "round_complete" if status != "interrupted" => {
                    status = "idle".to_string();
                }
                _ => {}
            }
        }
    }

    if status != "completed" {
        if let Some(ended) = summary_json_status(dir) {
            status = ended.to_string();
        }
    }

    // Dirs that predate the per-session config file: recover launch config
    // from the session's own (last) capabilities announcement — but only
    // under a source its own identity established. Fabricating a source
    // here (this used to default to "codex") let the daemon session's row
    // grow another session's launch config from tee-copied capabilities.
    if session_agent_config.is_none() {
        if let (Some(source), Some(caps)) = (external_source.as_deref(), capabilities.as_ref()) {
            let caps_str =
                |key: &str| -> Option<&str> { caps.get(key).and_then(|value| value.as_str()) };
            session_agent_config = Some(crate::session_config::from_wire(
                Some(source),
                caps_str("codex_command"),
                None,
                None,
                caps_str("codex_managed_context"),
                caps_str("codex_context_archive"),
                caps_str("codex_service_tier"),
            ));
        }
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

    let session_ended = SESSION_ENDED_STATUSES.contains(&status.as_str());
    if !session_ended {
        let has_model_work = turns > 0 || total_tokens > 0;
        if !has_model_work {
            let has_media = recording_count > 0 || annotation_count > 0 || clip_count > 0;
            if role.as_deref() == Some("resident") {
                // The daemon's own base session (marked at boot): a bare
                // one is the daemon idling, not an abandoned user task.
                status = "resident".to_string();
            } else if task.is_some() || has_media {
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
            cache_creation_tokens,
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
        "cache_creation_tokens": cache_creation_tokens,
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
        "worktree": worktree,
        "path": dir.to_string_lossy().to_string(),
        "can_delete": true,
        "can_resume": true,
        "relationships": relationships,
    });
    if let Some(preview) = preview.into_value() {
        wrapper_session["preview"] = preview;
    }
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
    list_sessions_from_home_impl(home_path, EXTERNAL_SESSION_SCAN_LIMIT, None)
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
    list_sessions_from_home_impl(home_path, external_scan_limit, limit)
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
    let mut worktree = serde_json::Value::Null;
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
            if let Some(value) = meta.get("worktree").filter(|v| v.is_object()) {
                worktree = value.clone();
            }
        }
    }
    if status != "completed" {
        if let Some(ended) = summary_json_status(dir) {
            status = ended.to_string();
        }
    }
    if created_at.is_none() {
        created_at = mtime_secs_to_string(file_mtime_secs(dir));
    }
    let created_at = created_at.unwrap_or_default();
    let updated_at = mtime_secs_to_string(session_activity_mtime_secs(dir))
        .unwrap_or_else(|| created_at.clone());
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
        "worktree": worktree,
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
    let logs_dir = crate::platform::intendant_home_in(home_path).join("logs");
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
            let mtime = session_activity_mtime_secs(&dir);
            Some((dir, mtime))
        })
        .collect::<Vec<_>>();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.1));
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
    list_sessions_from_home_impl(home_path, usize::MAX, None)
}

/// `row_cap: None` serves the complete corpus. Only explicit numeric limits
/// truncate: usage totals (Stats, Sessions header) fold the unlimited body,
/// and any silent cap deletes real history from them — a 5k-row cap on the
/// unlimited slot once ate ~36B of old-session tokens when test litter
/// pushed the corpus past it.
pub(crate) fn list_sessions_from_home_impl(
    home_path: &Path,
    external_scan_limit: usize,
    row_cap: Option<usize>,
) -> String {
    preload_session_index();
    let logs_dir = crate::platform::intendant_home_in(home_path).join("logs");
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
        if let Some(cap) = row_cap {
            truncate_sessions_preserving_sources_to(&mut external_sessions, cap);
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
            let mtime = session_activity_mtime_secs(&dir);
            Some((dir, mtime))
        })
        .collect::<Vec<_>>();
    if let Some(cap) = row_cap {
        dirs.sort_by_key(|d| std::cmp::Reverse(d.1));
        let scan_limit = cap
            .saturating_add(SESSION_SOURCE_FLOOR * 3)
            .clamp(cap, SESSION_LIST_LIMIT);
        dirs.truncate(scan_limit);
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
    if let Some(cap) = row_cap {
        truncate_sessions_preserving_sources_to(&mut sessions, cap);
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

/// The session-list stream's ONE line source (transport-unification
/// S10): quick skeleton rows, the hydrating phase marker, the
/// cache-hydrated replace, done — pushed as complete NDJSON lines into
/// `tx`. Both transports spawn exactly this function (via
/// `sessions_stream_api_response`) and own only their writers. This
/// production entry resolves the ambient home and hydrates through the
/// process-wide session-list caches; the parity fixture injects a temp
/// store and the direct scan through `stream_sessions_lines_from_home`.
pub(crate) fn stream_sessions_lines(
    requested_limit: Option<usize>,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    stream_sessions_lines_from_home(
        &crate::platform::home_dir(),
        requested_limit,
        || {
            requested_limit
                .map(cached_list_sessions_with_limit)
                .unwrap_or_else(cached_list_sessions)
        },
        tx,
    )
}

/// Root-injected body of [`stream_sessions_lines`] (hermetic-test
/// convention: the quick phase scans `home`; the replace phase's
/// hydrated list body comes from `hydrated_body` — the response caches
/// in production, a direct `list_sessions_from_home_with_limit` scan in
/// fixtures).
pub(crate) fn stream_sessions_lines_from_home(
    home: &Path,
    requested_limit: Option<usize>,
    hydrated_body: impl FnOnce() -> Arc<str>,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    let quick_limit = requested_limit
        .unwrap_or(SESSION_LIST_LIMIT)
        .min(SESSION_LIST_STREAM_QUICK_LIMIT);
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
        home,
        quick_limit,
    ));
    quick_rows.extend(list_codex_index_skeleton_sessions_with_limit(
        home,
        quick_limit,
    ));
    merge_quick_session_rows_with_wrapper_index(home, &mut quick_rows);
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

    let body = hydrated_body();
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
    fn invalidate_session_list_response_caches_clears_both_tiers() {
        {
            let cache = SESSION_LIST_RESPONSE_CACHE.get_or_init(|| Mutex::new(None));
            *cache.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(SessionListResponseCacheEntry {
                    generated_at: std::time::Instant::now(),
                    generation: next_session_list_body_generation(),
                    body: "[{\"session_id\":\"stale\"}]".into(),
                });
        }
        store_session_list_response(5, "[]".to_string());
        // Seed a projection so the invalidation has something to clear.
        let _ = projected_session_list_body(Some(u64::MAX), "[]".into(), Some(1), false);
        invalidate_session_list_response_caches();
        assert!(SESSION_LIST_RESPONSE_CACHE
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .is_none());
        assert!(cached_limited_session_list_cache()
            .lock()
            .unwrap()
            .is_empty());
        assert_eq!(session_list_projection_cache_len(), 0);
    }

    #[test]
    fn projected_session_list_body_caches_by_generation_and_matches_direct() {
        let body: Arc<str> = serde_json::json!([
            {"session_id": "s-1", "source": "intendant", "task": "keep",
             "total_tokens": 10, "model": "m", "project_root": "/repo"},
            {"session_id": "s-2", "source": "codex", "task": "drop-by-limit",
             "total_tokens": 20, "model": "m", "project_root": "/repo"},
        ])
        .to_string()
        .into();

        // Identity projection: no limit, no usage view — the SAME
        // allocation passes through (a refcount clone, not a text
        // copy) and nothing is cached for it.
        let identity = projected_session_list_body(Some(9_100), Arc::clone(&body), None, false);
        assert!(Arc::ptr_eq(&identity, &body));

        for (limit, usage_view) in [(Some(1), false), (Some(1), true), (None, true)] {
            let direct = {
                let limited = limit_session_list_body(&body, limit);
                if usage_view {
                    session_list_body_usage_view(&limited)
                } else {
                    limited
                }
            };
            // Unique generation per case isolates this test from cache
            // state other tests leave behind.
            let generation = 9_200 + limit.unwrap_or(0) as u64 * 2 + usage_view as u64;
            let first =
                projected_session_list_body(Some(generation), Arc::clone(&body), limit, usage_view);
            assert_eq!(&*first, direct.as_str());
            // Second call must serve the cached projection (same result,
            // same allocation); a DIFFERENT body under the SAME
            // generation proves the cache is keyed by generation, not by
            // re-deriving from the body.
            let second =
                projected_session_list_body(Some(generation), "[]".into(), limit, usage_view);
            assert_eq!(&*second, direct.as_str());
            assert!(Arc::ptr_eq(&first, &second));
            // A new generation recomputes from the supplied body.
            let recomputed = projected_session_list_body(
                Some(generation + 5_000),
                "[]".into(),
                limit,
                usage_view,
            );
            assert_ne!(&*recomputed, direct.as_str());
        }

        // Uncachable source (generation: None) computes directly.
        let usage = projected_session_list_body(None, Arc::clone(&body), None, true);
        assert_eq!(&*usage, session_list_body_usage_view(&body));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&usage).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].get("task").is_none(), "usage view strips tasks");
        assert!(parsed[0].get("total_tokens").is_some());
    }

    #[test]
    fn served_session_list_bodies_share_the_cached_allocation() {
        let body: Arc<str> = "[{\"session_id\":\"shared\"}]".into();
        let entry = SessionListResponseCacheEntry {
            generated_at: std::time::Instant::now(),
            generation: 4_242,
            body: Arc::clone(&body),
        };
        let (served, generation) =
            serve_session_list_cache_entry_with_generation(4_242, Some(&entry))
                .expect("fresh entry serves");
        assert_eq!(generation, 4_242);
        assert!(
            Arc::ptr_eq(&served, &body),
            "a fresh hit must hand out the stored Arc, not a text copy"
        );
        assert!(serve_session_list_cache_entry_with_generation(4_242, None).is_none());
    }

    #[test]
    fn session_list_refresh_is_single_flight_per_slot_and_globally_capped() {
        // Everything below drives the claim guard directly — no real
        // refresh thread ever spawns (list scans would leave the temp
        // roots), and every claim is released before the test returns.
        // One test fn on purpose: the in-flight set is process-global,
        // so splitting these assertions across tests would let them
        // interleave under the multi-threaded default test runner.

        // Two stale hits on one slot ⇒ one refresh: the first claim
        // wins, the second is refused while the first is in flight.
        let slot = 61_000;
        assert!(try_begin_session_list_refresh(slot));
        assert!(
            !try_begin_session_list_refresh(slot),
            "a slot with a refresh in flight must refuse a second claim"
        );

        // While that refresh is in flight, a stale-but-usable entry
        // still serves (stale-while-revalidate), and serving does not
        // spawn a second refresh — the slot claim above stays the only
        // one. (Instant is monotonic from boot; within the first TTL
        // seconds after boot there is nothing to back-date into, so the
        // serve probe is skipped there.)
        let body: Arc<str> = "[]".into();
        if let Some(stale_at) = std::time::Instant::now().checked_sub(
            std::time::Duration::from_secs(SESSION_LIST_RESPONSE_CACHE_TTL_SECS + 1),
        ) {
            let stale = SessionListResponseCacheEntry {
                generated_at: stale_at,
                generation: 7,
                body: Arc::clone(&body),
            };
            for _ in 0..2 {
                let (served, generation) =
                    serve_session_list_cache_entry_with_generation(slot, Some(&stale))
                        .expect("stale-but-usable entry serves while a refresh is in flight");
                assert!(Arc::ptr_eq(&served, &body));
                assert_eq!(generation, 7);
            }
            assert!(
                !try_begin_session_list_refresh(slot),
                "stale serves must not have released or duplicated the slot claim"
            );
        }
        if let Some(dead_at) = std::time::Instant::now().checked_sub(
            std::time::Duration::from_secs(SESSION_LIST_RESPONSE_STALE_MAX_SECS + 1),
        ) {
            let dead = SessionListResponseCacheEntry {
                generated_at: dead_at,
                generation: 8,
                body: Arc::clone(&body),
            };
            assert!(
                serve_session_list_cache_entry_with_generation(61_001, Some(&dead)).is_none(),
                "past the stale ceiling nothing serves (and nothing spawns)"
            );
        }
        end_session_list_refresh(slot);

        // Global ceiling: stale hits across DISTINCT slots stop
        // spawning once the cap of concurrent refreshes is reached.
        let base = 62_000;
        let mut claimed = Vec::new();
        for slot in base.. {
            if !try_begin_session_list_refresh(slot) {
                break;
            }
            claimed.push(slot);
        }
        assert_eq!(claimed.len(), SESSION_LIST_REFRESH_MAX_CONCURRENT);
        assert!(
            !try_begin_session_list_refresh(base + 10_000),
            "at the ceiling every further slot is refused"
        );
        end_session_list_refresh(claimed.pop().expect("cap claims"));
        assert!(
            try_begin_session_list_refresh(base + 10_000),
            "a finished refresh frees capacity for other slots"
        );
        claimed.push(base + 10_000);
        for slot in claimed {
            end_session_list_refresh(slot);
        }
    }

    #[test]
    fn summary_json_status_maps_outcomes_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let write = |outcome: &str| {
            std::fs::write(
                dir.path().join("summary.json"),
                serde_json::json!({ "task": "t", "outcome": outcome }).to_string(),
            )
            .unwrap();
        };
        write("completed");
        assert_eq!(summary_json_status(dir.path()), Some("completed"));
        write("stopped by user");
        assert_eq!(summary_json_status(dir.path()), Some("interrupted"));
        write("Interrupted");
        assert_eq!(summary_json_status(dir.path()), Some("interrupted"));
        write("external agent event channel closed");
        assert_eq!(summary_json_status(dir.path()), Some("failed"));
        write("error: spawn failed");
        assert_eq!(summary_json_status(dir.path()), Some("failed"));
        // Legacy summaries without an outcome field still read completed.
        std::fs::write(dir.path().join("summary.json"), "{\"task\":\"t\"}").unwrap();
        assert_eq!(summary_json_status(dir.path()), Some("completed"));
        // No summary at all → the session has not ended.
        std::fs::remove_file(dir.path().join("summary.json")).unwrap();
        assert_eq!(summary_json_status(dir.path()), None);
    }

    /// The Claude sign-in reload panel offers chips for ALIVE claude-code
    /// sessions by excluding this module's terminal statuses — a static
    /// frontend mirror ("derive, don't mirror" fallback rule: the mirror
    /// gets a daemon-side parity test). The fragment's set must equal
    /// `SESSION_ENDED_STATUSES` plus the `"abandoned"` classification,
    /// exactly — an addition or rename on either side fails here.
    #[test]
    fn claude_signin_terminal_status_mirror_matches_catalog_vocabulary() {
        let app = include_str!("../../../../../static/app.html");
        let start = "const CLAUDE_SIGNIN_TERMINAL_STATUSES = new Set([";
        let from = app
            .find(start)
            .expect("CLAUDE_SIGNIN_TERMINAL_STATUSES not found in app.html")
            + start.len();
        let rest = &app[from..];
        let to = rest
            .find("])")
            .expect("CLAUDE_SIGNIN_TERMINAL_STATUSES is unterminated");
        let mirrored: std::collections::BTreeSet<String> = rest[..to]
            .split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();

        let mut expected: std::collections::BTreeSet<String> = SESSION_ENDED_STATUSES
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.insert("abandoned".to_string());
        assert_eq!(
            mirrored, expected,
            "static/app/32-vault-custody.js CLAUDE_SIGNIN_TERMINAL_STATUSES \
             must mirror the catalog's terminal vocabulary"
        );
    }

    #[test]
    fn resident_role_dir_classifies_as_resident_not_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::json!({
                "session_id": "resident-1",
                "created_at": "2026-07-08T00:00:00",
                "status": "running",
                "role": "resident",
            })
            .to_string(),
        )
        .unwrap();
        let row = intendant_session_list_row_from_dir(dir.path(), "resident-1").unwrap();
        assert_eq!(row["status"], "resident");
        assert_eq!(row["role"], "resident");
    }

    #[test]
    fn bare_taskless_dir_without_role_still_classifies_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::json!({
                "session_id": "ghost-1",
                "created_at": "2026-07-08T00:00:00",
                "status": "running",
            })
            .to_string(),
        )
        .unwrap();
        let row = intendant_session_list_row_from_dir(dir.path(), "ghost-1").unwrap();
        assert_eq!(row["status"], "abandoned");
    }

    /// The daemon session's log carries the bus tee's copies of every
    /// session's lifecycle events. Its row must not absorb them: no
    /// backend-id claim, no launch config harvested from another session's
    /// capabilities, no status flip from another session's task_complete —
    /// the exact contamination that resumed a claude-code session on the
    /// codex binary (2026-07-16).
    #[test]
    fn daemon_lane_log_events_do_not_contaminate_the_dir_row() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_id = "aaaa1111-0000-4000-8000-00000000d0d0";
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::json!({
                "session_id": daemon_id,
                "created_at": "2026-07-16T08:33:17",
                "status": "running",
                "task": "daemon lane",
            })
            .to_string(),
        )
        .unwrap();
        let events = [
            serde_json::json!({"event": "session_start", "ts": "08:33:17.908"}),
            // Tee'd copies of a codex session's announcements…
            serde_json::json!({"event": "session_capabilities", "data": {
                "session_id": "cccc2222-0000-4000-8000-00000000c0de",
                "capabilities": {"codex_command": "codex", "codex_managed_context": "vanilla"},
            }}),
            serde_json::json!({"event": "session_identity", "data": {
                "session_id": "cccc2222-0000-4000-8000-00000000c0de",
                "source": "codex",
                "backend_session_id": "019ea8b9-0000-7000-8000-00000000beef",
            }}),
            // …and of that session finishing.
            serde_json::json!({"event": "task_complete", "data": {
                "session_id": "cccc2222-0000-4000-8000-00000000c0de",
            }}),
        ];
        std::fs::write(
            dir.path().join("session.jsonl"),
            events.map(|event| event.to_string()).join("\n") + "\n",
        )
        .unwrap();

        let row = intendant_session_list_row_from_dir(dir.path(), daemon_id).unwrap();
        assert_eq!(row["backend_session_id"], serde_json::Value::Null);
        assert_eq!(row["backend_source"], serde_json::Value::Null);
        assert!(row.get("agent_command").is_none());
        assert!(row.get("codex_command").is_none());
        assert!(row.get("configured_source").is_none());
        assert_eq!(row["capabilities"], serde_json::Value::Null);
        assert_ne!(row["status"], "completed");
    }

    /// A wrapper session's own events stay attributed through the identity
    /// collapse: once its identity event binds the backend id, later events
    /// keyed by that backend id (capabilities, completion) are its own.
    #[test]
    fn wrapper_row_follows_its_own_identity_chain() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper_id = "bbbb3333-0000-4000-8000-00000000aaaa";
        let backend_id = "019ea8b9-0000-7000-8000-00000000cafe";
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::json!({
                "session_id": wrapper_id,
                "created_at": "2026-07-16T07:46:41",
                "status": "running",
                "task": "wrapped work",
            })
            .to_string(),
        )
        .unwrap();
        let events = [
            serde_json::json!({"event": "session_start", "ts": "07:46:41.671"}),
            serde_json::json!({"event": "session_identity", "data": {
                "session_id": wrapper_id,
                "source": "codex",
                "backend_session_id": backend_id,
            }}),
            // Post-collapse: the session's own events are keyed by the
            // backend id.
            serde_json::json!({"event": "session_capabilities", "data": {
                "session_id": backend_id,
                "capabilities": {"codex_command": "codex-nightly", "codex_managed_context": "managed"},
            }}),
            serde_json::json!({"event": "task_complete", "data": {
                "session_id": backend_id,
            }}),
        ];
        std::fs::write(
            dir.path().join("session.jsonl"),
            events.map(|event| event.to_string()).join("\n") + "\n",
        )
        .unwrap();

        let row = intendant_session_list_row_from_dir(dir.path(), wrapper_id).unwrap();
        assert_eq!(row["backend_session_id"], backend_id);
        assert_eq!(row["backend_source"], "codex");
        assert_eq!(row["status"], "completed");
        assert_eq!(row["capabilities"]["codex_command"], "codex-nightly");
        // The capabilities fallback recovers launch config under the
        // session's OWN source (no fabricated default).
        assert_eq!(row["agent_command"], "codex-nightly");
        assert_eq!(row["codex_managed_context"], "managed");
    }

    #[test]
    fn session_row_answers_to_id_checks_all_identity_fields() {
        let row = serde_json::json!({
            "session_id": "wrapper-1",
            "resume_id": "resume-1",
            "backend_session_id": "backend-1",
            "intendant_session_id": "intendant-1",
        });
        for id in ["wrapper-1", "resume-1", "backend-1", "intendant-1"] {
            assert!(session_row_answers_to_id(&row, id), "{id}");
        }
        assert!(!session_row_answers_to_id(&row, "task-ghost"));
        assert!(!session_row_answers_to_id(&row, ""));
    }

    #[test]
    fn session_activity_mtime_prefers_transcript_over_dir_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("session.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        // Age the transcript, then simulate daemon bookkeeping (a
        // fission-ledger rewrite renames into the dir → dir mtime = now).
        let aged = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&transcript)
            .unwrap();
        f.set_modified(aged).unwrap();
        drop(f);
        std::fs::write(dir.path().join("fission_ledger.json"), "{}").unwrap();

        assert_eq!(session_activity_mtime_secs(dir.path()), 1_000_000_000);
        assert!(
            file_mtime_secs(dir.path()) > 1_000_000_000,
            "dir mtime should reflect the bookkeeping write"
        );
        // Without a transcript, the dir mtime is the only signal left.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(
            session_activity_mtime_secs(bare.path()),
            file_mtime_secs(bare.path())
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
    fn intendant_row_prices_gpt_5_6_cache_writes_from_native_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        std::fs::create_dir_all(&log_dir).unwrap();
        let events = [
            serde_json::json!({
                "ts": "2026-07-11T12:00:00Z",
                "event": "info",
                "message": "Provider: openai"
            }),
            serde_json::json!({
                "ts": "2026-07-11T12:00:01Z",
                "event": "info",
                "message": "Model: gpt-5.6-sol"
            }),
            serde_json::json!({
                "ts": "2026-07-11T12:00:02Z",
                "event": "model_response",
                "turn": 1,
                "message": "done",
                "data": {"tokens": {
                    "prompt": 3_000_000,
                    "completion": 1_000_000,
                    "total": 4_000_000,
                    "cached": 1_000_000,
                    "cache_creation": 1_000_000
                }}
            }),
        ];
        let jsonl: String = events.iter().map(|event| format!("{event}\n")).collect();
        std::fs::write(log_dir.join("session.jsonl"), jsonl).unwrap();

        let row = intendant_session_list_row_from_dir(&log_dir, "session").unwrap();
        assert_eq!(row["cache_creation_tokens"].as_u64(), Some(1_000_000));
        assert_eq!(row["cached_tokens"].as_u64(), Some(1_000_000));
        assert!((row["estimated_cost"].as_f64().unwrap() - 41.75).abs() < 1e-12);
    }

    #[test]
    fn intendant_row_preview_collects_prose_and_respects_slots() {
        let home = tempfile::tempdir().unwrap();
        let log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("preview-1");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "created_at": "2026-07-07T10:00:00Z",
                "task": "Fix the login bug",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        let events = [
            serde_json::json!({"ts": "2026-07-07T10:00:00Z", "event": "session_start", "message": "Session started"}),
            // Non-conversation events must never enter the preview.
            serde_json::json!({"ts": "2026-07-07T10:00:01Z", "event": "messages_input", "turn": 1, "message": "Messages logged (9000 bytes)"}),
            serde_json::json!({"ts": "2026-07-07T10:00:02Z", "event": "info", "level": "info", "message": "[runtime] Task dispatched: something"}),
            serde_json::json!({"ts": "2026-07-07T10:00:03Z", "event": "model_response", "turn": 1, "message": "I will start by reading the auth module."}),
            serde_json::json!({"ts": "2026-07-07T10:00:04Z", "event": "steer_requested", "message": "Steer requested: Also update the tests", "data": {"id": "s1", "status": "requested", "text": "Also update the tests"}}),
            serde_json::json!({"ts": "2026-07-07T10:00:05Z", "event": "model_response", "turn": 2, "message": "Tests updated; the fix is in."}),
            // Third assistant message: both assistant slots are taken.
            serde_json::json!({"ts": "2026-07-07T10:00:06Z", "event": "model_response", "turn": 3, "message": "Extra reply that must not appear."}),
        ];
        let jsonl: String = events.iter().map(|e| format!("{e}\n")).collect();
        std::fs::write(log_dir.join("session.jsonl"), jsonl).unwrap();

        let row = intendant_session_list_row_from_dir(&log_dir, "preview-1").unwrap();
        let preview = row.get("preview").and_then(|v| v.as_array()).unwrap();
        let flat: Vec<(String, String)> = preview
            .iter()
            .map(|e| {
                (
                    e.get("role").and_then(|v| v.as_str()).unwrap().to_string(),
                    e.get("text").and_then(|v| v.as_str()).unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            flat,
            vec![
                ("user".to_string(), "Fix the login bug".to_string()),
                (
                    "assistant".to_string(),
                    "I will start by reading the auth module.".to_string()
                ),
                ("user".to_string(), "Also update the tests".to_string()),
                (
                    "assistant".to_string(),
                    "Tests updated; the fix is in.".to_string()
                ),
            ]
        );
    }

    #[test]
    fn preview_builder_enforces_byte_cap_on_char_boundary() {
        let mut builder = SessionPreviewBuilder::default();
        let long = "é".repeat(200); // 2 bytes per char; compacted to 160 chars
        builder.push_user(&long);
        builder.push_assistant(&long);
        builder.push_user(&long);
        builder.push_assistant(&long);
        let value = builder.into_value().unwrap();
        let entries = value.as_array().unwrap();
        let total: usize = entries
            .iter()
            .map(|e| e.get("text").and_then(|v| v.as_str()).unwrap().len())
            .sum();
        assert!(total <= SESSION_PREVIEW_MAX_BYTES, "total {total}");
        // Every surviving text is valid UTF-8 by construction (as_str) —
        // the cap must land on a char boundary rather than panic.
        assert!(entries.len() >= 2);
    }

    #[test]
    fn preview_prose_text_excludes_tool_blocks() {
        let content = serde_json::json!([
            {"type": "text", "text": "real prose"},
            {"type": "tool_use", "name": "Bash", "input": {"command": "rm -rf /tmp/x"}},
            {"type": "tool_result", "content": "raw tool output that must not leak"},
        ]);
        assert_eq!(message_prose_text(&content).as_deref(), Some("real prose"));
        let tool_only = serde_json::json!([
            {"type": "tool_result", "content": "raw tool output"},
        ]);
        assert_eq!(message_prose_text(&tool_only), None);
        assert_eq!(
            message_prose_text(&serde_json::json!("plain string prompt")).as_deref(),
            Some("plain string prompt")
        );
    }

    #[test]
    fn unlimited_session_list_serves_every_row() {
        let home = tempfile::tempdir().unwrap();
        let logs_dir = home.path().join(".intendant").join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        for idx in 0..4 {
            let session_id = format!("complete-session-{idx}");
            let log_dir = logs_dir.join(&session_id);
            std::fs::create_dir_all(&log_dir).unwrap();
            std::fs::write(
                log_dir.join("session_meta.json"),
                serde_json::json!({
                    "created_at": format!("2026-06-07T16:0{idx}:00Z"),
                    "task": format!("complete task {idx}"),
                    "status": "idle"
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                log_dir.join("session.jsonl"),
                serde_json::json!({
                    "ts": format!("2026-06-07T16:0{idx}:00Z"),
                    "event": "session_start"
                })
                .to_string(),
            )
            .unwrap();
        }

        // The unlimited list is what Stats and the Sessions header fold for
        // lifetime usage totals — it must never truncate, only explicit
        // numeric limits may.
        let body = list_sessions_from_home(home.path());
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(sessions.len(), 4);

        let capped = list_sessions_from_home_impl(home.path(), usize::MAX, Some(3));
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&capped).unwrap();
        assert_eq!(sessions.len(), 3);
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
        let crate::web_gateway::ApiResponse::Json { status, body, .. } = response else {
            panic!("agent output must answer on the json lane");
        };
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_str(&body.into_string()).unwrap();
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
    fn usage_view_strips_heavy_row_fields() {
        let body = serde_json::json!([{
            "session_id": "s-1",
            "source": "codex",
            "model": "gpt-5.5",
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
        // The by-model Usage cards need the model name kept in the slim view.
        assert!(row.contains_key("model"));
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
