//! Fingerprint caches + tail-windowed scan for session-log replay.
//!
//! Before this tier, every websocket bootstrap converted the ENTIRE
//! `session.jsonl` (3-4 full parse passes plus per-entry side-file reads)
//! and then kept only the last ~250 entries, and every session-detail
//! page re-converted the whole log to slice one page. Two caches fix the
//! repeat cost and a tail scan fixes the first-touch cost:
//!
//! - the FULL-ENTRIES cache: converted (compacted) browser entries per
//!   log dir, keyed by `session.jsonl`'s stat fingerprint — detail pages
//!   become cheap slices, full replays become clones of an `Arc`;
//! - the BOOTSTRAP cache: the prepared, windowed `log_replay` payload per
//!   (log dir, limit), keyed by the same fingerprint;
//! - the TAIL SCAN: for logs too large to full-parse per connect, one
//!   cheap substring-prefiltered pass collects the header/pinned-kind
//!   facts (status, identity, relationships, capabilities, latest goals,
//!   legacy model-response spans) and conversion runs in REVERSE only
//!   until the window is full — side files are read for the window, not
//!   for the whole history.
//!
//! Everything here is sync (callers wrap in `spawn_blocking` at the
//! transport edges), guarded by a per-log-dir single-flight lock so
//! concurrent dashboard connects convert a log once, not once each.

use super::*;

/// Logs at or under this size take the exact full-parse path on a
/// bootstrap cache miss (identical output by construction); larger logs
/// take the tail scan.
pub(crate) const REPLAY_FULL_PARSE_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Full-entries cache admission gates: converted entries inline the turn
/// side files, so resident size tracks `session.jsonl` PLUS `turns/`.
/// Dirs beyond the gates still compute (callers keep today's per-request
/// cost) — they just are not retained.
pub(crate) const REPLAY_ENTRIES_CACHE_MAX_JSONL_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const REPLAY_ENTRIES_CACHE_MAX_TURNS_BYTES: u64 = 48 * 1024 * 1024;
pub(crate) const REPLAY_ENTRIES_CACHE_MAX_DIRS: usize = 4;

pub(crate) const BOOTSTRAP_REPLAY_CACHE_MAX_ENTRIES: usize = 8;

/// Stat identity of a `session.jsonl` (same fields as the session-list
/// cache keys): any append/rewrite/replace moves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionJsonlFingerprint {
    len: u64,
    mtime_nanos: u128,
    ctime_nanos: i128,
    dev: u64,
    ino: u64,
}

pub(crate) fn session_jsonl_fingerprint(log_dir: &Path) -> Option<SessionJsonlFingerprint> {
    let metadata = std::fs::metadata(log_dir.join("session.jsonl")).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let (dev, ino) = crate::platform::metadata_dev_ino(&metadata);
    Some(SessionJsonlFingerprint {
        len: metadata.len(),
        mtime_nanos: metadata_mtime_nanos(&metadata),
        ctime_nanos: metadata_ctime_nanos(&metadata),
        dev,
        ino,
    })
}

struct ReplayEntriesCacheEntry {
    fingerprint: SessionJsonlFingerprint,
    entries: Arc<Vec<serde_json::Value>>,
    external_session_id: Option<String>,
}

fn replay_entries_cache() -> &'static Mutex<HashMap<String, ReplayEntriesCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, ReplayEntriesCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

struct BootstrapReplayCacheEntry {
    fingerprint: SessionJsonlFingerprint,
    payload: String,
    external_session_id: Option<String>,
}

