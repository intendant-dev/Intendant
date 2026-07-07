//! Managed-context operations for supervised external agents: rewind
//! requests and anchor catalogs, preflight/density/recovery pressure
//! gates, primer pruning, side-session follow-up turns, context-usage
//! snapshots, and the rewind follow-up replay text machinery.

use crate::error::CallerError;
use crate::{context_rewind, external_wrapper_index, fission_ledger, fission_lifecycle, frontend, lineage_ledger};
use crate::{
    codex_payload_user_text, drain_external_agent_events, drain_steer_queue_as_followup,
    emit_external_turn_status, emit_fission_detach_relationships, emit_follow_up_status,
    emit_user_message_log, fission_anchor_cut_line, fission_anchor_first_lines,
    fission_anchor_reachable_after_rewind, fission_detach_parent_candidates,
    is_codex_injected_user_text_for_main, CodexThreadActionDedupe, DrainOutcome,
    ExternalBackendRecovery, ExternalContextSnapshotState, ExternalDiffDeltaTracker,
    PendingRuntimeSteer, UserAttachments, UserTurnRevisionState,
};
use serde::Serialize;
use crate::event::{AppEvent, EventBus};
use crate::external_agent;
use crate::session_log;
use crate::{slog, DrainConfig, FollowUpMessage, LoopStats};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagedContextRewindOnlyPressure {
    pub(crate) used_tokens: u64,
    pub(crate) rewind_only_limit: u64,
    pub(crate) hard_context_window: Option<u64>,
    pub(crate) status: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagedContextDensityPressure {
    pub(crate) used_tokens: u64,
    pub(crate) recommended_rewind_limit: u64,
    pub(crate) rewind_only_limit: u64,
    pub(crate) hard_context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalContextRewindRequest {
    pub(crate) session_id: Option<String>,
    pub(crate) item_id: String,
    pub(crate) position: external_agent::RollbackAnchorPosition,
    pub(crate) reason: Option<String>,
    pub(crate) primer: Option<String>,
    pub(crate) preserve: Vec<String>,
    pub(crate) discard: Vec<String>,
    pub(crate) artifacts: Vec<String>,
    pub(crate) next_steps: Vec<String>,
    pub(crate) auto_resume: bool,
    pub(crate) require_density_improvement: bool,
    /// Supervisor-chosen anchor + synthetic primer (surgical recovery after
    /// the model exhausted its recovery step limit without rewinding).
    /// Marked on the durable rewind record.
    pub(crate) surgical: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum ManagedContextRewindTurnStopStatus {
    #[default]
    NotRequested,
    StopRequestedNoToolObserved,
    StopRequestedCompleted {
        success: usize,
        failed: usize,
        cancelled: usize,
    },
    StopRequestedUnfinished {
        pending: usize,
        success: usize,
        failed: usize,
        cancelled: usize,
    },
    StopRequestFailed {
        message: String,
    },
}

#[derive(Debug, Default)]
pub(crate) struct ManagedContextRewindTurnStopTracker {
    stop_requested: bool,
    stop_error: Option<String>,
    pending_tools: HashSet<String>,
    success: usize,
    failed: usize,
    cancelled: usize,
}

impl ManagedContextRewindTurnStopTracker {
    pub(crate) fn request_stop(&mut self, active_tools: &HashSet<String>) {
        self.stop_requested = true;
        self.stop_error = None;
        self.pending_tools.extend(active_tools.iter().cloned());
    }

    pub(crate) fn fail_stop(&mut self, active_tools: &HashSet<String>, message: String) {
        self.stop_requested = true;
        self.stop_error = Some(message);
        self.pending_tools.extend(active_tools.iter().cloned());
    }

    pub(crate) fn record_tool_started(&mut self, item_id: &str) {
        if self.stop_requested && !item_id.trim().is_empty() {
            self.pending_tools.insert(item_id.to_string());
        }
    }

    pub(crate) fn record_tool_completed(
        &mut self,
        item_id: &str,
        status: &external_agent::ToolCompletionStatus,
    ) {
        if !self.stop_requested || !self.pending_tools.remove(item_id) {
            return;
        }
        match status {
            external_agent::ToolCompletionStatus::Success => self.success += 1,
            external_agent::ToolCompletionStatus::Failed { .. } => self.failed += 1,
            external_agent::ToolCompletionStatus::Cancelled => self.cancelled += 1,
        }
    }

    pub(crate) fn status(&self) -> ManagedContextRewindTurnStopStatus {
        if let Some(message) = self.stop_error.as_deref() {
            return ManagedContextRewindTurnStopStatus::StopRequestFailed {
                message: message.to_string(),
            };
        }
        if !self.stop_requested {
            return ManagedContextRewindTurnStopStatus::NotRequested;
        }
        let completed = self.success + self.failed + self.cancelled;
        if self.pending_tools.is_empty() {
            if completed == 0 {
                ManagedContextRewindTurnStopStatus::StopRequestedNoToolObserved
            } else {
                ManagedContextRewindTurnStopStatus::StopRequestedCompleted {
                    success: self.success,
                    failed: self.failed,
                    cancelled: self.cancelled,
                }
            }
        } else {
            ManagedContextRewindTurnStopStatus::StopRequestedUnfinished {
                pending: self.pending_tools.len(),
                success: self.success,
                failed: self.failed,
                cancelled: self.cancelled,
            }
        }
    }
}

impl ExternalContextRewindRequest {
    pub(crate) fn rendered_primer(
        &self,
        record_id: Option<&str>,
        carried_forward_prior_facts: Option<&str>,
    ) -> Option<String> {
        let primer = self.primer.as_deref()?.trim();
        if primer.is_empty() {
            return None;
        }
        let mut out = String::from(
            "<model_context_rewind_primer>\nHistory after the rewind target was pruned by rewind_context. Earlier history before the target is still present. Treat this primer as the carry-forward summary of only the pruned span.\n\n",
        );
        push_context_rewind_text_section(
            &mut out,
            "Reason",
            self.reason.as_deref().unwrap_or("context rewind"),
        );
        if let Some(record_id) = record_id.map(str::trim).filter(|id| !id.is_empty()) {
            push_context_rewind_text_section(&mut out, "Record id", record_id);
        }
        push_context_rewind_text_section(&mut out, "Primer", primer);
        push_context_rewind_list_section(&mut out, "Preserve", &self.preserve);
        push_context_rewind_list_section(&mut out, "Discard", &self.discard);
        push_context_rewind_list_section(&mut out, "Artifacts", &self.artifacts);
        push_context_rewind_list_section(&mut out, "Next steps", &self.next_steps);
        if let Some(facts) = carried_forward_prior_facts
            .map(str::trim)
            .filter(|facts| !facts.is_empty())
        {
            push_context_rewind_text_section(
                &mut out,
                "Previous managed-context primer facts not repeated above",
                facts,
            );
        }
        out.push_str("</model_context_rewind_primer>");
        Some(out)
    }

    pub(crate) fn resume_followup(&self) -> Option<FollowUpMessage> {
        if !self.auto_resume || self.primer.as_deref()?.trim().is_empty() {
            return None;
        }
        Some(FollowUpMessage::text(
            "<context_rewind_resumed>\nContinue from the model_context_rewind_primer that Intendant injected as developer context for the pruned span. Do not redo discarded work; continue with the next useful step.\n</context_rewind_resumed>"
                .to_string(),
        ))
    }

    pub(crate) fn target_label(&self) -> String {
        format!("{} item {}", self.position.as_str(), self.item_id)
    }
}

pub(crate) fn context_rewind_should_interrupt_active_turn(request: &ExternalContextRewindRequest) -> bool {
    // Model-origin managed rewinds must make the active Codex turn idle before
    // Intendant can apply the rewrite. Relying on the model to stop after the
    // tool result is brittle under context pressure: a single response can keep
    // issuing recovery tools and grow context before the scheduler runs.
    request.auto_resume
}

pub(crate) fn context_rewind_active_tool_defer_message(
    request: &ExternalContextRewindRequest,
    active_tool_count: usize,
    normal_tools_allowed: bool,
) -> Option<String> {
    if !request.auto_resume || active_tool_count == 0 || !normal_tools_allowed {
        return None;
    }
    Some(format!(
        "context rewind deferred to {}; {active_tool_count} active tool(s)/command(s) are still running and normal tools are currently allowed. Intendant did not stop the active turn or schedule the rewind. Wait for the active tool/command completion, then retry rewind_context with the same anchor and primer if cleanup is still needed.",
        request.target_label()
    ))
}

pub(crate) fn context_rewind_blocking_active_tool_count(
    active_tool_count: usize,
    action_origin: Option<&str>,
) -> usize {
    if action_origin == Some("mcp") {
        // The rewind_context MCP call is itself an active tool until this
        // handler returns; do not let it block its own scheduling.
        active_tool_count.saturating_sub(1)
    } else {
        active_tool_count
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedContextRewindAnchor {
    item_id: String,
}

impl ResolvedContextRewindAnchor {
    pub(crate) fn requested(item_id: &str) -> Self {
        Self {
            item_id: item_id.to_string(),
        }
    }

    pub(crate) fn target_label(&self, position: external_agent::RollbackAnchorPosition) -> String {
        format!("{} item {}", position.as_str(), self.item_id)
    }
}

pub(crate) fn push_context_rewind_text_section(out: &mut String, label: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    out.push_str(label);
    out.push_str(":\n");
    out.push_str(value);
    out.push_str("\n\n");
}

pub(crate) fn push_context_rewind_list_section(out: &mut String, label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.push_str(label);
    out.push_str(":\n");
    for value in values {
        out.push_str("- ");
        out.push_str(value);
        out.push('\n');
    }
    out.push('\n');
}

pub(crate) fn clean_context_rewind_list(params: &serde_json::Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn optional_trimmed_string(params: &serde_json::Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn context_rewind_anchor_item_id(params: &serde_json::Value) -> Option<String> {
    params
        .pointer("/anchor/itemId")
        .and_then(|value| value.as_str())
        .or_else(|| {
            params
                .pointer("/anchor/item_id")
                .and_then(|value| value.as_str())
        })
        .or_else(|| params.get("itemId").and_then(|value| value.as_str()))
        .or_else(|| params.get("item_id").and_then(|value| value.as_str()))
        .or_else(|| params.as_str())
        .map(str::trim)
        .filter(|item_id| !item_id.is_empty())
        .map(str::to_string)
}

pub(crate) fn context_rewind_anchor_position(
    params: &serde_json::Value,
) -> Option<external_agent::RollbackAnchorPosition> {
    match params
        .pointer("/anchor/position")
        .and_then(|value| value.as_str())
        .or_else(|| params.get("position").and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => external_agent::RollbackAnchorPosition::from_str(value),
        None => Some(external_agent::RollbackAnchorPosition::After),
    }
}

pub(crate) fn is_context_rewind_action(action: &str) -> bool {
    matches!(
        action,
        "rewind_context"
            | "rewind-context"
            | "rewind-anchor"
            | "rewind_anchor"
            | "rewind-to-item"
            | "rewind_to_item"
            | "rollback-anchor"
            | "rollback_anchor"
            | "rollback-to-item"
            | "rollback_to_item"
    )
}

pub(crate) fn external_context_rewind_request_from_action(
    action: &str,
    params: &serde_json::Value,
    session_id: Option<String>,
) -> Option<Result<ExternalContextRewindRequest, String>> {
    if !is_context_rewind_action(action) {
        return None;
    }

    let item_id = match context_rewind_anchor_item_id(params) {
        Some(item_id) => item_id,
        None => {
            return Some(Err(
                "rewind_context requires anchor.item_id or itemId".to_string()
            ));
        }
    };
    let position = match context_rewind_anchor_position(params) {
        Some(position) => position,
        None => {
            return Some(Err(
                "rewind_context anchor.position must be `before` or `after`".to_string(),
            ));
        }
    };
    let is_model_rewind = matches!(action, "rewind_context" | "rewind-context");
    let reason = optional_trimmed_string(params, "reason");
    let primer = optional_trimmed_string(params, "primer");
    if is_model_rewind {
        if reason.is_none() {
            return Some(Err("rewind_context requires a non-empty reason".to_string()));
        }
        if primer.is_none() {
            return Some(Err("rewind_context requires a non-empty primer".to_string()));
        }
    }

    Some(Ok(ExternalContextRewindRequest {
        session_id,
        item_id,
        position,
        reason,
        primer,
        preserve: clean_context_rewind_list(params, "preserve"),
        discard: clean_context_rewind_list(params, "discard"),
        artifacts: clean_context_rewind_list(params, "artifacts"),
        next_steps: clean_context_rewind_list(params, "next_steps"),
        auto_resume: is_model_rewind,
        require_density_improvement: false,
        surgical: false,
    }))
}

pub(crate) fn backend_recovery_outcome_or_context_rewind(
    request: Option<ExternalContextRewindRequest>,
    turn_stop_status: ManagedContextRewindTurnStopStatus,
    recovery: Option<ExternalBackendRecovery>,
    message: Option<String>,
    turns_in_round: usize,
) -> DrainOutcome {
    if let Some(request) = request {
        return DrainOutcome::ContextRewindRequested {
            request,
            message,
            turns_in_round,
            turn_stop_status,
        };
    }
    if let Some(recovery) = recovery {
        return DrainOutcome::RecoveryRequired {
            message: recovery.message,
            recovery_hint: recovery.recovery_hint,
            turns_in_round,
        };
    }
    DrainOutcome::TurnCompleted {
        message,
        turns_in_round,
    }
}

pub(crate) fn recovery_required_message(message: &str, recovery_hint: Option<&str>) -> String {
    let mut out = format!("External agent recovery required after backend error: {message}");
    if let Some(hint) = recovery_hint.filter(|hint| !hint.trim().is_empty()) {
        out.push_str("\nRecovery: ");
        out.push_str(hint.trim());
    }
    out
}

pub(crate) fn resolve_context_rewind_anchor(
    source_rollout_path: &Path,
    requested_item_id: &str,
) -> Result<ResolvedContextRewindAnchor, String> {
    let requested_item_id = requested_item_id.trim();
    if requested_item_id.is_empty() {
        return Err("rewind anchor item id is required".to_string());
    }

    let anchor = find_context_rewind_anchor_entry(source_rollout_path, requested_item_id).map_err(
        |err| {
            format!(
                "failed to inspect rollout anchors in {}: {err}",
                source_rollout_path.display()
            )
        },
    )?;
    if anchor.is_none() {
        return Err(format!(
            "rollback anchor item_id `{requested_item_id}` was not found in {}; call list_rewind_anchors to inspect valid exact anchors before retrying",
            source_rollout_path.display()
        ));
    }

    Ok(ResolvedContextRewindAnchor::requested(requested_item_id))
}

pub(crate) fn context_rewind_anchor_prefix_estimate(
    anchor: &ContextRewindAnchorCatalogEntry,
    position: external_agent::RollbackAnchorPosition,
) -> Option<u64> {
    match position {
        external_agent::RollbackAnchorPosition::Before => {
            anchor.prefix_estimated_tokens_before_anchor
        }
        external_agent::RollbackAnchorPosition::After => {
            anchor.prefix_estimated_tokens_after_anchor
        }
    }
}

pub(crate) fn context_rewind_anchor_restore_usage_for_headroom(
    anchor: &ContextRewindAnchorCatalogEntry,
    position: external_agent::RollbackAnchorPosition,
) -> Option<(&'static str, u64, u64)> {
    let rewind_only_limit = anchor.rewind_only_limit_at_or_after_anchor?;
    if position == external_agent::RollbackAnchorPosition::After {
        if let Some(backend_tokens) = anchor.backend_usage_at_or_after_anchor {
            return Some(("backend-reported", backend_tokens, rewind_only_limit));
        }
    }
    let prefix_tokens = context_rewind_anchor_prefix_estimate(anchor, position)?;
    // A backend report that measured a strict prefix of the cut is a real
    // lower bound on the post-rewind context. Char-based prefix estimates
    // cannot see instructions or tool specs, so when the backend floor is
    // higher it is the honest eligibility input — otherwise an optimistic
    // estimate offers cuts that keep far more than the threshold allows.
    let backend_floor = anchor.backend_usage_before_anchor.unwrap_or(0);
    if backend_floor > prefix_tokens {
        return Some(("backend-reported", backend_floor, rewind_only_limit));
    }
    Some(("estimated", prefix_tokens, rewind_only_limit))
}

pub(crate) fn context_rewind_anchor_position_recovery_eligible(
    anchor: &ContextRewindAnchorCatalogEntry,
    position: external_agent::RollbackAnchorPosition,
    latest_outcome: Option<ContextRewindBackendUsageAtLine>,
) -> Option<bool> {
    let (_, used_tokens, rewind_only_limit) =
        context_rewind_anchor_restore_usage_for_headroom(anchor, position)?;
    if !context_rewind_anchor_has_recovery_headroom(used_tokens, rewind_only_limit) {
        return Some(false);
    }
    Some(latest_outcome.is_none_or(|outcome| {
        context_rewind_anchor_has_recovery_headroom(outcome.used_tokens, outcome.rewind_only_limit)
    }))
}

pub(crate) fn managed_context_density_recommended_limit(rewind_only_limit: u64) -> u64 {
    (rewind_only_limit as f64 * MANAGED_CONTEXT_DENSITY_THRESHOLD_PCT / 100.0).floor() as u64
}

pub(crate) fn context_rewind_anchor_has_density_headroom(used_tokens: u64, rewind_only_limit: u64) -> bool {
    used_tokens < managed_context_density_recommended_limit(rewind_only_limit)
}

pub(crate) fn context_rewind_anchor_position_density_eligible(
    anchor: &ContextRewindAnchorCatalogEntry,
    position: external_agent::RollbackAnchorPosition,
    latest_outcome: Option<ContextRewindBackendUsageAtLine>,
) -> Option<bool> {
    let (_, used_tokens, rewind_only_limit) =
        context_rewind_anchor_restore_usage_for_headroom(anchor, position)?;
    if !context_rewind_anchor_has_density_headroom(used_tokens, rewind_only_limit) {
        return Some(false);
    }
    Some(latest_outcome.is_none_or(|outcome| {
        context_rewind_anchor_has_density_headroom(outcome.used_tokens, outcome.rewind_only_limit)
    }))
}

pub(crate) fn validate_context_rewind_anchor_restore_headroom(
    source_rollout_path: &Path,
    requested_item_id: &str,
    position: external_agent::RollbackAnchorPosition,
) -> Result<(), String> {
    let requested_item_id = requested_item_id.trim();
    let Some(anchor) = find_context_rewind_anchor_entry(source_rollout_path, requested_item_id)
        .map_err(|err| {
            format!(
                "failed to inspect rollout anchors in {}: {err}",
                source_rollout_path.display()
            )
        })?
    else {
        return Ok(());
    };

    if let Some(start_line) = anchor.managed_context_recovery_start_line {
        return Err(format!(
            "rewind anchor item_id `{requested_item_id}` is inside the active managed-context recovery span that starts at rollout line {start_line}; choosing it would preserve the recovery kickstart prompt or its list/inspect calls. Call list_rewind_anchors again without include_non_recovery and choose an anchor before that managed-context recovery span."
        ));
    }

    let Some((usage_kind, used_tokens, rewind_only_limit)) =
        context_rewind_anchor_restore_usage_for_headroom(&anchor, position)
    else {
        return Ok(());
    };
    if context_rewind_anchor_has_recovery_headroom(used_tokens, rewind_only_limit) {
        // Prior-outcome veto is anchor-level (managed.md: an anchor that
        // already proved insufficient is not re-offered), so a failed rewind
        // at either position blocks both positions of the same anchor.
        for outcome_position in [
            position,
            match position {
                external_agent::RollbackAnchorPosition::Before => {
                    external_agent::RollbackAnchorPosition::After
                }
                external_agent::RollbackAnchorPosition::After => {
                    external_agent::RollbackAnchorPosition::Before
                }
            },
        ] {
            let Some(outcome) = latest_context_rewind_outcome_for_anchor(
                source_rollout_path,
                requested_item_id,
                outcome_position,
            )
            .map_err(|err| {
                format!(
                    "failed to inspect prior rewind outcomes in {}: {err}",
                    source_rollout_path.display()
                )
            })?
            else {
                continue;
            };
            if context_rewind_anchor_has_recovery_headroom(
                outcome.used_tokens,
                outcome.rewind_only_limit,
            ) {
                continue;
            }
            return Err(format!(
                "rewind anchor item_id `{requested_item_id}` is not a valid recovery target for position `{}`: a prior rewind to {} item was followed by {} tokens against the {} token rewind-only limit. Rows returned only with include_non_recovery=true are diagnostic and must not be passed to rewind_context. Call list_rewind_anchors again without include_non_recovery and choose an anchor whose positions includes the requested position; audit rows may expose recovery_eligible_positions.",
                position.as_str(),
                outcome_position.as_str(),
                outcome.used_tokens,
                outcome.rewind_only_limit
            ));
        }
        return Ok(());
    }

    Err(format!(
        "rewind anchor item_id `{requested_item_id}` is not a valid recovery target for position `{}`: restoring {} item would keep {} {} tokens before the injected primer, leaving less than {} normal-tool headroom under the {} token rewind-only limit. Rows returned only with include_non_recovery=true are diagnostic and must not be passed to rewind_context. Call list_rewind_anchors again without include_non_recovery and choose an anchor whose positions includes the requested position; audit rows may expose recovery_eligible_positions.",
        position.as_str(),
        position.as_str(),
        usage_kind,
        used_tokens,
        CONTEXT_REWIND_RECOVERY_MIN_RESUME_HEADROOM_TOKENS,
        rewind_only_limit
    ))
}

pub(crate) fn validate_context_rewind_anchor_density_improvement(
    source_rollout_path: &Path,
    requested_item_id: &str,
    position: external_agent::RollbackAnchorPosition,
) -> Result<(), String> {
    let requested_item_id = requested_item_id.trim();
    let Some(anchor) = find_context_rewind_anchor_entry(source_rollout_path, requested_item_id)
        .map_err(|err| {
            format!(
                "failed to inspect rollout anchors in {}: {err}",
                source_rollout_path.display()
            )
        })?
    else {
        return Ok(());
    };

    let Some((usage_kind, used_tokens, rewind_only_limit)) =
        context_rewind_anchor_restore_usage_for_headroom(&anchor, position)
    else {
        return Err(format!(
            "density handoff rewind anchor item_id `{requested_item_id}` cannot be validated for material density improvement at position `{}` because the rollout has no backend or prefix usage estimate for that position. Call list_rewind_anchors with density_candidates_only=true and include_pruning_estimates=true, choose an exact returned anchor/position, or reply with a concise no-rewind handoff and stop.",
            position.as_str()
        ));
    };
    let recommended_rewind_limit = managed_context_density_recommended_limit(rewind_only_limit);
    if context_rewind_anchor_has_density_headroom(used_tokens, rewind_only_limit) {
        if let Some(outcome) = latest_context_rewind_outcome_for_anchor(
            source_rollout_path,
            requested_item_id,
            position,
        )
        .map_err(|err| {
            format!(
                "failed to inspect prior rewind outcomes in {}: {err}",
                source_rollout_path.display()
            )
        })? {
            if context_rewind_anchor_has_density_headroom(
                outcome.used_tokens,
                outcome.rewind_only_limit,
            ) {
                return Ok(());
            }
            return Err(format!(
                "density handoff rewind anchor item_id `{requested_item_id}` is too shallow for position `{}`: a prior rewind to that exact item was followed by {} tokens, still at or above the recommended density threshold {} for the {} token context. Choose an exact item_id/position from list_rewind_anchors with density_candidates_only=true, or reply with a concise no-rewind handoff and stop.",
                position.as_str(),
                outcome.used_tokens,
                managed_context_density_recommended_limit(outcome.rewind_only_limit),
                outcome.rewind_only_limit,
            ));
        }
        return Ok(());
    }

    Err(format!(
        "density handoff rewind anchor item_id `{requested_item_id}` is too shallow for position `{}`: restoring {} item would keep {} tokens, still at or above the recommended density threshold {} for the {} token context. Choose an exact item_id/position from list_rewind_anchors with density_candidates_only=true, or reply with a concise no-rewind handoff and stop.",
        position.as_str(),
        usage_kind,
        used_tokens,
        recommended_rewind_limit,
        rewind_only_limit
    ))
}

pub(crate) fn context_rewind_thread_id_candidates(
    session_id: Option<&str>,
    alias_session_id: Option<&str>,
) -> Vec<String> {
    let mut ids = Vec::new();
    for id in [session_id, alias_session_id]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

pub(crate) fn active_context_rewind_thread_ids(config: &DrainConfig<'_>) -> Vec<String> {
    context_rewind_thread_id_candidates(
        config.session_id.as_deref(),
        config.alias_session_id.as_deref(),
    )
}

pub(crate) async fn validate_context_rewind_request_before_schedule(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread_ids: &[String],
    request: &ExternalContextRewindRequest,
) -> Result<(), String> {
    if !agent.supports_item_anchor_rewind() {
        return Err(format!(
            "{} does not support item-anchor rewind",
            agent.name()
        ));
    }
    if thread_ids.is_empty() {
        return Err(
            "cannot validate context rewind: active Codex thread id is unknown".to_string(),
        );
    }

    let mut metadata_errors = Vec::new();
    for thread_id in thread_ids {
        match agent.read_thread_snapshot(thread_id).await {
            Ok(snapshot) => {
                let source_rollout_path = snapshot.rollout_path.ok_or_else(|| {
                    format!("thread metadata for {thread_id} did not include a rollout path")
                })?;
                resolve_context_rewind_anchor(&source_rollout_path, &request.item_id)?;
                validate_context_rewind_anchor_restore_headroom(
                    &source_rollout_path,
                    &request.item_id,
                    request.position,
                )?;
                if request.require_density_improvement {
                    validate_context_rewind_anchor_density_improvement(
                        &source_rollout_path,
                        &request.item_id,
                        request.position,
                    )?;
                }
                return Ok(());
            }
            Err(e) => metadata_errors.push(format!("{thread_id}: {e}")),
        }
    }

    Err(format!(
        "failed to read thread metadata before rewind for active thread candidates [{}]: {}",
        thread_ids.join(", "),
        metadata_errors.join("; ")
    ))
}

pub(crate) fn find_context_rewind_anchor_entry(
    source_rollout_path: &Path,
    requested_item_id: &str,
) -> io::Result<Option<ContextRewindAnchorCatalogEntry>> {
    Ok(scan_context_rewind_anchor_catalog(source_rollout_path)?
        .into_iter()
        .find(|anchor| anchor.item_id == requested_item_id))
}

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

pub(crate) async fn apply_external_context_rewind(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread_id: &str,
    request: &ExternalContextRewindRequest,
    config: &DrainConfig<'_>,
) -> Result<Option<FollowUpMessage>, String> {
    if !agent.supports_item_anchor_rewind() {
        return Err(format!(
            "{} does not support item-anchor rewind",
            agent.name()
        ));
    }

    let record_id = format!("rewind-{}", uuid::Uuid::new_v4().simple());
    let snapshot = agent
        .read_thread_snapshot(thread_id)
        .await
        .map_err(|e| format!("failed to read thread metadata before rewind: {}", e))?;
    let source_rollout_path = snapshot
        .rollout_path
        .clone()
        .ok_or_else(|| "thread metadata did not include a rollout path".to_string())?;
    let resolved_anchor = resolve_context_rewind_anchor(&source_rollout_path, &request.item_id)?;
    validate_context_rewind_anchor_restore_headroom(
        &source_rollout_path,
        &resolved_anchor.item_id,
        request.position,
    )?;
    if request.require_density_improvement {
        validate_context_rewind_anchor_density_improvement(
            &source_rollout_path,
            &resolved_anchor.item_id,
            request.position,
        )?;
    }
    let carried_forward_prior_facts = match context_rewind_pruned_prior_primer_facts(
        &source_rollout_path,
        &resolved_anchor.item_id,
        request.position,
        request,
    ) {
        Ok(facts) => facts,
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not inspect pruned prior managed-context primers before rewind {record_id}: {err}"
                ))
            });
            None
        }
    };
    // Fission detach prep, BEFORE the rollback mutates the rollout: snapshot
    // every anchor's first line plus the cut line of this rewind from the
    // pre-rewind catalog, so the post-rollback detach pass can decide which
    // fission spawn anchors were cut out of the effective history.
    let fission_detach_prep = match scan_context_rewind_anchor_catalog(&source_rollout_path) {
        Ok(anchors) => {
            fission_anchor_cut_line(&anchors, &resolved_anchor.item_id, request.position)
                .map(|cut_line| (fission_anchor_first_lines(&anchors), cut_line))
        }
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not snapshot rollout anchors for fission detach before rewind {record_id}: {err}"
                ))
            });
            None
        }
    };
    let recovery_rollout_path =
        context_rewind::copy_recovery_rollout(config.log_dir, &record_id, &source_rollout_path)
            .map_err(|e| format!("failed to copy pre-rewind rollout: {}", e))?;
    let fission_snapshot = match context_rewind::read_fission_snapshot(config.log_dir, thread_id) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not snapshot fission/session relationships before rewind: {err}"
                ))
            });
            None
        }
    };
    let lineage_ledger = match lineage_ledger::read_lineage_ledger(config.log_dir, thread_id) {
        Ok(ledger) => ledger,
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not snapshot lineage ledger before rewind: {err}"
                ))
            });
            None
        }
    };
    let fission_ledger =
        match fission_ledger::read_fission_ledger_for_session(config.log_dir, thread_id) {
            Ok(ledger) => ledger,
            Err(err) => {
                slog(config.session_log, |log| {
                    log.warn(&format!(
                        "Could not snapshot fission ledger before rewind: {err}"
                    ))
                });
                None
            }
        };
    // Freshest locally available usage at record creation, for offline
    // pressure-at-rewind analysis (no backend RPC): the pre-rewind rollout's
    // last `token_count` report — typically written moments before this
    // rewind by the turn that requested it — else the latest session-log
    // context snapshot, else `None`s.
    let (used_tokens_at_rewind, context_window_at_rewind, pressure_band_at_rewind) =
        context_rewind_pressure_at_record_creation(&source_rollout_path, config);

    let mut record = context_rewind::ContextRewindRecord {
        record_id: record_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        session_id: request
            .session_id
            .clone()
            .or_else(|| config.session_id.clone()),
        thread_id: snapshot.thread_id,
        item_id: resolved_anchor.item_id.clone(),
        position: request.position.as_str().to_string(),
        reason: request.reason.clone(),
        primer: request.primer.clone(),
        preserve: request.preserve.clone(),
        discard: request.discard.clone(),
        artifacts: request.artifacts.clone(),
        next_steps: request.next_steps.clone(),
        source_rollout_path: Some(source_rollout_path),
        recovery_rollout_path: Some(recovery_rollout_path),
        fission_snapshot,
        lineage_ledger,
        fission_ledger,
        detached_fission_group_ids: Vec::new(),
        used_tokens_at_rewind,
        context_window_at_rewind,
        pressure_band_at_rewind,
        surgical: request.surgical,
    };
    // Perform the rollback BEFORE persisting the durable record. The recovery
    // rollout was copied above (copy-before-mutation), but the record itself is
    // only written once the rollback succeeds, so an invalid/stale anchor (which
    // the backend rejects as a normal tool error) never leaves a success-looking
    // orphan record on disk. On failure, delete the orphaned recovery-rollout copy.
    if let Err(e) = agent
        .rollback_thread_to_item_anchor(thread_id, &resolved_anchor.item_id, request.position)
        .await
    {
        if let Err(cleanup) = context_rewind::remove_recovery_rollout(config.log_dir, &record_id) {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Failed to clean up recovery rollout after failed rewind {record_id}: {cleanup}"
                ))
            });
        }
        return Err(format!("thread rollback failed: {}", e));
    }

    // The rollback succeeded: sever every fission group whose spawn anchor
    // was cut out of the effective history, BEFORE the durable record is
    // written so the record carries the detached group ids. Skipped (with a
    // warning above) when the pre-rewind anchor snapshot could not be taken —
    // without it the predicate would wrongly report every anchor unreachable.
    if let Some((anchor_first_lines, cut_line)) = fission_detach_prep {
        let detach_parent_candidates = fission_detach_parent_candidates(thread_id, &record, config);
        match fission_ledger::detach_groups_with_invalid_anchors(
            config.log_dir,
            &detach_parent_candidates,
            |anchor_item_id| {
                fission_anchor_reachable_after_rewind(
                    &anchor_first_lines,
                    cut_line,
                    request.position,
                    anchor_item_id,
                )
            },
        ) {
            Ok(report) => {
                if !report.detached_group_ids.is_empty() {
                    emit_fission_detach_relationships(config, &report);
                    fission_lifecycle::drop_pending_deliveries(&report.detached_group_ids);
                    slog(config.session_log, |log| {
                        log.info(&format!(
                            "Rewind {record_id} detached fission group(s) [{}]",
                            report.detached_group_ids.join(", ")
                        ))
                    });
                    record.detached_fission_group_ids = report.detached_group_ids;
                }
            }
            Err(err) => slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not detach fission groups after rewind {record_id}: {err}"
                ))
            }),
        }
    }

    context_rewind::persist_record(config.log_dir, &record)
        .map_err(|e| format!("failed to persist context rewind record: {}", e))?;

    if let Some(primer) = request.rendered_primer(
        Some(record_id.as_str()),
        carried_forward_prior_facts.as_deref(),
    ) {
        agent
            .inject_thread_developer_message(thread_id, &primer)
            .await
            .map_err(|e| format!("failed to inject context rewind primer: {}", e))?;
    }

    let message = if request.primer.is_some() {
        format!(
            "context rewound to {}; primer injected; record {}",
            resolved_anchor.target_label(request.position),
            record_id
        )
    } else {
        format!(
            "rewound to {}; record {}",
            resolved_anchor.target_label(request.position),
            record_id
        )
    };
    slog(config.session_log, |l| l.info(&message));
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: request
            .session_id
            .clone()
            .or_else(|| config.session_id.clone()),
        action: "rewind_context".to_string(),
        success: true,
        message,
        record_id: Some(record_id),
    });
    if let Err(e) = refresh_external_context_usage_snapshot(agent, config).await {
        slog(config.session_log, |l| {
            l.debug(&format!(
                "Could not refresh context usage after successful rewind: {}",
                e
            ))
        });
    }

    Ok(request.resume_followup())
}

