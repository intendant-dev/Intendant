//! Per-session agent configuration and identity: session rename, the
//! configure-session-agent surface with its normalize/apply helper
//! family and launch overrides, and SessionIdentity application.

use super::*;

impl SessionSupervisor {
    pub(crate) async fn rename_session(
        &self,
        session_id: String,
        backend_session_id: Option<String>,
        source: Option<String>,
        name: String,
    ) {
        let managed = {
            let state = self.state.lock().await;
            let resolved_id = state
                .resolve_session_id(&session_id)
                .unwrap_or_else(|| session_id.clone());
            state
                .sessions
                .get(&resolved_id)
                .map(|session| (session.session_id.clone(), session.source.clone()))
        };

        if let Some((managed_id, managed_source)) = managed.as_ref() {
            let supports_native_rename = external_agent::AgentBackend::from_str_loose(
                managed_source,
            )
            .is_some_and(|backend| {
                matches!(
                    backend,
                    external_agent::AgentBackend::Codex
                        | external_agent::AgentBackend::Kimi
                        | external_agent::AgentBackend::Pi
                )
            });
            if supports_native_rename {
                self.config.bus.send(AppEvent::ControlCommand(
                    event::ControlMsg::CodexThreadAction {
                        session_id: Some(managed_id.clone()),
                        op: "rename".to_string(),
                        params: serde_json::json!({ "name": name }),
                        origin: None,
                    },
                ));
                return;
            }
        }

        let source = managed
            .map(|(_, source)| source)
            .or(source)
            .unwrap_or_else(|| "intendant".to_string());
        let normalized_source = crate::session_names::normalize_source(&source);
        let persistence_session_id = if normalized_source == "intendant" {
            session_id.as_str()
        } else {
            backend_session_id.as_deref().unwrap_or(&session_id)
        };
        let result = crate::session_names::rename_session(
            &self.logs_home(),
            &normalized_source,
            persistence_session_id,
            &name,
        );

        match result {
            Ok(name) => {
                self.config.bus.send(AppEvent::SessionRenameResult {
                    session_id,
                    source: Some(normalized_source),
                    name: Some(name.clone()),
                    success: true,
                    message: format!("Renamed session to {}", name),
                });
            }
            Err(message) => {
                self.config.bus.send(AppEvent::SessionRenameResult {
                    session_id,
                    source: Some(normalized_source),
                    name: None,
                    success: false,
                    message,
                });
            }
        }
    }

    pub(crate) async fn configure_session_agent(
        &self,
        session_id: String,
        source: Option<String>,
        backend_session_id: Option<String>,
        intendant_session_id: Option<String>,
        overrides: LaunchOverrides,
    ) {
        let managed = {
            let state = self.state.lock().await;
            state
                .resolve_session_id(&session_id)
                .and_then(|resolved_id| state.sessions.get(&resolved_id))
                .map(|session| {
                    (
                        session.session_id.clone(),
                        session.source.clone(),
                        session.session_dir.clone(),
                    )
                })
        };

        let normalized_source = managed
            .as_ref()
            .map(|(_, source, _)| source.clone())
            .or(source)
            .map(|source| crate::session_names::normalize_source(&source))
            .unwrap_or_default();
        let Some(backend) = external_agent::AgentBackend::from_str_loose(&normalized_source) else {
            let message = "Session config failed: choose an external agent session".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: normalized_source,
                backend_session_id,
                intendant_session_id,
                persisted_session_ids: Vec::new(),
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        };
        if let Some(command) = overrides
            .agent_command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
        {
            if let Some(owner) = crate::session_config::agent_command_conflicts_with_source(
                backend.as_short_str(),
                command,
            ) {
                // An explicit save of another backend's CLI is a user-visible
                // error, not something to silently strip: persisting it would
                // brick every future resume of this session (the codex-for-
                // claude-code incident, 2026-07-16).
                let message = format!(
                    "Session config failed: agent command '{}' is the {} CLI but this session's agent is {}",
                    command,
                    owner,
                    backend.as_short_str()
                );
                self.config.bus.send(AppEvent::SessionAgentConfigResult {
                    session_id,
                    source: backend.as_short_str().to_string(),
                    backend_session_id,
                    intendant_session_id,
                    persisted_session_ids: Vec::new(),
                    success: false,
                    message: message.clone(),
                });
                self.loop_error(message);
                return;
            }
        }
        let is_codex = matches!(backend, external_agent::AgentBackend::Codex);
        let is_claude = matches!(backend, external_agent::AgentBackend::ClaudeCode);
        let is_kimi = matches!(backend, external_agent::AgentBackend::Kimi);
        let is_pi = matches!(backend, external_agent::AgentBackend::Pi);
        let clear_codex_sandbox =
            is_codex && session_config_clear_value(overrides.codex_sandbox.as_deref());
        let clear_codex_approval_policy =
            is_codex && session_config_clear_value(overrides.codex_approval_policy.as_deref());
        // The clear sentinel must be checked on the RAW wire value, before
        // from_wire's normalization, and re-applied after the merge passes
        // below re-fill cleared fields from the persisted configs — same
        // dance as sandbox/approval. Otherwise "inherit" would either pin
        // the default into the overlay or be resurrected by the merge.
        let clear_codex_managed_context =
            is_codex && session_config_clear_value(overrides.codex_managed_context.as_deref());
        let clear_codex_context_archive =
            is_codex && session_config_clear_value(overrides.codex_context_archive.as_deref());
        let clear_claude_model =
            is_claude && session_config_clear_value(overrides.claude_model.as_deref());
        // "default" is a REAL permission mode (pinnable under a stricter
        // global); only inherit/global/empty clear it.
        let clear_claude_permission_mode = is_claude
            && session_config_clear_value_keeping_default(
                overrides.claude_permission_mode.as_deref(),
            );
        let clear_claude_allowed_tools =
            is_claude && session_config_clear_value(overrides.claude_allowed_tools.as_deref());
        let clear_claude_effort =
            is_claude && session_config_clear_value(overrides.claude_effort.as_deref());
        let clear_kimi_model =
            is_kimi && session_config_clear_value(overrides.kimi_model.as_deref());
        let clear_kimi_thinking =
            is_kimi && session_config_clear_value(overrides.kimi_thinking.as_deref());
        let clear_kimi_permission_mode = is_kimi
            && session_config_clear_value_keeping_default(
                overrides.kimi_permission_mode.as_deref(),
            );
        let clear_kimi_allowed_tools = is_kimi
            && overrides
                .kimi_allowed_tools
                .as_deref()
                .is_some_and(|tools| {
                    crate::session_config::normalize_kimi_allowed_tools(Some(tools)).is_none()
                });
        let clear_kimi_plan_mode =
            is_kimi && session_config_clear_value(overrides.kimi_plan_mode.as_deref());
        let clear_kimi_swarm_mode =
            is_kimi && session_config_clear_value(overrides.kimi_swarm_mode.as_deref());
        let clear_pi_model = is_pi && session_config_clear_value(overrides.pi_model.as_deref());
        let clear_pi_thinking =
            is_pi && session_config_clear_value(overrides.pi_thinking.as_deref());
        let clear_pi_allowed_tools = is_pi
            && overrides.pi_allowed_tools.as_deref().is_some_and(|tools| {
                crate::session_config::normalize_pi_allowed_tools(Some(tools)).is_none()
            });
        let mut config = crate::session_config::from_wire_fields(
            overrides.as_wire_fields(backend.as_short_str()),
        );
        let home = self.logs_home();
        if let Some(existing) = crate::session_config::load_for_resume(
            &home,
            backend.as_short_str(),
            &session_id,
            backend_session_id.as_deref(),
        ) {
            config.merge_missing_from(existing);
        }
        if let Some((_, _, session_dir)) = managed.as_ref() {
            if let Some(existing) = crate::session_config::read_log_dir_config(session_dir) {
                config.merge_missing_from(existing);
            }
        }
        if let Some(intendant_id) = intendant_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if let Some(dir) = session_log::SessionLog::find_session_by_id(intendant_id) {
                if let Some(existing) = crate::session_config::read_log_dir_config(&dir) {
                    config.merge_missing_from(existing);
                }
            }
        }
        if clear_codex_sandbox {
            config.codex_sandbox = None;
        }
        if clear_codex_approval_policy {
            config.codex_approval_policy = None;
        }
        if clear_codex_managed_context {
            config.codex_managed_context = None;
        }
        if clear_codex_context_archive {
            config.codex_context_archive = None;
        }
        if clear_claude_model {
            config.claude_model = None;
        }
        if clear_claude_permission_mode {
            config.claude_permission_mode = None;
        }
        if clear_claude_allowed_tools {
            config.claude_allowed_tools = None;
        }
        if clear_claude_effort {
            config.claude_effort = None;
        }
        if clear_kimi_model {
            config.kimi_model = None;
        }
        if clear_kimi_thinking {
            config.kimi_thinking = None;
        }
        if clear_kimi_permission_mode {
            config.kimi_permission_mode = None;
        }
        if clear_kimi_allowed_tools {
            config.kimi_allowed_tools = None;
        }
        if clear_kimi_plan_mode {
            config.kimi_plan_mode = None;
        }
        if clear_kimi_swarm_mode {
            config.kimi_swarm_mode = None;
        }
        if clear_pi_model {
            config.pi_model = None;
        }
        if clear_pi_thinking {
            config.pi_thinking = None;
        }
        if clear_pi_allowed_tools {
            config.pi_allowed_tools = None;
        }
        if config.is_empty() {
            let message = "Session config failed: no launch settings supplied".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids: Vec::new(),
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        }