fn bootstrap_replay_cache() -> &'static Mutex<HashMap<(String, usize), BootstrapReplayCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, usize), BootstrapReplayCacheEntry>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-log-dir single-flight: concurrent bootstraps/pages of one session
/// serialize on its lock and all but the first hit the caches.
fn replay_flight_lock(dir_key: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|e| e.into_inner());
    if locks.len() >= 64 && !locks.contains_key(dir_key) {
        // Dropping idle lock handles is safe: holders keep their Arc.
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    locks
        .entry(dir_key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn turns_dir_total_bytes(log_dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(log_dir.join("turns")) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

fn replay_entries_cache_admits(log_dir: &Path, fingerprint: &SessionJsonlFingerprint) -> bool {
    fingerprint.len <= REPLAY_ENTRIES_CACHE_MAX_JSONL_BYTES
        && turns_dir_total_bytes(log_dir) <= REPLAY_ENTRIES_CACHE_MAX_TURNS_BYTES
}

fn lookup_replay_entries_cache(
    dir_key: &str,
    fingerprint: &SessionJsonlFingerprint,
) -> Option<(Arc<Vec<serde_json::Value>>, Option<String>)> {
    let cache = replay_entries_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .get(dir_key)
        .filter(|entry| &entry.fingerprint == fingerprint)
        .map(|entry| (entry.entries.clone(), entry.external_session_id.clone()))
}

fn store_replay_entries_cache(
    dir_key: String,
    fingerprint: SessionJsonlFingerprint,
    entries: Arc<Vec<serde_json::Value>>,
    external_session_id: Option<String>,
) {
    let mut cache = replay_entries_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= REPLAY_ENTRIES_CACHE_MAX_DIRS && !cache.contains_key(&dir_key) {
        cache.clear();
    }
    cache.insert(
        dir_key,
        ReplayEntriesCacheEntry {
            fingerprint,
            entries,
            external_session_id,
        },
    );
}

/// Converted (and context-compacted) replay entries for a log dir, from
/// the full-entries cache when current. The fingerprint is taken BEFORE
/// the read: an append racing the read over-invalidates (next call
/// recomputes) instead of pinning a torn view.
pub(crate) fn cached_session_log_replay_entries(
    log_dir: &Path,
) -> Option<(Arc<Vec<serde_json::Value>>, Option<String>)> {
    let dir_key = session_list_path_key(log_dir);
    let flight = replay_flight_lock(&dir_key);
    let _guard = flight.lock().unwrap_or_else(|e| e.into_inner());
    let fingerprint = session_jsonl_fingerprint(log_dir)?;
    if let Some(hit) = lookup_replay_entries_cache(&dir_key, &fingerprint) {
        return Some(hit);
    }
    let (entries, external_session_id) =
        compute_full_replay_entries(log_dir, &dir_key, fingerprint)?;
    Some((entries, external_session_id))
}

/// The full conversion (single source of truth: the legacy pipeline),
/// compacted, stored when the dir passes the admission gates. Assumes the
/// caller holds the dir's flight lock.
fn compute_full_replay_entries(
    log_dir: &Path,
    dir_key: &str,
    fingerprint: SessionJsonlFingerprint,
) -> Option<(Arc<Vec<serde_json::Value>>, Option<String>)> {
    let (mut entries, external_session_id) = session_log_replay_entries_from_dir(log_dir)?;
    compact_context_snapshot_entries_for_replay(&mut entries);
    let entries = Arc::new(entries);
    if replay_entries_cache_admits(log_dir, &fingerprint) {
        store_replay_entries_cache(
            dir_key.to_string(),
            fingerprint,
            entries.clone(),
            external_session_id.clone(),
        );
    }
    Some((entries, external_session_id))
}

/// The windowed `log_replay` payload for a bootstrap-style caller, from
/// the bootstrap cache when current; on a miss, from the full-entries
/// cache/pipeline (small logs) or the tail scan (large logs).
pub(crate) fn cached_bootstrap_replay_payload(
    log_dir: &Path,
    limit: usize,
) -> Option<(String, Option<String>)> {
    let dir_key = session_list_path_key(log_dir);
    let flight = replay_flight_lock(&dir_key);
    let _guard = flight.lock().unwrap_or_else(|e| e.into_inner());
    let fingerprint = session_jsonl_fingerprint(log_dir)?;
    {
        let cache = bootstrap_replay_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache
            .get(&(dir_key.clone(), limit))
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            return Some((entry.payload.clone(), entry.external_session_id.clone()));
        }
    }

    // Current full-entries conversion (if any consumer already paid for
    // it) serves the window without another parse.
    let (payload, external_session_id) = if let Some((entries, external_session_id)) =
        lookup_replay_entries_cache(&dir_key, &fingerprint)
    {
        let prepared = prepare_websocket_bootstrap_replay_entries_ref(&entries, limit);
        (replay_payload_string(&prepared), external_session_id)
    } else if fingerprint.len <= REPLAY_FULL_PARSE_MAX_BYTES {
        let (entries, external_session_id) =
            compute_full_replay_entries(log_dir, &dir_key, fingerprint.clone())?;
        let prepared = prepare_websocket_bootstrap_replay_entries_ref(&entries, limit);
        (replay_payload_string(&prepared), external_session_id)
    } else {
        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).ok()?;
        let (entries, external_session_id) =
            bootstrap_entries_via_tail_scan(&contents, log_dir, limit);
        (replay_payload_string(&entries), external_session_id)
    };

    let mut cache = bootstrap_replay_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let key = (dir_key, limit);
    if cache.len() >= BOOTSTRAP_REPLAY_CACHE_MAX_ENTRIES && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(
        key,
        BootstrapReplayCacheEntry {
            fingerprint,
            payload: payload.clone(),
            external_session_id: external_session_id.clone(),
        },
    );
    Some((payload, external_session_id))
}

// ---------------------------------------------------------------------
// The tail scan
// ---------------------------------------------------------------------

/// Facts the cheap forward pass collects. Substring prefilters route
/// lines to `serde_json` only when they can possibly matter; a prefilter
/// can false-positive (the parse then no-ops, exactly like the full
/// pipeline's per-line checks) but never false-negative, because every
/// needle is a literal substring of the serialized row it targets.
#[derive(Default)]
struct BootstrapCheapScan<'a> {
    provider: Option<String>,
    model: Option<String>,
    autonomy: Option<String>,
    /// Resolved external (source, backend id) — the first
    /// `session_identity` row naming a non-intendant source, or the first
    /// completed message-scrape pair, whichever a forward scan hits
    /// first (mirrors `external_backend_session_from_replay`).
    external_session: Option<(String, String)>,
    scrape_source: Option<String>,
    scrape_id: Option<String>,
    /// Legacy (span-less) model_response rows per turn file, in line
    /// order: (line index, recorded content_length).
    legacy_by_file: HashMap<String, Vec<(usize, u64)>>,
    /// Pinned-kind rows (session_identity/relationship/capabilities),
    /// as (line index, raw line) so later conversion needs no re-seek.
    pinned_lines: Vec<(usize, &'a str)>,
    /// Every session_goal row (latest-per-session picked after
    /// conversion, on the converted entries' session keys).
    goal_lines: Vec<(usize, &'a str)>,
}

const STATUS_NEEDLES: [&str; 3] = ["Provider: ", "Model: ", "Autonomy: "];
const IDENTITY_NEEDLES: [&str; 3] = [
    "session_identity",
    "Mode: external agent",
    "External agent thread: ",
];
const PINNED_NEEDLES: [&str; 3] = [
    "session_identity",
    "session_relationship",
    "session_capabilities",
];

fn scan_line_for_status(scan: &mut BootstrapCheapScan<'_>, value: &serde_json::Value) {
    let ev = value.get("event").and_then(|x| x.as_str()).unwrap_or("");
    if !matches!(ev, "info" | "debug" | "warn" | "error") {
        return;
    }
    let Some(msg) = value.get("message").and_then(|x| x.as_str()) else {
        return;
    };
    if scan.provider.is_none() {
        if let Some(rest) = msg.strip_prefix("Provider: ") {
            scan.provider = Some(rest.split_whitespace().next().unwrap_or("").to_string());
        }
    }
    if scan.model.is_none() {
        if let Some(rest) = msg.strip_prefix("Model: ") {
            scan.model = Some(rest.to_string());
        }
    }
    if scan.autonomy.is_none() {
        if let Some(rest) = msg.strip_prefix("Autonomy: ") {
            scan.autonomy = Some(rest.to_string());
        }
    }
}

fn scan_line_for_external_identity(scan: &mut BootstrapCheapScan<'_>, value: &serde_json::Value) {
    if scan.external_session.is_some() {
        return;
    }
    if value.get("event").and_then(|v| v.as_str()) == Some("session_identity") {
        let data = value.get("data");
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
                scan.external_session = Some((source, id));
                return;
            }
        }
    }
    if let Some(message) = value.get("message").and_then(|v| v.as_str()) {
        if scan.scrape_source.is_none() {
            scan.scrape_source = external_agent_source_from_message(message);
        }
        if scan.scrape_id.is_none() {
            scan.scrape_id = external_agent_thread_id_from_message(message);
        }
        if let (Some(source), Some(id)) = (scan.scrape_source.as_ref(), scan.scrape_id.as_ref()) {
            scan.external_session = Some((source.clone(), id.clone()));
        }
    }
}

