use serde::{Deserialize, Serialize};

/// Configuration for the presence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    // --- Text mode (TUI / MCP) ---
    /// Provider name for text mode (e.g. "gemini", "anthropic", "openai").
    /// Default: auto-detect (prefers gemini when GEMINI_API_KEY is set).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model for text mode. Default: "gemini-2.5-flash".
    #[serde(default)]
    pub model: Option<String>,
    /// Context window size for the text-mode presence conversation.
    #[serde(default = "default_context_window")]
    pub context_window: u64,

    // --- Live mode (browser-side voice/realtime) ---
    /// Provider for the browser-side live model (e.g. "gemini", "openai").
    #[serde(default)]
    pub live_provider: Option<String>,
    /// Model name for live mode.
    #[serde(default)]
    pub live_model: Option<String>,
    /// Context window for the live model.
    #[serde(default = "default_live_context_window")]
    pub live_context_window: u64,
}

fn default_true() -> bool {
    true
}

fn default_context_window() -> u64 {
    1_048_576
}

fn default_live_context_window() -> u64 {
    32_768
}

/// Default text presence model.
pub const DEFAULT_TEXT_MODEL: &str = "gemini-2.5-flash";
/// Preferred text presence model (Gemini 3 Flash, when available).
#[allow(dead_code)]
pub const PREFERRED_TEXT_MODEL: &str = "gemini-3-flash-preview";
/// Default text presence provider.
#[allow(dead_code)]
pub const DEFAULT_TEXT_PROVIDER: &str = "gemini";

impl Default for PresenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            model: None,
            context_window: default_context_window(),
            live_provider: None,
            live_model: None,
            live_context_window: default_live_context_window(),
        }
    }
}

/// A structured task submission from presence to the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEnvelope {
    pub task: String,
    #[serde(default)]
    pub force_direct: bool,
    #[serde(default)]
    pub context_hints: Vec<String>,
}

/// Filtered events pushed to the presence layer from the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PresenceEvent {
    PhaseChanged { phase: String },
    TaskComplete { reason: String },
    ApprovalNeeded { id: u64, preview: String, category: String },
    HumanQuestion { question: String },
    BudgetWarning { pct: f64, remaining: u64 },
    RoundComplete { round: usize, turns_in_round: usize },
    Error { message: String },
}

/// Token usage snapshot from the presence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceUsage {
    pub total_tokens: u64,
    pub context_window: u64,
    pub usage_pct: f64,
    pub provider: String,
    pub model: String,
}

/// Queryable snapshot of the agent's current state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStateSnapshot {
    pub phase: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub last_output_summary: String,
    pub last_command_preview: String,
    pub active_workers: Vec<String>,
    /// Pending approval details (set when phase is "waiting_approval").
    /// Cleared when the approval is resolved (agent starts running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApprovalSnapshot>,
}

/// Serializable snapshot of a pending approval for the live model bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalSnapshot {
    pub id: u64,
    pub command_preview: String,
    pub category: String,
}

