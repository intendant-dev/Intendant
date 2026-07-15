//! Per-backend session listing: the codex accumulator + index skeleton,
//! claude/gemini list rows, session-file lookup, and external detail entry.

use super::*;

#[derive(Default)]
pub(crate) struct CodexSessionListAccumulator {
    pub(crate) id: Option<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) session_cwd: Option<String>,
    pub(crate) turn_cwd: Option<String>,
    pub(crate) command_cwd: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) lineage: SessionLineageMetadata,
    pub(crate) provider: Option<String>,
    pub(crate) usage: SessionUsage,
    pub(crate) first_usage_event: Option<CodexUsageEvent>,
    // Deltas from events without a parseable timestamp; folded into the
    // file-mtime day bucket at finish(), matching how the old
    // event-replay path bucketed undated events.
    pub(crate) undated_usage: SessionUsage,
    pub(crate) daily_usage: BTreeMap<String, SessionUsage>,
    pub(crate) goal: Option<SessionGoal>,
    pub(crate) task_started_turns: u64,
    pub(crate) saw_user_message_event: bool,
    pub(crate) event_user_turns: Vec<Option<String>>,
    pub(crate) fallback_user_turns: Vec<Option<String>>,
    // Assistant prose for the row preview (message-typed response items
    // only — tool payloads have other types). User preview texts derive
    // from the turn vectors above at finish(), which already handle
    // injected-text filtering and thread rollbacks.
    pub(crate) preview_assistant_texts: Vec<String>,
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
                        if role == "assistant"
                            && self.preview_assistant_texts.len() < SESSION_PREVIEW_ROLE_SLOTS
                        {
                            let compacted = compact_text(&text, SESSION_PREVIEW_TEXT_CHARS);
                            if !compacted.is_empty() {
                                self.preview_assistant_texts.push(compacted);
                            }
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
        // User texts follow the same preference as `turns`: event-stream
        // user_message turns when present, response-item fallbacks
        // otherwise (both already rollback-adjusted and injection-filtered).
        // Rollout excerpts don't interleave the two role streams by
        // timestamp, so entries group users-first.
        let user_turns = if self.saw_user_message_event {
            &self.event_user_turns
        } else {
            &self.fallback_user_turns
        };
        let mut preview_builder = SessionPreviewBuilder::default();
        for text in user_turns.iter().flatten().take(SESSION_PREVIEW_ROLE_SLOTS) {
            preview_builder.push_user(text);
        }
        for text in &self.preview_assistant_texts {
            preview_builder.push_assistant(text);
        }
        let preview = preview_builder.into_value();
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
            preview,
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
    let key = session_list_cache_key("codex", path, SESSION_ROW_PREVIEW_FORMAT)?;
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

    let mut files = collect_recent_files_keyed(&codex.join("sessions"), ".jsonl", scan_limit);
    files.extend(collect_recent_files_keyed(
        &codex.join("archived_sessions"),
        ".jsonl",
        scan_limit,
    ));
    // Keys carried out of the walks: no re-stat per comparison.
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    files.truncate(scan_limit);
    let mut summaries = Vec::new();
    for (_, path) in files {
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
        if let Some(preview) = summary.preview.as_ref() {
            session["preview"] = preview.clone();
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

/// Incremental fold state for [`claude_session_list_row_from_file`]. An
/// ACTIVE transcript invalidates its (len, mtime, ino) row-cache key on
/// every append, which used to re-parse the whole multi-MB file per list
/// rebuild, per live session, for as long as the agent runs — the
/// dominant steady CPU sink of the catalog. The accumulator checkpoints
/// the fold + consumed byte offset per path, so an append parses only
/// the suffix; identity/length/prefix-hash checks downgrade any rewrite
/// or replacement to the full re-parse.
#[derive(Clone, Default)]
struct ClaudeRowAccumulator {
    identity: Option<crate::platform::FileIdentity>,
    /// Head-hash over `prefix_hash_bytes` (≤ 4 KiB, and never more than
    /// the consumed range, so resumed and fresh captures compare the
    /// same window).
    prefix_hash16: String,
    prefix_hash_bytes: usize,
    /// Second rewrite-detection window: hash of the last
    /// `min(4096, consumed_len)` bytes ENDING at the consumed offset.
    /// The head window alone cannot exclude a rewrite past the first
    /// 4 KiB that also grows the file; requiring both narrows the
    /// undetected residual to a head-and-tail-preserving growth rewrite
    /// (documented, same contract as the message-search cursor).
    consumed_tail_hash16: String,
    /// Offset just past the last COMPLETE line folded.
    consumed_len: u64,
    /// Complete lines folded so far — continues the historical 0-based
    /// `line-{idx}` usage-dedup keys across resumed parses.
    lines_consumed: u64,
    created_at: Option<String>,
    updated_at: Option<String>,
    session_cwd: Option<String>,
    cwd: Option<String>,
    task: Option<String>,
    model: Option<String>,
    usage: SessionUsage,
    daily_usage: BTreeMap<String, SessionUsage>,
    seen_usage: HashSet<String>,
    turns: u64,
    preview: SessionPreviewBuilder,
}

impl ClaudeRowAccumulator {
    fn fold_line(&mut self, line: &str) {
        let line_idx = self.lines_consumed;
        self.lines_consumed += 1;
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        self.created_at = self
            .created_at
            .take()
            .or_else(|| value_str(&obj, "timestamp"));
        self.updated_at = value_str(&obj, "timestamp").or(self.updated_at.take());
        if let Some(value) = value_str(&obj, "cwd") {
            if self.session_cwd.is_none() {
                self.session_cwd = Some(value.clone());
            }
            self.cwd = Some(value);
        }
        let record_type = obj.get("type").and_then(|v| v.as_str());
        let record_is_meta = obj.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false);
        if record_type == Some("user") {
            self.turns += 1;
            if self.task.is_none() {
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
                        self.task = Some(compact_text(user_text, 180));
                    }
                }
            }
            // Preview wants real prompts only: prose text blocks (a
            // tool_result record is also type=user), never meta records.
            if !record_is_meta {
                if let Some(content) = obj
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(message_prose_text)
                {
                    let user_text = content
                        .split(
                            crate::external_agent::claude_code::CLAUDE_CODE_BOOTSTRAP_ADDENDUM_MARKER,
                        )
                        .next()
                        .unwrap_or(&content)
                        .trim_end();
                    self.preview.push_user(user_text);
                }
            }
        }
        if record_type == Some("assistant") && !record_is_meta {
            if let Some(content) = obj
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(message_prose_text)
            {
                self.preview.push_assistant(&content);
            }
        }
        if let Some(msg) = obj.get("message") {
            if self.model.is_none() {
                self.model = msg
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            if let Some(parsed) = msg.get("usage").and_then(claude_usage_from_message_usage) {
                let key = value_str(&obj, "requestId")
                    .or_else(|| value_str(msg, "id"))
                    .unwrap_or_else(|| format!("line-{line_idx}"));
                if self.seen_usage.insert(key) {
                    self.usage.add(parsed);
                    if let Some(day) =
                        usage_day_from_timestamp(value_str(&obj, "timestamp").as_deref())
                    {
                        self.daily_usage.entry(day).or_default().add(parsed);
                    }
                }
            }
        }
    }

    /// Whether the checkpoint may resume against the file's current state:
    /// same reliable identity, no truncation, unchanged consumed head AND
    /// unchanged consumed tail. Requiring both hash windows narrows the
    /// undetected rewrite to one that preserves the first 4 KiB, the last
    /// 4 KiB before the consumed offset, and grows the file — the same
    /// documented residual as the message-search cursor.
    fn resumable_for(&self, path: &Path, current_len: u64) -> bool {
        if current_len < self.consumed_len {
            return false;
        }
        let identity_ok = match (
            self.identity,
            crate::platform::FileIdentity::from_path(path).ok(),
        ) {
            (Some(saved), Some(current)) => {
                saved.is_reliable() && current.is_reliable() && saved == current
            }
            _ => false,
        };
        if !identity_ok {
            return false;
        }
        if self.consumed_tail_hash16.is_empty()
            || crate::message_search::cursor::tail_hash16_ending_at(path, self.consumed_len)
                .as_deref()
                != Some(self.consumed_tail_hash16.as_str())
        {
            return false;
        }
        crate::message_search::cursor::prefix_hash16_bytes(path, self.prefix_hash_bytes).as_deref()
            == Some(self.prefix_hash16.as_str())
    }

    fn render(self, path: &Path, session_id: String) -> serde_json::Value {
        let effective_cwd = self.cwd.or_else(|| self.session_cwd.clone());
        let project_root =
            derive_project_root_from_cwd(self.session_cwd.as_deref().or(effective_cwd.as_deref()));
        let mut session = external_session_json(
            "claude-code",
            "Claude Code",
            session_id.clone(),
            session_id,
            self.created_at
                .or_else(|| self.updated_at.clone())
                .or_else(|| file_mtime_string(path)),
            file_mtime_string(path).or(self.updated_at),
            None,
            self.task,
            "Claude Code",
            self.model.clone(),
            self.turns,
            project_root,
            effective_cwd,
            Some(path.to_string_lossy().to_string()),
            file_size(path),
        );
        apply_session_usage(&mut session, self.usage, self.model.as_deref());
        apply_session_daily_usage(&mut session, &self.daily_usage, self.model.as_deref());
        if let Some(preview) = self.preview.into_value() {
            session["preview"] = preview;
        }
        session
    }
}

/// Retained checkpoints for the incremental claude row fold, keyed by
/// transcript path. Bounded like the sibling row caches.
fn claude_row_accumulators() -> &'static Mutex<HashMap<PathBuf, ClaudeRowAccumulator>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, ClaudeRowAccumulator>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const CLAUDE_ROW_ACCUMULATOR_CAP: usize = 256;

pub(crate) fn claude_session_list_row_from_file(path: &Path) -> Option<serde_json::Value> {
    let key = session_list_cache_key("claude-code", path, SESSION_ROW_PREVIEW_FORMAT)?;
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
    let current_len = std::fs::metadata(path).ok()?.len();
    let checkpoint = claude_row_accumulators()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
    let mut acc = match checkpoint {
        Some(saved) if saved.resumable_for(path, current_len) => saved,
        _ => ClaudeRowAccumulator::default(),
    };
    let consumed = crate::message_search::cursor::for_each_complete_line_from(
        path,
        acc.consumed_len,
        |line| acc.fold_line(line),
    )
    .ok()?;
    acc.consumed_len = consumed;
    acc.identity = crate::platform::FileIdentity::from_path(path).ok();
    acc.prefix_hash_bytes =
        consumed.min(crate::message_search::cursor::PREFIX_HASH_BYTES as u64) as usize;
    acc.prefix_hash16 =
        crate::message_search::cursor::prefix_hash16_bytes(path, acc.prefix_hash_bytes)?;
    acc.consumed_tail_hash16 =
        crate::message_search::cursor::tail_hash16_ending_at(path, consumed)?;

    // Parity with the historical whole-file `.lines()` read: an
    // unterminated trailing line still renders into THIS row, but never
    // into the retained checkpoint (its bytes re-read once complete).
    let session = match read_unterminated_tail(path, consumed) {
        UnterminatedTail::None => acc.clone().render(path, session_id),
        UnterminatedTail::Tail(tail) => {
            let mut render = acc.clone();
            for piece in tail.split('\n') {
                let piece = piece.trim_end_matches('\r');
                if !piece.is_empty() {
                    render.fold_line(piece);
                }
            }
            render.render(path, session_id)
        }
        // A valid final record can legitimately exceed the tail cap (one
        // huge unterminated paste): silently omitting it would leave the
        // row permanently short on a stable file. Render such rows from
        // a full streaming `.lines()` pass instead — the checkpoint
        // (complete lines only) stays valid either way.
        UnterminatedTail::Oversized => claude_row_full_render(path, session_id)?,
    };

    {
        let mut cache = claude_row_accumulators()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.len() >= CLAUDE_ROW_ACCUMULATOR_CAP && !cache.contains_key(path) {
            cache.clear();
        }
        cache.insert(path.to_path_buf(), acc);
    }
    store_session_list_row(key, &session);
    Some(session)
}

/// In-memory bound on the unterminated-tail fast path; larger tails take
/// the full streaming render instead of being buffered here.
const CLAUDE_ROW_TAIL_CAP_BYTES: u64 = 4 * 1024 * 1024;

enum UnterminatedTail {
    /// The file ends exactly at the consumed offset.
    None,
    /// Bytes past the last complete line (a live writer mid-append).
    Tail(String),
    /// More tail bytes than the in-memory cap: the caller must render
    /// via the full streaming pass, never omit the record.
    Oversized,
}

fn read_unterminated_tail(path: &Path, from: u64) -> UnterminatedTail {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return UnterminatedTail::None;
    };
    let Ok(metadata) = file.metadata() else {
        return UnterminatedTail::None;
    };
    let len = metadata.len();
    if len <= from {
        return UnterminatedTail::None;
    }
    if len - from > CLAUDE_ROW_TAIL_CAP_BYTES {
        return UnterminatedTail::Oversized;
    }
    if file.seek(SeekFrom::Start(from)).is_err() {
        return UnterminatedTail::None;
    }
    let mut buf = Vec::with_capacity((len - from) as usize);
    if file
        .take(CLAUDE_ROW_TAIL_CAP_BYTES)
        .read_to_end(&mut buf)
        .is_err()
        || buf.is_empty()
    {
        return UnterminatedTail::None;
    }
    UnterminatedTail::Tail(String::from_utf8_lossy(&buf).into_owned())
}

