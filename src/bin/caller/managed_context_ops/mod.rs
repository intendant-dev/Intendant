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

mod anchors;
pub(crate) use anchors::*;
mod apply;
pub(crate) use apply::*;
mod pressure;
pub(crate) use pressure::*;

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

#[cfg(test)]
mod tests {
    use super::*;

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

}
