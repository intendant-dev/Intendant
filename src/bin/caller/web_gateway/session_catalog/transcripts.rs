//! External transcripts: cache slots + stable event ids, codex thread
//! projection, per-backend transcript entry parsing, activity replay, and
//! external context-snapshot replay assembly.

use super::*;

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
) -> Option<std::sync::Arc<Vec<serde_json::Value>>> {
    let slot = external_transcript_cache_slot(key);
    let cache = external_transcript_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .get(&slot)
        .filter(|entry| &entry.key == key)
        .map(|entry| std::sync::Arc::clone(&entry.entries))
}

/// Byte-admission gate: parses of sources beyond this size are served but
/// never cached. 32 unbounded slots otherwise pin several GB once a few
/// multi-hundred-MB rollouts cycle through (parsed `Value`s outweigh
/// their source bytes), and whales evict every warm small entry.
pub(crate) const EXTERNAL_TRANSCRIPT_CACHE_MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn store_external_transcript_entries(
    key: ExternalTranscriptCacheKey,
    entries: &std::sync::Arc<Vec<serde_json::Value>>,
) {
    if key.len > EXTERNAL_TRANSCRIPT_CACHE_MAX_SOURCE_BYTES {
        return;
    }
    // Admission re-check AFTER the parse: the key was stat'd before it,
    // and a live writer can grow the file past the cap (or past the
    // keyed snapshot) while we parse — caching that parse would pin an
    // oversized vec under a key no future lookup matches. Only cache
    // when the file still IS the keyed snapshot. (A metadata-preserving
    // replacement between parse and re-stat — same len AND mtime with
    // different bytes — is outside this cache's benign-writer model,
    // like every (len, mtime)-keyed cache in the catalog.)
    match std::fs::metadata(Path::new(&key.path)) {
        Ok(metadata)
            if metadata.len() == key.len && metadata_mtime_nanos(&metadata) == key.mtime_nanos => {}
        _ => return,
    }
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
            entries: std::sync::Arc::clone(entries),
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

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) fn push_codex_transcript_message(
    entries: &mut Vec<serde_json::Value>,
    user_turn_revisions: &mut ReplayUserTurnRevisionState,
    steer_cursor: &mut ExternalSteerCursor<'_>,
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
        // Mid-turn steers are user rows the wrapper never counted: the
        // live lane logged them WITHOUT turn metadata (the `turn/steer`
        // accept path), so hydration must render them the same way and
        // must not burn a turn index — otherwise every later prompt
        // drifts out of alignment with the wrapper's round counter (and
        // the frontend's text-signature dedupe bridge with it).
        let rendered_content = entries
            .last()
            .and_then(|entry| entry.get("content"))
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let is_mid_turn_steer = steer_cursor
            .try_consume_mid_turn_steer(&rendered_content, timestamp_millis_from_str(ts));
        if !is_mid_turn_steer {
            let (recorded_turn_index, recorded_turn_revision) =
                user_turn_revisions.record_next_turn();
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

pub(crate) fn parse_codex_session_entries(
    path: &Path,
    steers: &ExternalSteerLedger,
) -> Option<Vec<serde_json::Value>> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut user_turn_revisions = ReplayUserTurnRevisionState::default();
    let mut steer_cursor = steers.cursor();
    let mut pending_replacement_for_user_turn: Option<u32> = None;
    let mut rollout_session_id: Option<String> = None;
    let mut current_turn_id: Option<String> = None;
    let mut synthetic_item_seq = 0_u64;
    let mut command_calls: HashMap<String, serde_json::Value> = HashMap::new();
    // One combined probe pass (early-exits once both lanes are proven)
    // instead of two independent full-file scans before the main parse.
    let (canonical_user_message_events, canonical_assistant_response_items) =
        codex_session_canonical_lanes(path);
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
                    &mut steer_cursor,
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
                    &mut steer_cursor,
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
            // Reasoning response_items: surface the chain-of-thought as the
            // same first-class thinking row the live lane emits (level model
            // + kind reasoning, raw text via the same extractor the reader
            // uses) so window hydration dedupes against streamed rows.
            if payload.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
                if let Some(text) = crate::external_agent::codex::extract_reasoning_text(payload)
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
                {
                    let mut entry = serde_json::json!({
                        "ts": ts,
                        "level": "model",
                        "source": "codex",
                        "kind": "reasoning",
                        "content": text,
                    });
                    if let Some(session_id) = rollout_session_id.as_deref() {
                        entry["session_id"] = serde_json::json!(session_id);
                    }
                    entries.push(entry);
                    let reasoning_turn_id = current_turn_id
                        .clone()
                        .unwrap_or_else(|| "turn-unknown".to_string());
                    let reasoning_item_id = response_item_id.clone().unwrap_or_else(|| {
                        codex_next_synthetic_item_id(
                            &mut synthetic_item_seq,
                            &reasoning_turn_id,
                            "reasoning",
                        )
                    });
                    if let Some(entry) = entries.last_mut() {
                        apply_codex_thread_projection(
                            entry,
                            &reasoning_item_id,
                            "reasoning",
                            &reasoning_turn_id,
                            None,
                            None,
                        );
                    }
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

/// Whole-file facts the codex projection needs before line 1:
/// `(has canonical user_message events, has canonical assistant
/// response_items)`. One scan answers both — the two independent probe
/// passes each read the rollout to EOF whenever their answer was "no" —
/// and it stops as soon as both lanes are proven.
pub(crate) fn codex_session_canonical_lanes(path: &Path) -> (bool, bool) {
    let Ok(file) = std::fs::File::open(path) else {
        return (false, false);
    };
    let reader = std::io::BufReader::new(file);
    let mut has_user_message_events = false;
    let mut has_assistant_response_items = false;
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Substring prefilters keep the JSON parse off unrelated lines,
        // exactly as the split probes did.
        let user_candidate = !has_user_message_events && trimmed.contains("\"user_message\"");
        let assistant_candidate =
            !has_assistant_response_items && trimmed.contains("\"assistant\"");
        if !user_candidate && !assistant_candidate {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if user_candidate
            && obj.get("type").and_then(|v| v.as_str()) == Some("event_msg")
            && obj
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(|v| v.as_str())
                == Some("user_message")
        {
            has_user_message_events = true;
        }
        if assistant_candidate && obj.get("type").and_then(|v| v.as_str()) == Some("response_item")
        {
            if let Some(payload) = obj.get("payload") {
                if payload.get("type").and_then(|v| v.as_str()) == Some("message")
                    && payload.get("role").and_then(|v| v.as_str()) == Some("assistant")
                    && codex_payload_text(payload).is_some()
                {
                    has_assistant_response_items = true;
                }
            }
        }
        if has_user_message_events && has_assistant_response_items {
            break;
        }
    }
    (has_user_message_events, has_assistant_response_items)
}

/// Rebuild Activity entries from Claude Code's native `~/.claude` session
/// JSONL, STRUCTURALLY — mirroring the live event shapes (level / source /
/// kind / item_id) so hydration renders what the live feed rendered and the
/// transcript-sync dedupe (`sessionWindowTranscriptSignatures*`) collapses
/// the two instead of duplicating rows.
///
/// User prompt rows additionally carry `user_turn_index`/
/// `user_turn_revision` counted per transcript line, exactly like the
/// Codex parser (`push_codex_transcript_message`): the live
/// `UserMessageLog` row logs the prompt with the wrapper's turn metadata
/// and the DISPATCH-time timestamp, while this transcript records the
/// backend's own (later) timestamp for the same text — without matching
/// turn metadata the frontend has no signature bridging the two, and the
/// initial prompt rendered twice in the Activity log (observed live: a
/// 6s create→ready gap put the copies in different near-time buckets).
///
/// Counting is content-aware, not blind: user rows whose text the
/// wrapper's steer ledger proves entered mid-turn (`steers`) render
/// WITHOUT turn metadata — the live lane logged those with no index —
/// so post-steer prompts keep the wrapper's numbering. Synthetic
/// interrupt markers (`[Request interrupted by user…]`) are dropped
/// entirely, mirroring the live adapter's disposition
/// (`claude_code.rs::handle_user`): the live feed never rendered them,
/// and burning indexes on them shifted every later prompt.
///
/// The flat predecessor extracted every block's text with
/// `message_content_text` and stamped user-role envelopes as source
/// `"user"` — which put tool_result payloads (command output!) in the log
/// as USER speech, dropped tool-call rows entirely, and used a source
/// label ("claude") the live rows never carry ("Claude Code").
pub(crate) fn parse_claude_session_entries(
    path: &Path,
    steers: &ExternalSteerLedger,
) -> Option<Vec<serde_json::Value>> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();
    // The live rows' source label (AgentBackend::Display) — dedupe
    // signatures include the source, so this must match byte-for-byte.
    let agent_source = crate::external_agent::AgentBackend::ClaudeCode.to_string();
    // Fresh-state counters agree with the live lane's
    // (`UserTurnRevisionState` starts every turn at revision 1), so the
    // initial prompt hydrates as turn 1 rev 1 — the values the live
    // `UserMessageLog` row carries.
    let mut user_turn_revisions = ReplayUserTurnRevisionState::default();
    let mut steer_cursor = steers.cursor();

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
        // Machine-generated records: `isMeta` synthetic turns and
        // `isCompactSummary` context summaries are not conversation prose.
        // The live feed never rendered them, and the message-search ground
        // truth (`message_search::extract_claude::record_from_line`) skips
        // both — rendering them here painted harness plumbing as User rows.
        if obj.get("isMeta").and_then(|v| v.as_bool()) == Some(true)
            || obj.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true)
        {
            continue;
        }
        let ts = value_str(&obj, "timestamp").unwrap_or_default();
        let ts_ms = chrono::DateTime::parse_from_rfc3339(&ts)
            .ok()
            .map(|dt| dt.timestamp_millis());
        let line_uuid = value_str(&obj, "uuid");
        let mut push = |mut entry: serde_json::Value| {
            entry["ts"] = serde_json::Value::String(ts.clone());
            if let Some(ms) = ts_ms {
                entry["ts_ms"] = serde_json::Value::from(ms);
            }
            if let Some(uuid) = line_uuid.as_deref() {
                entry["message_uuid"] = serde_json::Value::String(uuid.to_string());
            }
            entries.push(entry);
        };
        let content = obj.get("message").and_then(|m| m.get("content"));
        // The wrapper appends its supervision addendum to the first prompt;
        // the live UserMessageLog row shows the user's own text. Trim it so
        // hydrated rows read (and dedupe) like the live ones.
        let user_prose = |text: &str| -> String {
            text.split(crate::external_agent::claude_code::CLAUDE_CODE_BOOTSTRAP_ADDENDUM_MARKER)
                .next()
                .unwrap_or(text)
                .trim()
                .to_string()
        };

        // Plain-string content: only user prompts use this shape.
        if let Some(text) = content.and_then(|c| c.as_str()) {
            let text = user_prose(text);
            if typ == "user" && !text.is_empty() && !is_injected_external_user_text(&text) {
                let mut entry = serde_json::json!({
                    "level": "info",
                    "source": "User",
                    "content": text,
                });
                // Mid-turn steer texts render turnless — the live row
                // carried no metadata, and counting them drifts every
                // later prompt off the wrapper's numbering.
                if !steer_cursor.try_consume_mid_turn_steer(&text, ts_ms) {
                    let (user_turn_index, user_turn_revision) =
                        user_turn_revisions.record_next_turn();
                    entry["user_turn_index"] = serde_json::json!(user_turn_index);
                    entry["user_turn_revision"] = serde_json::json!(user_turn_revision);
                }
                push(entry);
            }
            continue;
        }
        let Some(blocks) = content.and_then(|c| c.as_array()) else {
            continue;
        };
        // One user turn per transcript line: a multi-block user message is
        // one live prompt, so its prose blocks share the line's turn — or
        // the line's steer-ness (`Some(None)` = classified as a mid-turn
        // steer, rendered without turn metadata).
        let mut line_user_turn: Option<Option<(u32, u32)>> = None;
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    let text = block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .trim();
                    if text.is_empty() {
                        continue;
                    }
                    if typ == "assistant" {
                        // Live shape: ModelResponse summary (plain text
                        // passes through format_model_summary unchanged).
                        push(serde_json::json!({
                            "level": "model",
                            "source": agent_source,
                            "content": text,
                        }));
                    } else if !is_injected_external_user_text(text) {
                        let text = user_prose(text);
                        if !text.is_empty() {
                            // Live shape: UserMessageLog → LogEntry.
                            let line_turn = *line_user_turn.get_or_insert_with(|| {
                                if steer_cursor.try_consume_mid_turn_steer(&text, ts_ms) {
                                    None
                                } else {
                                    Some(user_turn_revisions.record_next_turn())
                                }
                            });
                            let mut entry = serde_json::json!({
                                "level": "info",
                                "source": "User",
                                "content": text,
                            });
                            if let Some((user_turn_index, user_turn_revision)) = line_turn {
                                entry["user_turn_index"] = serde_json::json!(user_turn_index);
                                entry["user_turn_revision"] =
                                    serde_json::json!(user_turn_revision);
                            }
                            push(entry);
                        }
                    }
                }
                "thinking" if typ == "assistant" => {
                    let text = block
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .trim();
                    if !text.is_empty() {
                        // Live shape: reasoning-only ModelResponse, which
                        // the dashboard renders as a first-class thinking
                        // row (level model + kind reasoning, raw text) —
                        // must match the live grammar exactly so the
                        // session-window dedupe signatures collapse this
                        // row with the streamed copy.
                        push(serde_json::json!({
                            "level": "model",
                            "source": agent_source,
                            "kind": "reasoning",
                            "content": text,
                        }));
                    }
                }
                "tool_use" if typ == "assistant" => {
                    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let input = block
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let preview = crate::external_agent::claude_code::tool_input_preview(&input);
                    let Some(content) =
                        crate::external_output::external_tool_preview_text(name, &preview)
                    else {
                        continue;
                    };
                    let mut entry = serde_json::json!({
                        "level": "agent",
                        "source": agent_source,
                        // Command announcement, never command output — the
                        // frontend's level-'agent' fallback groups untagged
                        // rows into output groups.
                        "kind": "tool_call",
                        "content": content,
                    });
                    if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                        entry["item_id"] = serde_json::Value::String(id.to_string());
                    }
                    push(entry);
                }
                "tool_result" => {
                    let text = block
                        .get("content")
                        .and_then(message_content_text)
                        .unwrap_or_default();
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let is_error = block
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    // Live shape: AgentOutput (kind agent_output), grouped
                    // under its tool call via item_id — NEVER user speech.
                    let mut entry = serde_json::json!({
                        "level": if is_error { "warn" } else { "agent" },
                        "source": agent_source,
                        "kind": "agent_output",
                        "content": text,
                    });
                    if let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) {
                        entry["item_id"] = serde_json::Value::String(id.to_string());
                    }
                    push(entry);
                }
                _ => {}
            }
        }
    }

    annotate_claude_entries_off_active_chain(path, &mut entries);
    Some(entries)
}