        let mut errors = Vec::new();
        let mut persisted_session_ids = Vec::new();
        let mut note_persisted = |id: &str| {
            let id = id.trim();
            if !id.is_empty() && !persisted_session_ids.iter().any(|existing| existing == id) {
                persisted_session_ids.push(id.to_string());
            }
        };
        if let Some((managed_id, _, session_dir)) = managed.as_ref() {
            if let Err(e) = crate::session_config::write_log_dir_config(session_dir, &config) {
                errors.push(e);
            } else {
                note_persisted(managed_id);
            }
        }
        let intendant_id = intendant_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(intendant_id) = intendant_id {
            if let Some(dir) = session_log::SessionLog::find_session_by_id(intendant_id) {
                if let Err(e) = crate::session_config::write_log_dir_config(&dir, &config) {
                    errors.push(e);
                } else {
                    note_persisted(intendant_id);
                }
            }
        }

        let external_ids = [
            backend_session_id.as_deref(),
            Some(session_id.as_str()),
            managed
                .as_ref()
                .map(|(managed_id, _, _)| managed_id.as_str()),
        ];
        let mut wrote_external = false;
        for external_id in external_ids
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if !external_agent::source_session_id_is_canonical(backend.as_short_str(), external_id)
            {
                continue;
            }
            wrote_external = true;
            if let Err(e) = crate::session_config::replace_external_overlay(
                &home,
                backend.as_short_str(),
                external_id,
                &config,
            ) {
                errors.push(e);
            } else {
                note_persisted(external_id);
            }
        }

        if !wrote_external && managed.is_none() && intendant_id.is_none() {
            let message = "Session config failed: no persistable session id".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        }
        if errors.is_empty() {
            let message = format!(
                "Session {} launch config saved for {} (takes effect on next attach/resume)",
                short_session(&session_id),
                backend.as_short_str()
            );
            self.info(&message);
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: true,
                message,
            });
        } else {
            let message = format!("Session config partially failed: {}", errors.join("; "));
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
        }
    }

    pub(crate) async fn apply_session_identity(
        &self,
        session_id: String,
        source: String,
        backend_session_id: String,
    ) {
        let source = crate::session_names::normalize_source(&source);
        if !external_agent::source_session_id_is_canonical(&source, &backend_session_id) {
            return;
        }
        {
            // Record the identity even for sessions this supervisor does not
            // manage (e.g. the CLI main loop's agent) so the thread-action
            // fallback responder knows another owner will answer for them.
            let mut state = self.state.lock().await;
            state.known_external_sessions.insert(session_id.clone());
            state
                .known_external_sessions
                .insert(backend_session_id.clone());
        }
        if session_id == backend_session_id {
            return;
        }

        let name_to_persist = {
            let mut state = self.state.lock().await;
            let Some(current_key) = state.resolve_session_id(&session_id) else {
                return;
            };
            if current_key == backend_session_id {
                state
                    .session_aliases
                    .insert(session_id, backend_session_id.clone());
                state
                    .sessions
                    .get(&backend_session_id)
                    .and_then(|session| session.name.clone())
            } else if state.sessions.contains_key(&backend_session_id) {
                let existing_name = state
                    .sessions
                    .get(&backend_session_id)
                    .and_then(|session| session.name.clone())
                    .or_else(|| {
                        state
                            .sessions
                            .get(&current_key)
                            .and_then(|session| session.name.clone())
                    });
                let name = if let Some(mut session) = state.sessions.remove(&current_key) {
                    if session.name.is_none() {
                        session.name = existing_name.clone();
                    }
                    let name = session.name.clone();
                    session.session_id = backend_session_id.clone();
                    session.source = source.clone();
                    state.sessions.insert(backend_session_id.clone(), session);
                    state.session_aliases.retain(|alias, target| {
                        alias != &backend_session_id && target != &current_key
                    });
                    state
                        .session_aliases
                        .insert(session_id.clone(), backend_session_id.clone());
                    state
                        .session_aliases
                        .insert(current_key.clone(), backend_session_id.clone());
                    name
                } else {
                    state
                        .session_aliases
                        .insert(session_id.clone(), backend_session_id.clone());
                    state
                        .session_aliases
                        .insert(current_key.clone(), backend_session_id.clone());
                    existing_name
                };
                if state.active_session_id.as_deref() == Some(&session_id)
                    || state.active_session_id.as_deref() == Some(&current_key)
                    || state.active_session_id.as_deref() == Some(&backend_session_id)
                {
                    state.active_session_id = Some(backend_session_id.clone());
                }
                name
            } else {
                let Some(mut session) = state.sessions.remove(&current_key) else {
                    return;
                };
                let name = session.name.clone();
                session.session_id = backend_session_id.clone();
                session.source = source.clone();
                state.sessions.insert(backend_session_id.clone(), session);
                // The entry is now directly keyed by the backend id; drop the
                // pre-identity alias register_session added under that id so
                // no alias entry shadows a live key.
                state.session_aliases.remove(&backend_session_id);
                state
                    .session_aliases
                    .insert(session_id.clone(), backend_session_id.clone());
                state
                    .session_aliases
                    .insert(current_key.clone(), backend_session_id.clone());
                if state.active_session_id.as_deref() == Some(&session_id)
                    || state.active_session_id.as_deref() == Some(&current_key)
                {
                    state.active_session_id = Some(backend_session_id.clone());
                }
                name
            }
        };

        if let Some(name) = name_to_persist {
            persist_external_session_name(
                &self.logs_home(),
                &self.config.bus,
                &source,
                &backend_session_id,
                &name,
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionAgentSelection {
    Configured,
    Internal,
    External(external_agent::AgentBackend),
}

impl SessionAgentSelection {
    pub(crate) fn from_wire(agent: Option<&str>) -> Result<Self, String> {
        let Some(agent) = agent else {
            return Ok(Self::Configured);
        };
        let trimmed = agent.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("configured") {
            return Ok(Self::Configured);
        }
        let lowered = trimmed.to_ascii_lowercase();
        if matches!(
            lowered.as_str(),
            "internal" | "intendant" | "native" | "none"
        ) {
            return Ok(Self::Internal);
        }
        external_agent::AgentBackend::from_str_loose(trimmed)
            .map(Self::External)
            .ok_or_else(|| {
                format!(
                    "unknown agent '{}' (expected internal, codex, claude-code, kimi, or pi)",
                    trimmed
                )
            })
    }
}

/// Intake validation of an [`crate::event::AgentLaunchConfig`] for lanes
/// that record the config now and spawn LATER (the agenda's digest-bound
/// manifests): reject at propose time exactly what the launch path would
/// deterministically reject at spawn time, by the same rules and in the
/// same words, so a reviewed-and-approved manifest never dies at 03:00 on
/// a contradiction that was knowable at intake. Deliberately no stricter
/// than spawn: pins under a `Configured` (absent) agent selection resolve
/// against the daemon default at fire time by design, so only an explicit
/// selection can contradict a pin here; fire-time mismatches journal as
/// the occurrence's named `failed` outcome instead.
pub(crate) fn validate_launch_config(
    config: &crate::event::AgentLaunchConfig,
) -> Result<(), String> {
    use external_agent::AgentBackend;
    let selection = SessionAgentSelection::from_wire(config.agent.as_deref())?;
    let pinned = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .is_some()
    };
    // (field, its backend family, that family's spawn-gate wording.)
    let families: [(&str, bool, AgentBackend, &str); 4] = [
        (
            "Claude Code",
            pinned(&config.claude_model)
                || pinned(&config.claude_permission_mode)
                || pinned(&config.claude_effort),
            AgentBackend::ClaudeCode,
            "claude_model/claude_permission_mode/claude_effort",
        ),
        (
            "Kimi",
            pinned(&config.kimi_model)
                || pinned(&config.kimi_thinking)
                || pinned(&config.kimi_permission_mode)
                || pinned(&config.kimi_allowed_tools)
                || config.kimi_plan_mode.is_some()
                || config.kimi_swarm_mode.is_some(),
            AgentBackend::Kimi,
            "kimi_* launch pins",
        ),
        (
            "Pi",
            pinned(&config.pi_model)
                || pinned(&config.pi_thinking)
                || pinned(&config.pi_allowed_tools),
            AgentBackend::Pi,
            "pi_* launch pins",
        ),
        (
            "Codex",
            pinned(&config.codex_model)
                || pinned(&config.codex_reasoning_effort)
                || pinned(&config.codex_sandbox)
                || pinned(&config.codex_approval_policy)
                || pinned(&config.codex_managed_context)
                || pinned(&config.codex_context_archive)
                || pinned(&config.codex_service_tier),
            AgentBackend::Codex,
            "codex_* launch pins",
        ),
    ];
    match &selection {
        SessionAgentSelection::Configured => {}
        SessionAgentSelection::Internal => {
            if pinned(&config.agent_command) {
                return Err("agent_command requires an external agent".to_string());
            }
            for (name, any_pinned, _, fields) in &families {
                if *any_pinned {
                    return Err(format!("{fields} require {name} (agent is internal)"));
                }
            }
        }
        SessionAgentSelection::External(backend) => {
            for (name, any_pinned, family, fields) in &families {
                if *any_pinned && family != backend {
                    return Err(format!(
                        "{fields} require {name} (agent is {})",
                        backend.as_short_str()
                    ));
                }
            }
            if let Some(command) = config
                .agent_command
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            {
                if let Some(owner) = crate::session_config::agent_command_conflicts_with_source(
                    backend.as_short_str(),
                    command,
                ) {
                    return Err(format!(
                        "agent command '{}' is the {} CLI but the session's agent is {}",
                        command,
                        owner,
                        backend.as_short_str()
                    ));
                }
            }
            if *backend == AgentBackend::Codex {
                if let Some(effort) = config
                    .codex_reasoning_effort
                    .as_deref()
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                {
                    let Some(normalized) = crate::project::normalize_reasoning_effort(Some(effort))
                    else {
                        return Err(format!("unsupported Codex reasoning effort: {effort}"));
                    };
                    if let Some(model) = config
                        .codex_model
                        .as_deref()
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                    {
                        if let Some(entry) = crate::project::codex_model_catalog_entry(model) {
                            if !entry.reasoning_efforts.contains(&normalized.as_str()) {
                                return Err(format!(
                                    "Codex model {model} does not support reasoning effort {normalized}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn codex_fast_new_session_agent(agent: Option<&str>) -> Result<String, String> {
    match SessionAgentSelection::from_wire(agent)? {
        SessionAgentSelection::Configured => Ok("codex".to_string()),
        SessionAgentSelection::External(external_agent::AgentBackend::Codex) => {
            Ok("codex".to_string())
        }
        SessionAgentSelection::Internal => {
            Err("/fast can only start a new Codex external-agent session".to_string())
        }
        SessionAgentSelection::External(other) => Err(format!(
            "/fast can only start a new Codex external-agent session; selected {other}"
        )),
    }
}

/// The `/fast` projection of a launch config: an idle Codex session keeps
/// the Codex fields and the agent command, drops the other backends' pins,
/// and forces the Fast service tier — identical for the CreateSession and
/// untargeted StartTask arms.
pub(crate) fn codex_fast_launch(
    launch: &crate::event::AgentLaunchConfig,
    agent: Option<String>,
) -> crate::event::AgentLaunchConfig {
    crate::event::AgentLaunchConfig {
        agent,
        agent_command: launch.agent_command.clone(),
        codex_model: launch.codex_model.clone(),
        codex_reasoning_effort: launch.codex_reasoning_effort.clone(),
        codex_sandbox: launch.codex_sandbox.clone(),
        codex_approval_policy: launch.codex_approval_policy.clone(),
        codex_managed_context: launch.codex_managed_context.clone(),
        codex_context_archive: launch.codex_context_archive.clone(),
        codex_service_tier: Some(crate::external_agent::codex::CODEX_FAST_SERVICE_TIER.to_string()),
        ..Default::default()
    }
}

pub(crate) fn normalize_session_agent_command(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn normalize_session_codex_managed_context(mode: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_managed_context(mode)
}

/// One-shot per-session launch overrides carried by a configure, resume, or
/// restart request. Raw wire values (clear sentinels intact) — backend
/// gating and normalization happen in `session_config::from_wire_fields`.
#[derive(Debug, Default)]
pub(crate) struct LaunchOverrides {
    pub(crate) agent_command: Option<String>,
    pub(crate) codex_sandbox: Option<String>,
    pub(crate) codex_approval_policy: Option<String>,
    pub(crate) codex_managed_context: Option<String>,
    pub(crate) codex_context_archive: Option<String>,
    pub(crate) claude_model: Option<String>,
    pub(crate) claude_permission_mode: Option<String>,
    pub(crate) claude_allowed_tools: Option<String>,
    pub(crate) claude_effort: Option<String>,
    pub(crate) kimi_model: Option<String>,
    pub(crate) kimi_thinking: Option<String>,
    pub(crate) kimi_permission_mode: Option<String>,
    pub(crate) kimi_allowed_tools: Option<String>,
    pub(crate) kimi_plan_mode: Option<String>,
    pub(crate) kimi_swarm_mode: Option<String>,
    pub(crate) pi_model: Option<String>,
    pub(crate) pi_thinking: Option<String>,
    pub(crate) pi_allowed_tools: Option<String>,
    /// Internal-only anchor-fork parameters (set by the supervisor's fork
    /// orchestrator, never parsed from any wire message — deliberately
    /// absent from `as_wire_fields`, so `ResumeSession` senders cannot
    /// inject them; applied onto the merged config by
    /// `apply_fork_lineage`). `fork_relationship` here overrides the
    /// wire-vetted kind (`anchor-fork` is minted internally only), and
    /// `forked_from` covers engines whose child resumes its OWN id (the
    /// claude-code chain-slice: resume token = child uuid ≠ parent).
    pub(crate) forked_from: Option<String>,
    pub(crate) fork_relationship: Option<String>,
    pub(crate) fork_anchor: Option<String>,
    pub(crate) codex_fork_rollout_path: Option<String>,
    pub(crate) codex_fork_rollback_turns: Option<u32>,
    pub(crate) codex_fork_rollback_item_id: Option<String>,
    pub(crate) codex_fork_rollback_position: Option<String>,
    pub(crate) kimi_fork_rollback_turns: Option<u32>,
    pub(crate) kimi_fork_expected_horizon: Option<String>,
}

impl LaunchOverrides {
    /// The matching normalizer input for `session_config::from_wire_fields`.
    pub(crate) fn as_wire_fields<'a>(
        &'a self,
        source: &'a str,
    ) -> crate::session_config::WireSessionAgentFields<'a> {
        crate::session_config::WireSessionAgentFields {
            source: Some(source),
            agent_command: self.agent_command.as_deref(),
            // Launch-time only (CreateSession): a codex session cannot
            // switch models mid-session, so configure/restart never carries
            // one and the persisted pin must survive untouched.
            codex_model: None,
            // Same launch-time pin policy as the model: configure/restart
            // preserves the persisted effort selected when the session began.
            codex_reasoning_effort: None,
            codex_sandbox: self.codex_sandbox.as_deref(),
            codex_approval_policy: self.codex_approval_policy.as_deref(),
            codex_managed_context: self.codex_managed_context.as_deref(),
            codex_context_archive: self.codex_context_archive.as_deref(),
            codex_service_tier: None,
            claude_model: self.claude_model.as_deref(),
            claude_permission_mode: self.claude_permission_mode.as_deref(),
            claude_allowed_tools: self.claude_allowed_tools.as_deref(),
            claude_effort: self.claude_effort.as_deref(),
            kimi_model: self.kimi_model.as_deref(),
            kimi_thinking: self.kimi_thinking.as_deref(),
            kimi_permission_mode: self.kimi_permission_mode.as_deref(),
            kimi_allowed_tools: self.kimi_allowed_tools.as_deref(),
            kimi_plan_mode: self.kimi_plan_mode.as_deref(),
            kimi_swarm_mode: self.kimi_swarm_mode.as_deref(),
            pi_model: self.pi_model.as_deref(),
            pi_thinking: self.pi_thinking.as_deref(),
            pi_allowed_tools: self.pi_allowed_tools.as_deref(),
        }
    }

    /// Apply the internal-only anchor-fork parameters onto the merged
    /// session config, after `from_wire_fields` + persisted-overlay merge
    /// (which cannot carry them). No-ops when the overrides carry none.
    pub(crate) fn apply_fork_lineage(
        &self,
        config: Option<&mut crate::session_config::SessionAgentConfig>,
    ) {
        let Some(config) = config else {
            return;
        };
        if self.forked_from.is_some() {
            config.forked_from = self.forked_from.clone();
        }
        if self.fork_relationship.is_some() {
            config.fork_relationship = self.fork_relationship.clone();
        }
        if self.fork_anchor.is_some() {
            config.fork_anchor = self.fork_anchor.clone();
        }
        if self.codex_fork_rollout_path.is_some() {
            config.codex_fork_rollout_path = self.codex_fork_rollout_path.clone();
        }
        if self.codex_fork_rollback_turns.is_some() {
            config.codex_fork_rollback_turns = self.codex_fork_rollback_turns;
        }
        if self.codex_fork_rollback_item_id.is_some() {
            config.codex_fork_rollback_item_id = self.codex_fork_rollback_item_id.clone();
        }
        if self.codex_fork_rollback_position.is_some() {
            config.codex_fork_rollback_position = self.codex_fork_rollback_position.clone();
        }
        if self.kimi_fork_rollback_turns.is_some() {
            config.kimi_fork_rollback_turns = self.kimi_fork_rollback_turns;
        }
        if self.kimi_fork_expected_horizon.is_some() {
            config.kimi_fork_expected_horizon = self.kimi_fork_expected_horizon.clone();
        }
    }
}

pub(crate) fn session_config_clear_value(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .map(|value| value.is_empty() || matches!(value, "inherit" | "default" | "global"))
        .unwrap_or(false)
}

/// Clear sentinel for the Claude permission-mode field, where "default" is a
/// real pinnable mode (unlike every other launch field).
pub(crate) fn session_config_clear_value_keeping_default(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .map(|value| value.is_empty() || matches!(value, "inherit" | "global"))
        .unwrap_or(false)
}

pub(crate) fn normalize_session_codex_sandbox(mode: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_sandbox(mode)
}

pub(crate) fn normalize_session_codex_approval_policy(policy: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_approval_policy(policy)
}

pub(crate) fn normalize_session_codex_context_archive(mode: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_context_archive(mode)
}

pub(crate) fn normalize_session_codex_service_tier(tier: Option<&str>) -> Option<String> {
    crate::project::normalize_codex_service_tier(tier)
}

pub(crate) fn normalize_session_name_option(name: Option<&str>) -> Result<Option<String>, String> {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => crate::session_names::normalize_session_name(name).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn apply_session_agent_command(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    command: String,
) {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.command = command;
        }
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.command = command;
        }
        external_agent::AgentBackend::Kimi => {
            project.config.agent.kimi.command = command;
        }
        external_agent::AgentBackend::Pi => {
            project.config.agent.pi.command = command;
        }
    }
}

pub(crate) fn apply_session_codex_managed_context(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.managed_context =
                crate::project::normalize_codex_managed_context(&mode);
            Ok(())
        }
        _ => Err("codex_managed_context requires Codex".to_string()),
    }
}

pub(crate) fn apply_session_claude_model(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    model: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.model = Some(model);
            Ok(())
        }
        _ => Err("claude_model requires Claude Code".to_string()),
    }
}

pub(crate) fn apply_session_claude_permission_mode(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.permission_mode =
                crate::project::normalize_claude_permission_mode(&mode);
            Ok(())
        }
        _ => Err("claude_permission_mode requires Claude Code".to_string()),
    }
}

pub(crate) fn apply_session_claude_effort(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    effort: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.effort =
                crate::project::normalize_claude_effort(Some(&effort));
            Ok(())
        }
        _ => Err("claude_effort requires Claude Code".to_string()),
    }
}