pub(crate) fn emit_context_rewind_failure(
    request: &ExternalContextRewindRequest,
    message: String,
    config: &DrainConfig<'_>,
) {
    slog(config.session_log, |l| {
        l.warn(&format!("Context rewind failed: {message}"))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: request
            .session_id
            .clone()
            .or_else(|| config.session_id.clone()),
        action: "rewind_context".to_string(),
        success: false,
        message,
        record_id: None,
    });
}

pub(crate) struct ExternalContextRewindResume<'a, 'b> {
    pub(crate) event_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    pub(crate) turn_bus_rx: &'a mut tokio::sync::broadcast::Receiver<AppEvent>,
    pub(crate) config: &'a DrainConfig<'b>,
    pub(crate) stats: &'a mut LoopStats,
    pub(crate) diff_tracker: &'a mut ExternalDiffDeltaTracker,
    pub(crate) pending_runtime_steers: &'a mut std::collections::VecDeque<PendingRuntimeSteer>,
    pub(crate) handled_steer_ids: &'a mut std::collections::HashSet<String>,
    pub(crate) cancelled_follow_ups: &'a mut HashSet<String>,
    pub(crate) codex_thread_action_dedupe: &'a mut CodexThreadActionDedupe,
    pub(crate) side_sessions: Option<&'a mut ExternalSideSessionState<'b>>,
}

