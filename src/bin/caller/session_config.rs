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
    pub agent_command: Option<String>,
    #[serde(
        default,
        alias = "codex_context_recovery",
        skip_serializing_if = "Option::is_none"
    )]
    pub codex_managed_context: Option<String>,
}

impl SessionAgentConfig {
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.agent_command.is_none()
            && self.codex_managed_context.is_none()
    }

    pub fn merge_missing_from(&mut self, fallback: SessionAgentConfig) {
        if self.source.is_none() {
            self.source = fallback.source;
        }
        if self.agent_command.is_none() {
            self.agent_command = fallback.agent_command;
        }
        if self.codex_managed_context.is_none() {
            self.codex_managed_context = fallback.codex_managed_context;
        }
    }
}

pub fn normalize_agent_command(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn from_wire(
    source: Option<&str>,
    agent_command: Option<&str>,
    codex_managed_context: Option<&str>,
) -> SessionAgentConfig {
    let source = source
        .map(crate::session_names::normalize_source)
        .filter(|value| !value.is_empty());
    let codex_managed_context = match source.as_deref() {
        Some("codex") => codex_managed_context.map(crate::project::normalize_codex_managed_context),
        _ => None,
    };
    SessionAgentConfig {
        source,
        agent_command: normalize_agent_command(agent_command),
        codex_managed_context,
    }
}

pub fn from_project(backend: &AgentBackend, project: &Project) -> SessionAgentConfig {
    match backend {
        AgentBackend::Codex => SessionAgentConfig {
            source: Some("codex".to_string()),
            agent_command: Some(project.config.agent.codex.command.clone()),
            codex_managed_context: Some(crate::project::normalize_codex_managed_context(
                &project.config.agent.codex.managed_context,
            )),
        },
        AgentBackend::ClaudeCode => SessionAgentConfig {
            source: Some("claude-code".to_string()),
            agent_command: Some(project.config.agent.claude_code.command.clone()),
            codex_managed_context: None,
        },
        AgentBackend::GeminiCli => SessionAgentConfig {
            source: Some("gemini".to_string()),
            agent_command: Some(project.config.agent.gemini_cli.command.clone()),
            codex_managed_context: None,
        },
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
            if let Some(mode) = config.codex_managed_context.clone() {
                project.config.agent.codex.managed_context =
                    crate::project::normalize_codex_managed_context(&mode);
            }
        }
        AgentBackend::ClaudeCode => {
            if let Some(command) = config.agent_command.clone() {
                project.config.agent.claude_code.command = command;
            }
        }
        AgentBackend::GeminiCli => {
            if let Some(command) = config.agent_command.clone() {
                project.config.agent.gemini_cli.command = command;
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
    std::fs::write(log_dir.join(SESSION_AGENT_CONFIG_FILE), json)
        .map_err(|e| format!("write session config: {e}"))
}

pub fn read_log_dir_config(log_dir: &Path) -> Option<SessionAgentConfig> {
    let raw = std::fs::read_to_string(log_dir.join(SESSION_AGENT_CONFIG_FILE)).ok()?;
    let mut config: SessionAgentConfig = serde_json::from_str(&raw).ok()?;
    if let Some(source) = config.source.take() {
        config.source = Some(crate::session_names::normalize_source(&source));
    }
    if let Some(command) = config.agent_command.take() {
        config.agent_command = normalize_agent_command(Some(&command));
    }
    if let Some(mode) = config.codex_managed_context.take() {
        config.codex_managed_context = Some(crate::project::normalize_codex_managed_context(&mode));
    }
    (!config.is_empty()).then_some(config)
}

pub fn write_external_overlay(
    home: &Path,
    source: &str,
    session_id: &str,
    config: &SessionAgentConfig,
) -> Result<(), String> {
    let source = crate::session_names::normalize_source(source);
    let session_id = session_id.trim();
    if source == "intendant" || session_id.is_empty() || config.is_empty() {
        return Ok(());
    }

    let path = overlay_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create overlay dir: {e}"))?;
    }
    let mut root = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
        }
        Err(_) => Value::Object(Map::new()),
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("root is object");
    let source_value = root_obj
        .entry(source)
        .or_insert_with(|| Value::Object(Map::new()));
    if !source_value.is_object() {
        *source_value = Value::Object(Map::new());
    }
    source_value
        .as_object_mut()
        .expect("source is object")
        .insert(
            session_id.to_string(),
            serde_json::to_value(config).map_err(|e| format!("serialize config: {e}"))?,
        );
    let json =
        serde_json::to_string_pretty(&root).map_err(|e| format!("serialize overlay: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write overlay: {e}"))
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
}

pub fn load_for_resume(
    home: &Path,
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
) -> Option<SessionAgentConfig> {
    let source = crate::session_names::normalize_source(source);
    let ids = [
        Some(session_id.trim()).filter(|id| !id.is_empty()),
        resume_id.map(str::trim).filter(|id| !id.is_empty()),
    ];

    let mut found = SessionAgentConfig::default();
    for id in ids.into_iter().flatten() {
        if let Some(config) = lookup_external_overlay(home, &source, id) {
            found.merge_missing_from(config);
        }
    }
    if !found.is_empty() {
        return Some(found);
    }

    find_wrapper_config_for_external_session(home, &source, session_id, resume_id)
}

pub fn apply_config_to_session_json(session: &mut Value, config: &SessionAgentConfig) {
    let Some(obj) = session.as_object_mut() else {
        return;
    };
    if let Some(source) = config.source.as_deref() {
        obj.entry("configured_source".to_string())
            .or_insert_with(|| Value::String(source.to_string()));
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
    home.join(".intendant").join(OVERLAY_FILE)
}

fn read_overlay_map(home: &Path) -> HashMap<String, HashMap<String, SessionAgentConfig>> {
    let path = overlay_path(home);
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let Some(obj) = value.as_object() else {
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
            let Ok(mut config) = serde_json::from_value::<SessionAgentConfig>(value.clone()) else {
                continue;
            };
            if config.source.is_none() {
                config.source = Some(source.clone());
            }
            if let Some(source) = config.source.take() {
                config.source = Some(crate::session_names::normalize_source(&source));
            }
            if let Some(command) = config.agent_command.take() {
                config.agent_command = normalize_agent_command(Some(&command));
            }
            if let Some(mode) = config.codex_managed_context.take() {
                config.codex_managed_context =
                    Some(crate::project::normalize_codex_managed_context(&mode));
            }
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
    let logs_dir = home.join(".intendant").join("logs");
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
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
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
        let jsonl = dir.join("session.jsonl");
        let Ok(contents) = std::fs::read_to_string(jsonl) else {
            continue;
        };
        let mentions = ids.iter().any(|id| contents.contains(id));
        if mentions {
            if config.source.is_none() {
                config.source = Some(source.to_string());
            }
            return Some(config);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_codex_wire_config() {
        let cfg = from_wire(Some("Codex"), Some("  /tmp/codex  "), Some("true"));
        assert_eq!(cfg.source.as_deref(), Some("codex"));
        assert_eq!(cfg.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(cfg.codex_managed_context.as_deref(), Some("managed"));
    }

    #[test]
    fn overlay_round_trips_external_config() {
        let home = tempfile::tempdir().unwrap();
        let cfg = from_wire(Some("codex"), Some("/tmp/codex"), Some("managed"));
        write_external_overlay(home.path(), "codex", "thread-1", &cfg).unwrap();
        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded, cfg);
    }
}