pub(crate) fn apply_session_kimi_model(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    model: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Kimi => {
            project.config.agent.kimi.model = Some(model);
            Ok(())
        }
        _ => Err("kimi_model requires Kimi".to_string()),
    }
}

pub(crate) fn apply_session_kimi_thinking(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    thinking: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Kimi => {
            project.config.agent.kimi.thinking =
                crate::project::normalize_kimi_thinking(Some(&thinking));
            Ok(())
        }
        _ => Err("kimi_thinking requires Kimi".to_string()),
    }
}

pub(crate) fn apply_session_kimi_permission_mode(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Kimi => {
            project.config.agent.kimi.permission_mode =
                crate::project::normalize_kimi_permission_mode(&mode);
            Ok(())
        }
        _ => Err("kimi_permission_mode requires Kimi".to_string()),
    }
}

pub(crate) fn apply_session_kimi_allowed_tools(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    tools: Vec<String>,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Kimi => {
            project.config.agent.kimi.allowed_tools =
                Some(crate::project::normalize_kimi_allowed_tools(&tools));
            Ok(())
        }
        _ => Err("kimi_allowed_tools requires Kimi".to_string()),
    }
}

pub(crate) fn apply_session_kimi_plan_mode(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    enabled: bool,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Kimi => {
            project.config.agent.kimi.plan_mode = enabled;
            Ok(())
        }
        _ => Err("kimi_plan_mode requires Kimi".to_string()),
    }
}

