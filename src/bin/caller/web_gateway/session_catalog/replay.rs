//! Session-log replay: JSONL-to-outbound/browser entry conversion, replay
//! metadata + ids, context-snapshot replay entries, detail paging, and the
//! websocket bootstrap replay preparation.

use super::*;

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

/// Browser-entry variant with line provenance (this is what the detail
/// read path consumes, via [`session_log_replay_entries_from_contents`]):
/// the second vec is index-aligned with the entries and carries the
/// 1-based `session.jsonl` line each entry was rendered from (`None` for
/// the synthetic prelude entries — `replay_start` and the wrapper
/// `session_identity` marker). Message-search locators anchor on source
/// lines, and this mapping is what turns a verified line into an entry
/// index without re-deriving the render rules (locate.rs).
pub(crate) fn replay_jsonl_to_browser_entries_with_lines(
    contents: &str,
    log_dir: &std::path::Path,
) -> (Vec<serde_json::Value>, Vec<Option<u64>>) {
    replay_jsonl_to_outbound_entries_tracked(contents, log_dir, true)
}

pub(crate) fn replay_jsonl_to_outbound_entries_inner(
    contents: &str,
    log_dir: &std::path::Path,
    compact_historical_context: bool,
) -> Vec<serde_json::Value> {
    replay_jsonl_to_outbound_entries_tracked(contents, log_dir, compact_historical_context).0
}

fn replay_jsonl_to_outbound_entries_tracked(
    contents: &str,
    log_dir: &std::path::Path,
    compact_historical_context: bool,
) -> (Vec<serde_json::Value>, Vec<Option<u64>>) {
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
    // Index-aligned 1-based source line per entry; None = synthetic
    // prelude (see `replay_jsonl_to_browser_entries_with_lines`).
    let mut entry_lines: Vec<Option<u64>> = Vec::new();
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
    entry_lines.push(None);
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
            entry_lines.push(None);
        }
    }

    let legacy_model_spans = validated_legacy_model_response_spans(contents, log_dir);
    let mut legacy_model_indices: HashMap<String, usize> = HashMap::new();
    for (line_index, line) in contents.lines().enumerate() {
        let line_no = line_index as u64 + 1;
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
                entry_lines.push(Some(line_no));
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
        entry_lines.push(Some(line_no));
    }

    (entries, entry_lines)
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
    let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).ok()?;
    let (entries, _, external_session_id) =
        session_log_replay_entries_from_contents(log_dir, &contents);
    Some((entries, external_session_id))
}

/// Core of [`session_log_replay_entries_from_dir`] over already-read
/// contents, also returning the per-entry source-line provenance (the
/// `locate=` resolver reads the same contents to verify locators, so it
/// must not re-read the file between verification and rendering).
pub(crate) fn session_log_replay_entries_from_contents(
    log_dir: &std::path::Path,
    contents: &str,
) -> (Vec<serde_json::Value>, Vec<Option<u64>>, Option<String>) {
    let external_session = external_backend_session_from_replay(contents);
    let external_session_id = external_session
        .as_ref()
        .map(|(_, id)| id.clone())
        .or_else(|| external_backend_session_id_from_replay(contents));
    let (mut entries, entry_lines) = replay_jsonl_to_browser_entries_with_lines(contents, log_dir);
    if let Some((source, session_id)) = external_session.as_ref() {
        let home = home_from_intendant_log_dir(log_dir).unwrap_or_else(crate::platform::home_dir);
        annotate_replay_user_turns_from_external_transcript(
            &mut entries,
            &home,
            source,
            session_id,
        );
    }
    (entries, entry_lines, external_session_id)
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

/// [`prepare_websocket_bootstrap_replay_entries`] over borrowed entries
/// (the cached replay tier): drops context snapshots and clones only the
/// kept window instead of the whole converted log.
pub(crate) fn prepare_websocket_bootstrap_replay_entries_ref(
    entries: &[serde_json::Value],
    limit: usize,
) -> Vec<serde_json::Value> {
    let filtered: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|entry| entry.get("event").and_then(|v| v.as_str()) != Some("context_snapshot"))
        .collect();
    let selection = session_detail_page_selection_over(filtered.iter().copied(), Some(limit), None);
    let mut out: Vec<serde_json::Value> = match &selection.keep {
        None => filtered[..selection.page_end]
            .iter()
            .map(|entry| (*entry).clone())
            .collect(),
        Some(keep) => keep
            .iter()
            .filter_map(|idx| filtered.get(*idx).map(|entry| (*entry).clone()))
            .collect(),
    };
    for entry in &mut out {
        compact_replay_entry_text_fields_for_websocket(entry);
    }
    out
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
    // Both shapes ride the fingerprint caches in `replay_cache`: the
    // windowed shape (every websocket bootstrap + external activity
    // replay) additionally takes the tail-scan path on large logs, so a
    // dashboard connect no longer converts the entire transcript to keep
    // its last few hundred entries.
    match limit {
        Some(limit) => cached_bootstrap_replay_payload(log_dir, limit),
        None => {
            let (entries, external_session_id) = cached_session_log_replay_entries(log_dir)?;
            Some((
                replay_payload_string(entries.as_slice()),
                external_session_id,
            ))
        }
    }
}

