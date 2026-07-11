//! Session usage accounting and list-row assembly: usage parsing per
//! backend, repricing, sort keys, and source-preserving truncation.

use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionUsage {
    pub(crate) total_tokens: u64,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
    pub(crate) cached_tokens: u64,
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
    if cutoff <= 0 {
        return None;
    }
    let exact_key = (parent_id.to_string(), cutoff);
    let baseline = exact_parent_baselines.get(&exact_key)?.unwrap_or_default();
    // Rollouts written by current Codex restart the cumulative token counters
    // at fork, so the parent's history never appears in the child's readings
    // and subtracting it would delete real usage. Only rebaseline when the
    // child's first reading is large enough to actually contain the parent
    // baseline (legacy carryover forks).
    let first = summary.first_usage_event.as_ref()?;
    if first.usage.total_tokens < baseline.total_tokens {
        return None;
    }
    Some(baseline)
}

/// Daily usage for a forked session. The parse-time buckets counted the
/// first usage event's cumulative reading from zero; for a carryover fork
/// that reading still contains the parent's history — remove the parent
/// baseline from the first event's day bucket. Callers pass `baseline`
/// from `codex_parent_baseline_for_summary`, which returns `None` for
/// fresh-counter forks (the child restarted at zero, so there is nothing
/// to remove and subtracting would delete real usage).
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

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

    pub(crate) fn total_usage(total_tokens: u64) -> SessionUsage {
        SessionUsage {
            total_tokens,
            ..Default::default()
        }
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
            preview: None,
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

    /// The parent baseline only applies when the child's counters actually
    /// carried the parent's history over. Current Codex restarts the
    /// cumulative counters at fork, so the child's first reading is tiny;
    /// subtracting the parent baseline there deletes real usage (the July
    /// 2026 Stats-tab halving: a 4.15B-token fork served as 1.25M).
    #[test]
    fn codex_parent_baseline_skips_fresh_counter_forks() {
        let created_at = "2026-07-01T10:00:00Z";
        let cutoff = timestamp_sort_secs(created_at);
        let summary_with_first_event = |first: Option<u64>| CodexSessionListSummary {
            id: "codex-fork".to_string(),
            created_at: Some(created_at.to_string()),
            session_cwd: None,
            effective_cwd: None,
            model: None,
            lineage: SessionLineageMetadata {
                parent_id: Some("codex-parent".to_string()),
                ..Default::default()
            },
            provider: Some("Codex".to_string()),
            usage: total_usage(4_000_000),
            first_usage_event: first.map(|total| CodexUsageEvent {
                timestamp: Some(created_at.to_string()),
                usage: total_usage(total),
            }),
            daily_usage: BTreeMap::new(),
            goal: None,
            task: None,
            turns: 2,
            file_updated_at: None,
            bytes: 64,
            preview: None,
        };
        let mut baselines: HashMap<(String, i64), Option<SessionUsage>> = HashMap::new();
        baselines.insert(
            ("codex-parent".to_string(), cutoff),
            Some(total_usage(3_000_000)),
        );

        // Fresh-counter fork: first reading far below the baseline.
        let fresh = summary_with_first_event(Some(30));
        assert_eq!(codex_parent_baseline_for_summary(&fresh, &baselines), None);

        // Carryover fork: first reading contains the parent history.
        let carryover = summary_with_first_event(Some(3_000_050));
        assert_eq!(
            codex_parent_baseline_for_summary(&carryover, &baselines),
            Some(total_usage(3_000_000))
        );

        // No first reading at all → nothing proves carryover; don't subtract.
        let missing = summary_with_first_event(None);
        assert_eq!(
            codex_parent_baseline_for_summary(&missing, &baselines),
            None
        );
    }
}
