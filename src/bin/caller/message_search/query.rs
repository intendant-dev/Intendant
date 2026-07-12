//! The C1 query side (plan §7): match a small term set against the shard
//! store and return session-grouped hits with byte-range highlights into
//! the ORIGINAL text. Pure over a store root — the gateway edge owns
//! transport, the freshness refresh, and the concurrency cap.
//!
//! Matching model: every term must appear in ONE message. Needle and
//! haystack are folded identically — per-char simple lowercase + Unicode
//! canonical decomposition (NFD) — which matches the same pairs as the
//! plan's "NFC + simple case-fold" while keeping the folded→original
//! offset map exact per original char (NFC composes ACROSS chars, which
//! would blur range edges). Highlight ranges are byte offsets into the
//! original text; the client never re-derives them.
//!
//! Pagination is snapshot-bound: the opaque cursor pins the query/filter
//! hash and the manifest watermark it was minted against. Any manifest
//! change between pages expires the cursor (`cursor_expired`) and the
//! client restarts — stricter than the plan's minimum (a page can never
//! silently skip a session that moved), and cheap to loosen later.

use super::record::{derive_active, Role, Source};
use super::store::{SessionShard, Store, RETENTION_MS};
use base64::Engine;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) const MAX_QUERY_BYTES: usize = 256;
pub(crate) const MAX_TERMS: usize = 8;
const MAX_SESSIONS_PER_PAGE: usize = 50;
const DEFAULT_SESSIONS_PER_PAGE: usize = 20;
const HITS_PER_SESSION: usize = 3;
/// Response body budget (plan §7): past this the page ends early with
/// `partial: "budget"` and a continuation cursor.
const RESPONSE_BUDGET_BYTES: usize = 256 * 1024;
/// Rough serialized overhead per hit besides the snippet itself.
const HIT_OVERHEAD_BYTES: usize = 320;
/// Snippet window around the first match, in bytes of original text.
const SNIPPET_BEFORE_BYTES: usize = 120;
const SNIPPET_TOTAL_BYTES: usize = 280;
/// Byte budget for shards resident in the fold arena (LRU beyond it).
/// Calibrated on the real corpus (soak 2026-07-12): 1.4k sessions cost
/// ~2x their 36MB of shard JSON with the ASCII fast path — a budget
/// smaller than the working set makes every query re-load and re-fold
/// under its time budget, answering `partial` forever.
const ARENA_BUDGET_BYTES: usize = 192 * 1024 * 1024;
/// Per-query ceiling on freshly loaded shard bytes: a query that would
/// have to page the whole arena through memory ends early with
/// `partial: "budget"` instead of thrashing.
const QUERY_LOAD_BUDGET_BYTES: usize = 48 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageSearchParams {
    pub q: String,
    /// `None` = every source.
    pub sources: Option<Vec<Source>>,
    pub include_superseded: bool,
    pub include_subagents: bool,
    pub cursor: Option<String>,
    pub limit: usize,
}

impl Default for MessageSearchParams {
    fn default() -> Self {
        Self {
            q: String::new(),
            sources: None,
            include_superseded: true,
            include_subagents: true,
            cursor: None,
            limit: DEFAULT_SESSIONS_PER_PAGE,
        }
    }
}

pub(crate) fn parse_sources(raw: &str) -> Option<Vec<Source>> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "all" {
        return None;
    }
    let sources: Vec<Source> = raw
        .split(',')
        .filter_map(|token| match token.trim() {
            "intendant" => Some(Source::Intendant),
            "codex" => Some(Source::Codex),
            "claude-code" => Some(Source::ClaudeCode),
            _ => None,
        })
        .collect();
    if sources.is_empty() {
        None
    } else {
        Some(sources)
    }
}