pub(crate) const MAX_CHAINED_CONTEXT_REWIND_RESUMES: usize = 8;

pub(crate) async fn send_external_context_rewind_resume_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread: &external_agent::AgentThread,
    followup: FollowUpMessage,
    resume: &mut ExternalContextRewindResume<'_, '_>,
) -> Result<DrainOutcome, String> {
    agent
        .send_message(thread, &followup.text)
        .await
        .map_err(|e| format!("failed to start resumed context-rewind turn: {}", e))?;
    Ok(drain_external_agent_events(
        agent,
        resume.event_rx,
        resume.turn_bus_rx,
        resume.config,
        resume.stats,
        resume.diff_tracker,
        resume.pending_runtime_steers,
        resume.handled_steer_ids,
        resume.cancelled_follow_ups,
        resume.codex_thread_action_dedupe,
        resume.side_sessions.as_deref_mut(),
        followup.managed_context_recovery_kickstart,
        followup.managed_context_density_handoff,
        followup.managed_context_density_handoff_completed,
    )
    .await)
}

pub(crate) fn emit_context_rewind_resume_round_complete(
    resume: &mut ExternalContextRewindResume<'_, '_>,
    message: Option<String>,
    turns_in_round: usize,
) {
    resume.stats.turns += 1;
    resume.stats.rounds += 1;
    resume.config.bus.send(AppEvent::DoneSignal {
        session_id: resume.config.session_id.clone(),
        message,
    });
    resume.config.bus.send(AppEvent::RoundComplete {
        session_id: resume.config.session_id.clone(),
        round: resume.stats.rounds,
        turns_in_round,
        native_message_count: None,
    });
}

pub(crate) async fn apply_chained_context_rewind_resume_turns(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread: &external_agent::AgentThread,
    initial_request: ExternalContextRewindRequest,
    resume: &mut ExternalContextRewindResume<'_, '_>,
) -> Result<Option<DrainOutcome>, (ExternalContextRewindRequest, String)> {
    let mut request = initial_request;
    for _ in 0..MAX_CHAINED_CONTEXT_REWIND_RESUMES {
        let followup =
            match apply_external_context_rewind(agent, &thread.thread_id, &request, resume.config)
                .await
            {
                Ok(followup) => followup,
                Err(message) => return Err((request, message)),
            };
        let Some(followup) = followup else {
            return Ok(None);
        };
        let outcome =
            match send_external_context_rewind_resume_turn(agent, thread, followup, resume).await {
                Ok(outcome) => outcome,
                Err(message) => return Err((request, message)),
            };
        match outcome {
            DrainOutcome::ContextRewindRequested {
                request: next_request,
                message,
                turns_in_round,
                ..
            } => {
                emit_context_rewind_resume_round_complete(resume, message, turns_in_round);
                request = next_request;
            }
            other => return Ok(Some(other)),
        }
    }
    Err((
        request,
        format!(
            "too many consecutive context rewinds in a single resumed turn chain (limit {})",
            MAX_CHAINED_CONTEXT_REWIND_RESUMES
        ),
    ))
}

pub(crate) struct ExternalSideSessionState<'a> {
    pub(crate) open_side_threads: &'a mut HashMap<String, String>,
    pub(crate) side_rounds: &'a mut HashMap<String, usize>,
    pub(crate) side_turn_revisions: &'a mut HashMap<String, UserTurnRevisionState>,
}

impl<'a> ExternalSideSessionState<'a> {
    pub(crate) fn has_side_thread(&self, thread_id: &str) -> bool {
        self.open_side_threads.contains_key(thread_id)
    }

    pub(crate) fn record_started(&mut self, parent_thread_id: String, child_thread_id: String) {
        self.open_side_threads
            .insert(child_thread_id.clone(), parent_thread_id);
        self.side_rounds.entry(child_thread_id.clone()).or_insert(1);
        self.side_turn_revisions
            .entry(child_thread_id)
            .or_insert_with(|| {
                let mut state = UserTurnRevisionState::default();
                state.record_next_turn();
                state
            });
    }

    pub(crate) fn record_closed(&mut self, child_thread_id: &str) {
        self.open_side_threads.remove(child_thread_id);
        self.side_rounds.remove(child_thread_id);
        self.side_turn_revisions.remove(child_thread_id);
    }
}

pub(crate) fn claim_active_side_turn_completion(
    active_side_turns: &mut HashSet<String>,
    session_id: Option<&str>,
) -> bool {
    session_id
        .map(|session_id| active_side_turns.remove(session_id))
        .unwrap_or(true)
}

pub(crate) async fn start_external_side_followup_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
    side_sessions: &mut Option<&mut ExternalSideSessionState<'_>>,
    active_side_turns: &mut HashSet<String>,
    session_id: String,
    text: String,
    attachments: UserAttachments,
    follow_up_id: Option<String>,
    steer_id: Option<String>,
) -> bool {
    let side_turn = if let Some(state) = side_sessions.as_deref_mut() {
        if state.has_side_thread(&session_id) {
            let side_round = state.side_rounds.entry(session_id.clone()).or_insert(0);
            *side_round += 1;
            let user_turn_revision = state
                .side_turn_revisions
                .entry(session_id.clone())
                .or_default()
                .record_active_turn(*side_round as u32);
            Some((*side_round, user_turn_revision))
        } else {
            None
        }
    } else {
        None
    };
    let Some((side_round, user_turn_revision)) = side_turn else {
        return false;
    };

    emit_user_message_log(
        config.bus,
        config.session_log,
        Some(&session_id),
        Some(side_round as u32),
        Some(user_turn_revision),
        None,
        &text,
    );
    let merged = drain_steer_queue_as_followup(
        config.context_injection,
        &text,
        config.bus,
        Some(&session_id),
        None,
    )
    .unwrap_or_else(|| text.clone());
    let side_thread = external_agent::AgentThread {
        thread_id: session_id.clone(),
    };
    emit_external_turn_status(
        config.bus,
        &config.autonomy,
        Some(&session_id),
        side_round,
        "thinking",
        format!("{} side turn in progress", agent.name()),
    )
    .await;
    let send_result = if attachments.is_empty() {
        agent.send_message(&side_thread, &merged).await
    } else {
        agent
            .send_message_with_attachments(&side_thread, &merged, &attachments.items)
            .await
    };
    if let Err(e) = send_result {
        emit_follow_up_status(
            config.bus,
            Some(&session_id),
            &follow_up_id,
            Some(&text),
            "failed",
            Some("failed to send side follow-up"),
        );
        config.bus.send(AppEvent::LoopError(format!(
            "Failed to send side follow-up: {}",
            e
        )));
        return true;
    }
    emit_follow_up_status(
        config.bus,
        Some(&session_id),
        &follow_up_id,
        Some(&text),
        "delivered",
        None,
    );
    if let Some(id) = steer_id {
        config.bus.send(AppEvent::SteerDelivered {
            session_id: Some(session_id.clone()),
            id,
            mid_turn: false,
        });
    }
    active_side_turns.insert(session_id);
    true
}

