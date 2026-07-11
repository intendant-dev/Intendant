//! External thread actions: codex fork/undo/rename and side threads,
//! fission spawn/import, context-rewind backout and anchor list/inspect
//! actions, subagent state emits, and the thread-action dispatcher.

use crate::event::{self, AppEvent, EventBus};
use crate::external_agent;
use crate::project::{self, Project};
use crate::types;
use crate::{context_rewind, fission_ledger, fission_lifecycle, platform, provider, worktree};
use crate::{
    drain_external_agent_events, emit_child_turn_complete, emit_child_turn_complete_for_session,
    emit_context_rewind_failure, emit_external_session_identity, external_agent_log_source,
    external_tool_failure_content, external_tool_preview_text,
    inspect_context_rewind_anchor_from_rollout, list_context_rewind_anchors_from_rollout,
    resolve_managed_context_edit_branch_target, scan_context_rewind_anchor_catalog,
    truncate_string_copy, ContextRewindAnchorCatalogEntry, DrainOutcome, ExternalDiffDeltaTracker,
    LoopStats, PendingRuntimeSteer, UserTurnRevisionState,
};
use crate::{slog, DrainConfig};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub(crate) struct CodexThreadActionDedupe {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl CodexThreadActionDedupe {
    const MAX_SEEN: usize = 512;

    pub(crate) fn mark_seen(&mut self, request_id: &str) -> bool {
        if self.seen.contains(request_id) {
            return false;
        }
        let request_id = request_id.to_string();
        self.seen.insert(request_id.clone());
        self.order.push_back(request_id);
        while self.order.len() > Self::MAX_SEEN {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }
}

pub(crate) fn forked_thread_id_from_message(message: &str) -> Option<String> {
    message
        .strip_prefix("forked into thread ")
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != "(unknown)")
        .map(str::to_string)
}

pub(crate) enum ExternalThreadActionEffect {
    None,
    SideTurnStarted {
        parent_thread_id: String,
        child_thread_id: String,
        prompt: Option<String>,
    },
    SideTurnClosed {
        child_thread_id: String,
    },
}

pub(crate) fn side_thread_ids_from_message(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix("side conversation started in thread ")?;
    let (child, parent) = rest.split_once(" from parent ")?;
    let child = child.trim();
    let parent = parent.trim();
    if child.is_empty() || parent.is_empty() {
        return None;
    }
    Some((parent.to_string(), child.to_string()))
}

pub(crate) fn fork_session_name_from_params(params: &serde_json::Value) -> Option<String> {
    params
        .get("name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

pub(crate) fn codex_thread_rename_name_from_result(
    params: &serde_json::Value,
    message: &str,
) -> Option<String> {
    fork_session_name_from_params(params).or_else(|| {
        message
            .strip_prefix("Codex thread renamed to ")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
}

pub(crate) fn persist_codex_thread_rename_overlay(
    home: &Path,
    session_id: Option<&str>,
    params: &serde_json::Value,
    message: &str,
) -> Result<Option<String>, String> {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let Some(name) = codex_thread_rename_name_from_result(params, message) else {
        return Ok(None);
    };
    crate::session_names::rename_session(home, "codex", session_id, &name).map(Some)
}

pub(crate) fn codex_thread_action_capabilities() -> Vec<String> {
    [
        "compact",
        "fast",
        "fork",
        "side",
        "side-close",
        "undo",
        "review",
        "rename",
        "goal",
        "goal-set",
        "goal-edit",
        "goal-get",
        "goal-clear",
        "goal-pause",
        "goal-resume",
        "goal-complete",
        "goal-budget-limited",
        "memory-reset",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub(crate) fn codex_service_tier_is_fast(service_tier: Option<&str>) -> bool {
    service_tier
        .map(str::trim)
        .filter(|tier| !tier.is_empty())
        .is_some_and(|tier| {
            tier.eq_ignore_ascii_case(external_agent::codex::CODEX_FAST_SERVICE_TIER)
                || tier.eq_ignore_ascii_case("fast")
        })
}

pub(crate) fn codex_service_tier_value(service_tier: Option<&str>) -> Option<String> {
    project::normalize_codex_service_tier(service_tier)
        .filter(|tier| !project::codex_service_tier_is_standard_clear(tier))
}

pub(crate) fn codex_external_session_capabilities(
    project: &Project,
    service_tier: Option<&str>,
) -> types::SessionCapabilities {
    types::SessionCapabilities {
        follow_up: true,
        steer: true,
        interrupt: true,
        thread_actions: codex_thread_action_capabilities(),
        codex_thread_actions: codex_thread_action_capabilities(),
        codex_managed_context: Some(project::normalize_codex_managed_context(
            &project.config.agent.codex.managed_context,
        )),
        codex_sandbox: Some(project::normalize_sandbox_mode(
            &project.config.agent.codex.sandbox,
        )),
        codex_approval_policy: Some(project::normalize_approval_policy(
            &project.config.agent.codex.approval_policy,
        )),
        codex_context_archive: Some(project::normalize_codex_context_archive(
            &project.config.agent.codex.context_archive,
        )),
        // Report what this session actually spawns: managed sessions
        // resolve to `managed_command` (the Intendant-aware fork) when set.
        codex_command: Some(project.config.agent.codex.effective_command(
            project::codex_managed_context_enabled(&project.config.agent.codex.managed_context),
        )),
        codex_fast_mode: Some(codex_service_tier_is_fast(service_tier)),
        codex_service_tier: codex_service_tier_value(service_tier),
    }
}

pub(crate) fn emit_codex_session_capabilities_for_project(
    bus: &EventBus,
    session_id: Option<&str>,
    project: &Project,
    service_tier: Option<&str>,
) {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::SessionCapabilities {
        session_id: session_id.to_string(),
        capabilities: codex_external_session_capabilities(project, service_tier),
    });
}

pub(crate) fn codex_drain_session_capabilities(
    config: &DrainConfig<'_>,
    service_tier: Option<&str>,
) -> types::SessionCapabilities {
    let launch = crate::session_config::read_log_dir_config(config.log_dir);
    let effective_service_tier = service_tier.or_else(|| {
        launch
            .as_ref()
            .and_then(|cfg| cfg.codex_service_tier.as_deref())
    });
    types::SessionCapabilities {
        follow_up: true,
        steer: true,
        interrupt: true,
        thread_actions: codex_thread_action_capabilities(),
        codex_thread_actions: codex_thread_action_capabilities(),
        codex_managed_context: launch
            .as_ref()
            .and_then(|cfg| cfg.codex_managed_context.clone()),
        codex_sandbox: launch.as_ref().and_then(|cfg| cfg.codex_sandbox.clone()),
        codex_approval_policy: launch
            .as_ref()
            .and_then(|cfg| cfg.codex_approval_policy.clone()),
        codex_context_archive: launch
            .as_ref()
            .and_then(|cfg| cfg.codex_context_archive.clone()),
        codex_command: launch.as_ref().and_then(|cfg| cfg.agent_command.clone()),
        codex_fast_mode: Some(codex_service_tier_is_fast(effective_service_tier)),
        codex_service_tier: codex_service_tier_value(effective_service_tier),
    }
}

pub(crate) fn persist_codex_service_tier_for_drain(
    config: &DrainConfig<'_>,
    session_id: Option<&str>,
    service_tier: Option<&str>,
) {
    let mut launch = crate::session_config::read_log_dir_config(config.log_dir).unwrap_or_default();
    if launch.source.is_none() {
        launch.source = Some("codex".to_string());
    }
    launch.codex_service_tier = codex_service_tier_value(service_tier);
    if let Err(e) = crate::session_config::write_log_dir_config(config.log_dir, &launch) {
        slog(config.session_log, |l| {
            l.debug(&format!("Persist Codex service tier failed: {e}"))
        });
    }

    let home = crate::platform::home_dir();
    for id in [
        session_id,
        config.session_id.as_deref(),
        config.alias_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|id| !id.is_empty())
    {
        if let Err(e) = crate::session_config::write_external_overlay(&home, "codex", id, &launch) {
            slog(config.session_log, |l| {
                l.debug(&format!(
                    "Persist Codex service tier overlay for {} failed: {e}",
                    short_external_session_id(id)
                ))
            });
        }
    }
}

pub(crate) fn emit_codex_session_capabilities_for_drain(
    config: &DrainConfig<'_>,
    session_id: Option<&str>,
    service_tier: Option<&str>,
) {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    config.bus.send(AppEvent::SessionCapabilities {
        session_id: session_id.to_string(),
        capabilities: codex_drain_session_capabilities(config, service_tier),
    });
}

/// The `goal*` op family served by the shared wrapper-level
/// `external_agent::GoalEngine` — advertised identically by every host
/// that runs the engine (the Claude Code adapter, the native session
/// loop) so frontends see one goal dialect.
pub(crate) const GOAL_THREAD_ACTION_OPS: [&str; 9] = [
    "goal",
    "goal-set",
    "goal-edit",
    "goal-get",
    "goal-clear",
    "goal-pause",
    "goal-resume",
    "goal-complete",
    "goal-budget-limited",
];

pub(crate) fn goal_thread_action_op(op: &str) -> bool {
    op == "goal" || op.starts_with("goal-")
}

/// Budget currency for goal engines: fresh tokens = uncached input +
/// output. Cache reads are excluded so a budget measures real work, not
/// re-reads — the same currency the Claude Code wrapper engine uses.
pub(crate) fn goal_fresh_tokens(usage: &provider::TokenUsage) -> u64 {
    usage
        .prompt_tokens
        .saturating_sub(usage.cached_tokens)
        .saturating_add(usage.completion_tokens)
}

/// Thread actions the Claude Code adapter supports: `compact` dispatches a
/// native `/compact` user message through `ClaudeCodeAgent::thread_action`;
/// `fork` respawns a resumed process with `--fork-session` via the drain's
/// `ForkHandling::RespawnResume` path; `side` (`/btw`) rides the same
/// respawn with the side boundary + question as the child's first prompt
/// (CC's native `/btw` is interactive-only — over stream-json the CLI
/// answers with a synthetic "isn't available in this environment" result,
/// probed on 2.1.206); the `goal*` family runs the wrapper-level goal
/// engine (adapter-owned state riding the universal
/// `GoalUpdated`/`GoalCleared` rails); `model` / `permission-mode`
/// reconfigure the RUNNING process live via `set_model` /
/// `set_permission_mode` control requests (verified on CC 2.1.201).
pub(crate) fn claude_code_thread_action_capabilities() -> Vec<String> {
    ["compact", "fork", "side"]
        .into_iter()
        .chain(GOAL_THREAD_ACTION_OPS)
        .chain(["model", "permission-mode"])
        .map(str::to_string)
        .collect()
}

/// Capabilities of the primary NATIVE session: follow-up turns, mid-turn
/// steering (the context-injection queue), interrupts, and the `goal*`
/// family served by the shared `GoalEngine` running in the presence loop.
/// The Codex-specific knobs stay unset so frontends keep those controls
/// hidden.
pub(crate) fn native_session_capabilities() -> types::SessionCapabilities {
    types::SessionCapabilities {
        follow_up: true,
        steer: true,
        interrupt: true,
        thread_actions: GOAL_THREAD_ACTION_OPS
            .into_iter()
            .map(str::to_string)
            .collect(),
        codex_thread_actions: Vec::new(),
        codex_managed_context: None,
        codex_sandbox: None,
        codex_approval_policy: None,
        codex_context_archive: None,
        codex_command: None,
        codex_fast_mode: None,
        codex_service_tier: None,
    }
}

pub(crate) fn emit_native_session_capabilities(bus: &EventBus, session_id: Option<&str>) {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::SessionCapabilities {
        session_id: session_id.to_string(),
        capabilities: native_session_capabilities(),
    });
}

/// Capabilities of a supervised Claude Code session: follow-up turns,
/// native mid-turn steering (a user message written mid-turn is absorbed
/// into the running turn), and native interrupts (stream-json
/// `control_request`). The Codex-specific knobs stay unset so frontends
/// keep those controls hidden.
pub(crate) fn claude_code_external_session_capabilities() -> types::SessionCapabilities {
    types::SessionCapabilities {
        follow_up: true,
        steer: true,
        interrupt: true,
        thread_actions: claude_code_thread_action_capabilities(),
        codex_thread_actions: Vec::new(),
        codex_managed_context: None,
        codex_sandbox: None,
        codex_approval_policy: None,
        codex_context_archive: None,
        codex_command: None,
        codex_fast_mode: None,
        codex_service_tier: None,
    }
}

pub(crate) fn emit_claude_code_session_capabilities(bus: &EventBus, session_id: Option<&str>) {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::SessionCapabilities {
        session_id: session_id.to_string(),
        capabilities: claude_code_external_session_capabilities(),
    });
}

pub(crate) fn side_session_prompt_from_params(params: &serde_json::Value) -> Option<String> {
    ["prompt", "message", "text", "task"]
        .iter()
        .find_map(|key| params.get(*key).and_then(|v| v.as_str()))
        .or_else(|| params.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn side_child_thread_id_from_params(params: &serde_json::Value) -> Option<String> {
    params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Ops served by the respawn path on `ForkHandling::RespawnResume`
/// backends: `fork` respawns bare; `side`/`btw` respawns with the side
/// boundary + question as the child's first prompt.
pub(crate) fn respawn_resume_thread_action_op(op: &str) -> bool {
    matches!(op, "fork" | "side" | "btw")
}

/// Marker line separating the side-conversation contract from the
/// operator's question in a respawned side child's first prompt.
/// `side_respawn_display_task` splits on it to recover the bare question
/// for display surfaces; the child still receives the full prologue.
const SIDE_QUESTION_MARKER: &str = "The side question:";

/// Prologue of a respawned side conversation's first prompt: the
/// backend-neutral side contract (shared verbatim with Codex's in-process
/// side threads) plus the question marker.
fn side_respawn_prologue() -> String {
    format!(
        "{}\n\n{SIDE_QUESTION_MARKER}\n",
        external_agent::SIDE_CONVERSATION_CONTRACT
    )
}

/// First prompt of a respawned side conversation: contract + question.
pub(crate) fn side_respawn_prompt(question: &str) -> String {
    format!("{}{question}", side_respawn_prologue())
}

/// Recover the bare side question from a respawn-composed first prompt, for
/// display surfaces (session meta, `SessionStarted`). `None` when the task
/// is not one — the match is exact-prefix, so an arbitrary task can never
/// be mistaken for a side prompt.
pub(crate) fn side_respawn_display_task(task: &str) -> Option<String> {
    task.strip_prefix(&side_respawn_prologue())
        .map(|question| question.trim().to_string())
        .filter(|question| !question.is_empty())
}

/// Build the shared respawn `ControlMsg` for `fork`/`side` on backends
/// without an in-process fork, returning `(success, outcome message)`.
/// Both dispatch sites (the drain handler and the presence loop's inline
/// mirror) call this so the two stay in sync by construction.
pub(crate) fn respawn_resume_thread_action(
    bus: &EventBus,
    agent_name: &str,
    thread_id: Option<String>,
    op: &str,
    params: &serde_json::Value,
    project_root: &std::path::Path,
    agent_command: Option<String>,
) -> (bool, String) {
    let Some(parent_thread_id) = thread_id else {
        return (
            false,
            format!("{op} needs a native session id — run a turn in this session first"),
        );
    };
    let side = op != "fork";
    let task = if side {
        let Some(prompt) = side_session_prompt_from_params(params) else {
            return (false, format!("Usage: /{op} <question>"));
        };
        Some(side_respawn_prompt(&prompt))
    } else {
        None
    };
    bus.send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
        source: agent_name.to_string(),
        session_id: parent_thread_id.clone(),
        resume_id: Some(parent_thread_id.clone()),
        project_root: Some(project_root.to_string_lossy().to_string()),
        task,
        direct: Some(true),
        attachments: Vec::new(),
        fork: true,
        relationship_kind: side.then(|| "side".to_string()),
        agent_command,
        codex_sandbox: None,
        codex_approval_policy: None,
        codex_managed_context: None,
        codex_context_archive: None,
    }));
    let message = if side {
        format!(
            "side conversation forking thread {} — it answers in its own session window",
            short_external_session_id(&parent_thread_id)
        )
    } else {
        format!(
            "forking thread {} — the fork announces its own session id on its first turn",
            short_external_session_id(&parent_thread_id)
        )
    };
    (true, message)
}

pub(crate) fn is_context_rewind_backout_action(op: &str) -> bool {
    matches!(
        op,
        "rewind-backout"
            | "rewind_backout"
            | "rewind-inspect"
            | "rewind_inspect"
            | "rewind-restore"
            | "rewind_restore"
            | "context-rewind-backout"
            | "context_rewind_backout"
            | "context-rewind-restore"
            | "context_rewind_restore"
    )
}

pub(crate) fn is_context_rewind_anchor_list_action(op: &str) -> bool {
    matches!(
        op,
        "list_rewind_anchors"
            | "list-rewind-anchors"
            | "rewind_anchors"
            | "rewind-anchors"
            | "context_rewind_anchors"
            | "context-rewind-anchors"
    )
}

pub(crate) fn is_context_rewind_anchor_inspect_action(op: &str) -> bool {
    matches!(
        op,
        "inspect_rewind_anchor"
            | "inspect-rewind-anchor"
            | "rewind_anchor_inspect"
            | "rewind-anchor-inspect"
            | "context_rewind_anchor_inspect"
            | "context-rewind-anchor-inspect"
    )
}

pub(crate) fn context_rewind_record_id_from_params(params: &serde_json::Value) -> Option<String> {
    params
        .get("recordId")
        .or_else(|| params.get("record_id"))
        .or_else(|| params.get("id"))
        .and_then(|value| value.as_str())
        .or_else(|| params.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(crate) fn context_rewind_backout_mode(op: &str, params: &serde_json::Value) -> String {
    if let Some(mode) = params
        .get("mode")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
    {
        return mode.to_string();
    }
    match op {
        "rewind-restore"
        | "rewind_restore"
        | "context-rewind-restore"
        | "context_rewind_restore" => "restore".to_string(),
        "rewind-inspect" | "rewind_inspect" => "inspect".to_string(),
        _ => "inspect".to_string(),
    }
}

#[cfg(test)]
pub(crate) fn context_rewind_allows_cache_reset(params: &serde_json::Value) -> bool {
    [
        "allowCacheReset",
        "allow_cache_reset",
        "allowCacheBreakingFork",
        "allow_cache_breaking_fork",
    ]
    .iter()
    .any(|key| {
        params
            .get(*key)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    })
}

pub(crate) async fn fork_managed_context_edit_branch(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread_id: &str,
    user_turn_index: u32,
    original_text: Option<&str>,
    replacement_text: String,
    unresolved_attachment_ids: Vec<String>,
    config: &DrainConfig<'_>,
) -> Result<Option<String>, String> {
    let Some(target) = resolve_managed_context_edit_branch_target(
        config.log_dir,
        thread_id,
        config.session_id.as_deref(),
        user_turn_index,
        original_text,
    )?
    else {
        return Ok(None);
    };
    let turns_to_drop = target
        .source_turn_count
        .saturating_sub(user_turn_index)
        .saturating_add(1);
    let name = format!(
        "Edit turn {} from {}",
        user_turn_index,
        short_external_session_id(&target.parent_thread_id)
    );
    let child = agent
        .fork_thread_from_rollout_path(&target.recovery_rollout_path, Some(&name))
        .await
        .map_err(|e| {
            format!(
                "failed to fork archived managed-context rollout {} for edit: {e}",
                target.record_id
            )
        })?;
    agent
        .rollback_thread_turns(&child.thread_id, turns_to_drop)
        .await
        .map_err(|e| {
            format!(
                "failed to roll back edit branch {} to before user turn {}: {e}",
                child.thread_id, user_turn_index
            )
        })?;

    config
        .bus
        .send(AppEvent::ControlCommand(event::ControlMsg::RenameSession {
            source: Some("codex".to_string()),
            session_id: child.thread_id.clone(),
            backend_session_id: Some(child.thread_id.clone()),
            name: name.clone(),
        }));
    emit_session_relationship(
        config.bus,
        Some(target.parent_thread_id.as_str()),
        &child.thread_id,
        "managed-edit-branch",
        false,
    );
    let launch = crate::session_config::read_log_dir_config(config.log_dir);
    config
        .bus
        .send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
            source: "codex".to_string(),
            session_id: child.thread_id.clone(),
            resume_id: Some(child.thread_id.clone()),
            project_root: Some(config.project_root.to_string_lossy().to_string()),
            task: Some(replacement_text),
            direct: Some(true),
            fork: false,
            relationship_kind: None,
            attachments: unresolved_attachment_ids,
            agent_command: launch.as_ref().and_then(|cfg| cfg.agent_command.clone()),
            codex_sandbox: launch.as_ref().and_then(|cfg| cfg.codex_sandbox.clone()),
            codex_approval_policy: launch
                .as_ref()
                .and_then(|cfg| cfg.codex_approval_policy.clone()),
            codex_managed_context: launch
                .as_ref()
                .and_then(|cfg| cfg.codex_managed_context.clone()),
            codex_context_archive: launch
                .as_ref()
                .and_then(|cfg| cfg.codex_context_archive.clone()),
        }));

    Ok(Some(format!(
        "created managed edit branch {} from rewind record {} at user turn {} (dropped {} archived turn{} before replay)",
        child.thread_id,
        target.record_id,
        user_turn_index,
        turns_to_drop,
        if turns_to_drop == 1 { "" } else { "s" }
    )))
}

pub(crate) fn thread_id_from_action_params(params: &serde_json::Value) -> Option<String> {
    params
        .pointer("/thread/id")
        .and_then(|value| value.as_str())
        .or_else(|| params.pointer("/threadId").and_then(|value| value.as_str()))
        .or_else(|| {
            params
                .pointer("/thread_id")
                .and_then(|value| value.as_str())
        })
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(crate) fn thread_action_params_with_thread_id(
    op: &str,
    params: serde_json::Value,
    thread_id: Option<&str>,
) -> serde_json::Value {
    if thread_id_from_action_params(&params).is_some() {
        return params;
    }

    let Some(thread_id) = thread_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return params;
    };

    match params {
        serde_json::Value::Object(mut obj) => {
            obj.insert(
                "threadId".to_string(),
                serde_json::Value::String(thread_id.to_string()),
            );
            serde_json::Value::Object(obj)
        }
        serde_json::Value::Null => serde_json::json!({ "threadId": thread_id }),
        serde_json::Value::String(prompt) if matches!(op, "side" | "btw") => {
            serde_json::json!({
                "threadId": thread_id,
                "prompt": prompt,
            })
        }
        other => other,
    }
}

pub(crate) fn thread_action_params_for_target(
    op: &str,
    params: serde_json::Value,
    target_session_id: &Option<String>,
    config: &DrainConfig<'_>,
) -> serde_json::Value {
    if thread_id_from_action_params(&params).is_some() {
        return params;
    }

    let target = target_session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .or(config.session_id.as_deref());
    let thread_id = target.map(|target| {
        let names_this_conversation = config.session_id.as_deref() == Some(target)
            || config.alias_session_id.as_deref() == Some(target);
        if names_this_conversation {
            // The action targets this conversation (by either of its names);
            // the backend thread id is authoritative when the caller supplied
            // it. Without one, fall back to the legacy alias→session mapping
            // (correct for paths where `session_id` is the thread id).
            if let Some(backend_thread_id) = config.backend_thread_id.as_deref() {
                backend_thread_id
            } else if config.alias_session_id.as_deref() == Some(target) {
                config.session_id.as_deref().unwrap_or(target)
            } else {
                target
            }
        } else {
            target
        }
    });

    thread_action_params_with_thread_id(op, params, thread_id)
}

pub(crate) fn emit_session_relationship(
    bus: &EventBus,
    parent_session_id: Option<&str>,
    child_session_id: &str,
    relationship: &str,
    ephemeral: bool,
) {
    let Some(parent_session_id) = parent_session_id.map(str::trim).filter(|id| !id.is_empty())
    else {
        return;
    };
    if parent_session_id == child_session_id {
        return;
    }
    bus.send(AppEvent::SessionRelationship {
        parent_session_id: parent_session_id.to_string(),
        child_session_id: child_session_id.to_string(),
        relationship: relationship.to_string(),
        ephemeral,
    });
}

pub(crate) fn emit_codex_fork_session_name(
    bus: &EventBus,
    child_id: &str,
    params: &serde_json::Value,
) {
    let Some(name) = fork_session_name_from_params(params) else {
        return;
    };
    bus.send(AppEvent::ControlCommand(event::ControlMsg::RenameSession {
        source: Some("codex".to_string()),
        session_id: child_id.to_string(),
        backend_session_id: Some(child_id.to_string()),
        name,
    }));
}

pub(crate) async fn apply_context_rewind_backout_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    op: &str,
    params: &serde_json::Value,
    config: &DrainConfig<'_>,
) -> Result<String, String> {
    let mode = context_rewind_backout_mode(op, params).to_ascii_lowercase();
    let restore = mode == "restore";
    if !matches!(mode.as_str(), "inspect" | "fork" | "backout" | "restore") {
        return Err(format!(
            "context rewind backout mode `{mode}` is not supported; use `inspect`, `fork`, or `restore`"
        ));
    }
    let record_id = context_rewind_record_id_from_params(params)
        .ok_or_else(|| "context rewind backout requires recordId".to_string())?;
    let record = context_rewind::read_record(config.log_dir, &record_id)
        .map_err(|e| format!("failed to read context rewind record {record_id}: {e}"))?;
    let recovery_rollout_path = record
        .recovery_rollout_path
        .as_deref()
        .ok_or_else(|| format!("context rewind record {record_id} has no recovery rollout"))?;
    if mode == "inspect" {
        let source = record
            .source_rollout_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        return Ok(format!(
            "context rewind record {record_id}: pre-rewind rollout copied from {source} to {}; restore uses same-thread Codex thread/restore when available; fork/backout creates a new Codex thread that inherits the lineage prompt-cache key when using the patched managed Codex binary",
            recovery_rollout_path.display()
        ));
    }
    if restore {
        // Restore must target the exact Codex thread that was rewound. That is
        // `record.thread_id` (captured from the thread snapshot at rewind time),
        // never the Intendant session id/alias that `thread_action_params_for_target`
        // may have injected into `params`, nor `record.session_id` (the Intendant id).
        let target_thread_id = Some(record.thread_id.clone())
            .filter(|id| !id.trim().is_empty())
            .or_else(|| thread_id_from_action_params(params))
            .or_else(|| record.session_id.clone())
            .ok_or_else(|| {
                format!("context rewind record {record_id} has no thread to restore into")
            })?;
        agent
            .restore_thread_from_rollout_path(
                &target_thread_id,
                recovery_rollout_path,
                Some(record_id.as_str()),
            )
            .await
            .map_err(|e| {
                format!("failed to restore recovery rollout into thread {target_thread_id}: {e}")
            })?;
        // Record the restore in the durable lineage ledger so the TUI/dashboard can
        // see that previously pruned context was reintroduced into this thread. This
        // is a same-thread restore (parent == child), so emit the rewind-restore edge
        // directly — the guarded `emit_session_relationship` helper drops self-edges.
        config.bus.send(AppEvent::SessionRelationship {
            parent_session_id: target_thread_id.clone(),
            child_session_id: target_thread_id.clone(),
            relationship: "rewind-restore".to_string(),
            ephemeral: false,
        });
        return Ok(format!(
            "restored context rewind record {} into existing Codex thread {}",
            record_id, target_thread_id
        ));
    }
    let default_name = if restore {
        format!("Rewind restore {}", record_id)
    } else {
        format!("Rewind backout {}", record_id)
    };
    let name = fork_session_name_from_params(params).unwrap_or(default_name);
    let child = agent
        .fork_thread_from_rollout_path(recovery_rollout_path, Some(&name))
        .await
        .map_err(|e| format!("failed to fork recovery rollout: {e}"))?;

    config
        .bus
        .send(AppEvent::ControlCommand(event::ControlMsg::RenameSession {
            source: Some("codex".to_string()),
            session_id: child.thread_id.clone(),
            backend_session_id: Some(child.thread_id.clone()),
            name,
        }));
    emit_session_relationship(
        config.bus,
        Some(record.thread_id.as_str()),
        &child.thread_id,
        if restore {
            "rewind-restore"
        } else {
            "rewind-backout"
        },
        false,
    );
    config
        .bus
        .send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
            source: "codex".to_string(),
            session_id: child.thread_id.clone(),
            resume_id: Some(child.thread_id.clone()),
            project_root: Some(config.project_root.to_string_lossy().to_string()),
            task: None,
            direct: Some(true),
            fork: false,
            relationship_kind: None,
            attachments: Vec::new(),
            agent_command: crate::session_config::read_log_dir_config(config.log_dir)
                .and_then(|cfg| cfg.agent_command),
            codex_sandbox: crate::session_config::read_log_dir_config(config.log_dir)
                .and_then(|cfg| cfg.codex_sandbox),
            codex_approval_policy: crate::session_config::read_log_dir_config(config.log_dir)
                .and_then(|cfg| cfg.codex_approval_policy),
            codex_managed_context: crate::session_config::read_log_dir_config(config.log_dir)
                .and_then(|cfg| cfg.codex_managed_context),
            codex_context_archive: crate::session_config::read_log_dir_config(config.log_dir)
                .and_then(|cfg| cfg.codex_context_archive),
        }));

    Ok(format!(
        "forked context rewind record {} with inherited lineage prompt-cache key into thread {}",
        record_id, child.thread_id
    ))
}