/// Run one search page. Returns `(http_status, body)`; the body always
/// carries `ok` and, on success, the session groups + coverage block.
pub(crate) fn run_message_search(
    store_root: &Path,
    params: &MessageSearchParams,
    now_ms: i64,
    time_budget_ms: u64,
) -> (u16, serde_json::Value) {
    if params.q.len() > MAX_QUERY_BYTES {
        return bad_request("query too long");
    }
    let terms: Vec<FoldedText> = params
        .q
        .split_whitespace()
        .map(fold_text)
        .filter(|folded| !folded.folded.is_empty())
        .collect();
    if terms.is_empty() {
        return bad_request("q required");
    }
    if terms.len() > MAX_TERMS {
        return bad_request("too many terms");
    }
    let limit = params.limit.clamp(1, MAX_SESSIONS_PER_PAGE);

    let store = match Store::open(store_root) {
        Ok(store) => store,
        Err(err) => {
            return (
                500,
                serde_json::json!({"ok": false, "error": format!("store open failed: {err}")}),
            );
        }
    };
    let snapshot = store.snapshot();
    // The snapshot pin: a monotonic write counter, never the clock (two
    // writes can share a millisecond).
    let watermark = snapshot.manifest.revision;
    let filters_fingerprint = fingerprint(params, &terms);
    let after = match params.cursor.as_deref() {
        None => None,
        Some(raw) => match decode_cursor(raw, &filters_fingerprint) {
            Ok(cursor) if cursor.watermark == watermark => Some(cursor),
            Ok(_) => {
                return (
                    410,
                    serde_json::json!({"ok": false, "error": "cursor_expired"}),
                );
            }
            Err(message) => return bad_request(message),
        },
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(time_budget_ms);
    let horizon_ms = now_ms.saturating_sub(RETENTION_MS);
    let mut partial_reason: Option<&'static str> = None;
    let mut loaded_bytes: usize = 0;

    // Scan newest-first with an early exit: a session's best hit can
    // never be newer than its entry's newest_ts_ms, so once the page is
    // full and the next entry is older than the page's worst kept best
    // hit, nothing later can displace it. Broad terms become
    // limit-bounded instead of full-corpus (soak 2026-07-12: key-order
    // scanning answered `partial` forever on the real corpus, and a
    // timeout cut could drop the newest sessions from the page).
    let mut ordered: Vec<(&String, &super::store::SessionEntry)> =
        snapshot.manifest.sessions.iter().collect();
    ordered.sort_by(|a, b| {
        b.1.newest_ts_ms
            .cmp(&a.1.newest_ts_ms)
            .then_with(|| a.0.cmp(b.0))
    });
    let mut matched: Vec<SessionMatch> = Vec::new();
    let mut kept_past_cursor = 0usize;
    for (session_key, entry) in ordered {
        if std::time::Instant::now() >= deadline {
            partial_reason = Some("timeout");
            break;
        }
        if kept_past_cursor > limit {
            let worst_kept = matched
                .iter()
                .map(|m| m.best_ts_ms)
                .min()
                .unwrap_or(i64::MIN);
            if entry.newest_ts_ms < worst_kept {
                break;
            }
        }
        let Some(source) = source_of_key(session_key) else {
            continue;
        };
        if params
            .sources
            .as_ref()
            .is_some_and(|sources| !sources.contains(&source))
        {
            continue;
        }
        if entry.newest_ts_ms < horizon_ms {
            continue;
        }
        let (shard, fresh_bytes) = match arena_load(store_root, &entry.generation_file) {
            Some(loaded) => loaded,
            None => continue,
        };
        loaded_bytes += fresh_bytes;
        if loaded_bytes > QUERY_LOAD_BUDGET_BYTES {
            partial_reason = Some("budget");
            break;
        }
        let active = &shard.active;
        let mut hits: Vec<Hit> = Vec::new();
        for (index, record) in shard.shard.records.iter().enumerate() {
            if record.ts_ms < horizon_ms {
                continue;
            }
            if !params.include_subagents && record.subagent {
                continue;
            }
            let superseded = !active[index];
            if superseded && !params.include_superseded {
                continue;
            }
            let folded = &shard.folded[index];
            let Some(ranges) = match_all_terms(folded, &terms) else {
                continue;
            };
            hits.push(Hit {
                record_index: index,
                superseded,
                ranges,
            });
        }
        if hits.is_empty() {
            continue;
        }
        let total_hits = hits.len();
        // Most-recent hits win the snippet slots.
        hits.sort_by(|a, b| {
            let a_ts = shard.shard.records[a.record_index].ts_ms;
            let b_ts = shard.shard.records[b.record_index].ts_ms;
            b_ts.cmp(&a_ts).then(b.record_index.cmp(&a.record_index))
        });
        hits.truncate(HITS_PER_SESSION);
        let best_ts_ms = shard.shard.records[hits[0].record_index].ts_ms;
        let past_cursor = match &after {
            None => true,
            Some(after) => {
                best_ts_ms < after.best_ts_ms
                    || (best_ts_ms == after.best_ts_ms && *session_key > after.session_key)
            }
        };
        if past_cursor {
            kept_past_cursor += 1;
        }
        matched.push(SessionMatch {
            session_key: session_key.clone(),
            source,
            best_ts_ms,
            total_hits,
            source_gone: entry.source_gone,
            hits,
            shard,
        });
    }

    // Sessions by best-hit recency within the pinned snapshot.
    matched.sort_by(|a, b| {
        b.best_ts_ms
            .cmp(&a.best_ts_ms)
            .then_with(|| a.session_key.cmp(&b.session_key))
    });
    if let Some(after) = &after {
        // Strictly past the cursor position in (best_ts desc, key asc)
        // order.
        matched.retain(|m| {
            m.best_ts_ms < after.best_ts_ms
                || (m.best_ts_ms == after.best_ts_ms && m.session_key > after.session_key)
        });
    }

    let mut sessions_json: Vec<serde_json::Value> = Vec::new();
    let mut spent_bytes = 0usize;
    let mut next_after: Option<(i64, String)> = None;
    let mut more = false;
    for m in &matched {
        if sessions_json.len() >= limit {
            more = true;
            break;
        }
        if spent_bytes > RESPONSE_BUDGET_BYTES {
            partial_reason.get_or_insert("budget");
            more = true;
            break;
        }
        let mut hits_json = Vec::new();
        for hit in &m.hits {
            let record = &m.shard.shard.records[hit.record_index];
            let (snippet, snippet_offset) = snippet_around(&record.text, &hit.ranges);
            spent_bytes += snippet.len() + HIT_OVERHEAD_BYTES;
            hits_json.push(serde_json::json!({
                "role": role_str(record.role),
                "ts_ms": record.ts_ms,
                "seq": record.seq,
                "superseded": hit.superseded,
                "truncated": record.truncated,
                "subagent": record.subagent,
                "snippet": snippet,
                "snippet_offset_bytes": snippet_offset,
                "ranges": hit.ranges,
                "locator": serde_json::to_value(&record.locator).unwrap_or_default(),
            }));
        }
        sessions_json.push(serde_json::json!({
            "session_key": m.session_key,
            "source": m.source.as_str(),
            "session_id": m.session_key.split_once(':').map(|(_, id)| id).unwrap_or_default(),
            "best_ts_ms": m.best_ts_ms,
            "total_hits": m.total_hits,
            "source_gone": m.source_gone,
            "hits": hits_json,
        }));
        next_after = Some((m.best_ts_ms, m.session_key.clone()));
    }

    let cursor = if more && partial_reason != Some("timeout") {
        next_after.map(|(best_ts_ms, session_key)| {
            encode_cursor(&Cursor {
                fingerprint: filters_fingerprint.clone(),
                watermark,
                best_ts_ms,
                session_key,
            })
        })
    } else {
        // A timeout page scanned an unknown subset: a continuation could
        // silently skip sessions, so the client retries the whole query
        // (the arena is warm by then).
        None
    };

    let state = if partial_reason.is_some() {
        "partial"
    } else if watermark == 0 {
        "building"
    } else {
        "ready"
    };
    let body = serde_json::json!({
        "ok": true,
        "state": state,
        "partial_reason": partial_reason,
        "window_days": RETENTION_MS / (24 * 60 * 60 * 1000),
        "sessions": sessions_json,
        "cursor": cursor,
        "coverage": coverage(&snapshot.manifest, horizon_ms),
    });
    (200, body)
}

fn bad_request(message: &str) -> (u16, serde_json::Value) {
    (400, serde_json::json!({"ok": false, "error": message}))
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn source_of_key(session_key: &str) -> Option<Source> {
    let (prefix, _) = session_key.split_once(':')?;
    match prefix {
        "intendant" => Some(Source::Intendant),
        "codex" => Some(Source::Codex),
        "claude-code" => Some(Source::ClaudeCode),
        _ => None,
    }
}

struct SessionMatch {
    session_key: String,
    source: Source,
    best_ts_ms: i64,
    total_hits: usize,
    source_gone: bool,
    hits: Vec<Hit>,
    shard: Arc<LoadedShard>,
}

struct Hit {
    record_index: usize,
    superseded: bool,
    /// Byte ranges into the ORIGINAL text: the first occurrence of each
    /// term, in term order.
    ranges: Vec<(u32, u32)>,
}

fn match_all_terms(folded: &FoldedText, terms: &[FoldedText]) -> Option<Vec<(u32, u32)>> {
    let mut ranges = Vec::with_capacity(terms.len());
    for term in terms {
        let found = folded.folded.find(&term.folded)?;
        ranges.push(folded.original_range(found, found + term.folded.len()));
    }
    Some(ranges)
}

/// A bounded window of ORIGINAL text around the earliest highlight;
/// `ranges` stay absolute, the client subtracts the returned offset.
fn snippet_around(text: &str, ranges: &[(u32, u32)]) -> (String, u32) {
    let anchor = ranges.iter().map(|(start, _)| *start).min().unwrap_or(0) as usize;
    let mut start = anchor.saturating_sub(SNIPPET_BEFORE_BYTES);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + SNIPPET_TOTAL_BYTES).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    (text[start..end].to_string(), start as u32)
}

fn coverage(manifest: &super::store::Manifest, horizon_ms: i64) -> serde_json::Value {
    let mut per_source: HashMap<&'static str, (usize, i64, i64, usize)> = HashMap::new();
    for (session_key, entry) in &manifest.sessions {
        let Some(source) = source_of_key(session_key) else {
            continue;
        };
        let slot = per_source
            .entry(source.as_str())
            .or_insert((0, i64::MAX, 0, 0));
        slot.0 += 1;
        slot.1 = slot.1.min(entry.newest_ts_ms);
        slot.2 = slot.2.max(entry.newest_ts_ms);
        if entry.source_gone {
            slot.3 += 1;
        }
    }
    let sources: serde_json::Map<String, serde_json::Value> = per_source
        .into_iter()
        .map(|(source, (sessions, oldest, newest, gone))| {
            (
                source.to_string(),
                serde_json::json!({
                    "sessions": sessions,
                    "oldest_ts_ms": if oldest == i64::MAX { 0 } else { oldest },
                    "newest_ts_ms": newest,
                    "source_gone": gone,
                }),
            )
        })
        .collect();
    serde_json::json!({
        "indexed_back_to_ms": horizon_ms,
        "sources": sources,
        // The legacy matrix (plan §7): what old sessions can ever yield.
        "legacy": {
            "ask_human_answers": "none before 2026-07 (never persisted)",
            "follow_ups": "best-effort before 2026-07",
        },
    })
}

// ---- Cursor ----

struct Cursor {
    fingerprint: String,
    watermark: u64,
    best_ts_ms: i64,
    session_key: String,
}

fn fingerprint(params: &MessageSearchParams, terms: &[FoldedText]) -> String {
    let folded_terms: Vec<&str> = terms.iter().map(|term| term.folded.as_str()).collect();
    let sources = params
        .sources
        .as_ref()
        .map(|sources| {
            sources
                .iter()
                .map(|source| source.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "all".to_string());
    crate::session_log::content_hash_hex16(&format!(
        "{}|{}|{}|{}",
        folded_terms.join("\u{1}"),
        sources,
        params.include_superseded,
        params.include_subagents,
    ))
}

fn encode_cursor(cursor: &Cursor) -> String {
    let body = serde_json::json!({
        "v": 1,
        "qh": cursor.fingerprint,
        "wm": cursor.watermark,
        "ts": cursor.best_ts_ms,
        "key": cursor.session_key,
    });
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body.to_string())
}

struct DecodedCursor {
    watermark: u64,
    best_ts_ms: i64,
    session_key: String,
}

fn decode_cursor(raw: &str, expected_fingerprint: &str) -> Result<DecodedCursor, &'static str> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| "invalid_cursor")?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "invalid_cursor")?;
    if value.get("v").and_then(|v| v.as_u64()) != Some(1) {
        return Err("invalid_cursor");
    }
    if value.get("qh").and_then(|v| v.as_str()) != Some(expected_fingerprint) {
        return Err("invalid_cursor");
    }
    Ok(DecodedCursor {
        watermark: value.get("wm").and_then(|v| v.as_u64()).unwrap_or(u64::MAX),
        best_ts_ms: value.get("ts").and_then(|v| v.as_i64()).unwrap_or(0),
        session_key: value
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

// ---- Folding ----

/// Folded text with an exact folded-byte → original-byte-offset map.
pub(crate) struct FoldedText {
    folded: String,
    /// Original char START offset for each folded byte. EMPTY when the
    /// fold is byte-aligned (pure-ASCII text: lowercasing is 1:1, and
    /// canonical decomposition of ASCII is identity) — the map is the
    /// arena's dominant cost (4 bytes per folded byte), and the corpus
    /// is overwhelmingly ASCII.
    starts: Vec<u32>,
    original_len: u32,
}

pub(crate) fn fold_text(text: &str) -> FoldedText {
    if text.is_ascii() {
        return FoldedText {
            folded: text.to_ascii_lowercase(),
            starts: Vec::new(),
            original_len: text.len() as u32,
        };
    }
    let mut folded = String::with_capacity(text.len());
    let mut starts = Vec::with_capacity(text.len());
    for (offset, ch) in text.char_indices() {
        let before = folded.len();
        for lower in ch.to_lowercase() {
            unicode_normalization::char::decompose_canonical(lower, |part| folded.push(part));
        }
        for _ in before..folded.len() {
            starts.push(offset as u32);
        }
    }
    FoldedText {
        folded,
        starts,
        original_len: text.len() as u32,
    }
}

impl FoldedText {
    /// Map a folded byte range back to original byte offsets. The start
    /// is the producing char's start; the end extends to the end of the
    /// original char that produced the last folded byte.
    fn original_range(&self, folded_start: usize, folded_end: usize) -> (u32, u32) {
        if self.starts.is_empty() {
            // Byte-aligned fold (ASCII fast path): offsets are identity.
            return (
                (folded_start as u32).min(self.original_len),
                (folded_end as u32).min(self.original_len),
            );
        }
        let start = self
            .starts
            .get(folded_start)
            .copied()
            .unwrap_or(self.original_len);
        if folded_end == 0 {
            return (start, start);
        }
        let last_char_start = self
            .starts
            .get(folded_end - 1)
            .copied()
            .unwrap_or(self.original_len);
        let mut end = self.original_len;
        for index in folded_end..self.starts.len() {
            if self.starts[index] != last_char_start {
                end = self.starts[index];
                break;
            }
        }
        (start, end)
    }
}

// ---- Fold arena ----

/// A parsed + folded shard, resident for reuse across queries.
pub(crate) struct LoadedShard {
    pub shard: SessionShard,
    pub folded: Vec<FoldedText>,
    /// Derived active flags, index-aligned with `records` — pure per
    /// content-named generation file, so computed once at load instead
    /// of per query (rare terms scan every session; per-query
    /// derive_active dominated their cost on the real corpus).
    pub active: Vec<bool>,
    cost_bytes: usize,
}

struct ArenaEntry {
    loaded: Arc<LoadedShard>,
    last_used: u64,
}

/// Byte-bounded LRU over resident shards, keyed by content-named
/// generation file — identical names ARE identical content, so the key
/// needs no store scoping.
#[derive(Default)]
struct Arena {
    entries: HashMap<String, ArenaEntry>,
    total_bytes: usize,
    tick: u64,
}

fn arena() -> &'static Mutex<Arena> {
    static ARENA: OnceLock<Mutex<Arena>> = OnceLock::new();
    ARENA.get_or_init(|| Mutex::new(Arena::default()))
}

/// Returns the resident shard and how many FRESH bytes this load added
/// (0 on a cache hit) — the per-query load budget counts only fresh
/// bytes. Parsing and folding run outside the lock; a concurrent
/// duplicate load resolves to whichever insert lands first.
fn arena_load(store_root: &Path, generation_file: &str) -> Option<(Arc<LoadedShard>, usize)> {
    let cache = arena();
    {
        let mut arena = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        arena.tick += 1;
        let tick = arena.tick;
        if let Some(entry) = arena.entries.get_mut(generation_file) {
            entry.last_used = tick;
            return Some((entry.loaded.clone(), 0));
        }
    }

    let raw = std::fs::read_to_string(store_root.join("generations").join(generation_file)).ok()?;
    let shard: SessionShard = serde_json::from_str(&raw).ok()?;
    let folded: Vec<FoldedText> = shard
        .records
        .iter()
        .map(|record| fold_text(&record.text))
        .collect();
    let active = derive_active(&shard.records, &shard.marks);
    let cost_bytes = raw.len()
        + folded
            .iter()
            .map(|f| f.folded.len() + f.starts.len() * 4)
            .sum::<usize>();
    let loaded = Arc::new(LoadedShard {
        shard,
        folded,
        active,
        cost_bytes,
    });

    let mut arena = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    arena.tick += 1;
    let tick = arena.tick;
    if let Some(existing) = arena.entries.get_mut(generation_file) {
        // A concurrent loader won the insert; keep its copy.
        existing.last_used = tick;
        return Some((existing.loaded.clone(), cost_bytes));
    }
    arena.total_bytes += loaded.cost_bytes;
    arena.entries.insert(
        generation_file.to_string(),
        ArenaEntry {
            loaded: loaded.clone(),
            last_used: tick,
        },
    );
    while arena.total_bytes > ARENA_BUDGET_BYTES && arena.entries.len() > 1 {
        let Some(oldest) = arena
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(evicted) = arena.entries.remove(&oldest) {
            arena.total_bytes = arena.total_bytes.saturating_sub(evicted.loaded.cost_bytes);
        }
    }
    Some((loaded, cost_bytes))
}

#[cfg(test)]
mod tests {
    use super::super::record::{Locator, MessageRecord, SupersessionMark};
    use super::super::store::PublishOutcome;
    use super::*;

    fn record(session: &str, seq: u64, ts_ms: i64, role: Role, text: &str) -> MessageRecord {
        MessageRecord {
            source: Source::Intendant,
            session_id: session.to_string(),
            role,
            ts_ms,
            text: text.to_string(),
            locator: Locator::NativeMessageId {
                message_id: format!("{session}-{seq}"),
            },
            seq: Some(seq),
            user_turn: None,
            item_id: None,
            subagent: false,
            generation: 0,
            truncated: false,
        }
    }

    fn publish(store: &Store, dir: &Path, key: &str, shard: &SessionShard) {
        let source = dir.join(format!("{}.jsonl", key.replace(':', "-")));
        std::fs::write(&source, "line\n".repeat(4)).unwrap();
        let cursor = super::super::cursor::SourceCursor::capture(&source, 5).unwrap();
        assert!(matches!(
            store
                .publish_session(key, shard, vec![cursor], false)
                .unwrap(),
            PublishOutcome::Published
        ));
    }

    fn params(q: &str) -> MessageSearchParams {
        MessageSearchParams {
            q: q.to_string(),
            ..MessageSearchParams::default()
        }
    }

    fn now() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn matches_group_order_and_snippet_ranges_into_original_text() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let base = now();
        publish(
            &store,
            tmp.path(),
            "intendant:s-old",
            &SessionShard {
                records: vec![record(
                    "s-old",
                    1,
                    base - 60_000,
                    Role::User,
                    "the Emerald payload waits",
                )],
                marks: vec![],
            },
        );
        publish(
            &store,
            tmp.path(),
            "intendant:s-new",
            &SessionShard {
                records: vec![
                    record("s-new", 1, base - 5_000, Role::User, "no match here"),
                    record(
                        "s-new",
                        2,
                        base - 2_000,
                        Role::Assistant,
                        "Chart the EMERALD payload course",
                    ),
                ],
                marks: vec![],
            },
        );

        let (status, body) = run_message_search(tmp.path(), &params("emerald payload"), base, 500);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["state"], "ready");
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        // Best-hit recency orders sessions.
        assert_eq!(sessions[0]["session_key"], "intendant:s-new");
        assert_eq!(sessions[1]["session_key"], "intendant:s-old");
        let hit = &sessions[0]["hits"][0];
        assert_eq!(hit["role"], "assistant");
        // Ranges are byte offsets into the ORIGINAL text, case preserved.
        let text = "Chart the EMERALD payload course";
        let ranges = hit["ranges"].as_array().unwrap();
        let (start, end) = (
            ranges[0][0].as_u64().unwrap() as usize,
            ranges[0][1].as_u64().unwrap() as usize,
        );
        assert_eq!(&text[start..end], "EMERALD");
        let snippet = hit["snippet"].as_str().unwrap();
        let offset = hit["snippet_offset_bytes"].as_u64().unwrap() as usize;
        assert_eq!(
            &snippet[start - offset..end - offset],
            "EMERALD",
            "client-side highlight math must land on the same bytes"
        );
        // Both terms within ONE message: the user row of s-new lacks
        // "payload", so it is not a hit.
        assert_eq!(sessions[0]["total_hits"], 1);
    }

    #[test]
    fn folding_matches_across_case_and_canonical_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let base = now();
        // Precomposed é in the haystack; decomposed e+◌́ in the needle.
        publish(
            &store,
            tmp.path(),
            "intendant:s-nfc",
            &SessionShard {
                records: vec![record(
                    "s-nfc",
                    1,
                    base - 1_000,
                    Role::User,
                    "caf\u{e9} REVIEW",
                )],
                marks: vec![],
            },
        );
        let (status, body) =
            run_message_search(tmp.path(), &params("cafe\u{301} review"), base, 500);
        assert_eq!(status, 200);
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{body}");
        let ranges = sessions[0]["hits"][0]["ranges"].as_array().unwrap();
        let text = "caf\u{e9} REVIEW";
        let (start, end) = (
            ranges[0][0].as_u64().unwrap() as usize,
            ranges[0][1].as_u64().unwrap() as usize,
        );
        assert_eq!(
            &text[start..end],
            "caf\u{e9}",
            "range covers the composed char"
        );
    }

    #[test]
    fn superseded_hits_are_badged_and_hideable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let base = now();
        publish(
            &store,
            tmp.path(),
            "intendant:s-cut",
            &SessionShard {
                records: vec![
                    record("s-cut", 1, base - 3_000, Role::User, "keep the payload"),
                    record("s-cut", 5, base - 2_000, Role::User, "cut the payload"),
                ],
                marks: vec![SupersessionMark::SeqCut {
                    cut_after_seq: 2,
                    at_ms: base - 1_000,
                }],
            },
        );

        let (_, body) = run_message_search(tmp.path(), &params("payload"), base, 500);
        let hits = body["sessions"][0]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["superseded"], true, "newest hit is the cut one");
        assert_eq!(hits[1]["superseded"], false);

        let mut hide = params("payload");
        hide.include_superseded = false;
        let (_, body) = run_message_search(tmp.path(), &hide, base, 500);
        let hits = body["sessions"][0]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["superseded"], false);
    }

    #[test]
    fn filters_sources_and_subagents() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let base = now();
        let mut sub = record("c-1", 1, base - 1_000, Role::User, "payload in a subagent");
        sub.subagent = true;
        sub.source = Source::ClaudeCode;
        publish(
            &store,
            tmp.path(),
            "claude-code:c-1",
            &SessionShard {
                records: vec![sub],
                marks: vec![],
            },
        );
        publish(
            &store,
            tmp.path(),
            "intendant:n-1",
            &SessionShard {
                records: vec![record("n-1", 1, base - 500, Role::User, "payload native")],
                marks: vec![],
            },
        );

        let mut only_native = params("payload");
        only_native.sources = parse_sources("intendant");
        let (_, body) = run_message_search(tmp.path(), &only_native, base, 500);
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(body["sessions"][0]["source"], "intendant");

        let mut no_subagents = params("payload");
        no_subagents.include_subagents = false;
        let (_, body) = run_message_search(tmp.path(), &no_subagents, base, 500);
        let keys: Vec<&str> = body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, vec!["intendant:n-1"]);
    }

    #[test]
    fn pagination_is_snapshot_bound_and_expires_on_store_change() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let base = now();
        for index in 0..5 {
            publish(
                &store,
                tmp.path(),
                &format!("intendant:s-{index}"),
                &SessionShard {
                    records: vec![record(
                        &format!("s-{index}"),
                        1,
                        base - 1_000 * (index as i64 + 1),
                        Role::User,
                        "shared payload",
                    )],
                    marks: vec![],
                },
            );
        }
        let mut page1 = params("payload");
        page1.limit = 2;
        let (_, body1) = run_message_search(tmp.path(), &page1, base, 500);
        let keys1: Vec<&str> = body1["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_key"].as_str().unwrap())
            .collect();
        assert_eq!(keys1, vec!["intendant:s-0", "intendant:s-1"]);
        let cursor = body1["cursor"].as_str().unwrap().to_string();

        let mut page2 = page1.clone();
        page2.cursor = Some(cursor.clone());
        let (_, body2) = run_message_search(tmp.path(), &page2, base, 500);
        let keys2: Vec<&str> = body2["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_key"].as_str().unwrap())
            .collect();
        assert_eq!(keys2, vec!["intendant:s-2", "intendant:s-3"]);

        // A different query cannot reuse the cursor.
        let mut wrong_query = page2.clone();
        wrong_query.q = "different".to_string();
        let (status, body) = run_message_search(tmp.path(), &wrong_query, base, 500);
        assert_eq!(status, 400, "{body}");

        // Any store change between pages expires the cursor.
        publish(
            &store,
            tmp.path(),
            "intendant:s-9",
            &SessionShard {
                records: vec![record("s-9", 1, base, Role::User, "shared payload")],
                marks: vec![],
            },
        );
        let (status, body) = run_message_search(tmp.path(), &page2, base, 500);
        assert_eq!(status, 410);
        assert_eq!(body["error"], "cursor_expired");
    }

    #[test]
    fn query_limits_are_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let _store = Store::open(tmp.path()).unwrap();
        let (status, _) = run_message_search(tmp.path(), &params(""), now(), 100);
        assert_eq!(status, 400);
        let (status, _) = run_message_search(tmp.path(), &params(&"x".repeat(300)), now(), 100);
        assert_eq!(status, 400);
        let (status, _) = run_message_search(tmp.path(), &params("a b c d e f g h i"), now(), 100);
        assert_eq!(status, 400);
    }

    #[test]
    fn empty_store_reports_building() {
        let tmp = tempfile::tempdir().unwrap();
        let _store = Store::open(tmp.path()).unwrap();
        let (status, body) = run_message_search(tmp.path(), &params("anything"), now(), 100);
        assert_eq!(status, 200);
        assert_eq!(body["state"], "building");
        assert!(body["sessions"].as_array().unwrap().is_empty());
        assert!(body["coverage"]["legacy"]["ask_human_answers"]
            .as_str()
            .unwrap()
            .contains("never persisted"));
    }

    #[test]
    fn retention_window_filters_old_hits_at_query_time() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let base = now();
        publish(
            &store,
            tmp.path(),
            "intendant:s-mixed",
            &SessionShard {
                records: vec![
                    record(
                        "s-mixed",
                        1,
                        base - RETENTION_MS - 1_000,
                        Role::User,
                        "old payload",
                    ),
                    record("s-mixed", 2, base - 1_000, Role::User, "fresh payload"),
                ],
                marks: vec![],
            },
        );
        let (_, body) = run_message_search(tmp.path(), &params("payload"), base, 500);
        let hits = body["sessions"][0]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0]["snippet"].as_str().unwrap().contains("fresh"));
    }
}
