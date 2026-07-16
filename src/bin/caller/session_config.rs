use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::external_agent::AgentBackend;
use crate::project::Project;

pub const SESSION_AGENT_CONFIG_FILE: &str = "session_agent_config.json";
const OVERLAY_FILE: &str = "session_agent_config.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAgentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_approval_policy: Option<String>,
    #[serde(
        default,
        alias = "codex_context_recovery",
        skip_serializing_if = "Option::is_none"
    )]
    pub codex_managed_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_context_archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    /// Claude Code launch pins (claude-code sessions only; same
    /// inherit-vs-pin semantics as the codex_* fields: `None` = inherit the
    /// global/Control default at spawn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_permission_mode: Option<String>,
    /// `None` = inherit; `Some(vec![])` = explicitly unrestricted (all
    /// tools), so a session can opt out of a restrictive global list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_effort: Option<String>,
    /// Canonical native id of the thread this session was FORKED from
    /// (backend-neutral lineage record). While the fork's own native id is
    /// still unknown, `resume == forked_from` also tells the spawner to add
    /// the backend's fork flag; once the child id is persisted, this only
    /// documents lineage and drives the `fork` relationship emit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Relationship kind the `forked_from` edge carries when the child
    /// announces its native id: `None` = plain `fork`; `side` = an
    /// ephemeral side conversation (`/btw`) materialized as a respawned
    /// fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_relationship: Option<String>,
}

impl SessionAgentConfig {
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.project_root.is_none()
            && self.agent_command.is_none()
            && self.codex_model.is_none()
            && self.codex_reasoning_effort.is_none()
            && self.codex_sandbox.is_none()
            && self.codex_approval_policy.is_none()
            && self.codex_managed_context.is_none()
            && self.codex_context_archive.is_none()
            && self.codex_service_tier.is_none()
            && self.codex_home.is_none()
            && self.claude_model.is_none()
            && self.claude_permission_mode.is_none()
            && self.claude_allowed_tools.is_none()
            && self.claude_effort.is_none()
            && self.forked_from.is_none()
            && self.fork_relationship.is_none()
    }

    pub fn merge_missing_from(&mut self, fallback: SessionAgentConfig) {
        if self.source.is_none() {
            self.source = fallback.source;
        }
        if self.project_root.is_none() {
            self.project_root = fallback.project_root;
        }
        if self.agent_command.is_none() {
            self.agent_command = fallback.agent_command;
        }
        if self.codex_model.is_none() {
            self.codex_model = fallback.codex_model;
        }
        if self.codex_reasoning_effort.is_none() {
            self.codex_reasoning_effort = fallback.codex_reasoning_effort;
        }
        if self.codex_sandbox.is_none() {
            self.codex_sandbox = fallback.codex_sandbox;
        }
        if self.codex_approval_policy.is_none() {
            self.codex_approval_policy = fallback.codex_approval_policy;
        }
        if self.codex_managed_context.is_none() {
            self.codex_managed_context = fallback.codex_managed_context;
        }
        if self.codex_context_archive.is_none() {
            self.codex_context_archive = fallback.codex_context_archive;
        }
        if self.codex_service_tier.is_none() {
            self.codex_service_tier = fallback.codex_service_tier;
        }
        if self.codex_home.is_none() {
            self.codex_home = fallback.codex_home;
        }
        if self.claude_model.is_none() {
            self.claude_model = fallback.claude_model;
        }
        if self.claude_permission_mode.is_none() {
            self.claude_permission_mode = fallback.claude_permission_mode;
        }
        if self.claude_allowed_tools.is_none() {
            self.claude_allowed_tools = fallback.claude_allowed_tools;
        }
        if self.claude_effort.is_none() {
            self.claude_effort = fallback.claude_effort;
        }
        if self.forked_from.is_none() {
            self.forked_from = fallback.forked_from;
        }
        if self.fork_relationship.is_none() {
            self.fork_relationship = fallback.fork_relationship;
        }
    }
}

/// The agent command names ANOTHER known backend's CLI: `Some(<that
/// backend's short name>)` when `command`'s executable is unmistakably a
/// different agent's canonical binary than `source`'s (spawning the codex
/// CLI with claude-code's wire flags can never work — observed live
/// 2026-07-16, where a contaminated catalog row resumed a claude-code
/// session with `agent_command: "codex"` and the session died on codex's
/// argument parser). Custom wrappers and absolute paths pass: only an
/// executable stem that IS a known backend's binary name conflicts.
/// Unknown sources are never judged.
pub fn agent_command_conflicts_with_source(source: &str, command: &str) -> Option<&'static str> {
    let source_backend = AgentBackend::from_str_loose(source)?;
    let executable = command.split_whitespace().next()?;
    let stem = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .trim();
    let stem = stem
        .rsplit_once('.')
        .filter(|(_, ext)| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "exe" | "cmd" | "bat" | "ps1"
            )
        })
        .map(|(name, _)| name)
        .unwrap_or(stem);
    let owner = match stem.to_ascii_lowercase().as_str() {
        "codex" => AgentBackend::Codex,
        "claude" => AgentBackend::ClaudeCode,
        _ => return None,
    };
    (owner != source_backend).then(|| owner.as_short_str())
}

/// Drop an agent command that names a different backend's CLI than
/// `source` (see [`agent_command_conflicts_with_source`]). Every config
/// funnel (wire parse, dir/overlay read, overlay write) applies this, so a
/// cross-agent command can neither launch nor persist — and stores that
/// were already poisoned heal on the next read.
fn sanitize_agent_command_for_source(
    source: Option<&str>,
    command: Option<String>,
) -> Option<String> {
    let command = command?;
    match source {
        Some(source) if agent_command_conflicts_with_source(source, &command).is_some() => None,
        _ => Some(command),
    }
}

