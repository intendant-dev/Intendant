//! Context-usage snapshots and the managed-context pressure machinery:
//! snapshot emit/refresh, preflight rewind-only/density gates, tool
//! allow-lists, dashboard-command classification, recovery and density
//! kickstart/handoff texts, surgical recovery, and follow-up replay text.

use super::*;

pub(crate) fn external_context_snapshot_key(
    snapshot: &external_agent::AgentContextSnapshot,
) -> u64 {
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

pub(crate) fn shell_command_has_explicit_dashboard_cleanup(
    command: &str,
    tokens: &[String],
) -> bool {
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

pub(crate) fn shell_command_has_owned_dashboard_lifecycle(
    command: &str,
    tokens: &[String],
) -> bool {
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

pub(crate) fn managed_context_density_handoff_text(
    pressure: ManagedContextDensityPressure,
) -> String {
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
}