pub(crate) async fn start_external_primary_steer_followup_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
    session_id: String,
    text: String,
    steer_id: String,
    reason: String,
) -> Result<(), CallerError> {
    let thread = external_agent::AgentThread {
        thread_id: session_id.clone(),
    };
    let send_result = agent.send_message(&thread, &text).await;
    match send_result {
        Ok(()) => {
            emit_user_message_log(
                config.bus,
                config.session_log,
                Some(&session_id),
                None,
                None,
                None,
                &text,
            );
            slog(config.session_log, |l| l.info(&reason));
            config.bus.send(AppEvent::SteerQueued {
                session_id: Some(session_id.clone()),
                id: steer_id.clone(),
                reason,
            });
            config.bus.send(AppEvent::SteerDelivered {
                session_id: Some(session_id),
                id: steer_id,
                mid_turn: false,
            });
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn scoped_event_targets_config(
    thread_id: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
) -> bool {
    match thread_id {
        Some(thread_id) => {
            session_id.as_deref() == Some(thread_id.as_str())
                || alias_session_id.as_deref() == Some(thread_id.as_str())
        }
        None => true,
    }
}

pub(crate) fn emit_child_turn_complete(
    config: &DrainConfig<'_>,
    conversation_kind: &str,
    message: Option<String>,
) {
    emit_child_turn_complete_for_session(
        config.bus,
        config.session_id.clone(),
        conversation_kind,
        message,
    );
}

pub(crate) fn emit_child_turn_complete_for_session(
    bus: &EventBus,
    session_id: Option<String>,
    conversation_kind: &str,
    message: Option<String>,
) {
    if let Some(message) = message {
        bus.send(AppEvent::LogEntry {
            session_id: session_id.clone(),
            level: "info".to_string(),
            source: "Codex".to_string(),
            content: message,
            turn: None,
        });
    }
    bus.send(AppEvent::LogEntry {
        session_id,
        level: "info".to_string(),
        source: "Codex".to_string(),
        content: format!(
            "Round complete: {} conversation ready for follow-up",
            conversation_kind
        ),
        turn: None,
    });
}

pub(crate) fn external_context_snapshot_key(snapshot: &external_agent::AgentContextSnapshot) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    if snapshot.request_id.is_some() {
        snapshot.source.hash(&mut h);
        snapshot.request_id.hash(&mut h);
        snapshot.request_index.hash(&mut h);
        return h.finish();
    }
    snapshot.source.hash(&mut h);
    snapshot.label.hash(&mut h);
    snapshot.format.hash(&mut h);
    snapshot.token_count.hash(&mut h);
    snapshot.token_count_kind.hash(&mut h);
    snapshot.context_window.hash(&mut h);
    snapshot.hard_context_window.hash(&mut h);
    snapshot.item_count.hash(&mut h);
    match serde_json::to_vec(&snapshot.raw) {
        Ok(bytes) => bytes.hash(&mut h),
        Err(_) => snapshot.raw.to_string().hash(&mut h),
    }
    h.finish()
}

pub(crate) fn external_context_snapshot_turn(stats: &LoopStats) -> Option<usize> {
    if stats.turns > 0 {
        Some(stats.turns)
    } else {
        None
    }
}

pub(crate) fn external_context_snapshot_usage(
    snapshot: &external_agent::AgentContextSnapshot,
) -> Option<frontend::ModelUsageSnapshot> {
    let tokens_used = external_context_snapshot_backend_token_count(snapshot)?;
    let context_window = snapshot.context_window?;
    if context_window == 0 {
        return None;
    }

    let provider = if snapshot.format.starts_with("openai.") {
        "openai"
    } else if snapshot.format.starts_with("anthropic.") {
        "anthropic"
    } else if snapshot.format.starts_with("gemini.") {
        "gemini"
    } else {
        snapshot.source.as_str()
    };
    let model = snapshot
        .raw
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(snapshot.source.as_str());

    Some(frontend::ModelUsageSnapshot {
        provider: provider.to_string(),
        model: model.to_string(),
        tokens_used,
        context_window,
        hard_context_window: snapshot.hard_context_window,
        usage_pct: tokens_used as f64 / context_window as f64 * 100.0,
        prompt_tokens: tokens_used,
        ..Default::default()
    })
}

pub(crate) fn external_context_snapshot_backend_token_count(
    snapshot: &external_agent::AgentContextSnapshot,
) -> Option<u64> {
    (snapshot.token_count_kind == Some(external_agent::AgentContextTokenCountKind::BackendReported))
        .then_some(snapshot.token_count)
        .flatten()
}

pub(crate) fn emit_external_context_usage_snapshot(
    config: &DrainConfig<'_>,
    snapshot: &external_agent::AgentContextSnapshot,
) -> bool {
    let Some(main) = external_context_snapshot_usage(snapshot) else {
        return false;
    };
    emit_external_context_usage_snapshot_from_usage(config, main);
    true
}

pub(crate) fn emit_external_context_usage_snapshot_from_usage(
    config: &DrainConfig<'_>,
    main: frontend::ModelUsageSnapshot,
) {
    config.bus.send(AppEvent::UsageSnapshot {
        session_id: config.session_id.clone(),
        main,
        presence: None,
    });
}

pub(crate) async fn refresh_external_context_usage_snapshot(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
) -> Result<Option<external_agent::AgentContextSnapshot>, CallerError> {
    let snapshot = agent.context_snapshot().await?;
    if let Some(snapshot) = snapshot.as_ref() {
        emit_external_context_usage_snapshot(config, snapshot);
    }
    Ok(snapshot)
}

pub(crate) fn latest_external_context_snapshot_from_log(
    config: &DrainConfig<'_>,
) -> Option<external_agent::AgentContextSnapshot> {
    let session_path = config.log_dir.join("session.jsonl");
    let contents = std::fs::read_to_string(session_path).ok()?;
    let session_id = config.session_id.as_deref();
    let alias_session_id = config.alias_session_id.as_deref();
    let mut latest = None;

    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some(AppEvent::ContextSnapshot {
            session_id: snapshot_session_id,
            source,
            label,
            request_id,
            request_index,
            format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            item_count,
            raw,
            ..
        }) = session_log::session_log_entry_to_app_event(&entry, config.log_dir)
        else {
            continue;
        };
        let targets_session = match snapshot_session_id.as_deref() {
            Some(id) => session_id == Some(id) || alias_session_id == Some(id),
            None => true,
        };
        if !targets_session {
            continue;
        }
        let token_count_kind = match token_count_kind.as_deref() {
            Some("backend_reported") => {
                Some(external_agent::AgentContextTokenCountKind::BackendReported)
            }
            Some("local_estimate") => {
                Some(external_agent::AgentContextTokenCountKind::LocalEstimate)
            }
            _ => None,
        };
        latest = Some(external_agent::AgentContextSnapshot {
            source,
            label,
            request_id,
            request_index,
            rollout_path: None,
            format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            item_count,
            raw,
        });
    }

    latest
}

pub(crate) async fn refresh_external_context_usage_snapshot_for_preflight(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
) -> Result<Option<external_agent::AgentContextSnapshot>, CallerError> {
    match refresh_external_context_usage_snapshot(agent, config).await? {
        Some(snapshot) => Ok(Some(snapshot)),
        None => {
            let snapshot = latest_external_context_snapshot_from_log(config);
            if let Some(snapshot) = snapshot.as_ref() {
                emit_external_context_usage_snapshot(config, snapshot);
                slog(config.session_log, |l| {
                    l.debug(
                        "Using latest session-log Codex context snapshot for managed-context preflight",
                    )
                });
            }
            Ok(snapshot)
        }
    }
}

pub(crate) fn managed_context_rewind_only_pressure(
    snapshot: &external_agent::AgentContextSnapshot,
) -> Option<ManagedContextRewindOnlyPressure> {
    managed_context_recovery_pressure(snapshot)
}

pub(crate) const MANAGED_CONTEXT_DENSITY_THRESHOLD_PCT: f64 = 85.0;

pub(crate) fn managed_context_density_pressure(
    snapshot: &external_agent::AgentContextSnapshot,
) -> Option<ManagedContextDensityPressure> {
    let used_tokens = external_context_snapshot_backend_token_count(snapshot)?;
    let rewind_only_limit = snapshot.context_window?;
    if rewind_only_limit == 0 || used_tokens >= rewind_only_limit {
        return None;
    }
    let recommended_rewind_limit = managed_context_density_recommended_limit(rewind_only_limit);
    if used_tokens < recommended_rewind_limit {
        return None;
    }
    Some(ManagedContextDensityPressure {
        used_tokens,
        recommended_rewind_limit,
        rewind_only_limit,
        hard_context_window: snapshot.hard_context_window,
    })
}

pub(crate) fn managed_context_rewind_only_pressure_from_usage(
    usage: &external_agent::AgentUsageSnapshot,
) -> Option<ManagedContextRewindOnlyPressure> {
    let rewind_only_limit = usage.context_window;
    if rewind_only_limit == 0 || usage.tokens_used < rewind_only_limit {
        return None;
    }
    let status = if usage
        .hard_context_window
        .is_some_and(|hard| hard > 0 && usage.tokens_used >= hard)
    {
        "critical"
    } else {
        "high"
    };
    Some(ManagedContextRewindOnlyPressure {
        used_tokens: usage.tokens_used,
        rewind_only_limit,
        hard_context_window: usage.hard_context_window,
        status,
    })
}

pub(crate) fn managed_context_preflight_rewind_only_gate_enabled(
    codex_managed_context_enabled: bool,
    managed_context_recovery_kickstart: bool,
    managed_context_density_handoff: bool,
) -> bool {
    codex_managed_context_enabled
        && !managed_context_recovery_kickstart
        && !managed_context_density_handoff
}

pub(crate) fn managed_context_preflight_density_gate_enabled(
    managed_context_rewind_only_gate_enabled: bool,
    managed_context_density_handoff_completed: bool,
) -> bool {
    managed_context_rewind_only_gate_enabled && !managed_context_density_handoff_completed
}

pub(crate) fn managed_context_post_turn_density_handoff_enabled(
    managed_context_recovery_kickstart: bool,
    managed_context_density_handoff: bool,
    managed_context_density_handoff_completed: bool,
) -> bool {
    !managed_context_recovery_kickstart
        && !managed_context_density_handoff
        && !managed_context_density_handoff_completed
}

#[derive(Debug, Clone)]
pub(crate) enum ManagedContextPreflightDecision {
    Recovery {
        recovery_followup: FollowUpMessage,
        held_followup: Option<FollowUpMessage>,
        pressure: ManagedContextRewindOnlyPressure,
    },
    DensityHandoff {
        handoff_followup: FollowUpMessage,
        held_followup: FollowUpMessage,
        pressure: ManagedContextDensityPressure,
    },
}

pub(crate) fn managed_context_followup_for_replay(followup: &FollowUpMessage) -> FollowUpMessage {
    let mut replay = followup.clone();
    replay.managed_context_recovery_kickstart = false;
    replay.managed_context_density_handoff = false;
    replay.managed_context_density_handoff_completed = false;
    replay
}

pub(crate) fn managed_context_preflight_decision(
    codex_managed_context_enabled: bool,
    followup: &FollowUpMessage,
    snapshot: &external_agent::AgentContextSnapshot,
) -> Option<ManagedContextPreflightDecision> {
    let rewind_only_gate_enabled = managed_context_preflight_rewind_only_gate_enabled(
        codex_managed_context_enabled,
        followup.managed_context_recovery_kickstart,
        followup.managed_context_density_handoff,
    );
    if !rewind_only_gate_enabled {
        return None;
    }

    if let Some(pressure) = managed_context_rewind_only_pressure(snapshot) {
        let drop_original = managed_context_drop_original_for_recovery(
            &followup.text,
            !followup.attachments.is_empty(),
            followup.steer_id.is_some(),
            followup.edit_user_turn_index.is_some(),
        );
        let held_followup = (!drop_original).then(|| managed_context_followup_for_replay(followup));
        let mut recovery_followup = FollowUpMessage::text(managed_context_recovery_kickstart_text(
            pressure,
            held_followup.is_some(),
        ))
        .managed_context_recovery_kickstart();
        if held_followup.is_none() {
            recovery_followup = recovery_followup.with_follow_up_id(followup.follow_up_id.clone());
        }
        return Some(ManagedContextPreflightDecision::Recovery {
            recovery_followup,
            held_followup,
            pressure,
        });
    }

    if managed_context_preflight_density_gate_enabled(
        rewind_only_gate_enabled,
        followup.managed_context_density_handoff_completed,
    ) {
        if let Some(pressure) = managed_context_density_pressure(snapshot) {
            return Some(ManagedContextPreflightDecision::DensityHandoff {
                handoff_followup: FollowUpMessage::text(managed_context_density_handoff_text(
                    pressure,
                ))
                .managed_context_density_handoff(),
                held_followup: managed_context_followup_for_replay(followup),
                pressure,
            });
        }
    }

    None
}

pub(crate) fn managed_context_density_pressure_from_usage(
    usage: &external_agent::AgentUsageSnapshot,
) -> Option<ManagedContextDensityPressure> {
    let rewind_only_limit = usage.context_window;
    if rewind_only_limit == 0 || usage.tokens_used >= rewind_only_limit {
        return None;
    }
    let recommended_rewind_limit = managed_context_density_recommended_limit(rewind_only_limit);
    if usage.tokens_used < recommended_rewind_limit {
        return None;
    }
    Some(ManagedContextDensityPressure {
        used_tokens: usage.tokens_used,
        recommended_rewind_limit,
        rewind_only_limit,
        hard_context_window: usage.hard_context_window,
    })
}

pub(crate) fn managed_context_rewind_only_tool_allowed(tool_name: &str, preview: &str) -> bool {
    fn allowed_name(name: &str) -> bool {
        matches!(
            name.trim(),
            "get_status"
                | "get_logs"
                | "get_pending_approval"
                | "get_pending_input"
                | "get_restart_status"
                | "get_controller_loop_status"
                | "list_rewind_anchors"
                | "inspect_rewind_anchor"
                | "rewind_context"
                | "rewind_backout"
        )
    }

    if allowed_name(tool_name) {
        return true;
    }
    if tool_name != "mcp" {
        return false;
    }
    let preview = preview.trim();
    allowed_name(preview)
        || preview
            .rsplit_once(':')
            .is_some_and(|(_, name)| allowed_name(name))
}

/// Tools allowed to start while the managed-context density steer is active
/// (watch band: at or above the recommended density threshold, below the
/// rewind-only limit). Everything the rewind-only gate allows, plus the
/// fission tools: spawning a branch at watch pressure is itself a density
/// action — the work and its context noise land in the branch while the
/// parent only carries the spawn call and an eventual import. Under
/// rewind-only pressure the stricter
/// [`managed_context_rewind_only_tool_allowed`] gate applies instead and
/// fission stays blocked: the parent must shrink first.
pub(crate) fn managed_context_density_tool_allowed(tool_name: &str, preview: &str) -> bool {
    fn fission_name(name: &str) -> bool {
        matches!(
            name.trim(),
            "fission_spawn" | "fission_control" | "claim_fission_canonical"
        )
    }

    if managed_context_rewind_only_tool_allowed(tool_name, preview) || fission_name(tool_name) {
        return true;
    }
    if tool_name != "mcp" {
        return false;
    }
    let preview = preview.trim();
    fission_name(preview)
        || preview
            .rsplit_once(':')
            .is_some_and(|(_, name)| fission_name(name))
}

pub(crate) fn shellish_command_tokens(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| {
                    matches!(
                        c,
                        '"' | '\'' | '`' | '(' | ')' | '{' | '}' | '[' | ']' | ';' | ','
                    )
                })
                .to_string()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

pub(crate) fn shell_token_basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

pub(crate) fn shell_token_is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

pub(crate) fn shell_command_starts_with_non_execution_reader(tokens: &[String]) -> bool {
    let Some(first) = tokens
        .iter()
        .find(|token| !shell_token_is_assignment(token))
        .map(|token| shell_token_basename(token).to_ascii_lowercase())
    else {
        return false;
    };
    matches!(
        first.as_str(),
        "rg" | "grep"
            | "sed"
            | "cat"
            | "printf"
            | "echo"
            | "awk"
            | "jq"
            | "find"
            | "ls"
            | "ps"
            | "pgrep"
            | "pkill"
            | "kill"
            | "python"
            | "python3"
            | "node"
            | "perl"
    )
}

pub(crate) fn shell_token_is_intendant_binary(token: &str) -> bool {
    matches!(shell_token_basename(token), "intendant" | "intendant.exe")
}

pub(crate) fn shell_token_is_web_flag(token: &str) -> bool {
    token == "--web" || token.starts_with("--web=")
}

pub(crate) fn shell_command_invokes_intendant_web(tokens: &[String]) -> bool {
    if shell_command_starts_with_non_execution_reader(tokens) {
        return false;
    }
    tokens
        .iter()
        .any(|token| shell_token_is_intendant_binary(token))
        && tokens.iter().any(|token| shell_token_is_web_flag(token))
}

pub(crate) fn shell_command_has_background_operator(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    for (idx, ch) in chars.iter().enumerate() {
        if *ch != '&' {
            continue;
        }
        let prev = idx.checked_sub(1).and_then(|i| chars.get(i)).copied();
        let next = chars.get(idx + 1).copied();
        if matches!(prev, Some('&' | '>' | '<')) || matches!(next, Some('&' | '>')) {
            continue;
        }
        return true;
    }
    false
}

pub(crate) fn shell_command_has_explicit_dashboard_cleanup(command: &str, tokens: &[String]) -> bool {
    if !shell_command_has_background_operator(command) {
        return false;
    }
    let has_trap = tokens.iter().any(|token| token == "trap");
    let has_kill = tokens
        .iter()
        .any(|token| matches!(shell_token_basename(token), "kill" | "killall"));
    let references_background_pid = command.contains("$!")
        || tokens
            .iter()
            .any(|token| token.to_ascii_lowercase().contains("pid"));
    has_kill && (has_trap || references_background_pid)
}

pub(crate) fn shell_command_has_owned_dashboard_lifecycle(command: &str, tokens: &[String]) -> bool {
    let lower = command.to_ascii_lowercase();
    if lower.contains("validate-dashboard.cjs") && lower.contains("--launch-dashboard") {
        return true;
    }
    if tokens
        .iter()
        .any(|token| matches!(shell_token_basename(token), "timeout" | "gtimeout"))
    {
        return true;
    }
    shell_command_has_explicit_dashboard_cleanup(command, tokens)
}

pub(crate) fn managed_codex_foreground_dashboard_command(tool_name: &str, preview: &str) -> bool {
    if tool_name.trim() != "command" {
        return false;
    }
    let command = preview
        .trim()
        .strip_prefix("command:")
        .map(str::trim)
        .unwrap_or_else(|| preview.trim());
    if command.is_empty() {
        return false;
    }
    let tokens = shellish_command_tokens(command);
    shell_command_invokes_intendant_web(&tokens)
        && !shell_command_has_owned_dashboard_lifecycle(command, &tokens)
}

pub(crate) fn managed_context_recovery_pressure(
    snapshot: &external_agent::AgentContextSnapshot,
) -> Option<ManagedContextRewindOnlyPressure> {
    let used_tokens = external_context_snapshot_backend_token_count(snapshot)?;
    let rewind_only_limit = snapshot.context_window?;
    if rewind_only_limit == 0 {
        return None;
    }
    let hard_context_window = snapshot.hard_context_window;
    if used_tokens < rewind_only_limit {
        return None;
    }
    let status = if hard_context_window.is_some_and(|hard| hard > 0 && used_tokens >= hard) {
        "critical"
    } else {
        "high"
    };
    Some(ManagedContextRewindOnlyPressure {
        used_tokens,
        rewind_only_limit,
        hard_context_window,
        status,
    })
}

pub(crate) fn managed_context_user_kickstart_is_trivial(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "continue" | "resume" | "go on" | "carry on" | "keep going"
    )
}

pub(crate) fn managed_context_drop_original_for_recovery(
    text: &str,
    has_attachments: bool,
    has_steer_id: bool,
    is_user_turn_edit: bool,
) -> bool {
    !has_attachments
        && !has_steer_id
        && !is_user_turn_edit
        && managed_context_user_kickstart_is_trivial(text)
}

pub(crate) const MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND: u8 = 2;

/// Interrupt reason recorded when the managed-context density tool gate
/// blocks a broad ordinary tool mid-turn. The external-agent loop keys on
/// this exact reason to continue autonomously (density handoff / recovery
/// kickstart) instead of waiting for a user follow-up that headless
/// sessions never receive.
pub(crate) const MANAGED_CONTEXT_DENSITY_BLOCK_INTERRUPT_REASON: &str =
    "managed-context density watch blocked broad ordinary tool";

/// Upper bound on consecutive density-gate interrupts answered with an
/// automatic maintenance handoff while pressure never leaves the density
/// band. Past this, recovery did not converge and the loop fails loudly
/// instead of ping-ponging until the task timeout.
pub(crate) const MANAGED_CONTEXT_DENSITY_BLOCK_MAX_HANDOFFS_WITHOUT_RELIEF: u8 = 4;

pub(crate) fn managed_context_recovery_kickstart_text(
    pressure: ManagedContextRewindOnlyPressure,
    held_user_input: bool,
) -> String {
    let hard = pressure
        .hard_context_window
        .map(|hard| format!(" hard_limit={hard}"))
        .unwrap_or_default();
    let held = if held_user_input {
        " Intendant is holding the user's follow-up outside Codex history; replay it only after rewind_context succeeds."
    } else {
        ""
    };
    format!(
        "<managed_context_recovery>\nBackend-reported Codex context pressure is {status} ({used}/{limit} tokens{hard}), leaving too little room for a normal tool/result cycle. Do not continue normally. The Intendant MCP tools list_rewind_anchors and inspect_rewind_anchor are callable in this turn; any earlier transcript claim that either is unavailable is stale and incorrect. If a recovery catalog page from this stall is already in view, do not list again: choose one exact item_id from it and call rewind_context now. Otherwise call list_rewind_anchors once without a query to inspect the first bounded compact page of valid non-management recovery anchors; use next_offset/offset, limit, query, or reverse to inspect other catalog ranges without dumping the whole catalog, and never re-request a page you can already see. The normal catalog hides anchors known to remain at/above the rewind-only limit or without enough normal-tool resume headroom; include_non_recovery=true is diagnostic-only and rows with recovery_eligible=false must not be passed to rewind_context. If a compact catalog row is ambiguous, call inspect_rewind_anchor for the candidate item_id before mutating the thread. Then call rewind_context with one exact returned item_id, the returned position_hint or a value in positions, and a dense carry-forward primer. If the catalog reports no_eligible_anchors, do not keep listing: state that recovery has no valid anchor and end the turn so the supervisor can recover manually. A successful rewind only validates lineage; normal tools remain unavailable until backend-reported pressure has enough normal-tool headroom below the rewind-only limit. Do not synthesize anchor ids from prior failed tool calls. Do not use auto anchors or N-turn rewinds.{held}\n</managed_context_recovery>",
        status = pressure.status,
        used = pressure.used_tokens,
        limit = pressure.rewind_only_limit,
        hard = hard,
        held = held,
    )
}

pub(crate) fn managed_context_density_handoff_text(pressure: ManagedContextDensityPressure) -> String {
    let hard = pressure
        .hard_context_window
        .map(|hard| format!(" hard_limit={hard}"))
        .unwrap_or_default();
    format!(
        "<managed_context_density_handoff>\nwatch {used}/{limit}; recommended_density_threshold={recommended}{hard}. Maintenance only. For a useful density rewind, call list_rewind_anchors with density_candidates_only=true, include_pruning_estimates=true, limit=1; inspect only if that row is ambiguous; then call rewind_context with one exact returned item_id, a returned position, and a dense primer. Density rows hide anchors without a density-valid position and narrow positions to choices expected below the threshold. If no exact anchor is clearly worthwhile, reply with a concise no-rewind handoff covering durable facts, changed files, verification, constraints, remaining decisions, and state that you are leaving context unchanged. Do not do broad ordinary-tool work. Fission stays allowed: delegating separable work to a branch via fission_spawn is a valid density action. Do not use auto anchors, N-turn rewinds, synthesized ids, failed-example ids, or management-tool anchors.\n</managed_context_density_handoff>",
        used = pressure.used_tokens,
        limit = pressure.rewind_only_limit,
        recommended = pressure.recommended_rewind_limit,
        hard = hard,
    )
}