pub(crate) fn apply_session_kimi_swarm_mode(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    enabled: bool,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Kimi => {
            project.config.agent.kimi.swarm_mode = enabled;
            Ok(())
        }
        _ => Err("kimi_swarm_mode requires Kimi".to_string()),
    }
}

pub(crate) fn apply_session_pi_model(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    model: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Pi => {
            project.config.agent.pi.model = Some(model);
            Ok(())
        }
        _ => Err("pi_model requires Pi".to_string()),
    }
}

pub(crate) fn apply_session_pi_thinking(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    thinking: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Pi => {
            project.config.agent.pi.thinking =
                crate::project::normalize_pi_thinking(Some(&thinking));
            Ok(())
        }
        _ => Err("pi_thinking requires Pi".to_string()),
    }
}

pub(crate) fn apply_session_pi_allowed_tools(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    tools: Vec<String>,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Pi => {
            project.config.agent.pi.allowed_tools =
                Some(crate::project::normalize_pi_allowed_tools(&tools));
            Ok(())
        }
        _ => Err("pi_allowed_tools requires Pi".to_string()),
    }
}

pub(crate) fn apply_session_codex_model(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    model: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.model = Some(model);
            Ok(())
        }
        _ => Err("codex_model requires Codex".to_string()),
    }
}