/// The `log_replay` wire envelope over already-prepared entries.
pub(crate) fn replay_payload_string(entries: &[serde_json::Value]) -> String {
    serde_json::json!({
        "t": "log_replay",
        "entries": entries,
    })
    .to_string()
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
    pub(crate) entries: Vec<serde_json::Value>,
    pub(crate) total_entries: usize,
    pub(crate) page_start: usize,
    pub(crate) page_end: usize,
}

/// The paging decision, shared by the by-value and by-ref variants:
/// which indices survive (pinned kinds + latest goal per session + the
/// requested window), and the window bounds. `None` keep-set means
/// "all of `0..page_end`" (the no-limit fast paths).
pub(crate) struct SessionDetailPageSelection {
    pub(crate) keep: Option<BTreeSet<usize>>,
    pub(crate) total_entries: usize,
    pub(crate) page_start: usize,
    pub(crate) page_end: usize,
}

pub(crate) fn session_detail_page_selection(
    entries: &[serde_json::Value],
    limit: Option<usize>,
    before: Option<usize>,
) -> SessionDetailPageSelection {
    session_detail_page_selection_over(entries.iter(), limit, before)
}

pub(crate) fn session_detail_page_selection_over<'a, I>(
    entries: I,
    limit: Option<usize>,
    before: Option<usize>,
) -> SessionDetailPageSelection
where
    I: ExactSizeIterator<Item = &'a serde_json::Value>,
{
    let total_entries = entries.len();
    let Some(limit) = limit else {
        let end = before.unwrap_or(total_entries).min(total_entries);
        return SessionDetailPageSelection {
            keep: None,
            total_entries,
            page_start: 0,
            page_end: end,
        };
    };
    let limit = limit.clamp(1, SESSION_DETAIL_ENTRY_LIMIT_MAX);
    let page_end = before.unwrap_or(total_entries).min(total_entries);
    let page_start = page_end.saturating_sub(limit);
    if total_entries <= limit && before.is_none() {
        return SessionDetailPageSelection {
            keep: None,
            total_entries,
            page_start: 0,
            page_end: total_entries,
        };
    }

    let mut keep = BTreeSet::new();
    let mut latest_goal_by_session: HashMap<String, usize> = HashMap::new();
    for (idx, entry) in entries.enumerate() {
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

    SessionDetailPageSelection {
        keep: Some(keep),
        total_entries,
        page_start,
        page_end,
    }
}

pub(crate) fn session_detail_page_entries(
    entries: Vec<serde_json::Value>,
    limit: Option<usize>,
    before: Option<usize>,
) -> SessionDetailPageEntries {
    let selection = session_detail_page_selection(&entries, limit, before);
    let kept = match &selection.keep {
        None if selection.page_end == selection.total_entries => entries,
        None => entries.into_iter().take(selection.page_end).collect(),
        Some(keep) => entries
            .into_iter()
            .enumerate()
            .filter_map(|(idx, entry)| keep.contains(&idx).then_some(entry))
            .collect(),
    };
    SessionDetailPageEntries {
        entries: kept,
        total_entries: selection.total_entries,
        page_start: selection.page_start,
        page_end: selection.page_end,
    }
}

/// [`session_detail_page_entries`] over borrowed entries (the cached
/// replay tier hands out `Arc<Vec<_>>`): clones only the kept page
/// instead of the whole converted log.
pub(crate) fn session_detail_page_entries_ref(
    entries: &[serde_json::Value],
    limit: Option<usize>,
    before: Option<usize>,
) -> SessionDetailPageEntries {
    let selection = session_detail_page_selection(entries, limit, before);
    let kept = match &selection.keep {
        None => entries[..selection.page_end].to_vec(),
        Some(keep) => keep
            .iter()
            .filter_map(|idx| entries.get(*idx).cloned())
            .collect(),
    };
    SessionDetailPageEntries {
        entries: kept,
        total_entries: selection.total_entries,
        page_start: selection.page_start,
        page_end: selection.page_end,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_session_log_replay_from_dir_reads_active_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.info("Provider: openai");
        log.model_response("still here after refresh", 0, 0, 0, 0, 0, None);
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
}