pub(crate) fn is_fission_spawn_action(op: &str) -> bool {
    matches!(op, "fission_spawn" | "fission-spawn")
}

pub(crate) fn is_fission_import_action(op: &str) -> bool {
    matches!(op, "fission_import" | "fission-import")
}

/// Most branches a single `fission_spawn` call may launch.
pub(crate) const FISSION_SPAWN_MAX_BRANCHES: usize = 4;

/// One branch request inside a `fission_spawn` thread action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FissionSpawnBranchSpec {
    objective: String,
    write_scope: Vec<String>,
    name: Option<String>,
}

pub(crate) fn fission_spawn_branch_specs_from_params(
    params: &serde_json::Value,
) -> Result<Vec<FissionSpawnBranchSpec>, String> {
    let branches = params
        .get("branches")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "fission_spawn requires a `branches` array".to_string())?;
    if branches.is_empty() || branches.len() > FISSION_SPAWN_MAX_BRANCHES {
        return Err(format!(
            "fission_spawn takes between 1 and {FISSION_SPAWN_MAX_BRANCHES} branches; got {}",
            branches.len()
        ));
    }
    let mut specs = Vec::new();
    for (index, branch) in branches.iter().enumerate() {
        let objective = branch
            .get("objective")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|objective| !objective.is_empty())
            .ok_or_else(|| {
                format!(
                    "fission_spawn branch {} requires a non-empty `objective`",
                    index + 1
                )
            })?;
        let write_scope_value = branch
            .get("write_scope")
            .or_else(|| branch.get("writeScope"));
        let write_scope: Vec<String> = match write_scope_value {
            Some(serde_json::Value::Array(entries)) => entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect(),
            // Tolerate a bare string scope; the contract shape is an array.
            Some(serde_json::Value::String(entry)) => Some(entry.trim())
                .filter(|entry| !entry.is_empty())
                .map(|entry| vec![entry.to_string()])
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let name = branch
            .get("name")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        specs.push(FissionSpawnBranchSpec {
            objective: objective.to_string(),
            write_scope,
            name,
        });
    }
    Ok(specs)
}

pub(crate) fn fission_spawn_use_worktree_override(params: &serde_json::Value) -> Option<bool> {
    params
        .get("use_worktree")
        .or_else(|| params.get("useWorktree"))
        .and_then(|value| value.as_bool())
}

pub(crate) fn fission_anchor_item_id_from_params(params: &serde_json::Value) -> Option<String> {
    params
        .get("anchor_item_id")
        .or_else(|| params.get("anchorItemId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(crate) fn fission_params_with_anchor_item_id(
    params: serde_json::Value,
    anchor_item_id: &str,
) -> serde_json::Value {
    match params {
        serde_json::Value::Object(mut obj) => {
            obj.insert(
                "anchor_item_id".to_string(),
                serde_json::Value::String(anchor_item_id.to_string()),
            );
            serde_json::Value::Object(obj)
        }
        other => other,
    }
}

/// Pick the spawn anchor from the in-flight tool items of the active turn:
/// the most recently started `mcp` tool call whose preview references
/// `fission_spawn` — i.e. the very tool call that asked for the spawn. The
/// mid-turn dispatch arm injects the winner into the action params as
/// `anchor_item_id`.
pub(crate) fn most_recent_inflight_fission_spawn_tool_item(
    active_tool_ids: &HashSet<String>,
    tool_previews: &HashMap<String, String>,
    tool_start_seq: &HashMap<String, u64>,
) -> Option<String> {
    active_tool_ids
        .iter()
        .filter(|item_id| {
            tool_previews.get(item_id.as_str()).is_some_and(|preview| {
                preview.starts_with("mcp") && preview.contains("fission_spawn")
            })
        })
        .max_by_key(|item_id| tool_start_seq.get(item_id.as_str()).copied().unwrap_or(0))
        .cloned()
}

/// A spawn anchor resolved from the parent's rollout catalog when the action
/// params carried none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FissionSpawnResolvedAnchor {
    item_id: String,
    /// True when no anchor named for `fission_spawn` existed and the catalog
    /// head (last anchor) was used instead; recorded as tool
    /// `fission_spawn:head` for honest provenance.
    head_fallback: bool,
}

pub(crate) fn fission_spawn_anchor_from_catalog(
    anchors: &[ContextRewindAnchorCatalogEntry],
) -> Option<FissionSpawnResolvedAnchor> {
    if let Some(anchor) = anchors.iter().rev().find(|anchor| {
        anchor
            .names
            .iter()
            .any(|name| name.contains("fission_spawn"))
    }) {
        return Some(FissionSpawnResolvedAnchor {
            item_id: anchor.item_id.clone(),
            head_fallback: false,
        });
    }
    anchors.last().map(|anchor| FissionSpawnResolvedAnchor {
        item_id: anchor.item_id.clone(),
        head_fallback: true,
    })
}

/// Worktree default for a fission branch: an owned write scope in a git
/// project gets an isolated checkout; an explicit `use_worktree` overrides in
/// both directions.
pub(crate) fn fission_branch_uses_worktree(
    has_write_scope: bool,
    project_root_is_git_repo: bool,
    use_worktree_override: Option<bool>,
) -> bool {
    use_worktree_override.unwrap_or(has_write_scope && project_root_is_git_repo)
}

pub(crate) fn fission_project_root_is_git_repo(project_root: &Path) -> bool {
    // Repo roots have a `.git` directory; linked worktrees a `.git` file.
    // Either way the root can host `git worktree add`.
    project_root.join(".git").exists()
}

/// Git branch name for a fission worktree: `fission/<short-group-hash>-<ordinal>`.
/// The short hash is the collision-resistant tail segment of the fission
/// group id, so sibling spawns at different anchors never collide.
pub(crate) fn fission_branch_git_name(group_id: &str, ordinal: usize) -> String {
    let hash = group_id.rsplit('-').next().unwrap_or(group_id);
    let short: String = hash.chars().take(8).collect();
    format!("fission/{short}-{ordinal}")
}

/// The `<fission_charter>` developer message injected into a freshly forked
/// branch: identity (group + branch session), mandate (objective + owned
/// write scope + worktree), and the report-back contract.
pub(crate) fn fission_charter_message(
    group_id: &str,
    branch_session_id: &str,
    objective: &str,
    write_scope: &[String],
    branch_worktree: Option<&worktree::Worktree>,
) -> String {
    let scope = if write_scope.is_empty() {
        "read-only".to_string()
    } else {
        write_scope.join(", ")
    };
    let mut out = String::from("<fission_charter>\n");
    out.push_str(&format!("group_id: {group_id}\n"));
    out.push_str(&format!("branch_session_id: {branch_session_id}\n"));
    out.push_str(&format!("objective: {objective}\n"));
    out.push_str(&format!("owned write scope: {scope}\n"));
    if let Some(wt) = branch_worktree {
        out.push_str(&format!(
            "worktree: {} (git branch {})\n",
            wt.path.display(),
            wt.branch_name
        ));
    }
    out.push_str(
        "Work only within your write scope. When done, end your turn with a concise outcome \
         summary (it becomes your ledger summary). If your result should become the group's \
         canonical outcome, call claim_fission_canonical with this group_id. Prefer the fission \
         ledger in get_status over reading sibling raw logs.\n",
    );
    out.push_str("</fission_charter>");
    out
}

/// Shared per-spawn facts threaded through the per-branch launcher.
pub(crate) struct FissionSpawnContext<'a> {
    parent_thread_id: &'a str,
    anchor_item_id: &'a str,
    group_id: &'a str,
    use_worktree_override: Option<bool>,
    project_root_is_git: bool,
    launch: Option<&'a crate::session_config::SessionAgentConfig>,
}

pub(crate) async fn apply_fission_spawn_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    params: &serde_json::Value,
    config: &DrainConfig<'_>,
) -> Result<String, String> {
    let specs = fission_spawn_branch_specs_from_params(params)?;
    let parent_thread_id = thread_id_from_action_params(params)
        .or_else(|| config.backend_thread_id.clone())
        .or_else(|| config.alias_session_id.clone())
        .or_else(|| config.session_id.clone())
        .ok_or_else(|| "fission_spawn requires a parent thread id".to_string())?;

    // Anchor resolution order: explicit `anchor_item_id` (the mid-turn
    // dispatch arm injects the in-flight fission_spawn tool item) → newest
    // rollout anchor named for fission_spawn → catalog head with honest
    // `fission_spawn:head` provenance.
    let (anchor_item_id, head_fallback) = match fission_anchor_item_id_from_params(params) {
        Some(item_id) => (item_id, false),
        None => {
            let snapshot = agent
                .read_thread_snapshot(&parent_thread_id)
                .await
                .map_err(|e| {
                    format!("failed to read parent thread metadata for fission spawn: {e}")
                })?;
            let rollout_path = snapshot.rollout_path.ok_or_else(|| {
                "parent thread metadata did not include a rollout path to anchor the fission group"
                    .to_string()
            })?;
            let anchors = scan_context_rewind_anchor_catalog(&rollout_path)
                .map_err(|e| format!("failed to scan rollout anchors for fission spawn: {e}"))?;
            let resolved = fission_spawn_anchor_from_catalog(&anchors).ok_or_else(|| {
                "parent thread rollout has no anchors to attach a fission group to".to_string()
            })?;
            (resolved.item_id, resolved.head_fallback)
        }
    };

    let group_id = fission_ledger::group_id(&parent_thread_id, &anchor_item_id);
    let group_tool = if head_fallback {
        "fission_spawn:head"
    } else {
        "fission_spawn"
    };
    // Stamp the group's tool provenance before registering branches;
    // `register_spawned_branch` keys into the same `(parent, anchor)` group
    // and leaves an existing group's tool untouched.
    fission_ledger::record_fission_observation(
        config.log_dir,
        fission_ledger::FissionObservation {
            parent_session_id: parent_thread_id.clone(),
            anchor_item_id: anchor_item_id.clone(),
            tool: group_tool.to_string(),
            status: "running".to_string(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            branches: Vec::new(),
        },
    )
    .map_err(|e| format!("failed to record fission group {group_id}: {e}"))?;

    let launch = crate::session_config::read_log_dir_config(config.log_dir);
    let ctx = FissionSpawnContext {
        parent_thread_id: &parent_thread_id,
        anchor_item_id: &anchor_item_id,
        group_id: &group_id,
        use_worktree_override: fission_spawn_use_worktree_override(params),
        project_root_is_git: fission_project_root_is_git_repo(config.project_root),
        launch: launch.as_ref(),
    };
    let mut results = Vec::new();
    let mut spawned = 0usize;
    for (index, spec) in specs.iter().enumerate() {
        let ordinal = index + 1;
        match spawn_single_fission_branch(agent, config, &ctx, ordinal, spec).await {
            Ok(line) => {
                spawned += 1;
                results.push(format!("branch {ordinal}: {line}"));
            }
            Err(err) => results.push(format!("branch {ordinal}: FAILED — {err}")),
        }
    }
    let message = format!(
        "fission group {group_id} (anchor {anchor_item_id}): spawned {spawned}/{} branch(es)\n{}",
        specs.len(),
        results.join("\n"),
    );
    if spawned == 0 {
        Err(message)
    } else {
        Ok(message)
    }
}

/// Launch one fission branch: optional worktree → live-thread fork → charter
/// injection → durable ledger registration → lifecycle route → frontend
/// wiring (rename + relationship + resume with the kickoff task). Any failed
/// step removes the worktree this branch created, so a partial spawn leaves
/// no orphaned checkouts.
pub(crate) async fn spawn_single_fission_branch(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
    ctx: &FissionSpawnContext<'_>,
    ordinal: usize,
    spec: &FissionSpawnBranchSpec,
) -> Result<String, String> {
    let use_worktree = fission_branch_uses_worktree(
        !spec.write_scope.is_empty(),
        ctx.project_root_is_git,
        ctx.use_worktree_override,
    );
    let branch_worktree = if use_worktree {
        Some(
            worktree::create(
                config.project_root,
                &fission_branch_git_name(ctx.group_id, ordinal),
                "HEAD",
            )
            .map_err(|e| format!("worktree creation failed: {e}"))?,
        )
    } else {
        None
    };
    let cleanup_worktree = |wt: &Option<worktree::Worktree>| -> String {
        if let Some(wt) = wt {
            if let Err(err) = worktree::remove_worktree_and_branch(config.project_root, wt) {
                return format!(
                    "; cleanup of worktree {} also failed: {err}",
                    wt.path.display()
                );
            }
        }
        String::new()
    };

    let display_name = spec.name.clone().unwrap_or_else(|| {
        format!(
            "Fission {}: {}",
            ordinal,
            truncate_string_copy(&spec.objective, 48)
        )
    });
    let child = match agent
        .fork_thread_with_options(
            ctx.parent_thread_id,
            Some(&display_name),
            branch_worktree.as_ref().map(|wt| wt.path.as_path()),
        )
        .await
    {
        Ok(child) => child,
        Err(e) => {
            let cleanup = cleanup_worktree(&branch_worktree);
            return Err(format!("live-thread fork failed: {e}{cleanup}"));
        }
    };

    let charter = fission_charter_message(
        ctx.group_id,
        &child.thread_id,
        &spec.objective,
        &spec.write_scope,
        branch_worktree.as_ref(),
    );
    if let Err(e) = agent
        .inject_thread_developer_message(&child.thread_id, &charter)
        .await
    {
        let cleanup = cleanup_worktree(&branch_worktree);
        return Err(format!(
            "charter injection into forked thread {} failed: {e}{cleanup}",
            child.thread_id
        ));
    }

    let kickoff = format!("Begin your fission charter: {}", spec.objective);
    if let Err(err) = fission_ledger::register_spawned_branch(
        config.log_dir,
        ctx.parent_thread_id,
        ctx.anchor_item_id,
        fission_ledger::BranchCharter {
            objective: spec.objective.clone(),
            write_scope: (!spec.write_scope.is_empty()).then(|| spec.write_scope.join(", ")),
            worktree_requested: use_worktree,
        },
        fission_ledger::NewSpawnedBranch {
            session_id: child.thread_id.clone(),
            backend_session_id: Some(child.thread_id.clone()),
            worktree_path: branch_worktree.as_ref().map(|wt| wt.path.clone()),
            task: Some(kickoff.clone()),
            ..Default::default()
        },
    ) {
        let cleanup = cleanup_worktree(&branch_worktree);
        return Err(format!(
            "fission ledger registration for forked thread {} failed: {err}{cleanup}",
            child.thread_id
        ));
    }
    fission_lifecycle::register_branch(&child.thread_id, ctx.group_id, config.log_dir);

    config
        .bus
        .send(AppEvent::ControlCommand(event::ControlMsg::RenameSession {
            source: Some("codex".to_string()),
            session_id: child.thread_id.clone(),
            backend_session_id: Some(child.thread_id.clone()),
            name: display_name,
        }));
    emit_session_relationship(
        config.bus,
        Some(ctx.parent_thread_id),
        &child.thread_id,
        "fission-branch",
        false,
    );
    let branch_project_root = branch_worktree
        .as_ref()
        .map(|wt| wt.path.as_path())
        .unwrap_or(config.project_root);
    config
        .bus
        .send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
            source: "codex".to_string(),
            session_id: child.thread_id.clone(),
            resume_id: Some(child.thread_id.clone()),
            project_root: Some(branch_project_root.to_string_lossy().to_string()),
            task: Some(kickoff),
            direct: Some(true),
            fork: false,
            relationship_kind: None,
            attachments: Vec::new(),
            agent_command: ctx.launch.and_then(|cfg| cfg.agent_command.clone()),
            codex_sandbox: ctx.launch.and_then(|cfg| cfg.codex_sandbox.clone()),
            codex_approval_policy: ctx.launch.and_then(|cfg| cfg.codex_approval_policy.clone()),
            codex_managed_context: ctx.launch.and_then(|cfg| cfg.codex_managed_context.clone()),
            codex_context_archive: ctx.launch.and_then(|cfg| cfg.codex_context_archive.clone()),
        }));

    let location = branch_worktree
        .as_ref()
        .map(|wt| {
            format!(
                " in worktree {} (git branch {})",
                wt.path.display(),
                wt.branch_name
            )
        })
        .unwrap_or_default();
    Ok(format!(
        "spawned thread {}{} — {}",
        child.thread_id, location, spec.objective
    ))
}