pub(crate) fn apply_session_codex_reasoning_effort(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    effort: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            let normalized = crate::project::normalize_reasoning_effort(Some(&effort))
                .ok_or_else(|| format!("unsupported Codex reasoning effort: {effort}"))?;
            if let Some(model) = project.config.agent.codex.model.as_deref() {
                if let Some(entry) = crate::project::codex_model_catalog_entry(model) {
                    if !entry.reasoning_efforts.contains(&normalized.as_str()) {
                        return Err(format!(
                            "Codex model {model} does not support reasoning effort {normalized}"
                        ));
                    }
                }
            }
            project.config.agent.codex.reasoning_effort = Some(normalized);
            Ok(())
        }
        _ => Err("codex_reasoning_effort requires Codex".to_string()),
    }
}

pub(crate) fn apply_session_codex_sandbox(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(&mode);
            Ok(())
        }
        _ => Err("codex_sandbox requires Codex".to_string()),
    }
}

pub(crate) fn apply_session_codex_approval_policy(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    policy: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.approval_policy =
                crate::project::normalize_approval_policy(&policy);
            Ok(())
        }
        _ => Err("codex_approval_policy requires Codex".to_string()),
    }
}

pub(crate) fn apply_session_codex_context_archive(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.context_archive =
                crate::project::normalize_codex_context_archive(&mode);
            Ok(())
        }
        _ => Err("codex_context_archive requires Codex".to_string()),
    }
}

/// Rebuild the effective per-session config from the project, re-applying
/// the per-session facts a project can never supply. This enumerates fields
/// by hand (the pin policy): when adding a `SessionAgentConfig` field that
/// isn't project-derivable, extend BOTH `merge_missing_from` and this
/// copy-over, plus the preservation test below — a field missed here
/// silently reverts to the project default on the rebuild.
pub(crate) fn effective_session_agent_config_from_project(
    backend: &external_agent::AgentBackend,
    project: &Project,
    overrides: Option<&crate::session_config::SessionAgentConfig>,
) -> crate::session_config::SessionAgentConfig {
    let mut config = crate::session_config::from_project(backend, project);
    if matches!(backend, external_agent::AgentBackend::Codex) {
        if let Some(overrides) = overrides {
            if overrides.codex_service_tier.is_some() {
                config.codex_service_tier = overrides.codex_service_tier.clone();
            }
            if overrides.codex_home.is_some() {
                config.codex_home = overrides.codex_home.clone();
            }
        }
    }
    if matches!(backend, external_agent::AgentBackend::Kimi) {
        if let Some(overrides) = overrides {
            if overrides.kimi_home.is_some() {
                config.kimi_home = overrides.kimi_home.clone();
            }
        }
    }
    // Fork lineage is a per-session fact, never derivable from the project.
    if let Some(overrides) = overrides {
        if overrides.forked_from.is_some() {
            config.forked_from = overrides.forked_from.clone();
        }
        // The lineage KIND rides with it (side conversations emit `side`
        // instead of `fork` at the child's identity upgrade) — verified
        // live: dropping it here relabeled a /btw child as a plain fork.
        if overrides.fork_relationship.is_some() {
            config.fork_relationship = overrides.fork_relationship.clone();
        }
        if overrides.fork_anchor.is_some() {
            config.fork_anchor = overrides.fork_anchor.clone();
        }
        if overrides.codex_fork_rollout_path.is_some() {
            config.codex_fork_rollout_path = overrides.codex_fork_rollout_path.clone();
        }
        if overrides.codex_fork_rollback_turns.is_some() {
            config.codex_fork_rollback_turns = overrides.codex_fork_rollback_turns;
        }
        if overrides.codex_fork_rollback_item_id.is_some() {
            config.codex_fork_rollback_item_id = overrides.codex_fork_rollback_item_id.clone();
        }
        if overrides.codex_fork_rollback_position.is_some() {
            config.codex_fork_rollback_position = overrides.codex_fork_rollback_position.clone();
        }
        if overrides.kimi_fork_rollback_turns.is_some() {
            config.kimi_fork_rollback_turns = overrides.kimi_fork_rollback_turns;
        }
        if overrides.kimi_fork_expected_horizon.is_some() {
            config.kimi_fork_expected_horizon = overrides.kimi_fork_expected_horizon.clone();
        }
    }
    config
}