/// Full streaming render including a final unterminated line of any size
/// (the historical `.lines()` semantics) — the oversized-tail fallback.
fn claude_row_full_render(path: &Path, session_id: String) -> Option<serde_json::Value> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path).ok()?;
    let mut acc = ClaudeRowAccumulator::default();
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        acc.fold_line(&line);
    }
    Some(acc.render(path, session_id))
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
    let key = session_list_cache_key(
        "gemini",
        path,
        format!("{SESSION_ROW_PREVIEW_FORMAT}:{roots_fingerprint}"),
    )?;
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
    let mut preview = SessionPreviewBuilder::default();
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
                let text = msg
                    .get("text")
                    .or_else(|| msg.get("message"))
                    .or_else(|| msg.get("content"))
                    .and_then(message_content_text);
                if let Some(text) = text {
                    if task.is_none() {
                        task = Some(compact_text(&text, 180));
                    }
                    preview.push_user(&text);
                }
            } else if role == "assistant" || role == "model" {
                if let Some(text) = msg
                    .get("text")
                    .or_else(|| msg.get("message"))
                    .or_else(|| msg.get("content"))
                    .and_then(message_prose_text)
                {
                    preview.push_assistant(&text);
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
    if let Some(preview) = preview.into_value() {
        session["preview"] = preview;
    }
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

// Consolidated (message-search F3 phase 2): the streaming id reader moved
// to `external_agent::codex::rollout` (this file's copy was the canonical
// body), and the finder delegates to `codex_history`'s engine — which
// ported this file's filename fast path and adds the wrapper-index
// stored-path cache, so catalog lookups stop rescanning migrated homes.
pub(crate) use crate::external_agent::codex::rollout::codex_session_file_id;

pub(crate) fn find_codex_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    crate::codex_history::find_codex_session_file_in(&codex_dir(home), home, session_id)
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
    let entries = external_session_entries_from_home(home, source, session_id)?;
    let effective_limit = limit.or(Some(EXTERNAL_SESSION_DETAIL_DEFAULT_ENTRY_LIMIT));
    let page = session_detail_page_entries(entries, effective_limit, before);
    Some(external_session_detail_body(session_id, page, None))
}

/// Shared assembly of the external session-detail body (the historical
/// field set — per-entry websocket text compaction included) plus the
/// optional additive `locate` object the anchored read (`locate.rs`)
/// attaches.
pub(crate) fn external_session_detail_body(
    session_id: &str,
    mut page: SessionDetailPageEntries,
    locate: Option<serde_json::Value>,
) -> String {
    for entry in &mut page.entries {
        compact_replay_entry_text_fields_for_websocket(entry);
    }

    let mut body = serde_json::json!({
        "session_id": session_id,
        "transcript_semantics": EXTERNAL_TRANSCRIPT_SEMANTICS,
        "entries": page.entries,
        "total_entries": page.total_entries,
        "page_start": page.page_start,
        "page_end": page.page_end,
        "has_older": page.page_start > 0,
        "frames": [],
    });
    if let Some(locate) = locate {
        body["locate"] = locate;
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn codex_row_preview_pairs_user_turns_with_assistant_prose() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("07")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let id = "019e37ae-preview-pairs";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:00Z",
                "type": "session_meta",
                "payload": {"id": id, "timestamp": "2026-07-07T10:00:00Z", "cwd": "/repo"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:01Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Port the parser"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:02Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Starting with the lexer."}]
                }
            }),
            // Tool payloads are not message-typed — never preview material.
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:03Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "arguments": "{\"command\":[\"cargo\",\"test\"]}"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:04Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Now the emitter"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:05Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Emitter done."}]
                }
            }),
            // Both assistant slots taken — must not appear.
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:06Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Extra reply."}]
                }
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-07-07T10-00-00-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        let preview = session.get("preview").and_then(|v| v.as_array()).unwrap();
        let flat: Vec<(&str, &str)> = preview
            .iter()
            .map(|e| {
                (
                    e.get("role").and_then(|v| v.as_str()).unwrap(),
                    e.get("text").and_then(|v| v.as_str()).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            flat,
            vec![
                ("user", "Port the parser"),
                ("user", "Now the emitter"),
                ("assistant", "Starting with the lexer."),
                ("assistant", "Emitter done."),
            ]
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
                    "model": "gpt-5.6",
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
                            "cache_write_tokens": 200,
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
        assert_eq!(session["cache_creation_tokens"].as_u64(), Some(200));
        assert_eq!(session["total_tokens"].as_u64(), Some(1250));
        let cost = session["estimated_cost"].as_f64().unwrap();
        assert!((cost - 0.01095).abs() < 1e-12, "unexpected cost {cost}");
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

    /// A fork whose rollout restarts the cumulative counters at zero (what
    /// current Codex writes) must keep its full usage: the parent baseline
    /// was never part of the child's readings, so nothing is subtracted.
    #[test]
    fn list_codex_sessions_keeps_fresh_counter_fork_usage() {
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_id = "019e37c5-parent-fresh-thread";
        let child_id = "019e37c5-child-fresh-thread";
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
        // The child forks after the parent's 2400-token reading but its own
        // counters restart: first reading 100, final reading 400.
        let child_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T21:12:00Z",
                "type": "session_meta",
                "payload": {
                    "id": child_id,
                    "forked_from_id": parent_id,
                    "timestamp": "2026-05-17T21:12:00Z",
                    "model": "gpt-5.4",
                    "model_provider": "openai"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:12:10Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 80,
                            "cached_input_tokens": 20,
                            "output_tokens": 20,
                            "total_tokens": 100
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T21:12:40Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 300,
                            "cached_input_tokens": 120,
                            "output_tokens": 100,
                            "total_tokens": 400
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
            sessions_dir.join(format!("rollout-2026-05-17T21-12-00-{child_id}.jsonl")),
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
        // Full child usage survives — no parent baseline subtraction.
        assert_eq!(child["total_tokens"].as_u64(), Some(400));
        assert_eq!(child["prompt_tokens"].as_u64(), Some(300));
        assert_eq!(child["cached_tokens"].as_u64(), Some(120));
        assert_eq!(child["completion_tokens"].as_u64(), Some(100));
        let daily = child["daily_usage"]
            .as_array()
            .expect("child daily usage buckets");
        let daily_total: u64 = daily
            .iter()
            .filter_map(|entry| entry.get("total_tokens").and_then(|v| v.as_u64()))
            .sum();
        assert_eq!(daily_total, 400);
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
    fn claude_row_preview_takes_prose_and_skips_tool_and_meta_records() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home.path().join(".claude").join("projects").join("-tmp-p");
        std::fs::create_dir_all(&project_dir).unwrap();

        let lines = [
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:00Z",
                "type": "user",
                "cwd": "/tmp/p",
                "message": {"content": "Refactor the parser"}
            }),
            // Meta records are typed user but are not conversation.
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:01Z",
                "type": "user",
                "isMeta": true,
                "message": {"content": "Caveat: system housekeeping text"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:02Z",
                "type": "assistant",
                "message": {"content": [
                    {"type": "text", "text": "Starting with the tokenizer."},
                    {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}}
                ]}
            }),
            // Tool results come back as type=user; content is tool output.
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:03Z",
                "type": "user",
                "message": {"content": [
                    {"type": "tool_result", "content": "test result: ok. 100 passed"}
                ]}
            }),
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:04Z",
                "type": "user",
                "message": {"content": "Now handle unicode too"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-07T10:00:05Z",
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": "Unicode handled."}]}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(project_dir.join("preview-abc.jsonl"), contents).unwrap();

        let sessions = list_claude_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("preview-abc"))
            .expect("claude session should be listed");
        let preview = session.get("preview").and_then(|v| v.as_array()).unwrap();
        let flat: Vec<(&str, &str)> = preview
            .iter()
            .map(|e| {
                (
                    e.get("role").and_then(|v| v.as_str()).unwrap(),
                    e.get("text").and_then(|v| v.as_str()).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            flat,
            vec![
                ("user", "Refactor the parser"),
                ("assistant", "Starting with the tokenizer."),
                ("user", "Now handle unicode too"),
                ("assistant", "Unicode handled."),
            ]
        );
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
    fn claude_row_fold_resumes_on_append_instead_of_reparsing() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-tmp-inc");
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join("fold-resume-abc.jsonl");

        let first = serde_json::json!({
            "timestamp": "2026-07-14T10:00:00Z",
            "type": "user",
            "cwd": "/tmp/inc",
            "message": {"content": "first prompt"}
        });
        std::fs::write(&path, format!("{first}\n")).unwrap();
        let row = claude_session_list_row_from_file(&path).unwrap();
        assert_eq!(row["turns"].as_u64(), Some(1));

        // Poison the retained checkpoint's fold state: if the append path
        // resumes from the checkpoint (as it must), the poison shows in
        // the next row; a silent full re-parse would erase it.
        {
            let mut cache = claude_row_accumulators()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let acc = cache.get_mut(&path).expect("checkpoint retained");
            acc.turns += 100;
        }
        let second = serde_json::json!({
            "timestamp": "2026-07-14T10:01:00Z",
            "type": "user",
            "message": {"content": "second prompt"}
        });
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{second}").unwrap();
        }
        let row = claude_session_list_row_from_file(&path).unwrap();
        assert_eq!(
            row["turns"].as_u64(),
            Some(102),
            "append must fold into the retained checkpoint (1 + poison 100 + 1)"
        );
    }

    #[test]
    fn claude_row_fold_matches_a_full_parse_and_detects_rewrites() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-tmp-par");
        std::fs::create_dir_all(&project_dir).unwrap();
        let incremental = project_dir.join("fold-parity-abc.jsonl");
        let fresh = project_dir.join("fold-parity-fresh.jsonl");

        let user = serde_json::json!({
            "timestamp": "2026-07-14T11:00:00Z",
            "type": "user",
            "cwd": "/tmp/par",
            "message": {"content": "count my usage"}
        });
        let assistant = serde_json::json!({
            "timestamp": "2026-07-14T11:00:02Z",
            "type": "assistant",
            "requestId": "req-1",
            "message": {
                "id": "msg-1",
                "model": "claude-sonnet-4-6",
                "usage": {"input_tokens": 100, "output_tokens": 200}
            }
        });
        let follow_up = serde_json::json!({
            "timestamp": "2026-07-14T11:01:00Z",
            "type": "user",
            "message": {"content": "and again"}
        });
        let assistant_two = serde_json::json!({
            "timestamp": "2026-07-14T11:01:02Z",
            "type": "assistant",
            "requestId": "req-2",
            "message": {
                "id": "msg-2",
                "model": "claude-sonnet-4-6",
                "usage": {"input_tokens": 10, "output_tokens": 20},
                "content": [{"type": "text", "text": "done again"}]
            }
        });

        // Incremental: parse the prefix, then fold the appended suffix.
        std::fs::write(&incremental, format!("{user}\n{assistant}\n")).unwrap();
        claude_session_list_row_from_file(&incremental).unwrap();
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&incremental)
                .unwrap();
            writeln!(file, "{follow_up}").unwrap();
            writeln!(file, "{assistant_two}").unwrap();
        }
        let folded = claude_session_list_row_from_file(&incremental).unwrap();

        // Fresh: the same final bytes parsed from scratch.
        std::fs::write(
            &fresh,
            format!("{user}\n{assistant}\n{follow_up}\n{assistant_two}\n"),
        )
        .unwrap();
        let full = claude_session_list_row_from_file(&fresh).unwrap();

        for field in [
            "turns",
            "task",
            "model",
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "preview",
            "cwd",
            "project_root",
            "daily_usage",
        ] {
            assert_eq!(
                folded.get(field),
                full.get(field),
                "fold/full divergence on {field}"
            );
        }

        // A rewrite (changed head) must fall back to the full re-parse:
        // nothing folded from the old content may survive.
        let rewritten_user = serde_json::json!({
            "timestamp": "2026-07-14T12:00:00Z",
            "type": "user",
            "cwd": "/tmp/par",
            "message": {"content": "brand new history"}
        });
        std::fs::write(&incremental, format!("{rewritten_user}\n")).unwrap();
        let rewritten = claude_session_list_row_from_file(&incremental).unwrap();
        assert_eq!(rewritten["turns"].as_u64(), Some(1));
        assert_eq!(
            rewritten["task"].as_str(),
            Some("brand new history"),
            "stale folded state must not survive a rewrite"
        );
    }

    #[test]
    fn claude_row_post_prefix_rewrite_that_grows_falls_back_to_full_parse() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home.path().join(".claude").join("projects").join("-tmp-rw");
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join("fold-rewrite-abc.jsonl");

        // First record alone fills the 4 KiB head window, so a rewrite
        // past it leaves the prefix hash intact.
        let padded = serde_json::json!({
            "timestamp": "2026-07-15T09:00:00Z",
            "type": "user",
            "cwd": "/tmp/rw",
            "message": {"content": "p".repeat(5000)}
        });
        let old_second = serde_json::json!({
            "timestamp": "2026-07-15T09:01:00Z",
            "type": "user",
            "message": {"content": "old second"}
        });
        std::fs::write(&path, format!("{padded}\n{old_second}\n")).unwrap();
        let row = claude_session_list_row_from_file(&path).unwrap();
        assert_eq!(row["turns"].as_u64(), Some(2));

        // Rewrite past the prefix window AND grow the file (same inode:
        // fs::write truncates in place). A checkpoint resume here would
        // fold suffix bytes onto stale totals; the consumed-tail window
        // must force the full re-parse.
        let new_second = serde_json::json!({
            "timestamp": "2026-07-15T09:02:00Z",
            "type": "user",
            "message": {"content": "new second"}
        });
        let third = serde_json::json!({
            "timestamp": "2026-07-15T09:03:00Z",
            "type": "user",
            "message": {"content": "third prompt"}
        });
        std::fs::write(&path, format!("{padded}\n{new_second}\n{third}\n")).unwrap();
        let row = claude_session_list_row_from_file(&path).unwrap();
        assert_eq!(
            row["turns"].as_u64(),
            Some(3),
            "stale checkpoint state must not survive a post-prefix rewrite"
        );
        let preview = row["preview"].to_string();
        assert!(preview.contains("new second"), "{preview}");
        assert!(!preview.contains("old second"), "{preview}");
    }

    #[test]
    fn claude_row_renders_an_oversized_unterminated_final_record() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home.path().join(".claude").join("projects").join("-tmp-ov");
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join("oversized-tail-abc.jsonl");

        let first = serde_json::json!({
            "timestamp": "2026-07-15T10:00:00Z",
            "type": "user",
            "cwd": "/tmp/ov",
            "message": {"content": "small first"}
        });
        // A VALID final record larger than the tail fast-path cap,
        // WITHOUT a trailing newline: it must still count (the old cap
        // silently dropped it from the row forever on a stable file).
        let huge = serde_json::json!({
            "timestamp": "2026-07-15T10:01:00Z",
            "type": "user",
            "message": {"content": "h".repeat(CLAUDE_ROW_TAIL_CAP_BYTES as usize + 4096)}
        });
        std::fs::write(&path, format!("{first}\n{huge}")).unwrap();

        let row = claude_session_list_row_from_file(&path).unwrap();
        assert_eq!(
            row["turns"].as_u64(),
            Some(2),
            "an oversized unterminated final record must render via the full parse"
        );
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
}