pub(crate) fn managed_context_density_active_steer_text(
    pressure: ManagedContextDensityPressure,
    in_flight_tool_count: usize,
) -> String {
    let hard = pressure
        .hard_context_window
        .map(|hard| format!(" hard_limit={hard}"))
        .unwrap_or_default();
    let in_flight = if in_flight_tool_count == 0 {
        "No command/tool was active when Intendant sent this steer; do not start a broad build, QA, exploration, or implementation loop before density maintenance."
    } else {
        "Allow the currently in-flight narrow validation/build/tool to finish and preserve its durable result, but do not start another broad build, QA, exploration, or implementation loop before density maintenance."
    };
    format!(
        "<managed_context_density_steer>\nBackend-reported Codex context pressure is watch ({used}/{limit} tokens, recommended_density_threshold={recommended}{hard}). This steer is freshness-bound to the latest backend-reported context status; if a later status reports below recommended_density_threshold, this steer is stale and must be ignored. {in_flight} Normal tools are still allowed below rewind_only, but before broad follow-up work do exact-anchor density maintenance if a current catalog anchor can materially reduce pressure below the recommended density threshold, or give a concise no-rewind density handoff that crystallizes durable facts, changed files, validation results, constraints, and remaining decisions. Fission tools stay allowed at watch: delegating separable work to a fission branch is itself a valid density action. Use list_rewind_anchors with density_candidates_only=true and include_pruning_estimates=true, and inspect_rewind_anchor only as needed; if rewinding, call rewind_context with one exact returned item_id, a valid returned position, and a dense carry-forward primer. Do not use auto anchors, N-turn rewinds, synthesized item ids, anchors from failed examples, or managed-context maintenance calls as rewind targets.\n</managed_context_density_steer>",
        used = pressure.used_tokens,
        limit = pressure.rewind_only_limit,
        recommended = pressure.recommended_rewind_limit,
        hard = hard,
        in_flight = in_flight,
    )
}

pub(crate) fn managed_context_density_active_steer_clear_text(
    prior_pressure: ManagedContextDensityPressure,
    usage: &external_agent::AgentUsageSnapshot,
) -> Option<String> {
    if usage.context_window == 0 || usage.tokens_used >= usage.context_window {
        return None;
    }
    let recommended_rewind_limit = managed_context_density_recommended_limit(usage.context_window);
    if usage.tokens_used >= recommended_rewind_limit {
        return None;
    }
    Some(format!(
        "<managed_context_density_steer_cleared>\nA later backend-reported Codex context snapshot is below the recommended density threshold ({used}/{limit} tokens, recommended_density_threshold={recommended}). This supersedes the earlier managed_context_density_steer from {prior_used}/{prior_limit} tokens. Do not call list_rewind_anchors, inspect_rewind_anchor, or rewind_context solely because of that stale density steer. Continue the current concrete work normally unless the latest get_status/context_pressure reports watch or rewind-only again, or a genuinely noisy/unexpectedly large result independently makes context maintenance worthwhile.\n</managed_context_density_steer_cleared>",
        used = usage.tokens_used,
        limit = usage.context_window,
        recommended = recommended_rewind_limit,
        prior_used = prior_pressure.used_tokens,
        prior_limit = prior_pressure.rewind_only_limit,
    ))
}

pub(crate) fn managed_context_backend_recovery_kickstart_text(
    message: &str,
    recovery_hint: Option<&str>,
) -> String {
    let hint = recovery_hint
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .map(|hint| format!(" Codex recovery hint: {hint}"))
        .unwrap_or_default();
    format!(
        "<managed_context_recovery>\nCodex reported backend recovery required before completing the turn: {message}.{hint} Do not continue normally. The Intendant MCP tools list_rewind_anchors and inspect_rewind_anchor are callable in this turn; any earlier transcript claim that either is unavailable is stale and incorrect. If a recovery catalog page from this stall is already in view, do not list again: choose one exact item_id from it and call rewind_context now. Otherwise call list_rewind_anchors once without a query to inspect the first bounded compact page of valid non-management recovery anchors; use next_offset/offset, limit, query, or reverse to inspect other catalog ranges without dumping the whole catalog, and never re-request a page you can already see. The normal catalog hides anchors known to remain at/above the rewind-only limit or without enough normal-tool resume headroom; include_non_recovery=true is diagnostic-only and rows with recovery_eligible=false must not be passed to rewind_context. If a compact catalog row is ambiguous, call inspect_rewind_anchor for the candidate item_id before mutating the thread. Then call rewind_context with one exact returned item_id, the returned position_hint or a value in positions, and a dense carry-forward primer. If the catalog reports no_eligible_anchors, do not keep listing: state that recovery has no valid anchor and end the turn so the supervisor can recover manually. A successful rewind only validates lineage; normal tools remain unavailable until backend-reported pressure has enough normal-tool headroom below the rewind-only limit. Do not synthesize anchor ids from prior failed tool calls. Do not use auto anchors or N-turn rewinds.\n</managed_context_recovery>"
    )
}

/// Cap on supervisor-forced surgical recoveries per session. Each one is a
/// last-resort context amputation with a synthetic primer (no model-authored
/// carry-forward), so repeated need signals a structural problem the loop
/// must surface loudly instead of papering over forever.
pub(crate) const MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES: u8 = 2;

/// Reason recorded on the durable rewind record (and shown in the dashboard)
/// for a supervisor-forced surgical recovery.
pub(crate) const MANAGED_CONTEXT_SURGICAL_RECOVERY_REASON: &str =
    "supervisor surgical recovery after step-limit exhaustion";

/// Whether another supervisor-forced surgical recovery may run this session.
/// Model rewinds do not consume this budget — only surgical ones — so a
/// session where the model recovers on its own never triggers the backstop.
pub(crate) fn managed_context_surgical_recovery_available(surgical_recoveries: u8) -> bool {
    surgical_recoveries < MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
}

/// First pruned rollout line for an (anchor, position) cut: `before` prunes
/// from the anchor's first occurrence, `after` keeps the whole anchored group
/// and prunes from the next line. Lower = deeper cut = more pruning.
pub(crate) fn managed_context_surgical_cut_start_line(
    anchor: &ContextRewindAnchorCatalogEntry,
    position: external_agent::RollbackAnchorPosition,
) -> usize {
    match position {
        external_agent::RollbackAnchorPosition::Before => anchor.first_line,
        external_agent::RollbackAnchorPosition::After => anchor.last_line.saturating_add(1),
    }
}

/// Supervisor-chosen anchor for a surgical recovery: the recovery-eligible
/// (anchor, position) pair with maximum pruning — i.e. the earliest cut line
/// — mirroring the model-visible default catalog (`list_rewind_anchors`):
/// rows with `recovery_eligible == Some(false)` (insufficient headroom,
/// prior-outcome veto, or inside the active recovery span) are excluded, and
/// per-row positions come from `recovery_eligible_positions`. Rows with
/// unknown eligibility (`None`, no backend usage coverage) are offered by the
/// catalog too, but only as a fallback here — at `after` (never `before`, so
/// an unknown first row cannot empty the thread) — and the apply path still
/// validates restore headroom before mutating anything.
pub(crate) fn managed_context_surgical_anchor_choice(
    anchors: &[ContextRewindAnchorCatalogEntry],
) -> Option<(String, external_agent::RollbackAnchorPosition)> {
    let eligible = anchors
        .iter()
        .filter(|anchor| anchor.recovery_eligible == Some(true))
        .flat_map(|anchor| {
            anchor
                .recovery_eligible_positions
                .iter()
                .flatten()
                .filter_map(move |position| {
                    external_agent::RollbackAnchorPosition::from_str(position)
                        .map(|position| (anchor, position))
                })
        })
        .min_by_key(|(anchor, position)| {
            (
                managed_context_surgical_cut_start_line(anchor, *position),
                anchor.ordinal,
            )
        });
    if let Some((anchor, position)) = eligible {
        return Some((anchor.item_id.clone(), position));
    }
    anchors
        .iter()
        .filter(|anchor| {
            anchor.recovery_eligible.is_none() && !context_rewind_anchor_is_management_tool(anchor)
        })
        .map(|anchor| (anchor, external_agent::RollbackAnchorPosition::After))
        .min_by_key(|(anchor, position)| {
            (
                managed_context_surgical_cut_start_line(anchor, *position),
                anchor.ordinal,
            )
        })
        .map(|(anchor, position)| (anchor.item_id.clone(), position))
}

/// Synthetic minimal primer for a supervisor-forced surgical recovery. The
/// supervisor cannot summarize the pruned span (only the model could), so the
/// primer states plainly what happened, restates the task, and points at the
/// durable rewind records / raw logs to rebuild working state from
/// (managed.md: "expose a manual/surgical recovery path that prunes just
/// enough context to let the model author the next rewind").
pub(crate) fn managed_context_surgical_primer(
    task_statement: Option<&str>,
    prior_rewind_record_ids: &[String],
) -> String {
    let mut out = String::from(
        "This is an automatic surgical recovery: the model did not choose a rewind anchor within the managed-context recovery step limit, so Intendant rewound the thread to the deepest recovery-eligible anchor itself. The pruned span was NOT summarized; no model-authored carry-forward exists for it.",
    );
    out.push_str("\n\nTask:\n");
    match task_statement.map(str::trim).filter(|task| !task.is_empty()) {
        Some(task) => out.push_str(task),
        None => out.push_str(
            "(no task statement was available to the supervisor; recover it from the preserved history or the rewind records below)",
        ),
    }
    out.push_str("\n\nRewind records so far (newest first): ");
    if prior_rewind_record_ids.is_empty() {
        out.push_str("none — this surgical record is the first rewind of the session.");
    } else {
        out.push_str(&prior_rewind_record_ids.join(", "));
    }
    out.push_str(
        "\n\nRebuild any working state you need from those rewind records and the session's raw logs (rewind_backout inspect, get_logs), verify what is already done before redoing expensive steps, and continue the task from the preserved history.",
    );
    out
}

/// Resume follow-up after a successful surgical recovery: the held user
/// follow-up when one is queued (managed.md: a held follow-up is delivered
/// only after the rewind succeeds), else the rewind's automatic resume.
pub(crate) fn managed_context_surgical_recovery_continuation(
    pending_replays: &mut std::collections::VecDeque<FollowUpMessage>,
    automatic_resume: Option<FollowUpMessage>,
) -> FollowUpMessage {
    pending_replays
        .pop_front()
        .map(managed_context_sanitize_queued_followup_replay)
        .or(automatic_resume)
        .unwrap_or_else(|| {
            FollowUpMessage::text(
                "<context_rewind_resumed>\nContinue from the model_context_rewind_primer that Intendant injected as developer context for the pruned span. Do not redo discarded work; continue with the next useful step.\n</context_rewind_resumed>"
                    .to_string(),
            )
        })
}

/// Supervisor-forced surgical context rewind — the backstop behind the
/// model-driven recovery flow. Ran when recovery kickstarts exhausted their
/// retry budget without a rewind (the fork's recovery turn hits its 8-step
/// limit and ends the turn while pressure is still rewind-only; the
/// supervisor observes the turn completing — or recovery being re-reported —
/// without a rewind). Instead of ending the session, the supervisor chooses
/// the deepest recovery-eligible anchor from the existing catalog and applies
/// the rewind itself with a synthetic minimal primer; the durable record is
/// marked `surgical` with a distinct reason. Returns the follow-up to resume
/// with (held user replay first, else the automatic resume).
pub(crate) async fn attempt_supervisor_surgical_context_rewind(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread_id: &str,
    config: &DrainConfig<'_>,
    task_statement: Option<&str>,
    pending_replays: &mut std::collections::VecDeque<FollowUpMessage>,
) -> Result<FollowUpMessage, String> {
    let snapshot = agent
        .read_thread_snapshot(thread_id)
        .await
        .map_err(|e| format!("failed to read thread metadata before surgical rewind: {e}"))?;
    let source_rollout_path = snapshot
        .rollout_path
        .ok_or_else(|| "thread metadata did not include a rollout path".to_string())?;
    let anchors = scan_context_rewind_anchor_catalog(&source_rollout_path).map_err(|err| {
        format!(
            "failed to inspect rewind anchors in {}: {err}",
            source_rollout_path.display()
        )
    })?;
    let Some((item_id, position)) = managed_context_surgical_anchor_choice(&anchors) else {
        return Err("no recovery-eligible anchor in the rewind catalog".to_string());
    };
    let prior_rewind_record_ids: Vec<String> = context_rewind::list_records(config.log_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|record| record.thread_id == thread_id)
        .map(|record| record.record_id)
        .collect();
    let request = ExternalContextRewindRequest {
        session_id: config.session_id.clone(),
        item_id,
        position,
        reason: Some(MANAGED_CONTEXT_SURGICAL_RECOVERY_REASON.to_string()),
        primer: Some(managed_context_surgical_primer(
            task_statement,
            &prior_rewind_record_ids,
        )),
        preserve: Vec::new(),
        discard: Vec::new(),
        artifacts: Vec::new(),
        next_steps: Vec::new(),
        auto_resume: true,
        require_density_improvement: false,
        surgical: true,
    };
    let automatic_resume = apply_external_context_rewind(agent, thread_id, &request, config)
        .await
        .map_err(|e| format!("surgical rewind to {} failed: {e}", request.target_label()))?;
    Ok(managed_context_surgical_recovery_continuation(
        pending_replays,
        automatic_resume,
    ))
}

pub(crate) async fn emit_external_context_snapshot_if_changed(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
    turn: Option<usize>,
    state: &mut ExternalContextSnapshotState,
) {
    match agent.context_snapshots().await {
        Ok(snapshots) => {
            let mut emitted = false;
            for snapshot in snapshots {
                let key = external_context_snapshot_key(&snapshot);
                if !state.emitted_keys.insert(key) {
                    continue;
                }
                emitted = true;
                state.last_error = None;
                let usage = external_context_snapshot_usage(&snapshot);
                config.bus.send(AppEvent::ContextSnapshot {
                    session_id: config.session_id.clone(),
                    source: snapshot.source,
                    label: snapshot.label,
                    request_id: snapshot.request_id,
                    request_index: snapshot.request_index,
                    turn,
                    format: snapshot.format,
                    token_count: snapshot.token_count,
                    token_count_kind: snapshot
                        .token_count_kind
                        .map(|kind| kind.as_str().to_string()),
                    context_window: snapshot.context_window,
                    hard_context_window: snapshot.hard_context_window,
                    item_count: snapshot.item_count,
                    raw: snapshot.raw,
                });
                if let Some(main) = usage {
                    emit_external_context_usage_snapshot_from_usage(config, main);
                }
            }
            if !emitted {
                state.last_error = None;
            }
        }
        Err(e) => {
            let message = format!(
                "Failed to read context snapshot from {}: {}",
                agent.name(),
                e
            );
            if state.last_error.as_deref() != Some(message.as_str()) {
                slog(config.session_log, |l| l.warn(&message));
                state.last_error = Some(message);
            }
        }
    }
}

pub(crate) const MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN: &str =
    "<managed_context_rewind_followup_replay>";
pub(crate) const MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_CLOSE: &str =
    "</managed_context_rewind_followup_replay>";
pub(crate) const MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_USER_MARKER: &str = "\n\nUser follow-up:\n";
pub(crate) const MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_INSTRUCTIONS: &str =
    "A managed-context rewind requested during this queued follow-up has already succeeded. Continue the user's follow-up below from the rewound context. Do not call rewind_context again merely to satisfy any instruction to rewind first; only rewind again if new context pressure or an invalid anchor genuinely requires it.";
pub(crate) const MANAGED_CONTEXT_REWIND_ACTIVE_RESUME_INSTRUCTIONS: &str =
    "A managed-context rewind requested during the active follow-up has already succeeded. The active follow-up is already in the preserved thread history; the model_context_rewind_primer is the authoritative carry-forward summary for the pruned span. Continue with the next unfinished step. Use only completed validation, setup, or research facts that are preserved in the current history or primer, and do not call rewind_context again merely to satisfy any prior instruction to rewind first.";

