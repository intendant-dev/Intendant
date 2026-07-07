//! The context-rewind anchor catalog: rollout user turns and managed-context
//! edit records, anchor list/inspect/scan over the session rollout, compact
//! catalog serialization, outcome keys, usage extraction from rollout entries,
//! anchor matching/summaries, primer-fact pruning, and pressure bands.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RolloutUserTurn {
    index: u32,
    line: usize,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedContextEditBranchTarget {
    pub(crate) record_id: String,
    pub(crate) parent_thread_id: String,
    pub(crate) recovery_rollout_path: PathBuf,
    pub(crate) source_turn_count: u32,
    pub(crate) target_turn_text: String,
}

pub(crate) fn rollout_user_turns(rollout_path: &Path) -> io::Result<Vec<RolloutUserTurn>> {
    let file = std::fs::File::open(rollout_path)?;
    let reader = io::BufReader::new(file);
    let mut saw_user_message_event = false;
    let mut event_turns = Vec::new();
    let mut fallback_turns = Vec::new();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match entry.get("type").and_then(|value| value.as_str()) {
            Some("event_msg") => {
                let Some(payload) = entry.get("payload") else {
                    continue;
                };
                match payload.get("type").and_then(|value| value.as_str()) {
                    Some("user_message") => {
                        saw_user_message_event = true;
                        if let Some(text) = payload
                            .get("message")
                            .and_then(|value| value.as_str())
                            .filter(|text| !is_codex_injected_user_text_for_main(text))
                        {
                            push_rollout_user_turn(
                                &mut event_turns,
                                line_index.saturating_add(1),
                                text.to_string(),
                            );
                        }
                    }
                    Some("thread_rolled_back") => {
                        let turns_to_drop = payload
                            .get("num_turns")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0) as u32;
                        truncate_rollout_user_turns(&mut event_turns, turns_to_drop);
                        truncate_rollout_user_turns(&mut fallback_turns, turns_to_drop);
                    }
                    _ => {}
                }
            }
            Some("response_item") => {
                let Some(payload) = entry.get("payload") else {
                    continue;
                };
                if let Some(text) = codex_payload_user_text(payload) {
                    push_rollout_user_turn(&mut fallback_turns, line_index.saturating_add(1), text);
                }
            }
            _ => {}
        }
    }

    Ok(if saw_user_message_event {
        event_turns
    } else {
        fallback_turns
    })
}

pub(crate) fn push_rollout_user_turn(turns: &mut Vec<RolloutUserTurn>, line: usize, text: String) {
    turns.push(RolloutUserTurn {
        index: turns.len().saturating_add(1) as u32,
        line,
        text,
    });
}

pub(crate) fn truncate_rollout_user_turns(turns: &mut Vec<RolloutUserTurn>, turns_to_drop: u32) {
    if turns_to_drop == 0 || turns.is_empty() {
        return;
    }
    let keep = turns.len().saturating_sub(turns_to_drop as usize);
    turns.truncate(keep);
}

pub(crate) fn managed_context_edit_record_dirs(
    log_dir: &Path,
    thread_id: &str,
    session_id: Option<&str>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    let mut push_dir = |path: PathBuf| {
        if !path.is_dir() {
            return;
        }
        let key = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .to_string();
        if seen.insert(key) {
            dirs.push(path);
        }
    };

    push_dir(log_dir.to_path_buf());
    let Some(home) = external_wrapper_index::home_from_log_dir(log_dir) else {
        return dirs;
    };

    let ids = [Some(thread_id), session_id];
    for id in ids
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        push_dir(
            crate::platform::intendant_home_in(&home)
                .join("logs")
                .join(id),
        );
        for record in external_wrapper_index::wrappers_for(&home, "codex", id) {
            push_dir(PathBuf::from(record.log_path));
        }
    }

    let logs_dir = crate::platform::intendant_home_in(&home).join("logs");
    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && context_rewind::records_dir(&path).is_dir() {
                push_dir(path);
            }
        }
    }

    dirs
}