fn bootstrap_cheap_scan(contents: &str) -> (BootstrapCheapScan<'_>, usize) {
    let mut scan = BootstrapCheapScan::default();
    let mut total_lines = 0usize;
    for (line_no, raw_line) in contents.lines().enumerate() {
        total_lines = line_no + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let status_pending =
            scan.provider.is_none() || scan.model.is_none() || scan.autonomy.is_none();
        let wants_status =
            status_pending && STATUS_NEEDLES.iter().any(|needle| line.contains(needle));
        // Identity scraping stops exactly where the original scan
        // returns: once the pair is resolved, later rows can't change it
        // (and the id-only fallback below the pair is first-wins too).
        let wants_identity = scan.external_session.is_none()
            && IDENTITY_NEEDLES.iter().any(|needle| line.contains(needle));
        let wants_legacy = line.contains("model_response");
        let wants_pinned = PINNED_NEEDLES.iter().any(|needle| line.contains(needle));
        let wants_goal = line.contains("session_goal");
        if !(wants_status || wants_identity || wants_legacy || wants_pinned || wants_goal) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if wants_status {
            scan_line_for_status(&mut scan, &value);
        }
        if wants_identity {
            scan_line_for_external_identity(&mut scan, &value);
        }
        if wants_legacy {
            if let Some((rel, len)) = legacy_model_response_file_and_len(&value) {
                scan.legacy_by_file
                    .entry(rel)
                    .or_default()
                    .push((line_no, len));
            }
        }
        if wants_pinned
            && value
                .get("event")
                .and_then(|v| v.as_str())
                .is_some_and(|event| PINNED_NEEDLES.contains(&event))
        {
            scan.pinned_lines.push((line_no, raw_line));
        }
        if wants_goal && value.get("event").and_then(|v| v.as_str()) == Some("session_goal") {
            scan.goal_lines.push((line_no, raw_line));
        }
    }
    (scan, total_lines)
}