pub(crate) fn managed_context_canonical_followup_replay_text(text: &str) -> String {
    let mut current = text.trim();
    loop {
        let Some(inner) = current
            .strip_prefix(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN)
            .and_then(|inner| inner.strip_suffix(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_CLOSE))
        else {
            break;
        };
        let Some((_, user_followup)) =
            inner.split_once(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_USER_MARKER)
        else {
            break;
        };
        let next = user_followup.trim();
        if next == current {
            break;
        }
        current = next;
    }
    current.to_string()
}

pub(crate) fn managed_context_followup_replay_text(user_followup: &str) -> String {
    format!(
        "{open}\n{instructions}{marker}{user_followup}\n{close}",
        open = MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN,
        instructions = MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_INSTRUCTIONS,
        marker = MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_USER_MARKER,
        user_followup = user_followup.trim(),
        close = MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_CLOSE,
    )
}

pub(crate) fn managed_context_rewind_turn_stop_status_text(
    status: &ManagedContextRewindTurnStopStatus,
) -> Option<String> {
    match status {
        ManagedContextRewindTurnStopStatus::NotRequested => None,
        ManagedContextRewindTurnStopStatus::StopRequestedNoToolObserved => Some(
            "Tool/command status: Intendant requested a stop for the active turn before applying the rewind. No active tool or command completion was observed in the stop window."
                .to_string(),
        ),
        ManagedContextRewindTurnStopStatus::StopRequestedCompleted {
            success,
            failed,
            cancelled,
        } => {
            let total = success + failed + cancelled;
            if *failed == 0 && *cancelled == 0 {
                Some(format!(
                    "Tool/command status: Intendant requested a stop for the active turn before applying the rewind. All {total} tool(s)/command(s) active in the stop window emitted successful completion before the rewind."
                ))
            } else {
                Some(format!(
                    "Tool/command status: Intendant requested a stop for the active turn before applying the rewind. Tool(s)/command(s) active in the stop window emitted completion before the rewind with statuses: {success} success, {failed} failed, {cancelled} cancelled. A cancelled validation or setup command has no successful result preserved; rerun any required check whose success is not preserved in the current history or primer."
                ))
            }
        }
        ManagedContextRewindTurnStopStatus::StopRequestedUnfinished {
            pending,
            success,
            failed,
            cancelled,
        } => Some(format!(
            "Tool/command status: Intendant requested a stop for the active turn before applying the rewind. {pending} tool(s)/command(s) active in the stop window did not emit completion before the rewind; their outcome is unknown. Completed statuses observed before the rewind: {success} success, {failed} failed, {cancelled} cancelled. Rerun any required validation or setup whose result is not preserved in the current history or primer."
        )),
        ManagedContextRewindTurnStopStatus::StopRequestFailed { message } => Some(format!(
            "Tool/command status: Intendant attempted to stop the active turn before applying the rewind, but the stop request failed: {message}. Treat tool outcomes according to the current preserved history and primer."
        )),
    }
}

pub(crate) fn managed_context_active_followup_resume_text(
    turn_stop_status: &ManagedContextRewindTurnStopStatus,
) -> String {
    let status_text = managed_context_rewind_turn_stop_status_text(turn_stop_status)
        .map(|text| format!("\n\n{text}"))
        .unwrap_or_default();
    format!(
        "{open}\n{instructions}{status_text}\n{close}",
        open = MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN,
        instructions = MANAGED_CONTEXT_REWIND_ACTIVE_RESUME_INSTRUCTIONS,
        status_text = status_text,
        close = MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_CLOSE,
    )
}

pub(crate) fn managed_context_is_active_followup_resume(text: &str) -> bool {
    let text = text.trim();
    text.strip_prefix(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN)
        .and_then(|inner| inner.strip_suffix(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_CLOSE))
        .is_some_and(|inner| {
            inner.contains(MANAGED_CONTEXT_REWIND_ACTIVE_RESUME_INSTRUCTIONS)
                && !inner.contains(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_USER_MARKER)
        })
}

pub(crate) fn managed_context_followup_replay_after_rewind(
    active_followup: &FollowUpMessage,
    turn_stop_status: &ManagedContextRewindTurnStopStatus,
) -> Option<FollowUpMessage> {
    if active_followup.managed_context_recovery_kickstart || active_followup.text.trim().is_empty()
    {
        return None;
    }

    let text = if managed_context_is_active_followup_resume(&active_followup.text) {
        active_followup.text.trim().to_string()
    } else {
        managed_context_active_followup_resume_text(turn_stop_status)
    };

    let followup = FollowUpMessage::with_attachments(text, active_followup.attachments.clone())
        .after_managed_context_density_handoff();
    Some(followup)
}

pub(crate) fn managed_context_sanitize_queued_followup_replay(
    mut followup: FollowUpMessage,
) -> FollowUpMessage {
    let canonical = managed_context_canonical_followup_replay_text(&followup.text);
    followup.text = managed_context_followup_replay_text(&canonical);
    followup
}

pub(crate) fn managed_context_rewind_continuation(
    pending_replays: &mut std::collections::VecDeque<FollowUpMessage>,
    active_followup: &FollowUpMessage,
    automatic_resume: Option<FollowUpMessage>,
    turn_stop_status: &ManagedContextRewindTurnStopStatus,
) -> Option<FollowUpMessage> {
    pending_replays
        .pop_front()
        .map(managed_context_sanitize_queued_followup_replay)
        .or_else(|| managed_context_followup_replay_after_rewind(active_followup, turn_stop_status))
        .or(automatic_resume)
}