pub(crate) fn managed_context_edit_records(
    log_dir: &Path,
    thread_id: &str,
    session_id: Option<&str>,
) -> Result<Vec<context_rewind::ContextRewindRecord>, String> {
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for dir in managed_context_edit_record_dirs(log_dir, thread_id, session_id) {
        let mut from_dir = context_rewind::list_records(&dir).map_err(|err| {
            format!(
                "failed to list managed-context rewind records in {}: {err}",
                dir.display()
            )
        })?;
        from_dir.retain(|record| {
            record.thread_id == thread_id
                || session_id
                    .is_some_and(|session_id| record.session_id.as_deref() == Some(session_id))
        });
        for record in from_dir {
            let key = format!(
                "{}\0{}\0{}",
                record.record_id,
                record.thread_id,
                record
                    .recovery_rollout_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
            if seen.insert(key) {
                records.push(record);
            }
        }
    }
    records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(records)
}

pub(crate) fn managed_context_edit_text_matches(recorded: &str, original: Option<&str>) -> bool {
    let Some(original) = original.map(str::trim).filter(|text| !text.is_empty()) else {
        return true;
    };
    recorded.trim() == original
}

pub(crate) fn resolve_managed_context_edit_branch_target(
    log_dir: &Path,
    thread_id: &str,
    session_id: Option<&str>,
    user_turn_index: u32,
    original_text: Option<&str>,
) -> Result<Option<ManagedContextEditBranchTarget>, String> {
    if user_turn_index == 0 {
        return Ok(None);
    }
    let records = managed_context_edit_records(log_dir, thread_id, session_id)?;
    let mut text_mismatches = Vec::new();

    for record in records {
        let matches_thread = record.thread_id == thread_id
            || session_id
                .is_some_and(|session_id| record.session_id.as_deref() == Some(session_id));
        if !matches_thread {
            continue;
        }
        let Some(recovery_rollout_path) = record.recovery_rollout_path.as_deref() else {
            continue;
        };
        let turns = rollout_user_turns(recovery_rollout_path).map_err(|err| {
            format!(
                "failed to inspect archived rollout for rewind record {}: {err}",
                record.record_id
            )
        })?;
        let Some(turn) = turns.get(user_turn_index.saturating_sub(1) as usize) else {
            continue;
        };
        if !managed_context_edit_text_matches(&turn.text, original_text) {
            text_mismatches.push(record.record_id.clone());
            continue;
        }
        return Ok(Some(ManagedContextEditBranchTarget {
            record_id: record.record_id,
            parent_thread_id: record.thread_id,
            recovery_rollout_path: recovery_rollout_path.to_path_buf(),
            source_turn_count: turns.len() as u32,
            target_turn_text: turn.text.clone(),
        }));
    }

    if !text_mismatches.is_empty() {
        return Err(format!(
            "found archived managed-context rollout(s) containing user turn {}, but none matched the clicked message text; refusing to branch from an ambiguous stale edit",
            user_turn_index
        ));
    }
    Ok(None)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContextRewindAnchorCatalogEntry {
    pub(crate) ordinal: usize,
    pub(crate) item_id: String,
    pub(crate) first_line: usize,
    pub(crate) last_line: usize,
    pub(crate) first_item_type: String,
    pub(crate) last_item_type: String,
    /// Whether the anchor's last item was emitted by the model (assistant
    /// message, reasoning, tool *call*) rather than appended by the runtime
    /// (tool *output*, user/developer message). Model-emitted items are
    /// covered by their own response's token report; runtime-appended items
    /// are only covered by a report from a *later* model response.
    #[serde(skip_serializing)]
    pub(crate) last_item_is_model: bool,
    pub(crate) positions: Vec<&'static str>,
    pub(crate) position_hint: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) names: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) roles: Vec<String>,
    pub(crate) summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_usage_at_or_after_anchor: Option<u64>,
    /// Last backend report that measured a strict prefix of this anchor
    /// (its response began at or before the anchor's first line). Real
    /// lower bound for what any cut keeping this anchor's prefix retains;
    /// char-based prefix estimates understate because they cannot see
    /// instructions or tool specs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_usage_before_anchor: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rewind_only_limit_at_or_after_anchor: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recommended_rewind_limit_at_or_after_anchor: Option<u64>,
    #[serde(skip_serializing)]
    pub(crate) prefix_estimated_tokens_before_anchor: Option<u64>,
    #[serde(skip_serializing)]
    pub(crate) prefix_estimated_tokens_after_anchor: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) approx_pruned_tokens_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) approx_pruned_tokens_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prefix_tokens_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_rewind_usage_after_anchor: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_rewind_limit_after_anchor: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recovery_eligible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recovery_eligible_positions: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) density_eligible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) density_eligible_positions: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) managed_context_recovery_start_line: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContextRewindAnchorCompactEntry {
    ordinal: usize,
    item_id: String,
    item_type: String,
    positions: Vec<&'static str>,
    position_hint: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    names: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    roles: Vec<String>,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    density_eligible_positions: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    approx_pruned_tokens_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    approx_pruned_tokens_after: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContextRewindAnchorCatalog {
    total: usize,
    filtered_total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    query: Option<String>,
    include_management_tools: bool,
    include_non_recovery: bool,
    recovery_candidates_only: bool,
    density_candidates_only: bool,
    pruning_estimates_included: bool,
    anchors: Vec<ContextRewindAnchorCatalogEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContextRewindAnchorCompactCatalog {
    catalog_format: &'static str,
    total: usize,
    filtered_total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    output_cap_bytes: usize,
    output_truncated: bool,
    query: Option<String>,
    include_management_tools: bool,
    include_non_recovery: bool,
    recovery_candidates_only: bool,
    density_candidates_only: bool,
    pruning_estimates_included: bool,
    /// True when this is a re-listing of the default page inside one
    /// managed-context stall: the rows are unchanged, so the model should
    /// commit instead of listing again.
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat_listing: Option<bool>,
    /// True when the eligible catalog itself is empty: no anchor remains
    /// that a rewind could legally target.
    #[serde(skip_serializing_if = "Option::is_none")]
    no_eligible_anchors: Option<bool>,
    /// Set with an empty page to say why it is empty
    /// (`no_eligible_anchors`, `query_unmatched`, `offset_past_end`).
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_page_reason: Option<&'static str>,
    /// Plain-language direction for the repeat/empty cases above.
    #[serde(skip_serializing_if = "Option::is_none")]
    notice: Option<String>,
    anchors: Vec<ContextRewindAnchorCompactEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContextRewindAnchorInspectItem {
    line: usize,
    item_type: String,
    item_ids: Vec<String>,
    names: Vec<String>,
    roles: Vec<String>,
    summary: String,
    anchor_span: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ContextRewindAnchorInspection {
    rollout_path: String,
    anchor: ContextRewindAnchorCatalogEntry,
    radius: usize,
    context: Vec<ContextRewindAnchorInspectItem>,
    usage: &'static str,
}

pub(crate) const CONTEXT_REWIND_ANCHOR_LIST_DEFAULT_LIMIT: usize = 5;
pub(crate) const CONTEXT_REWIND_ANCHOR_LIST_MAX_LIMIT: usize = 10;
pub(crate) const CONTEXT_REWIND_ANCHOR_INSPECT_DEFAULT_RADIUS: usize = 2;
pub(crate) const CONTEXT_REWIND_ANCHOR_INSPECT_MAX_RADIUS: usize = 5;
pub(crate) const CONTEXT_REWIND_ANCHOR_COMPACT_SUMMARY_LIMIT: usize = 96;
pub(crate) const CONTEXT_REWIND_ANCHOR_COMPACT_TEXT_LIST_LIMIT: usize = 4;
pub(crate) const CONTEXT_REWIND_ANCHOR_COMPACT_TEXT_ITEM_LIMIT: usize = 48;
pub(crate) const CONTEXT_REWIND_ANCHOR_COMPACT_OUTPUT_MAX_BYTES: usize = 6_000;
pub(crate) const CONTEXT_REWIND_ANCHOR_SUMMARY_LIMIT: usize = 120;
pub(crate) const CONTEXT_REWIND_ANCHOR_MERGED_SUMMARY_LIMIT: usize = 160;
pub(crate) const CONTEXT_REWIND_RECOVERY_MIN_RESUME_HEADROOM_TOKENS: u64 = 8_000;
pub(crate) const CONTEXT_REWIND_PRIOR_PRIMER_FACT_LIMIT: usize = 12_000;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContextRewindBackendUsageAtLine {
    pub(crate) line: usize,
    pub(crate) used_tokens: u64,
    pub(crate) rewind_only_limit: u64,
    /// First rollout line of the model response this report measured. The
    /// report's request input only covered items *strictly before* this
    /// line; anything at or after it (e.g. a `function_call_output` that is
    /// appended before the response's `token_count` is persisted) was not in
    /// the measured context. Defaults to `line` (covers everything before
    /// itself) when the response boundary is unknown.
    ///
    /// Consecutive model responses with no interleaved runtime item merge
    /// into one run, so this is an *under*-approximation: safe for "did this
    /// report consume item X" checks, unsafe as a prefix floor on its own.
    pub(crate) response_start_line: usize,
    /// True only for the first report after a model run began: that report
    /// measured exactly the context preceding `response_start_line`. Later
    /// reports in a merged run measured more than the run-start prefix.
    pub(crate) measures_prefix_exactly: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingContextRewindOutcome {
    key: String,
    prior_usage: Option<ContextRewindBackendUsageAtLine>,
}

pub(crate) fn context_rewind_anchor_outcome_key(
    item_id: &str,
    position: external_agent::RollbackAnchorPosition,
) -> String {
    format!("{}\0{}", position.as_str(), item_id)
}

pub(crate) fn list_context_rewind_anchors_from_rollout(
    source_rollout_path: &Path,
    params: &serde_json::Value,
) -> Result<String, String> {
    let offset = params
        .get("offset")
        .or_else(|| params.get("start_index"))
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let limit = params
        .get("limit")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(CONTEXT_REWIND_ANCHOR_LIST_DEFAULT_LIMIT)
        .clamp(1, CONTEXT_REWIND_ANCHOR_LIST_MAX_LIMIT);
    let query = params
        .get("query")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let include_management_tools = params
        .get("include_management_tools")
        .or_else(|| params.get("includeManagementTools"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let include_non_recovery = params
        .get("include_non_recovery")
        .or_else(|| params.get("includeNonRecovery"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let recovery_candidates_only = !include_non_recovery;
    let density_candidates_only = params
        .get("density_candidates_only")
        .or_else(|| params.get("densityCandidatesOnly"))
        .or_else(|| params.get("density_mode"))
        .or_else(|| params.get("densityMode"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let reverse = params
        .get("reverse")
        .and_then(|value| value.as_bool())
        .unwrap_or_else(|| {
            params
                .get("direction")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .is_some_and(|direction| direction.eq_ignore_ascii_case("backward"))
        });
    let include_pruning_estimates = params
        .get("include_pruning_estimates")
        .or_else(|| params.get("includePruningEstimates"))
        .and_then(|value| value.as_bool())
        .unwrap_or(reverse || query.is_some());
    let compact_catalog = context_rewind_anchor_use_compact_catalog(params, include_non_recovery);

    let mut anchors = scan_context_rewind_anchor_catalog(source_rollout_path).map_err(|err| {
        format!(
            "failed to inspect rewind anchors in {}: {err}",
            source_rollout_path.display()
        )
    })?;
    let scanned_total = anchors.len();
    let trailing_listing_calls = context_rewind_trailing_listing_calls(&anchors);
    if !include_management_tools {
        anchors.retain(|anchor| !context_rewind_anchor_is_management_tool(anchor));
    }
    // Model-visible accounting must be idempotent across listing-only growth:
    // when management calls are hidden from rows they are excluded from
    // `total` too, so a recovery stall (where listings/status polls are the
    // only thread growth) re-lists with stable counts instead of a catalog
    // that grows by one per listing call.
    let total = if include_management_tools {
        scanned_total
    } else {
        anchors.len()
    };
    if recovery_candidates_only {
        anchors.retain(|anchor| anchor.recovery_eligible != Some(false));
        for anchor in &mut anchors {
            if let Some(positions) = anchor
                .recovery_eligible_positions
                .as_ref()
                .filter(|positions| !positions.is_empty())
            {
                anchor.positions = positions.clone();
                if !anchor.positions.contains(&anchor.position_hint) {
                    anchor.position_hint = anchor.positions[0];
                }
                anchor.recovery_eligible_positions = None;
            }
        }
    }
    if density_candidates_only {
        anchors.retain(|anchor| {
            anchor
                .density_eligible_positions
                .as_ref()
                .is_some_and(|positions| !positions.is_empty())
        });
        for anchor in &mut anchors {
            if let Some(positions) = anchor
                .density_eligible_positions
                .as_ref()
                .filter(|positions| !positions.is_empty())
            {
                anchor.positions = positions.clone();
                if !anchor.positions.contains(&anchor.position_hint) {
                    anchor.position_hint = anchor.positions[0];
                }
            }
        }
    }
    if reverse {
        anchors.reverse();
    }
    if let Some(query) = query.as_deref() {
        let needle = query.to_ascii_lowercase();
        anchors.retain(|anchor| context_rewind_anchor_matches_query(anchor, &needle));
    }
    let filtered_total = anchors.len();
    if compact_catalog {
        let page = anchors
            .iter()
            .skip(offset)
            .take(limit)
            .map(|anchor| context_rewind_anchor_compact_entry(anchor, include_pruning_estimates))
            .collect::<Vec<_>>();
        let next_offset = (offset.saturating_add(page.len()) < filtered_total)
            .then_some(offset.saturating_add(page.len()));
        let mut compact = ContextRewindAnchorCompactCatalog {
            catalog_format: "compact_page",
            total,
            filtered_total,
            offset,
            limit,
            next_offset,
            output_cap_bytes: CONTEXT_REWIND_ANCHOR_COMPACT_OUTPUT_MAX_BYTES,
            output_truncated: false,
            query,
            include_management_tools,
            include_non_recovery,
            recovery_candidates_only,
            density_candidates_only,
            pruning_estimates_included: include_pruning_estimates,
            repeat_listing: None,
            no_eligible_anchors: None,
            empty_page_reason: None,
            notice: None,
            anchors: page,
        };
        annotate_compact_catalog_repeats_and_dead_ends(
            &mut compact,
            trailing_listing_calls,
            reverse,
        );
        return serialize_context_rewind_anchor_compact_catalog(compact, filtered_total);
    }
    let mut page = anchors
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    if !include_pruning_estimates {
        for anchor in &mut page {
            anchor.approx_pruned_tokens_before = None;
            anchor.approx_pruned_tokens_after = None;
        }
    }
    let next_offset =
        (offset.saturating_add(limit) < filtered_total).then_some(offset.saturating_add(limit));
    let catalog = ContextRewindAnchorCatalog {
        total,
        filtered_total,
        offset,
        limit,
        next_offset,
        query,
        include_management_tools,
        include_non_recovery,
        recovery_candidates_only,
        density_candidates_only,
        pruning_estimates_included: include_pruning_estimates,
        anchors: page,
    };
    serde_json::to_string(&catalog).map_err(|err| err.to_string())
}

pub(crate) fn context_rewind_anchor_use_compact_catalog(
    params: &serde_json::Value,
    include_non_recovery: bool,
) -> bool {
    if include_non_recovery {
        return false;
    }
    if params
        .get("detail")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    if params
        .get("compact_catalog")
        .or_else(|| params.get("compactCatalog"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    true
}

/// Annotates a compact catalog page with anti-stall guidance, addressing the
/// two protocol dead-ends measured in the 2026-06-12 w40 bench (7 of 20
/// managed misses ended re-listing offset 0 until the recovery step limit;
/// 1 ended on a page with nothing valid to rewind to):
/// - `repeat_listing`: the default page was re-requested inside one
///   management-only stall, so the rows are unchanged and the model should
///   commit to `rewind_context` instead of listing again.
/// - empty pages say *why* they are empty and what to do instead of leaving
///   a bare page that invites another listing: an exhausted eligible catalog
///   fails loudly toward the supervisor's manual recovery path (or, for a
///   density-only listing, says to skip maintenance and continue the task).
pub(crate) fn annotate_compact_catalog_repeats_and_dead_ends(
    compact: &mut ContextRewindAnchorCompactCatalog,
    trailing_listing_calls: usize,
    reverse: bool,
) {
    let default_page_view = compact.offset == 0 && compact.query.is_none() && !reverse;
    // The scan normally already contains the in-flight listing call, so two
    // or more trailing listings mean at least one completed prior listing.
    let prior_listings = trailing_listing_calls.saturating_sub(1);
    if default_page_view && prior_listings >= 1 && !compact.anchors.is_empty() {
        compact.repeat_listing = Some(true);
        compact.notice = Some(format!(
            "repeat listing: this default page was already returned {prior_listings} time(s) in the current managed-context stall and is unchanged (management/status calls are excluded from rows and counts, so re-listing cannot surface new anchors). Do not call list_rewind_anchors again: choose one exact item_id from the rows above and call rewind_context now, or page exactly once with offset=next_offset if every visible row is unusable."
        ));
    }
    if !compact.anchors.is_empty() {
        return;
    }
    if compact.filtered_total == 0 {
        if compact.query.is_some() {
            compact.empty_page_reason = Some("query_unmatched");
            compact.notice = Some(
                "no eligible anchors match this query. Do not repeat the query: re-list once without a query to see the eligible catalog (an empty unqueried page means nothing is left to rewind to)."
                    .to_string(),
            );
        } else {
            compact.no_eligible_anchors = Some(true);
            compact.empty_page_reason = Some("no_eligible_anchors");
            compact.notice = Some(if compact.density_candidates_only {
                "no eligible density anchors remain: every remaining thread item is managed-context management/status activity or has no density-valid position. There is nothing to prune — do not call list_rewind_anchors again and do not end the session over this; skip density maintenance and continue the task normally."
                    .to_string()
            } else {
                "no eligible rewind anchors remain: every remaining thread item is managed-context management/status activity or is known to leave backend pressure at/above the rewind-only limit. Re-listing cannot surface new anchors — do not call list_rewind_anchors again. State plainly that managed-context recovery has no valid anchor and end the turn with a brief status message so the supervisor can take a manual recovery path (rewind_backout, thread restore, or operator intervention). include_non_recovery=true remains available for read-only diagnostics only."
                    .to_string()
            });
        }
    } else {
        compact.empty_page_reason = Some("offset_past_end");
        compact.notice = Some(format!(
            "offset {} is past the end of the eligible catalog ({} anchors). Every eligible row has already been returned. Do not keep paging: choose an item_id from rows already in view and call rewind_context, or re-list from offset 0 only if those rows are no longer visible.",
            compact.offset, compact.filtered_total
        ));
    }
}

pub(crate) fn serialize_context_rewind_anchor_compact_catalog(
    mut compact: ContextRewindAnchorCompactCatalog,
    filtered_total: usize,
) -> Result<String, String> {
    loop {
        let serialized = serde_json::to_string(&compact).map_err(|err| err.to_string())?;
        if serialized.len() <= CONTEXT_REWIND_ANCHOR_COMPACT_OUTPUT_MAX_BYTES
            || compact.anchors.len() <= 1
        {
            return Ok(serialized);
        }
        compact.anchors.pop();
        compact.output_truncated = true;
        compact.next_offset = (compact.offset.saturating_add(compact.anchors.len())
            < filtered_total)
            .then_some(compact.offset.saturating_add(compact.anchors.len()));
    }
}

pub(crate) fn context_rewind_anchor_compact_entry(
    anchor: &ContextRewindAnchorCatalogEntry,
    include_pruning_estimates: bool,
) -> ContextRewindAnchorCompactEntry {
    let item_type = if anchor.first_item_type == anchor.last_item_type {
        anchor.first_item_type.clone()
    } else {
        format!("{}..{}", anchor.first_item_type, anchor.last_item_type)
    };
    let mut summary = anchor.summary.clone();
    truncate_string(&mut summary, CONTEXT_REWIND_ANCHOR_COMPACT_SUMMARY_LIMIT);
    ContextRewindAnchorCompactEntry {
        ordinal: anchor.ordinal,
        item_id: anchor.item_id.clone(),
        item_type,
        positions: anchor.positions.clone(),
        position_hint: anchor.position_hint,
        names: context_rewind_anchor_compact_strings(&anchor.names),
        roles: context_rewind_anchor_compact_strings(&anchor.roles),
        summary,
        density_eligible_positions: anchor.density_eligible_positions.clone(),
        approx_pruned_tokens_before: include_pruning_estimates
            .then_some(anchor.approx_pruned_tokens_before)
            .flatten(),
        approx_pruned_tokens_after: include_pruning_estimates
            .then_some(anchor.approx_pruned_tokens_after)
            .flatten(),
    }
}

pub(crate) fn context_rewind_anchor_compact_strings(values: &[String]) -> Vec<String> {
    values
        .iter()
        .take(CONTEXT_REWIND_ANCHOR_COMPACT_TEXT_LIST_LIMIT)
        .map(|value| truncate_string_copy(value, CONTEXT_REWIND_ANCHOR_COMPACT_TEXT_ITEM_LIMIT))
        .collect()
}

pub(crate) fn inspect_context_rewind_anchor_from_rollout(
    source_rollout_path: &Path,
    params: &serde_json::Value,
) -> Result<String, String> {
    let item_id = context_rewind_anchor_item_id(params)
        .ok_or_else(|| "inspect_rewind_anchor requires anchor.item_id or item_id".to_string())?;
    let radius = params
        .get("radius")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(CONTEXT_REWIND_ANCHOR_INSPECT_DEFAULT_RADIUS)
        .min(CONTEXT_REWIND_ANCHOR_INSPECT_MAX_RADIUS);
    let anchors = scan_context_rewind_anchor_catalog(source_rollout_path).map_err(|err| {
        format!(
            "failed to inspect rewind anchors in {}: {err}",
            source_rollout_path.display()
        )
    })?;
    let anchor = anchors
        .into_iter()
        .find(|anchor| anchor.item_id == item_id)
        .ok_or_else(|| {
            format!(
                "rollback anchor item_id `{item_id}` was not found in {}; call list_rewind_anchors to inspect valid exact anchors before retrying",
                source_rollout_path.display()
            )
        })?;
    let first_line = anchor.first_line.saturating_sub(radius).max(1);
    let last_line = anchor.last_line.saturating_add(radius);
    let file = std::fs::File::open(source_rollout_path).map_err(|err| {
        format!(
            "failed to inspect rewind anchor context in {}: {err}",
            source_rollout_path.display()
        )
    })?;
    let reader = io::BufReader::new(file);
    let mut context = Vec::new();

    for (line_index, line) in reader.lines().enumerate() {
        let line_number = line_index.saturating_add(1);
        if line_number < first_line || line_number > last_line {
            continue;
        }
        let line = line.map_err(|err| {
            format!(
                "failed to read rewind anchor context in {}: {err}",
                source_rollout_path.display()
            )
        })?;
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if entry.get("type").and_then(|value| value.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        let item_type = payload
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let item_ids = response_item_anchor_ids(payload);
        let anchor_span = (line_number >= anchor.first_line && line_number <= anchor.last_line)
            || item_ids
                .iter()
                .any(|candidate| candidate == &anchor.item_id);
        context.push(ContextRewindAnchorInspectItem {
            line: line_number,
            item_type,
            item_ids,
            names: context_rewind_anchor_names(payload),
            roles: context_rewind_anchor_roles(payload),
            summary: context_rewind_anchor_summary(payload),
            anchor_span,
        });
    }

    let inspection = ContextRewindAnchorInspection {
        rollout_path: source_rollout_path.display().to_string(),
        anchor,
        radius,
        context,
        usage: "Use this context to decide whether the selected item_id is the intended rewind point. If so, pass only the exact item_id and position to rewind_context; otherwise inspect another anchor.",
    };
    serde_json::to_string(&inspection).map_err(|err| err.to_string())
}

pub(crate) fn scan_context_rewind_anchor_catalog(
    source_rollout_path: &Path,
) -> io::Result<Vec<ContextRewindAnchorCatalogEntry>> {
    let file = std::fs::File::open(source_rollout_path)?;
    let reader = io::BufReader::new(file);
    let mut anchors = Vec::<ContextRewindAnchorCatalogEntry>::new();
    let mut by_item_id = HashMap::<String, usize>::new();
    let mut backend_usage = Vec::<ContextRewindBackendUsageAtLine>::new();
    let mut pending_rewind_outcome: Option<PendingContextRewindOutcome> = None;
    let mut latest_rewind_outcomes = HashMap::<String, ContextRewindBackendUsageAtLine>::new();
    let mut latest_backend_usage = None::<ContextRewindBackendUsageAtLine>;
    let mut prefix_estimated_tokens = 0_u64;
    // Where the latest model response began (first model-emitted item of the
    // current model-item run). Token reports cover input strictly before this
    // line; see `context_rewind_usage_covers_anchor`.
    let mut current_model_response_start = None::<usize>;
    let mut previous_response_item_was_model = false;
    let mut reports_in_current_run = 0usize;
    // Effective-history replay of `thread_rolled_back` markers. Rollouts are
    // append-only, so items dropped by a prior anchored rollback remain in
    // the file; offering them as anchors would make the fork's rollback
    // (which resolves against *effective* history) fail with "anchor not
    // found in thread history". Track every response_item line, per-id
    // occurrence lines, and per-line token estimates; replay each rollback
    // marker into a dead line span; post-filter the catalog to live lines.
    // Anchor-less markers (plain N-turn `/undo` rollbacks) and
    // `thread/restore` checkpoints are not replayed — both are conservative
    // (the catalog stays as permissive as before for those paths).
    let mut dead_line_spans: Vec<(usize, usize)> = Vec::new();
    let mut id_occurrence_lines = HashMap::<String, Vec<usize>>::new();
    let mut item_tokens_by_line: Vec<(usize, u64)> = Vec::new();
    let mut managed_context_recovery_kickstart_lines: Vec<usize> = Vec::new();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let line_number = line_index.saturating_add(1);
        if let Some(mut usage) =
            context_rewind_backend_usage_from_rollout_entry(line_number, &entry)
        {
            usage.response_start_line = current_model_response_start.unwrap_or(line_number);
            usage.measures_prefix_exactly =
                current_model_response_start.is_some() && reports_in_current_run == 0;
            reports_in_current_run = reports_in_current_run.saturating_add(1);
            if let Some(pending) = pending_rewind_outcome.take() {
                latest_rewind_outcomes.insert(
                    pending.key,
                    context_rewind_worse_usage(pending.prior_usage, usage),
                );
            }
            latest_backend_usage = Some(usage);
            backend_usage.push(usage);
        }
        if let Some(key) = context_rewind_rollback_anchor_outcome_key_from_rollout_entry(&entry) {
            if let Some(pending) = pending_rewind_outcome.take() {
                if let Some(prior_usage) = pending.prior_usage {
                    latest_rewind_outcomes.insert(pending.key, prior_usage);
                }
            }
            pending_rewind_outcome = Some(PendingContextRewindOutcome {
                key,
                prior_usage: latest_backend_usage
                    .filter(|usage| line_number.saturating_sub(usage.line) <= 3),
            });
        }
        if let Some((anchor_id, position)) = context_rewind_rollback_cut_from_rollout_entry(&entry)
        {
            // Resolve the cut against the occurrences that are still live at
            // this marker (chained rollbacks accumulate spans in order).
            let live_lines: Vec<usize> = id_occurrence_lines
                .get(&anchor_id)
                .map(|lines| {
                    lines
                        .iter()
                        .copied()
                        .filter(|line| !context_rewind_line_is_dead(*line, &dead_line_spans))
                        .collect()
                })
                .unwrap_or_default();
            if let (Some(&group_first), Some(&group_last)) = (live_lines.first(), live_lines.last())
            {
                let cut_start = match position {
                    external_agent::RollbackAnchorPosition::Before => group_first,
                    external_agent::RollbackAnchorPosition::After => group_last.saturating_add(1),
                };
                if cut_start <= line_number {
                    dead_line_spans.push((cut_start, line_number));
                }
            }
        }
        if entry.get("type").and_then(|value| value.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        if response_item_is_managed_context_recovery_kickstart(payload) {
            managed_context_recovery_kickstart_lines.push(line_number);
        }
        let item_is_model_emitted = response_item_is_model_emitted(payload);
        if item_is_model_emitted && !previous_response_item_was_model {
            current_model_response_start = Some(line_number);
            reports_in_current_run = 0;
        }
        previous_response_item_was_model = item_is_model_emitted;
        let item_estimated_tokens = context_rewind_estimated_tokens(payload);
        let prefix_before_item = prefix_estimated_tokens;
        prefix_estimated_tokens = prefix_estimated_tokens.saturating_add(item_estimated_tokens);
        let prefix_after_item = prefix_estimated_tokens;
        item_tokens_by_line.push((line_number, item_estimated_tokens));
        let item_type = payload
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let names = context_rewind_anchor_names(payload);
        let roles = context_rewind_anchor_roles(payload);
        let summary = context_rewind_anchor_summary(payload);
        for item_id in response_item_anchor_ids(payload) {
            id_occurrence_lines
                .entry(item_id.clone())
                .or_default()
                .push(line_number);
            if let Some(index) = by_item_id.get(&item_id).copied() {
                let anchor = &mut anchors[index];
                anchor.last_line = line_number;
                anchor.last_item_type = item_type.clone();
                anchor.last_item_is_model = item_is_model_emitted;
                anchor.prefix_estimated_tokens_after_anchor = Some(prefix_after_item);
                merge_string_set(&mut anchor.names, names.iter().cloned());
                merge_string_set(&mut anchor.roles, roles.iter().cloned());
                if !summary.is_empty() && !anchor.summary.contains(&summary) {
                    if !anchor.summary.is_empty() {
                        anchor.summary.push_str(" | ");
                    }
                    anchor.summary.push_str(&summary);
                    truncate_string(
                        &mut anchor.summary,
                        CONTEXT_REWIND_ANCHOR_MERGED_SUMMARY_LIMIT,
                    );
                }
                continue;
            }
            let index = anchors.len();
            by_item_id.insert(item_id.clone(), index);
            anchors.push(ContextRewindAnchorCatalogEntry {
                ordinal: index,
                item_id,
                first_line: line_number,
                last_line: line_number,
                first_item_type: item_type.clone(),
                last_item_type: item_type.clone(),
                last_item_is_model: item_is_model_emitted,
                positions: vec!["before", "after"],
                position_hint: "after",
                names: names.clone(),
                roles: roles.clone(),
                summary: summary.clone(),
                backend_usage_at_or_after_anchor: None,
                backend_usage_before_anchor: None,
                rewind_only_limit_at_or_after_anchor: None,
                recommended_rewind_limit_at_or_after_anchor: None,
                prefix_estimated_tokens_before_anchor: Some(prefix_before_item),
                prefix_estimated_tokens_after_anchor: Some(prefix_after_item),
                approx_pruned_tokens_before: None,
                approx_pruned_tokens_after: None,
                prefix_tokens_after: None,
                latest_rewind_usage_after_anchor: None,
                latest_rewind_limit_after_anchor: None,
                recovery_eligible: None,
                recovery_eligible_positions: None,
                density_eligible: None,
                density_eligible_positions: None,
                managed_context_recovery_start_line: None,
            });
        }
    }
    if let Some(pending) = pending_rewind_outcome {
        if let Some(prior_usage) = pending.prior_usage {
            latest_rewind_outcomes.insert(pending.key, prior_usage);
        }
    }

    if !dead_line_spans.is_empty() {
        // Drop anchors whose every occurrence was cut by a prior rollback —
        // they no longer exist in effective history and the fork would
        // reject them. Anchors that re-appear after the cut stay, rebased to
        // their live occurrence lines.
        anchors.retain(|anchor| {
            id_occurrence_lines
                .get(&anchor.item_id)
                .is_some_and(|lines| {
                    lines
                        .iter()
                        .any(|line| !context_rewind_line_is_dead(*line, &dead_line_spans))
                })
        });
        for (ordinal, anchor) in anchors.iter_mut().enumerate() {
            anchor.ordinal = ordinal;
            if let Some(lines) = id_occurrence_lines.get(&anchor.item_id) {
                let mut live = lines
                    .iter()
                    .copied()
                    .filter(|line| !context_rewind_line_is_dead(*line, &dead_line_spans));
                if let Some(first) = live.next() {
                    anchor.first_line = first;
                    anchor.last_line = live.next_back().unwrap_or(first);
                }
            }
        }
        // A usage report is poisoned only if its measured prefix contained
        // an item that a *later* rollback dropped: some dead item line sits
        // before the report while the marker that killed it sits at/after
        // the report. Reports measuring a purely-live prefix (e.g. taken
        // between the cut point and the marker with no dead items in
        // between, or after the rollback applied) stay valid.
        let mut dead_item_lines: Vec<(usize, usize)> = Vec::new(); // (item_line, marker_line)
        for (line, _tokens) in &item_tokens_by_line {
            if let Some((_, marker_line)) = dead_line_spans
                .iter()
                .find(|(start, end)| line >= start && line <= end)
            {
                dead_item_lines.push((*line, *marker_line));
            }
        }
        backend_usage.retain(|usage| {
            !dead_item_lines.iter().any(|(item_line, marker_line)| {
                *item_line < usage.line && *marker_line >= usage.line
            })
        });
    }
    // Recompute prefix estimates over the live item sequence so pruning
    // estimates do not count items a prior rollback already dropped.
    let mut live_running_total = 0_u64;
    let mut live_prefix_before_by_line = HashMap::<usize, u64>::new();
    let mut live_prefix_after_by_line = HashMap::<usize, u64>::new();
    for (line, tokens) in &item_tokens_by_line {
        if context_rewind_line_is_dead(*line, &dead_line_spans) {
            continue;
        }
        live_prefix_before_by_line.insert(*line, live_running_total);
        live_running_total = live_running_total.saturating_add(*tokens);
        live_prefix_after_by_line.insert(*line, live_running_total);
    }
    for anchor in &mut anchors {
        if let Some(prefix) = live_prefix_before_by_line.get(&anchor.first_line) {
            anchor.prefix_estimated_tokens_before_anchor = Some(*prefix);
        }
        if let Some(prefix) = live_prefix_after_by_line.get(&anchor.last_line) {
            anchor.prefix_estimated_tokens_after_anchor = Some(*prefix);
        }
    }
    let managed_context_recovery_start_line = managed_context_recovery_kickstart_lines
        .iter()
        .copied()
        .find(|line| !context_rewind_line_is_dead(*line, &dead_line_spans));

    let total_prefix_estimated_tokens = live_running_total;
    for anchor in &mut anchors {
        anchor.approx_pruned_tokens_before = anchor
            .prefix_estimated_tokens_before_anchor
            .map(|prefix_tokens| total_prefix_estimated_tokens.saturating_sub(prefix_tokens));
        anchor.approx_pruned_tokens_after = anchor
            .prefix_estimated_tokens_after_anchor
            .map(|prefix_tokens| total_prefix_estimated_tokens.saturating_sub(prefix_tokens));
        let in_managed_recovery_span = managed_context_recovery_start_line
            .filter(|start_line| anchor.last_line >= *start_line);
        if let Some(start_line) = in_managed_recovery_span {
            anchor.managed_context_recovery_start_line = Some(start_line);
            anchor.recovery_eligible = Some(false);
            anchor.recovery_eligible_positions = None;
        }
        // Latest report that measured a strict prefix of this anchor: either
        // it physically precedes the anchor item, or it is the exact first
        // report of a model run that began at/before the anchor. Reports
        // deeper in a merged run measured more than the run-start prefix and
        // must not be used as a prefix floor.
        anchor.backend_usage_before_anchor = backend_usage
            .iter()
            .rev()
            .find(|usage| {
                usage.line <= anchor.first_line
                    || (usage.measures_prefix_exactly
                        && usage.response_start_line <= anchor.first_line)
            })
            .map(|usage| usage.used_tokens);
        let Some(usage) = backend_usage
            .iter()
            .find(|usage| context_rewind_usage_covers_anchor(usage, anchor))
            .copied()
        else {
            continue;
        };
        anchor.backend_usage_at_or_after_anchor = Some(usage.used_tokens);
        anchor.rewind_only_limit_at_or_after_anchor = Some(usage.rewind_only_limit);
        anchor.recommended_rewind_limit_at_or_after_anchor = Some(
            managed_context_density_recommended_limit(usage.rewind_only_limit),
        );
        if in_managed_recovery_span.is_some() {
            anchor.density_eligible = Some(false);
            anchor.density_eligible_positions = None;
            continue;
        }
        let backend_has_headroom =
            context_rewind_anchor_has_recovery_headroom(usage.used_tokens, usage.rewind_only_limit);
        let restore_prefix_after_has_headroom = anchor
            .prefix_estimated_tokens_after_anchor
            .is_some_and(|prefix_tokens| {
                context_rewind_anchor_has_recovery_headroom(prefix_tokens, usage.rewind_only_limit)
            });
        if !backend_has_headroom && !restore_prefix_after_has_headroom {
            anchor.prefix_tokens_after = anchor.prefix_estimated_tokens_after_anchor;
        }
        let latest_rewind_after_outcome = latest_rewind_outcomes
            .get(&context_rewind_anchor_outcome_key(
                &anchor.item_id,
                external_agent::RollbackAnchorPosition::After,
            ))
            .copied();
        let latest_rewind_after_has_headroom = latest_rewind_after_outcome.is_none_or(|outcome| {
            context_rewind_anchor_has_recovery_headroom(
                outcome.used_tokens,
                outcome.rewind_only_limit,
            )
        });
        if let Some(outcome) =
            latest_rewind_after_outcome.filter(|_| !latest_rewind_after_has_headroom)
        {
            anchor.latest_rewind_usage_after_anchor = Some(outcome.used_tokens);
            anchor.latest_rewind_limit_after_anchor = Some(outcome.rewind_only_limit);
        }
        let latest_rewind_before_outcome = latest_rewind_outcomes
            .get(&context_rewind_anchor_outcome_key(
                &anchor.item_id,
                external_agent::RollbackAnchorPosition::Before,
            ))
            .copied();
        // An anchor that already proved insufficient is not re-offered for
        // recovery — the veto is anchor-level (managed.md), not per-position:
        // recovery should move to a different anchor instead of retrying the
        // same cut point one notch harder.
        let anchor_rewind_outcome =
            match (latest_rewind_before_outcome, latest_rewind_after_outcome) {
                (Some(before), Some(after)) => {
                    Some(context_rewind_worse_usage(Some(before), after))
                }
                (before, after) => before.or(after),
            };
        let after_density_eligible = context_rewind_anchor_position_density_eligible(
            anchor,
            external_agent::RollbackAnchorPosition::After,
            latest_rewind_after_outcome,
        )
        .unwrap_or(false);
        let before_density_eligible = context_rewind_anchor_position_density_eligible(
            anchor,
            external_agent::RollbackAnchorPosition::Before,
            latest_rewind_before_outcome,
        )
        .unwrap_or(false);
        anchor.density_eligible = Some(after_density_eligible || before_density_eligible);
        let mut density_eligible_positions = Vec::new();
        if before_density_eligible {
            density_eligible_positions.push("before");
        }
        if after_density_eligible {
            density_eligible_positions.push("after");
        }
        anchor.density_eligible_positions =
            (!density_eligible_positions.is_empty()).then_some(density_eligible_positions);
        let after_recovery_eligible = context_rewind_anchor_position_recovery_eligible(
            anchor,
            external_agent::RollbackAnchorPosition::After,
            anchor_rewind_outcome,
        )
        .unwrap_or(false);
        // Offer `before` whenever `after` is not eligible by the best
        // available source. `context_rewind_anchor_position_recovery_eligible`
        // already prefers the backend-reported usage and falls back to the
        // prefix estimate, so re-checking the raw `after` estimate here would
        // let an optimistic estimate (estimates can't see instructions/tool
        // specs) overrule a backend report that says the `after` cut keeps
        // too much — suppressing the only position that can actually recover.
        let before_recovery_eligible = context_rewind_anchor_position_recovery_eligible(
            anchor,
            external_agent::RollbackAnchorPosition::Before,
            anchor_rewind_outcome,
        )
        .unwrap_or(false)
            && !after_recovery_eligible;
        anchor.position_hint = if after_recovery_eligible {
            "after"
        } else if before_recovery_eligible {
            "before"
        } else {
            "after"
        };
        anchor.recovery_eligible = Some(after_recovery_eligible || before_recovery_eligible);
        let mut recovery_eligible_positions = Vec::new();
        if before_recovery_eligible {
            recovery_eligible_positions.push("before");
        }
        if after_recovery_eligible {
            recovery_eligible_positions.push("after");
        }
        anchor.recovery_eligible_positions =
            (!recovery_eligible_positions.is_empty()).then_some(recovery_eligible_positions);
    }

    Ok(anchors)
}

pub(crate) fn latest_context_rewind_outcome_for_anchor(
    source_rollout_path: &Path,
    requested_item_id: &str,
    position: external_agent::RollbackAnchorPosition,
) -> io::Result<Option<ContextRewindBackendUsageAtLine>> {
    let key = context_rewind_anchor_outcome_key(requested_item_id, position);
    let file = std::fs::File::open(source_rollout_path)?;
    let reader = io::BufReader::new(file);
    let mut pending = None::<PendingContextRewindOutcome>;
    let mut latest_backend_usage = None::<ContextRewindBackendUsageAtLine>;
    let mut latest = None;
    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let line_number = line_index.saturating_add(1);
        if let Some(usage) = context_rewind_backend_usage_from_rollout_entry(line_number, &entry) {
            if let Some(pending_key) = pending.take() {
                if pending_key.key == key {
                    latest = Some(context_rewind_worse_usage(pending_key.prior_usage, usage));
                }
            }
            latest_backend_usage = Some(usage);
        }
        if let Some(outcome_key) =
            context_rewind_rollback_anchor_outcome_key_from_rollout_entry(&entry)
        {
            if let Some(pending_key) = pending.take() {
                if pending_key.key == key {
                    if let Some(prior_usage) = pending_key.prior_usage {
                        latest = Some(prior_usage);
                    }
                }
            }
            pending = Some(PendingContextRewindOutcome {
                key: outcome_key,
                prior_usage: latest_backend_usage
                    .filter(|usage| line_number.saturating_sub(usage.line) <= 3),
            });
        }
    }
    if let Some(pending_key) = pending {
        if pending_key.key == key {
            if let Some(prior_usage) = pending_key.prior_usage {
                latest = Some(prior_usage);
            }
        }
    }
    Ok(latest)
}

pub(crate) fn context_rewind_estimated_tokens(value: &serde_json::Value) -> u64 {
    let chars = serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or_else(|_| value.to_string().len());
    std::cmp::max(1, chars.div_ceil(4) as u64)
}

pub(crate) fn context_rewind_anchor_has_recovery_headroom(used_tokens: u64, rewind_only_limit: u64) -> bool {
    used_tokens < rewind_only_limit
        && rewind_only_limit.saturating_sub(used_tokens)
            >= CONTEXT_REWIND_RECOVERY_MIN_RESUME_HEADROOM_TOKENS
}

pub(crate) fn context_rewind_worse_usage(
    prior_usage: Option<ContextRewindBackendUsageAtLine>,
    next_usage: ContextRewindBackendUsageAtLine,
) -> ContextRewindBackendUsageAtLine {
    let Some(prior_usage) = prior_usage else {
        return next_usage;
    };
    let prior_has_headroom = context_rewind_anchor_has_recovery_headroom(
        prior_usage.used_tokens,
        prior_usage.rewind_only_limit,
    );
    let next_has_headroom = context_rewind_anchor_has_recovery_headroom(
        next_usage.used_tokens,
        next_usage.rewind_only_limit,
    );
    match (prior_has_headroom, next_has_headroom) {
        (false, true) => prior_usage,
        (true, false) => next_usage,
        _ => {
            let prior_remaining = prior_usage
                .rewind_only_limit
                .saturating_sub(prior_usage.used_tokens);
            let next_remaining = next_usage
                .rewind_only_limit
                .saturating_sub(next_usage.used_tokens);
            if prior_remaining <= next_remaining {
                prior_usage
            } else {
                next_usage
            }
        }
    }
}

pub(crate) fn context_rewind_backend_usage_from_rollout_entry(
    line: usize,
    entry: &serde_json::Value,
) -> Option<ContextRewindBackendUsageAtLine> {
    if entry.get("type").and_then(|value| value.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = entry.get("payload")?;
    if payload.get("type").and_then(|value| value.as_str()) != Some("token_count") {
        return None;
    }
    let info = payload.get("info")?;
    let rewind_only_limit = info
        .get("model_context_window")
        .or_else(|| info.get("modelContextWindow"))?
        .as_u64()?;
    if rewind_only_limit == 0 {
        return None;
    }
    let last = info
        .get("last_token_usage")
        .or_else(|| info.get("lastTokenUsage"))?;
    let used_tokens = last
        .get("total_tokens")
        .or_else(|| last.get("totalTokens"))?
        .as_u64()?;
    Some(ContextRewindBackendUsageAtLine {
        line,
        used_tokens,
        rewind_only_limit,
        response_start_line: line,
        measures_prefix_exactly: false,
    })
}

/// Whether a rollout line sits inside a span that a prior `thread_rolled_back`
/// marker removed from effective history.
pub(crate) fn context_rewind_line_is_dead(line: usize, dead_line_spans: &[(usize, usize)]) -> bool {
    dead_line_spans
        .iter()
        .any(|(start, end)| line >= *start && line <= *end)
}

/// Parse a `thread_rolled_back` marker into the anchored cut it applied:
/// the anchor item id plus the cut position. Markers without an anchor
/// (plain N-turn rollbacks) return `None` — the catalog replay treats those
/// conservatively (no dead span).
pub(crate) fn context_rewind_rollback_cut_from_rollout_entry(
    entry: &serde_json::Value,
) -> Option<(String, external_agent::RollbackAnchorPosition)> {
    if entry.get("type").and_then(|value| value.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = entry.get("payload")?;
    if payload.get("type").and_then(|value| value.as_str()) != Some("thread_rolled_back") {
        return None;
    }
    let anchor = payload.get("anchor")?;
    let item_id = anchor
        .get("itemId")
        .or_else(|| anchor.get("item_id"))?
        .as_str()?
        .trim();
    if item_id.is_empty() {
        return None;
    }
    let position = anchor
        .get("position")
        .and_then(|value| value.as_str())
        .and_then(external_agent::RollbackAnchorPosition::from_str)
        .unwrap_or(external_agent::RollbackAnchorPosition::After);
    Some((item_id.to_string(), position))
}

pub(crate) fn context_rewind_rollback_anchor_outcome_key_from_rollout_entry(
    entry: &serde_json::Value,
) -> Option<String> {
    if entry.get("type").and_then(|value| value.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = entry.get("payload")?;
    if payload.get("type").and_then(|value| value.as_str()) != Some("thread_rolled_back") {
        return None;
    }
    let anchor = payload.get("anchor")?;
    let item_id = anchor
        .get("itemId")
        .or_else(|| anchor.get("item_id"))?
        .as_str()?
        .trim();
    if item_id.is_empty() {
        return None;
    }
    let position = anchor
        .get("position")
        .and_then(|value| value.as_str())
        .and_then(external_agent::RollbackAnchorPosition::from_str)
        .unwrap_or(external_agent::RollbackAnchorPosition::After);
    Some(context_rewind_anchor_outcome_key(item_id, position))
}

pub(crate) fn context_rewind_anchor_matches_query(
    anchor: &ContextRewindAnchorCatalogEntry,
    needle: &str,
) -> bool {
    anchor.item_id.to_ascii_lowercase().contains(needle)
        || anchor.first_item_type.to_ascii_lowercase().contains(needle)
        || anchor.last_item_type.to_ascii_lowercase().contains(needle)
        || anchor
            .names
            .iter()
            .any(|name| name.to_ascii_lowercase().contains(needle))
        || anchor
            .roles
            .iter()
            .any(|role| role.to_ascii_lowercase().contains(needle))
        || anchor.summary.to_ascii_lowercase().contains(needle)
        || anchor.first_line.to_string() == needle
        || anchor.last_line.to_string() == needle
}

/// Managed-context machinery and supervisor status/observability calls.
/// These thread items are protocol churn, not substantive task milestones:
/// they are hidden from the default anchor catalog and excluded from its
/// accounting so a recovery stall (whose only thread growth is these calls)
/// re-lists byte-identically instead of presenting a "growing" catalog, and
/// so a status poll can never be the last "eligible" rewind candidate (the
/// 2026-06-12 bench dead-end: a density handoff whose only returned row was a
/// `get_status` anchor the handoff itself disallowed).
pub(crate) fn context_rewind_anchor_is_management_tool(anchor: &ContextRewindAnchorCatalogEntry) -> bool {
    anchor.names.iter().any(|name| {
        matches!(
            name.as_str(),
            "list_rewind_anchors"
                | "inspect_rewind_anchor"
                | "rewind_context"
                | "rewind_backout"
                | "context_rewind_anchors"
                | "context_rewind_anchor_inspect"
                | "context_rewind"
                | "context_rewind_backout"
                | "get_status"
                | "get_logs"
                | "get_pending_approval"
                | "get_pending_input"
                | "get_restart_status"
        )
    })
}

/// True when `anchor` is a `list_rewind_anchors` (or legacy alias) call.
pub(crate) fn context_rewind_anchor_is_listing_call(anchor: &ContextRewindAnchorCatalogEntry) -> bool {
    anchor.names.iter().any(|name| {
        matches!(
            name.as_str(),
            "list_rewind_anchors" | "context_rewind_anchors"
        )
    })
}

/// Number of `list_rewind_anchors` calls in the trailing management-only run
/// of the catalog scan — i.e. in the current managed-context stall, since
/// under rewind-only/density gating management tools are the only items the
/// model can append. Includes the in-flight listing call when the backend has
/// already persisted it.
pub(crate) fn context_rewind_trailing_listing_calls(anchors: &[ContextRewindAnchorCatalogEntry]) -> usize {
    anchors
        .iter()
        .rev()
        .take_while(|anchor| context_rewind_anchor_is_management_tool(anchor))
        .filter(|anchor| context_rewind_anchor_is_listing_call(anchor))
        .count()
}

pub(crate) fn response_item_is_managed_context_recovery_kickstart(item: &serde_json::Value) -> bool {
    if item.get("type").and_then(|value| value.as_str()) != Some("message") {
        return false;
    }
    if item.get("role").and_then(|value| value.as_str()) != Some("user") {
        return false;
    }
    response_item_content_text(item).any(|text| text.contains("<managed_context_recovery>"))
}

/// Whether a rollout `response_item` was emitted by the model (assistant
/// message, reasoning, tool *calls*) as opposed to appended by the runtime
/// afterwards (tool *outputs*, user/developer messages). Used to track where
/// each model response begins inside the rollout line stream.
pub(crate) fn response_item_is_model_emitted(item: &serde_json::Value) -> bool {
    match item.get("type").and_then(|value| value.as_str()) {
        Some("message") => item
            .get("role")
            .and_then(|value| value.as_str())
            .is_some_and(|role| role.eq_ignore_ascii_case("assistant")),
        Some("reasoning")
        | Some("function_call")
        | Some("local_shell_call")
        | Some("custom_tool_call")
        | Some("tool_search_call")
        | Some("web_search_call")
        | Some("image_generation_call") => true,
        _ => false,
    }
}

/// Whether a backend token report actually measured the anchor's content.
///
/// A `token_count` event reports the request that produced the latest model
/// response: its input covered items strictly before that response's first
/// item, plus the response's own output (the model-emitted items). Codex
/// persists a tool's `function_call_output` *before* the corresponding
/// `token_count` line, so the report that lands right after an output never
/// measured it. Attributing such a report to the call/output group made
/// `position="after"` rewinds look like they pruned the very output they
/// keep, and suppressed the `before` position that would actually recover
/// (observed live in the context-stress harness, 2026-06-11).
pub(crate) fn context_rewind_usage_covers_anchor(
    usage: &ContextRewindBackendUsageAtLine,
    anchor: &ContextRewindAnchorCatalogEntry,
) -> bool {
    if anchor.last_item_is_model {
        // The item was part of a model response; the report for that very
        // response (the first one at/after the item) includes it.
        usage.line >= anchor.last_line
    } else {
        // Runtime-appended item (tool output, user message): only a report
        // for a *later* model response consumed it as input.
        usage.response_start_line > anchor.last_line
    }
}

pub(crate) fn context_rewind_anchor_names(item: &serde_json::Value) -> Vec<String> {
    item.get("name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| vec![value.to_string()])
        .unwrap_or_default()
}

pub(crate) fn context_rewind_anchor_roles(item: &serde_json::Value) -> Vec<String> {
    item.get("role")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| vec![value.to_string()])
        .unwrap_or_default()
}

pub(crate) fn context_rewind_anchor_summary(item: &serde_json::Value) -> String {
    let item_type = item
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let mut summary = match item_type {
        "message" => response_item_content_text(item)
            .collect::<Vec<_>>()
            .join(" "),
        "function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call" => {
            let name = item
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("tool");
            let arguments = item
                .get("arguments")
                .or_else(|| item.get("input"))
                .and_then(|value| value.as_str())
                .unwrap_or("");
            format!("{name} {arguments}")
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => item
            .get("output")
            .or_else(|| item.get("result"))
            .and_then(|value| value.as_str())
            .unwrap_or("tool output")
            .to_string(),
        _ => item_type.to_string(),
    };
    summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_string(&mut summary, CONTEXT_REWIND_ANCHOR_SUMMARY_LIMIT);
    summary
}

pub(crate) fn merge_string_set(target: &mut Vec<String>, values: impl Iterator<Item = String>) {
    for value in values {
        if !target.iter().any(|existing| existing == &value) {
            target.push(value);
        }
    }
}

pub(crate) fn truncate_string(value: &mut String, max_chars: usize) {
    if value.chars().count() <= max_chars {
        return;
    }
    *value = value.chars().take(max_chars).collect::<String>();
    value.push_str("...");
}

pub(crate) fn truncate_string_copy(value: &str, max_chars: usize) -> String {
    let mut value = value.to_string();
    truncate_string(&mut value, max_chars);
    value
}

pub(crate) fn response_item_content_text(item: &serde_json::Value) -> impl Iterator<Item = &str> {
    item.get("content")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|content| {
            content
                .get("text")
                .or_else(|| content.get("input_text"))
                .or_else(|| content.get("output_text"))
                .and_then(|value| value.as_str())
        })
}

pub(crate) fn response_item_managed_context_rewind_primer_text(item: &serde_json::Value) -> Option<String> {
    if item.get("type").and_then(|value| value.as_str()) != Some("message") {
        return None;
    }
    let text = response_item_content_text(item)
        .collect::<Vec<_>>()
        .join("\n");
    text.contains("<model_context_rewind_primer>")
        .then_some(text)
}

pub(crate) fn context_rewind_pruned_prior_primer_facts(
    source_rollout_path: &Path,
    anchor_item_id: &str,
    position: external_agent::RollbackAnchorPosition,
    request: &ExternalContextRewindRequest,
) -> io::Result<Option<String>> {
    let Some(current_primer) = request.rendered_primer(None, None) else {
        return Ok(None);
    };
    let Some(anchor) = find_context_rewind_anchor_entry(source_rollout_path, anchor_item_id)?
    else {
        return Ok(None);
    };

    let file = std::fs::File::open(source_rollout_path)?;
    let reader = io::BufReader::new(file);
    let mut prior_primers = Vec::new();
    for (line_index, line) in reader.lines().enumerate() {
        let line_number = line_index.saturating_add(1);
        let pruned_by_rewind = match position {
            external_agent::RollbackAnchorPosition::After => line_number > anchor.last_line,
            external_agent::RollbackAnchorPosition::Before => line_number >= anchor.first_line,
        };
        if !pruned_by_rewind {
            continue;
        }
        let line = line?;
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if entry.get("type").and_then(|value| value.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = entry.get("payload") else {
            continue;
        };
        if let Some(primer) = response_item_managed_context_rewind_primer_text(payload) {
            prior_primers.push(primer);
        }
    }

    Ok(context_rewind_unrepeated_prior_primer_facts(
        &prior_primers,
        &current_primer,
    ))
}

pub(crate) fn context_rewind_unrepeated_prior_primer_facts(
    prior_primers: &[String],
    current_primer: &str,
) -> Option<String> {
    if prior_primers.is_empty() {
        return None;
    }
    let current_normalized =
        normalize_context_rewind_fact_for_compare(current_primer).to_ascii_lowercase();
    let mut seen = HashSet::new();
    let mut out = String::new();

    for primer in prior_primers.iter().rev() {
        for line in context_rewind_prior_primer_fact_lines(primer) {
            let normalized = normalize_context_rewind_fact_for_compare(&line);
            if normalized.is_empty() {
                continue;
            }
            let normalized_lower = normalized.to_ascii_lowercase();
            if current_normalized.contains(&normalized_lower) || !seen.insert(normalized_lower) {
                continue;
            }
            let additional = line.len().saturating_add(1);
            if out.len().saturating_add(additional) > CONTEXT_REWIND_PRIOR_PRIMER_FACT_LIMIT {
                if !out.is_empty() {
                    out.push_str("...\n");
                }
                return (!out.trim().is_empty()).then(|| out.trim().to_string());
            }
            out.push_str(&line);
            out.push('\n');
        }
    }

    (!out.trim().is_empty()).then(|| out.trim().to_string())
}

pub(crate) fn context_rewind_prior_primer_fact_lines(primer: &str) -> Vec<String> {
    primer
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !context_rewind_prior_primer_admin_line(line))
        .map(str::to_string)
        .collect()
}

pub(crate) fn context_rewind_prior_primer_admin_line(line: &str) -> bool {
    matches!(
        line,
        "<model_context_rewind_primer>"
            | "</model_context_rewind_primer>"
            | "Reason:"
            | "Record id:"
            | "Primer:"
            | "Preserve:"
            | "Discard:"
            | "Artifacts:"
            | "Next steps:"
            | "Previous managed-context primer facts not repeated above:"
    ) || line.starts_with("History after the rewind target was pruned")
        || (line.starts_with("rewind-") && line.split_whitespace().count() == 1)
}

pub(crate) fn normalize_context_rewind_fact_for_compare(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn response_item_anchor_ids(item: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();
    match item.get("type").and_then(|value| value.as_str()) {
        Some("message")
        | Some("reasoning")
        | Some("web_search_call")
        | Some("image_generation_call") => {
            push_json_string_id(item, &mut ids, "id");
        }
        Some("local_shell_call")
        | Some("function_call")
        | Some("tool_search_call")
        | Some("custom_tool_call") => {
            push_json_string_id(item, &mut ids, "id");
            push_json_string_id(item, &mut ids, "call_id");
            push_json_string_id(item, &mut ids, "callId");
        }
        Some("function_call_output")
        | Some("tool_search_output")
        | Some("custom_tool_call_output") => {
            push_json_string_id(item, &mut ids, "call_id");
            push_json_string_id(item, &mut ids, "callId");
        }
        _ => {}
    }
    ids
}

pub(crate) fn push_json_string_id(item: &serde_json::Value, ids: &mut Vec<String>, key: &str) {
    if let Some(id) = item
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        ids.push(id.to_string());
    }
}

/// Latest backend `token_count` report in a rollout, in file order. Rollouts
/// are append-only, so the chronologically last report is the backend's
/// freshest usage measurement at read time. It can still be stale: when no
/// model turn ran since an immediately preceding rollback it measured the
/// pre-rollback context — recorded as-is, because nothing fresher exists
/// locally and querying the backend would add an RPC to the rewind path.
pub(crate) fn latest_context_rewind_backend_usage_in_rollout(
    source_rollout_path: &Path,
) -> io::Result<Option<ContextRewindBackendUsageAtLine>> {
    let file = std::fs::File::open(source_rollout_path)?;
    let reader = io::BufReader::new(file);
    let mut latest = None;
    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(usage) =
            context_rewind_backend_usage_from_rollout_entry(line_index.saturating_add(1), &entry)
        {
            latest = Some(usage);
        }
    }
    Ok(latest)
}

/// Pressure band for a usage measurement against the effective context
/// window, mirroring the live managed-context gates: `watch` starts at the
/// `MANAGED_CONTEXT_DENSITY_THRESHOLD_PCT` share of the window (via
/// [`managed_context_density_recommended_limit`], the density-handoff gate),
/// `high` at the window (the rewind-only/recovery gate), and `critical` at
/// the hard window when the snapshot knew it.
pub(crate) fn context_rewind_pressure_band(
    used_tokens: u64,
    context_window: u64,
    hard_context_window: Option<u64>,
) -> &'static str {
    if hard_context_window.is_some_and(|hard| hard > 0 && used_tokens >= hard) {
        return "critical";
    }
    if used_tokens >= context_window {
        return "high";
    }
    if used_tokens >= managed_context_density_recommended_limit(context_window) {
        return "watch";
    }
    "ok"
}

/// Context pressure observed at context-rewind record creation, derived
/// without any new backend RPC: the chronologically last backend
/// `token_count` report in the pre-rewind rollout, falling back to the
/// latest persisted session-log context snapshot (the same source the
/// managed-context preflight falls back to). Returns `(used_tokens,
/// context_window, pressure_band)`; each is `None` when that piece is
/// unavailable — e.g. all three for a rollout without usage reports and no
/// logged snapshot, or the band when the log snapshot lacks either number.
pub(crate) fn context_rewind_pressure_at_record_creation(
    source_rollout_path: &Path,
    config: &DrainConfig<'_>,
) -> (Option<u64>, Option<u64>, Option<String>) {
    match latest_context_rewind_backend_usage_in_rollout(source_rollout_path) {
        Ok(Some(usage)) => {
            let band =
                context_rewind_pressure_band(usage.used_tokens, usage.rewind_only_limit, None);
            return (
                Some(usage.used_tokens),
                Some(usage.rewind_only_limit),
                Some(band.to_string()),
            );
        }
        Ok(None) => {}
        Err(err) => slog(config.session_log, |log| {
            log.warn(&format!(
                "Could not read rollout usage for context-rewind pressure instrumentation: {err}"
            ))
        }),
    }
    let Some(snapshot) = latest_external_context_snapshot_from_log(config) else {
        return (None, None, None);
    };
    let used_tokens = external_context_snapshot_backend_token_count(&snapshot);
    let context_window = snapshot.context_window.filter(|window| *window > 0);
    let band = match (used_tokens, context_window) {
        (Some(used), Some(window)) => Some(
            context_rewind_pressure_band(used, window, snapshot.hard_context_window).to_string(),
        ),
        _ => None,
    };
    (used_tokens, context_window, band)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_rewind_carries_unrepeated_prior_primer_facts_from_pruned_span() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let prior_primer = "<model_context_rewind_primer>\nHistory after the rewind target was pruned by rewind_context. Earlier history before the target is still present. Treat this primer as the carry-forward summary of only the pruned span.\n\nPrimer:\nHost peer connected.\n- Prior selector: .display-picker.visible .display-picker-item\n\n</model_context_rewind_primer>";
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_anchor",
                        "arguments": "{}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": prior_primer }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        let request = ExternalContextRewindRequest {
            session_id: Some("s".to_string()),
            item_id: "call_anchor".to_string(),
            position: external_agent::RollbackAnchorPosition::After,
            reason: Some("repeat rewind".to_string()),
            primer: Some("Host peer connected.".to_string()),
            preserve: Vec::new(),
            discard: Vec::new(),
            artifacts: Vec::new(),
            next_steps: Vec::new(),
            auto_resume: true,
            require_density_improvement: false,
            surgical: false,
        };

        let carried = context_rewind_pruned_prior_primer_facts(
            &path,
            "call_anchor",
            external_agent::RollbackAnchorPosition::After,
            &request,
        )
        .expect("carry-forward scan")
        .expect("missing prior fact");

        assert!(!carried.contains("Host peer connected."));
        assert!(carried.contains(".display-picker.visible .display-picker-item"));
    }

    #[test]
    fn context_rewind_anchor_catalog_lists_all_exact_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "rewind_context",
                        "call_id": "call_prior_rewind",
                        "arguments": "{}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_prior_rewind",
                        "output": "ok"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "developer",
                        "content": [{
                            "type": "input_text",
                            "text": "<model_context_rewind_primer>dense state</model_context_rewind_primer>"
                        }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "rewind_context",
                        "call_id": "call_latest_rewind",
                        "arguments": "{}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "thread_rolled_back",
                        "num_turns": 1
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_latest_rewind",
                        "output": "ok"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "developer",
                        "content": [{
                            "type": "input_text",
                            "text": "<model_context_rewind_primer>newer dense state</model_context_rewind_primer>"
                        }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "continue" }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let default_catalog = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "offset": 0, "limit": 20 }),
        )
        .expect("default anchor catalog");
        let default_catalog: serde_json::Value = serde_json::from_str(&default_catalog).unwrap();
        let default_ids = default_catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert!(!default_ids.contains(&"call_prior_rewind"));
        assert!(!default_ids.contains(&"call_latest_rewind"));

        let catalog = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 20,
                "include_management_tools": true,
            }),
        )
        .expect("anchor catalog");
        let catalog: serde_json::Value = serde_json::from_str(&catalog).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"call_prior_rewind"));
        assert!(ids.contains(&"call_latest_rewind"));

        let filtered = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "query": "latest",
                "offset": 0,
                "limit": 20,
                "include_management_tools": true,
            }),
        )
        .expect("filtered anchor catalog");
        let filtered: serde_json::Value = serde_json::from_str(&filtered).unwrap();
        let filtered_ids = filtered["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(filtered_ids, vec!["call_latest_rewind"]);

        let missing = resolve_context_rewind_anchor(&path, "rewind_context-call_7")
            .expect_err("synthetic anchors are not accepted");
        assert!(missing.contains("call list_rewind_anchors"));
        assert!(!missing.contains("call_latest_rewind"));
        assert!(!missing.contains("call_prior_rewind"));
    }

    #[test]
    fn context_rewind_anchor_catalog_query_hides_recovery_tools_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_launch_dashboard",
                        "arguments": "{\"cmd\":\"./target/debug/intendant --web 8767 --no-presence\"}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_launch_dashboard",
                        "output": "Web TUI: http://0.0.0.0:8767"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "list_rewind_anchors",
                        "call_id": "call_recovery_list",
                        "arguments": "{\"query\":\"target/debug/intendant --web 8767\"}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_recovery_list",
                        "output": "target/debug/intendant --web 8767"
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "query": "target/debug/intendant --web 8767",
                "reverse": true,
                "limit": 10,
            }),
        )
        .expect("anchor catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_launch_dashboard"]);
        assert_eq!(catalog["include_management_tools"].as_bool(), Some(false));

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "query": "target/debug/intendant --web 8767",
                "reverse": true,
                "limit": 10,
                "include_management_tools": true,
            }),
        )
        .expect("anchor catalog with management tools");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_recovery_list", "call_launch_dashboard"]);
    }

    #[test]
    fn context_rewind_anchor_catalog_excludes_active_recovery_span() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_before_recovery",
                        "arguments": "{\"cmd\":\"true\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "id": "msg_recovery_kickstart",
                        "content": [{
                            "type": "input_text",
                            "text": "<managed_context_recovery>Backend-reported Codex context pressure is high. First call list_rewind_anchors without a query.</managed_context_recovery>"
                        }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "list_rewind_anchors",
                        "call_id": "call_recovery_list",
                        "arguments": "{}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_recovery_list",
                        "output": "{\"anchors\":[{\"item_id\":\"call_before_recovery\"}]}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "get_status",
                        "call_id": "call_recovery_status",
                        "arguments": "{}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_after_recovery_prompt",
                        "arguments": "{\"cmd\":\"echo healthy now\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "offset": 0, "limit": 10 }),
        )
        .expect("default recovery catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_before_recovery"]);

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "include_non_recovery": true,
                "include_management_tools": true,
            }),
        )
        .expect("audit catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let anchors = catalog["anchors"].as_array().unwrap();
        let before = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_before_recovery")
            .expect("pre-recovery anchor");
        assert_eq!(before["recovery_eligible"].as_bool(), Some(true));
        assert!(before["managed_context_recovery_start_line"].is_null());

        for item_id in [
            "msg_recovery_kickstart",
            "call_recovery_list",
            "call_recovery_status",
            "call_after_recovery_prompt",
        ] {
            let anchor = anchors
                .iter()
                .find(|anchor| anchor["item_id"] == item_id)
                .unwrap_or_else(|| panic!("missing audit anchor {item_id}: {catalog}"));
            assert_eq!(
                anchor["recovery_eligible"].as_bool(),
                Some(false),
                "got {anchor}"
            );
            assert_eq!(
                anchor["managed_context_recovery_start_line"].as_u64(),
                Some(3),
                "got {anchor}"
            );
        }
    }

    #[test]
    fn context_rewind_anchor_catalog_exposes_compaction_impact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let large_output = "branch detail ".repeat(400);
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_branch_start",
                        "arguments": "{\"cmd\":\"rg display runway\"}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_branch_start",
                        "output": large_output
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_post_commit",
                        "arguments": "{\"cmd\":\"git rev-parse HEAD\"}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_post_commit",
                        "output": "ea304cf"
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let newest_raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "reverse": true, "limit": 1 }),
        )
        .expect("newest-first catalog");
        let newest: serde_json::Value = serde_json::from_str(&newest_raw).unwrap();
        assert!(newest.get("selection_note").is_none(), "got {newest}");
        assert!(newest.get("usage").is_none(), "got {newest}");
        let newest_anchor = &newest["anchors"].as_array().unwrap()[0];
        assert_eq!(newest_anchor["item_id"].as_str(), Some("call_post_commit"));
        assert_eq!(
            newest_anchor["approx_pruned_tokens_after"].as_u64(),
            Some(0),
            "post-branch anchors should reveal that they prune nothing"
        );

        let branch_raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "query": "display runway", "limit": 1 }),
        )
        .expect("branch-start catalog");
        let branch: serde_json::Value = serde_json::from_str(&branch_raw).unwrap();
        let branch_anchor = &branch["anchors"].as_array().unwrap()[0];
        assert_eq!(branch_anchor["item_id"].as_str(), Some("call_branch_start"));
        let pruned_before = branch_anchor["approx_pruned_tokens_before"]
            .as_u64()
            .expect("branch-start anchor should expose before pruning");
        let pruned_after = branch_anchor["approx_pruned_tokens_after"]
            .as_u64()
            .expect("branch-start anchor should expose after pruning");
        assert!(
            pruned_before > 1000,
            "branch-start anchor should expose meaningful before-position pruning impact: {branch_anchor}"
        );
        assert!(
            pruned_after < pruned_before,
            "after-position pruning should show that keeping the branch-start call preserves more context: {branch_anchor}"
        );
    }

    #[test]
    fn context_rewind_density_handoff_rejects_shallow_exact_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_good_density",
                        "arguments": "{\"cmd\":\"cargo test -p intendant focused\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 40_000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 40_000
                            },
                            "model_context_window": 100_000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_shallow_density",
                        "arguments": "{\"cmd\":\"git rev-parse HEAD\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 87_000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 87_000
                            },
                            "model_context_window": 100_000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "density filler ".repeat(40_000)
                        }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_too_shallow_density",
                        "arguments": "{\"cmd\":\"git status --short\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 87_000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 87_000
                            },
                            "model_context_window": 100_000
                        }
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "include_non_recovery": true,
            }),
        )
        .expect("anchor catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let anchors = catalog["anchors"].as_array().unwrap();
        let good = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_good_density")
            .expect("good density anchor");
        assert_eq!(
            good["recommended_rewind_limit_at_or_after_anchor"].as_u64(),
            Some(85_000)
        );
        assert_eq!(good["density_eligible"].as_bool(), Some(true));
        let good_positions = good["density_eligible_positions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|position| position.as_str())
            .collect::<Vec<_>>();
        assert_eq!(good_positions, vec!["before", "after"]);

        let shallow = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_shallow_density")
            .expect("shallow density anchor");
        assert_eq!(shallow["recovery_eligible"].as_bool(), Some(true));
        assert_eq!(shallow["density_eligible"].as_bool(), Some(true));
        let shallow_positions = shallow["density_eligible_positions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|position| position.as_str())
            .collect::<Vec<_>>();
        assert_eq!(shallow_positions, vec!["before"]);
        let all_shallow_positions = shallow["positions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|position| position.as_str())
            .collect::<Vec<_>>();
        assert_eq!(all_shallow_positions, vec!["before", "after"]);

        let too_shallow = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_too_shallow_density")
            .expect("fully shallow density anchor");
        assert_eq!(too_shallow["density_eligible"].as_bool(), Some(false));
        assert!(too_shallow["density_eligible_positions"].is_null());

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "density_candidates_only": true,
                "include_pruning_estimates": true,
            }),
        )
        .expect("density-filtered catalog");
        let density_catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            density_catalog["density_candidates_only"].as_bool(),
            Some(true)
        );
        let density_anchors = density_catalog["anchors"].as_array().unwrap();
        let density_ids = density_anchors
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            density_ids,
            vec!["call_good_density", "call_shallow_density"]
        );
        let shallow_density_row = density_anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_shallow_density")
            .expect("shallow row should remain for before-position density rewind");
        let shallow_density_positions = shallow_density_row["positions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|position| position.as_str())
            .collect::<Vec<_>>();
        assert_eq!(shallow_density_positions, vec!["before"]);
        assert_eq!(
            shallow_density_row["position_hint"].as_str(),
            Some("before")
        );
        assert!(
            density_anchors
                .iter()
                .all(|anchor| anchor["item_id"] != "call_too_shallow_density"),
            "fully shallow anchors should not be offered in density mode: {density_catalog}"
        );

        validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_shallow_density",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect("shallow anchor still has ordinary recovery headroom");
        let err = validate_context_rewind_anchor_density_improvement(
            &path,
            "call_shallow_density",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect_err("density handoff must reject barely useful after-position rewind");
        assert!(err.contains("density handoff rewind anchor"), "got: {err}");
        assert!(err.contains("too shallow"), "got: {err}");
        assert!(
            err.contains("recommended density threshold 85000"),
            "got: {err}"
        );

        validate_context_rewind_anchor_density_improvement(
            &path,
            "call_good_density",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect("earlier exact anchor clears density threshold");
    }

    #[test]
    fn context_rewind_anchor_catalog_filters_known_non_recovery_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_below_soft",
                        "arguments": "{\"cmd\":\"true\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 800,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 10000
                        }
                    }
                }),
                // Bulky filler that physically accounts for the usage growth
                // between the reports, so prefix estimates and backend floors
                // agree that cuts above it cannot recover. (No `id`, so it is
                // not itself an anchor.)
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "bulky filler ".repeat(3_000)
                        }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "view_image",
                        "call_id": "call_near_soft",
                        "arguments": "{}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 9700,
                                "cached_input_tokens": 9000,
                                "output_tokens": 0,
                                "total_tokens": 9700
                            },
                            "model_context_window": 10000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_above_soft",
                        "arguments": "{\"cmd\":\"rg noisy\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "lastTokenUsage": {
                                "inputTokens": 10500,
                                "cachedInputTokens": 10000,
                                "outputTokens": 0,
                                "totalTokens": 10500
                            },
                            "modelContextWindow": 10000
                        }
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "include_non_recovery": true,
            }),
        )
        .expect("full catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let anchors = catalog["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), 3);
        assert_eq!(catalog["include_non_recovery"].as_bool(), Some(true));
        assert_eq!(catalog["recovery_candidates_only"].as_bool(), Some(false));
        let below = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_below_soft")
            .expect("below-soft anchor");
        assert_eq!(
            below["backend_usage_at_or_after_anchor"].as_u64(),
            Some(1000)
        );
        assert_eq!(
            below["rewind_only_limit_at_or_after_anchor"].as_u64(),
            Some(10000)
        );
        assert_eq!(below["recovery_eligible"].as_bool(), Some(true));
        let near = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_near_soft")
            .expect("near-soft anchor");
        assert_eq!(
            near["backend_usage_at_or_after_anchor"].as_u64(),
            Some(9700)
        );
        assert_eq!(near["recovery_eligible"].as_bool(), Some(false));
        let above = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_above_soft")
            .expect("above-soft anchor");
        assert_eq!(
            above["backend_usage_at_or_after_anchor"].as_u64(),
            Some(10500)
        );
        assert_eq!(above["recovery_eligible"].as_bool(), Some(false));

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "detail": true,
                "recovery_candidates_only": true,
            }),
        )
        .expect("recovery catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_below_soft"]);
        assert_eq!(catalog["filtered_total"].as_u64(), Some(1));
        assert_eq!(catalog["include_non_recovery"].as_bool(), Some(false));
        assert_eq!(catalog["recovery_candidates_only"].as_bool(), Some(true));

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": false,
            }),
        )
        .expect("normal catalog ignores false recovery bypass");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_below_soft"]);
        assert_eq!(catalog["include_non_recovery"].as_bool(), Some(false));
        assert_eq!(catalog["recovery_candidates_only"].as_bool(), Some(true));

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": true,
                "include_non_recovery": true,
            }),
        )
        .expect("audit catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["anchors"].as_array().unwrap().len(), 3);
        assert_eq!(catalog["include_non_recovery"].as_bool(), Some(true));
        assert_eq!(catalog["recovery_candidates_only"].as_bool(), Some(false));
    }

    #[test]
    fn context_rewind_anchor_catalog_attributes_usage_after_tool_output_to_later_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let bulky_output = "y".repeat(30_000);
        let token_count = |total: u64| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": total,
                            "cached_input_tokens": 0,
                            "output_tokens": 0,
                            "total_tokens": total
                        },
                        "model_context_window": 10000
                    }
                }
            })
        };
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "id": "msg_user_task",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "run the bulky benchmark command"}]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_bulky_emit",
                        "arguments": "{\"cmd\":\"python3 emit_context.py 3000\"}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_bulky_emit",
                        "output": bulky_output
                    }
                }),
                // Report for the response that EMITTED the call: persisted
                // after the output line but measured a context without it.
                token_count(1500),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "id": "msg_marker_reply",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "MANAGED_INITIAL_OUTPUT_SEEN"}]
                    }
                }),
                // Report for the next response, which did consume the output.
                token_count(9800),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "include_non_recovery": true,
            }),
        )
        .expect("audit catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let anchors = catalog["anchors"].as_array().unwrap();
        let emit = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "call_bulky_emit")
            .expect("bulky emit anchor");
        // The first report that covers the call/output group is the later
        // response's 9800, not the stale 1500 written between call and output.
        assert_eq!(
            emit["backend_usage_at_or_after_anchor"].as_u64(),
            Some(9800)
        );
        assert_eq!(emit["recovery_eligible"].as_bool(), Some(true));
        assert_eq!(
            emit["recovery_eligible_positions"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>(),
            vec!["before"],
            "after keeps the bulky output and must not be offered; before must not be suppressed"
        );
        let user = anchors
            .iter()
            .find(|anchor| anchor["item_id"] == "msg_user_task")
            .expect("user message anchor");
        // The user message was measured by the call-emitting response (1500),
        // so cutting after it has genuine recovery headroom.
        assert_eq!(
            user["backend_usage_at_or_after_anchor"].as_u64(),
            Some(1500)
        );
        assert_eq!(
            user["recovery_eligible_positions"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>(),
            vec!["after"]
        );

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({"offset": 0, "limit": 10}),
        )
        .expect("recovery catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let by_id: Vec<(&str, Vec<&str>)> = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|anchor| {
                (
                    anchor["item_id"].as_str().unwrap(),
                    anchor["positions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|value| value.as_str())
                        .collect(),
                )
            })
            .collect();
        assert!(
            by_id.contains(&("call_bulky_emit", vec!["before"])),
            "recovery catalog must offer the bulky group with position before: {by_id:?}"
        );
        assert!(
            by_id.contains(&("msg_user_task", vec!["after"])),
            "recovery catalog must offer the pre-bulk user message with position after: {by_id:?}"
        );
    }

    #[test]
    fn context_rewind_anchor_catalog_trusts_backend_usage_over_oversized_prefix_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let huge_prior_context = "x".repeat(90_000);
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_fit",
                        "arguments": "{\"cmd\":\"true\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": huge_prior_context }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_after_huge_prefix",
                        "arguments": "{\"cmd\":\"echo after\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": true,
            }),
        )
        .expect("recovery catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_fit", "call_after_huge_prefix"]);

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": false,
            }),
        )
        .expect("normal catalog ignores false recovery bypass");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|anchor| anchor["item_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_fit", "call_after_huge_prefix"]);
        assert_eq!(catalog["include_non_recovery"].as_bool(), Some(false));
        assert_eq!(catalog["recovery_candidates_only"].as_bool(), Some(true));

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": true,
                "include_non_recovery": true,
            }),
        )
        .expect("audit catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let oversized = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|anchor| anchor["item_id"] == "call_after_huge_prefix")
            .expect("oversized-prefix anchor");
        assert_eq!(
            oversized["backend_usage_at_or_after_anchor"].as_u64(),
            Some(1000)
        );
        assert_eq!(oversized["position_hint"].as_str(), Some("after"));
        assert_eq!(oversized["recovery_eligible"].as_bool(), Some(true));
        assert!(
            oversized["prefix_tokens_after"].is_null(),
            "got {oversized}"
        );

        validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_after_huge_prefix",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect("backend-reported headroom should allow the anchor");
    }

    #[test]
    fn context_rewind_anchor_catalog_surfaces_noisy_completed_tool_before_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let warning_output = "warning: unused import\n".repeat(20_000);
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_noisy_build",
                        "arguments": "{\"cmd\":\"cargo build --release --bin intendant -q\"}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_noisy_build",
                        "output": warning_output
                    }
                }),
                // Real rollouts persist the call-output before the next model
                // response; the report that measured the output only arrives
                // after that later response's own items.
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "id": "msg_noisy_build_summary",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "build finished with warnings"}]
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 0,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 60000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "query": "cargo build --release --bin intendant",
                "offset": 0,
                "limit": 10,
                "detail": true,
                "recovery_candidates_only": true,
            }),
        )
        .expect("recovery catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let anchors = catalog["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), 1, "got {catalog}");
        let anchor = &anchors[0];
        assert_eq!(anchor["item_id"].as_str(), Some("call_noisy_build"));
        assert_eq!(anchor["position_hint"].as_str(), Some("before"));
        assert_eq!(anchor["recovery_eligible"].as_bool(), Some(true));
        let positions = anchor["positions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|position| position.as_str())
            .collect::<Vec<_>>();
        assert_eq!(positions, vec!["before"]);
        assert!(anchor["recovery_eligible_positions"].is_null());
        for position in positions {
            let position = external_agent::RollbackAnchorPosition::from_str(position).unwrap();
            validate_context_rewind_anchor_restore_headroom(&path, "call_noisy_build", position)
                .expect("listed recovery position should validate");
        }
        assert!(
            anchor["prefix_tokens_after"]
                .as_u64()
                .is_some_and(|tokens| tokens > 22_000),
            "got {anchor}"
        );

        validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_noisy_build",
            external_agent::RollbackAnchorPosition::Before,
        )
        .expect("before noisy output should be a valid recovery target");

        let err = validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_noisy_build",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect_err("after noisy output should not be a valid recovery target");
        assert!(err.contains("not a valid recovery target"), "got: {err}");
    }

    #[test]
    fn context_rewind_anchor_catalog_filters_prior_failed_rewind_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_failed_recovery_anchor",
                        "arguments": "{\"cmd\":\"true\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "thread_rolled_back",
                        "num_turns": 0,
                        "anchor": {
                            "itemId": "call_failed_recovery_anchor",
                            "position": "after"
                        }
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 0,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 45000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": true,
            }),
        )
        .expect("recovery catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(catalog["anchors"].as_array().unwrap().is_empty());

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": true,
                "include_non_recovery": true,
            }),
        )
        .expect("audit catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let anchor = &catalog["anchors"].as_array().unwrap()[0];
        assert_eq!(anchor["recovery_eligible"].as_bool(), Some(false));
        assert_eq!(
            anchor["latest_rewind_usage_after_anchor"].as_u64(),
            Some(45000)
        );
        assert_eq!(
            anchor["latest_rewind_limit_after_anchor"].as_u64(),
            Some(30000)
        );

        let err = validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_failed_recovery_anchor",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect_err("prior failed rewind must be rejected");
        assert!(err.contains("prior rewind"), "got: {err}");
    }

    #[test]
    fn context_rewind_anchor_catalog_uses_pre_rollback_restore_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_failed_pre_marker",
                        "arguments": "{\"cmd\":\"true\"}"
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1000,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 0,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 45000
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "thread_rolled_back",
                        "num_turns": 1,
                        "anchor": {
                            "itemId": "call_failed_pre_marker",
                            "position": "after"
                        }
                    }
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 1200,
                                "cached_input_tokens": 0,
                                "output_tokens": 0,
                                "total_tokens": 1200
                            },
                            "model_context_window": 30000
                        }
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": true,
            }),
        )
        .expect("recovery catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(catalog["anchors"].as_array().unwrap().is_empty());

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "offset": 0,
                "limit": 10,
                "recovery_candidates_only": true,
                "include_non_recovery": true,
            }),
        )
        .expect("audit catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let anchor = &catalog["anchors"].as_array().unwrap()[0];
        assert_eq!(anchor["recovery_eligible"].as_bool(), Some(false));
        assert_eq!(
            anchor["latest_rewind_usage_after_anchor"].as_u64(),
            Some(45000)
        );

        let err = validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_failed_pre_marker",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect_err("prior pre-marker failed rewind must be rejected");
        assert!(err.contains("prior rewind"), "got: {err}");
    }

    #[test]
    fn context_rewind_anchor_catalog_is_compact_under_pressure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let long_output = "large diagnostic output ".repeat(50);
        let lines = (0..12)
            .flat_map(|idx| {
                let call_id = format!("call_{idx}");
                [
                    serde_json::json!({
                        "type": "response_item",
                        "payload": {
                            "type": "function_call",
                            "name": "exec_command",
                            "call_id": call_id,
                            "arguments": "{\"command\":\"very long command output\"}"
                        }
                    }),
                    serde_json::json!({
                        "type": "response_item",
                        "payload": {
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": long_output
                        }
                    }),
                ]
            })
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, lines).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "offset": 0, "limit": 500 }),
        )
        .expect("anchor catalog");
        assert!(!raw.contains('\n'), "catalog should use compact JSON");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            catalog["limit"].as_u64(),
            Some(CONTEXT_REWIND_ANCHOR_LIST_MAX_LIMIT as u64)
        );
        let anchors = catalog["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), CONTEXT_REWIND_ANCHOR_LIST_MAX_LIMIT);
        for anchor in anchors {
            let summary = anchor["summary"].as_str().unwrap_or_default();
            assert!(summary.len() <= CONTEXT_REWIND_ANCHOR_MERGED_SUMMARY_LIMIT + 3);
        }
        assert!(catalog.get("rollout_path").is_none());
        assert!(catalog.get("selection_note").is_none());
        assert!(catalog.get("usage").is_none());
        assert!(raw.len() < 5_000, "catalog too large: {} bytes", raw.len());
    }

    #[test]
    fn context_rewind_anchor_catalog_default_is_bounded_compact_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let lines = (0..224)
            .map(|idx| {
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": format!("call_{idx}"),
                        "arguments": "{}"
                    }
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, lines).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("anchor catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["catalog_format"].as_str(), Some("compact_page"));
        assert_eq!(
            catalog["limit"].as_u64(),
            Some(CONTEXT_REWIND_ANCHOR_LIST_DEFAULT_LIMIT as u64)
        );
        assert_eq!(
            catalog["next_offset"].as_u64(),
            Some(CONTEXT_REWIND_ANCHOR_LIST_DEFAULT_LIMIT as u64)
        );
        assert_eq!(catalog["filtered_total"].as_u64(), Some(224));
        assert_eq!(
            catalog["output_cap_bytes"].as_u64(),
            Some(CONTEXT_REWIND_ANCHOR_COMPACT_OUTPUT_MAX_BYTES as u64)
        );
        assert_eq!(
            catalog["anchors"].as_array().unwrap().len(),
            CONTEXT_REWIND_ANCHOR_LIST_DEFAULT_LIMIT
        );
        assert!(catalog.get("selection_note").is_none());
        assert!(catalog.get("usage").is_none());
        assert!(
            raw.len() <= CONTEXT_REWIND_ANCHOR_COMPACT_OUTPUT_MAX_BYTES,
            "catalog too large: {} bytes",
            raw.len()
        );
    }

    #[test]
    fn density_maintenance_prompt_and_catalog_stay_inside_soft_headroom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let long_output = "large density diagnostic output ".repeat(80);
        let lines = (0..12)
            .flat_map(|idx| {
                let call_id = format!("call_density_{idx}");
                [
                    serde_json::json!({
                        "type": "response_item",
                        "payload": {
                            "type": "function_call",
                            "name": "exec_command",
                            "call_id": call_id,
                            "arguments": format!("{{\"cmd\":\"density step {idx}\"}}")
                        }
                    }),
                    serde_json::json!({
                        "type": "response_item",
                        "payload": {
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": long_output
                        }
                    }),
                    serde_json::json!({
                        "type": "event_msg",
                        "payload": {
                            "type": "token_count",
                            "info": {
                                "last_token_usage": {
                                    "input_tokens": 80_000 + idx,
                                    "cached_input_tokens": 0,
                                    "output_tokens": 0,
                                    "total_tokens": 80_000 + idx
                                },
                                "model_context_window": 258_400
                            }
                        }
                    }),
                ]
            })
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, lines).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({
                "density_candidates_only": true,
                "include_pruning_estimates": true,
                "limit": 1,
            }),
        )
        .expect("density maintenance catalog");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["catalog_format"].as_str(), Some("compact_page"));
        assert_eq!(catalog["density_candidates_only"].as_bool(), Some(true));
        assert_eq!(catalog["pruning_estimates_included"].as_bool(), Some(true));
        assert_eq!(catalog["limit"].as_u64(), Some(1));
        assert_eq!(catalog["next_offset"].as_u64(), Some(1));
        let anchors = catalog["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), 1);
        let anchor = &anchors[0];
        assert_eq!(anchor["item_id"].as_str(), Some("call_density_0"));
        assert!(
            anchor["positions"]
                .as_array()
                .is_some_and(|positions| !positions.is_empty()),
            "density catalog must return an exact usable position: {anchor}"
        );
        assert!(
            anchor["density_eligible_positions"]
                .as_array()
                .is_some_and(|positions| !positions.is_empty()),
            "density catalog must expose density-valid positions: {anchor}"
        );
        assert!(
            !raw.contains(&"large density diagnostic output ".repeat(8)),
            "compact density catalog leaked raw output"
        );
        assert!(
            raw.len() < 1_500,
            "density catalog too large: {}",
            raw.len()
        );

        let pressure = ManagedContextDensityPressure {
            used_tokens: 253_793,
            recommended_rewind_limit: 219_640,
            rewind_only_limit: 258_400,
            hard_context_window: Some(272_000),
        };
        let prompt = managed_context_density_handoff_text(pressure);
        let overhead_bytes = prompt.len() + raw.len();
        let soft_headroom = pressure
            .rewind_only_limit
            .saturating_sub(pressure.used_tokens);
        assert!(
            overhead_bytes as u64 <= soft_headroom,
            "maintenance prompt+catalog should not cross rewind-only from overhead alone: {overhead_bytes} bytes over {soft_headroom} token headroom"
        );
    }

    #[test]
    fn context_rewind_anchor_compact_pages_discover_all_without_detail_bloat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let long_output = "large diagnostic output ".repeat(80);
        let lines = (0..12)
            .flat_map(|idx| {
                let call_id = format!("call_{idx}");
                [
                    serde_json::json!({
                        "type": "response_item",
                        "payload": {
                            "type": "function_call",
                            "name": "exec_command",
                            "call_id": call_id,
                            "arguments": format!("{{\"command\":\"step {idx}\"}}")
                        }
                    }),
                    serde_json::json!({
                        "type": "response_item",
                        "payload": {
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": long_output
                        }
                    }),
                ]
            })
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, lines).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("anchor catalog");
        assert!(!raw.contains('\n'), "catalog should use compact JSON");
        assert!(
            !raw.contains(&"large diagnostic output ".repeat(10)),
            "compact catalog should not expose repeated raw output blobs"
        );
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["catalog_format"].as_str(), Some("compact_page"));
        assert_eq!(catalog["filtered_total"].as_u64(), Some(12));
        let anchors = catalog["anchors"].as_array().unwrap();
        assert_eq!(anchors.len(), CONTEXT_REWIND_ANCHOR_LIST_DEFAULT_LIMIT);

        let mut ids = Vec::new();
        let mut offset = 0usize;
        loop {
            let raw = list_context_rewind_anchors_from_rollout(
                &path,
                &serde_json::json!({ "offset": offset, "limit": 5 }),
            )
            .expect("anchor catalog page");
            assert!(
                raw.len() <= CONTEXT_REWIND_ANCHOR_COMPACT_OUTPUT_MAX_BYTES,
                "catalog page too large: {} bytes",
                raw.len()
            );
            let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(catalog["catalog_format"].as_str(), Some("compact_page"));
            for anchor in catalog["anchors"].as_array().unwrap() {
                assert!(anchor.get("first_line").is_none(), "got {anchor}");
                assert!(
                    anchor.get("backend_usage_at_or_after_anchor").is_none(),
                    "got {anchor}"
                );
                let summary = anchor["summary"].as_str().unwrap_or_default();
                assert!(summary.len() <= CONTEXT_REWIND_ANCHOR_COMPACT_SUMMARY_LIMIT + 3);
                ids.push(anchor["item_id"].as_str().unwrap().to_string());
            }
            let Some(next_offset) = catalog["next_offset"].as_u64() else {
                break;
            };
            offset = usize::try_from(next_offset).unwrap();
        }
        assert_eq!(
            ids,
            (0..12).map(|idx| format!("call_{idx}")).collect::<Vec<_>>()
        );
    }

    fn rollout_call_lines(name: &str, call_id: &str, with_output: bool) -> Vec<String> {
        let mut lines = vec![serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": name,
                "call_id": call_id,
                "arguments": "{}"
            }
        })
        .to_string()];
        if with_output {
            lines.push(
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": format!("{name} output")
                    }
                })
                .to_string(),
            );
        }
        lines
    }

    #[test]
    fn context_rewind_anchor_catalog_is_idempotent_across_listing_churn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut lines = Vec::new();
        for idx in 0..6 {
            lines.extend(rollout_call_lines(
                "exec_command",
                &format!("call_sub_{idx}"),
                true,
            ));
        }
        // First listing call of the stall (persisted before its output, like
        // the live backend does for the in-flight call).
        lines.extend(rollout_call_lines(
            "list_rewind_anchors",
            "call_list_1",
            false,
        ));
        std::fs::write(&path, lines.join("\n")).unwrap();

        let first_raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("first listing");
        let first: serde_json::Value = serde_json::from_str(&first_raw).unwrap();

        // The stall grows by the first listing's output, a status poll, and
        // the second in-flight listing call — management churn only.
        let mut lines = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        lines.push(
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_list_1",
                    "output": first_raw.clone()
                }
            })
            .to_string(),
        );
        lines.extend(rollout_call_lines("get_status", "call_status_1", true));
        lines.extend(rollout_call_lines(
            "list_rewind_anchors",
            "call_list_2",
            false,
        ));
        std::fs::write(&path, lines.join("\n")).unwrap();

        let second_raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("second listing");
        let second: serde_json::Value = serde_json::from_str(&second_raw).unwrap();

        assert_eq!(
            first["total"], second["total"],
            "total must not count management churn"
        );
        assert_eq!(second["total"].as_u64(), Some(6));
        assert_eq!(first["filtered_total"], second["filtered_total"]);
        assert_eq!(
            first["anchors"], second["anchors"],
            "page rows must be identical"
        );
        let ordinals = |catalog: &serde_json::Value| {
            catalog["anchors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|anchor| anchor["ordinal"].as_u64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(ordinals(&first), ordinals(&second));
        // The second listing is a repeat of an unchanged page and says so.
        assert!(first.get("repeat_listing").is_none());
        assert_eq!(second["repeat_listing"].as_bool(), Some(true));
        let notice = second["notice"].as_str().unwrap_or_default();
        assert!(
            notice.contains("Do not call list_rewind_anchors again"),
            "got: {notice}"
        );
        assert!(notice.contains("rewind_context"), "got: {notice}");
    }

    #[test]
    fn supervisor_status_calls_are_management_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut lines = rollout_call_lines("exec_command", "call_sub", true);
        lines.extend(rollout_call_lines("get_status", "call_status", true));
        lines.extend(rollout_call_lines("get_logs", "call_logs", true));
        std::fs::write(&path, lines.join("\n")).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("default listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["total"].as_u64(), Some(1));
        assert_eq!(catalog["filtered_total"].as_u64(), Some(1));
        let ids = catalog["anchors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|anchor| anchor["item_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["call_sub"]);

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "include_management_tools": true, "detail": true }),
        )
        .expect("management listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["total"].as_u64(), Some(3));
        assert_eq!(catalog["filtered_total"].as_u64(), Some(3));
    }

    #[test]
    fn paging_and_queries_are_not_flagged_as_repeat_listings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut lines = Vec::new();
        for idx in 0..8 {
            lines.extend(rollout_call_lines(
                "exec_command",
                &format!("call_sub_{idx}"),
                true,
            ));
        }
        lines.extend(rollout_call_lines(
            "list_rewind_anchors",
            "call_list_1",
            true,
        ));
        lines.extend(rollout_call_lines(
            "list_rewind_anchors",
            "call_list_2",
            false,
        ));
        std::fs::write(&path, lines.join("\n")).unwrap();

        // Paging to the next offset is deliberate progress, not a repeat.
        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "offset": 5, "limit": 5 }),
        )
        .expect("paged listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(catalog.get("repeat_listing").is_none(), "got: {catalog}");
        assert!(catalog.get("notice").is_none(), "got: {catalog}");

        // A focused query is a different view, not a repeat.
        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "query": "call_sub_3" }),
        )
        .expect("query listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(catalog.get("repeat_listing").is_none(), "got: {catalog}");

        // A reverse listing is a different view, not a repeat.
        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "reverse": true }),
        )
        .expect("reverse listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(catalog.get("repeat_listing").is_none(), "got: {catalog}");

        // The bare default view is the loop signature and is flagged.
        let raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("repeat listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["repeat_listing"].as_bool(), Some(true));
    }

    #[test]
    fn substantive_progress_clears_repeat_listing_detection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut lines = Vec::new();
        lines.extend(rollout_call_lines("exec_command", "call_sub_0", true));
        lines.extend(rollout_call_lines(
            "list_rewind_anchors",
            "call_list_1",
            true,
        ));
        // Substantive work after the earlier listing ends the stall.
        lines.extend(rollout_call_lines("exec_command", "call_sub_1", true));
        lines.extend(rollout_call_lines(
            "list_rewind_anchors",
            "call_list_2",
            false,
        ));
        std::fs::write(&path, lines.join("\n")).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("listing after progress");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(catalog.get("repeat_listing").is_none(), "got: {catalog}");
    }

    #[test]
    fn empty_eligible_catalog_reports_dead_end_plainly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut lines = rollout_call_lines("get_status", "call_status", true);
        lines.extend(rollout_call_lines(
            "list_rewind_anchors",
            "call_list_1",
            false,
        ));
        std::fs::write(&path, lines.join("\n")).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({}))
            .expect("empty listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["filtered_total"].as_u64(), Some(0));
        assert_eq!(catalog["anchors"].as_array().map(Vec::len), Some(0));
        assert_eq!(catalog["no_eligible_anchors"].as_bool(), Some(true));
        assert_eq!(
            catalog["empty_page_reason"].as_str(),
            Some("no_eligible_anchors")
        );
        let notice = catalog["notice"].as_str().unwrap_or_default();
        assert!(
            notice.contains("no eligible rewind anchors remain"),
            "got: {notice}"
        );
        assert!(notice.contains("manual recovery path"), "got: {notice}");
        assert!(
            notice.contains("do not call list_rewind_anchors again"),
            "got: {notice}"
        );

        // A density-only listing in the same state directs continuing the
        // task instead of ending the session.
        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "density_candidates_only": true, "limit": 1 }),
        )
        .expect("empty density listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog["no_eligible_anchors"].as_bool(), Some(true));
        let notice = catalog["notice"].as_str().unwrap_or_default();
        assert!(notice.contains("skip density maintenance"), "got: {notice}");
        assert!(notice.contains("continue the task"), "got: {notice}");
    }

    #[test]
    fn empty_pages_distinguish_query_and_offset_dead_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut lines = Vec::new();
        for idx in 0..3 {
            lines.extend(rollout_call_lines(
                "exec_command",
                &format!("call_sub_{idx}"),
                true,
            ));
        }
        std::fs::write(&path, lines.join("\n")).unwrap();

        let raw = list_context_rewind_anchors_from_rollout(
            &path,
            &serde_json::json!({ "query": "no_such_item" }),
        )
        .expect("query listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            catalog["empty_page_reason"].as_str(),
            Some("query_unmatched")
        );
        assert!(catalog.get("no_eligible_anchors").is_none());
        let notice = catalog["notice"].as_str().unwrap_or_default();
        assert!(
            notice.contains("re-list once without a query"),
            "got: {notice}"
        );

        let raw =
            list_context_rewind_anchors_from_rollout(&path, &serde_json::json!({ "offset": 13 }))
                .expect("past-end listing");
        let catalog: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            catalog["empty_page_reason"].as_str(),
            Some("offset_past_end")
        );
        assert!(catalog.get("no_eligible_anchors").is_none());
        let notice = catalog["notice"].as_str().unwrap_or_default();
        assert!(notice.contains("past the end"), "got: {notice}");
        assert!(notice.contains("Do not keep paging"), "got: {notice}");
    }

    fn write_user_turn_rollout(path: &Path, turns: &[&str]) {
        let mut rows = vec![serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "<environment_context>ignored injected context</environment_context>"
                }]
            }
        })];
        rows.extend(turns.iter().map(|turn| {
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": turn }]
                }
            })
        }));
        std::fs::write(
            path,
            rows.into_iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    }

    fn write_event_user_turn_rollout(path: &Path, events: &[serde_json::Value]) {
        std::fs::write(
            path,
            events
                .iter()
                .map(|payload| {
                    serde_json::json!({
                        "type": "event_msg",
                        "payload": payload
                    })
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    }

    fn write_rewind_record(
        log_dir: &Path,
        record_id: &str,
        created_at: &str,
        recovery_rollout_path: &Path,
    ) {
        context_rewind::persist_record(
            log_dir,
            &context_rewind::ContextRewindRecord {
                record_id: record_id.to_string(),
                created_at: created_at.to_string(),
                session_id: Some("intendant-session".to_string()),
                thread_id: "codex-thread".to_string(),
                item_id: "call_anchor".to_string(),
                position: "after".to_string(),
                reason: Some("test".to_string()),
                primer: Some("dense".to_string()),
                preserve: Vec::new(),
                discard: Vec::new(),
                artifacts: Vec::new(),
                next_steps: Vec::new(),
                source_rollout_path: None,
                recovery_rollout_path: Some(recovery_rollout_path.to_path_buf()),
                fission_snapshot: None,
                lineage_ledger: None,
                fission_ledger: None,
                detached_fission_group_ids: Vec::new(),
                used_tokens_at_rewind: None,
                context_window_at_rewind: None,
                pressure_band_at_rewind: None,
                surgical: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn managed_context_edit_branch_resolves_latest_matching_archived_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let old_rollout = dir.path().join("old.jsonl");
        let new_rollout = dir.path().join("new.jsonl");
        write_user_turn_rollout(&old_rollout, &["first", "clicked", "old tail"]);
        write_user_turn_rollout(
            &new_rollout,
            &["first", "clicked", "new tail", "latest tail"],
        );
        write_rewind_record(
            dir.path(),
            "rewind-old",
            "2026-06-01T00:00:00Z",
            &old_rollout,
        );
        write_rewind_record(
            dir.path(),
            "rewind-new",
            "2026-06-02T00:00:00Z",
            &new_rollout,
        );

        let target = resolve_managed_context_edit_branch_target(
            dir.path(),
            "codex-thread",
            Some("intendant-session"),
            2,
            Some("clicked"),
        )
        .unwrap()
        .expect("matching archived rollout");

        assert_eq!(target.record_id, "rewind-new");
        assert_eq!(target.source_turn_count, 4);
        assert_eq!(target.target_turn_text, "clicked");
    }

    #[test]
    fn rollout_user_turns_prefers_canonical_events_and_rewinds() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_event_user_turn_rollout(
            &rollout,
            &[
                serde_json::json!({ "type": "user_message", "message": "first" }),
                serde_json::json!({ "type": "user_message", "message": "continue" }),
                serde_json::json!({ "type": "thread_rolled_back", "num_turns": 1 }),
                serde_json::json!({ "type": "user_message", "message": "managed recovery" }),
            ],
        );

        let turns = rollout_user_turns(&rollout).unwrap();

        assert_eq!(
            turns
                .iter()
                .map(|turn| (turn.index, turn.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "first"), (2, "managed recovery")]
        );
    }

    #[test]
    fn managed_context_edit_branch_scans_historical_wrapper_logs() {
        let home = tempfile::tempdir().unwrap();
        let current_log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("current-wrapper");
        let historical_log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("historical-wrapper");
        std::fs::create_dir_all(&current_log_dir).unwrap();
        std::fs::create_dir_all(&historical_log_dir).unwrap();

        let old_rollout = historical_log_dir.join("old.jsonl");
        let new_rollout = historical_log_dir.join("new.jsonl");
        write_event_user_turn_rollout(
            &old_rollout,
            &[
                serde_json::json!({ "type": "user_message", "message": "first" }),
                serde_json::json!({ "type": "user_message", "message": "continue" }),
            ],
        );
        write_event_user_turn_rollout(
            &new_rollout,
            &[
                serde_json::json!({ "type": "user_message", "message": "first" }),
                serde_json::json!({ "type": "user_message", "message": "managed recovery" }),
            ],
        );
        write_rewind_record(
            &historical_log_dir,
            "rewind-old",
            "2026-06-02T08:08:19Z",
            &old_rollout,
        );
        write_rewind_record(
            &historical_log_dir,
            "rewind-new",
            "2026-06-02T08:11:12Z",
            &new_rollout,
        );

        let target = resolve_managed_context_edit_branch_target(
            &current_log_dir,
            "codex-thread",
            Some("intendant-session"),
            2,
            Some("continue"),
        )
        .unwrap()
        .expect("matching historical rollout");

        assert_eq!(target.record_id, "rewind-old");
        assert_eq!(target.source_turn_count, 2);
        assert_eq!(target.target_turn_text, "continue");
    }

    #[test]
    fn managed_context_edit_branch_rejects_text_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_user_turn_rollout(&rollout, &["first", "different"]);
        write_rewind_record(dir.path(), "rewind-one", "2026-06-02T00:00:00Z", &rollout);

        let err = resolve_managed_context_edit_branch_target(
            dir.path(),
            "codex-thread",
            Some("intendant-session"),
            2,
            Some("clicked"),
        )
        .unwrap_err();

        assert!(err.contains("none matched the clicked message text"));
    }

    #[test]
    fn context_rewind_anchor_inspect_returns_neighbor_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "before anchor" }]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_exact",
                        "arguments": "{\"cmd\":\"pwd\"}"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_exact",
                        "output": "/tmp/project"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "after anchor" }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let inspection = inspect_context_rewind_anchor_from_rollout(
            &path,
            &serde_json::json!({ "anchor": { "item_id": "call_exact" }, "radius": 1 }),
        )
        .expect("inspection");
        let inspection: serde_json::Value = serde_json::from_str(&inspection).unwrap();
        assert_eq!(inspection["anchor"]["item_id"], "call_exact");
        assert_eq!(inspection["radius"], 1);
        let context = inspection["context"].as_array().unwrap();
        assert_eq!(context.len(), 4);
        assert!(context.iter().any(|item| item["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("before anchor"))));
        assert!(context.iter().any(|item| item["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("after anchor"))));
        assert!(context
            .iter()
            .any(|item| item["anchor_span"].as_bool() == Some(true)));
        assert!(inspection["usage"]
            .as_str()
            .unwrap()
            .contains("pass only the exact item_id"));
    }

    fn write_rollback_test_rollout(path: &Path, lines: &[serde_json::Value]) {
        std::fs::write(
            path,
            lines
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    }

    fn rollback_test_call(call_id: &str) -> [serde_json::Value; 2] {
        [
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"echo hi\"}",
                    "call_id": call_id
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": "hi"
                }
            }),
        ]
    }

    fn rollback_test_marker(item_id: &str, position: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "thread_rolled_back",
                "num_turns": 1,
                "anchor": { "itemId": item_id, "position": position }
            }
        })
    }

    fn catalog_item_ids(anchors: &[ContextRewindAnchorCatalogEntry]) -> Vec<String> {
        anchors
            .iter()
            .map(|anchor| anchor.item_id.clone())
            .collect()
    }

    #[test]
    fn catalog_drops_anchors_cut_by_prior_rollback_position_before() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        let [a_call, a_out] = rollback_test_call("call_a");
        let [b_call, b_out] = rollback_test_call("call_b");
        let [c_call, c_out] = rollback_test_call("call_c");
        let [d_call, d_out] = rollback_test_call("call_d");
        write_rollback_test_rollout(
            &rollout,
            &[
                a_call,
                a_out,
                b_call,
                b_out,
                c_call,
                c_out,
                // Rollback to *before* call_b: call_b and call_c leave
                // effective history; the fork would reject them as anchors.
                rollback_test_marker("call_b", "before"),
                d_call,
                d_out,
            ],
        );
        let anchors = scan_context_rewind_anchor_catalog(&rollout).unwrap();
        assert_eq!(catalog_item_ids(&anchors), vec!["call_a", "call_d"]);
        // Ordinals re-assigned after the filter; lines reflect live spans.
        assert_eq!(anchors[0].ordinal, 0);
        assert_eq!(anchors[1].ordinal, 1);
        assert_eq!(anchors[0].first_line, 1);
        assert_eq!(anchors[0].last_line, 2);
        assert_eq!(anchors[1].first_line, 8);
        assert_eq!(anchors[1].last_line, 9);
    }

    #[test]
    fn catalog_keeps_anchor_group_on_position_after_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        let [a_call, a_out] = rollback_test_call("call_a");
        let [b_call, b_out] = rollback_test_call("call_b");
        let [c_call, c_out] = rollback_test_call("call_c");
        let [d_call, d_out] = rollback_test_call("call_d");
        write_rollback_test_rollout(
            &rollout,
            &[
                a_call,
                a_out,
                b_call,
                b_out,
                c_call,
                c_out,
                // Rollback to *after* call_b keeps the call/output group;
                // only call_c is cut.
                rollback_test_marker("call_b", "after"),
                d_call,
                d_out,
            ],
        );
        let anchors = scan_context_rewind_anchor_catalog(&rollout).unwrap();
        assert_eq!(
            catalog_item_ids(&anchors),
            vec!["call_a", "call_b", "call_d"]
        );
    }

    #[test]
    fn catalog_replays_chained_rollbacks_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        let [a_call, a_out] = rollback_test_call("call_a");
        let [b_call, b_out] = rollback_test_call("call_b");
        let [c_call, c_out] = rollback_test_call("call_c");
        let [d_call, d_out] = rollback_test_call("call_d");
        write_rollback_test_rollout(
            &rollout,
            &[
                a_call,
                a_out,
                b_call,
                b_out,
                c_call,
                c_out,
                // First cut: before call_c (drops call_c only).
                rollback_test_marker("call_c", "before"),
                d_call,
                d_out,
                // Second cut: after call_a (drops call_b and call_d; the
                // already-dead call_c span stays dead).
                rollback_test_marker("call_a", "after"),
            ],
        );
        let anchors = scan_context_rewind_anchor_catalog(&rollout).unwrap();
        assert_eq!(catalog_item_ids(&anchors), vec!["call_a"]);
    }

    #[test]
    fn catalog_rollback_cut_parses_marker_variants() {
        // itemId (wire form) and item_id (serde form) both parse; position
        // defaults to `after` when missing.
        let entry = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "thread_rolled_back",
                "num_turns": 2,
                "anchor": { "item_id": "call_x" }
            }
        });
        let (item_id, position) =
            context_rewind_rollback_cut_from_rollout_entry(&entry).expect("anchor cut");
        assert_eq!(item_id, "call_x");
        assert_eq!(position, external_agent::RollbackAnchorPosition::After);
        // Anchor-less plain N-turn rollbacks are conservatively ignored.
        let plain = serde_json::json!({
            "type": "event_msg",
            "payload": { "type": "thread_rolled_back", "num_turns": 3 }
        });
        assert!(context_rewind_rollback_cut_from_rollout_entry(&plain).is_none());
    }

    #[test]
    fn context_rewind_pressure_band_mirrors_managed_context_gates() {
        // `watch` starts at the MANAGED_CONTEXT_DENSITY_THRESHOLD_PCT share
        // of the window (85% of 38_000 = 32_300), `high` at the window,
        // `critical` at the hard window when known.
        assert_eq!(context_rewind_pressure_band(0, 38_000, None), "ok");
        assert_eq!(context_rewind_pressure_band(32_299, 38_000, None), "ok");
        assert_eq!(context_rewind_pressure_band(32_300, 38_000, None), "watch");
        assert_eq!(context_rewind_pressure_band(37_999, 38_000, None), "watch");
        assert_eq!(context_rewind_pressure_band(38_000, 38_000, None), "high");
        assert_eq!(
            context_rewind_pressure_band(39_000, 38_000, Some(40_000)),
            "high"
        );
        assert_eq!(
            context_rewind_pressure_band(41_772, 38_000, Some(40_000)),
            "critical"
        );
        // An unknown or zero hard window never reports critical.
        assert_eq!(
            context_rewind_pressure_band(1_000_000, 38_000, Some(0)),
            "high"
        );
        // The watch threshold tracks the shared density constant, not a copy.
        let recommended = managed_context_density_recommended_limit(38_000);
        assert_eq!(
            context_rewind_pressure_band(recommended, 38_000, None),
            "watch"
        );
        assert_eq!(
            context_rewind_pressure_band(recommended - 1, 38_000, None),
            "ok"
        );
    }
}