pub fn normalize_agent_command(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn normalize_project_root(root: Option<&str>) -> Option<String> {
    root.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn normalize_codex_service_tier(tier: Option<&str>) -> Option<String> {
    crate::project::normalize_codex_service_tier(tier)
}

pub fn normalize_codex_model(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Per-session Codex reasoning-effort pin. Empty and the explicit inherit
/// sentinels clear the pin; known values share the project-level normalizer.
pub fn normalize_codex_reasoning_effort(effort: Option<&str>) -> Option<String> {
    let trimmed = effort.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    crate::project::normalize_reasoning_effort(Some(trimmed))
}

pub fn normalize_codex_home(home: Option<&str>) -> Option<String> {
    home.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn normalize_codex_sandbox(mode: Option<&str>) -> Option<String> {
    let trimmed = mode.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    Some(crate::project::normalize_sandbox_mode(trimmed))
}

pub fn normalize_codex_approval_policy(policy: Option<&str>) -> Option<String> {
    let trimmed = policy.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    Some(crate::project::normalize_approval_policy(trimmed))
}

/// Per-session managed-context override. `None` means "no per-session pin —
/// inherit the global default". The `inherit` sentinel (and empty input) must
/// map to `None` BEFORE the project-level normalizer runs, because that
/// normalizer maps every unrecognized string — including "inherit" — to
/// `"vanilla"`, which would silently pin vanilla into the session overlay.
pub fn normalize_codex_managed_context(mode: Option<&str>) -> Option<String> {
    let trimmed = mode.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    Some(crate::project::normalize_codex_managed_context(trimmed))
}

/// Per-session context-archive override; same `inherit` semantics as
/// [`normalize_codex_managed_context`] (the project-level normalizer would
/// otherwise collapse "inherit" to `"summary"`).
pub fn normalize_codex_context_archive(mode: Option<&str>) -> Option<String> {
    let trimmed = mode.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    Some(crate::project::normalize_codex_context_archive(trimmed))
}

/// Per-session Claude model pin. `None` clears (inherit); "default" is safe
/// as a clear sentinel here because it is never a model id or alias.
pub fn normalize_claude_model(model: Option<&str>) -> Option<String> {
    let trimmed = model.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    Some(trimmed.to_string())
}

/// Per-session permission-mode pin. Unlike the other fields, "default" is a
/// REAL Claude Code mode and must stay pinnable (a session can pin `default`
/// under a stricter global mode) — only "inherit"/"global"/empty clear.
pub fn normalize_claude_permission_mode(mode: Option<&str>) -> Option<String> {
    let trimmed = mode.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "global") {
        return None;
    }
    Some(crate::project::normalize_claude_permission_mode(trimmed))
}

/// Per-session allowed-tools pin, comma-separated on the wire (rules can
/// contain spaces — `Bash(cargo test *)` — but never commas: the spawner
/// joins the list with commas for `--allowedTools`). "all"/"*" pins the
/// explicitly-unrestricted empty list so a session can escape a restrictive
/// global list; "inherit"/"default"/"global"/empty clear the pin.
pub fn normalize_claude_allowed_tools(tools: Option<&str>) -> Option<Vec<String>> {
    let trimmed = tools.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    if matches!(trimmed, "all" | "*") {
        return Some(Vec::new());
    }
    Some(
        trimmed
            .split(',')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Per-session reasoning-effort pin. "default" is not a real effort level,
/// so it clears alongside "inherit"/"global" (matching the project-level
/// normalizer, which also treats "default" as unset).
pub fn normalize_claude_effort(effort: Option<&str>) -> Option<String> {
    let trimmed = effort.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    crate::project::normalize_claude_effort(Some(trimmed))
}

pub fn effective_codex_home() -> Option<String> {
    let from_env = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = from_env.unwrap_or_else(|| crate::platform::home_dir().join(".codex"));
    normalize_codex_home(Some(&home.to_string_lossy()))
}

/// Raw wire values for a per-session launch config, named so call sites can
/// set only the fields their backend carries (everything else defaults to
/// "not supplied"). Backend gating and normalization happen in
/// [`from_wire_fields`].
#[derive(Debug, Default, Clone, Copy)]
pub struct WireSessionAgentFields<'a> {
    pub source: Option<&'a str>,
    pub agent_command: Option<&'a str>,
    pub codex_model: Option<&'a str>,
    pub codex_reasoning_effort: Option<&'a str>,
    pub codex_sandbox: Option<&'a str>,
    pub codex_approval_policy: Option<&'a str>,
    pub codex_managed_context: Option<&'a str>,
    pub codex_context_archive: Option<&'a str>,
    pub codex_service_tier: Option<&'a str>,
    pub claude_model: Option<&'a str>,
    pub claude_permission_mode: Option<&'a str>,
    pub claude_allowed_tools: Option<&'a str>,
    pub claude_effort: Option<&'a str>,
}

pub fn from_wire(
    source: Option<&str>,
    agent_command: Option<&str>,
    codex_sandbox: Option<&str>,
    codex_approval_policy: Option<&str>,
    codex_managed_context: Option<&str>,
    codex_context_archive: Option<&str>,
    codex_service_tier: Option<&str>,
) -> SessionAgentConfig {
    from_wire_fields(WireSessionAgentFields {
        source,
        agent_command,
        codex_sandbox,
        codex_approval_policy,
        codex_managed_context,
        codex_context_archive,
        codex_service_tier,
        ..Default::default()
    })
}

pub fn from_wire_fields(fields: WireSessionAgentFields) -> SessionAgentConfig {
    let source = fields
        .source
        .map(crate::session_names::normalize_source)
        .filter(|value| !value.is_empty());
    let is_codex = source.as_deref() == Some("codex");
    let is_claude = source.as_deref() == Some("claude-code");
    let agent_command = sanitize_agent_command_for_source(
        source.as_deref(),
        normalize_agent_command(fields.agent_command),
    );
    SessionAgentConfig {
        source,
        project_root: None,
        agent_command,
        codex_model: is_codex
            .then(|| normalize_codex_model(fields.codex_model))
            .flatten(),
        codex_reasoning_effort: is_codex
            .then(|| normalize_codex_reasoning_effort(fields.codex_reasoning_effort))
            .flatten(),
        codex_sandbox: is_codex
            .then(|| normalize_codex_sandbox(fields.codex_sandbox))
            .flatten(),
        codex_approval_policy: is_codex
            .then(|| normalize_codex_approval_policy(fields.codex_approval_policy))
            .flatten(),
        codex_managed_context: is_codex
            .then(|| normalize_codex_managed_context(fields.codex_managed_context))
            .flatten(),
        codex_context_archive: is_codex
            .then(|| normalize_codex_context_archive(fields.codex_context_archive))
            .flatten(),
        codex_service_tier: is_codex
            .then(|| normalize_codex_service_tier(fields.codex_service_tier))
            .flatten(),
        codex_home: None,
        claude_model: is_claude
            .then(|| normalize_claude_model(fields.claude_model))
            .flatten(),
        claude_permission_mode: is_claude
            .then(|| normalize_claude_permission_mode(fields.claude_permission_mode))
            .flatten(),
        claude_allowed_tools: is_claude
            .then(|| normalize_claude_allowed_tools(fields.claude_allowed_tools))
            .flatten(),
        claude_effort: is_claude
            .then(|| normalize_claude_effort(fields.claude_effort))
            .flatten(),
        forked_from: None,
        fork_relationship: None,
    }
}

pub fn from_project(backend: &AgentBackend, project: &Project) -> SessionAgentConfig {
    match backend {
        AgentBackend::Codex => SessionAgentConfig {
            source: Some("codex".to_string()),
            project_root: normalize_project_root(Some(&project.root.to_string_lossy())),
            agent_command: Some(project.config.agent.codex.command.clone()),
            codex_model: normalize_codex_model(project.config.agent.codex.model.as_deref()),
            codex_reasoning_effort: normalize_codex_reasoning_effort(
                project.config.agent.codex.reasoning_effort.as_deref(),
            ),
            codex_sandbox: Some(crate::project::normalize_sandbox_mode(
                &project.config.agent.codex.sandbox,
            )),
            codex_approval_policy: Some(crate::project::normalize_approval_policy(
                &project.config.agent.codex.approval_policy,
            )),
            codex_managed_context: Some(crate::project::normalize_codex_managed_context(
                &project.config.agent.codex.managed_context,
            )),
            codex_context_archive: Some(crate::project::normalize_codex_context_archive(
                &project.config.agent.codex.context_archive,
            )),
            codex_service_tier: crate::project::normalize_codex_service_tier(
                project.config.agent.codex.service_tier.as_deref(),
            ),
            codex_home: effective_codex_home(),
            claude_model: None,
            claude_permission_mode: None,
            claude_allowed_tools: None,
            claude_effort: None,
            forked_from: None,
            fork_relationship: None,
        },
        AgentBackend::ClaudeCode => {
            let claude = &project.config.agent.claude_code;
            SessionAgentConfig {
                source: Some("claude-code".to_string()),
                project_root: normalize_project_root(Some(&project.root.to_string_lossy())),
                agent_command: Some(claude.command.clone()),
                codex_model: None,
                codex_reasoning_effort: None,
                codex_sandbox: None,
                codex_approval_policy: None,
                codex_managed_context: None,
                codex_context_archive: None,
                codex_service_tier: None,
                codex_home: None,
                // Pin the launch-time settings (same as the codex arm pins
                // sandbox/approval): a resume reproduces what this session
                // was actually launched with, not the current global config.
                claude_model: normalize_claude_model(claude.model.as_deref()),
                claude_permission_mode: Some(crate::project::normalize_claude_permission_mode(
                    &claude.permission_mode,
                )),
                claude_allowed_tools: Some(claude.allowed_tools.clone()),
                claude_effort: crate::project::normalize_claude_effort(claude.effort.as_deref()),
                forked_from: None,
                fork_relationship: None,
            }
        }
    }
}

pub fn apply_to_project(
    project: &mut Project,
    backend: &AgentBackend,
    config: &SessionAgentConfig,
) {
    match backend {
        AgentBackend::Codex => {
            if let Some(command) = config.agent_command.clone() {
                project.config.agent.codex.command = command;
            }
            if let Some(model) = config.codex_model.clone() {
                project.config.agent.codex.model = Some(model);
            }
            if let Some(effort) = config.codex_reasoning_effort.clone() {
                project.config.agent.codex.reasoning_effort = Some(effort);
            }
            if let Some(mode) = config.codex_sandbox.clone() {
                project.config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(&mode);
            }
            if let Some(policy) = config.codex_approval_policy.clone() {
                project.config.agent.codex.approval_policy =
                    crate::project::normalize_approval_policy(&policy);
            }
            if let Some(mode) = config.codex_managed_context.clone() {
                project.config.agent.codex.managed_context =
                    crate::project::normalize_codex_managed_context(&mode);
            }
            if let Some(mode) = config.codex_context_archive.clone() {
                project.config.agent.codex.context_archive =
                    crate::project::normalize_codex_context_archive(&mode);
            }
        }
        AgentBackend::ClaudeCode => {
            let claude = &mut project.config.agent.claude_code;
            if let Some(command) = config.agent_command.clone() {
                claude.command = command;
            }
            if let Some(model) = config.claude_model.clone() {
                claude.model = Some(model);
            }
            if let Some(mode) = config.claude_permission_mode.as_deref() {
                claude.permission_mode = crate::project::normalize_claude_permission_mode(mode);
            }
            if let Some(tools) = config.claude_allowed_tools.clone() {
                claude.allowed_tools = tools;
            }
            if let Some(effort) = config.claude_effort.as_deref() {
                claude.effort = crate::project::normalize_claude_effort(Some(effort));
            }
        }
    }
}

pub fn write_log_dir_config(log_dir: &Path, config: &SessionAgentConfig) -> Result<(), String> {
    if config.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(log_dir).map_err(|e| format!("create session dir: {e}"))?;
    let json =
        serde_json::to_string_pretty(config).map_err(|e| format!("serialize config: {e}"))?;
    crate::file_watcher::atomic_write(&log_dir.join(SESSION_AGENT_CONFIG_FILE), json.as_bytes())
        .map_err(|e| format!("write session config: {e}"))
}

pub fn read_log_dir_config(log_dir: &Path) -> Option<SessionAgentConfig> {
    let raw = std::fs::read_to_string(log_dir.join(SESSION_AGENT_CONFIG_FILE)).ok()?;
    let config: SessionAgentConfig = serde_json::from_str(&raw).ok()?;
    let mut config = normalize_session_agent_config(config, None);
    if config.project_root.is_none() {
        config.project_root = read_log_dir_project_root(log_dir);
    }
    (!config.is_empty()).then_some(config)
}

fn read_log_dir_project_root(log_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(log_dir.join("session_meta.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    normalize_project_root(value.get("project_root").and_then(|v| v.as_str()))
}

fn normalize_session_agent_config(
    mut config: SessionAgentConfig,
    default_source: Option<&str>,
) -> SessionAgentConfig {
    if config.source.is_none() {
        config.source = default_source
            .map(crate::session_names::normalize_source)
            .filter(|source| !source.is_empty());
    }
    if let Some(source) = config.source.take() {
        config.source = Some(crate::session_names::normalize_source(&source));
    }
    if let Some(root) = config.project_root.take() {
        config.project_root = normalize_project_root(Some(&root));
    }
    if let Some(command) = config.agent_command.take() {
        config.agent_command = sanitize_agent_command_for_source(
            config.source.as_deref(),
            normalize_agent_command(Some(&command)),
        );
    }
    if let Some(model) = config.codex_model.take() {
        config.codex_model = normalize_codex_model(Some(&model));
    }
    if let Some(effort) = config.codex_reasoning_effort.take() {
        config.codex_reasoning_effort = normalize_codex_reasoning_effort(Some(&effort));
    }
    if let Some(mode) = config.codex_sandbox.take() {
        config.codex_sandbox = normalize_codex_sandbox(Some(&mode));
    }
    if let Some(policy) = config.codex_approval_policy.take() {
        config.codex_approval_policy = normalize_codex_approval_policy(Some(&policy));
    }
    if let Some(mode) = config.codex_managed_context.take() {
        config.codex_managed_context = normalize_codex_managed_context(Some(&mode));
    }
    if let Some(mode) = config.codex_context_archive.take() {
        config.codex_context_archive = normalize_codex_context_archive(Some(&mode));
    }
    if let Some(tier) = config.codex_service_tier.take() {
        config.codex_service_tier = normalize_codex_service_tier(Some(&tier));
    }
    if let Some(home) = config.codex_home.take() {
        config.codex_home = normalize_codex_home(Some(&home));
    }
    if let Some(model) = config.claude_model.take() {
        config.claude_model = normalize_claude_model(Some(&model));
    }
    if let Some(mode) = config.claude_permission_mode.take() {
        config.claude_permission_mode = normalize_claude_permission_mode(Some(&mode));
    }
    if let Some(tools) = config.claude_allowed_tools.take() {
        // Keep Some(vec![]) — that's the explicit "all tools" pin.
        config.claude_allowed_tools = Some(
            tools
                .into_iter()
                .map(|rule| rule.trim().to_string())
                .filter(|rule| !rule.is_empty())
                .collect(),
        );
    }
    if let Some(effort) = config.claude_effort.take() {
        config.claude_effort = normalize_claude_effort(Some(&effort));
    }
    config
}

pub fn write_external_overlay(
    home: &Path,
    source: &str,
    session_id: &str,
    config: &SessionAgentConfig,
) -> Result<(), String> {
    write_external_overlay_inner(home, source, session_id, config, true)
}

pub fn replace_external_overlay(
    home: &Path,
    source: &str,
    session_id: &str,
    config: &SessionAgentConfig,
) -> Result<(), String> {
    write_external_overlay_inner(home, source, session_id, config, false)
}

fn write_external_overlay_inner(
    home: &Path,
    source: &str,
    session_id: &str,
    config: &SessionAgentConfig,
    merge_existing: bool,
) -> Result<(), String> {
    let source = crate::session_names::normalize_source(source);
    let session_id = session_id.trim();
    if source == "intendant" || session_id.is_empty() || config.is_empty() {
        return Ok(());
    }

    // Serialize the read-modify-write across intendant processes that share this
    // single global overlay file, so concurrent writers don't lose each other's
    // entries (atomic_write alone prevents torn files, not lost updates).
    with_overlay_lock(home, || {
        let path = overlay_path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create overlay dir: {e}"))?;
        }
        let mut root = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                Ok(value) => value,
                Err(err) => {
                    // Don't silently reset a corrupt overlay — that would discard
                    // every other session's config. Preserve it for forensics and warn.
                    let backup = path.with_extension("corrupt");
                    let _ = std::fs::rename(&path, &backup);
                    eprintln!(
                        "[session_config] agent-config overlay {} was corrupt ({err}); moved to {} and started fresh",
                        path.display(),
                        backup.display()
                    );
                    Value::Object(Map::new())
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
            Err(err) => return Err(format!("read overlay: {err}")),
        };
        if !root.is_object() {
            root = Value::Object(Map::new());
        }
        let root_obj = root.as_object_mut().expect("root is object");
        let source_value = root_obj
            .entry(source.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !source_value.is_object() {
            *source_value = Value::Object(Map::new());
        }
        let source_entries = source_value.as_object_mut().expect("source is object");
        let mut merged = normalize_session_agent_config(config.clone(), Some(&source));
        if merge_existing {
            if let Some(existing) = source_entries
                .get(session_id)
                .and_then(|value| serde_json::from_value::<SessionAgentConfig>(value.clone()).ok())
            {
                merged.merge_missing_from(normalize_session_agent_config(existing, Some(&source)));
            }
        }
        source_entries.insert(
            session_id.to_string(),
            serde_json::to_value(&merged).map_err(|e| format!("serialize config: {e}"))?,
        );
        let json =
            serde_json::to_string_pretty(&root).map_err(|e| format!("serialize overlay: {e}"))?;
        // Atomic write so a concurrent reader never sees a torn file and collapses
        // every other session's managed-context flag to the default.
        crate::file_watcher::atomic_write(&path, json.as_bytes())
            .map_err(|e| format!("write overlay: {e}"))
    })
}

/// Run `write` while holding a best-effort cross-process advisory lock on the
/// shared overlay file, so concurrent intendant processes serialize their
/// read-modify-write. The lock is a pure-safe `O_CREAT|O_EXCL` lock file with a
/// stale-lock timeout (so a crashed holder can't wedge other writers); if it
/// can't be acquired within the bound, the write proceeds unlocked rather than
/// blocking forever.
fn with_overlay_lock<T>(
    home: &Path,
    write: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    use std::time::{Duration, Instant};
    const STALE_AFTER: Duration = Duration::from_secs(5);
    const GIVE_UP_AFTER: Duration = Duration::from_secs(15);
    const POLL: Duration = Duration::from_millis(25);

    let lock_path = overlay_path(home).with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let start = Instant::now();
    let mut acquired = false;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => {
                acquired = true;
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&lock_path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .map(|age| age > STALE_AFTER)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                if start.elapsed() > GIVE_UP_AFTER {
                    break; // proceed unlocked rather than block forever
                }
                std::thread::sleep(POLL);
            }
            Err(_) => break, // cannot create a lock file (perms, etc.) — proceed unlocked
        }
    }
    let result = write();
    if acquired {
        let _ = std::fs::remove_file(&lock_path);
    }
    result
}

pub fn lookup_external_overlay(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<SessionAgentConfig> {
    let source = crate::session_names::normalize_source(source);
    let session_id = session_id.trim();
    if source == "intendant" || session_id.is_empty() {
        return None;
    }
    read_overlay_map(home)
        .get(&source)
        .and_then(|by_id| by_id.get(session_id))
        .cloned()
        // Re-normalize on read: entries written before the cross-agent
        // command guard existed may carry another backend's binary; they
        // must not feed a resume while they wait to be rewritten.
        .map(|config| normalize_session_agent_config(config, Some(&source)))
}

pub fn load_for_resume(
    home: &Path,
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
) -> Option<SessionAgentConfig> {
    let source = crate::session_names::normalize_source(source);
    let ids = [
        resume_id.map(str::trim).filter(|id| !id.is_empty()),
        Some(session_id.trim()).filter(|id| !id.is_empty()),
    ];

    let mut found = SessionAgentConfig::default();
    for id in ids.into_iter().flatten() {
        if let Some(config) = lookup_external_overlay(home, &source, id) {
            found.merge_missing_from(config);
        }
    }
    if let Some(config) =
        find_wrapper_config_for_external_session(home, &source, session_id, resume_id)
    {
        found.merge_missing_from(config);
    }
    if !found.is_empty() {
        return Some(found);
    }
    None
}

pub fn apply_config_to_session_json(session: &mut Value, config: &SessionAgentConfig) {
    let Some(obj) = session.as_object_mut() else {
        return;
    };
    if let Some(source) = config.source.as_deref() {
        obj.entry("configured_source".to_string())
            .or_insert_with(|| Value::String(source.to_string()));
    }
    if let Some(root) = config.project_root.as_deref() {
        let should_insert = obj
            .get("project_root")
            .and_then(|value| value.as_str())
            .map(str::is_empty)
            .unwrap_or(true);
        if should_insert {
            obj.insert("project_root".to_string(), Value::String(root.to_string()));
        }
    }
    if let Some(command) = config.agent_command.as_deref() {
        obj.insert(
            "agent_command".to_string(),
            Value::String(command.to_string()),
        );
        if config.source.as_deref() == Some("codex") {
            obj.insert(
                "codex_command".to_string(),
                Value::String(command.to_string()),
            );
        }
    }
    if let Some(mode) = config.codex_managed_context.as_deref() {
        obj.insert(
            "codex_managed_context".to_string(),
            Value::String(crate::project::normalize_codex_managed_context(mode)),
        );
    }
    if let Some(mode) = config.codex_sandbox.as_deref() {
        obj.insert(
            "codex_sandbox".to_string(),
            Value::String(crate::project::normalize_sandbox_mode(mode)),
        );
    }
    if let Some(policy) = config.codex_approval_policy.as_deref() {
        obj.insert(
            "codex_approval_policy".to_string(),
            Value::String(crate::project::normalize_approval_policy(policy)),
        );
    }
    if let Some(mode) = config.codex_context_archive.as_deref() {
        obj.insert(
            "codex_context_archive".to_string(),
            Value::String(crate::project::normalize_codex_context_archive(mode)),
        );
    }
    if let Some(home) = config.codex_home.as_deref() {
        obj.insert("codex_home".to_string(), Value::String(home.to_string()));
    }
    if let Some(model) = config.codex_model.as_deref() {
        obj.insert("codex_model".to_string(), Value::String(model.to_string()));
    }
    if let Some(effort) = config.codex_reasoning_effort.as_deref() {
        obj.insert(
            "codex_reasoning_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
    if let Some(model) = config.claude_model.as_deref() {
        obj.insert("claude_model".to_string(), Value::String(model.to_string()));
    }
    if let Some(mode) = config.claude_permission_mode.as_deref() {
        obj.insert(
            "claude_permission_mode".to_string(),
            Value::String(mode.to_string()),
        );
    }
    if let Some(tools) = config.claude_allowed_tools.as_ref() {
        obj.insert(
            "claude_allowed_tools".to_string(),
            Value::Array(
                tools
                    .iter()
                    .map(|rule| Value::String(rule.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(effort) = config.claude_effort.as_deref() {
        obj.insert(
            "claude_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
}

pub fn apply_overlays_to_sessions(home: &Path, sessions: &mut [Value]) {
    let overlays = read_overlay_map(home);
    if overlays.is_empty() {
        return;
    }
    for session in sessions {
        let source = session
            .get("source")
            .and_then(|v| v.as_str())
            .map(crate::session_names::normalize_source)
            .unwrap_or_default();
        if source == "intendant" || source.is_empty() {
            continue;
        }
        for key in ["session_id", "resume_id", "backend_session_id"] {
            let Some(session_id) = session.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(config) = overlays
                .get(&source)
                .and_then(|by_id| by_id.get(session_id))
            else {
                continue;
            };
            apply_config_to_session_json(session, config);
            break;
        }
    }
}

fn overlay_path(home: &Path) -> PathBuf {
    crate::platform::intendant_home_in(home).join(OVERLAY_FILE)
}

fn read_overlay_map(home: &Path) -> HashMap<String, HashMap<String, SessionAgentConfig>> {
    let path = overlay_path(home);
    // Distinguish "absent" (normal — no overlay yet) from "present but unreadable/
    // corrupt". The latter must not be silently collapsed to empty, since that would
    // revert every external session's managed-context flag to the default with no signal.
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            eprintln!(
                "[session_config] could not read agent-config overlay {}: {err}; sessions keep default managed-context",
                path.display()
            );
            return HashMap::new();
        }
    };
    let value = match serde_json::from_str::<Value>(&raw) {
        Ok(value) => value,
        Err(err) => {
            eprintln!(
                "[session_config] agent-config overlay {} is not valid JSON ({err}); ignoring it",
                path.display()
            );
            return HashMap::new();
        }
    };
    let Some(obj) = value.as_object() else {
        eprintln!(
            "[session_config] agent-config overlay {} is not a JSON object; ignoring it",
            path.display()
        );
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (source, entries) in obj {
        let source = crate::session_names::normalize_source(source);
        let Some(entries) = entries.as_object() else {
            continue;
        };
        let mut by_id = HashMap::new();
        for (session_id, value) in entries {
            let Ok(config) = serde_json::from_value::<SessionAgentConfig>(value.clone()) else {
                continue;
            };
            let config = normalize_session_agent_config(config, Some(&source));
            if !config.is_empty() {
                by_id.insert(session_id.clone(), config);
            }
        }
        if !by_id.is_empty() {
            out.insert(source, by_id);
        }
    }
    out
}

fn find_wrapper_config_for_external_session(
    home: &Path,
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
) -> Option<SessionAgentConfig> {
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let ids: Vec<String> = [Some(session_id), resume_id]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .collect();
    if ids.is_empty() {
        return None;
    }

    let entries = std::fs::read_dir(logs_dir).ok()?;
    // Newest first: a resumed session is almost always among the most recent
    // stores, and the per-directory identity check below can end up reading
    // session logs — in readdir order a hit near the end pays that cost
    // across the whole store. The sort key is the wrapper CONFIG file's
    // mtime, not the directory's: unrelated bookkeeping rewrites bump a
    // store dir's mtime, while the config file only changes when the launch
    // config itself is (re)written. Directories without a config carry
    // `None` and sort after every configured store — even one restored with
    // a pre-epoch mtime — but stay in the scan: the loop below skips them
    // exactly as before, preserving exhaustive-until-exact-match semantics.
    let mut dirs: Vec<(Option<std::time::SystemTime>, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            if !dir.is_dir() {
                return None;
            }
            let modified = std::fs::metadata(dir.join(SESSION_AGENT_CONFIG_FILE))
                .and_then(|meta| meta.modified())
                .ok();
            Some((modified, dir))
        })
        .collect();
    resume_scan_order(&mut dirs);
    for (_, dir) in dirs {
        let Some(mut config) = read_log_dir_config(&dir) else {
            continue;
        };
        let config_source = config
            .source
            .as_deref()
            .map(crate::session_names::normalize_source)
            .unwrap_or_default();
        if config_source != source {
            continue;
        }
        if wrapper_config_matches_external_session(&dir, source, &ids) {
            if config.source.is_none() {
                config.source = Some(source.to_string());
            }
            return Some(config);
        }
    }
    None
}

/// Ordering for the resume scan's candidate stores: configured stores
/// newest-first by their config file's mtime; configless stores (`None`)
/// after every configured one — `None` orders below `Some(_)`, so under the
/// descending `Reverse` sort it lands last even against a config restored
/// with a pre-epoch mtime. The sort is stable, so equal keys keep readdir
/// order.
fn resume_scan_order(dirs: &mut [(Option<std::time::SystemTime>, PathBuf)]) {
    dirs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
}

/// A wrapper config belongs to an external thread only when persisted
/// identity names it. Never infer identity from an arbitrary substring in
/// `session.jsonl`: prompts, tool output, and project paths routinely contain
/// other session ids (and can otherwise lend one thread another's launch
/// config).
fn wrapper_config_matches_external_session(dir: &Path, source: &str, ids: &[String]) -> bool {
    let dir_id = dir.file_name().and_then(|name| name.to_str());
    let canonical_id = crate::session_identity::canonical_session_id_from_meta(dir);
    if ids
        .iter()
        .any(|id| dir_id == Some(id.as_str()) || canonical_id.as_deref() == Some(id.as_str()))
    {
        return true;
    }

    for id in ids {
        let Some(scan) = crate::session_identity::scan_session_dir(dir, id) else {
            continue;
        };
        if scan.identities.iter().any(|identity| {
            identity.source == source
                && (identity.backend_session_id == *id
                    || identity.wrapper_id.as_deref() == Some(id.as_str()))
        }) {
            return true;
        }
        // Pre-structured-event logs recorded the backend id in one frozen,
        // exact message grammar. The shared reader parses only that grammar;
        // ordinary user/tool text cannot become identity evidence.
        if scan.count == 0 && scan.legacy_resume_id.as_deref() == Some(id.as_str()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the resume-scan ordering invariant exactly: configured stores
    /// newest-first, and a configless store sorts after EVERY configured
    /// one — including a config restored with a pre-epoch mtime, which the
    /// old epoch-sentinel key would have (wrongly) ordered behind it.
    #[test]
    fn resume_scan_orders_configured_newest_first_then_configless() {
        use std::time::{Duration, UNIX_EPOCH};
        let recent = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let older = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let pre_epoch = UNIX_EPOCH - Duration::from_secs(86_400);
        let mut dirs = vec![
            (None, PathBuf::from("configless")),
            (Some(pre_epoch), PathBuf::from("restored-pre-epoch")),
            (Some(recent), PathBuf::from("recent")),
            (Some(older), PathBuf::from("older")),
        ];
        resume_scan_order(&mut dirs);
        let order: Vec<&str> = dirs.iter().map(|(_, dir)| dir.to_str().unwrap()).collect();
        assert_eq!(
            order,
            ["recent", "older", "restored-pre-epoch", "configless"]
        );
    }

    /// The cross-agent command guard: another backend's canonical binary
    /// conflicts, everything else (custom wrappers, paths that merely
    /// contain a backend name, unknown sources) passes.
    #[test]
    fn agent_command_conflict_detection() {
        assert_eq!(
            agent_command_conflicts_with_source("claude-code", "codex"),
            Some("codex")
        );
        assert_eq!(
            agent_command_conflicts_with_source("claude-code", "/usr/local/bin/codex"),
            Some("codex")
        );
        assert_eq!(
            agent_command_conflicts_with_source("claude-code", "Codex.exe"),
            Some("codex")
        );
        assert_eq!(
            agent_command_conflicts_with_source("claude-code", "codex --dangerously"),
            Some("codex")
        );
        assert_eq!(
            agent_command_conflicts_with_source("codex", "claude"),
            Some("claude-code")
        );
        assert_eq!(
            agent_command_conflicts_with_source("codex", r"C:\tools\claude.CMD"),
            Some("claude-code")
        );
        // Own binary, custom wrappers, and paths that merely mention a
        // backend never conflict.
        assert_eq!(
            agent_command_conflicts_with_source("claude-code", "claude"),
            None
        );
        assert_eq!(
            agent_command_conflicts_with_source("claude-code", "/opt/claude-nightly/claude"),
            None
        );
        assert_eq!(agent_command_conflicts_with_source("codex", "codex"), None);
        assert_eq!(
            agent_command_conflicts_with_source("claude-code", "my-claude-wrapper"),
            None
        );
        assert_eq!(
            agent_command_conflicts_with_source("codex", "/home/codex/bin/run-agent"),
            None
        );
        // Unknown sources are never judged.
        assert_eq!(
            agent_command_conflicts_with_source("intendant", "codex"),
            None
        );
        assert_eq!(agent_command_conflicts_with_source("", "codex"), None);
    }

    /// Every config funnel drops a cross-agent command: the wire parse, the
    /// per-dir file read (already-poisoned stores heal on read), and the
    /// overlay lookup.
    #[test]
    fn cross_agent_command_is_dropped_at_every_funnel() {
        // Wire parse.
        let cfg = from_wire(
            Some("claude-code"),
            Some("codex"),
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(cfg.source.as_deref(), Some("claude-code"));
        assert_eq!(cfg.agent_command, None);

        // Dir read of a poisoned file (the 2026-07-16 incident's shape).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SESSION_AGENT_CONFIG_FILE),
            serde_json::json!({
                "source": "claude-code",
                "agent_command": "codex",
                "claude_model": "fable",
            })
            .to_string(),
        )
        .unwrap();
        let read = read_log_dir_config(dir.path()).expect("config still parses");
        assert_eq!(read.agent_command, None);
        assert_eq!(read.claude_model.as_deref(), Some("fable"));

        // Overlay lookup of a poisoned entry.
        let home = tempfile::tempdir().unwrap();
        let overlay = overlay_path(home.path());
        std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        std::fs::write(
            &overlay,
            serde_json::json!({
                "claude-code": {
                    "0caf4660-7345-4f3b-b8e7-407e59aefa5d": {
                        "source": "claude-code",
                        "agent_command": "codex",
                        "claude_permission_mode": "bypassPermissions",
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let looked_up = lookup_external_overlay(
            home.path(),
            "claude-code",
            "0caf4660-7345-4f3b-b8e7-407e59aefa5d",
        )
        .expect("entry resolves");
        assert_eq!(looked_up.agent_command, None);
        assert_eq!(
            looked_up.claude_permission_mode.as_deref(),
            Some("bypassPermissions")
        );

        // A rewrite through the overlay writer persists the healed shape.
        write_external_overlay(
            home.path(),
            "claude-code",
            "0caf4660-7345-4f3b-b8e7-407e59aefa5d",
            &looked_up,
        )
        .unwrap();
        let raw = std::fs::read_to_string(&overlay).unwrap();
        let disk: Value = serde_json::from_str(&raw).unwrap();
        assert!(disk["claude-code"]["0caf4660-7345-4f3b-b8e7-407e59aefa5d"]
            .get("agent_command")
            .is_none());
    }

    /// A legitimate custom binary survives every funnel untouched.
    #[test]
    fn custom_agent_command_survives_normalization() {
        let cfg = from_wire(
            Some("claude-code"),
            Some("/opt/claude-nightly/claude"),
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            cfg.agent_command.as_deref(),
            Some("/opt/claude-nightly/claude")
        );
    }

    #[test]
    fn normalizes_codex_wire_config() {
        let cfg = from_wire(
            Some("Codex"),
            Some("  /tmp/codex  "),
            Some("danger-full-access"),
            Some("on-request"),
            Some("true"),
            Some("raw"),
            Some(" priority "),
        );
        assert_eq!(cfg.source.as_deref(), Some("codex"));
        assert_eq!(cfg.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(cfg.codex_sandbox.as_deref(), Some("danger-full-access"));
        assert_eq!(cfg.codex_approval_policy.as_deref(), Some("on-request"));
        assert_eq!(cfg.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(cfg.codex_context_archive.as_deref(), Some("exact"));
        assert_eq!(cfg.codex_service_tier.as_deref(), Some("priority"));

        let normal_cfg = from_wire(Some("codex"), None, None, None, None, None, Some("normal"));
        assert_eq!(
            normal_cfg.codex_service_tier.as_deref(),
            Some(crate::project::CODEX_STANDARD_SERVICE_TIER)
        );
    }

    #[test]
    fn forked_from_round_trips_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = from_wire(
            Some("claude-code"),
            Some("claude"),
            None,
            None,
            None,
            None,
            None,
        );
        cfg.forked_from = Some("parent-uuid".into());
        assert!(!cfg.is_empty());
        write_log_dir_config(dir.path(), &cfg).unwrap();
        let read = read_log_dir_config(dir.path()).expect("config round-trips");
        assert_eq!(read.forked_from.as_deref(), Some("parent-uuid"));

        // Rehydration on resume merges the persisted lineage in.
        let mut fresh = from_wire(Some("claude-code"), None, None, None, None, None, None);
        fresh.merge_missing_from(read);
        assert_eq!(fresh.forked_from.as_deref(), Some("parent-uuid"));
    }

    #[test]
    fn overlay_round_trips_external_config() {
        let home = tempfile::tempdir().unwrap();
        let mut cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("summary"),
            Some("priority"),
        );
        cfg.codex_home = Some("/home/user/.codex-managed".to_string());
        write_external_overlay(home.path(), "codex", "thread-1", &cfg).unwrap();
        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn partial_overlay_write_preserves_existing_launch_sandbox() {
        let home = tempfile::tempdir().unwrap();
        let full = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("vanilla"),
            Some("summary"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &full).unwrap();

        let partial = from_wire(Some("codex"), None, None, None, Some("managed"), None, None);
        write_external_overlay(home.path(), "codex", "thread-1", &partial).unwrap();

        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(loaded.codex_sandbox.as_deref(), Some("danger-full-access"));
        assert_eq!(loaded.codex_approval_policy.as_deref(), Some("never"));
        assert_eq!(loaded.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(loaded.codex_context_archive.as_deref(), Some("summary"));
    }

    /// "inherit" (and empty / default / global) must clear a per-session
    /// managed-context / context-archive override rather than being
    /// normalized to an explicit "vanilla" / "summary" pin. The sentinel
    /// check has to run before the project-level normalizer, which maps
    /// every unrecognized string to the default.
    #[test]
    fn from_wire_inherit_clears_managed_context_and_archive() {
        for sentinel in ["inherit", "default", "global", "", "  "] {
            let cfg = from_wire(
                Some("codex"),
                None,
                None,
                None,
                Some(sentinel),
                Some(sentinel),
                None,
            );
            assert_eq!(
                cfg.codex_managed_context, None,
                "managed_context {sentinel:?} should clear, not pin"
            );
            assert_eq!(
                cfg.codex_context_archive, None,
                "context_archive {sentinel:?} should clear, not pin"
            );
        }
        // Explicit values still pin.
        let cfg = from_wire(
            Some("codex"),
            None,
            None,
            None,
            Some("managed"),
            Some("off"),
            None,
        );
        assert_eq!(cfg.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(cfg.codex_context_archive.as_deref(), Some("off"));
    }

    /// A persisted overlay that somehow stored the "inherit" sentinel must
    /// read back as "no pin" instead of an explicit vanilla/summary pin.
    #[test]
    fn stored_inherit_sentinel_reads_back_as_unpinned() {
        let raw = serde_json::json!({
            "source": "codex",
            "agent_command": "/tmp/codex",
            "codex_managed_context": "inherit",
            "codex_context_archive": "inherit",
        });
        let config: SessionAgentConfig = serde_json::from_value(raw).unwrap();
        let normalized = normalize_session_agent_config(config, Some("codex"));
        assert_eq!(normalized.codex_managed_context, None);
        assert_eq!(normalized.codex_context_archive, None);
        assert_eq!(normalized.agent_command.as_deref(), Some("/tmp/codex"));
    }

    /// Replacing the overlay with an inherit-derived config un-pins the
    /// managed-context / context-archive fields while keeping the rest.
    #[test]
    fn replace_overlay_inherit_unpins_managed_context_and_archive() {
        let home = tempfile::tempdir().unwrap();
        let full = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("vanilla"),
            Some("exact"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &full).unwrap();

        let inherit = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("inherit"),
            Some("inherit"),
            None,
        );
        replace_external_overlay(home.path(), "codex", "thread-1", &inherit).unwrap();

        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(loaded.codex_sandbox.as_deref(), Some("danger-full-access"));
        assert_eq!(loaded.codex_managed_context, None);
        assert_eq!(loaded.codex_context_archive, None);
    }

    /// An absent managed-context / context-archive on a merge write leaves
    /// the existing pin untouched (only the explicit sentinel clears).
    #[test]
    fn partial_overlay_write_preserves_existing_managed_context_pin() {
        let home = tempfile::tempdir().unwrap();
        let full = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            None,
            None,
            Some("managed"),
            Some("exact"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &full).unwrap();

        let partial = from_wire(
            Some("codex"),
            Some("/tmp/other-codex"),
            None,
            None,
            None,
            None,
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &partial).unwrap();

        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/other-codex"));
        assert_eq!(loaded.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(loaded.codex_context_archive.as_deref(), Some("exact"));
    }

    #[test]
    fn replace_overlay_can_clear_launch_sandbox_override() {
        let home = tempfile::tempdir().unwrap();
        let full = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("vanilla"),
            Some("summary"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &full).unwrap();

        let inherit = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("inherit"),
            Some("inherit"),
            Some("managed"),
            Some("summary"),
            None,
        );
        replace_external_overlay(home.path(), "codex", "thread-1", &inherit).unwrap();

        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(loaded.codex_sandbox, None);
        assert_eq!(loaded.codex_approval_policy, None);
        assert_eq!(loaded.codex_managed_context.as_deref(), Some("managed"));
    }

    #[test]
    fn log_config_round_trips_codex_home_and_applies_to_session_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("exact"),
            Some("priority"),
        );
        cfg.project_root = Some("/tmp/intendant-project".to_string());
        cfg.codex_home = Some("  /home/user/.codex-managed  ".to_string());

        write_log_dir_config(dir.path(), &cfg).unwrap();
        let loaded = read_log_dir_config(dir.path()).unwrap();
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/tmp/intendant-project")
        );
        assert_eq!(
            loaded.codex_home.as_deref(),
            Some("/home/user/.codex-managed")
        );

        let mut session = serde_json::json!({"source": "codex", "session_id": "thread-1"});
        apply_config_to_session_json(&mut session, &loaded);
        assert_eq!(
            session.get("codex_home").and_then(|v| v.as_str()),
            Some("/home/user/.codex-managed")
        );
        assert_eq!(
            session.get("project_root").and_then(|v| v.as_str()),
            Some("/tmp/intendant-project")
        );
        assert_eq!(
            session.get("codex_sandbox").and_then(|v| v.as_str()),
            Some("danger-full-access")
        );
        assert_eq!(
            session
                .get("codex_approval_policy")
                .and_then(|v| v.as_str()),
            Some("never")
        );
    }

    #[test]
    fn log_config_uses_session_meta_project_root_for_legacy_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("exact"),
            None,
        );
        write_log_dir_config(dir.path(), &cfg).unwrap();
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-id",
                "created_at": "2026-06-07T00:00:00Z",
                "project_root": "  /home/user/projects/intendant-station-mainline-123e28c  "
            })
            .to_string(),
        )
        .unwrap();

        let loaded = read_log_dir_config(dir.path()).unwrap();
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/home/user/projects/intendant-station-mainline-123e28c")
        );
    }

    #[test]
    fn resume_prefers_backend_overlay_over_stale_wrapper_overlay() {
        let home = tempfile::tempdir().unwrap();
        let mut stale_wrapper = from_wire(
            Some("codex"),
            Some("/tmp/stale-wrapper-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        stale_wrapper.codex_home = Some("/home/user/.codex-wrapper".to_string());
        let mut backend = from_wire(
            Some("codex"),
            Some("/tmp/backend-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        backend.codex_home = Some("/home/user/.codex-managed".to_string());
        backend.project_root =
            Some("/home/user/projects/intendant-station-mainline-123e28c".into());
        write_external_overlay(home.path(), "codex", "wrapper-id", &stale_wrapper).unwrap();
        write_external_overlay(home.path(), "codex", "backend-thread", &backend).unwrap();

        let loaded =
            load_for_resume(home.path(), "codex", "wrapper-id", Some("backend-thread")).unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/backend-codex"));
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/home/user/projects/intendant-station-mainline-123e28c")
        );
        assert_eq!(
            loaded.codex_home.as_deref(),
            Some("/home/user/.codex-managed")
        );
    }

    #[test]
    fn resume_merges_codex_home_from_wrapper_when_backend_overlay_lacks_it() {
        let home = tempfile::tempdir().unwrap();
        let mut wrapper = from_wire(
            Some("codex"),
            Some("/tmp/wrapper-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        wrapper.codex_home = Some("/home/user/.codex-managed".to_string());
        let wrapper_log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-id");
        write_log_dir_config(&wrapper_log_dir, &wrapper).unwrap();
        std::fs::write(
            wrapper_log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-id",
                "created_at": "2026-06-07T00:00:00Z",
                "project_root": "/home/user/projects/intendant-station-mainline-123e28c"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            wrapper_log_dir.join("session.jsonl"),
            "debug: External agent thread: backend-thread\n",
        )
        .unwrap();
        let backend = from_wire(
            Some("codex"),
            Some("/tmp/backend-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        write_external_overlay(home.path(), "codex", "backend-thread", &backend).unwrap();

        let loaded =
            load_for_resume(home.path(), "codex", "wrapper-id", Some("backend-thread")).unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/backend-codex"));
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/home/user/projects/intendant-station-mainline-123e28c")
        );
        assert_eq!(
            loaded.codex_home.as_deref(),
            Some("/home/user/.codex-managed")
        );
    }

    #[test]
    fn resume_does_not_borrow_config_from_arbitrary_session_id_mentions() {
        let home = tempfile::tempdir().unwrap();
        let unrelated_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("unrelated-wrapper");
        let unrelated = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            claude_model: Some("haiku"),
            ..Default::default()
        });
        write_log_dir_config(&unrelated_dir, &unrelated).unwrap();
        std::fs::write(
            unrelated_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "unrelated-wrapper",
                "project_root": "/tmp/unrelated"
            })
            .to_string(),
        )
        .unwrap();
        let target_id = "07ca095f-c8aa-4d95-af53-c5f67cad6c3a";
        std::fs::write(
            unrelated_dir.join("session.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({
                    "event": "session_identity",
                    "data": {
                        "session_id": "unrelated-wrapper",
                        "source": "claude-code",
                        "backend_session_id": "unrelated-backend"
                    }
                }),
                serde_json::json!({
                    "event": "info",
                    "message": format!("write /tmp/{target_id}/scratchpad/file.txt")
                }),
            ),
        )
        .unwrap();

        assert!(load_for_resume(home.path(), "claude-code", target_id, Some(target_id)).is_none());
    }

    #[test]
    fn resume_finds_wrapper_config_from_structured_identity() {
        let home = tempfile::tempdir().unwrap();
        let wrapper_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-id");
        let config = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            claude_model: Some("fable"),
            ..Default::default()
        });
        write_log_dir_config(&wrapper_dir, &config).unwrap();
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            serde_json::json!({
                "event": "session_identity",
                "data": {
                    "session_id": "wrapper-id",
                    "source": "claude-code",
                    "backend_session_id": "backend-thread"
                }
            })
            .to_string(),
        )
        .unwrap();

        let loaded = load_for_resume(
            home.path(),
            "claude-code",
            "wrapper-id",
            Some("backend-thread"),
        )
        .expect("structured identity should find the wrapper config");
        assert_eq!(loaded.claude_model.as_deref(), Some("fable"));
    }

    #[test]
    fn claude_wire_fields_normalize_and_gate_on_source() {
        let cfg = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            agent_command: Some(" claude "),
            claude_model: Some("  opus  "),
            claude_permission_mode: Some("acceptEdits"),
            claude_allowed_tools: Some("Read, Edit, Bash(cargo test *)"),
            claude_effort: Some(" XHIGH "),
            ..Default::default()
        });
        assert_eq!(cfg.source.as_deref(), Some("claude-code"));
        assert_eq!(cfg.agent_command.as_deref(), Some("claude"));
        assert_eq!(cfg.claude_model.as_deref(), Some("opus"));
        assert_eq!(cfg.claude_permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(
            cfg.claude_allowed_tools.as_deref(),
            Some(
                &[
                    "Read".to_string(),
                    "Edit".into(),
                    "Bash(cargo test *)".into()
                ][..]
            ),
            "comma-split must preserve spaces inside a rule"
        );
        assert_eq!(cfg.claude_effort.as_deref(), Some("xhigh"));

        // Codex sessions never absorb claude fields (and vice versa).
        let cross = from_wire_fields(WireSessionAgentFields {
            source: Some("codex"),
            claude_model: Some("opus"),
            claude_effort: Some("max"),
            ..Default::default()
        });
        assert!(cross.claude_model.is_none());
        assert!(cross.claude_effort.is_none());
    }

    #[test]
    fn codex_reasoning_effort_round_trips_applies_and_gates_on_source() {
        let cfg = from_wire_fields(WireSessionAgentFields {
            source: Some("codex"),
            codex_model: Some("gpt-5.6-sol"),
            codex_reasoning_effort: Some(" ultra "),
            ..Default::default()
        });
        assert_eq!(cfg.codex_reasoning_effort.as_deref(), Some("ultra"));

        let dir = tempfile::tempdir().unwrap();
        write_log_dir_config(dir.path(), &cfg).unwrap();
        let loaded = read_log_dir_config(dir.path()).expect("reasoning pin round-trips");
        assert_eq!(loaded.codex_reasoning_effort.as_deref(), Some("ultra"));
        let mut merged = SessionAgentConfig {
            source: Some("codex".to_string()),
            ..Default::default()
        };
        merged.merge_missing_from(loaded.clone());
        assert_eq!(merged.codex_reasoning_effort.as_deref(), Some("ultra"));

        std::fs::write(dir.path().join("intendant.toml"), "").unwrap();
        let mut project = Project::from_root(dir.path().to_path_buf()).unwrap();
        apply_to_project(&mut project, &AgentBackend::Codex, &loaded);
        assert_eq!(
            project.config.agent.codex.reasoning_effort.as_deref(),
            Some("ultra")
        );

        let mut session = serde_json::json!({"source": "codex", "session_id": "sess-1"});
        apply_config_to_session_json(&mut session, &loaded);
        assert_eq!(
            session
                .get("codex_reasoning_effort")
                .and_then(|value| value.as_str()),
            Some("ultra")
        );

        let cross = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            codex_reasoning_effort: Some("max"),
            ..Default::default()
        });
        assert!(cross.codex_reasoning_effort.is_none());
    }

    #[test]
    fn claude_inherit_sentinels_clear_but_default_mode_pins() {
        for sentinel in ["inherit", "global", "", "  "] {
            let cfg = from_wire_fields(WireSessionAgentFields {
                source: Some("claude-code"),
                claude_model: Some(sentinel),
                claude_permission_mode: Some(sentinel),
                claude_allowed_tools: Some(sentinel),
                claude_effort: Some(sentinel),
                ..Default::default()
            });
            assert!(
                cfg.claude_model.is_none(),
                "model {sentinel:?} should clear"
            );
            assert!(
                cfg.claude_permission_mode.is_none(),
                "mode {sentinel:?} should clear"
            );
            assert!(
                cfg.claude_allowed_tools.is_none(),
                "tools {sentinel:?} should clear"
            );
            assert!(
                cfg.claude_effort.is_none(),
                "effort {sentinel:?} should clear"
            );
        }
        // "default" is a REAL permission mode and must pin, while it clears
        // the other three fields (never a valid model/tool/effort value).
        let cfg = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            claude_model: Some("default"),
            claude_permission_mode: Some("default"),
            claude_allowed_tools: Some("default"),
            claude_effort: Some("default"),
            ..Default::default()
        });
        assert!(cfg.claude_model.is_none());
        assert_eq!(cfg.claude_permission_mode.as_deref(), Some("default"));
        assert!(cfg.claude_allowed_tools.is_none());
        assert!(cfg.claude_effort.is_none());
        // "all" pins the explicitly-unrestricted empty list.
        let cfg = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            claude_allowed_tools: Some("all"),
            ..Default::default()
        });
        assert_eq!(cfg.claude_allowed_tools.as_deref(), Some(&[][..]));
    }

    #[test]
    fn claude_overlay_round_trips_and_partial_write_preserves_pins() {
        let home = tempfile::tempdir().unwrap();
        let full = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            agent_command: Some("/tmp/claude"),
            claude_model: Some("sonnet"),
            claude_permission_mode: Some("plan"),
            claude_allowed_tools: Some("all"),
            claude_effort: Some("low"),
            ..Default::default()
        });
        write_external_overlay(home.path(), "claude-code", "sess-1", &full).unwrap();
        let loaded = lookup_external_overlay(home.path(), "claude-code", "sess-1").unwrap();
        assert_eq!(loaded, full);
        assert_eq!(
            loaded.claude_allowed_tools.as_deref(),
            Some(&[][..]),
            "the explicit all-tools pin survives the JSON round-trip"
        );

        // A later partial write (only the model) keeps the other pins.
        let partial = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            claude_model: Some("haiku"),
            ..Default::default()
        });
        write_external_overlay(home.path(), "claude-code", "sess-1", &partial).unwrap();
        let loaded = lookup_external_overlay(home.path(), "claude-code", "sess-1").unwrap();
        assert_eq!(loaded.claude_model.as_deref(), Some("haiku"));
        assert_eq!(loaded.claude_permission_mode.as_deref(), Some("plan"));
        assert_eq!(loaded.claude_effort.as_deref(), Some("low"));
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/claude"));

        // A replace with inherit-derived values un-pins.
        let inherit = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            agent_command: Some("/tmp/claude"),
            claude_model: Some("inherit"),
            claude_permission_mode: Some("inherit"),
            claude_allowed_tools: Some("inherit"),
            claude_effort: Some("inherit"),
            ..Default::default()
        });
        replace_external_overlay(home.path(), "claude-code", "sess-1", &inherit).unwrap();
        let loaded = lookup_external_overlay(home.path(), "claude-code", "sess-1").unwrap();
        assert!(loaded.claude_model.is_none());
        assert!(loaded.claude_permission_mode.is_none());
        assert!(loaded.claude_allowed_tools.is_none());
        assert!(loaded.claude_effort.is_none());
    }

    #[test]
    fn claude_config_applies_to_project_and_session_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("intendant.toml"), "").unwrap();
        let mut project = Project::from_root(dir.path().to_path_buf()).unwrap();
        project.config.agent.claude_code.allowed_tools = vec!["Read".to_string()];

        let cfg = from_wire_fields(WireSessionAgentFields {
            source: Some("claude-code"),
            claude_model: Some("opus"),
            claude_permission_mode: Some("acceptEdits"),
            claude_allowed_tools: Some("all"),
            claude_effort: Some("max"),
            ..Default::default()
        });
        apply_to_project(&mut project, &AgentBackend::ClaudeCode, &cfg);
        let claude = &project.config.agent.claude_code;
        assert_eq!(claude.model.as_deref(), Some("opus"));
        assert_eq!(claude.permission_mode, "acceptEdits");
        assert!(
            claude.allowed_tools.is_empty(),
            "the all-tools pin overrides a restrictive global list"
        );
        assert_eq!(claude.effort.as_deref(), Some("max"));

        let mut session = serde_json::json!({"source": "claude-code", "session_id": "sess-1"});
        apply_config_to_session_json(&mut session, &cfg);
        assert_eq!(
            session.get("claude_model").and_then(|v| v.as_str()),
            Some("opus")
        );
        assert_eq!(
            session
                .get("claude_permission_mode")
                .and_then(|v| v.as_str()),
            Some("acceptEdits")
        );
        assert_eq!(
            session
                .get("claude_allowed_tools")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(
            session.get("claude_effort").and_then(|v| v.as_str()),
            Some("max")
        );
    }

    #[test]
    fn corrupt_overlay_is_preserved_and_overwritten_fresh() {
        let home = tempfile::tempdir().unwrap();
        let path = overlay_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        // Writing must not panic or silently wipe the file; it preserves the corrupt
        // copy and starts fresh so the new entry is still readable.
        let cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            None,
            None,
            Some("managed"),
            Some("off"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &cfg).unwrap();

        assert!(path.with_extension("corrupt").exists());
        assert_eq!(
            lookup_external_overlay(home.path(), "codex", "thread-1").unwrap(),
            cfg
        );
        // The lock file is released after the write.
        assert!(!path.with_extension("lock").exists());
    }
}