/// Compact developer-message payload for `fission_import`: everything the
/// parent needs to fold a branch's outcome into its continuation without
/// reading the sibling's raw log.
pub(crate) fn fission_import_payload(
    group: &fission_ledger::FissionGroup,
    branch: &fission_ledger::FissionBranch,
    branch_ext: Option<&fission_ledger::FissionBranchExt>,
) -> String {
    let objective = branch_ext
        .and_then(|ext| ext.charter.as_ref())
        .map(|charter| charter.objective.clone())
        .or_else(|| branch.task.clone())
        .or_else(|| group.objective.clone())
        .unwrap_or_else(|| "(none recorded)".to_string());
    let mut out = String::from("<fission_import>\n");
    out.push_str(&format!("group_id: {}\n", group.group_id));
    out.push_str(&format!("branch_session_id: {}\n", branch.session_id));
    out.push_str(&format!("objective: {objective}\n"));
    out.push_str(&format!(
        "status: {}\n",
        fission_ledger::normalize_branch_status(&branch.status)
    ));
    if let Some(summary) = branch.summary.as_deref() {
        out.push_str(&format!("summary: {summary}\n"));
    }
    if let Some(ext) = branch_ext {
        if !ext.changed_files.is_empty() {
            out.push_str(&format!(
                "changed_files: {}\n",
                ext.changed_files.join(", ")
            ));
        }
        if !ext.tests_run.is_empty() {
            out.push_str(&format!("tests_run: {}\n", ext.tests_run.join(", ")));
        }
    }
    if let Some(worktree_path) = branch.worktree_path.as_deref() {
        out.push_str(&format!("worktree: {}\n", worktree_path.display()));
    }
    out.push_str(&format!("raw_log: {}\n", branch.raw_log));
    out.push_str("</fission_import>");
    out
}

pub(crate) async fn apply_fission_import_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    params: &serde_json::Value,
    config: &DrainConfig<'_>,
) -> Result<String, String> {
    let group_id = params
        .get("group_id")
        .or_else(|| params.get("groupId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "fission_import requires `group_id`".to_string())?
        .to_string();
    let branch_session_id = params
        .get("branch_session_id")
        .or_else(|| params.get("branchSessionId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "fission_import requires `branch_session_id`".to_string())?
        .to_string();

    let document = fission_ledger::read_fission_ledger_document(config.log_dir)
        .map_err(|e| format!("failed to read fission ledger: {e}"))?
        .ok_or_else(|| {
            format!("fission group `{group_id}` was not found (no fission ledger exists yet)")
        })?;
    let Some(group) = document
        .groups
        .iter()
        .find(|group| group.group_id == group_id)
    else {
        return Err(format!("fission group `{group_id}` was not found"));
    };
    if document.group_is_detached(&group_id) {
        return Err(format!(
            "fission group `{group_id}` is detached: a context rewind cut its spawn anchor out of the effective history, so its results cannot be auto-imported into the live lineage. Inspect the branch raw logs directly (see the fission ledger's raw_log pointers), or use rewind_backout on the covering rewind record to revisit the pre-rewind lineage."
        ));
    }
    let Some(branch) = group
        .branches
        .iter()
        .find(|branch| branch.session_id == branch_session_id)
    else {
        return Err(format!(
            "branch `{branch_session_id}` is not part of fission group `{group_id}`"
        ));
    };
    let payload = fission_import_payload(
        group,
        branch,
        document.branch_ext(&group_id, &branch_session_id),
    );

    // Inject into the thread the action targets (the caller's continuation);
    // default to the group's recorded parent.
    let parent_thread_id =
        thread_id_from_action_params(params).unwrap_or_else(|| group.parent_session_id.clone());
    agent
        .inject_thread_developer_message(&parent_thread_id, &payload)
        .await
        .map_err(|e| {
            format!("failed to inject fission import into thread {parent_thread_id}: {e}")
        })?;
    fission_ledger::mark_branch_imported(config.log_dir, &group_id, &branch_session_id, None)
        .map_err(|e| format!("failed to mark branch `{branch_session_id}` imported: {e}"))?;
    emit_session_relationship(
        config.bus,
        Some(group.parent_session_id.as_str()),
        &branch_session_id,
        "fission-imported",
        false,
    );
    Ok(payload)
}

/// First line of every anchor in a pre-rewind rollout catalog, keyed by item
/// id. Snapshotted BEFORE a rollback so the post-rollback fission detach pass
/// can decide anchor reachability against the pre-rewind line numbers.
pub(crate) fn fission_anchor_first_lines(
    anchors: &[ContextRewindAnchorCatalogEntry],
) -> HashMap<String, usize> {
    anchors
        .iter()
        .map(|anchor| (anchor.item_id.clone(), anchor.first_line))
        .collect()
}

/// The rollout line where a rewind cuts history: everything at/after the cut
/// is pruned for `position=before` (cut = the anchor's first line), and
/// everything strictly after it for `position=after` (cut = the anchor's
/// last line) — matching the rollback semantics in
/// `context_rewind_pruned_prior_primer_facts`.
pub(crate) fn fission_anchor_cut_line(
    anchors: &[ContextRewindAnchorCatalogEntry],
    item_id: &str,
    position: external_agent::RollbackAnchorPosition,
) -> Option<usize> {
    let entry = anchors.iter().find(|anchor| anchor.item_id == item_id)?;
    Some(match position {
        external_agent::RollbackAnchorPosition::Before => entry.first_line,
        external_agent::RollbackAnchorPosition::After => entry.last_line,
    })
}

/// Whether a fission group's spawn anchor survives a rewind that cut the
/// rollout at `cut_line`: it must exist in the pre-rewind catalog AND start
/// on the kept side of the cut — strictly before the cut for
/// `position=before` (the cut line itself is pruned), at-or-before it for
/// `position=after` (the cut line is the last kept line).
pub(crate) fn fission_anchor_reachable_after_rewind(
    anchor_first_lines: &HashMap<String, usize>,
    cut_line: usize,
    position: external_agent::RollbackAnchorPosition,
    anchor_item_id: &str,
) -> bool {
    let Some(first_line) = anchor_first_lines.get(anchor_item_id) else {
        return false;
    };
    match position {
        external_agent::RollbackAnchorPosition::Before => *first_line < cut_line,
        external_agent::RollbackAnchorPosition::After => *first_line <= cut_line,
    }
}

/// Every id the rewound parent is known by — the live thread id, the
/// snapshot/record ids, and the Intendant session id/alias — because the
/// fission recording paths differ in which id they store as
/// `parent_session_id`.
pub(crate) fn fission_detach_parent_candidates(
    thread_id: &str,
    record: &context_rewind::ContextRewindRecord,
    config: &DrainConfig<'_>,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for id in [
        Some(thread_id),
        Some(record.thread_id.as_str()),
        record.session_id.as_deref(),
        config.session_id.as_deref(),
        config.alias_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|id| !id.is_empty())
    {
        if !candidates.iter().any(|existing| existing == id) {
            candidates.push(id.to_string());
        }
    }
    candidates
}

/// Emit the `fission-detached` lineage markers (parent→branch) for every
/// branch a detach report flipped, so lineage views fold the marker into the
/// spawn row. Branches that kept a terminal status are deliberately not
/// marked — their recorded results stay real even though the join point is
/// gone.
pub(crate) fn emit_fission_detach_relationships(
    config: &DrainConfig<'_>,
    report: &fission_ledger::DetachReport,
) {
    if report.detached_group_ids.is_empty() {
        return;
    }
    let document = match fission_ledger::read_fission_ledger_document(config.log_dir) {
        Ok(Some(document)) => document,
        Ok(None) => return,
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not read fission ledger to emit detach relationships: {err}"
                ))
            });
            return;
        }
    };
    let flipped: HashSet<&str> = report
        .detached_branch_session_ids
        .iter()
        .map(String::as_str)
        .collect();
    for group_id in &report.detached_group_ids {
        let Some(group) = document
            .groups
            .iter()
            .find(|group| &group.group_id == group_id)
        else {
            continue;
        };
        for branch in &group.branches {
            if flipped.contains(branch.session_id.as_str()) {
                emit_session_relationship(
                    config.bus,
                    Some(group.parent_session_id.as_str()),
                    &branch.session_id,
                    "fission-detached",
                    false,
                );
            }
        }
    }
}

pub(crate) async fn apply_context_rewind_anchor_list_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    params: &serde_json::Value,
) -> Result<String, String> {
    let thread_id = thread_id_from_action_params(params);
    let mut rollout_path = None;
    if let Some(thread_id) = thread_id.as_deref() {
        rollout_path = agent
            .read_thread_snapshot(thread_id)
            .await
            .ok()
            .and_then(|snapshot| snapshot.rollout_path);
    }
    if rollout_path.is_none() {
        rollout_path = agent
            .context_snapshot()
            .await
            .map_err(|e| e.to_string())?
            .and_then(|snapshot| snapshot.rollout_path);
    }
    let rollout_path = rollout_path
        .ok_or_else(|| "Codex thread metadata did not include a rollout path".to_string())?;
    list_context_rewind_anchors_from_rollout(&rollout_path, params)
}

pub(crate) async fn apply_context_rewind_anchor_inspect_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    params: &serde_json::Value,
) -> Result<String, String> {
    let thread_id = thread_id_from_action_params(params);
    let mut rollout_path = None;
    if let Some(thread_id) = thread_id.as_deref() {
        rollout_path = agent
            .read_thread_snapshot(thread_id)
            .await
            .ok()
            .and_then(|snapshot| snapshot.rollout_path);
    }
    if rollout_path.is_none() {
        rollout_path = agent
            .context_snapshot()
            .await
            .map_err(|e| e.to_string())?
            .and_then(|snapshot| snapshot.rollout_path);
    }
    let rollout_path = rollout_path
        .ok_or_else(|| "Codex thread metadata did not include a rollout path".to_string())?;
    inspect_context_rewind_anchor_from_rollout(&rollout_path, params)
}

pub(crate) async fn handle_external_thread_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    op: String,
    params: serde_json::Value,
    target_session_id: Option<String>,
    config: &DrainConfig<'_>,
) -> ExternalThreadActionEffect {
    let params = thread_action_params_for_target(&op, params, &target_session_id, config);
    let action_thread_id = thread_id_from_action_params(&params);
    let result_session_id = target_session_id.or_else(|| config.session_id.clone());
    // Backends without an in-process fork (Claude Code) fork by respawning:
    // a NEW supervisor session resumes the current thread with the backend's
    // fork flag, and the child announces its own native id on its first turn
    // (the lineage relationship is emitted at that identity upgrade). The
    // same respawn carries `/side` (`/btw`) conversations there, with the
    // side boundary + question as the child's first prompt.
    if respawn_resume_thread_action_op(&op) {
        if let external_agent::ForkHandling::RespawnResume { thread_id } = agent.fork_handling() {
            let launch = crate::session_config::read_log_dir_config(config.log_dir);
            let (success, message) = respawn_resume_thread_action(
                config.bus,
                agent.name(),
                thread_id,
                &op,
                &params,
                config.project_root,
                launch.and_then(|cfg| cfg.agent_command),
            );
            slog(config.session_log, |l| {
                l.info(&format!(
                    "{} thread action /{}: {} — {}",
                    agent.name(),
                    op,
                    if success { "ok" } else { "FAILED" },
                    message
                ))
            });
            config.bus.send(AppEvent::CodexThreadActionResult {
                session_id: result_session_id.clone(),
                action: op.clone(),
                success,
                message,
                record_id: None,
            });
            return ExternalThreadActionEffect::None;
        }
    }
    let result = if is_context_rewind_anchor_list_action(&op) {
        apply_context_rewind_anchor_list_action(agent, &params).await
    } else if is_context_rewind_anchor_inspect_action(&op) {
        apply_context_rewind_anchor_inspect_action(agent, &params).await
    } else if is_context_rewind_backout_action(&op) {
        apply_context_rewind_backout_action(agent, &op, &params, config).await
    } else if is_fission_spawn_action(&op) {
        apply_fission_spawn_action(agent, &params, config).await
    } else if is_fission_import_action(&op) {
        apply_fission_import_action(agent, &params, config).await
    } else {
        agent
            .thread_action(&op, &params)
            .await
            .map_err(|e| e.to_string())
    };
    let (success, mut message) = match result {
        Ok(msg) => (true, msg),
        Err(e) => (false, e),
    };
    if success && op == "rename" {
        if let Some(home) = dirs::home_dir() {
            match persist_codex_thread_rename_overlay(
                &home,
                result_session_id.as_deref(),
                &params,
                &message,
            ) {
                Ok(Some(name)) => {
                    message = format!("Codex thread renamed to {}", name);
                }
                Ok(None) => {}
                Err(err) => slog(config.session_log, |l| {
                    l.warn(&format!("Failed to persist Codex thread rename: {err}"))
                }),
            }
        }
    }
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex thread action /{}: {} — {}",
            op,
            if success { "ok" } else { "FAILED" },
            codex_thread_action_log_message(&op, &message)
        ))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: result_session_id.clone(),
        action: op.clone(),
        success,
        message: message.clone(),
        record_id: None,
    });

    if success && op == "fast" {
        let service_tier = agent.service_tier().map(str::to_string);
        persist_codex_service_tier_for_drain(
            config,
            result_session_id.as_deref(),
            service_tier.as_deref(),
        );
        emit_codex_session_capabilities_for_drain(
            config,
            result_session_id.as_deref(),
            service_tier.as_deref(),
        );
    }

    if success && op == "fork" {
        if let Some(child_id) = forked_thread_id_from_message(&message) {
            emit_codex_fork_session_name(config.bus, &child_id, &params);
            emit_session_relationship(
                config.bus,
                action_thread_id
                    .as_deref()
                    .or(result_session_id.as_deref())
                    .or(config.session_id.as_deref()),
                &child_id,
                "fork",
                false,
            );
            config
                .bus
                .send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                    source: "codex".to_string(),
                    session_id: child_id.clone(),
                    resume_id: Some(child_id),
                    project_root: Some(config.project_root.to_string_lossy().to_string()),
                    task: None,
                    direct: Some(true),
                    fork: false,
                    relationship_kind: None,
                    attachments: Vec::new(),
                    agent_command: crate::session_config::read_log_dir_config(config.log_dir)
                        .and_then(|cfg| cfg.agent_command),
                    codex_sandbox: crate::session_config::read_log_dir_config(config.log_dir)
                        .and_then(|cfg| cfg.codex_sandbox),
                    codex_approval_policy: crate::session_config::read_log_dir_config(
                        config.log_dir,
                    )
                    .and_then(|cfg| cfg.codex_approval_policy),
                    codex_managed_context: crate::session_config::read_log_dir_config(
                        config.log_dir,
                    )
                    .and_then(|cfg| cfg.codex_managed_context),
                    codex_context_archive: crate::session_config::read_log_dir_config(
                        config.log_dir,
                    )
                    .and_then(|cfg| cfg.codex_context_archive),
                }));
        }
    }

    if success && op == "side" {
        if let Some((parent_thread_id, child_thread_id)) = side_thread_ids_from_message(&message) {
            return ExternalThreadActionEffect::SideTurnStarted {
                parent_thread_id,
                child_thread_id,
                prompt: side_session_prompt_from_params(&params),
            };
        }
    }

    if success && matches!(op.as_str(), "side-close" | "side_close") {
        if let Some(child_thread_id) = side_child_thread_id_from_params(&params) {
            config.bus.send(AppEvent::SessionEnded {
                session_id: child_thread_id.clone(),
                reason: "side conversation closed".to_string(),
                error_kind: None,
            });
            return ExternalThreadActionEffect::SideTurnClosed { child_thread_id };
        }
    }

    ExternalThreadActionEffect::None
}