impl AgentStateSnapshot {
    /// Update state from a server-sent event (OutboundEvent JSON).
    /// Returns an optional `PresenceEvent` if this event is worth narrating.
    pub fn update_from_server_event(&mut self, event: &serde_json::Value) -> Option<PresenceEvent> {
        let event_type = event.get("event")?.as_str()?;
        match event_type {
            "turn_started" => {
                if let Some(t) = event["turn"].as_u64() {
                    self.turn = t as usize;
                }
                if let Some(b) = event["budget_pct"].as_f64() {
                    self.budget_pct = b;
                }
                self.phase = "thinking".to_string();
                Some(PresenceEvent::PhaseChanged {
                    phase: "thinking".to_string(),
                })
            }
            "status" => {
                if let Some(p) = event["phase"].as_str() {
                    self.phase = p.to_string();
                    Some(PresenceEvent::PhaseChanged {
                        phase: p.to_string(),
                    })
                } else {
                    None
                }
            }
            "agent_output" => {
                let stdout = event["stdout"].as_str().unwrap_or("");
                self.last_output_summary = crate::truncate(stdout, 500);
                None // agent_output is not narrated by default
            }
            "approval_required" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let command = event["command"].as_str().unwrap_or("").to_string();
                let category = event["category"].as_str().unwrap_or("").to_string();
                self.phase = "waiting_approval".to_string();
                self.pending_approval = Some(PendingApprovalSnapshot {
                    id,
                    command_preview: command.clone(),
                    category: category.clone(),
                });
                Some(PresenceEvent::ApprovalNeeded {
                    id,
                    preview: command,
                    category,
                })
            }
            "ask_human" => {
                let question = event["question"].as_str().unwrap_or("").to_string();
                self.phase = "waiting_human".to_string();
                Some(PresenceEvent::HumanQuestion { question })
            }
            "task_complete" => {
                let reason = event["reason"].as_str().unwrap_or("done").to_string();
                self.phase = "idle".to_string();
                self.pending_approval = None;
                Some(PresenceEvent::TaskComplete { reason })
            }
            "round_complete" => {
                self.phase = "idle".to_string();
                self.pending_approval = None;
                let round = event["round"].as_u64().unwrap_or(0) as usize;
                let turns = event["turns_in_round"].as_u64().unwrap_or(0) as usize;
                Some(PresenceEvent::RoundComplete {
                    round,
                    turns_in_round: turns,
                })
            }
            "error" => {
                let message = event["message"].as_str().unwrap_or("unknown error").to_string();
                Some(PresenceEvent::Error { message })
            }
            _ => None,
        }
    }

    /// Update state when agent starts running (clears pending approval).
    pub fn on_agent_started(&mut self, commands_preview: &str) {
        self.phase = "running_agent".to_string();
        self.last_command_preview = commands_preview.to_string();
        self.pending_approval = None;
    }
}

/// Minimum interval between phase-change narrations (in milliseconds).
/// Phase events arriving faster than this are skipped.
pub const NARRATION_DEBOUNCE_MS: u64 = 500;

