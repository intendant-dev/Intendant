use crate::error::CallerError;
use crate::provider::TokenUsage;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SubAgentRole {
    Research,
    Implementation,
    Testing,
    Orchestrator,
    LiveAudio,
    Custom(String),
}

impl SubAgentRole {
    pub fn as_str(&self) -> &str {
        match self {
            SubAgentRole::Research => "research",
            SubAgentRole::Implementation => "implementation",
            SubAgentRole::Testing => "testing",
            SubAgentRole::Orchestrator => "orchestrator",
            SubAgentRole::LiveAudio => "live_audio",
            SubAgentRole::Custom(s) => s,
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "research" => SubAgentRole::Research,
            "implementation" => SubAgentRole::Implementation,
            "testing" => SubAgentRole::Testing,
            "orchestrator" => SubAgentRole::Orchestrator,
            "live_audio" => SubAgentRole::LiveAudio,
            other => SubAgentRole::Custom(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SubAgentStatus {
    Completed,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    pub id: String,
    pub status: SubAgentStatus,
    pub summary: String,
    pub brief: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<PathBuf>,
    pub usage: TokenUsage,
}

pub fn format_result_message(result: &SubAgentResult) -> String {
    let status_str = match &result.status {
        SubAgentStatus::Completed => "completed".to_string(),
        SubAgentStatus::Failed(reason) => format!("failed: {}", reason),
    };

    let mut msg = format!(
        "[Sub-Agent Result: {}]\nStatus: {}\nSummary: {}",
        result.id, status_str, result.summary
    );

    if !result.findings.is_empty() {
        msg.push_str("\nFindings:");
        for finding in &result.findings {
            msg.push_str(&format!("\n  - {}", finding));
        }
    }

    if !result.artifacts.is_empty() {
        msg.push_str("\nArtifacts:");
        for artifact in &result.artifacts {
            msg.push_str(&format!("\n  - {}", artifact.display()));
        }
    }

    msg.push_str(&format!(
        "\nTokens used: prompt={} completion={} total={}",
        result.usage.prompt_tokens, result.usage.completion_tokens, result.usage.total_tokens
    ));

    msg
}

/// PARKED: disk form for orchestrator project state checkpoints.
///
/// The live checkpoint path uses the knowledge store (`store_memory` on the
/// `project_state` channel). These disk helpers remain unwired.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectState {
    pub completed_tasks: Vec<String>,
    pub active_tasks: Vec<String>,
    pub constraints: Vec<String>,
    pub decisions: Vec<String>,
    pub updated_at: String,
}

/// Write the parked disk project-state checkpoint to the given directory.
#[allow(dead_code)]
pub fn write_project_state(dir: &Path, state: &ProjectState) -> Result<(), CallerError> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(dir)?;

    // Write JSON (machine-readable)
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| CallerError::SubAgent(format!("Failed to serialize project state: {}", e)))?;
    std::fs::write(dir.join("project_state.json"), &json)?;

    // Write Markdown (human-readable)
    let mut md = String::new();
    md.push_str("# Project State Checkpoint\n\n");
    md.push_str(&format!("Updated: {}\n\n", state.updated_at));
    if !state.completed_tasks.is_empty() {
        md.push_str("## Completed Tasks\n");
        for task in &state.completed_tasks {
            md.push_str(&format!("- {}\n", task));
        }
        md.push('\n');
    }
    if !state.active_tasks.is_empty() {
        md.push_str("## Active Tasks\n");
        for task in &state.active_tasks {
            md.push_str(&format!("- {}\n", task));
        }
        md.push('\n');
    }
    if !state.decisions.is_empty() {
        md.push_str("## Decisions\n");
        for d in &state.decisions {
            md.push_str(&format!("- {}\n", d));
        }
        md.push('\n');
    }
    if !state.constraints.is_empty() {
        md.push_str("## Constraints\n");
        for c in &state.constraints {
            md.push_str(&format!("- {}\n", c));
        }
        md.push('\n');
    }
    std::fs::write(dir.join("project_state.md"), &md)?;

    Ok(())
}

/// Read the parked disk project-state checkpoint from the given directory.
#[allow(dead_code)]
pub fn read_project_state(dir: &Path) -> Result<ProjectState, CallerError> {
    let path = dir.join("project_state.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| CallerError::SubAgent(format!("Failed to read project state: {}", e)))?;
    serde_json::from_str(&content)
        .map_err(|e| CallerError::SubAgent(format!("Failed to parse project state: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_agent_role_roundtrip() {
        assert_eq!(SubAgentRole::from_str("research"), SubAgentRole::Research);
        assert_eq!(
            SubAgentRole::from_str("implementation"),
            SubAgentRole::Implementation
        );
        assert_eq!(SubAgentRole::from_str("testing"), SubAgentRole::Testing);
        assert_eq!(
            SubAgentRole::from_str("orchestrator"),
            SubAgentRole::Orchestrator
        );
        assert_eq!(
            SubAgentRole::from_str("custom_role"),
            SubAgentRole::Custom("custom_role".to_string())
        );

        assert_eq!(SubAgentRole::Research.as_str(), "research");
        assert_eq!(SubAgentRole::Implementation.as_str(), "implementation");
        assert_eq!(SubAgentRole::Orchestrator.as_str(), "orchestrator");
    }

    #[test]
    fn format_result_message_completed() {
        let result = SubAgentResult {
            id: "research-1".to_string(),
            status: SubAgentStatus::Completed,
            summary: "Found the database schema".to_string(),
            brief: "Found the database schema with 3 tables.".to_string(),
            findings: vec![
                "3 tables found".to_string(),
                "No migrations pending".to_string(),
            ],
            artifacts: vec![PathBuf::from("/tmp/schema.sql")],
            usage: TokenUsage {
                prompt_tokens: 1000,
                completion_tokens: 500,
                total_tokens: 1500,
                ..Default::default()
            },
        };
        let msg = format_result_message(&result);
        assert!(msg.contains("[Sub-Agent Result: research-1]"));
        assert!(msg.contains("Status: completed"));
        assert!(msg.contains("Found the database schema"));
        assert!(msg.contains("3 tables found"));
        assert!(msg.contains("schema.sql"));
        assert!(msg.contains("1500"));
    }

    #[test]
    fn format_result_message_failed() {
        let result = SubAgentResult {
            id: "impl-1".to_string(),
            status: SubAgentStatus::Failed("Compilation error".to_string()),
            summary: "Could not compile the module".to_string(),
            brief: "Compilation failed.".to_string(),
            findings: vec![],
            artifacts: vec![],
            usage: TokenUsage::default(),
        };
        let msg = format_result_message(&result);
        assert!(msg.contains("failed: Compilation error"));
        assert!(!msg.contains("Findings:"));
        assert!(!msg.contains("Artifacts:"));
    }

    #[test]
    fn format_result_message_no_findings() {
        let result = SubAgentResult {
            id: "test-1".to_string(),
            status: SubAgentStatus::Completed,
            summary: "All tests passed".to_string(),
            brief: "All tests passed.".to_string(),
            findings: vec![],
            artifacts: vec![],
            usage: TokenUsage::default(),
        };
        let msg = format_result_message(&result);
        assert!(!msg.contains("Findings:"));
    }

    #[test]
    fn sub_agent_result_serialization() {
        let result = SubAgentResult {
            id: "test-1".to_string(),
            status: SubAgentStatus::Failed("timeout".to_string()),
            summary: "Timed out".to_string(),
            brief: "Task timed out.".to_string(),
            findings: vec!["partial result".to_string()],
            artifacts: vec![],
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SubAgentResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, SubAgentStatus::Failed("timeout".to_string()));
    }

    #[test]
    fn project_state_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state = ProjectState {
            completed_tasks: vec!["research database".to_string()],
            active_tasks: vec!["implement auth".to_string()],
            constraints: vec!["Python 3.9+".to_string()],
            decisions: vec!["Use PostgreSQL".to_string()],
            updated_at: "2025-01-15T12:00:00".to_string(),
        };
        write_project_state(dir.path(), &state).unwrap();

        let loaded = read_project_state(dir.path()).unwrap();
        assert_eq!(loaded.completed_tasks, state.completed_tasks);
        assert_eq!(loaded.active_tasks, state.active_tasks);
        assert_eq!(loaded.constraints, state.constraints);
        assert_eq!(loaded.decisions, state.decisions);
        assert_eq!(loaded.updated_at, state.updated_at);

        // Verify markdown file also exists
        assert!(dir.path().join("project_state.md").exists());
    }

    #[test]
    fn project_state_default() {
        let state = ProjectState::default();
        assert!(state.completed_tasks.is_empty());
        assert!(state.active_tasks.is_empty());
        assert!(state.constraints.is_empty());
        assert!(state.decisions.is_empty());
    }

    #[test]
    fn custom_role_roundtrip() {
        // "presence" is no longer a built-in role; it round-trips as Custom
        assert_eq!(
            SubAgentRole::from_str("presence"),
            SubAgentRole::Custom("presence".into())
        );
    }
}