pub(crate) fn codex_thread_action_log_message(op: &str, message: &str) -> String {
    if !is_context_rewind_anchor_list_action(op) {
        return message.to_string();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(message) else {
        return truncate_string_copy(message, 320);
    };
    let anchor_count = value
        .get("anchors")
        .and_then(|anchors| anchors.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let total = value.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    let filtered_total = value
        .get("filtered_total")
        .and_then(|v| v.as_u64())
        .unwrap_or(total);
    let offset = value.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
    let limit = value.get("limit").and_then(|v| v.as_u64()).unwrap_or(0);
    let next_offset = value
        .get("next_offset")
        .and_then(|v| v.as_u64())
        .map(|offset| offset.to_string())
        .unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"anchors\":{anchor_count},\"total\":{total},\"filtered_total\":{filtered_total},\"offset\":{offset},\"limit\":{limit},\"next_offset\":{next_offset},\"bytes\":{}}}",
        message.len()
    )
}

pub(crate) fn parse_codex_fast_slash_command(
    text: &str,
) -> Option<Result<(&'static str, serde_json::Value), String>> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut split = rest.splitn(2, char::is_whitespace);
    let name = split.next()?.trim().to_ascii_lowercase();
    if name != "fast" {
        return None;
    }
    let args = split.next().unwrap_or("").trim();
    if !args.is_empty() {
        return Some(Err("/fast does not accept arguments".to_string()));
    }
    Some(Ok(("fast", serde_json::json!({}))))
}

pub(crate) async fn maybe_handle_codex_fast_slash_steer(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    text: &str,
    target_session_id: Option<String>,
    steer_id: String,
    config: &DrainConfig<'_>,
) -> bool {
    if agent.name() != "codex" {
        return false;
    }
    // Codex app-server `turn/steer` is text-only; service-tier changes must
    // be routed as thread actions and applied to future supported requests.
    let Some(parsed) = parse_codex_fast_slash_command(text) else {
        return false;
    };
    match parsed {
        Ok((op, params)) => {
            handle_external_thread_action(
                agent,
                op.to_string(),
                params,
                target_session_id.clone(),
                config,
            )
            .await;
        }
        Err(message) => {
            config.bus.send(AppEvent::CodexThreadActionResult {
                session_id: target_session_id
                    .clone()
                    .or_else(|| config.session_id.clone()),
                action: "fast".to_string(),
                success: false,
                message,
                record_id: None,
            });
        }
    }
    if !steer_id.trim().is_empty() {
        config.bus.send(AppEvent::SteerDelivered {
            session_id: target_session_id,
            id: steer_id,
            mid_turn: false,
        });
    }
    true
}

pub(crate) fn undo_turns_from_params(params: &serde_json::Value) -> u32 {
    params.get("turns").and_then(|v| v.as_u64()).unwrap_or(1) as u32
}

pub(crate) fn side_rewind_first_turn_for_undo(
    current_turn_count: usize,
    turns: u32,
    side_thread_id: &str,
) -> Result<u32, String> {
    if turns == 0 {
        return Err("rollback count must be at least 1".to_string());
    }
    if turns as usize > current_turn_count {
        return Err(format!(
            "Cannot /undo {} turn(s) in side conversation {}; only {} side turn(s) exist after the /side boundary",
            turns, side_thread_id, current_turn_count
        ));
    }
    Ok(current_turn_count as u32 - turns + 1)
}

pub(crate) fn parent_rewind_first_turn_for_undo(
    current_turn_count: usize,
    turns: u32,
) -> Result<u32, String> {
    if turns == 0 {
        return Err("rollback count must be at least 1".to_string());
    }
    if turns as usize > current_turn_count {
        return Err(format!(
            "Cannot /undo {} turn(s); only {} user turn(s) are active",
            turns, current_turn_count
        ));
    }
    Ok(current_turn_count as u32 - turns + 1)
}

pub(crate) async fn rollback_parent_thread_from_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    round: &mut usize,
    user_turn_revisions: &mut UserTurnRevisionState,
    first_user_turn_index: u32,
    config: &DrainConfig<'_>,
) -> Result<u32, String> {
    if first_user_turn_index == 0 {
        return Err("Cannot rewind user turn 0".to_string());
    }
    if first_user_turn_index as usize > *round {
        return Err(format!(
            "Cannot rewind to user turn {}; current user turn count is {}",
            first_user_turn_index, *round
        ));
    }

    let turns_to_drop = *round as u32 - first_user_turn_index + 1;
    agent
        .rollback_turns(turns_to_drop)
        .await
        .map_err(|e| format!("thread rollback failed: {}", e))?;

    user_turn_revisions.rewind_from_turn(first_user_turn_index);
    *round = first_user_turn_index.saturating_sub(1) as usize;
    config.bus.send(AppEvent::UserMessageRewind {
        session_id: config.session_id.clone(),
        user_turn_index: first_user_turn_index,
        turns_removed: turns_to_drop,
    });
    Ok(turns_to_drop)
}

pub(crate) async fn handle_parent_undo_thread_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    round: &mut usize,
    user_turn_revisions: &mut UserTurnRevisionState,
    params: serde_json::Value,
    config: &DrainConfig<'_>,
) {
    let turns = undo_turns_from_params(&params);
    let result = match parent_rewind_first_turn_for_undo(*round, turns) {
        Ok(first_user_turn_index) => rollback_parent_thread_from_turn(
            agent,
            round,
            user_turn_revisions,
            first_user_turn_index,
            config,
        )
        .await
        .map(|turns_removed| format!("rolled back {} turn(s)", turns_removed)),
        Err(message) => Err(message),
    };

    let (success, message) = match result {
        Ok(message) => (true, message),
        Err(message) => (false, message),
    };
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex thread action /undo: {} — {}",
            if success { "ok" } else { "FAILED" },
            message
        ))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: config.session_id.clone(),
        action: "undo".to_string(),
        success,
        message,
        record_id: None,
    });
}

pub(crate) async fn rollback_side_thread_from_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    side_rounds: &mut HashMap<String, usize>,
    side_turn_revisions: &mut HashMap<String, UserTurnRevisionState>,
    side_thread_id: &str,
    first_user_turn_index: u32,
    config: &DrainConfig<'_>,
) -> Result<u32, String> {
    if first_user_turn_index == 0 {
        return Err(format!(
            "Cannot rewind side conversation {}; user turn index must be at least 1",
            side_thread_id
        ));
    }

    let current_turn_count = *side_rounds.entry(side_thread_id.to_string()).or_insert(1);
    if first_user_turn_index as usize > current_turn_count {
        return Err(format!(
            "Cannot rewind side conversation {} to user turn {}; current side user turn count is {}",
            side_thread_id, first_user_turn_index, current_turn_count
        ));
    }

    let turns_to_drop = current_turn_count as u32 - first_user_turn_index + 1;
    agent
        .rollback_thread_turns(side_thread_id, turns_to_drop)
        .await
        .map_err(|e| format!("thread rollback failed: {}", e))?;

    let revisions = side_turn_revisions
        .entry(side_thread_id.to_string())
        .or_default();
    revisions.seed_active_turns_to(current_turn_count as u32);
    revisions.rewind_from_turn(first_user_turn_index);
    side_rounds.insert(
        side_thread_id.to_string(),
        first_user_turn_index.saturating_sub(1) as usize,
    );
    config.bus.send(AppEvent::UserMessageRewind {
        session_id: Some(side_thread_id.to_string()),
        user_turn_index: first_user_turn_index,
        turns_removed: turns_to_drop,
    });
    Ok(turns_to_drop)
}

pub(crate) async fn handle_side_undo_thread_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    side_rounds: &mut HashMap<String, usize>,
    side_turn_revisions: &mut HashMap<String, UserTurnRevisionState>,
    side_thread_id: &str,
    params: serde_json::Value,
    config: &DrainConfig<'_>,
) {
    let turns = undo_turns_from_params(&params);
    let current_turn_count = *side_rounds.entry(side_thread_id.to_string()).or_insert(1);
    let result = match side_rewind_first_turn_for_undo(current_turn_count, turns, side_thread_id) {
        Ok(first_user_turn_index) => rollback_side_thread_from_turn(
            agent,
            side_rounds,
            side_turn_revisions,
            side_thread_id,
            first_user_turn_index,
            config,
        )
        .await
        .map(|turns_removed| format!("rolled back {} turn(s)", turns_removed)),
        Err(message) => Err(message),
    };

    let (success, message) = match result {
        Ok(message) => (true, message),
        Err(message) => (false, message),
    };
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex side thread action /undo: {} — {}",
            if success { "ok" } else { "FAILED" },
            message
        ))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: Some(side_thread_id.to_string()),
        action: "undo".to_string(),
        success,
        message,
        record_id: None,
    });
}

pub(crate) fn emit_side_session_started(
    config: &DrainConfig<'_>,
    parent_thread_id: &str,
    child_thread_id: &str,
    prompt: Option<&str>,
) {
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex /side: side conversation started in thread {} from parent {}",
            child_thread_id, parent_thread_id
        ))
    });
    config.bus.send(AppEvent::SessionStarted {
        session_id: child_thread_id.to_string(),
        task: Some(
            prompt
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("Side conversation")
                .to_string(),
        ),
    });
    config.bus.send(AppEvent::SessionIdentity {
        session_id: child_thread_id.to_string(),
        source: "codex".to_string(),
        backend_session_id: child_thread_id.to_string(),
    });
    let parent_session_id = config.session_id.as_deref().unwrap_or(parent_thread_id);
    emit_session_relationship(
        config.bus,
        Some(parent_session_id),
        child_thread_id,
        "side",
        true,
    );
}

/// Which external backend this drain supervises, from its display source
/// ("Codex", "Claude Code"). None for unknown/legacy sources.
pub(crate) fn external_backend_of_config(
    config: &DrainConfig<'_>,
) -> Option<external_agent::AgentBackend> {
    config
        .agent_source
        .as_deref()
        .and_then(external_agent::AgentBackend::from_str_loose)
}

/// Announce a backend-native sub-agent as a synthetic child session:
/// identity, parent relationship, capability ceiling, and the started log
/// line. Backend-neutral — the universal sub-agent rail. Codex collab
/// children accept follow-ups (injected via collab tools); Claude Code's
/// in-band tasks are fire-and-forget, so their windows expose none.
pub(crate) fn emit_external_subagent_started(
    config: &DrainConfig<'_>,
    parent_thread_id: &str,
    child_thread_id: &str,
    prompt: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) {
    let child_thread_id = child_thread_id.trim();
    if child_thread_id.is_empty() {
        return;
    }
    let parent_thread_id = parent_thread_id.trim();
    let parent_session_id = if parent_thread_id.is_empty() {
        config.session_id.as_deref().unwrap_or("")
    } else {
        parent_thread_id
    };
    if parent_session_id.is_empty() || parent_session_id == child_thread_id {
        return;
    }

    let backend = external_backend_of_config(config);
    let source_label = external_agent_log_source(config.agent_source.as_deref());
    config.bus.send(AppEvent::SessionIdentity {
        session_id: child_thread_id.to_string(),
        source: backend
            .as_ref()
            .map(|b| b.as_short_str().to_string())
            .unwrap_or_else(|| "codex".to_string()),
        backend_session_id: child_thread_id.to_string(),
    });
    emit_session_relationship(
        config.bus,
        Some(parent_session_id),
        child_thread_id,
        "subagent",
        false,
    );
    config.bus.send(AppEvent::SessionCapabilities {
        session_id: child_thread_id.to_string(),
        capabilities: types::SessionCapabilities {
            follow_up: !matches!(backend, Some(external_agent::AgentBackend::ClaudeCode)),
            steer: false,
            interrupt: false,
            thread_actions: Vec::new(),
            codex_thread_actions: Vec::new(),
            codex_managed_context: None,
            codex_sandbox: None,
            codex_approval_policy: None,
            codex_context_archive: None,
            codex_command: None,
            codex_fast_mode: None,
            codex_service_tier: None,
        },
    });
    config.bus.send(AppEvent::SessionStarted {
        session_id: child_thread_id.to_string(),
        task: Some(
            prompt
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{source_label} subagent")),
        ),
    });

    let mut details = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|s| !s.is_empty()) {
        details.push(format!("model {model}"));
    }
    if let Some(effort) = reasoning_effort.map(str::trim).filter(|s| !s.is_empty()) {
        details.push(format!("reasoning {effort}"));
    }
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    let content = prompt
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|prompt| format!("{source_label} subagent started{suffix}: {prompt}"))
        .unwrap_or_else(|| format!("{source_label} subagent started{suffix}"));
    config.bus.send(AppEvent::LogEntry {
        session_id: Some(child_thread_id.to_string()),
        level: "agent".to_string(),
        source: source_label,
        content,
        turn: None,
    });
}

/// Register a `SubAgentToolCall`'s children in the routing tables and
/// announce the new ones. Shared by the active drain and the idle listener
/// so children spawned in either state get windows and scoped-event
/// routing.
pub(crate) fn register_external_subagent_children(
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    sender_thread_id: &str,
    subagent_thread_ids: &[String],
    prompt: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) {
    for child_thread_id in subagent_thread_ids {
        let child_thread_id = child_thread_id.trim();
        if child_thread_id.is_empty() || child_thread_id == sender_thread_id.trim() {
            continue;
        }
        let sender_thread_id = sender_thread_id.trim();
        if !sender_thread_id.is_empty() {
            stats
                .codex_subagent_parent_threads
                .entry(child_thread_id.to_string())
                .or_insert_with(|| sender_thread_id.to_string());
        }
        if stats
            .codex_subagent_sessions
            .insert(child_thread_id.to_string())
        {
            emit_external_subagent_started(
                config,
                sender_thread_id,
                child_thread_id,
                prompt,
                model,
                reasoning_effort,
            );
        }
        emit_codex_subagent_transcript_updates(config, stats, child_thread_id);
    }
}

pub(crate) fn emit_external_subagent_state(
    config: &DrainConfig<'_>,
    state: &external_agent::SubAgentState,
) {
    let thread_id = state.thread_id.trim();
    if thread_id.is_empty() {
        return;
    }
    let label = external_agent_log_source(config.agent_source.as_deref());
    let status = state.status.trim();
    let message = state
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (level, content) = match status {
        "completed" => (
            "info",
            message
                .map(|message| format!("Task complete: {label} subagent completed: {message}"))
                .unwrap_or_else(|| format!("Task complete: {label} subagent completed")),
        ),
        "interrupted" => (
            "warn",
            message
                .map(|message| {
                    format!("Agent interrupted: {label} subagent interrupted: {message}")
                })
                .unwrap_or_else(|| format!("Agent interrupted: {label} subagent interrupted")),
        ),
        "errored" => (
            "warn",
            message
                .map(|message| format!("Session ended: {label} subagent errored: {message}"))
                .unwrap_or_else(|| format!("Session ended: {label} subagent errored")),
        ),
        "shutdown" => (
            "info",
            message
                .map(|message| format!("Session ended: {label} subagent shut down: {message}"))
                .unwrap_or_else(|| format!("Session ended: {label} subagent shut down")),
        ),
        "notFound" => (
            "warn",
            message
                .map(|message| format!("Session ended: {label} subagent not found: {message}"))
                .unwrap_or_else(|| format!("Session ended: {label} subagent not found")),
        ),
        "pendingInit" | "running" => return,
        other => (
            "info",
            message
                .map(|message| format!("{label} subagent {other}: {message}"))
                .unwrap_or_else(|| format!("{label} subagent {other}")),
        ),
    };
    config.bus.send(AppEvent::LogEntry {
        session_id: Some(thread_id.to_string()),
        level: level.to_string(),
        source: label,
        content,
        turn: None,
    });
}

pub(crate) fn external_subagent_terminal_reason(
    label: &str,
    state: &external_agent::SubAgentState,
) -> Option<String> {
    let status = state.status.trim();
    let message = state
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match status {
        "interrupted" => Some(
            message
                .map(|message| format!("{label} subagent interrupted: {message}"))
                .unwrap_or_else(|| format!("{label} subagent interrupted")),
        ),
        "errored" => Some(
            message
                .map(|message| format!("{label} subagent errored: {message}"))
                .unwrap_or_else(|| format!("{label} subagent errored")),
        ),
        "shutdown" => Some(
            message
                .map(|message| format!("{label} subagent shut down: {message}"))
                .unwrap_or_else(|| format!("{label} subagent shut down")),
        ),
        "notFound" => Some(
            message
                .map(|message| format!("{label} subagent not found: {message}"))
                .unwrap_or_else(|| format!("{label} subagent not found")),
        ),
        _ => None,
    }
}

pub(crate) fn emit_external_subagent_terminal(
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    state: &external_agent::SubAgentState,
) {
    let thread_id = state.thread_id.trim();
    if thread_id.is_empty() {
        return;
    }
    let label = external_agent_log_source(config.agent_source.as_deref());
    let Some(reason) = external_subagent_terminal_reason(&label, state) else {
        return;
    };
    if !stats
        .codex_subagent_terminal_sessions
        .insert(thread_id.to_string())
    {
        return;
    }

    if state.status.trim() == "interrupted" {
        config.bus.send(AppEvent::Interrupted {
            session_id: Some(thread_id.to_string()),
            reason,
        });
    } else {
        config.bus.send(AppEvent::SessionEnded {
            session_id: thread_id.to_string(),
            reason,
            error_kind: None,
        });
    }
}

pub(crate) fn json_u32_field(value: &serde_json::Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
}

pub(crate) fn emit_codex_subagent_transcript_entry(
    config: &DrainConfig<'_>,
    child_thread_id: &str,
    entry: &serde_json::Value,
) {
    let content = entry
        .get("content")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(content) = content else {
        return;
    };
    let source = entry
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("codex");
    if source.eq_ignore_ascii_case("user") {
        config.bus.send(AppEvent::UserMessageLog {
            session_id: Some(child_thread_id.to_string()),
            content: content.to_string(),
            user_turn_index: json_u32_field(entry, "user_turn_index"),
            user_turn_revision: json_u32_field(entry, "user_turn_revision"),
            replacement_for_user_turn_index: json_u32_field(
                entry,
                "replacement_for_user_turn_index",
            ),
        });
        return;
    }

    config.bus.send(AppEvent::LogEntry {
        session_id: Some(child_thread_id.to_string()),
        level: entry
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_string(),
        source: source.to_string(),
        content: content.to_string(),
        turn: None,
    });
}

pub(crate) fn emit_codex_subagent_transcript_updates(
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    child_thread_id: &str,
) {
    // Reads Codex's on-disk session transcripts; other backends' children
    // stream their transcript in-band as scoped events.
    if !matches!(
        external_backend_of_config(config),
        None | Some(external_agent::AgentBackend::Codex)
    ) {
        return;
    }
    let child_thread_id = child_thread_id.trim();
    if child_thread_id.is_empty() {
        return;
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
    let Some(entries) =
        crate::web_gateway::external_session_entries_from_home(&home, "codex", child_thread_id)
    else {
        return;
    };

    let offset = stats
        .codex_subagent_transcript_offsets
        .entry(child_thread_id.to_string())
        .or_insert(0);
    if *offset > entries.len() {
        *offset = 0;
    }
    for entry in entries.iter().skip(*offset) {
        emit_codex_subagent_transcript_entry(config, child_thread_id, entry);
    }
    *offset = entries.len();
}

pub(crate) fn codex_subagent_thread_ids(
    receiver_thread_ids: &[String],
    agents: &[external_agent::SubAgentState],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for id in receiver_thread_ids
        .iter()
        .map(String::as_str)
        .chain(agents.iter().map(|state| state.thread_id.as_str()))
    {
        let id = id.trim();
        if !id.is_empty() && seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
    }
    ids
}

pub(crate) struct CodexFissionObservationInput<'a> {
    pub(crate) item_id: &'a str,
    pub(crate) tool: &'a str,
    pub(crate) status: &'a str,
    pub(crate) sender_thread_id: &'a str,
    pub(crate) subagent_thread_ids: &'a [String],
    pub(crate) prompt: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) reasoning_effort: Option<&'a str>,
    pub(crate) agents: &'a [external_agent::SubAgentState],
}

pub(crate) fn record_codex_fission_observation(
    config: &DrainConfig<'_>,
    input: CodexFissionObservationInput<'_>,
) {
    // Fission is a Codex managed-context concept; other backends' in-band
    // sub-agents are relationships, not fission branches.
    if !matches!(
        external_backend_of_config(config),
        None | Some(external_agent::AgentBackend::Codex)
    ) {
        return;
    }
    let parent_session_id = {
        let sender_thread_id = input.sender_thread_id.trim();
        if sender_thread_id.is_empty() {
            config.session_id.clone().unwrap_or_default()
        } else {
            sender_thread_id.to_string()
        }
    };
    if parent_session_id.trim().is_empty() || input.item_id.trim().is_empty() {
        return;
    }

    let default_branch_status = if input.status.trim() == "failed" {
        "failed"
    } else {
        "running"
    };
    let mut branches = std::collections::BTreeMap::new();
    for id in input.subagent_thread_ids {
        let id = id.trim();
        if !id.is_empty() && id != parent_session_id {
            branches.insert(
                id.to_string(),
                fission_ledger::FissionBranchObservation {
                    session_id: id.to_string(),
                    status: default_branch_status.to_string(),
                    summary: None,
                },
            );
        }
    }
    for state in input.agents {
        let id = state.thread_id.trim();
        if id.is_empty() || id == parent_session_id {
            continue;
        }
        branches.insert(
            id.to_string(),
            fission_ledger::FissionBranchObservation {
                session_id: id.to_string(),
                status: state.status.trim().to_string(),
                summary: state
                    .message
                    .as_deref()
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .map(str::to_string),
            },
        );
    }
    if branches.is_empty() {
        return;
    }

    let observation = fission_ledger::FissionObservation {
        parent_session_id,
        anchor_item_id: input.item_id.trim().to_string(),
        tool: input.tool.trim().to_string(),
        status: input.status.trim().to_string(),
        prompt: input
            .prompt
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        model: input
            .model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        reasoning_effort: input
            .reasoning_effort
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        branches: branches.into_values().collect(),
    };
    if let Err(err) = fission_ledger::record_fission_observation(config.log_dir, observation) {
        slog(config.session_log, |log| {
            log.warn(&format!(
                "Could not persist fission ledger observation: {err}"
            ))
        });
    }
}

pub(crate) fn short_external_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

pub(crate) fn collab_agent_tool_preview(
    tool: &str,
    receiver_thread_ids: &[String],
    prompt: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    let receivers: Vec<String> = receiver_thread_ids
        .iter()
        .map(|id| short_external_session_id(id))
        .collect();
    if !receivers.is_empty() {
        parts.push(format!("target {}", receivers.join(", ")));
    }
    if let Some(prompt) = prompt.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(prompt.chars().take(120).collect());
    }
    if parts.is_empty() {
        tool.to_string()
    } else {
        format!("{}: {}", tool, parts.join(" - "))
    }
}