/// `validated_legacy_model_response_spans` over the cheap scan's
/// collected lengths: same all-rows-sum-to-file-length validation, same
/// span layout.
fn validated_spans_from_scan(
    scan: &BootstrapCheapScan,
    log_dir: &Path,
) -> HashMap<String, Vec<(u64, u64)>> {
    let mut spans_by_file = HashMap::new();
    for (rel, rows) in &scan.legacy_by_file {
        let Ok(meta) = std::fs::metadata(log_dir.join(rel)) else {
            continue;
        };
        let mut expected_len = rows.len().saturating_sub(1) as u64;
        let mut overflowed = false;
        for (_, len) in rows {
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
        let mut spans = Vec::with_capacity(rows.len());
        for (_, len) in rows {
            spans.push((offset, *len));
            offset = offset.saturating_add(*len).saturating_add(1);
        }
        spans_by_file.insert(rel.clone(), spans);
    }
    spans_by_file
}

struct TailConvertCtx<'a> {
    log_dir: &'a Path,
    spans_by_file: HashMap<String, Vec<(u64, u64)>>,
    legacy_by_file: HashMap<String, Vec<(usize, u64)>>,
    replay_session_id: Option<String>,
    external_replay_session_id: Option<String>,
    wrapper_replay_session_id: Option<String>,
}

