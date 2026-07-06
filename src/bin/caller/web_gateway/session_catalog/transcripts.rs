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

#[cfg(test)]
mod tests {
    use super::*;

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
}