pub(crate) async fn drain_external_child_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    bus_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    diff_tracker: &mut ExternalDiffDeltaTracker,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    handled_steer_ids: &mut std::collections::HashSet<String>,
    cancelled_follow_ups: &mut HashSet<String>,
    codex_thread_action_dedupe: &mut CodexThreadActionDedupe,
    child_thread_id: String,
    conversation_kind: &str,
) {
    slog(config.session_log, |l| {
        l.info(&format!(
            "Draining Codex {} conversation {}",
            conversation_kind, child_thread_id
        ))
    });

    let child_session_id = Some(child_thread_id.clone());
    let child_config = DrainConfig {
        bus: config.bus,
        web_port: config.web_port,
        session_id: child_session_id.clone(),
        alias_session_id: None,
        backend_thread_id: Some(child_thread_id.clone()),
        autonomy: config.autonomy.clone(),
        session_log: config.session_log,
        project_root: config.project_root,
        log_dir: config.log_dir,
        approval_registry: config.approval_registry,
        json_approval: config.json_approval,
        agent_source: config.agent_source.clone(),
        suppress_agent_started: config.suppress_agent_started,
        persist_model_responses_inline: config.persist_model_responses_inline,
        headless: config.headless,
        context_injection: config.context_injection,
    };

    match drain_external_agent_events(
        agent,
        event_rx,
        bus_rx,
        &child_config,
        stats,
        diff_tracker,
        pending_runtime_steers,
        handled_steer_ids,
        cancelled_follow_ups,
        codex_thread_action_dedupe,
        None,
        false,
        false,
        false,
    )
    .await
    {
        DrainOutcome::TurnCompleted { message, .. } => {
            emit_child_turn_complete(&child_config, conversation_kind, message);
        }
        DrainOutcome::ContextRewindRequested { request, .. } => {
            emit_context_rewind_failure(
                &request,
                format!(
                    "context rewind is not supported inside {} conversations",
                    conversation_kind
                ),
                &child_config,
            );
        }
        DrainOutcome::RecoveryRequired {
            message,
            recovery_hint,
            ..
        } => {
            let mut content = format!(
                "Agent recovery required: {} conversation stopped after backend error: {}",
                conversation_kind, message
            );
            if let Some(hint) = recovery_hint {
                content.push_str("\nRecovery: ");
                content.push_str(&hint);
            }
            child_config.bus.send(AppEvent::LogEntry {
                session_id: child_config.session_id.clone(),
                level: "error".to_string(),
                source: "Codex".to_string(),
                content,
                turn: None,
            });
        }
        DrainOutcome::Interrupted { reason } => {
            child_config.bus.send(AppEvent::LogEntry {
                session_id: child_config.session_id.clone(),
                level: "warn".to_string(),
                source: "Codex".to_string(),
                content: format!(
                    "Agent interrupted: {} conversation stopped: {}",
                    conversation_kind, reason
                ),
                turn: None,
            });
        }
        DrainOutcome::Terminated { reason, exit_code } => {
            slog(config.session_log, |l| {
                l.warn(&format!(
                    "Codex terminated during {} conversation: {} (exit code: {:?})",
                    conversation_kind, reason, exit_code
                ))
            });
        }
        DrainOutcome::ChannelClosed => {
            slog(config.session_log, |l| {
                l.warn(&format!(
                    "Codex {} conversation event channel closed",
                    conversation_kind
                ))
            });
        }
    }
}

pub(crate) fn persist_external_model_response_if_needed(
    config: &DrainConfig<'_>,
    content: &str,
    reasoning: Option<&str>,
) {
    persist_external_model_response_for_session_if_needed(
        config,
        config.session_id.as_deref(),
        content,
        reasoning,
    );
}

pub(crate) fn persist_external_model_response_for_session_if_needed(
    config: &DrainConfig<'_>,
    session_id: Option<&str>,
    content: &str,
    reasoning: Option<&str>,
) {
    if !config.persist_model_responses_inline {
        return;
    }
    if !content.is_empty() {
        slog(config.session_log, |l| {
            l.model_response_for_session(
                session_id,
                content,
                0,
                0,
                0,
                0,
                config.agent_source.as_deref(),
            )
        });
    }
    if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
        slog(config.session_log, |l| {
            l.reasoning_content(Some(reasoning), None)
        });
    }
}

pub(crate) fn emit_external_tool_output(
    config: &DrainConfig<'_>,
    session_id: Option<&str>,
    stdout: String,
) {
    if stdout.is_empty() {
        return;
    }
    let output_id = event::next_agent_output_id();
    slog(config.session_log, |l| {
        l.agent_output_with_session_id(
            session_id,
            &stdout,
            "",
            config.agent_source.as_deref(),
            Some(&output_id),
        )
    });
    config.bus.send(AppEvent::AgentOutput {
        session_id: session_id.map(str::to_string),
        stdout,
        stderr: String::new(),
        source: config.agent_source.clone(),
        output_id: Some(output_id),
    });
}

pub(crate) fn scoped_event_codex_subagent_thread_id(
    event_thread_id: &Option<String>,
    stats: &LoopStats,
) -> Option<String> {
    event_thread_id
        .as_deref()
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
        .filter(|thread_id| stats.codex_subagent_parent_threads.contains_key(*thread_id))
        .map(str::to_string)
}

pub(crate) fn emit_external_session_goal(
    config: &DrainConfig<'_>,
    session_id: Option<String>,
    goal: Option<types::SessionGoal>,
) {
    if let Some(session_id) = session_id.or_else(|| config.session_id.clone()) {
        config.bus.send(AppEvent::SessionGoal { session_id, goal });
    }
}

/// A backend announced its native conversation id after thread start
/// (Claude Code reveals it on the first stdout message of the first turn).
/// Upgrade Intendant's identity and resume records from the placeholder
/// thread id: frontends re-key via `AppEvent::SessionIdentity` and the
/// external overlay makes `--continue`/resume find the native id.
pub(crate) fn persist_native_backend_session_id(config: &DrainConfig<'_>, native_id: &str) {
    let native_id = native_id.trim();
    let Some(backend) = config
        .agent_source
        .as_deref()
        .and_then(external_agent::AgentBackend::from_str_loose)
    else {
        return;
    };
    if !backend.thread_id_is_canonical(native_id) {
        return;
    }
    emit_external_session_identity(
        config.bus,
        config
            .session_id
            .clone()
            .or_else(|| config.alias_session_id.clone()),
        backend.as_short_str(),
        native_id,
    );
    // The bus tee only writes into the daemon-main log; supervisor-spawned
    // session loops never see their own identity event teed back. Record it
    // directly in the owning log — resume resolution
    // (`persisted_external_identity_for_session`) and the wrapper index
    // read THIS session's log, and a forked child whose identity only
    // exists in the main log cannot be resumed. (Main-loop sessions get a
    // duplicate row from the tee; readers take any matching record.)
    if let Ok(mut log) = config.session_log.lock() {
        let wrapper_id = config
            .session_id
            .clone()
            .or_else(|| config.alias_session_id.clone())
            .unwrap_or_else(|| native_id.to_string());
        log.session_identity(&wrapper_id, backend.as_short_str(), native_id);
    }
    if backend == external_agent::AgentBackend::ClaudeCode {
        // Frontends may address the session by either id after the
        // identity upgrade; advertise capabilities under the native id too.
        emit_claude_code_session_capabilities(config.bus, Some(native_id));
    }
    let mut launch = crate::session_config::read_log_dir_config(config.log_dir).unwrap_or_default();
    if launch.source.is_none() {
        launch.source = Some(backend.as_short_str().to_string());
    }
    if let Err(e) = crate::session_config::write_external_overlay(
        &platform::home_dir(),
        backend.as_short_str(),
        native_id,
        &launch,
    ) {
        slog(config.session_log, |l| {
            l.debug(&format!(
                "Persist external overlay for native session id {} failed: {e}",
                short_external_session_id(native_id)
            ))
        });
    }
    // A forked child announcing its own id is the first moment both ends of
    // the lineage edge exist — materialize the lineage relationship now
    // (`fork`, or the persisted kind: `side` for /btw conversations).
    if let Some(parent) = launch
        .forked_from
        .as_deref()
        .map(str::trim)
        .filter(|parent| !parent.is_empty() && *parent != native_id)
    {
        let kind = launch
            .fork_relationship
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .unwrap_or("fork");
        // Side children are ephemeral Q&A surfaces — same flag Codex's
        // in-process side start emits; plain forks are durable sessions.
        emit_session_relationship(config.bus, Some(parent), native_id, kind, kind == "side");
    }
}