pub(crate) fn managed_context_recovery_without_rewind_blocks_held_replay(
    managed_context_recovery_kickstart: bool,
    pending_replays: &std::collections::VecDeque<FollowUpMessage>,
) -> bool {
    managed_context_recovery_kickstart && !pending_replays.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn scoped_event_targets_config_matches_session_or_alias() {
        assert!(scoped_event_targets_config(
            &Some("session-1".to_string()),
            &Some("session-1".to_string()),
            &None,
        ));
        assert!(scoped_event_targets_config(
            &Some("codex-thread".to_string()),
            &Some("intendant-session".to_string()),
            &Some("codex-thread".to_string()),
        ));
        assert!(!scoped_event_targets_config(
            &Some("side-thread".to_string()),
            &Some("intendant-session".to_string()),
            &Some("codex-thread".to_string()),
        ));
        assert!(scoped_event_targets_config(
            &None,
            &Some("intendant-session".to_string()),
            &Some("codex-thread".to_string()),
        ));
    }

    #[test]
    fn external_context_snapshot_usage_tracks_codex_backend_pressure() {
        let snapshot = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(71_876),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(51),
            raw: serde_json::json!({
                "model": "gpt-5.5",
                "input": []
            }),
        };

        let usage = external_context_snapshot_usage(&snapshot).unwrap();
        assert_eq!(usage.provider, "openai");
        assert_eq!(usage.model, "gpt-5.5");
        assert_eq!(usage.tokens_used, 71_876);
        assert_eq!(usage.context_window, 258_400);
        assert_eq!(usage.hard_context_window, Some(272_000));
        assert_eq!(usage.prompt_tokens, 71_876);
        assert_eq!(usage.completion_tokens, 0);
        assert!((usage.usage_pct - (71_876.0 / 258_400.0 * 100.0)).abs() < 1e-12);

        let local_estimate = external_agent::AgentContextSnapshot {
            token_count: Some(312_502),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::LocalEstimate),
            ..snapshot
        };
        assert!(external_context_snapshot_usage(&local_estimate).is_none());
    }

    #[test]
    fn forced_context_usage_snapshot_emits_backend_pressure_usage() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("wrapper-session".to_string()),
            alias_session_id: Some("codex-thread".to_string()),
            backend_thread_id: Some("codex-thread".to_string()),
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let snapshot = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(70_046),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(51),
            raw: serde_json::json!({ "model": "gpt-5.2-codex" }),
        };

        assert!(emit_external_context_usage_snapshot(&config, &snapshot));
        match rx.try_recv().expect("usage event") {
            AppEvent::UsageSnapshot {
                session_id,
                main,
                presence,
            } => {
                assert_eq!(session_id.as_deref(), Some("wrapper-session"));
                assert_eq!(main.provider, "openai");
                assert_eq!(main.model, "gpt-5.2-codex");
                assert_eq!(main.tokens_used, 70_046);
                assert_eq!(main.context_window, 258_400);
                assert_eq!(main.hard_context_window, Some(272_000));
                assert!(presence.is_none());
            }
            other => panic!("expected UsageSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn managed_context_preflight_can_use_latest_session_log_snapshot() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        {
            let mut log = session_log.lock().unwrap();
            log.context_snapshot_for_session(
                Some("other-thread"),
                "codex",
                "Other Codex resolved request payload",
                Some("req-other"),
                Some(1),
                Some(2),
                "openai.responses.resolved_request.v1",
                Some(100_000),
                Some("backend_reported"),
                Some(258_400),
                Some(272_000),
                Some(12),
                &serde_json::json!({ "model": "gpt-5.2-codex" }),
            );
            log.context_snapshot_for_session(
                Some("codex-thread"),
                "codex",
                "Codex resolved request payload",
                Some("req-1"),
                Some(4),
                Some(8),
                "openai.responses.resolved_request.v1",
                Some(225_440),
                Some("backend_reported"),
                Some(258_400),
                Some(272_000),
                Some(632),
                &serde_json::json!({ "model": "gpt-5.2-codex" }),
            );
        }
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("wrapper-session".to_string()),
            alias_session_id: Some("codex-thread".to_string()),
            backend_thread_id: Some("codex-thread".to_string()),
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };

        let snapshot = latest_external_context_snapshot_from_log(&config).expect("snapshot");
        assert_eq!(snapshot.request_id.as_deref(), Some("req-1"));
        assert_eq!(snapshot.token_count, Some(225_440));
        assert_eq!(
            snapshot.token_count_kind,
            Some(external_agent::AgentContextTokenCountKind::BackendReported)
        );
        let followup = FollowUpMessage::text("Continue Station QA.".to_string());
        assert!(matches!(
            managed_context_preflight_decision(true, &followup, &snapshot),
            Some(ManagedContextPreflightDecision::DensityHandoff { .. })
        ));
    }

    #[test]
    fn managed_context_rewind_only_pressure_uses_soft_limit() {
        let below_soft = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(258_399),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(51),
            raw: serde_json::json!({}),
        };
        assert_eq!(managed_context_rewind_only_pressure(&below_soft), None);

        let at_soft = external_agent::AgentContextSnapshot {
            token_count: Some(258_400),
            ..below_soft.clone()
        };
        assert_eq!(
            managed_context_rewind_only_pressure(&at_soft),
            Some(ManagedContextRewindOnlyPressure {
                used_tokens: 258_400,
                rewind_only_limit: 258_400,
                hard_context_window: Some(272_000),
                status: "high",
            })
        );

        let at_hard = external_agent::AgentContextSnapshot {
            token_count: Some(272_000),
            ..below_soft
        };
        assert_eq!(
            managed_context_rewind_only_pressure(&at_hard).map(|pressure| pressure.status),
            Some("critical")
        );

        let over_hard = external_agent::AgentContextSnapshot {
            token_count: Some(312_502),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::LocalEstimate),
            ..at_hard
        };
        assert_eq!(managed_context_rewind_only_pressure(&over_hard), None);
    }

    #[test]
    fn managed_context_rewind_only_pressure_from_usage_uses_soft_limit() {
        let below_soft = external_agent::AgentUsageSnapshot {
            provider: "openai".to_string(),
            model: "gpt-5.2-codex".to_string(),
            tokens_used: 258_399,
            context_window: 258_400,
            hard_context_window: Some(272_000),
            usage_pct: 99.9,
            prompt_tokens: 258_000,
            completion_tokens: 399,
            cached_tokens: 0,
            ..Default::default()
        };
        assert_eq!(
            managed_context_rewind_only_pressure_from_usage(&below_soft),
            None
        );

        let at_soft = external_agent::AgentUsageSnapshot {
            tokens_used: 258_400,
            ..below_soft.clone()
        };
        assert_eq!(
            managed_context_rewind_only_pressure_from_usage(&at_soft),
            Some(ManagedContextRewindOnlyPressure {
                used_tokens: 258_400,
                rewind_only_limit: 258_400,
                hard_context_window: Some(272_000),
                status: "high",
            })
        );

        let at_hard = external_agent::AgentUsageSnapshot {
            tokens_used: 272_000,
            ..below_soft
        };
        assert_eq!(
            managed_context_rewind_only_pressure_from_usage(&at_hard)
                .map(|pressure| pressure.status),
            Some("critical")
        );
    }

    #[test]
    fn managed_context_rewind_only_tool_classifier_allows_only_safe_tools() {
        assert!(managed_context_rewind_only_tool_allowed(
            "mcp",
            "intendant:list_rewind_anchors"
        ));
        assert!(managed_context_rewind_only_tool_allowed(
            "mcp",
            "intendant:rewind_context"
        ));
        assert!(managed_context_rewind_only_tool_allowed(
            "mcp",
            "intendant:get_status"
        ));
        assert!(managed_context_rewind_only_tool_allowed(
            "get_controller_loop_status",
            ""
        ));
        assert!(!managed_context_rewind_only_tool_allowed(
            "mcp",
            "intendant:execute_cu_actions"
        ));
        assert!(!managed_context_rewind_only_tool_allowed(
            "command",
            "git status"
        ));
        assert!(!managed_context_rewind_only_tool_allowed(
            "web_search",
            "search"
        ));
    }

    #[test]
    fn managed_context_density_tool_classifier_allows_fission_at_watch_only() {
        // Watch band (density steer): fission tools may start — delegating
        // separable work to a branch is itself a density action.
        assert!(managed_context_density_tool_allowed("fission_spawn", ""));
        assert!(managed_context_density_tool_allowed("fission_control", ""));
        assert!(managed_context_density_tool_allowed(
            "claim_fission_canonical",
            ""
        ));
        assert!(managed_context_density_tool_allowed(
            "mcp",
            "intendant:fission_spawn"
        ));
        assert!(managed_context_density_tool_allowed(
            "mcp",
            "fission_control"
        ));
        assert!(managed_context_density_tool_allowed(
            "mcp",
            "intendant:claim_fission_canonical"
        ));
        // Everything the rewind-only gate allows stays allowed at watch...
        assert!(managed_context_density_tool_allowed(
            "mcp",
            "intendant:rewind_context"
        ));
        assert!(managed_context_density_tool_allowed("get_status", ""));
        // ...while broad ordinary tools stay blocked at watch.
        assert!(!managed_context_density_tool_allowed(
            "command",
            "cargo build"
        ));
        assert!(!managed_context_density_tool_allowed(
            "mcp",
            "intendant:execute_cu_actions"
        ));
        // Rewind-only stays stricter: fission is blocked there with every
        // other ordinary tool — the parent must shrink first.
        assert!(!managed_context_rewind_only_tool_allowed(
            "fission_spawn",
            ""
        ));
        assert!(!managed_context_rewind_only_tool_allowed(
            "fission_control",
            ""
        ));
        assert!(!managed_context_rewind_only_tool_allowed(
            "claim_fission_canonical",
            ""
        ));
        assert!(!managed_context_rewind_only_tool_allowed(
            "mcp",
            "intendant:fission_spawn"
        ));
        assert!(!managed_context_rewind_only_tool_allowed(
            "mcp",
            "intendant:claim_fission_canonical"
        ));
    }

    #[test]
    fn managed_codex_dashboard_command_classifier_flags_foreground_launch() {
        assert!(managed_codex_foreground_dashboard_command(
            "command",
            "./target/release/intendant --web 8997 --no-tui --no-tls --agent codex"
        ));
        assert!(managed_codex_foreground_dashboard_command(
            "command",
            "bash -lc './target/release/intendant --web=8997 --no-tui --no-tls --agent codex'"
        ));
    }

    #[test]
    fn managed_codex_dashboard_command_classifier_allows_owned_lifecycle() {
        assert!(!managed_codex_foreground_dashboard_command(
            "command",
            "node scripts/validate-dashboard.cjs --launch-dashboard --port 8997 --selector '#app'"
        ));
        assert!(!managed_codex_foreground_dashboard_command(
            "command",
            "timeout 60 ./target/release/intendant --web 8997 --no-tui --no-tls"
        ));
        assert!(!managed_codex_foreground_dashboard_command(
            "command",
            "set -e; ./target/release/intendant --web 8997 --no-tui > /tmp/intendant.log 2>&1 & server_pid=$!; trap 'kill $server_pid' EXIT; curl -fsS http://127.0.0.1:8997/debug"
        ));
        assert!(!managed_codex_foreground_dashboard_command(
            "command",
            "rg './target/release/intendant --web 8997' docs/src"
        ));
        assert!(!managed_codex_foreground_dashboard_command(
            "mcp",
            "./target/release/intendant --web 8997 --no-tui"
        ));
    }

    #[test]
    fn managed_context_recovery_pressure_excludes_below_soft_watch_state() {
        let snapshot = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(220_385),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(458),
            raw: serde_json::json!({}),
        };

        assert_eq!(managed_context_recovery_pressure(&snapshot), None);
        assert_eq!(managed_context_rewind_only_pressure(&snapshot), None);
    }

    #[test]
    fn managed_context_density_pressure_uses_recommended_threshold_only() {
        let below = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(219_639),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(458),
            raw: serde_json::json!({}),
        };
        assert_eq!(managed_context_density_pressure(&below), None);

        let watch = external_agent::AgentContextSnapshot {
            token_count: Some(241_746),
            ..below.clone()
        };
        assert_eq!(
            managed_context_density_pressure(&watch),
            Some(ManagedContextDensityPressure {
                used_tokens: 241_746,
                recommended_rewind_limit: 219_640,
                rewind_only_limit: 258_400,
                hard_context_window: Some(272_000),
            })
        );
        assert_eq!(managed_context_rewind_only_pressure(&watch), None);

        let rewind_only = external_agent::AgentContextSnapshot {
            token_count: Some(258_400),
            ..below
        };
        assert_eq!(managed_context_density_pressure(&rewind_only), None);
        assert!(managed_context_rewind_only_pressure(&rewind_only).is_some());
    }

    #[test]
    fn managed_context_preflight_decision_holds_density_followup() {
        let snapshot = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(225_440),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(458),
            raw: serde_json::json!({}),
        };
        let followup = FollowUpMessage::text("Continue Station QA and fixes.".to_string());

        let decision =
            managed_context_preflight_decision(true, &followup, &snapshot).expect("decision");
        match decision {
            ManagedContextPreflightDecision::DensityHandoff {
                handoff_followup,
                held_followup,
                pressure,
            } => {
                assert!(handoff_followup.managed_context_density_handoff);
                assert_eq!(held_followup.text, "Continue Station QA and fixes.");
                assert!(!held_followup.managed_context_density_handoff);
                assert_eq!(pressure.used_tokens, 225_440);
                assert_eq!(pressure.recommended_rewind_limit, 219_640);
            }
            other => panic!("expected density handoff, got {:?}", other),
        }
    }

    #[test]
    fn managed_context_preflight_decision_drops_trivial_recovery_kickstart() {
        let snapshot = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(258_400),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(458),
            raw: serde_json::json!({}),
        };
        let followup = FollowUpMessage::text("continue".to_string());

        let decision =
            managed_context_preflight_decision(true, &followup, &snapshot).expect("decision");
        match decision {
            ManagedContextPreflightDecision::Recovery {
                recovery_followup,
                held_followup,
                pressure,
            } => {
                assert!(recovery_followup.managed_context_recovery_kickstart);
                assert!(held_followup.is_none());
                assert_eq!(pressure.used_tokens, 258_400);
                assert!(!recovery_followup.text.contains("held"));
            }
            other => panic!("expected recovery kickstart, got {:?}", other),
        }
    }

    #[test]
    fn managed_context_recovery_kickstart_flow_is_append_only() {
        let snapshot = external_agent::AgentContextSnapshot {
            source: "codex".to_string(),
            label: "Codex resolved request payload".to_string(),
            request_id: Some("req-1".to_string()),
            request_index: Some(1),
            rollout_path: None,
            format: "openai.responses.resolved_request.v1".to_string(),
            token_count: Some(258_400),
            token_count_kind: Some(external_agent::AgentContextTokenCountKind::BackendReported),
            context_window: Some(258_400),
            hard_context_window: Some(272_000),
            item_count: Some(458),
            raw: serde_json::json!({}),
        };
        let original_text = "Implement the next milestone and run the tests.".to_string();
        let followup = FollowUpMessage::text(original_text.clone());

        let decision =
            managed_context_preflight_decision(true, &followup, &snapshot).expect("decision");
        let ManagedContextPreflightDecision::Recovery {
            recovery_followup,
            held_followup,
            ..
        } = decision
        else {
            panic!("expected recovery kickstart");
        };

        // Held replay text is byte-identical to what the user sent.
        let held = held_followup.expect("non-trivial follow-up is held");
        assert_eq!(held.text, original_text);

        // The kickstart is a fresh appended user message: no user-turn edit
        // (edits rewrite an earlier request message) and no reuse of the
        // held follow-up's identity.
        assert!(recovery_followup.managed_context_recovery_kickstart);
        assert!(recovery_followup.edit_user_turn_index.is_none());
        assert!(recovery_followup.edit_user_turn_revision.is_none());
        assert!(recovery_followup.steer_id.is_none());
        assert!(recovery_followup.attachments.is_empty());

        // The eventual replay wraps the held text without altering it.
        let replay = managed_context_sanitize_queued_followup_replay(held);
        assert!(replay.text.contains(&original_text));
        assert_eq!(
            managed_context_canonical_followup_replay_text(&replay.text),
            original_text
        );
    }

    #[test]
    fn managed_context_density_handoff_text_preserves_exact_anchor_policy() {
        let text = managed_context_density_handoff_text(ManagedContextDensityPressure {
            used_tokens: 241_746,
            recommended_rewind_limit: 219_640,
            rewind_only_limit: 258_400,
            hard_context_window: Some(272_000),
        });

        assert!(text.contains("watch 241746/258400"));
        assert!(text.contains("recommended_density_threshold=219640"));
        assert!(text.contains("density_candidates_only=true"));
        assert!(text.contains("limit=1"));
        assert!(text.contains("one exact returned item_id"));
        assert!(text.contains("narrow positions"));
        assert!(text.contains("concise no-rewind handoff"));
        assert!(text.contains("leaving context unchanged"));
        assert!(text.contains("Fission stays allowed"));
        assert!(text.contains("fission_spawn"));
        assert!(text.contains("Do not use auto anchors"));
        assert!(text.contains("N-turn rewinds"));
        assert!(!text.contains("call_"));
        assert!(!text.contains("rewind N"));
        assert!(
            text.len() < 1_100,
            "density maintenance prompt should stay tiny: {} bytes",
            text.len()
        );
    }

    #[test]
    fn managed_context_density_rewind_marks_replay_handoff_completed() {
        let active =
            FollowUpMessage::text("density handoff".into()).managed_context_density_handoff();
        let mut pending = std::collections::VecDeque::from([FollowUpMessage::text(
            "Run the next narrow browser QA step.".into(),
        )]);

        let continuation = managed_context_rewind_continuation(
            &mut pending,
            &active,
            None,
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("held follow-up should replay")
        .after_managed_context_density_handoff();

        assert_eq!(
            continuation.text,
            managed_context_followup_replay_text("Run the next narrow browser QA step.")
        );
        assert!(continuation.text.contains("has already succeeded"));
        assert!(continuation.text.contains("User follow-up:"));
        assert!(continuation.managed_context_density_handoff_completed);
        assert!(!continuation.managed_context_density_handoff);
    }

    #[test]
    fn managed_context_active_rewind_replay_suppresses_repeat_density_handoff() {
        let active = FollowUpMessage::text("Continue the narrow Station QA loop.".into());
        let mut pending = std::collections::VecDeque::new();

        let continuation = managed_context_rewind_continuation(
            &mut pending,
            &active,
            None,
            &ManagedContextRewindTurnStopStatus::StopRequestedUnfinished {
                pending: 1,
                success: 0,
                failed: 0,
                cancelled: 0,
            },
        )
        .expect("active follow-up should replay after rewind");

        assert!(managed_context_is_active_followup_resume(
            &continuation.text
        ));
        assert!(continuation.managed_context_density_handoff_completed);
        assert!(!continuation.managed_context_density_handoff);
        assert!(managed_context_preflight_rewind_only_gate_enabled(
            true,
            continuation.managed_context_recovery_kickstart,
            continuation.managed_context_density_handoff,
        ));
        assert!(!managed_context_preflight_density_gate_enabled(
            true,
            continuation.managed_context_density_handoff_completed,
        ));
        assert!(!managed_context_post_turn_density_handoff_enabled(
            false,
            false,
            continuation.managed_context_density_handoff_completed,
        ));
    }

    #[test]
    fn managed_context_density_handoff_completed_still_checks_rewind_only_pressure() {
        assert!(managed_context_preflight_rewind_only_gate_enabled(
            true, false, false
        ));
        assert!(!managed_context_preflight_rewind_only_gate_enabled(
            true, true, false
        ));
        assert!(!managed_context_preflight_rewind_only_gate_enabled(
            true, false, true
        ));

        let replay =
            FollowUpMessage::text("held follow-up".into()).after_managed_context_density_handoff();
        assert!(replay.managed_context_density_handoff_completed);
        assert!(!replay.managed_context_density_handoff);
        assert!(managed_context_preflight_rewind_only_gate_enabled(
            true,
            replay.managed_context_recovery_kickstart,
            replay.managed_context_density_handoff,
        ));
        assert!(!managed_context_preflight_density_gate_enabled(
            true,
            replay.managed_context_density_handoff_completed,
        ));
        assert!(!managed_context_post_turn_density_handoff_enabled(
            false,
            false,
            replay.managed_context_density_handoff_completed,
        ));
        assert!(managed_context_post_turn_density_handoff_enabled(
            false, false, false,
        ));
    }

    #[test]
    fn managed_context_recovery_without_rewind_does_not_release_held_followup() {
        let held = FollowUpMessage::text("implement the queued normal task".into());
        let pending = std::collections::VecDeque::from([held]);
        assert!(managed_context_recovery_without_rewind_blocks_held_replay(
            true, &pending
        ));
        assert!(!managed_context_recovery_without_rewind_blocks_held_replay(
            false, &pending
        ));
        assert!(!managed_context_recovery_without_rewind_blocks_held_replay(
            true,
            &std::collections::VecDeque::new(),
        ));
    }

    #[test]
    fn context_rewind_thread_id_candidates_prefers_session_then_alias() {
        assert_eq!(
            context_rewind_thread_id_candidates(Some("backend-thread"), Some("wrapper-session")),
            vec!["backend-thread".to_string(), "wrapper-session".to_string()]
        );
        assert_eq!(
            context_rewind_thread_id_candidates(Some("same"), Some(" same ")),
            vec!["same".to_string()]
        );
        assert_eq!(
            context_rewind_thread_id_candidates(Some("  "), Some("alias")),
            vec!["alias".to_string()]
        );
    }

    #[test]
    fn managed_context_trivial_kickstarts_do_not_hold_user_input() {
        assert!(managed_context_user_kickstart_is_trivial(" Continue "));
        assert!(managed_context_user_kickstart_is_trivial("keep going"));
        assert!(!managed_context_user_kickstart_is_trivial(""));
        assert!(!managed_context_user_kickstart_is_trivial(
            "continue, but use the station prototype"
        ));
        assert!(managed_context_drop_original_for_recovery(
            "continue", false, false, false
        ));
        assert!(!managed_context_drop_original_for_recovery(
            "continue", false, false, true
        ));
        assert!(!managed_context_drop_original_for_recovery(
            "continue", true, false, false
        ));
        assert!(!managed_context_drop_original_for_recovery(
            "continue", false, true, false
        ));
    }

    #[test]
    fn managed_context_recovery_kickstart_corrects_stale_tool_claims_without_anchors() {
        let text = managed_context_recovery_kickstart_text(
            ManagedContextRewindOnlyPressure {
                used_tokens: 269_000,
                rewind_only_limit: 258_400,
                hard_context_window: Some(272_000),
                status: "critical",
            },
            true,
        );

        assert!(text.contains("list_rewind_anchors and inspect_rewind_anchor are callable"));
        assert!(text.contains("If a recovery catalog page from this stall is already in view"));
        assert!(text.contains("call list_rewind_anchors once without a query"));
        assert!(text.contains("never re-request a page you can already see"));
        assert!(text.contains("no_eligible_anchors"));
        assert!(text.contains("stale and incorrect"));
        assert!(text.contains("Do not synthesize anchor ids"));
        assert!(text.contains("hard_limit=272000"));
        assert!(text.contains("holding the user's follow-up outside Codex history"));
        assert!(!text.contains("call_"));
        assert!(!text.contains("item_id `"));
    }

    #[test]
    fn managed_context_backend_recovery_kickstart_requires_exact_rewind() {
        let text = managed_context_backend_recovery_kickstart_text(
            "Codex ran out of room",
            Some("rewind context first"),
        );

        assert!(text.contains("backend recovery required"));
        assert!(text.contains("Codex recovery hint: rewind context first"));
        assert!(text.contains("list_rewind_anchors"));
        assert!(text.contains("inspect_rewind_anchor"));
        assert!(text.contains("If a recovery catalog page from this stall is already in view"));
        assert!(text.contains("call list_rewind_anchors once without a query"));
        assert!(text.contains("rewind_context with one exact returned item_id"));
        assert!(text.contains("Do not synthesize anchor ids"));
        assert!(!text.contains("call_"));
    }

    #[test]
    fn managed_context_rewind_continuation_replays_active_followup_before_auto_resume() {
        let active = FollowUpMessage::text(
            "First rewind context, then implement the next Station slice.".into(),
        )
        .with_follow_up_id(Some("follow-1".into()));
        let automatic = FollowUpMessage::text("<context_rewind_resumed/>".into());
        let mut pending = std::collections::VecDeque::new();

        let continuation = managed_context_rewind_continuation(
            &mut pending,
            &active,
            Some(automatic),
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("continuation");

        assert!(continuation
            .text
            .contains("active follow-up is already in the preserved thread history"));
        assert!(continuation.text.contains("has already succeeded"));
        assert!(continuation
            .text
            .contains("Use only completed validation, setup, or research facts"));
        assert!(!continuation
            .text
            .contains("then implement the next Station slice"));
        assert_eq!(continuation.follow_up_id, None);
        assert!(!continuation.managed_context_recovery_kickstart);
    }

    #[test]
    fn managed_context_rewind_replay_is_idempotent_for_active_followup() {
        let original = "First inspect the failing harness log.\nThen patch the replay builder.";
        let active = FollowUpMessage::text(original.into());
        let first = managed_context_followup_replay_after_rewind(
            &active,
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("first replay");
        let second = managed_context_followup_replay_after_rewind(
            &first,
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("second replay");

        assert_eq!(second.text, first.text);
        assert!(!first.text.contains(original));
        assert!(first.text.contains(
            "the model_context_rewind_primer is the authoritative carry-forward summary"
        ));
        assert_eq!(
            second
                .text
                .matches(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN)
                .count(),
            1
        );
        assert_eq!(
            second
                .text
                .matches(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_CLOSE)
                .count(),
            1
        );
        assert_eq!(
            second
                .text
                .matches(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_USER_MARKER)
                .count(),
            0
        );
        assert!(second.text.contains("Use only completed validation"));
    }

    #[test]
    fn managed_context_rewind_active_replay_reports_unknown_stopped_tool() {
        let active = FollowUpMessage::text("Run release validation.".into());
        let status = ManagedContextRewindTurnStopStatus::StopRequestedUnfinished {
            pending: 1,
            success: 0,
            failed: 0,
            cancelled: 0,
        };

        let replay =
            managed_context_followup_replay_after_rewind(&active, &status).expect("replay");

        assert!(replay
            .text
            .contains("active turn before applying the rewind"));
        assert!(replay
            .text
            .contains("did not emit completion before the rewind"));
        assert!(replay.text.contains("their outcome is unknown"));
        assert!(replay
            .text
            .contains("Rerun any required validation or setup"));
    }

    #[test]
    fn managed_context_rewind_active_replay_reports_completed_stopped_tool_status() {
        let active = FollowUpMessage::text("Run release validation.".into());
        let status = ManagedContextRewindTurnStopStatus::StopRequestedCompleted {
            success: 0,
            failed: 0,
            cancelled: 1,
        };

        let replay =
            managed_context_followup_replay_after_rewind(&active, &status).expect("replay");

        assert!(replay.text.contains("emitted completion before the rewind"));
        assert!(replay.text.contains("0 success, 0 failed, 1 cancelled"));
        assert!(replay
            .text
            .contains("cancelled validation or setup command has no successful result"));
    }

    #[test]
    fn managed_context_rewind_replay_unwraps_nested_active_replay() {
        let original = "Apply the focused fix and run the harness unit test.";
        let nested =
            managed_context_followup_replay_text(&managed_context_followup_replay_text(original));
        let active = FollowUpMessage::text(nested);
        let mut pending = std::collections::VecDeque::new();

        let continuation = managed_context_rewind_continuation(
            &mut pending,
            &active,
            None,
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("continuation");

        assert_eq!(
            continuation.text,
            managed_context_active_followup_resume_text(
                &ManagedContextRewindTurnStopStatus::NotRequested
            )
        );
        assert_eq!(
            continuation
                .text
                .matches(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN)
                .count(),
            1
        );
        assert_eq!(continuation.text.matches(original).count(), 0);
    }

    #[test]
    fn managed_context_rewind_replay_sanitizes_nested_queued_replay() {
        let original = "Preserve the user's exact queued intent.";
        let nested =
            managed_context_followup_replay_text(&managed_context_followup_replay_text(original));
        let active =
            FollowUpMessage::text("recovery kickstart".into()).managed_context_recovery_kickstart();
        let mut pending = std::collections::VecDeque::from([FollowUpMessage::text(nested)]);

        let continuation = managed_context_rewind_continuation(
            &mut pending,
            &active,
            None,
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("continuation");

        assert_eq!(
            continuation.text,
            managed_context_followup_replay_text(original)
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn managed_context_rewind_continuation_prefers_held_followup() {
        let active =
            FollowUpMessage::text("recovery kickstart".into()).managed_context_recovery_kickstart();
        let held = FollowUpMessage::text("original queued user request".into());
        let automatic = FollowUpMessage::text("<context_rewind_resumed/>".into());
        let mut pending = std::collections::VecDeque::from([held]);

        let continuation = managed_context_rewind_continuation(
            &mut pending,
            &active,
            Some(automatic),
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("continuation");

        assert_eq!(
            continuation.text,
            managed_context_followup_replay_text("original queued user request")
        );
        assert!(continuation
            .text
            .contains("Do not call rewind_context again merely to satisfy"));
        assert!(pending.is_empty());
    }

    #[test]
    fn managed_context_rewind_continuation_uses_auto_resume_for_recovery_kickstart() {
        let active =
            FollowUpMessage::text("recovery kickstart".into()).managed_context_recovery_kickstart();
        let automatic = FollowUpMessage::text("<context_rewind_resumed/>".into());
        let mut pending = std::collections::VecDeque::new();

        let continuation = managed_context_rewind_continuation(
            &mut pending,
            &active,
            Some(automatic),
            &ManagedContextRewindTurnStopStatus::NotRequested,
        )
        .expect("continuation");

        assert_eq!(continuation.text, "<context_rewind_resumed/>");
    }

    fn surgical_test_catalog_entry(
        ordinal: usize,
        item_id: &str,
        first_line: usize,
        last_line: usize,
    ) -> ContextRewindAnchorCatalogEntry {
        ContextRewindAnchorCatalogEntry {
            ordinal,
            item_id: item_id.to_string(),
            first_line,
            last_line,
            first_item_type: "function_call".to_string(),
            last_item_type: "function_call".to_string(),
            last_item_is_model: true,
            positions: vec!["before", "after"],
            position_hint: "after",
            names: Vec::new(),
            roles: Vec::new(),
            summary: String::new(),
            backend_usage_at_or_after_anchor: None,
            backend_usage_before_anchor: None,
            rewind_only_limit_at_or_after_anchor: None,
            recommended_rewind_limit_at_or_after_anchor: None,
            prefix_estimated_tokens_before_anchor: None,
            prefix_estimated_tokens_after_anchor: None,
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
        }
    }

    #[test]
    fn managed_context_surgical_recovery_budget_caps_at_two_per_session() {
        assert!(managed_context_surgical_recovery_available(0));
        assert!(managed_context_surgical_recovery_available(1));
        assert!(!managed_context_surgical_recovery_available(
            MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
        ));
        assert!(!managed_context_surgical_recovery_available(u8::MAX));
    }

    #[test]
    fn managed_context_surgical_anchor_choice_picks_maximum_pruning() {
        // Three anchors: the earliest is vetoed (recovery_eligible=false, e.g.
        // a prior insufficient rewind), the middle is eligible at `after`, the
        // latest is eligible at both positions. The chooser must take the
        // earliest *eligible* cut — the middle anchor — not the vetoed one and
        // not the deeper-ordinal one.
        let mut vetoed = surgical_test_catalog_entry(0, "call_vetoed", 2, 2);
        vetoed.recovery_eligible = Some(false);
        let mut mid = surgical_test_catalog_entry(1, "call_mid", 5, 5);
        mid.recovery_eligible = Some(true);
        mid.recovery_eligible_positions = Some(vec!["after"]);
        let mut late = surgical_test_catalog_entry(2, "call_late", 9, 9);
        late.recovery_eligible = Some(true);
        late.recovery_eligible_positions = Some(vec!["before", "after"]);

        let (item_id, position) =
            managed_context_surgical_anchor_choice(&[vetoed.clone(), mid.clone(), late.clone()])
                .expect("choice");
        assert_eq!(item_id, "call_mid");
        assert_eq!(position, external_agent::RollbackAnchorPosition::After);

        // A `before`-eligible cut at the same anchor prunes one line more
        // than `after` at an earlier line: before@9 (cut line 9) loses to
        // after@5 (cut line 6), but before@5 beats after@5.
        let mut mid_before = mid.clone();
        mid_before.recovery_eligible_positions = Some(vec!["before", "after"]);
        let (item_id, position) =
            managed_context_surgical_anchor_choice(&[mid_before, late.clone()]).expect("choice");
        assert_eq!(item_id, "call_mid");
        assert_eq!(position, external_agent::RollbackAnchorPosition::Before);

        // No Some(true) anchors: unknown-eligibility anchors are the
        // fallback, always at `after`, and management tools are skipped
        // (mirroring the default catalog view).
        let mut management = surgical_test_catalog_entry(0, "call_listing", 1, 1);
        management.names = vec!["list_rewind_anchors".to_string()];
        let unknown = surgical_test_catalog_entry(1, "call_unknown", 3, 3);
        let (item_id, position) =
            managed_context_surgical_anchor_choice(&[management, unknown, vetoed])
                .expect("fallback choice");
        assert_eq!(item_id, "call_unknown");
        assert_eq!(position, external_agent::RollbackAnchorPosition::After);

        // Nothing usable at all → no surgical rewind.
        let mut only_vetoed = surgical_test_catalog_entry(0, "call_only", 1, 1);
        only_vetoed.recovery_eligible = Some(false);
        assert!(managed_context_surgical_anchor_choice(&[only_vetoed]).is_none());
        assert!(managed_context_surgical_anchor_choice(&[]).is_none());
    }

    #[test]
    fn managed_context_surgical_primer_lists_task_records_and_instruction() {
        let primer = managed_context_surgical_primer(
            Some("Refactor the parser and keep the CLI stable"),
            &["rewind-aaa".to_string(), "rewind-bbb".to_string()],
        );
        assert!(primer.contains("automatic surgical recovery"));
        assert!(primer.contains("recovery step limit"));
        assert!(primer.contains("Task:\nRefactor the parser and keep the CLI stable"));
        assert!(primer.contains("rewind-aaa, rewind-bbb"));
        assert!(primer.contains("rewind records"));
        assert!(primer.contains("continue the task"));

        // Without a task statement or prior records the primer says so
        // plainly instead of leaving empty sections.
        let primer = managed_context_surgical_primer(None, &[]);
        assert!(primer.contains("no task statement was available"));
        assert!(primer.contains("none — this surgical record is the first rewind"));
    }

    #[test]
    fn managed_context_surgical_recovery_continuation_prefers_held_replay() {
        let mut pending = std::collections::VecDeque::new();
        pending.push_back(FollowUpMessage::text("finish the held task".into()));
        let automatic = FollowUpMessage::text("<context_rewind_resumed/>".into());

        let continuation =
            managed_context_surgical_recovery_continuation(&mut pending, Some(automatic.clone()));
        assert!(continuation.text.contains("finish the held task"));
        assert!(continuation
            .text
            .starts_with(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN));
        assert!(pending.is_empty());

        // No held replay → the rewind's automatic resume.
        let continuation =
            managed_context_surgical_recovery_continuation(&mut pending, Some(automatic));
        assert_eq!(continuation.text, "<context_rewind_resumed/>");

        // Defensive total fallback keeps the session moving even if the
        // resume was somehow absent.
        let continuation = managed_context_surgical_recovery_continuation(&mut pending, None);
        assert!(continuation.text.contains("<context_rewind_resumed>"));
    }

    #[test]
    fn context_rewind_request_requires_primer_for_model_tool() {
        let params = serde_json::json!({
            "anchor": {"item_id": "call-123", "position": "after"},
            "reason": "prune noisy output"
        });
        let err = external_context_rewind_request_from_action("rewind_context", &params, None)
            .expect("rewind action")
            .unwrap_err();
        assert!(err.contains("primer"), "got: {err}");
    }

    #[test]
    fn context_rewind_request_renders_developer_primer() {
        let params = serde_json::json!({
            "anchor": {"item_id": "call-123", "position": "before"},
            "reason": "dead end",
            "primer": "Keep the useful result.",
            "preserve": [" fact A ", ""],
            "discard": ["bad assumption"],
            "artifacts": ["log.txt"],
            "next_steps": ["continue"]
        });
        let request = external_context_rewind_request_from_action(
            "rewind_context",
            &params,
            Some("s".into()),
        )
        .expect("rewind action")
        .expect("valid request");
        assert_eq!(request.item_id, "call-123");
        assert_eq!(
            request.position,
            external_agent::RollbackAnchorPosition::Before
        );
        assert!(request.auto_resume);
        assert!(context_rewind_should_interrupt_active_turn(&request));

        let primer = request
            .rendered_primer(Some("rewind-test-record"), None)
            .expect("primer");
        assert!(primer.contains("<model_context_rewind_primer>"));
        assert!(primer.contains("Earlier history before the target is still present"));
        assert!(primer.contains("Record id:\nrewind-test-record"));
        assert!(primer.contains("Keep the useful result."));
        assert!(primer.contains("- fact A"));
        assert!(primer.contains("- bad assumption"));
        assert!(!primer.contains("- \n"));
        assert!(request.resume_followup().is_some());
    }

    #[test]
    fn context_rewind_deferred_when_active_tool_and_normal_tools_allowed() {
        let params = serde_json::json!({
            "anchor": {"item_id": "call-123", "position": "after"},
            "reason": "cleanup stale build warning noise",
            "primer": "Keep the durable build result once it completes."
        });
        let request = external_context_rewind_request_from_action(
            "rewind_context",
            &params,
            Some("s".into()),
        )
        .expect("rewind action")
        .expect("valid request");

        let message = context_rewind_active_tool_defer_message(&request, 1, true)
            .expect("active optional cleanup rewind should defer");

        assert!(message.contains("context rewind deferred"));
        assert!(message.contains("1 active tool(s)/command(s) are still running"));
        assert!(message.contains("normal tools are currently allowed"));
        assert!(message.contains("did not stop the active turn or schedule the rewind"));
        assert!(message.contains("retry rewind_context"));
    }

    #[test]
    fn context_rewind_ignores_current_mcp_tool_for_active_defer() {
        assert_eq!(context_rewind_blocking_active_tool_count(1, Some("mcp")), 0);
        assert_eq!(context_rewind_blocking_active_tool_count(2, Some("mcp")), 1);
        assert_eq!(context_rewind_blocking_active_tool_count(1, None), 1);
    }

    #[test]
    fn context_rewind_mcp_self_tool_does_not_defer() {
        let params = serde_json::json!({
            "anchor": {"item_id": "call-123", "position": "after"},
            "reason": "cleanup stale build warning noise",
            "primer": "Keep the durable build result once it completes."
        });
        let request = external_context_rewind_request_from_action(
            "rewind_context",
            &params,
            Some("s".into()),
        )
        .expect("rewind action")
        .expect("valid request");
        let blocking = context_rewind_blocking_active_tool_count(1, Some("mcp"));

        assert!(context_rewind_active_tool_defer_message(&request, blocking, true).is_none());
    }

    #[test]
    fn context_rewind_not_deferred_without_active_tool_or_under_recovery_pressure() {
        let params = serde_json::json!({
            "anchor": {"item_id": "call-123", "position": "after"},
            "reason": "recover context",
            "primer": "Keep only the recovery facts."
        });
        let request = external_context_rewind_request_from_action(
            "rewind_context",
            &params,
            Some("s".into()),
        )
        .expect("rewind action")
        .expect("valid request");

        assert!(context_rewind_active_tool_defer_message(&request, 0, true).is_none());
        assert!(context_rewind_active_tool_defer_message(&request, 1, false).is_none());
    }

    #[test]
    fn context_rewind_request_renders_carried_forward_prior_facts() {
        let request = ExternalContextRewindRequest {
            session_id: Some("s".to_string()),
            item_id: "call-123".to_string(),
            position: external_agent::RollbackAnchorPosition::After,
            reason: Some("repeat rewind".to_string()),
            primer: Some("Current durable fact.".to_string()),
            preserve: Vec::new(),
            discard: Vec::new(),
            artifacts: Vec::new(),
            next_steps: Vec::new(),
            auto_resume: true,
            require_density_improvement: false,
            surgical: false,
        };

        let primer = request
            .rendered_primer(
                Some("rewind-test-record"),
                Some("- Prior selector: .display-picker.visible .display-picker-item"),
            )
            .expect("primer");

        assert!(primer.contains("Current durable fact."));
        assert!(primer.contains("Previous managed-context primer facts not repeated above"));
        assert!(primer.contains(".display-picker.visible .display-picker-item"));
    }

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
    fn context_rewind_rejects_anchor_inside_active_recovery_span() {
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
                            "text": "<managed_context_recovery>First call list_rewind_anchors without a query.</managed_context_recovery>"
                        }]
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
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_before_recovery",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect("pre-recovery anchor should remain valid");

        let err = validate_context_rewind_anchor_restore_headroom(
            &path,
            "call_recovery_status",
            external_agent::RollbackAnchorPosition::After,
        )
        .expect_err("active recovery span anchor must be rejected");
        assert!(
            err.contains("active managed-context recovery span"),
            "got: {err}"
        );
        assert!(
            err.contains("preserve the recovery kickstart prompt"),
            "got: {err}"
        );
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

    #[test]
    fn context_rewind_anchor_keeps_exact_rollout_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "call_id": "call_exact",
                    "arguments": "{}"
                }
            })
            .to_string(),
        )
        .unwrap();

        let exact = resolve_context_rewind_anchor(&path, "call_exact").expect("exact anchor");
        assert_eq!(exact.item_id, "call_exact");
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

    struct RecordingExternalAgent {
        sent: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        fail_send: bool,
    }

    #[async_trait::async_trait]
    impl external_agent::ExternalAgent for RecordingExternalAgent {
        fn name(&self) -> &str {
            "codex"
        }

        async fn initialize(
            &mut self,
            _config: external_agent::AgentConfig,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>, CallerError>
        {
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            Ok(rx)
        }

        async fn start_thread(&mut self) -> Result<external_agent::AgentThread, CallerError> {
            Ok(external_agent::AgentThread {
                thread_id: "thread-1".to_string(),
            })
        }

        async fn send_message(
            &mut self,
            thread: &external_agent::AgentThread,
            message: &str,
        ) -> Result<(), CallerError> {
            if self.fail_send {
                return Err(CallerError::ExternalAgent("turn/start failed".to_string()));
            }
            self.sent
                .lock()
                .unwrap()
                .push((thread.thread_id.clone(), message.to_string()));
            Ok(())
        }

        async fn resolve_approval(
            &mut self,
            _request_id: &str,
            _decision: external_agent::ApprovalDecision,
        ) -> Result<(), CallerError> {
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), CallerError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn primary_steer_followup_sends_turn_and_marks_delivered() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(RecordingExternalAgent {
            sent: sent.clone(),
            fail_send: false,
        });

        start_external_primary_steer_followup_turn(
            &mut agent,
            &config,
            "thread-1".to_string(),
            "continue on signed main".to_string(),
            "steer-1".to_string(),
            "codex reported no active parent turn; sending steer as immediate follow-up"
                .to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            *sent.lock().unwrap(),
            vec![(
                "thread-1".to_string(),
                "continue on signed main".to_string()
            )]
        );

        let mut saw_queued = false;
        let mut saw_delivered = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::SteerQueued {
                    session_id,
                    id,
                    reason,
                } => {
                    saw_queued = true;
                    assert_eq!(session_id.as_deref(), Some("thread-1"));
                    assert_eq!(id, "steer-1");
                    assert!(reason.contains("immediate follow-up"));
                }
                AppEvent::SteerDelivered {
                    session_id,
                    id,
                    mid_turn,
                } => {
                    saw_delivered = true;
                    assert_eq!(session_id.as_deref(), Some("thread-1"));
                    assert_eq!(id, "steer-1");
                    assert!(!mid_turn);
                }
                _ => {}
            }
        }
        assert!(saw_queued, "expected SteerQueued");
        assert!(saw_delivered, "expected SteerDelivered");
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