impl TailConvertCtx<'_> {
    /// One line → one outbound replay entry, matching the full
    /// pipeline's per-line work: legacy span inference (at this line's
    /// file-wide index), conversion, context compaction, metadata.
    fn convert_line(&self, line_no: usize, raw_line: &str) -> Option<serde_json::Value> {
        let line = raw_line.trim();
        if line.is_empty() {
            return None;
        }
        let mut entry_json = serde_json::from_str::<serde_json::Value>(line).ok()?;
        if let Some((rel, _len)) = legacy_model_response_file_and_len(&entry_json) {
            let span = self.spans_by_file.get(&rel).and_then(|spans| {
                let index = self
                    .legacy_by_file
                    .get(&rel)?
                    .binary_search_by_key(&line_no, |(no, _)| *no)
                    .ok()?;
                spans.get(index).copied()
            });
            if let Some((offset, len)) = span {
                if let Some(data) = entry_json
                    .get_mut("data")
                    .and_then(|value| value.as_object_mut())
                {
                    data.insert("model_offset".to_string(), serde_json::Value::from(offset));
                    data.insert("model_bytes".to_string(), serde_json::Value::from(len));
                }
            }
        }
        let app_event =
            crate::session_log::session_log_entry_to_app_event(&entry_json, self.log_dir)?;
        let outbound = crate::event::app_event_to_outbound(&app_event)?;
        let mut value = serde_json::to_value(&outbound).ok()?;
        compact_context_snapshot_raw_for_replay(&mut value);
        inject_replay_entry_metadata(
            &mut value,
            &entry_json,
            self.replay_session_id.as_deref(),
            self.external_replay_session_id.as_deref(),
            self.wrapper_replay_session_id.as_deref(),
        );
        Some(value)
    }
}

/// Whether a raw line is a `context_snapshot` row — those are dropped by
/// the bootstrap preparation wholesale, so the tail path skips them
/// BEFORE conversion (their side files are never read).
fn raw_line_is_context_snapshot(line: &str) -> bool {
    line.contains("context_snapshot")
        && serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .is_some_and(|value| {
                value.get("event").and_then(|v| v.as_str()) == Some("context_snapshot")
            })
}