pub(crate) fn persist_external_session_name(
    home: &std::path::Path,
    bus: &EventBus,
    source: &str,
    session_id: &str,
    name: &str,
) {
    let source = crate::session_names::normalize_source(source);
    if source == "intendant" || name.trim().is_empty() {
        return;
    }
    let result = crate::session_names::rename_session(home, &source, session_id, name);
    if let Err(message) = result {
        bus.send(AppEvent::LogEntry {
            session_id: Some(session_id.to_string()),
            level: "warn".to_string(),
            source: "session-supervisor".to_string(),
            content: format!("Failed to persist session name: {}", message),
            turn: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{managed_session, test_supervisor};

    #[test]
    fn effective_config_preserves_fork_lineage_and_kind() {
        // The effective config is rebuilt from project defaults; fork
        // lineage (parent id + relationship kind) must survive the rebuild
        // or a /btw child re-labels as a plain fork (caught live).
        let project = Project {
            root: PathBuf::from("/tmp/project"),
            config: Default::default(),
        };
        let overrides = crate::session_config::SessionAgentConfig {
            forked_from: Some("parent-native".to_string()),
            fork_relationship: Some("side".to_string()),
            ..Default::default()
        };
        let config = effective_session_agent_config_from_project(
            &external_agent::AgentBackend::ClaudeCode,
            &project,
            Some(&overrides),
        );
        assert_eq!(config.forked_from.as_deref(), Some("parent-native"));
        assert_eq!(config.fork_relationship.as_deref(), Some("side"));
    }

    #[test]
    fn effective_config_preserves_anchor_fork_one_shots() {
        // Same pin policy as fork lineage: the anchor-fork staging fields
        // are per-session facts a project can never supply — dropping any
        // of them in the rebuild breaks the child's spawn-time fork.
        let project = Project {
            root: PathBuf::from("/tmp/project"),
            config: Default::default(),
        };
        let overrides = crate::session_config::SessionAgentConfig {
            forked_from: Some("parent-backend".to_string()),
            fork_relationship: Some("anchor-fork".to_string()),
            fork_anchor: Some("{\"kind\":\"turn-boundary\",\"turn\":2}".to_string()),
            codex_fork_rollout_path: Some("/tmp/staged.jsonl".to_string()),
            codex_fork_rollback_turns: Some(3),
            codex_fork_rollback_item_id: Some("item_9".to_string()),
            codex_fork_rollback_position: Some("after".to_string()),
            ..Default::default()
        };
        let config = effective_session_agent_config_from_project(
            &external_agent::AgentBackend::Codex,
            &project,
            Some(&overrides),
        );
        assert_eq!(config.fork_relationship.as_deref(), Some("anchor-fork"));
        assert!(config
            .fork_anchor
            .as_deref()
            .is_some_and(|anchor| anchor.contains("turn-boundary")));
        assert_eq!(
            config.codex_fork_rollout_path.as_deref(),
            Some("/tmp/staged.jsonl")
        );
        assert_eq!(config.codex_fork_rollback_turns, Some(3));
        assert_eq!(
            config.codex_fork_rollback_item_id.as_deref(),
            Some("item_9")
        );
        assert_eq!(
            config.codex_fork_rollback_position.as_deref(),
            Some("after")
        );

        let kimi_overrides = crate::session_config::SessionAgentConfig {
            forked_from: Some("session_parent".to_string()),
            fork_relationship: Some("anchor-fork".to_string()),
            fork_anchor: Some("{\"kind\":\"turn-boundary\",\"turn\":2}".to_string()),
            kimi_fork_rollback_turns: Some(4),
            kimi_fork_expected_horizon: Some("{\"active_turns\":6}".to_string()),
            kimi_home: Some("/tmp/private-kimi-bridge".to_string()),
            ..Default::default()
        };
        let kimi = effective_session_agent_config_from_project(
            &external_agent::AgentBackend::Kimi,
            &project,
            Some(&kimi_overrides),
        );
        assert_eq!(kimi.kimi_fork_rollback_turns, Some(4));
        assert_eq!(
            kimi.kimi_fork_expected_horizon.as_deref(),
            Some("{\"active_turns\":6}")
        );
        assert_eq!(kimi.kimi_home.as_deref(), Some("/tmp/private-kimi-bridge"));
    }

    #[tokio::test]
    async fn external_identity_moves_wrapper_session_to_backend_id() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("wrapper", "codex");
            session.phase = "thinking".to_string();
            state.sessions.insert("wrapper".to_string(), session);
            state.active_session_id = Some("wrapper".to_string());
        }

        supervisor
            .apply_session_identity(
                "wrapper".to_string(),
                "codex".to_string(),
                "backend".to_string(),
            )
            .await;

        let state = supervisor.state.lock().await;
        assert!(!state.sessions.contains_key("wrapper"));
        assert_eq!(
            state.resolve_session_id("wrapper").as_deref(),
            Some("backend")
        );
        assert_eq!(
            state.resolve_session_id("backend").as_deref(),
            Some("backend")
        );
        assert_eq!(state.active_session_id.as_deref(), Some("backend"));
        assert_eq!(
            state
                .sessions
                .get("backend")
                .map(|session| session.phase.as_str()),
            Some("thinking")
        );
    }

    #[tokio::test]
    async fn kimi_managed_rename_dispatches_native_thread_action() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "kimi-session".to_string(),
                managed_session("kimi-session", "kimi"),
            );
        }

        supervisor
            .rename_session(
                "kimi-session".to_string(),
                Some("kimi-session".to_string()),
                Some("kimi".to_string()),
                "Native Kimi title".to_string(),
            )
            .await;

        let event = events.recv().await.expect("native rename action");
        match event {
            AppEvent::ControlCommand(event::ControlMsg::CodexThreadAction {
                session_id,
                op,
                params,
                ..
            }) => {
                assert_eq!(session_id.as_deref(), Some("kimi-session"));
                assert_eq!(op, "rename");
                assert_eq!(params["name"], "Native Kimi title");
            }
            other => panic!("unexpected rename event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_identity_replaces_stale_backend_entry_with_new_wrapper() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (old_tx, mut old_rx) = mpsc::channel(1);
        let (new_tx, mut new_rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            let mut old_session = managed_session("backend", "codex");
            old_session.name = Some("saved name".to_string());
            old_session.phase = "done".to_string();
            old_session.follow_up_tx = old_tx;
            old_session.instance_id = 1;
            state.sessions.insert("backend".to_string(), old_session);

            let mut new_session = managed_session("wrapper-new", "codex");
            new_session.phase = "idle".to_string();
            new_session.follow_up_tx = new_tx;
            new_session.instance_id = 2;
            state
                .sessions
                .insert("wrapper-new".to_string(), new_session);
            state.active_session_id = Some("wrapper-new".to_string());
        }

        supervisor
            .apply_session_identity(
                "wrapper-new".to_string(),
                "codex".to_string(),
                "backend".to_string(),
            )
            .await;

        {
            let state = supervisor.state.lock().await;
            assert!(!state.sessions.contains_key("wrapper-new"));
            assert_eq!(
                state.resolve_session_id("wrapper-new").as_deref(),
                Some("backend")
            );
            let session = state.sessions.get("backend").expect("backend session");
            assert_eq!(session.phase, "idle");
            assert_eq!(session.instance_id, 2);
            assert_eq!(session.name.as_deref(), Some("saved name"));
            assert_eq!(state.active_session_id.as_deref(), Some("backend"));
        }

        supervisor
            .route_edit_user_message(
                Some("backend".to_string()),
                None,
                None,
                None,
                Some(true),
                117,
                Some(1),
                Some("old prompt".to_string()),
                "new prompt".to_string(),
                Vec::new(),
            )
            .await;

        assert!(old_rx.try_recv().is_err());
        let msg = new_rx
            .try_recv()
            .expect("edit should route to the newly attached wrapper");
        assert_eq!(msg.text, "new prompt");
        assert_eq!(msg.edit_user_turn_index, Some(117));
        assert_eq!(msg.edit_user_turn_revision, Some(1));
    }

    #[tokio::test]
    async fn identity_rekey_drops_pre_identity_alias_without_shadowing() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (tx, _rx) = mpsc::channel(1);
        supervisor
            .register_session(
                "wrapper-1".to_string(),
                "codex".to_string(),
                "idle".to_string(),
                PathBuf::from("/tmp/project"),
                PathBuf::from("/tmp/session"),
                tx,
                event::ApprovalRegistry::default(),
                None,
                None,
                Some("backend-thread".to_string()),
                0,
                None,
            )
            .await;
        supervisor
            .apply_session_identity(
                "wrapper-1".to_string(),
                "codex".to_string(),
                "backend-thread".to_string(),
            )
            .await;

        let state = supervisor.state.lock().await;
        // Re-keyed entry is addressable by both ids...
        assert_eq!(
            state.resolve_session_id("backend-thread").as_deref(),
            Some("backend-thread")
        );
        assert_eq!(
            state.resolve_session_id("wrapper-1").as_deref(),
            Some("backend-thread")
        );
        // ...and no alias entry shadows the live backend key.
        assert!(!state.session_aliases.contains_key("backend-thread"));
    }

    #[test]
    fn fast_new_session_forces_or_accepts_codex_agent() {
        assert_eq!(
            codex_fast_new_session_agent(None).unwrap(),
            "codex".to_string()
        );
        assert_eq!(
            codex_fast_new_session_agent(Some("configured")).unwrap(),
            "codex".to_string()
        );
        assert_eq!(
            codex_fast_new_session_agent(Some("codex")).unwrap(),
            "codex".to_string()
        );

        let err = codex_fast_new_session_agent(Some("claude-code")).unwrap_err();
        assert!(err.contains("Codex"), "got: {err}");
        let err = codex_fast_new_session_agent(Some("internal")).unwrap_err();
        assert!(err.contains("Codex"), "got: {err}");
        // Retired backend: "gemini" fails as an unknown agent, not a
        // non-Codex selection.
        let err = codex_fast_new_session_agent(Some("gemini")).unwrap_err();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[test]
    fn parses_session_agent_selection() {
        assert_eq!(
            SessionAgentSelection::from_wire(None).unwrap(),
            SessionAgentSelection::Configured
        );
        assert_eq!(
            SessionAgentSelection::from_wire(Some("internal")).unwrap(),
            SessionAgentSelection::Internal
        );
        assert_eq!(
            SessionAgentSelection::from_wire(Some("kimi-code")).unwrap(),
            SessionAgentSelection::External(external_agent::AgentBackend::Kimi)
        );
        // Retired backend: "gemini" must no longer resolve to a live backend.
        assert!(SessionAgentSelection::from_wire(Some("gemini")).is_err());
        assert!(SessionAgentSelection::from_wire(Some("unknown")).is_err());
    }

    #[test]
    fn applies_session_agent_command_to_selected_backend() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_agent_command(
            &mut project,
            &external_agent::AgentBackend::ClaudeCode,
            "/opt/claude/bin/claude".to_string(),
        );
        assert_eq!(
            project.config.agent.claude_code.command,
            "/opt/claude/bin/claude"
        );
    }

    /// The create/resume wire normalizers must treat the `inherit` sentinel
    /// (and empty strings) as "no per-session override" — not pin the
    /// project-level default. Explicit values still pin, and absent stays
    /// absent. This is what lets a launch-config save change only the
    /// binary path without permanently pinning vanilla into the session.
    #[test]
    fn session_codex_managed_context_inherit_means_no_override() {
        for sentinel in [
            Some("inherit"),
            Some("default"),
            Some("global"),
            Some(""),
            None,
        ] {
            assert_eq!(
                normalize_session_codex_managed_context(sentinel),
                None,
                "{sentinel:?} should not produce a managed-context override"
            );
            assert_eq!(
                normalize_session_codex_context_archive(sentinel),
                None,
                "{sentinel:?} should not produce a context-archive override"
            );
        }
        assert_eq!(
            normalize_session_codex_managed_context(Some("managed")).as_deref(),
            Some("managed")
        );
        assert_eq!(
            normalize_session_codex_managed_context(Some("vanilla")).as_deref(),
            Some("vanilla")
        );
        assert_eq!(
            normalize_session_codex_context_archive(Some("exact")).as_deref(),
            Some("exact")
        );
        // The configure_session_agent clear flags use the same sentinel set.
        assert!(session_config_clear_value(Some("inherit")));
        assert!(session_config_clear_value(Some("")));
        assert!(!session_config_clear_value(Some("managed")));
        assert!(!session_config_clear_value(Some("vanilla")));
        assert!(!session_config_clear_value(None));
        // The Claude permission-mode variant keeps "default" pinnable.
        assert!(session_config_clear_value_keeping_default(Some("inherit")));
        assert!(session_config_clear_value_keeping_default(Some("global")));
        assert!(session_config_clear_value_keeping_default(Some("")));
        assert!(!session_config_clear_value_keeping_default(Some("default")));
        assert!(!session_config_clear_value_keeping_default(Some(
            "acceptEdits"
        )));
        assert!(!session_config_clear_value_keeping_default(None));
    }

    #[test]
    fn launch_overrides_map_to_wire_fields_and_gate_by_source() {
        let overrides = LaunchOverrides {
            agent_command: Some("/tmp/claude".to_string()),
            claude_model: Some("sonnet".to_string()),
            claude_permission_mode: Some("plan".to_string()),
            claude_allowed_tools: Some("Read, Bash(cargo test *)".to_string()),
            claude_effort: Some("high".to_string()),
            ..Default::default()
        };
        // The claude configure path: fields normalize into pins.
        let config =
            crate::session_config::from_wire_fields(overrides.as_wire_fields("claude-code"));
        assert_eq!(config.agent_command.as_deref(), Some("/tmp/claude"));
        assert_eq!(config.claude_model.as_deref(), Some("sonnet"));
        assert_eq!(config.claude_permission_mode.as_deref(), Some("plan"));
        assert_eq!(
            config.claude_allowed_tools.as_deref(),
            Some(&["Read".to_string(), "Bash(cargo test *)".into()][..])
        );
        assert_eq!(config.claude_effort.as_deref(), Some("high"));
        // The same overrides against a codex session never leak claude pins.
        let cross = crate::session_config::from_wire_fields(overrides.as_wire_fields("codex"));
        assert!(cross.claude_model.is_none());
        assert!(cross.claude_permission_mode.is_none());
        assert!(cross.claude_allowed_tools.is_none());
        assert!(cross.claude_effort.is_none());

        let kimi_overrides = LaunchOverrides {
            agent_command: Some("/tmp/kimi".to_string()),
            kimi_model: Some("k2.7 coding".to_string()),
            kimi_thinking: Some("high".to_string()),
            kimi_permission_mode: Some("yolo".to_string()),
            kimi_allowed_tools: Some("Read,\nWrite,Read".to_string()),
            kimi_plan_mode: Some("true".to_string()),
            kimi_swarm_mode: Some("enabled".to_string()),
            ..Default::default()
        };
        let kimi =
            crate::session_config::from_wire_fields(kimi_overrides.as_wire_fields("kimi-code"));
        assert_eq!(kimi.agent_command.as_deref(), Some("/tmp/kimi"));
        assert_eq!(kimi.kimi_model.as_deref(), Some("k2.7 coding"));
        assert_eq!(kimi.kimi_thinking.as_deref(), Some("high"));
        assert_eq!(kimi.kimi_permission_mode.as_deref(), Some("yolo"));
        assert_eq!(
            kimi.kimi_allowed_tools.as_deref(),
            Some(&["Read".to_string(), "Write".to_string()][..])
        );
        assert_eq!(kimi.kimi_plan_mode, Some(true));
        assert_eq!(kimi.kimi_swarm_mode, Some(true));
        let cross = crate::session_config::from_wire_fields(kimi_overrides.as_wire_fields("codex"));
        assert!(cross.kimi_model.is_none());
        assert!(cross.kimi_thinking.is_none());
        assert!(cross.kimi_permission_mode.is_none());
        assert!(cross.kimi_allowed_tools.is_none());
        assert!(cross.kimi_plan_mode.is_none());
        assert!(cross.kimi_swarm_mode.is_none());

        let pi_overrides = LaunchOverrides {
            agent_command: Some("/tmp/pi".to_string()),
            pi_model: Some("openai-codex/gpt-5.6-codex".to_string()),
            pi_thinking: Some(" HIGH ".to_string()),
            pi_allowed_tools: Some("read,\nbash,read".to_string()),
            ..Default::default()
        };
        let pi = crate::session_config::from_wire_fields(pi_overrides.as_wire_fields("pi"));
        assert_eq!(pi.agent_command.as_deref(), Some("/tmp/pi"));
        assert_eq!(pi.pi_model.as_deref(), Some("openai-codex/gpt-5.6-codex"));
        assert_eq!(pi.pi_thinking.as_deref(), Some("high"));
        assert_eq!(
            pi.pi_allowed_tools.as_deref(),
            Some(&["read".to_string(), "bash".to_string()][..])
        );
        let cross = crate::session_config::from_wire_fields(pi_overrides.as_wire_fields("codex"));
        assert!(cross.pi_model.is_none());
        assert!(cross.pi_thinking.is_none());
        assert!(cross.pi_allowed_tools.is_none());
    }

    #[test]
    fn applies_all_session_kimi_controls_to_kimi_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        let kimi = external_agent::AgentBackend::Kimi;
        apply_session_kimi_model(&mut project, &kimi, "k2.7 coding".to_string()).unwrap();
        apply_session_kimi_thinking(&mut project, &kimi, " HIGH ".to_string()).unwrap();
        apply_session_kimi_permission_mode(&mut project, &kimi, "bypass-permissions".to_string())
            .unwrap();
        apply_session_kimi_allowed_tools(
            &mut project,
            &kimi,
            vec![
                " Read ".to_string(),
                "Write".to_string(),
                "Read".to_string(),
            ],
        )
        .unwrap();
        apply_session_kimi_plan_mode(&mut project, &kimi, true).unwrap();
        apply_session_kimi_swarm_mode(&mut project, &kimi, true).unwrap();
        assert_eq!(
            project.config.agent.kimi.model.as_deref(),
            Some("k2.7 coding")
        );
        assert_eq!(project.config.agent.kimi.thinking.as_deref(), Some("high"));
        assert_eq!(project.config.agent.kimi.permission_mode, "yolo");
        assert_eq!(
            project.config.agent.kimi.allowed_tools.as_deref(),
            Some(&["Read".to_string(), "Write".to_string()][..])
        );
        assert!(project.config.agent.kimi.plan_mode);
        assert!(project.config.agent.kimi.swarm_mode);

        let codex = external_agent::AgentBackend::Codex;
        for error in [
            apply_session_kimi_model(&mut project, &codex, "model".to_string()).unwrap_err(),
            apply_session_kimi_thinking(&mut project, &codex, "high".to_string()).unwrap_err(),
            apply_session_kimi_permission_mode(&mut project, &codex, "auto".to_string())
                .unwrap_err(),
            apply_session_kimi_allowed_tools(&mut project, &codex, Vec::new()).unwrap_err(),
            apply_session_kimi_plan_mode(&mut project, &codex, false).unwrap_err(),
            apply_session_kimi_swarm_mode(&mut project, &codex, false).unwrap_err(),
        ] {
            assert!(error.contains("requires Kimi"), "got: {error}");
        }
    }

    #[test]
    fn applies_all_session_pi_controls_to_pi_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        let pi = external_agent::AgentBackend::Pi;
        apply_session_agent_command(&mut project, &pi, "/opt/pi/bin/pi".to_string());
        apply_session_pi_model(&mut project, &pi, "openai-codex/gpt-5.6-codex".to_string())
            .unwrap();
        apply_session_pi_thinking(&mut project, &pi, " HIGH ".to_string()).unwrap();
        apply_session_pi_allowed_tools(
            &mut project,
            &pi,
            vec![" read ".to_string(), "bash".to_string(), "read".to_string()],
        )
        .unwrap();
        assert_eq!(project.config.agent.pi.command, "/opt/pi/bin/pi");
        assert_eq!(
            project.config.agent.pi.model.as_deref(),
            Some("openai-codex/gpt-5.6-codex")
        );
        assert_eq!(project.config.agent.pi.thinking.as_deref(), Some("high"));
        assert_eq!(
            project.config.agent.pi.allowed_tools.as_deref(),
            Some(&["read".to_string(), "bash".to_string()][..])
        );

        let codex = external_agent::AgentBackend::Codex;
        for error in [
            apply_session_pi_model(&mut project, &codex, "model".to_string()).unwrap_err(),
            apply_session_pi_thinking(&mut project, &codex, "high".to_string()).unwrap_err(),
            apply_session_pi_allowed_tools(&mut project, &codex, Vec::new()).unwrap_err(),
        ] {
            assert!(error.contains("requires Pi"), "got: {error}");
        }
    }

    #[test]
    fn applies_session_codex_managed_context_to_codex_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_codex_managed_context(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "on".to_string(),
        )
        .unwrap();
        assert_eq!(project.config.agent.codex.managed_context, "managed");

        let err = apply_session_codex_managed_context(
            &mut project,
            &external_agent::AgentBackend::ClaudeCode,
            "managed".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("requires Codex"));
    }

    #[test]
    fn applies_session_codex_context_archive_to_codex_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_codex_context_archive(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "raw".to_string(),
        )
        .unwrap();
        assert_eq!(project.config.agent.codex.context_archive, "exact");

        let err = apply_session_codex_context_archive(
            &mut project,
            &external_agent::AgentBackend::ClaudeCode,
            "summary".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("requires Codex"));
    }

    #[test]
    fn rejects_reasoning_effort_incompatible_with_catalog_model() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        project.config.agent.codex.model = Some("gpt-5.6-luna".to_string());

        let err = apply_session_codex_reasoning_effort(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "ultra".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("gpt-5.6-luna"));
        assert!(err.contains("ultra"));

        apply_session_codex_reasoning_effort(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "max".to_string(),
        )
        .unwrap();
        assert_eq!(
            project.config.agent.codex.reasoning_effort.as_deref(),
            Some("max")
        );
    }

    #[test]
    fn normalizes_optional_session_name() {
        assert_eq!(
            normalize_session_name_option(Some("  Dashboard   work  ")).unwrap(),
            Some("Dashboard work".to_string())
        );
        assert_eq!(normalize_session_name_option(Some("   ")).unwrap(), None);
        assert_eq!(normalize_session_name_option(None).unwrap(), None);
    }

    /// The dry intake validator (Track AU) accepts exactly what the
    /// launch path would accept and names exactly what it would
    /// deterministically reject — and stays deliberately silent on
    /// `Configured`-selection pins, whose backend resolves against the
    /// daemon default at fire time by design.
    #[test]
    fn validate_launch_config_mirrors_the_launch_gates() {
        use crate::event::AgentLaunchConfig;
        let ok = |config: AgentLaunchConfig| {
            validate_launch_config(&config).unwrap_or_else(|e| panic!("expected accept, got {e}"))
        };
        let rejects = |config: AgentLaunchConfig, needle: &str| {
            let err = validate_launch_config(&config).unwrap_err();
            assert!(err.contains(needle), "wanted {needle:?} in {err:?}");
        };

        ok(AgentLaunchConfig::default());
        ok(AgentLaunchConfig {
            agent: Some("claude-code".into()),
            claude_model: Some("fable-5".into()),
            claude_effort: Some("max".into()),
            ..Default::default()
        });
        // Configured selection: pins ride to fire-time resolution.
        ok(AgentLaunchConfig {
            claude_effort: Some("max".into()),
            ..Default::default()
        });
        // Whitespace-only pins are not pins (the launch path's filter).
        ok(AgentLaunchConfig {
            agent: Some("internal".into()),
            claude_effort: Some("   ".into()),
            ..Default::default()
        });
        ok(AgentLaunchConfig {
            agent: Some("codex".into()),
            codex_model: Some("gpt-5.6-sol".into()),
            codex_reasoning_effort: Some("ultra".into()),
            ..Default::default()
        });

        rejects(
            AgentLaunchConfig {
                agent: Some("warp-drive".into()),
                ..Default::default()
            },
            "unknown agent",
        );
        rejects(
            AgentLaunchConfig {
                agent: Some("internal".into()),
                agent_command: Some("codex".into()),
                ..Default::default()
            },
            "agent_command requires an external agent",
        );
        rejects(
            AgentLaunchConfig {
                agent: Some("internal".into()),
                kimi_model: Some("k2".into()),
                ..Default::default()
            },
            "require Kimi",
        );
        rejects(
            AgentLaunchConfig {
                agent: Some("claude-code".into()),
                pi_model: Some("gpt-5.2".into()),
                ..Default::default()
            },
            "require Pi",
        );
        rejects(
            AgentLaunchConfig {
                agent: Some("kimi".into()),
                codex_sandbox: Some("workspace-write".into()),
                ..Default::default()
            },
            "require Codex",
        );
        rejects(
            AgentLaunchConfig {
                agent: Some("codex".into()),
                codex_reasoning_effort: Some("warp".into()),
                ..Default::default()
            },
            "unsupported Codex reasoning effort",
        );
        rejects(
            AgentLaunchConfig {
                agent: Some("codex".into()),
                codex_model: Some("gpt-5.6-luna".into()),
                codex_reasoning_effort: Some("ultra".into()),
                ..Default::default()
            },
            "does not support reasoning effort",
        );
        rejects(
            AgentLaunchConfig {
                agent: Some("kimi".into()),
                agent_command: Some("claude".into()),
                ..Default::default()
            },
            "is the claude-code CLI but the session's agent is kimi",
        );
    }
}