/// Flag entries whose transcript line is not on the active uuid chain
/// (abandoned sibling branches from edits/forks). Chain truth comes from
/// the same `shared_claude_tree_scan` the fork-point catalog uses, so the
/// detail view and the catalog can never disagree about which rows are
/// live history.
fn annotate_claude_entries_off_active_chain(path: &Path, entries: &mut [serde_json::Value]) {
    let Ok(tree) = crate::session_fork::shared_claude_tree_scan(path) else {
        return;
    };
    let Some(leaf) = tree.active_leaf.as_deref() else {
        return;
    };
    let on_chain: std::collections::HashSet<&str> = tree
        .ancestor_chain(leaf)
        .iter()
        .map(|node| node.uuid.as_str())
        .collect();
    for entry in entries.iter_mut() {
        let off = match entry.get("message_uuid").and_then(|v| v.as_str()) {
            Some(uuid) => !on_chain.contains(uuid),
            None => false,
        };
        if off {
            entry["off_active_chain"] = serde_json::Value::Bool(true);
        }
    }
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
    // Fast path: mains live at `projects/<project>/<session_id>.jsonl` —
    // one stat per project dir instead of a full recursive walk of the
    // store (>1,000 files on a busy box) per WS bootstrap / detail fetch.
    let projects = home.join(".claude").join("projects");
    let file_name = format!("{session_id}.jsonl");
    if let Ok(project_dirs) = std::fs::read_dir(&projects) {
        for project in project_dirs.flatten() {
            let candidate = project.path().join(&file_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Fallback: the historical exhaustive walk (unusual nesting).
    let mut files = Vec::new();
    collect_files(&projects, ".jsonl", &mut files);
    files
        .into_iter()
        .find(|path| path.file_stem().and_then(|n| n.to_str()) == Some(session_id))
}

/// (home, session_id) → chat path for gemini lookups, so a repeat fetch
/// verifies ONE file instead of read+parsing every chat in the store
/// again. Keyed by the caller-supplied home: the same session id under
/// two homes (tests inject temp homes; leased stores differ) must
/// resolve independently.
fn gemini_transcript_path_cache() -> &'static Mutex<HashMap<(PathBuf, String), PathBuf>> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), PathBuf>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn gemini_chat_file_matches(path: &Path, session_id: &str) -> bool {
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
}

pub(crate) fn find_gemini_session_file_for_transcript(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    // Normalized once so symlinked/relative spellings of the same home
    // share one entry instead of duplicating the cache per alias.
    let home_key = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let cache_key = (home_key, session_id.to_string());
    if let Some(cached) = gemini_transcript_path_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_key)
        .cloned()
    {
        // Re-verify the single remembered file (ids are stable but chats
        // can be deleted or rewritten); a miss falls through to the scan.
        if gemini_chat_file_matches(&cached, session_id) {
            return Some(cached);
        }
    }
    let mut files = Vec::new();
    collect_files(&home.join(".gemini").join("tmp"), ".json", &mut files);
    let found = files
        .into_iter()
        .find(|path| gemini_chat_file_matches(path, session_id))?;
    let mut cache = gemini_transcript_path_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 512 && !cache.contains_key(&cache_key) {
        cache.clear();
    }
    cache.insert(cache_key, found.clone());
    Some(found)
}

pub(crate) fn parse_external_session_entries_from_file(
    source: &str,
    session_id: &str,
    path: &Path,
    steers: &ExternalSteerLedger,
) -> Option<Vec<serde_json::Value>> {
    match source {
        "codex" => parse_codex_session_entries(path, steers),
        "claude-code" => parse_claude_session_entries(path, steers),
        "gemini" => parse_gemini_session_entries(path, session_id),
        _ => None,
    }
}

/// Shared-snapshot form: read-only consumers take the cache's Arc
/// directly instead of deep-cloning the whole parsed transcript per hit.
///
/// The steer ledger is built only on a cache MISS: a cached parse of an
/// unchanged transcript is invariant under ledger growth, because a new
/// ledger entry's timestamp guard admits only rows written after its
/// request — rows the keyed snapshot cannot contain (and when the steer's
/// row does land, the transcript's len/mtime key changes and the parse
/// reruns with the fresh ledger).
pub(crate) fn external_session_entries_from_file_arc(
    home: &Path,
    source: &str,
    session_id: &str,
    path: &Path,
) -> Option<std::sync::Arc<Vec<serde_json::Value>>> {
    let key = external_transcript_cache_key(source, session_id, path)?;
    if let Some(entries) = cached_external_transcript_entries(&key) {
        return Some(entries);
    }

    let steers = match source {
        "codex" | "claude-code" => external_mid_turn_steer_ledger(home, source, session_id),
        _ => ExternalSteerLedger::default(),
    };
    let mut entries = parse_external_session_entries_from_file(source, session_id, path, &steers)?;
    annotate_external_transcript_entries(source, session_id, &mut entries);
    let entries = std::sync::Arc::new(entries);
    store_external_transcript_entries(key, &entries);
    Some(entries)
}

pub(crate) fn external_session_entries_from_home_arc(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<std::sync::Arc<Vec<serde_json::Value>>> {
    let source = crate::session_names::normalize_source(source);
    let path = match source.as_str() {
        "codex" => find_codex_session_file(home, session_id),
        "claude-code" => find_claude_session_file_for_transcript(home, session_id),
        "gemini" => find_gemini_session_file_for_transcript(home, session_id),
        _ => None,
    }?;

    external_session_entries_from_file_arc(home, &source, session_id, &path)
}

pub(crate) fn external_session_entries_from_home(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<Vec<serde_json::Value>> {
    external_session_entries_from_home_arc(home, source, session_id)
        .map(|entries| (*entries).clone())
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
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
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
            let mtime = session_activity_mtime_secs(&path);
            Some((mtime, path))
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_transcript_lookup_is_scoped_per_home() {
        let write_chat = |home: &Path, session_id: &str, text: &str| {
            let chats = home.join(".gemini").join("tmp").join("proj").join("chats");
            std::fs::create_dir_all(&chats).unwrap();
            let body = serde_json::json!({
                "sessionId": session_id,
                "messages": [{ "role": "user", "content": text }],
            });
            std::fs::write(chats.join("chat-1.json"), body.to_string()).unwrap();
        };
        let home_a = tempfile::tempdir().unwrap();
        let home_b = tempfile::tempdir().unwrap();
        // The SAME session id exists under both homes: each lookup must
        // resolve within its own home — a shared id-only cache key would
        // serve home A's transcript to home B.
        write_chat(home_a.path(), "shared-id", "from home a");
        write_chat(home_b.path(), "shared-id", "from home b");

        let found_a = find_gemini_session_file_for_transcript(home_a.path(), "shared-id")
            .expect("home a chat resolves");
        assert!(found_a.starts_with(home_a.path()), "{}", found_a.display());
        // Repeat (cache-hit path) must stay inside home B, not replay A.
        for _ in 0..2 {
            let found_b = find_gemini_session_file_for_transcript(home_b.path(), "shared-id")
                .expect("home b chat resolves");
            assert!(found_b.starts_with(home_b.path()), "{}", found_b.display());
        }
    }

    #[test]
    fn transcript_cache_admission_rechecks_the_source_after_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-post-parse.jsonl");
        std::fs::write(&path, b"{\"type\":\"noise\"}\n").unwrap();
        // Key stat'd BEFORE the (simulated) parse...
        let key = external_transcript_cache_key("codex", "post-parse-growth", &path)
            .expect("key from pre-parse stat");
        // ...then the file grows mid-parse: caching now would pin the
        // parse under a key no future lookup matches (and could exceed
        // the byte gate unnoticed).
        std::fs::write(&path, b"{\"type\":\"noise\"}\n{\"type\":\"more\"}\n").unwrap();
        let entries = std::sync::Arc::new(vec![serde_json::json!({ "content": "stale parse" })]);
        store_external_transcript_entries(key.clone(), &entries);
        assert!(
            cached_external_transcript_entries(&key).is_none(),
            "a parse of bytes that no longer match the keyed snapshot must not be cached"
        );
        // The steady case (file unchanged since the key) still caches.
        let fresh_key =
            external_transcript_cache_key("codex", "post-parse-growth", &path).expect("fresh key");
        store_external_transcript_entries(fresh_key.clone(), &entries);
        assert!(cached_external_transcript_entries(&fresh_key).is_some());
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

    /// Reasoning response_items surface as first-class thinking rows with
    /// the SAME grammar the live lane emits (level model + kind reasoning,
    /// raw extractor text) so window hydration dedupes against streamed
    /// copies instead of double-rendering.
    #[test]
    fn codex_transcript_surfaces_reasoning_items() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-transcript-reasoning";
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
                        "id": "rs_1",
                        "type": "reasoning",
                        "summary": [
                            { "type": "summary_text", "text": "Weigh the options" },
                            { "type": "summary_text", "text": "Pick the safe path" }
                        ]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "event_msg",
                    "payload": { "type": "agent_message", "message": "Done" }
                }),
                // Empty reasoning item: no text, no summary — honest absence,
                // no row.
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:05Z",
                    "type": "response_item",
                    "payload": { "id": "rs_2", "type": "reasoning" }
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
        let reasoning: Vec<_> = entries
            .iter()
            .filter(|entry| entry["kind"].as_str() == Some("reasoning"))
            .collect();
        assert_eq!(reasoning.len(), 1, "one non-empty reasoning row");
        let row = reasoning[0];
        assert_eq!(row["level"].as_str(), Some("model"));
        assert_eq!(row["source"].as_str(), Some("codex"));
        assert_eq!(
            row["content"].as_str(),
            Some("Weigh the options\nPick the safe path"),
            "content is the raw extractor text (live-lane parity)"
        );
        assert_eq!(row["item_id"].as_str(), Some("rs_1"));
        assert_eq!(row["session_id"].as_str(), Some(session_id));
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

    /// The Claude transcript reconstruction mirrors the LIVE event shapes:
    /// tool_results are agent output under their tool call (never "user"
    /// speech), tool calls render as command rows with item ids, thinking
    /// becomes a first-class reasoning row (level model + kind reasoning),
    /// and the source label is the live "Claude Code" so hydration dedupes
    /// against streamed rows.
    #[test]
    fn claude_transcript_parse_is_structural() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let lines = [
            r#"{"type":"user","timestamp":"2026-07-13T03:22:56.000Z","message":{"role":"user","content":[{"type":"text","text":"Read README.md"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-07-13T03:23:13.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"planning the read"},{"type":"text","text":"I'll read it."},{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"cat README.md"}}]}}"#,
            r##"{"type":"user","timestamp":"2026-07-13T03:23:14.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"# QA sandbox"}]}}"##,
            r#"{"type":"user","timestamp":"2026-07-13T03:23:15.000Z","message":{"role":"user","content":[{"type":"text","text":"<local-command-stdout>noise</local-command-stdout>"}]}}"#,
            r#"{"type":"user","timestamp":"2026-07-13T03:23:16.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_02","is_error":true,"content":[{"type":"text","text":"boom"}]}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let entries = parse_claude_session_entries(&path, &ExternalSteerLedger::default()).expect("parse");
        let rows: Vec<(String, String, String, String, String)> = entries
            .iter()
            .map(|e| {
                (
                    e["level"].as_str().unwrap_or("").to_string(),
                    e["source"].as_str().unwrap_or("").to_string(),
                    e["kind"].as_str().unwrap_or("").to_string(),
                    e["item_id"].as_str().unwrap_or("").to_string(),
                    e["content"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();

        assert_eq!(
            rows,
            vec![
                (
                    "info".into(),
                    "User".into(),
                    "".into(),
                    "".into(),
                    "Read README.md".into()
                ),
                (
                    "model".into(),
                    "Claude Code".into(),
                    "reasoning".into(),
                    "".into(),
                    "planning the read".into()
                ),
                (
                    "model".into(),
                    "Claude Code".into(),
                    "".into(),
                    "".into(),
                    "I'll read it.".into()
                ),
                (
                    "agent".into(),
                    "Claude Code".into(),
                    "tool_call".into(),
                    "toolu_01".into(),
                    "Bash: cat README.md".into()
                ),
                (
                    "agent".into(),
                    "Claude Code".into(),
                    "agent_output".into(),
                    "toolu_01".into(),
                    "# QA sandbox".into()
                ),
                (
                    "warn".into(),
                    "Claude Code".into(),
                    "agent_output".into(),
                    "toolu_02".into(),
                    "boom".into()
                ),
            ]
        );
        // Timestamps ride both string and millisecond forms for ordered
        // merging on the frontend.
        assert_eq!(entries[0]["ts"], "2026-07-13T03:22:56.000Z");
        assert_eq!(entries[0]["ts_ms"], 1_783_912_976_000_i64);
    }

    /// `isMeta` synthetic turns and `isCompactSummary` context summaries are
    /// machine-generated — the ground truth
    /// (`message_search::extract_claude::record_from_line`) skips both, and
    /// the structural parser must too instead of rendering them as User rows.
    #[test]
    fn claude_transcript_parse_skips_meta_and_compact_summary_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let lines = [
            r#"{"type":"user","isMeta":true,"timestamp":"2026-07-13T03:22:50.000Z","message":{"role":"user","content":"Caveat: the messages below were generated by the harness"}}"#,
            r#"{"type":"user","isMeta":true,"timestamp":"2026-07-13T03:22:51.000Z","message":{"role":"user","content":[{"type":"text","text":"<system-notice>synthetic turn</system-notice>"}]}}"#,
            r#"{"type":"user","isCompactSummary":true,"timestamp":"2026-07-13T03:22:52.000Z","message":{"role":"user","content":[{"type":"text","text":"This session is being continued from a previous conversation..."}]}}"#,
            r#"{"type":"user","timestamp":"2026-07-13T03:22:56.000Z","message":{"role":"user","content":[{"type":"text","text":"real user prompt"}]}}"#,
            r#"{"type":"assistant","isMeta":false,"timestamp":"2026-07-13T03:23:13.000Z","message":{"role":"assistant","content":[{"type":"text","text":"real reply"}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let entries = parse_claude_session_entries(&path, &ExternalSteerLedger::default()).expect("parse");
        let contents: Vec<&str> = entries
            .iter()
            .map(|e| e["content"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            contents,
            vec!["real user prompt", "real reply"],
            "machine-generated records must not render as conversation rows"
        );
        assert!(entries
            .iter()
            .all(|e| e["source"] != "User" || e["content"] == "real user prompt"));
    }

    /// User prompt rows carry sequential `user_turn_index`/
    /// `user_turn_revision` — the dedupe bridge to the live `UserMessageLog`
    /// rows. The live row logs the prompt at DISPATCH time with the
    /// wrapper's turn metadata; the backend transcript re-serves the same
    /// text under the backend's own (later) timestamp, so without matching
    /// turn metadata no frontend signature collapses the two and the
    /// initial prompt renders twice in the Activity log (a 6s create→ready
    /// gap defeats even the near-time bucket). Tool results, meta records,
    /// and harness-injected text must consume NO turn — they are not live
    /// user turns, and burning indexes on them would shift every later
    /// prompt out of alignment with the wrapper's round counter.
    #[test]
    fn claude_transcript_user_rows_carry_live_turn_metadata() {
        let addendum_marker =
            crate::external_agent::claude_code::CLAUDE_CODE_BOOTSTRAP_ADDENDUM_MARKER;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let initial_prompt =
            format!("fix the flaky test\\n\\n{addendum_marker}\\nsupervisor plumbing");
        let lines = [
            // The wrapper appends the supervision addendum to the first
            // prompt; hydration must serve the user's own prose (what the
            // live row shows) with turn 1 rev 1 — the fresh-state values
            // the live emission mints.
            format!(
                r#"{{"type":"user","timestamp":"2026-07-13T03:22:56.000Z","message":{{"role":"user","content":"{initial_prompt}"}}}}"#
            ),
            r#"{"type":"assistant","timestamp":"2026-07-13T03:23:13.000Z","message":{"role":"assistant","content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"cargo test"}}]}}"#.to_string(),
            // Interleaved non-turns: tool_result, isMeta, injected text.
            r#"{"type":"user","timestamp":"2026-07-13T03:23:14.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"1 test passed"}]}}"#.to_string(),
            r#"{"type":"user","isMeta":true,"timestamp":"2026-07-13T03:23:15.000Z","message":{"role":"user","content":"Caveat: harness-generated"}}"#.to_string(),
            r#"{"type":"user","timestamp":"2026-07-13T03:23:16.000Z","message":{"role":"user","content":[{"type":"text","text":"<local-command-stdout>noise</local-command-stdout>"}]}}"#.to_string(),
            // Follow-up in block form (an image block rides along): ONE
            // turn for the line, assigned to its prose block.
            r#"{"type":"user","timestamp":"2026-07-13T03:24:00.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}},{"type":"text","text":"now fix the docs"}]}}"#.to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let entries = parse_claude_session_entries(&path, &ExternalSteerLedger::default()).expect("parse");
        let user_rows: Vec<(&str, u64, u64)> = entries
            .iter()
            .filter(|e| e["source"] == "User")
            .map(|e| {
                (
                    e["content"].as_str().unwrap_or(""),
                    e["user_turn_index"].as_u64().unwrap_or(0),
                    e["user_turn_revision"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        assert_eq!(
            user_rows,
            vec![("fix the flaky test", 1, 1), ("now fix the docs", 2, 1)],
            "user prompts carry sequential turn metadata; non-turns consume no index"
        );
        // Non-user rows must not claim turn metadata (the frontend keys
        // edit affordances and dedupe signatures off these fields).
        assert!(entries
            .iter()
            .filter(|e| e["source"] != "User")
            .all(|e| e.get("user_turn_index").is_none() && e.get("user_turn_revision").is_none()));
        // The cross-lane contract that makes the signatures collapse: the
        // live lane's counter (UserTurnRevisionState, external_mode round
        // bookkeeping) and the replay/hydration counter mint identical
        // fresh-state sequences.
        let mut live = crate::codex_history::UserTurnRevisionState::default();
        let mut replay = ReplayUserTurnRevisionState::default();
        assert_eq!(live.record_next_turn(), replay.record_next_turn());
        assert_eq!(live.record_next_turn(), replay.record_next_turn());
    }

    fn rfc3339_ms(value: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(value)
            .expect("fixture timestamp")
            .timestamp_millis()
    }

    /// End-to-end mid-turn steer alignment for Codex: a steer the wrapper
    /// log proves entered mid-turn (`steer_requested` + `steer_accepted`,
    /// the `turn/steer` OK arc) hydrates as a user row WITHOUT turn
    /// metadata — the live lane logged it with none — and consumes no
    /// index, so the post-steer follow-up keeps the wrapper's numbering
    /// (turn 2, matching the live `emit_user_message_log` row) instead of
    /// drifting to 3 and falling back to the fragile near-time dedupe
    /// bucket. Exercises the full pipeline: wrapper-index discovery →
    /// ledger build → parse.
    #[test]
    fn codex_transcript_mid_turn_steer_rows_stay_turnless() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session_id = "019e37b2-steer-alignment";
        let wrapper_id = "wrapper-steer-alignment";
        let steer_text = "also update the docs";

        let wrapper_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        let requested_ts_ms = rfc3339_ms("2026-07-15T10:00:29Z");
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            [
                serde_json::json!({
                    "ts": "10:00:29", "ts_ms": requested_ts_ms,
                    "event": "steer_requested", "level": "info",
                    "message": format!("Steer requested: {steer_text}"),
                    "data": { "session_id": session_id, "id": "steer-1", "status": "pending", "text": steer_text },
                }),
                serde_json::json!({
                    "ts": "10:00:29", "ts_ms": requested_ts_ms + 150,
                    "event": "steer_accepted", "level": "info",
                    "message": "Steer accepted",
                    "data": { "session_id": session_id, "id": "steer-1", "status": "accepted" },
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            session_id,
            wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        std::fs::write(
            sessions_dir.join(format!("rollout-2026-07-15T10-00-00-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-07-15T10:00:00Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-07-15T10:00:01Z",
                    "type": "event_msg",
                    "payload": { "type": "task_started", "turn_id": "codex-turn-1" }
                }),
                serde_json::json!({
                    "timestamp": "2026-07-15T10:00:01Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "fix the flaky auth test" }
                }),
                serde_json::json!({
                    "timestamp": "2026-07-15T10:00:10Z",
                    "type": "event_msg",
                    "payload": { "type": "agent_message", "message": "Looking at it." }
                }),
                // The mid-turn steer, echoed into the rollout AFTER the
                // wrapper logged its request.
                serde_json::json!({
                    "timestamp": "2026-07-15T10:00:30Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": steer_text }
                }),
                serde_json::json!({
                    "timestamp": "2026-07-15T10:01:00Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "now ship the fix" }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let entries = external_session_entries_from_home(home.path(), "codex", session_id)
            .expect("codex session should resolve");
        let user_rows: Vec<(&str, Option<u64>, Option<u64>)> = entries
            .iter()
            .filter(|e| e["source"] == "user")
            .map(|e| {
                (
                    e["content"].as_str().unwrap_or(""),
                    e.get("user_turn_index").and_then(|v| v.as_u64()),
                    e.get("user_turn_revision").and_then(|v| v.as_u64()),
                )
            })
            .collect();
        assert_eq!(
            user_rows,
            vec![
                ("fix the flaky auth test", Some(1), Some(1)),
                (steer_text, None, None),
                ("now ship the fix", Some(2), Some(1)),
            ],
            "the mid-turn steer renders turnless and burns no index"
        );
        // The steer row still belongs to the turn it steered — its thread
        // projection attributes it to the active turn, not a phantom one.
        let steer_row = entries
            .iter()
            .find(|e| e["source"] == "user" && e["content"] == steer_text)
            .expect("steer row rendered");
        assert_eq!(steer_row["turn_id"].as_str(), Some("codex-turn-1"));
    }

    /// Steer classification never collapses or re-classifies repeated
    /// identical prompts: one ledger entry justifies AT MOST one turnless
    /// row, and every other occurrence of the same text keeps minting its
    /// own distinct turn (turn identity is what distinguishes legitimately
    /// repeated prompts — the #444 safety bar).
    #[test]
    fn codex_transcript_repeated_identical_prompts_stay_distinct_after_steer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-repeat-steer.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "timestamp": "2026-07-15T11:00:00Z",
                    "type": "session_meta",
                    "payload": { "id": "019e37b2-repeat-steer" }
                }),
                serde_json::json!({
                    "timestamp": "2026-07-15T11:00:01Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "start the migration" }
                }),
                // Mid-turn steer (in the ledger, echoed after its request).
                serde_json::json!({
                    "timestamp": "2026-07-15T11:00:30Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "keep going" }
                }),
                // The user later sends the IDENTICAL text as a real
                // follow-up: the spent ledger entry must not touch it.
                serde_json::json!({
                    "timestamp": "2026-07-15T11:05:00Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "keep going" }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let steers = ExternalSteerLedger::from_entries(vec![ExternalSteerLedgerEntry {
            text: "keep going".to_string(),
            requested_ts_ms: Some(rfc3339_ms("2026-07-15T11:00:29Z")),
        }]);
        let entries = parse_codex_session_entries(&path, &steers).expect("parse");
        let user_rows: Vec<(&str, Option<u64>, Option<u64>)> = entries
            .iter()
            .filter(|e| e["source"] == "user")
            .map(|e| {
                (
                    e["content"].as_str().unwrap_or(""),
                    e.get("user_turn_index").and_then(|v| v.as_u64()),
                    e.get("user_turn_revision").and_then(|v| v.as_u64()),
                )
            })
            .collect();
        assert_eq!(
            user_rows,
            vec![
                ("start the migration", Some(1), Some(1)),
                ("keep going", None, None),
                ("keep going", Some(2), Some(1)),
            ],
            "one ledger entry = at most one turnless row; the repeat is a distinct real turn"
        );
    }

    /// Claude Code twin of the steer-alignment contract, plus the CC
    /// synthetic abort marker: a ledger-proven mid-turn steer renders
    /// turnless (block form — the classification is per transcript line),
    /// `[Request interrupted by user]` rows disappear entirely (the live
    /// adapter drops them — `handle_user`; hydration painting them as
    /// User rows also burned an index and shifted every later prompt),
    /// the initial prompt keeps the #444 contract (turn 1 rev 1, addendum
    /// trimmed), and identical repeated prompts keep distinct turns.
    #[test]
    fn claude_transcript_steer_and_interrupt_rows_do_not_shift_turns() {
        let addendum_marker =
            crate::external_agent::claude_code::CLAUDE_CODE_BOOTSTRAP_ADDENDUM_MARKER;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let initial_prompt = format!("refactor the parser\\n\\n{addendum_marker}\\nsupervisor plumbing");
        let lines = [
            format!(
                r#"{{"type":"user","timestamp":"2026-07-15T12:00:00.000Z","message":{{"role":"user","content":"{initial_prompt}"}}}}"#
            ),
            r#"{"type":"assistant","timestamp":"2026-07-15T12:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Starting."}]}}"#.to_string(),
            // CC's synthetic abort marker rides a user message; the live
            // feed never rendered it and no turn may burn on it.
            r#"{"type":"user","timestamp":"2026-07-15T12:00:20.000Z","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#.to_string(),
            // The mid-turn steer (block form), echoed after its request.
            r#"{"type":"user","timestamp":"2026-07-15T12:00:30.000Z","message":{"role":"user","content":[{"type":"text","text":"also fix the docs"}]}}"#.to_string(),
            r#"{"type":"user","timestamp":"2026-07-15T12:01:00.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}},{"type":"text","text":"resume with plan B"}]}}"#.to_string(),
            // Identical repeated prompt: a distinct real turn.
            r#"{"type":"user","timestamp":"2026-07-15T12:02:00.000Z","message":{"role":"user","content":"resume with plan B"}}"#.to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let steers = ExternalSteerLedger::from_entries(vec![ExternalSteerLedgerEntry {
            text: "also fix the docs".to_string(),
            requested_ts_ms: Some(rfc3339_ms("2026-07-15T12:00:29Z")),
        }]);
        let entries = parse_claude_session_entries(&path, &steers).expect("parse");
        let user_rows: Vec<(&str, Option<u64>, Option<u64>)> = entries
            .iter()
            .filter(|e| e["source"] == "User")
            .map(|e| {
                (
                    e["content"].as_str().unwrap_or(""),
                    e.get("user_turn_index").and_then(|v| v.as_u64()),
                    e.get("user_turn_revision").and_then(|v| v.as_u64()),
                )
            })
            .collect();
        assert_eq!(
            user_rows,
            vec![
                ("refactor the parser", Some(1), Some(1)),
                ("also fix the docs", None, None),
                ("resume with plan B", Some(2), Some(1)),
                ("resume with plan B", Some(3), Some(1)),
            ],
            "steer turnless, interrupt marker absent, repeats distinct, initial prompt intact"
        );
        assert!(
            entries.iter().all(|e| !e["content"]
                .as_str()
                .unwrap_or("")
                .contains("Request interrupted")),
            "synthetic abort markers never render on any row"
        );
    }

    /// Transcript entries carry the line's `message_uuid`, abandoned sibling
    /// branches are flagged `off_active_chain`, and every Claude fork point's
    /// `at_message_uuid` display anchor resolves to a rendered row with the
    /// expected chain membership — the parity that lets the dashboard place
    /// fork affordances inline on transcript rows.
    #[test]
    fn claude_transcript_entries_join_fork_point_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // u1 → a1 → u2a (abandoned branch); u1 → a1 → u2b → a2 (active tail).
        let lines = [
            r#"{"uuid":"u1","parentUuid":null,"type":"user","timestamp":"2026-07-16T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"first prompt"}]}}"#,
            r#"{"uuid":"a1","parentUuid":"u1","type":"assistant","timestamp":"2026-07-16T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"first reply"}]}}"#,
            r#"{"uuid":"u2a","parentUuid":"a1","type":"user","timestamp":"2026-07-16T00:00:03.000Z","message":{"role":"user","content":[{"type":"text","text":"abandoned follow-up"}]}}"#,
            r#"{"uuid":"u2b","parentUuid":"a1","type":"user","timestamp":"2026-07-16T00:00:04.000Z","message":{"role":"user","content":[{"type":"text","text":"kept follow-up"}]}}"#,
            r#"{"uuid":"a2","parentUuid":"u2b","type":"assistant","timestamp":"2026-07-16T00:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"final reply"}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let entries = parse_claude_session_entries(&path, &ExternalSteerLedger::default()).expect("parse");
        let entry_for = |uuid: &str| {
            entries
                .iter()
                .find(|e| e["message_uuid"] == uuid)
                .unwrap_or_else(|| panic!("no entry for uuid {uuid}"))
        };
        assert!(entries.iter().all(|e| e["message_uuid"].is_string()));
        assert_eq!(entry_for("u2a")["off_active_chain"], true);
        for uuid in ["u1", "a1", "u2b", "a2"] {
            assert!(
                entry_for(uuid).get("off_active_chain").is_none(),
                "{uuid} is on the active chain"
            );
        }

        let catalog = crate::session_fork::claude_fork_points(
            "sess",
            "backend",
            &path,
            &crate::session_fork::ForkPointQuery::default(),
        )
        .expect("catalog");
        assert!(catalog.supported);
        assert!(!catalog.fork_points.is_empty());
        for point in &catalog.fork_points {
            let at = point
                .at_message_uuid
                .as_deref()
                .unwrap_or_else(|| panic!("claude point {} lacks at_message_uuid", point.id));
            let row = entry_for(at);
            match point.kind {
                "branch-tip" => assert_eq!(
                    row["off_active_chain"], true,
                    "branch tip {at} must land on an off-chain row"
                ),
                _ => assert!(
                    row.get("off_active_chain").is_none(),
                    "{} point {at} must land on an active-chain row",
                    point.kind
                ),
            }
        }
    }
}