pub(crate) fn handle_idle_codex_subagent_event(
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    child_thread_id: String,
    event: external_agent::AgentEvent,
) {
    let session_id = Some(child_thread_id.clone());
    match event {
        external_agent::AgentEvent::NativeSessionId { .. } => {
            // Native-id announcements describe the primary conversation;
            // a Codex subagent child never re-keys the session.
        }
        external_agent::AgentEvent::MessageDelta { text } => {
            config
                .bus
                .send(AppEvent::ModelResponseDelta { session_id, text });
        }
        external_agent::AgentEvent::Message { text } => {
            persist_external_model_response_for_session_if_needed(
                config,
                Some(&child_thread_id),
                &text,
                None,
            );
            config.bus.send(AppEvent::ModelResponse {
                session_id,
                turn: stats
                    .codex_subagent_rounds
                    .get(&child_thread_id)
                    .copied()
                    .unwrap_or(0),
                content: text,
                usage: provider::TokenUsage::default(),
                reasoning: None,
                source: config.agent_source.clone(),
            });
        }
        external_agent::AgentEvent::UserMessage { text } => {
            config.bus.send(AppEvent::UserMessageLog {
                session_id,
                content: text,
                user_turn_index: None,
                user_turn_revision: None,
                replacement_for_user_turn_index: None,
            });
        }
        external_agent::AgentEvent::Reasoning { text } => {
            persist_external_model_response_for_session_if_needed(
                config,
                Some(&child_thread_id),
                "",
                Some(&text),
            );
            config.bus.send(AppEvent::ModelResponse {
                session_id,
                turn: stats
                    .codex_subagent_rounds
                    .get(&child_thread_id)
                    .copied()
                    .unwrap_or(0),
                content: String::new(),
                usage: provider::TokenUsage::default(),
                reasoning: Some(text),
                source: config.agent_source.clone(),
            });
        }
        external_agent::AgentEvent::Log { level, message } => {
            config.bus.send(AppEvent::LogEntry {
                session_id,
                level,
                source: config
                    .agent_source
                    .clone()
                    .unwrap_or_else(|| "worker".to_string()),
                content: message,
                turn: None,
            });
        }
        external_agent::AgentEvent::BackendError {
            message,
            code,
            details,
            recovery_hint,
            ..
        } => {
            let label = external_agent_log_source(config.agent_source.as_deref());
            let mut content = if let Some(code) = code {
                format!("{label} subagent backend error ({code}): {message}")
            } else {
                format!("{label} subagent backend error: {message}")
            };
            if let Some(details) = details.filter(|s| !s.trim().is_empty()) {
                content.push('\n');
                content.push_str(details.trim());
            }
            if let Some(hint) = recovery_hint {
                content.push_str("\nRecovery: ");
                content.push_str(&hint);
            }
            config.bus.send(AppEvent::LogEntry {
                session_id,
                level: "error".to_string(),
                source: external_agent_log_source(config.agent_source.as_deref()),
                content,
                turn: None,
            });
        }
        external_agent::AgentEvent::ToolStarted {
            item_id,
            tool_name,
            preview,
        } => {
            let commands_preview = external_tool_preview_text(&tool_name, &preview)
                .unwrap_or_else(|| tool_name.clone());
            let turn = stats
                .codex_subagent_rounds
                .entry(child_thread_id.clone())
                .or_insert(0);
            *turn += 1;
            config.bus.send(AppEvent::AgentStarted {
                session_id,
                turn: *turn,
                commands_preview,
                item_id: Some(item_id),
                source: config.agent_source.clone(),
            });
        }
        external_agent::AgentEvent::ToolOutputDelta { item_id, text } => {
            let tool_output_limiter = stats
                .codex_subagent_tool_output_limiters
                .entry(child_thread_id.clone())
                .or_default();
            let Some(stdout) = tool_output_limiter.filter(&item_id, text) else {
                return;
            };
            emit_external_tool_output(config, Some(&child_thread_id), stdout);
        }
        external_agent::AgentEvent::ToolCompleted { item_id, status } => {
            if let Some(limiter) = stats
                .codex_subagent_tool_output_limiters
                .get_mut(&child_thread_id)
            {
                if let Some(stdout) = limiter.complete(&item_id) {
                    emit_external_tool_output(config, Some(&child_thread_id), stdout);
                }
            }
            if let external_agent::ToolCompletionStatus::Failed { message } = status {
                let content = external_tool_failure_content(&item_id, &message, None);
                let limiter = stats
                    .codex_subagent_tool_failure_limiters
                    .entry(child_thread_id.clone())
                    .or_default();
                let Some(content) = limiter.filter(content) else {
                    return;
                };
                config.bus.send(AppEvent::LogEntry {
                    session_id,
                    level: "warn".to_string(),
                    source: external_agent_log_source(config.agent_source.as_deref()),
                    content,
                    turn: None,
                });
            }
        }
        external_agent::AgentEvent::TurnCompleted { message } => {
            stats
                .codex_subagent_tool_output_limiters
                .remove(&child_thread_id);
            stats
                .codex_subagent_tool_failure_limiters
                .remove(&child_thread_id);
            emit_child_turn_complete_for_session(config.bus, session_id, "subagent", message);
        }
        external_agent::AgentEvent::SubAgentToolCall {
            item_id,
            tool,
            status,
            sender_thread_id,
            receiver_thread_ids,
            prompt,
            model,
            reasoning_effort,
            agents,
        } => {
            let subagent_thread_ids = codex_subagent_thread_ids(&receiver_thread_ids, &agents);
            record_codex_fission_observation(
                config,
                CodexFissionObservationInput {
                    item_id: &item_id,
                    tool: &tool,
                    status: &status,
                    sender_thread_id: &sender_thread_id,
                    subagent_thread_ids: &subagent_thread_ids,
                    prompt: prompt.as_deref(),
                    model: model.as_deref(),
                    reasoning_effort: reasoning_effort.as_deref(),
                    agents: &agents,
                },
            );
            register_external_subagent_children(
                config,
                stats,
                &sender_thread_id,
                &subagent_thread_ids,
                prompt.as_deref(),
                model.as_deref(),
                reasoning_effort.as_deref(),
            );
            for state in &agents {
                emit_external_subagent_state(config, state);
                emit_external_subagent_terminal(config, stats, state);
            }
        }
        external_agent::AgentEvent::Usage { usage } => {
            config.bus.send(AppEvent::UsageSnapshot {
                session_id,
                main: usage.into_model_snapshot(),
                presence: None,
            });
        }
        external_agent::AgentEvent::GoalUpdated { goal } => {
            emit_external_session_goal(config, Some(child_thread_id), Some(goal));
        }
        external_agent::AgentEvent::GoalCleared => {
            emit_external_session_goal(config, Some(child_thread_id), None);
        }
        external_agent::AgentEvent::PlanUpdate { .. }
        | external_agent::AgentEvent::ApprovalRequest { .. }
        | external_agent::AgentEvent::FileApprovalRequest { .. }
        | external_agent::AgentEvent::UserQuestionRequest { .. }
        | external_agent::AgentEvent::DiffUpdated { .. }
        | external_agent::AgentEvent::Terminated { .. }
        | external_agent::AgentEvent::Scoped { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn fork_session_name_from_params_trims_blank_names() {
        assert_eq!(
            fork_session_name_from_params(&serde_json::json!({ "name": "  Forked work  " })),
            Some("Forked work".to_string())
        );
        assert_eq!(
            fork_session_name_from_params(&serde_json::json!({ "name": "   " })),
            None
        );
        assert_eq!(fork_session_name_from_params(&serde_json::json!({})), None);
    }

    #[test]
    fn codex_thread_rename_overlay_persists_native_rename() {
        let home = tempfile::TempDir::new().unwrap();
        let persisted = persist_codex_thread_rename_overlay(
            home.path(),
            Some("thread-1"),
            &serde_json::json!({ "name": "  Better name  " }),
            "Codex thread renamed to ignored fallback",
        )
        .unwrap();
        assert_eq!(persisted.as_deref(), Some("Better name"));

        let mut sessions = vec![serde_json::json!({
            "source": "codex",
            "session_id": "thread-1",
            "name": "Old name"
        })];
        crate::session_names::apply_session_name_overlays(home.path(), &mut sessions);
        assert_eq!(sessions[0]["name"], "Better name");
    }

    #[test]
    fn codex_thread_rename_name_falls_back_to_result_message() {
        assert_eq!(
            codex_thread_rename_name_from_result(
                &serde_json::json!({}),
                "Codex thread renamed to Fallback name"
            ),
            Some("Fallback name".to_string())
        );
        assert_eq!(
            codex_thread_rename_name_from_result(
                &serde_json::json!({ "name": "Param name" }),
                "Codex thread renamed to Fallback name"
            ),
            Some("Param name".to_string())
        );
    }

    #[test]
    fn codex_thread_action_capabilities_cover_dashboard_actions() {
        let actions = codex_thread_action_capabilities();
        for action in [
            "fast",
            "side-close",
            "goal",
            "goal-get",
            "goal-edit",
            "goal-clear",
            "goal-pause",
            "goal-resume",
            "goal-complete",
            "goal-budget-limited",
        ] {
            assert!(
                actions.iter().any(|candidate| candidate == action),
                "missing dashboard Codex action capability: {}",
                action
            );
        }
    }

    #[test]
    fn native_capabilities_advertise_exactly_the_goal_family() {
        let caps = native_session_capabilities();
        assert!(caps.follow_up && caps.steer && caps.interrupt);
        let expected: Vec<String> = GOAL_THREAD_ACTION_OPS
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(caps.thread_actions, expected);
        // Every advertised op routes through the goal engine's dispatch.
        for op in &caps.thread_actions {
            assert!(goal_thread_action_op(op), "non-goal op advertised: {op}");
        }
        // The codex-named alias stays empty so codex-only UI never lights
        // up on native sessions.
        assert!(caps.codex_thread_actions.is_empty());
        assert!(caps.codex_fast_mode.is_none());
    }

    #[test]
    fn goal_fresh_tokens_excludes_cache_reads() {
        let usage = provider::TokenUsage {
            prompt_tokens: 1_000,
            completion_tokens: 200,
            total_tokens: 1_200,
            cached_tokens: 900,
            ..Default::default()
        };
        assert_eq!(goal_fresh_tokens(&usage), 300);
        // Degenerate provider reports (cached > prompt) saturate at output.
        let weird = provider::TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 400,
            ..Default::default()
        };
        assert_eq!(goal_fresh_tokens(&weird), 50);
    }

    #[test]
    fn claude_code_capabilities_advertise_universal_thread_actions() {
        let caps = claude_code_external_session_capabilities();
        for op in ["compact", "fork", "side", "goal", "goal-edit", "goal-clear"] {
            assert!(
                caps.thread_actions.iter().any(|candidate| candidate == op),
                "missing advertised Claude thread action: {op}"
            );
        }
        // Never advertise ops the adapter rejects.
        for op in ["fast", "review", "memory-reset", "undo"] {
            assert!(
                !caps.thread_actions.iter().any(|candidate| candidate == op),
                "advertised unsupported Claude thread action: {op}"
            );
        }
        // The codex-named alias stays empty so codex-only UI (tier chip,
        // fast toggle heuristics) never lights up on Claude sessions.
        assert!(caps.codex_thread_actions.is_empty());
        // Claude's ops must exist in the dashboard's action registry
        // (today the codex vocabulary) or the kebab could not render them —
        // EXCEPT ops the dashboard deliberately drives from the Launch-config
        // modal instead of the kebab (live apply on save).
        let modal_driven = ["model", "permission-mode"];
        for op in modal_driven {
            assert!(
                caps.thread_actions.iter().any(|candidate| candidate == op),
                "missing advertised modal-driven Claude thread action: {op}"
            );
        }
        let registry = codex_thread_action_capabilities();
        for op in &caps.thread_actions {
            if modal_driven.contains(&op.as_str()) {
                continue;
            }
            assert!(
                registry.contains(op),
                "op {op} missing from the dashboard action registry"
            );
        }
    }

    #[test]
    fn codex_service_tier_fast_mode_accepts_canonical_and_legacy_values() {
        assert!(codex_service_tier_is_fast(Some("priority")));
        assert!(codex_service_tier_is_fast(Some(" FAST ")));
        assert!(!codex_service_tier_is_fast(None));
        assert!(!codex_service_tier_is_fast(Some("")));
        assert!(!codex_service_tier_is_fast(Some("standard")));
        assert_eq!(codex_service_tier_value(Some("normal")), None);
        assert_eq!(
            codex_service_tier_value(Some(" FAST ")).as_deref(),
            Some("priority")
        );
    }

    #[test]
    fn codex_fast_slash_command_parses_for_steer_intercept() {
        let parsed = parse_codex_fast_slash_command(" /fast ")
            .expect("recognized slash command")
            .expect("valid slash command");
        assert_eq!(parsed.0, "fast");
        assert_eq!(parsed.1, serde_json::json!({}));
        assert!(parse_codex_fast_slash_command("/fork").is_none());

        let err = parse_codex_fast_slash_command("/fast now")
            .expect("recognized slash command")
            .unwrap_err();
        assert!(err.contains("does not accept arguments"), "got: {err}");
    }

    #[test]
    fn side_session_prompt_from_params_accepts_prompt_aliases() {
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!({ "prompt": "  quick question  " })),
            Some("quick question".to_string())
        );
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!({ "task": "check this" })),
            Some("check this".to_string())
        );
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!("inline prompt")),
            Some("inline prompt".to_string())
        );
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!({ "prompt": "   " })),
            None
        );
    }

    #[test]
    fn codex_subagent_thread_ids_uses_late_agent_state_ids() {
        let agents = vec![
            external_agent::SubAgentState {
                thread_id: " child-from-state ".to_string(),
                status: "completed".to_string(),
                message: None,
            },
            external_agent::SubAgentState {
                thread_id: "child-from-receiver".to_string(),
                status: "running".to_string(),
                message: None,
            },
        ];
        assert_eq!(
            codex_subagent_thread_ids(
                &[
                    " child-from-receiver ".to_string(),
                    String::new(),
                    "child-from-receiver".to_string(),
                ],
                &agents,
            ),
            vec![
                "child-from-receiver".to_string(),
                "child-from-state".to_string()
            ]
        );
    }

    #[test]
    fn codex_subagent_completed_is_not_terminal() {
        let state = external_agent::SubAgentState {
            thread_id: "child".to_string(),
            status: "completed".to_string(),
            message: Some("done".to_string()),
        };
        assert!(external_subagent_terminal_reason("Codex", &state).is_none());

        let running = external_agent::SubAgentState {
            thread_id: "child".to_string(),
            status: "running".to_string(),
            message: None,
        };
        assert!(external_subagent_terminal_reason("Codex", &running).is_none());
    }

    #[test]
    fn side_thread_ids_from_message_extracts_parent_child() {
        assert_eq!(
            side_thread_ids_from_message(
                "side conversation started in thread child-123 from parent parent-456"
            ),
            Some(("parent-456".to_string(), "child-123".to_string()))
        );
        assert_eq!(
            side_thread_ids_from_message("forked into thread child"),
            None
        );
        // The respawn path's side message must NOT parse as an in-process
        // side start — run_modes' codex-only child-drain branch keys off
        // this parser.
        let (_, respawn_message) = respawn_resume_thread_action(
            &EventBus::new(),
            "claude-code",
            Some("parent-abc".to_string()),
            "side",
            &serde_json::json!({ "prompt": "what changed?" }),
            std::path::Path::new("/tmp"),
            None,
        );
        assert_eq!(side_thread_ids_from_message(&respawn_message), None);
    }

    #[test]
    fn respawn_resume_side_sends_fork_resume_with_side_relationship() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (success, message) = respawn_resume_thread_action(
            &bus,
            "claude-code",
            Some("parent-abc".to_string()),
            "btw",
            &serde_json::json!({ "prompt": "what is the plan?" }),
            std::path::Path::new("/repo"),
            Some("claude".to_string()),
        );
        assert!(success, "side respawn failed: {message}");
        assert!(message.contains("side conversation"), "{message}");
        match rx.try_recv() {
            Ok(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                source,
                session_id,
                resume_id,
                task,
                fork,
                relationship_kind,
                direct,
                ..
            })) => {
                assert_eq!(source, "claude-code");
                assert_eq!(session_id, "parent-abc");
                assert_eq!(resume_id.as_deref(), Some("parent-abc"));
                assert!(fork, "side must respawn as a fork");
                assert_eq!(relationship_kind.as_deref(), Some("side"));
                assert_eq!(direct, Some(true));
                let task = task.expect("side carries the question as the first prompt");
                assert!(
                    task.contains("side conversation"),
                    "boundary missing: {task}"
                );
                assert!(
                    task.ends_with("what is the plan?"),
                    "question missing: {task}"
                );
                // The contract prologue is shared verbatim with Codex's
                // in-process side threads — anti-drift by construction.
                assert!(
                    task.starts_with(external_agent::SIDE_CONVERSATION_CONTRACT),
                    "prologue must be the shared side contract: {task}"
                );
                // Display surfaces recover the bare question from the blob.
                assert_eq!(
                    side_respawn_display_task(&task).as_deref(),
                    Some("what is the plan?")
                );
            }
            other => panic!("expected ResumeSession, got {other:?}"),
        }
    }

    #[test]
    fn side_respawn_display_task_only_matches_composed_prompts() {
        assert_eq!(side_respawn_display_task("fix the login bug"), None);
        assert_eq!(
            side_respawn_display_task(external_agent::SIDE_CONVERSATION_CONTRACT),
            None,
            "a contract with no question must not display as an empty task"
        );
        assert_eq!(side_respawn_display_task(&side_respawn_prompt("")), None);
    }

    #[test]
    fn respawn_resume_side_requires_a_prompt_and_a_thread_id() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (success, message) = respawn_resume_thread_action(
            &bus,
            "claude-code",
            Some("parent-abc".to_string()),
            "side",
            &serde_json::Value::Null,
            std::path::Path::new("/repo"),
            None,
        );
        assert!(!success);
        assert!(message.contains("Usage"), "{message}");
        let (success, message) = respawn_resume_thread_action(
            &bus,
            "claude-code",
            None,
            "side",
            &serde_json::json!({ "prompt": "q" }),
            std::path::Path::new("/repo"),
            None,
        );
        assert!(!success);
        assert!(message.contains("native session id"), "{message}");
        assert!(
            rx.try_recv().is_err(),
            "failures must not send ResumeSession"
        );
    }

    #[test]
    fn respawn_resume_fork_stays_bare() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (success, _) = respawn_resume_thread_action(
            &bus,
            "claude-code",
            Some("parent-abc".to_string()),
            "fork",
            &serde_json::Value::Null,
            std::path::Path::new("/repo"),
            None,
        );
        assert!(success);
        match rx.try_recv() {
            Ok(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                task,
                fork,
                relationship_kind,
                ..
            })) => {
                assert!(fork);
                assert_eq!(task, None, "a bare fork idles until prompted");
                assert_eq!(relationship_kind, None);
            }
            other => panic!("expected ResumeSession, got {other:?}"),
        }
    }

    #[test]
    fn scoped_codex_subagent_events_match_known_child_threads() {
        let mut stats = LoopStats::default();
        stats
            .codex_subagent_parent_threads
            .insert("child-thread".to_string(), "parent-thread".to_string());

        assert_eq!(
            scoped_event_codex_subagent_thread_id(&Some(" child-thread ".to_string()), &stats),
            Some("child-thread".to_string())
        );
        assert_eq!(
            scoped_event_codex_subagent_thread_id(&Some("parent-thread".to_string()), &stats),
            None
        );
        assert_eq!(scoped_event_codex_subagent_thread_id(&None, &stats), None);
    }

    #[test]
    fn idle_codex_subagent_turn_completed_marks_child_ready() {
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
            session_id: Some("parent-thread".to_string()),
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
        let mut stats = LoopStats::default();

        handle_idle_codex_subagent_event(
            &config,
            &mut stats,
            "child-thread".to_string(),
            external_agent::AgentEvent::TurnCompleted {
                message: Some("child final answer".to_string()),
            },
        );

        match rx.try_recv().expect("child final message") {
            AppEvent::LogEntry {
                session_id,
                content,
                ..
            } => {
                assert_eq!(session_id.as_deref(), Some("child-thread"));
                assert_eq!(content, "child final answer");
            }
            other => panic!("expected child final LogEntry, got {:?}", other),
        }

        match rx.try_recv().expect("child round completion") {
            AppEvent::LogEntry {
                session_id,
                content,
                ..
            } => {
                assert_eq!(session_id.as_deref(), Some("child-thread"));
                assert_eq!(
                    content,
                    "Round complete: subagent conversation ready for follow-up"
                );
            }
            other => panic!("expected child completion LogEntry, got {:?}", other),
        }
    }

    #[test]
    fn claude_task_subagent_registration_is_claude_flavored() {
        // The universal sub-agent rail, driven by a Claude Code drain:
        // identity carries the claude-code source, the child advertises no
        // follow-up (in-band tasks are fire-and-forget), labels say
        // "Claude Code", and no fission observation is recorded (fission is
        // Codex managed-context).
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
            session_id: Some("cc-parent".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Claude Code".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let mut stats = LoopStats::default();

        let children = vec!["task-ABC123".to_string()];
        record_codex_fission_observation(
            &config,
            CodexFissionObservationInput {
                item_id: "toolu_01x",
                tool: "Agent",
                status: "inProgress",
                sender_thread_id: "cc-parent",
                subagent_thread_ids: &children,
                prompt: Some("probe"),
                model: None,
                reasoning_effort: None,
                agents: &[],
            },
        );
        assert!(
            !fission_ledger::ledger_path(&log_dir).exists(),
            "Claude Code sub-agents must not record fission observations"
        );

        register_external_subagent_children(
            &config,
            &mut stats,
            "cc-parent",
            &children,
            Some("probe echo"),
            None,
            None,
        );
        assert_eq!(
            stats.codex_subagent_parent_threads.get("task-ABC123"),
            Some(&"cc-parent".to_string())
        );

        match rx.try_recv().expect("identity") {
            AppEvent::SessionIdentity {
                session_id, source, ..
            } => {
                assert_eq!(session_id, "task-ABC123");
                assert_eq!(source, "claude-code");
            }
            other => panic!("expected SessionIdentity, got {other:?}"),
        }
        match rx.try_recv().expect("relationship") {
            AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            } => {
                assert_eq!(parent_session_id, "cc-parent");
                assert_eq!(child_session_id, "task-ABC123");
                assert_eq!(relationship, "subagent");
                assert!(!ephemeral);
            }
            other => panic!("expected SessionRelationship, got {other:?}"),
        }
        match rx.try_recv().expect("capabilities") {
            AppEvent::SessionCapabilities { capabilities, .. } => {
                assert!(
                    !capabilities.follow_up,
                    "in-band Claude Code tasks accept no follow-ups"
                );
            }
            other => panic!("expected SessionCapabilities, got {other:?}"),
        }
        match rx.try_recv().expect("started") {
            AppEvent::SessionStarted { session_id, task } => {
                assert_eq!(session_id, "task-ABC123");
                assert_eq!(task.as_deref(), Some("probe echo"));
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
        match rx.try_recv().expect("log line") {
            AppEvent::LogEntry {
                source, content, ..
            } => {
                assert_eq!(source, "Claude Code");
                assert!(
                    content.starts_with("Claude Code subagent started"),
                    "{content}"
                );
            }
            other => panic!("expected LogEntry, got {other:?}"),
        }
    }

    #[test]
    fn idle_codex_subagent_tool_output_omits_middle_and_keeps_completion_tail() {
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
            session_id: Some("parent-thread".to_string()),
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
        let mut stats = LoopStats::default();

        handle_idle_codex_subagent_event(
            &config,
            &mut stats,
            "child-thread".to_string(),
            external_agent::AgentEvent::ToolOutputDelta {
                item_id: "call-1".to_string(),
                text: format!(
                    "BEGIN\n{}END-MARKER\n",
                    "middle\n".repeat(EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT)
                ),
            },
        );

        match rx.try_recv().expect("first capped output") {
            AppEvent::AgentOutput {
                session_id, stdout, ..
            } => {
                assert_eq!(session_id.as_deref(), Some("child-thread"));
                assert!(stdout.starts_with("BEGIN\n"));
                assert!(stdout.contains("omitting additional external tool output"));
                assert!(!stdout.contains("END-MARKER"));
            }
            other => panic!("expected child AgentOutput, got {:?}", other),
        }

        handle_idle_codex_subagent_event(
            &config,
            &mut stats,
            "child-thread".to_string(),
            external_agent::AgentEvent::ToolOutputDelta {
                item_id: "call-1".to_string(),
                text: "more".to_string(),
            },
        );

        assert!(
            rx.try_recv().is_err(),
            "second delta after per-tool cap should be suppressed"
        );

        handle_idle_codex_subagent_event(
            &config,
            &mut stats,
            "child-thread".to_string(),
            external_agent::AgentEvent::ToolCompleted {
                item_id: "call-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            },
        );

        match rx.try_recv().expect("completion tail") {
            AppEvent::AgentOutput {
                session_id, stdout, ..
            } => {
                assert_eq!(session_id.as_deref(), Some("child-thread"));
                assert!(stdout.contains("bytes from the middle"));
                assert!(stdout.contains("END-MARKER"));
                assert!(stdout.contains("more"));
            }
            other => panic!("expected child completion AgentOutput, got {:?}", other),
        }
    }

    #[test]
    fn side_rewind_first_turn_for_undo_stays_inside_side_boundary() {
        assert_eq!(
            side_rewind_first_turn_for_undo(3, 1, "side-child").unwrap(),
            3
        );
        assert_eq!(
            side_rewind_first_turn_for_undo(3, 3, "side-child").unwrap(),
            1
        );

        let zero = side_rewind_first_turn_for_undo(3, 0, "side-child").unwrap_err();
        assert!(zero.contains("at least 1"), "got: {zero}");

        let beyond = side_rewind_first_turn_for_undo(3, 4, "side-child").unwrap_err();
        assert!(beyond.contains("after the /side boundary"), "got: {beyond}");
    }

    #[test]
    fn parent_rewind_first_turn_for_undo_tracks_active_turn_count() {
        assert_eq!(parent_rewind_first_turn_for_undo(3, 1).unwrap(), 3);
        assert_eq!(parent_rewind_first_turn_for_undo(3, 2).unwrap(), 2);

        let zero = parent_rewind_first_turn_for_undo(3, 0).unwrap_err();
        assert!(zero.contains("at least 1"), "got: {zero}");

        let beyond = parent_rewind_first_turn_for_undo(3, 4).unwrap_err();
        assert!(beyond.contains("only 3 user turn"), "got: {beyond}");
    }

    #[test]
    fn codex_thread_action_log_compacts_rewind_anchor_catalog() {
        let message = serde_json::json!({
            "total": 42,
            "filtered_total": 12,
            "offset": 5,
            "limit": 5,
            "next_offset": 10,
            "anchors": [
                { "item_id": "call_a", "summary": "a".repeat(500) },
                { "item_id": "call_b", "summary": "b".repeat(500) }
            ]
        })
        .to_string();

        let compact = codex_thread_action_log_message("list_rewind_anchors", &message);
        assert_eq!(
            compact,
            format!(
                "{{\"anchors\":2,\"total\":42,\"filtered_total\":12,\"offset\":5,\"limit\":5,\"next_offset\":10,\"bytes\":{}}}",
                message.len()
            )
        );
        assert!(!compact.contains("call_a"));
        assert!(!compact.contains(&"a".repeat(20)));
    }

    #[test]
    fn codex_thread_action_dedupe_rejects_replayed_broadcast_ids() {
        let mut dedupe = CodexThreadActionDedupe::default();
        assert!(dedupe.mark_seen("request-1"));
        assert!(!dedupe.mark_seen("request-1"));
        assert!(dedupe.mark_seen("request-2"));
    }

    #[test]
    fn context_rewind_backout_mode_supports_restore_aliases() {
        assert_eq!(
            context_rewind_backout_mode("rewind_backout", &serde_json::json!({})),
            "inspect"
        );
        assert_eq!(
            context_rewind_backout_mode(
                "rewind_backout",
                &serde_json::json!({"mode": " restore "})
            ),
            "restore"
        );
        assert_eq!(
            context_rewind_backout_mode("rewind-restore", &serde_json::json!({})),
            "restore"
        );
        assert!(is_context_rewind_backout_action("context-rewind-restore"));
    }

    #[test]
    fn context_rewind_backout_parses_legacy_cache_reset_aliases() {
        assert!(!context_rewind_allows_cache_reset(&serde_json::json!({})));
        assert!(!context_rewind_allows_cache_reset(&serde_json::json!({
            "allowCacheReset": false,
        })));
        assert!(context_rewind_allows_cache_reset(&serde_json::json!({
            "allowCacheReset": true,
        })));
        assert!(context_rewind_allows_cache_reset(&serde_json::json!({
            "allow_cache_breaking_fork": true,
        })));
    }

    #[test]
    fn thread_action_params_with_thread_id_targets_clicked_window() {
        let params = thread_action_params_with_thread_id(
            "fork",
            serde_json::json!({ "name": "Parent fork" }),
            Some("parent-thread"),
        );
        assert_eq!(params["threadId"], "parent-thread");
        assert_eq!(params["name"], "Parent fork");

        let explicit = thread_action_params_with_thread_id(
            "fork",
            serde_json::json!({ "threadId": "explicit-thread" }),
            Some("parent-thread"),
        );
        assert_eq!(explicit["threadId"], "explicit-thread");

        let side_prompt = thread_action_params_with_thread_id(
            "side",
            serde_json::json!("quick check"),
            Some("parent-thread"),
        );
        assert_eq!(side_prompt["threadId"], "parent-thread");
        assert_eq!(side_prompt["prompt"], "quick check");
    }

    // ---- fission supervisor core ----

    fn fission_test_drain_config<'a>(
        bus: &'a EventBus,
        session_log: &'a SharedSessionLog,
        approval_registry: &'a event::ApprovalRegistry,
        context_injection: &'a event::ContextInjectionQueue,
        project_root: &'a Path,
        log_dir: &'a Path,
        session_id: &str,
    ) -> DrainConfig<'a> {
        DrainConfig {
            bus,
            web_port: None,
            session_id: Some(session_id.to_string()),
            alias_session_id: None,
            backend_thread_id: Some(session_id.to_string()),
            autonomy: autonomy::shared_autonomy(AutonomyState::default()),
            session_log,
            project_root,
            log_dir,
            approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: true,
            persist_model_responses_inline: false,
            headless: true,
            context_injection,
        }
    }

    /// Write a synthetic Codex rollout: one `function_call` response item per
    /// `(call_id, name)` pair, one rollout line each.
    fn write_fission_test_rollout(path: &Path, items: &[(&str, &str)]) {
        let mut out = String::new();
        for (call_id, name) in items {
            out.push_str(
                &serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": "{}",
                    }
                })
                .to_string(),
            );
            out.push('\n');
        }
        std::fs::write(path, out).unwrap();
    }

    fn init_fission_test_git_repo(root: &Path) {
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(root.join("README.md"), "# fission test\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "initial"]);
    }

    #[test]
    fn fission_action_routing_helpers_match_contract_ops() {
        assert!(is_fission_spawn_action("fission_spawn"));
        assert!(is_fission_spawn_action("fission-spawn"));
        assert!(!is_fission_spawn_action("fission_import"));
        assert!(is_fission_import_action("fission_import"));
        assert!(is_fission_import_action("fission-import"));
        assert!(!is_fission_import_action("fission_spawn"));
        assert!(!is_fission_spawn_action("fork"));
    }

    #[test]
    fn fission_spawn_branch_specs_validate_count_and_objectives() {
        let specs = fission_spawn_branch_specs_from_params(&serde_json::json!({
            "branches": [
                { "objective": "fix the parser", "write_scope": ["src/parser.rs", " "], "name": "Parser fix" },
                { "objective": "  survey docs  ", "write_scope": "docs/" },
                { "objective": "third" },
            ]
        }))
        .unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].objective, "fix the parser");
        assert_eq!(specs[0].write_scope, vec!["src/parser.rs".to_string()]);
        assert_eq!(specs[0].name.as_deref(), Some("Parser fix"));
        assert_eq!(specs[1].objective, "survey docs");
        assert_eq!(specs[1].write_scope, vec!["docs/".to_string()]);
        assert!(specs[2].write_scope.is_empty());
        assert_eq!(specs[2].name, None);

        assert!(fission_spawn_branch_specs_from_params(&serde_json::json!({})).is_err());
        assert!(
            fission_spawn_branch_specs_from_params(&serde_json::json!({ "branches": [] })).is_err()
        );
        assert!(fission_spawn_branch_specs_from_params(&serde_json::json!({
            "branches": [{}, {}, {}, {}, {}]
        }))
        .is_err());
        assert!(fission_spawn_branch_specs_from_params(&serde_json::json!({
            "branches": [{ "objective": "   " }]
        }))
        .is_err());
    }

    #[test]
    fn fission_worktree_default_matrix() {
        // Default: write scope AND git repo.
        assert!(fission_branch_uses_worktree(true, true, None));
        assert!(!fission_branch_uses_worktree(true, false, None));
        assert!(!fission_branch_uses_worktree(false, true, None));
        assert!(!fission_branch_uses_worktree(false, false, None));
        // use_worktree overrides both ways.
        assert!(!fission_branch_uses_worktree(true, true, Some(false)));
        assert!(fission_branch_uses_worktree(false, false, Some(true)));
        assert!(fission_branch_uses_worktree(false, true, Some(true)));
        assert!(!fission_branch_uses_worktree(false, true, Some(false)));
    }

    #[test]
    fn fission_branch_git_name_uses_group_hash_tail() {
        let group_id = fission_ledger::group_id("parent-thread", "call-7");
        let hash_tail = group_id.rsplit('-').next().unwrap();
        let name = fission_branch_git_name(&group_id, 2);
        assert_eq!(
            name,
            format!("fission/{}-2", &hash_tail[..8.min(hash_tail.len())])
        );
        assert!(name.starts_with("fission/"));
    }

    #[test]
    fn fission_mid_turn_anchor_prefers_most_recent_inflight_spawn_item() {
        let mut active = HashSet::new();
        let mut previews = HashMap::new();
        let mut seq = HashMap::new();
        let add = |id: &str,
                   preview: &str,
                   order: u64,
                   active: &mut HashSet<String>,
                   previews: &mut HashMap<String, String>,
                   seq: &mut HashMap<String, u64>| {
            active.insert(id.to_string());
            previews.insert(id.to_string(), preview.to_string());
            seq.insert(id.to_string(), order);
        };
        // No matching item: nothing captured.
        add(
            "item_shell",
            "command: git status",
            1,
            &mut active,
            &mut previews,
            &mut seq,
        );
        assert_eq!(
            most_recent_inflight_fission_spawn_tool_item(&active, &previews, &seq),
            None
        );
        // Two in-flight fission_spawn MCP calls: the most recently started wins.
        add(
            "item_spawn_old",
            "mcp: intendant:fission_spawn {\"branches\":[…]}",
            2,
            &mut active,
            &mut previews,
            &mut seq,
        );
        add(
            "item_spawn_new",
            "mcp: intendant:fission_spawn {\"branches\":[…]}",
            3,
            &mut active,
            &mut previews,
            &mut seq,
        );
        // A non-mcp tool mentioning fission_spawn must not match.
        add(
            "item_grep",
            "command: rg fission_spawn src/",
            4,
            &mut active,
            &mut previews,
            &mut seq,
        );
        assert_eq!(
            most_recent_inflight_fission_spawn_tool_item(&active, &previews, &seq).as_deref(),
            Some("item_spawn_new")
        );
        // A completed item (no longer active) is not considered.
        active.remove("item_spawn_new");
        previews.remove("item_spawn_new");
        seq.remove("item_spawn_new");
        assert_eq!(
            most_recent_inflight_fission_spawn_tool_item(&active, &previews, &seq).as_deref(),
            Some("item_spawn_old")
        );

        // Param injection only fills a missing anchor.
        let injected = fission_params_with_anchor_item_id(
            serde_json::json!({ "branches": [] }),
            "item_spawn_old",
        );
        assert_eq!(
            fission_anchor_item_id_from_params(&injected).as_deref(),
            Some("item_spawn_old")
        );
    }

    #[test]
    fn fission_catalog_anchor_prefers_named_spawn_then_head_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_fission_test_rollout(
            &rollout,
            &[
                ("call_1", "shell"),
                ("call_2", "intendant.fission_spawn"),
                ("call_3", "intendant.fission_spawn"),
                ("call_4", "shell"),
            ],
        );
        let anchors = scan_context_rewind_anchor_catalog(&rollout).unwrap();
        let resolved = fission_spawn_anchor_from_catalog(&anchors).unwrap();
        // Newest fission_spawn-named anchor wins, not the head.
        assert_eq!(resolved.item_id, "call_3");
        assert!(!resolved.head_fallback);

        // No fission_spawn names anywhere: head fallback with honest flag.
        write_fission_test_rollout(&rollout, &[("call_1", "shell"), ("call_2", "shell")]);
        let anchors = scan_context_rewind_anchor_catalog(&rollout).unwrap();
        let resolved = fission_spawn_anchor_from_catalog(&anchors).unwrap();
        assert_eq!(resolved.item_id, "call_2");
        assert!(resolved.head_fallback);

        // Empty catalog: nothing to anchor to.
        assert!(fission_spawn_anchor_from_catalog(&[]).is_none());
    }

    #[test]
    fn fission_detach_predicate_math_on_synthetic_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_fission_test_rollout(
            &rollout,
            &[
                ("call_early", "shell"),
                ("call_mid", "shell"),
                ("call_late", "shell"),
            ],
        );
        let anchors = scan_context_rewind_anchor_catalog(&rollout).unwrap();
        let first_lines = fission_anchor_first_lines(&anchors);
        assert_eq!(first_lines.get("call_early"), Some(&1));
        assert_eq!(first_lines.get("call_mid"), Some(&2));
        assert_eq!(first_lines.get("call_late"), Some(&3));

        // position=after on call_mid: the cut keeps lines 1..=2.
        let cut_after = fission_anchor_cut_line(
            &anchors,
            "call_mid",
            external_agent::RollbackAnchorPosition::After,
        )
        .unwrap();
        assert_eq!(cut_after, 2);
        let after = external_agent::RollbackAnchorPosition::After;
        assert!(fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_after,
            after,
            "call_early"
        ));
        assert!(fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_after,
            after,
            "call_mid"
        ));
        assert!(!fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_after,
            after,
            "call_late"
        ));

        // position=before on call_mid: the cut prunes line 2 and beyond.
        let cut_before = fission_anchor_cut_line(
            &anchors,
            "call_mid",
            external_agent::RollbackAnchorPosition::Before,
        )
        .unwrap();
        assert_eq!(cut_before, 2);
        let before = external_agent::RollbackAnchorPosition::Before;
        assert!(fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_before,
            before,
            "call_early"
        ));
        assert!(!fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_before,
            before,
            "call_mid"
        ));
        assert!(!fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_before,
            before,
            "call_late"
        ));

        // Absent anchors are unreachable in either direction.
        assert!(!fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_after,
            after,
            "call_unknown"
        ));
        assert!(!fission_anchor_reachable_after_rewind(
            &first_lines,
            cut_before,
            before,
            "call_unknown"
        ));

        // Unknown cut anchor yields no cut line at all.
        assert!(fission_anchor_cut_line(
            &anchors,
            "call_unknown",
            external_agent::RollbackAnchorPosition::After
        )
        .is_none());
    }

    /// External-agent mock for the fission flows: records live-thread forks,
    /// developer-message injections, and anchored rollbacks; serves a fixed
    /// rollout path as the thread snapshot.
    struct FissionTestAgent {
        rollout_path: Option<PathBuf>,
        child_id_prefix: String,
        forks: Arc<Mutex<Vec<(String, Option<String>, Option<PathBuf>)>>>,
        injected: Arc<Mutex<Vec<(String, String)>>>,
        rollbacks: Arc<Mutex<Vec<(String, String, &'static str)>>>,
        fail_fork_for_name_containing: Option<String>,
        fail_rollback: bool,
    }

    impl FissionTestAgent {
        fn new(rollout_path: Option<PathBuf>, child_id_prefix: &str) -> Self {
            Self {
                rollout_path,
                child_id_prefix: child_id_prefix.to_string(),
                forks: Arc::new(Mutex::new(Vec::new())),
                injected: Arc::new(Mutex::new(Vec::new())),
                rollbacks: Arc::new(Mutex::new(Vec::new())),
                fail_fork_for_name_containing: None,
                fail_rollback: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl external_agent::ExternalAgent for FissionTestAgent {
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
            _thread: &external_agent::AgentThread,
            _message: &str,
        ) -> Result<(), CallerError> {
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

        fn supports_item_anchor_rewind(&self) -> bool {
            true
        }

        async fn read_thread_snapshot(
            &mut self,
            thread_id: &str,
        ) -> Result<external_agent::AgentThreadSnapshot, CallerError> {
            Ok(external_agent::AgentThreadSnapshot {
                thread_id: thread_id.to_string(),
                rollout_path: self.rollout_path.clone(),
            })
        }

        async fn fork_thread_with_options(
            &mut self,
            thread_id: &str,
            name: Option<&str>,
            cwd: Option<&Path>,
        ) -> Result<external_agent::AgentThread, CallerError> {
            if let Some(needle) = self.fail_fork_for_name_containing.as_deref() {
                if name.is_some_and(|name| name.contains(needle)) {
                    return Err(CallerError::ExternalAgent(
                        "thread/fork refused by test".to_string(),
                    ));
                }
            }
            let mut forks = self.forks.lock().unwrap();
            forks.push((
                thread_id.to_string(),
                name.map(str::to_string),
                cwd.map(Path::to_path_buf),
            ));
            Ok(external_agent::AgentThread {
                thread_id: format!("{}-{}", self.child_id_prefix, forks.len()),
            })
        }

        async fn rollback_thread_to_item_anchor(
            &mut self,
            thread_id: &str,
            item_id: &str,
            position: external_agent::RollbackAnchorPosition,
        ) -> Result<(), CallerError> {
            if self.fail_rollback {
                return Err(CallerError::ExternalAgent(
                    "thread/rollback refused by test".to_string(),
                ));
            }
            self.rollbacks.lock().unwrap().push((
                thread_id.to_string(),
                item_id.to_string(),
                position.as_str(),
            ));
            Ok(())
        }

        async fn inject_thread_developer_message(
            &mut self,
            thread_id: &str,
            message: &str,
        ) -> Result<(), CallerError> {
            self.injected
                .lock()
                .unwrap()
                .push((thread_id.to_string(), message.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn apply_fission_spawn_action_spawns_branches_with_worktree_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        init_fission_test_git_repo(&project_root);
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            &project_root,
            &log_dir,
            "spawn-parent-a",
        );

        let test_agent = FissionTestAgent::new(None, "spawnA-child");
        let forks = test_agent.forks.clone();
        let injected = test_agent.injected.clone();
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);

        let params = serde_json::json!({
            "threadId": "spawn-parent-a",
            "anchor_item_id": "call_anchor_a",
            "branches": [
                { "objective": "fix the parser", "write_scope": ["src/parser.rs"] },
                { "objective": "survey the docs", "name": "Docs survey" },
            ],
        });
        let message = apply_fission_spawn_action(&mut agent, &params, &config)
            .await
            .expect("spawn succeeds");
        assert!(message.contains("spawned 2/2"), "message: {message}");

        let group_id = fission_ledger::group_id("spawn-parent-a", "call_anchor_a");

        // Fork calls: branch 1 in a worktree (write scope + git repo), branch
        // 2 sharing the parent checkout.
        let forks = forks.lock().unwrap();
        assert_eq!(forks.len(), 2);
        assert_eq!(forks[0].0, "spawn-parent-a");
        let worktree_path = forks[0].2.clone().expect("branch 1 forked into worktree");
        assert!(worktree_path.exists());
        assert!(worktree_path
            .to_string_lossy()
            .contains(&fission_branch_git_name(&group_id, 1)));
        assert_eq!(forks[1].2, None, "branch 2 must not get a worktree");
        assert_eq!(forks[1].1.as_deref(), Some("Docs survey"));

        // Charters: injected into each child as developer messages.
        let injected = injected.lock().unwrap();
        assert_eq!(injected.len(), 2);
        assert_eq!(injected[0].0, "spawnA-child-1");
        assert!(injected[0].1.starts_with("<fission_charter>"));
        assert!(injected[0].1.contains(&format!("group_id: {group_id}")));
        assert!(injected[0].1.contains("branch_session_id: spawnA-child-1"));
        assert!(injected[0].1.contains("objective: fix the parser"));
        assert!(injected[0].1.contains("owned write scope: src/parser.rs"));
        assert!(injected[0].1.contains("worktree: "));
        assert!(injected[0].1.contains("claim_fission_canonical"));
        assert!(injected[0]
            .1
            .contains("end your turn with a concise outcome summary"));
        assert!(injected[1].1.contains("owned write scope: read-only"));
        assert!(!injected[1].1.contains("worktree: "));

        // Ledger: group with fission_spawn provenance, both branches running,
        // per-branch charters with the joined write scope.
        let document = fission_ledger::read_fission_ledger_document(&log_dir)
            .unwrap()
            .expect("ledger document");
        let group = document
            .groups
            .iter()
            .find(|group| group.group_id == group_id)
            .expect("group registered");
        assert_eq!(group.tool, "fission_spawn");
        assert_eq!(group.branches.len(), 2);
        let branch_1 = group
            .branches
            .iter()
            .find(|branch| branch.session_id == "spawnA-child-1")
            .expect("branch 1");
        assert_eq!(branch_1.status, "running");
        assert_eq!(
            branch_1.worktree_path.as_deref(),
            Some(worktree_path.as_path())
        );
        assert_eq!(
            branch_1.task.as_deref(),
            Some("Begin your fission charter: fix the parser")
        );
        let charter_1 = document
            .branch_ext(&group_id, "spawnA-child-1")
            .and_then(|ext| ext.charter.as_ref())
            .expect("branch 1 charter");
        assert_eq!(charter_1.objective, "fix the parser");
        assert_eq!(charter_1.write_scope.as_deref(), Some("src/parser.rs"));
        assert!(charter_1.worktree_requested);
        let charter_2 = document
            .branch_ext(&group_id, "spawnA-child-2")
            .and_then(|ext| ext.charter.as_ref())
            .expect("branch 2 charter");
        assert_eq!(charter_2.write_scope, None);
        assert!(!charter_2.worktree_requested);

        // Lifecycle routes registered for both branches.
        let route = fission_lifecycle::branch_route("spawnA-child-1").expect("route 1");
        assert_eq!(route.group_id, group_id);
        assert_eq!(route.log_dir, log_dir);
        assert!(fission_lifecycle::branch_route("spawnA-child-2").is_some());

        // Frontend wiring: rename + fission-branch relationship + resumed
        // kickoff turn per branch, with the worktree as branch 1's root.
        let mut renames = Vec::new();
        let mut relationships = Vec::new();
        let mut resumes = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::ControlCommand(event::ControlMsg::RenameSession {
                    session_id,
                    name,
                    ..
                }) => renames.push((session_id, name)),
                AppEvent::SessionRelationship {
                    parent_session_id,
                    child_session_id,
                    relationship,
                    ..
                } => relationships.push((parent_session_id, child_session_id, relationship)),
                AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                    session_id,
                    project_root,
                    task,
                    direct,
                    ..
                }) => resumes.push((session_id, project_root, task, direct)),
                _ => {}
            }
        }
        assert_eq!(renames.len(), 2);
        assert!(renames
            .iter()
            .any(|(id, name)| id == "spawnA-child-2" && name == "Docs survey"));
        assert_eq!(
            relationships
                .iter()
                .filter(|(parent, _, relationship)| parent == "spawn-parent-a"
                    && relationship == "fission-branch")
                .count(),
            2
        );
        assert_eq!(resumes.len(), 2);
        let resume_1 = resumes
            .iter()
            .find(|(id, ..)| id == "spawnA-child-1")
            .expect("resume for branch 1");
        assert_eq!(
            resume_1.1.as_deref(),
            Some(worktree_path.to_string_lossy().to_string().as_str())
        );
        assert_eq!(
            resume_1.2.as_deref(),
            Some("Begin your fission charter: fix the parser")
        );
        assert_eq!(resume_1.3, Some(true));
        let resume_2 = resumes
            .iter()
            .find(|(id, ..)| id == "spawnA-child-2")
            .expect("resume for branch 2");
        assert_eq!(
            resume_2.1.as_deref(),
            Some(project_root.to_string_lossy().to_string().as_str())
        );

        fission_lifecycle::drop_pending_deliveries(&[group_id]);
    }

    #[tokio::test]
    async fn apply_fission_spawn_action_honors_worktree_override_and_reports_failures() {
        // Non-git project root: the default would skip worktrees, but the
        // explicit override forces the attempt, which fails honestly and
        // counts as a failed branch.
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("plain");
        std::fs::create_dir_all(&project_root).unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        let bus = EventBus::new();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            &project_root,
            &log_dir,
            "spawn-parent-b",
        );

        let test_agent = FissionTestAgent::new(None, "spawnB-child");
        let forks = test_agent.forks.clone();
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);

        let params = serde_json::json!({
            "threadId": "spawn-parent-b",
            "anchor_item_id": "call_anchor_b",
            "use_worktree": true,
            "branches": [{ "objective": "doomed worktree branch" }],
        });
        let err = apply_fission_spawn_action(&mut agent, &params, &config)
            .await
            .expect_err("all branches failed");
        assert!(err.contains("spawned 0/1"), "error: {err}");
        assert!(err.contains("worktree creation failed"), "error: {err}");
        assert!(forks.lock().unwrap().is_empty(), "no fork without worktree");

        // Git repo + write scope, but use_worktree=false suppresses isolation.
        init_fission_test_git_repo(&project_root);
        let params = serde_json::json!({
            "threadId": "spawn-parent-b",
            "anchor_item_id": "call_anchor_b2",
            "use_worktree": false,
            "branches": [{ "objective": "shared checkout branch", "write_scope": ["src/"] }],
        });
        let message = apply_fission_spawn_action(&mut agent, &params, &config)
            .await
            .expect("spawn succeeds");
        assert!(message.contains("spawned 1/1"));
        assert_eq!(forks.lock().unwrap().last().unwrap().2, None);

        let group_b2 = fission_ledger::group_id("spawn-parent-b", "call_anchor_b2");
        fission_lifecycle::drop_pending_deliveries(&[group_b2]);
    }

    #[tokio::test]
    async fn apply_fission_spawn_action_partial_failure_removes_failed_branch_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        init_fission_test_git_repo(&project_root);
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        let bus = EventBus::new();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            &project_root,
            &log_dir,
            "spawn-parent-c",
        );

        let mut test_agent = FissionTestAgent::new(None, "spawnC-child");
        test_agent.fail_fork_for_name_containing = Some("doomed".to_string());
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);

        let params = serde_json::json!({
            "threadId": "spawn-parent-c",
            "anchor_item_id": "call_anchor_c",
            "branches": [
                { "objective": "healthy branch", "write_scope": ["src/a.rs"] },
                { "objective": "doomed branch", "write_scope": ["src/b.rs"], "name": "doomed fork" },
            ],
        });
        let message = apply_fission_spawn_action(&mut agent, &params, &config)
            .await
            .expect("partial success is Ok");
        assert!(message.contains("spawned 1/2"), "message: {message}");
        assert!(message.contains("branch 1: spawned thread spawnC-child-1"));
        assert!(message.contains("branch 2: FAILED"));
        assert!(message.contains("live-thread fork failed"));

        let group_id = fission_ledger::group_id("spawn-parent-c", "call_anchor_c");
        // The failed branch's worktree was removed; the healthy one remains.
        let worktrees_root = project_root.join(".intendant").join("worktrees");
        assert!(worktrees_root
            .join(fission_branch_git_name(&group_id, 1))
            .exists());
        assert!(!worktrees_root
            .join(fission_branch_git_name(&group_id, 2))
            .exists());

        // Only the healthy branch made it into the ledger and the registry.
        let document = fission_ledger::read_fission_ledger_document(&log_dir)
            .unwrap()
            .expect("ledger document");
        let group = document
            .groups
            .iter()
            .find(|group| group.group_id == group_id)
            .expect("group");
        assert_eq!(group.branches.len(), 1);
        assert_eq!(group.branches[0].session_id, "spawnC-child-1");
        assert!(fission_lifecycle::branch_route("spawnC-child-2").is_none());

        fission_lifecycle::drop_pending_deliveries(&[group_id]);
    }

    #[tokio::test]
    async fn apply_fission_spawn_action_falls_back_to_catalog_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("plain");
        std::fs::create_dir_all(&project_root).unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_fission_test_rollout(
            &rollout,
            &[
                ("call_1", "shell"),
                ("call_spawn", "intendant.fission_spawn"),
                ("call_tail", "shell"),
            ],
        );

        let bus = EventBus::new();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            &project_root,
            &log_dir,
            "spawn-parent-d",
        );

        // Preference 2: the newest anchor named for fission_spawn.
        let test_agent = FissionTestAgent::new(Some(rollout.clone()), "spawnD-child");
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);
        let params = serde_json::json!({
            "threadId": "spawn-parent-d",
            "branches": [{ "objective": "anchor by name" }],
        });
        apply_fission_spawn_action(&mut agent, &params, &config)
            .await
            .expect("spawn succeeds");
        let named_group = fission_ledger::group_id("spawn-parent-d", "call_spawn");
        let document = fission_ledger::read_fission_ledger_document(&log_dir)
            .unwrap()
            .expect("document");
        let group = document
            .groups
            .iter()
            .find(|group| group.group_id == named_group)
            .expect("group anchored at the fission_spawn call");
        assert_eq!(group.anchor_item_id, "call_spawn");
        assert_eq!(group.tool, "fission_spawn");

        // Preference 3: no fission_spawn-named anchor anywhere → catalog head
        // with `fission_spawn:head` provenance.
        write_fission_test_rollout(&rollout, &[("call_1", "shell"), ("call_head", "shell")]);
        let params = serde_json::json!({
            "threadId": "spawn-parent-d2",
            "branches": [{ "objective": "anchor at head" }],
        });
        let test_agent = FissionTestAgent::new(Some(rollout.clone()), "spawnD2-child");
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);
        apply_fission_spawn_action(&mut agent, &params, &config)
            .await
            .expect("spawn succeeds");
        let head_group = fission_ledger::group_id("spawn-parent-d2", "call_head");
        let document = fission_ledger::read_fission_ledger_document(&log_dir)
            .unwrap()
            .expect("document");
        let group = document
            .groups
            .iter()
            .find(|group| group.group_id == head_group)
            .expect("group anchored at the catalog head");
        assert_eq!(group.anchor_item_id, "call_head");
        assert_eq!(group.tool, "fission_spawn:head");

        fission_lifecycle::drop_pending_deliveries(&[named_group, head_group]);
    }

    #[tokio::test]
    async fn apply_fission_import_action_injects_payload_and_marks_imported() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            dir.path(),
            &log_dir,
            "import-parent",
        );

        let group = fission_ledger::register_spawned_branch(
            &log_dir,
            "import-parent",
            "call_import",
            fission_ledger::BranchCharter {
                objective: "trace the regression".to_string(),
                write_scope: Some("src/decoder.rs".to_string()),
                worktree_requested: true,
            },
            fission_ledger::NewSpawnedBranch {
                session_id: "import-child".to_string(),
                backend_session_id: Some("import-child".to_string()),
                worktree_path: Some(PathBuf::from("/tmp/fission-import-wt")),
                ..Default::default()
            },
        )
        .unwrap();
        let group_id = group.group_id;
        fission_ledger::update_branch_work(
            &log_dir,
            &group_id,
            "import-child",
            &["src/decoder.rs".to_string()],
            &["cargo test --bins".to_string()],
            Some("found the off-by-one in frame reorder"),
        )
        .unwrap();

        let test_agent = FissionTestAgent::new(None, "import-unused");
        let injected = test_agent.injected.clone();
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);
        let params = serde_json::json!({
            "threadId": "import-parent",
            "group_id": group_id,
            "branch_session_id": "import-child",
        });
        let payload = apply_fission_import_action(&mut agent, &params, &config)
            .await
            .expect("import succeeds");

        // Payload shape: identity + objective + status + work metadata +
        // worktree + raw-log pointer, returned AND injected verbatim.
        assert!(payload.starts_with("<fission_import>"));
        assert!(payload.contains(&format!("group_id: {group_id}")));
        assert!(payload.contains("branch_session_id: import-child"));
        assert!(payload.contains("objective: trace the regression"));
        assert!(payload.contains("status: running"));
        assert!(payload.contains("summary: found the off-by-one in frame reorder"));
        assert!(payload.contains("changed_files: src/decoder.rs"));
        assert!(payload.contains("tests_run: cargo test --bins"));
        assert!(payload.contains("worktree: /tmp/fission-import-wt"));
        assert!(payload.contains("raw_log: session.jsonl#session_id=import-child"));
        assert!(payload.ends_with("</fission_import>"));
        let injected = injected.lock().unwrap();
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].0, "import-parent");
        assert_eq!(injected[0].1, payload);

        // Imported marker stamped; status untouched by import.
        let document = fission_ledger::read_fission_ledger_document(&log_dir)
            .unwrap()
            .expect("document");
        assert!(document
            .branch_ext(&group_id, "import-child")
            .and_then(|ext| ext.imported_at.as_ref())
            .is_some());

        // fission-imported relationship parent→branch.
        let mut saw_imported = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ..
            } = event
            {
                if relationship == "fission-imported" {
                    assert_eq!(parent_session_id, "import-parent");
                    assert_eq!(child_session_id, "import-child");
                    saw_imported = true;
                }
            }
        }
        assert!(saw_imported, "expected fission-imported relationship");
    }

    #[tokio::test]
    async fn apply_fission_import_action_refuses_detached_and_unknown_groups() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let bus = EventBus::new();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            dir.path(),
            &log_dir,
            "import-parent-2",
        );

        let test_agent = FissionTestAgent::new(None, "import2-unused");
        let injected = test_agent.injected.clone();
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);

        // Unknown group.
        let err = apply_fission_import_action(
            &mut agent,
            &serde_json::json!({ "group_id": "missing", "branch_session_id": "x" }),
            &config,
        )
        .await
        .expect_err("unknown group refused");
        assert!(err.contains("was not found"), "error: {err}");

        // Detached group: refusal explains the recovery paths.
        let group = fission_ledger::register_spawned_branch(
            &log_dir,
            "import-parent-2",
            "call_detached",
            fission_ledger::BranchCharter {
                objective: "orphaned work".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            fission_ledger::NewSpawnedBranch {
                session_id: "import2-child".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        fission_ledger::detach_group(&log_dir, &group.group_id, "rewind crossed anchor").unwrap();
        let err = apply_fission_import_action(
            &mut agent,
            &serde_json::json!({
                "group_id": group.group_id,
                "branch_session_id": "import2-child",
            }),
            &config,
        )
        .await
        .expect_err("detached group refused");
        assert!(err.contains("detached"), "error: {err}");
        assert!(err.contains("rewind_backout"), "error: {err}");
        assert!(err.contains("raw log"), "error: {err}");
        assert!(injected.lock().unwrap().is_empty(), "nothing injected");

        // Missing branch in a live group.
        let err = apply_fission_import_action(
            &mut agent,
            &serde_json::json!({
                "group_id": fission_ledger::register_spawned_branch(
                    &log_dir,
                    "import-parent-2",
                    "call_live",
                    fission_ledger::BranchCharter {
                        objective: "live".to_string(),
                        write_scope: None,
                        worktree_requested: false,
                    },
                    fission_ledger::NewSpawnedBranch {
                        session_id: "import2-live-child".to_string(),
                        ..Default::default()
                    },
                )
                .unwrap()
                .group_id,
                "branch_session_id": "no-such-branch",
            }),
            &config,
        )
        .await
        .expect_err("missing branch refused");
        assert!(err.contains("is not part of fission group"), "error: {err}");
    }

    /// Append a Codex-shaped `token_count` event_msg line to a rollout file.
    fn append_test_rollout_token_count(path: &Path, used_tokens: u64, context_window: u64) {
        let mut contents = std::fs::read_to_string(path).unwrap();
        contents.push_str(
            &serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "model_context_window": context_window,
                        "last_token_usage": { "total_tokens": used_tokens },
                    }
                }
            })
            .to_string(),
        );
        contents.push('\n');
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn latest_context_rewind_backend_usage_in_rollout_returns_last_report() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");

        // No token_count entries at all.
        write_fission_test_rollout(&rollout, &[("call_a", "shell")]);
        assert!(latest_context_rewind_backend_usage_in_rollout(&rollout)
            .unwrap()
            .is_none());

        // With several reports, the chronologically last one wins.
        append_test_rollout_token_count(&rollout, 14_492, 38_000);
        append_test_rollout_token_count(&rollout, 27_900, 38_000);
        let usage = latest_context_rewind_backend_usage_in_rollout(&rollout)
            .unwrap()
            .expect("usage");
        assert_eq!(usage.used_tokens, 27_900);
        assert_eq!(usage.rewind_only_limit, 38_000);
    }

    #[tokio::test]
    async fn apply_external_context_rewind_records_pressure_from_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_fission_test_rollout(&rollout, &[("call_mid", "shell"), ("call_late", "shell")]);
        // Freshest backend usage report in the pre-rewind rollout. Low
        // enough to keep recovery headroom (the rewind must be accepted),
        // and below the density threshold, so the band is `ok`.
        append_test_rollout_token_count(&rollout, 13_993, 38_000);

        let bus = EventBus::new();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            dir.path(),
            &log_dir,
            "rewind-pressure-thread",
        );

        let test_agent = FissionTestAgent::new(Some(rollout.clone()), "rewindP-unused");
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);
        let request = ExternalContextRewindRequest {
            session_id: Some("rewind-pressure-thread".to_string()),
            item_id: "call_mid".to_string(),
            position: external_agent::RollbackAnchorPosition::After,
            reason: Some("trim the tail".to_string()),
            primer: None,
            preserve: Vec::new(),
            discard: Vec::new(),
            artifacts: Vec::new(),
            next_steps: Vec::new(),
            auto_resume: false,
            require_density_improvement: false,
            surgical: false,
        };
        apply_external_context_rewind(&mut agent, "rewind-pressure-thread", &request, &config)
            .await
            .expect("rewind succeeds");

        let records = context_rewind::list_records(&log_dir).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].used_tokens_at_rewind, Some(13_993));
        assert_eq!(records[0].context_window_at_rewind, Some(38_000));
        assert_eq!(records[0].pressure_band_at_rewind.as_deref(), Some("ok"));
        assert!(
            !records[0].surgical,
            "model rewinds must not be marked surgical"
        );
    }

    /// Step-limit exhaustion backstop: the supervisor-forced surgical rewind
    /// chooses the deepest recovery-eligible anchor from the catalog, applies
    /// it with the synthetic primer, marks the durable record `surgical` with
    /// the distinct reason, and resumes with the held follow-up first.
    #[tokio::test]
    async fn supervisor_surgical_rewind_uses_deepest_eligible_anchor_and_marks_record() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_fission_test_rollout(
            &rollout,
            &[
                ("call_deep", "shell"),
                ("call_mid", "shell"),
                ("call_late", "shell"),
            ],
        );
        // Trailing backend report with recovery headroom: every anchor is
        // covered, so all three are recovery-eligible at `after` and the
        // earliest cut (call_deep) is the maximum-pruning choice.
        append_test_rollout_token_count(&rollout, 13_993, 38_000);

        let bus = EventBus::new();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            dir.path(),
            &log_dir,
            "surgical-thread",
        );

        // A prior (model) rewind record of this thread: the synthetic primer
        // must point at it so the model can rebuild state from records.
        let mut prior = context_rewind::ContextRewindRecord {
            record_id: "rewind-prior".to_string(),
            created_at: "2026-06-12T00:00:00Z".to_string(),
            session_id: Some("surgical-thread".to_string()),
            thread_id: "surgical-thread".to_string(),
            item_id: "call_old".to_string(),
            position: "after".to_string(),
            reason: Some("model rewind".to_string()),
            primer: Some("earlier primer".to_string()),
            preserve: Vec::new(),
            discard: Vec::new(),
            artifacts: Vec::new(),
            next_steps: Vec::new(),
            source_rollout_path: None,
            recovery_rollout_path: None,
            fission_snapshot: None,
            lineage_ledger: None,
            fission_ledger: None,
            detached_fission_group_ids: Vec::new(),
            used_tokens_at_rewind: None,
            context_window_at_rewind: None,
            pressure_band_at_rewind: None,
            surgical: false,
        };
        context_rewind::persist_record(&log_dir, &prior).unwrap();
        // A record from another thread must not leak into the primer.
        prior.record_id = "rewind-other-thread".to_string();
        prior.thread_id = "other-thread".to_string();
        context_rewind::persist_record(&log_dir, &prior).unwrap();

        let test_agent = FissionTestAgent::new(Some(rollout.clone()), "surgical-unused");
        let rollbacks = test_agent.rollbacks.clone();
        let injected = test_agent.injected.clone();
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);

        let mut pending = std::collections::VecDeque::new();
        pending.push_back(FollowUpMessage::text("finish the held user task".into()));

        let continuation = attempt_supervisor_surgical_context_rewind(
            &mut agent,
            "surgical-thread",
            &config,
            Some("Ship the recovery backstop"),
            &mut pending,
        )
        .await
        .expect("surgical rewind succeeds");

        // Deepest eligible anchor, applied via the normal rewind machinery.
        assert_eq!(
            rollbacks.lock().unwrap().as_slice(),
            &[(
                "surgical-thread".to_string(),
                "call_deep".to_string(),
                "after"
            )]
        );

        // Durable record: marked surgical, distinct reason, synthetic primer
        // carrying the task statement and prior record pointers (this
        // thread's only).
        let records = context_rewind::list_records(&log_dir).unwrap();
        let record = records
            .iter()
            .find(|record| record.surgical)
            .expect("surgical record persisted");
        assert_eq!(
            record.reason.as_deref(),
            Some(MANAGED_CONTEXT_SURGICAL_RECOVERY_REASON)
        );
        assert_eq!(record.item_id, "call_deep");
        assert_eq!(record.position, "after");
        let primer = record.primer.as_deref().expect("synthetic primer");
        assert!(primer.contains("automatic surgical recovery"));
        assert!(primer.contains("Task:\nShip the recovery backstop"));
        assert!(primer.contains("rewind-prior"));
        assert!(!primer.contains("rewind-other-thread"));

        // The primer was injected as developer context with the record id.
        let injected = injected.lock().unwrap();
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].0, "surgical-thread");
        assert!(injected[0].1.starts_with("<model_context_rewind_primer>"));
        assert!(injected[0]
            .1
            .contains(MANAGED_CONTEXT_SURGICAL_RECOVERY_REASON));

        // Held user follow-up resumes first (wrapped as a replay), not the
        // generic auto-resume.
        assert!(continuation.text.contains("finish the held user task"));
        assert!(continuation
            .text
            .starts_with(MANAGED_CONTEXT_REWIND_FOLLOWUP_REPLAY_OPEN));
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn supervisor_surgical_rewind_fails_cleanly_without_catalog_or_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        let bus = EventBus::new();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            dir.path(),
            &log_dir,
            "surgical-err-thread",
        );

        // No rollout path in the thread snapshot → loud error, no record.
        let mut agent: Box<dyn external_agent::ExternalAgent> =
            Box::new(FissionTestAgent::new(None, "surgical-unused"));
        let mut pending = std::collections::VecDeque::new();
        let err = attempt_supervisor_surgical_context_rewind(
            &mut agent,
            "surgical-err-thread",
            &config,
            None,
            &mut pending,
        )
        .await
        .expect_err("no rollout path must fail");
        assert!(err.contains("rollout path"), "err: {err}");

        // Empty catalog (rollout with no anchorable items) → loud error.
        let rollout = dir.path().join("rollout-empty.jsonl");
        std::fs::write(&rollout, "").unwrap();
        let mut agent: Box<dyn external_agent::ExternalAgent> =
            Box::new(FissionTestAgent::new(Some(rollout), "surgical-unused"));
        let err = attempt_supervisor_surgical_context_rewind(
            &mut agent,
            "surgical-err-thread",
            &config,
            None,
            &mut pending,
        )
        .await
        .expect_err("empty catalog must fail");
        assert!(err.contains("no recovery-eligible anchor"), "err: {err}");
        assert!(context_rewind::list_records(&log_dir).unwrap().is_empty());
    }

    #[tokio::test]
    async fn apply_external_context_rewind_detaches_groups_cut_by_the_rewind() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_fission_test_rollout(
            &rollout,
            &[
                ("call_early", "shell"),
                ("call_mid", "shell"),
                ("call_late", "intendant.fission_spawn"),
            ],
        );

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            dir.path(),
            &log_dir,
            "rewind-thread",
        );

        // Two groups for the same parent: one anchored before the cut, one
        // after it.
        let surviving_group = fission_ledger::register_spawned_branch(
            &log_dir,
            "rewind-thread",
            "call_early",
            fission_ledger::BranchCharter {
                objective: "early work".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            fission_ledger::NewSpawnedBranch {
                session_id: "rewindA-child-early".to_string(),
                ..Default::default()
            },
        )
        .unwrap()
        .group_id;
        let doomed_group = fission_ledger::register_spawned_branch(
            &log_dir,
            "rewind-thread",
            "call_late",
            fission_ledger::BranchCharter {
                objective: "late work".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            fission_ledger::NewSpawnedBranch {
                session_id: "rewindA-child-late".to_string(),
                ..Default::default()
            },
        )
        .unwrap()
        .group_id;
        fission_lifecycle::register_branch("rewindA-child-early", &surviving_group, &log_dir);
        fission_lifecycle::register_branch("rewindA-child-late", &doomed_group, &log_dir);

        let test_agent = FissionTestAgent::new(Some(rollout.clone()), "rewindA-unused");
        let rollbacks = test_agent.rollbacks.clone();
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);
        let request = ExternalContextRewindRequest {
            session_id: Some("rewind-thread".to_string()),
            item_id: "call_mid".to_string(),
            position: external_agent::RollbackAnchorPosition::After,
            reason: Some("trim the tail".to_string()),
            primer: None,
            preserve: Vec::new(),
            discard: Vec::new(),
            artifacts: Vec::new(),
            next_steps: Vec::new(),
            auto_resume: false,
            require_density_improvement: false,
            surgical: false,
        };
        apply_external_context_rewind(&mut agent, "rewind-thread", &request, &config)
            .await
            .expect("rewind succeeds");

        // Rollback happened, and detach ran after it (the ledger flip is
        // observable only post-rollback by construction of the mock).
        assert_eq!(
            rollbacks.lock().unwrap().as_slice(),
            &[("rewind-thread".to_string(), "call_mid".to_string(), "after")]
        );

        // Ledger: the late-anchored group is detached, the early one is not.
        let document = fission_ledger::read_fission_ledger_document(&log_dir)
            .unwrap()
            .expect("document");
        assert!(document.group_is_detached(&doomed_group));
        assert!(!document.group_is_detached(&surviving_group));
        let doomed_branch = document
            .groups
            .iter()
            .find(|group| group.group_id == doomed_group)
            .unwrap()
            .branches
            .iter()
            .find(|branch| branch.session_id == "rewindA-child-late")
            .unwrap();
        assert_eq!(doomed_branch.status, "detached");

        // Rewind record carries the detached group ids. The synthetic
        // rollout has no token_count reports and the test session log has no
        // context snapshots, so the pressure-at-rewind fields stay `None`.
        let records = context_rewind::list_records(&log_dir).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].detached_fission_group_ids,
            vec![doomed_group.clone()]
        );
        assert!(records[0].used_tokens_at_rewind.is_none());
        assert!(records[0].context_window_at_rewind.is_none());
        assert!(records[0].pressure_band_at_rewind.is_none());

        // Pending deliveries dropped for the detached group only.
        assert!(fission_lifecycle::branch_route("rewindA-child-late").is_none());
        assert!(fission_lifecycle::branch_route("rewindA-child-early").is_some());

        // fission-detached relationship emitted parent→detached branch only.
        let mut detached_edges = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ..
            } = event
            {
                if relationship == "fission-detached" {
                    detached_edges.push((parent_session_id, child_session_id));
                }
            }
        }
        assert_eq!(
            detached_edges,
            vec![(
                "rewind-thread".to_string(),
                "rewindA-child-late".to_string()
            )]
        );

        fission_lifecycle::drop_pending_deliveries(&[surviving_group]);
    }

    #[tokio::test]
    async fn apply_external_context_rewind_failed_rollback_leaves_fission_ledger_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        write_fission_test_rollout(&rollout, &[("call_mid", "shell"), ("call_late", "shell")]);

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let config = fission_test_drain_config(
            &bus,
            &session_log,
            &approval_registry,
            &context_injection,
            dir.path(),
            &log_dir,
            "rewind-thread-fail",
        );

        let doomed_group = fission_ledger::register_spawned_branch(
            &log_dir,
            "rewind-thread-fail",
            "call_late",
            fission_ledger::BranchCharter {
                objective: "late work".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            fission_ledger::NewSpawnedBranch {
                session_id: "rewindB-child-late".to_string(),
                ..Default::default()
            },
        )
        .unwrap()
        .group_id;
        fission_lifecycle::register_branch("rewindB-child-late", &doomed_group, &log_dir);

        let mut test_agent = FissionTestAgent::new(Some(rollout.clone()), "rewindB-unused");
        test_agent.fail_rollback = true;
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(test_agent);
        let request = ExternalContextRewindRequest {
            session_id: Some("rewind-thread-fail".to_string()),
            item_id: "call_mid".to_string(),
            position: external_agent::RollbackAnchorPosition::After,
            reason: Some("trim the tail".to_string()),
            primer: None,
            preserve: Vec::new(),
            discard: Vec::new(),
            artifacts: Vec::new(),
            next_steps: Vec::new(),
            auto_resume: false,
            require_density_improvement: false,
            surgical: false,
        };
        let err =
            apply_external_context_rewind(&mut agent, "rewind-thread-fail", &request, &config)
                .await
                .expect_err("rollback failure surfaces");
        assert!(err.contains("thread rollback failed"), "error: {err}");

        // Detach never ran: ledger untouched, route intact, no record, no
        // relationship markers.
        let document = fission_ledger::read_fission_ledger_document(&log_dir)
            .unwrap()
            .expect("document");
        assert!(!document.group_is_detached(&doomed_group));
        let branch = document
            .groups
            .iter()
            .find(|group| group.group_id == doomed_group)
            .unwrap()
            .branches
            .iter()
            .find(|branch| branch.session_id == "rewindB-child-late")
            .unwrap();
        assert_eq!(branch.status, "running");
        assert!(fission_lifecycle::branch_route("rewindB-child-late").is_some());
        assert!(context_rewind::list_records(&log_dir).unwrap().is_empty());
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::SessionRelationship { relationship, .. } = event {
                assert_ne!(relationship, "fission-detached");
            }
        }

        fission_lifecycle::drop_pending_deliveries(&[doomed_group]);
    }
}