/// Bootstrap entries for a large log without full conversion: cheap
/// forward scan for header/pinned facts, reverse conversion until the
/// window fills, then the SAME preparation finisher the full path uses.
/// Returns `(prepared entries, external_session_id)` — the payload
/// equivalent of `session_log_replay_entries_from_dir` +
/// `prepare_websocket_bootstrap_replay_entries`.
pub(crate) fn bootstrap_entries_via_tail_scan(
    contents: &str,
    log_dir: &Path,
    limit: usize,
) -> (Vec<serde_json::Value>, Option<String>) {
    let window_limit = limit.clamp(1, SESSION_DETAIL_ENTRY_LIMIT_MAX);
    let (scan, total_lines) = bootstrap_cheap_scan(contents);

    let wrapper_replay_session_id = replay_session_id_from_dir(log_dir);
    let external_pair_id = scan.external_session.as_ref().map(|(_, id)| id.clone());
    let replay_session_id = external_pair_id
        .clone()
        .or_else(|| wrapper_replay_session_id.clone());
    // The RETURNED id keeps the message-scrape fallback the legacy
    // `external_backend_session_id_from_replay` applied on top of the
    // resolved pair.
    let external_session_id = external_pair_id.clone().or_else(|| scan.scrape_id.clone());

    let ctx = TailConvertCtx {
        log_dir,
        spans_by_file: validated_spans_from_scan(&scan, log_dir),
        legacy_by_file: scan.legacy_by_file.clone(),
        replay_session_id: replay_session_id.clone(),
        external_replay_session_id: external_pair_id.clone(),
        wrapper_replay_session_id: wrapper_replay_session_id.clone(),
    };

    // Reverse fill: newest lines first, counting every converted
    // (non-context-snapshot) entry toward the window, exactly as the
    // full path's window counts the last N ctx-free entries.
    let mut converted: BTreeMap<usize, serde_json::Value> = BTreeMap::new();
    let mut window_count = 0usize;
    for (rev_offset, raw_line) in contents.lines().rev().enumerate() {
        if window_count >= window_limit {
            break;
        }
        let line_no = total_lines - 1 - rev_offset;
        let line = raw_line.trim();
        if line.is_empty() || raw_line_is_context_snapshot(line) {
            continue;
        }
        if let Some(value) = ctx.convert_line(line_no, raw_line) {
            converted.insert(line_no, value);
            window_count += 1;
        }
    }

    // Pinned kinds + goals from anywhere in the file (the window's own
    // rows are already converted; dedup by line index).
    for (line_no, raw_line) in scan.pinned_lines.iter().chain(scan.goal_lines.iter()) {
        if converted.contains_key(line_no) {
            continue;
        }
        if let Some(value) = ctx.convert_line(*line_no, raw_line) {
            converted.insert(*line_no, value);
        }
    }

    // Header entries, exactly as the full pipeline synthesizes them.
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(converted.len() + 2);
    entries.push(serde_json::json!({
        "event": "replay_start",
        "provider": scan.provider,
        "model": scan.model,
        "autonomy": scan.autonomy,
        "event_id": format!(
            "session-log:{}:replay_start",
            replay_session_id.as_deref().unwrap_or("unknown")
        ),
        "delivery": "state",
    }));
    if let (Some((source, backend_session_id)), Some(wrapper_session_id)) = (
        scan.external_session.as_ref(),
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
    entries.extend(converted.into_values());

    if let Some((source, session_id)) = scan.external_session.as_ref() {
        let home = home_from_intendant_log_dir(log_dir).unwrap_or_else(crate::platform::home_dir);
        annotate_replay_user_turns_from_external_transcript(
            &mut entries,
            &home,
            source,
            session_id,
        );
    }

    (
        prepare_websocket_bootstrap_replay_entries(entries, limit),
        external_session_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The legacy pipeline verbatim (full conversion → prepare →
    /// payload), as the equality oracle for the cached/tail paths.
    fn bootstrap_payload_via_full_pipeline(
        log_dir: &Path,
        limit: usize,
    ) -> Option<(String, Option<String>)> {
        let (mut entries, external_session_id) = session_log_replay_entries_from_dir(log_dir)?;
        entries = prepare_websocket_bootstrap_replay_entries(entries, limit);
        compact_context_snapshot_entries_for_replay(&mut entries);
        Some((replay_payload_string(&entries), external_session_id))
    }

    fn tail_payload(log_dir: &Path, limit: usize) -> (String, Option<String>) {
        let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let (entries, external_session_id) =
            bootstrap_entries_via_tail_scan(&contents, log_dir, limit);
        (replay_payload_string(&entries), external_session_id)
    }

    /// A busy synthetic modern log: status header, identity/relationship/
    /// capabilities pinned rows early, goals early AND late per session,
    /// context snapshots, and far more entries than the window.
    fn write_busy_log(log_dir: &Path) {
        let mut log = crate::session_log::SessionLog::open(log_dir.to_path_buf()).unwrap();
        log.info("Provider: openai");
        log.info("Model: gpt-5");
        log.info("Autonomy: Medium");
        log.session_started("session", Some("busy task"));
        log.session_relationship("session", "child-1", "subagent", false);
        log.session_goal(
            "session",
            Some(&crate::types::SessionGoal {
                objective: "early goal".to_string(),
                ..Default::default()
            }),
        );
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
        for turn in 0..40 {
            log.turn_start(turn, 0.5, 90_000);
            let _ = log.model_response(&format!("model response {turn}"), 1, 2, 3, 0, 0, None);
            log.auto_approved(&format!("exec: step {turn}"));
            log.agent_output_with_id(
                &format!("stdout for turn {turn}"),
                "",
                None,
                Some(&format!("out-{turn}")),
            );
            log.round_complete(turn, 1);
        }
        log.session_goal(
            "session",
            Some(&crate::types::SessionGoal {
                objective: "latest goal".to_string(),
                ..Default::default()
            }),
        );
        drop(log);
    }

    #[test]
    fn tail_scan_bootstrap_matches_full_pipeline_on_modern_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("busy");
        write_busy_log(&log_dir);

        for limit in [5, 25, 250] {
            let full = bootstrap_payload_via_full_pipeline(&log_dir, limit).unwrap();
            let tail = tail_payload(&log_dir, limit);
            assert_eq!(
                tail.0, full.0,
                "tail-window payload must equal full-parse payload (limit {limit})"
            );
            assert_eq!(tail.1, full.1);
        }
    }

    #[test]
    fn tail_scan_bootstrap_matches_full_pipeline_on_external_wrapper_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("wrapper");
        let backend_id = "019e598b-256e-7b61-8816-22908ece438a";
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.session_started("wrapper", Some("external task"));
        log.session_identity("wrapper", "codex", backend_id);
        log.info("Mode: external agent (Codex)");
        log.debug(&format!("External agent thread: {backend_id}"));
        for idx in 0..30 {
            log.info(&format!("[user] instruction {idx}"));
            let _ = log.model_response(
                &format!("external reply {idx}"),
                1,
                2,
                3,
                0,
                0,
                Some("Codex"),
            );
        }
        drop(log);

        for limit in [4, 250] {
            let full = bootstrap_payload_via_full_pipeline(&log_dir, limit).unwrap();
            let tail = tail_payload(&log_dir, limit);
            assert_eq!(tail.0, full.0, "external wrapper (limit {limit})");
            assert_eq!(tail.1, full.1);
        }
    }

    #[test]
    fn tail_scan_matches_full_pipeline_on_legacy_spanless_log() {
        // Legacy shape: model_response rows without model_offset/bytes,
        // sharing one appended turn file — the span inference must
        // validate + index identically from the cheap scan.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("legacy");
        std::fs::create_dir_all(log_dir.join("turns")).unwrap();
        std::fs::write(
            log_dir.join("turns/turn_000_model.txt"),
            "first response\nsecond response",
        )
        .unwrap();
        let mut lines = Vec::new();
        for (idx, text) in ["first response", "second response"].iter().enumerate() {
            lines.push(
                serde_json::json!({
                    "ts": format!("01:00:0{idx}.000"),
                    "turn": 0,
                    "event": "model_response",
                    "level": "info",
                    "message": text,
                    "file": "turns/turn_000_model.txt",
                    "data": {
                        "content_length": text.len(),
                        "tokens": {"prompt": 1, "completion": 2, "total": 3, "cached": 0}
                    },
                })
                .to_string(),
            );
        }
        std::fs::write(log_dir.join("session.jsonl"), lines.join("\n") + "\n").unwrap();

        // Window smaller than the row count: the in-window legacy row
        // must still get ITS OWN span (index 1), not index 0.
        let full = bootstrap_payload_via_full_pipeline(&log_dir, 1).unwrap();
        let tail = tail_payload(&log_dir, 1);
        assert_eq!(tail.0, full.0);
        assert!(tail.0.contains("second response"));
    }

    #[test]
    fn cached_bootstrap_payload_matches_and_invalidates_on_append() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("cache-a");
        write_busy_log(&log_dir);

        let full = bootstrap_payload_via_full_pipeline(&log_dir, 25).unwrap();
        let first = cached_bootstrap_replay_payload(&log_dir, 25).unwrap();
        assert_eq!(first.0, full.0);
        assert_eq!(first.1, full.1);
        // Cache hit: identical.
        let second = cached_bootstrap_replay_payload(&log_dir, 25).unwrap();
        assert_eq!(second.0, first.0);

        // Append a new row; the fingerprint moves and the payload must
        // pick the new tail up.
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        let _ = log.model_response("freshly appended response", 1, 2, 3, 0, 0, None);
        drop(log);
        let third = cached_bootstrap_replay_payload(&log_dir, 25).unwrap();
        assert_ne!(third.0, second.0);
        assert!(third.0.contains("freshly appended response"));
        assert_eq!(
            third.0,
            bootstrap_payload_via_full_pipeline(&log_dir, 25).unwrap().0
        );
    }

    #[test]
    fn cached_full_entries_match_uncached_and_page_slices_cheaply() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join(".intendant").join("logs").join("cache-b");
        write_busy_log(&log_dir);

        let (uncached, _) = {
            let (mut entries, external) = session_log_replay_entries_from_dir(&log_dir).unwrap();
            compact_context_snapshot_entries_for_replay(&mut entries);
            (entries, external)
        };
        let (cached, _) = cached_session_log_replay_entries(&log_dir).unwrap();
        assert_eq!(cached.as_slice(), uncached.as_slice());

        // Page slicing over the cached Arc equals the by-value pager.
        let by_value = session_detail_page_entries(uncached.clone(), Some(10), Some(40));
        let by_ref = session_detail_page_entries_ref(&cached, Some(10), Some(40));
        assert_eq!(by_ref.entries, by_value.entries);
        assert_eq!(by_ref.total_entries, by_value.total_entries);
        assert_eq!(by_ref.page_start, by_value.page_start);
        assert_eq!(by_ref.page_end, by_value.page_end);

        // Append → fingerprint moves → cached tier refreshes.
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.info("appended after cache fill");
        drop(log);
        let (refreshed, _) = cached_session_log_replay_entries(&log_dir).unwrap();
        assert!(refreshed.len() > cached.len());
        assert!(refreshed
            .iter()
            .any(|entry| entry.get("content").and_then(|v| v.as_str())
                == Some("appended after cache fill")));
    }
}