/// Presence turn offset to avoid collisions with agent turns in TUI collapse logic.
pub const PRESENCE_TURN_OFFSET: usize = 100_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_config_defaults() {
        let config = PresenceConfig::default();
        assert!(config.enabled);
        assert!(config.provider.is_none());
        assert!(config.model.is_none());
        assert_eq!(config.context_window, 1_048_576);
        assert!(config.live_provider.is_none());
        assert!(config.live_model.is_none());
        assert_eq!(config.live_context_window, 32_768);
    }

    #[test]
    fn presence_config_deserialize_json() {
        let json_str = r#"{
            "enabled": false,
            "provider": "anthropic",
            "model": "claude-sonnet-4-5-20250929",
            "context_window": 200000
        }"#;
        let config: PresenceConfig = serde_json::from_str(json_str).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.provider.as_deref(), Some("anthropic"));
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-5-20250929"));
        assert_eq!(config.context_window, 200000);
    }

    #[test]
    fn task_envelope_roundtrip() {
        let envelope = TaskEnvelope {
            task: "fix the bug".to_string(),
            force_direct: true,
            context_hints: vec!["src/main.rs".to_string()],
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: TaskEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.task, "fix the bug");
        assert!(back.force_direct);
        assert_eq!(back.context_hints.len(), 1);
    }

    #[test]
    fn presence_event_serialize_roundtrip() {
        let event = PresenceEvent::ApprovalNeeded {
            id: 42,
            preview: "exec: rm -rf /tmp".to_string(),
            category: "Destructive".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: PresenceEvent = serde_json::from_str(&json).unwrap();
        match back {
            PresenceEvent::ApprovalNeeded { id, preview, category } => {
                assert_eq!(id, 42);
                assert_eq!(preview, "exec: rm -rf /tmp");
                assert_eq!(category, "Destructive");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_state_snapshot_defaults() {
        let s = AgentStateSnapshot::default();
        assert!(s.phase.is_empty());
        assert_eq!(s.turn, 0);
        assert_eq!(s.budget_pct, 0.0);
        assert!(s.last_output_summary.is_empty());
        assert!(s.last_command_preview.is_empty());
        assert!(s.active_workers.is_empty());
        assert!(s.pending_approval.is_none());
    }

    #[test]
    fn agent_state_snapshot_with_pending_approval() {
        let s = AgentStateSnapshot {
            phase: "waiting_approval".to_string(),
            pending_approval: Some(PendingApprovalSnapshot {
                id: 1,
                command_preview: "exec: ls -la /tmp".to_string(),
                category: "CommandExec".to_string(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("pending_approval"));
        assert!(json.contains("exec: ls -la /tmp"));
        let back: AgentStateSnapshot = serde_json::from_str(&json).unwrap();
        assert!(back.pending_approval.is_some());
        let pa = back.pending_approval.unwrap();
        assert_eq!(pa.id, 1);
        assert_eq!(pa.command_preview, "exec: ls -la /tmp");
    }

    #[test]
    fn agent_state_snapshot_without_approval_omits_field() {
        let s = AgentStateSnapshot::default();
        let json = serde_json::to_string(&s).unwrap();
        // skip_serializing_if = "Option::is_none" should omit the field
        assert!(!json.contains("pending_approval"));
    }

    #[test]
    fn update_from_server_event_turn_started() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({"event": "turn_started", "turn": 5, "budget_pct": 0.3});
        let narration = s.update_from_server_event(&event);
        assert_eq!(s.turn, 5);
        assert!((s.budget_pct - 0.3).abs() < f64::EPSILON);
        assert_eq!(s.phase, "thinking");
        assert!(narration.is_some());
    }

    #[test]
    fn update_from_server_event_approval_required() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({
            "event": "approval_required",
            "id": 42,
            "command": "rm -rf /tmp",
            "category": "Destructive"
        });
        let narration = s.update_from_server_event(&event);
        assert_eq!(s.phase, "waiting_approval");
        assert!(s.pending_approval.is_some());
        let pa = s.pending_approval.as_ref().unwrap();
        assert_eq!(pa.id, 42);
        assert_eq!(pa.command_preview, "rm -rf /tmp");
        assert!(narration.is_some());
    }

    #[test]
    fn update_from_server_event_task_complete_clears_approval() {
        let mut s = AgentStateSnapshot {
            phase: "waiting_approval".to_string(),
            pending_approval: Some(PendingApprovalSnapshot {
                id: 1,
                command_preview: "test".to_string(),
                category: "exec".to_string(),
            }),
            ..Default::default()
        };
        let event = serde_json::json!({"event": "task_complete", "reason": "all done"});
        let narration = s.update_from_server_event(&event);
        assert_eq!(s.phase, "idle");
        assert!(s.pending_approval.is_none());
        assert!(narration.is_some());
    }

    #[test]
    fn update_from_server_event_agent_output_not_narrated() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({"event": "agent_output", "stdout": "hello world"});
        let narration = s.update_from_server_event(&event);
        assert!(narration.is_none());
        assert_eq!(s.last_output_summary, "hello world");
    }

    #[test]
    fn update_from_server_event_unknown_ignored() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({"event": "usage_update", "tokens": 1000});
        let narration = s.update_from_server_event(&event);
        assert!(narration.is_none());
    }

    #[test]
    fn on_agent_started_clears_approval() {
        let mut s = AgentStateSnapshot {
            pending_approval: Some(PendingApprovalSnapshot {
                id: 1,
                command_preview: "test".to_string(),
                category: "exec".to_string(),
            }),
            ..Default::default()
        };
        s.on_agent_started("cargo test");
        assert_eq!(s.phase, "running_agent");
        assert_eq!(s.last_command_preview, "cargo test");
        assert!(s.pending_approval.is_none());
    }
}
