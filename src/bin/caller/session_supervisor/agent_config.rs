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
            if managed_source == "codex" {
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
        let is_codex = matches!(backend, external_agent::AgentBackend::Codex);
        let is_claude = matches!(backend, external_agent::AgentBackend::ClaudeCode);
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
                    "unknown agent '{}' (expected internal, codex, or claude-code)",
                    trimmed
                )
            })
    }
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
}
