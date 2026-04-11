//! Centralized control plane for shared state updates.
//!
//! Subscribes to the EventBus and handles ControlMsg events that update
//! shared state (autonomy level, external agent backend, etc.). This ensures
//! state is updated regardless of which frontend (TUI, web, MCP) is active.
//! Frontends remain display-only — they render state changes but never write
//! to shared state from ControlMsg handlers.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::autonomy::SharedAutonomy;
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::external_agent;

pub struct ControlPlaneState {
    pub autonomy: SharedAutonomy,
    pub external_agent: Arc<RwLock<Option<external_agent::AgentBackend>>>,
    pub bus: EventBus,
}

/// Spawn the control plane as a background task. Returns a JoinHandle.
pub fn spawn(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    state: ControlPlaneState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(AppEvent::ControlCommand(msg)) => {
                    handle_control_msg(&msg, &state).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {} // Other events, lagged -- ignore
            }
        }
    })
}

async fn handle_control_msg(msg: &ControlMsg, state: &ControlPlaneState) {
    match msg {
        ControlMsg::SetAutonomy { level } => {
            use crate::autonomy::AutonomyLevel;
            let new_level = AutonomyLevel::from_str_loose(level);
            let mut guard = state.autonomy.write().await;
            guard.level = new_level;
        }
        ControlMsg::SetExternalAgent { agent } => {
            let parsed = agent
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(external_agent::AgentBackend::from_str_loose);
            *state.external_agent.write().await = parsed.clone();
            // Broadcast so frontends can update their status bars.
            state.bus.send(AppEvent::ExternalAgentChanged {
                agent: parsed.map(|b| b.to_string()),
            });
        }
        _ => {} // Other control messages don't update shared state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{AutonomyLevel, AutonomyState};
    use crate::event::EventBus;

    #[tokio::test]
    async fn set_autonomy_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                bus: bus.clone(),
            },
        );

        // Verify initial state
        assert_eq!(autonomy.read().await.level, AutonomyLevel::Medium);

        // Send SetAutonomy
        bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: "high".to_string(),
        }));

        // Give the spawned task time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(autonomy.read().await.level, AutonomyLevel::High);

        handle.abort();
    }

    #[tokio::test]
    async fn set_external_agent_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                bus: bus.clone(),
            },
        );

        // Verify initial state
        assert!(external_agent.read().await.is_none());

        // Send SetExternalAgent with a value
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: Some("codex".to_string()),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            *external_agent.read().await,
            Some(external_agent::AgentBackend::Codex)
        );

        // Send SetExternalAgent with None to clear
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: None,
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(external_agent.read().await.is_none());

        handle.abort();
    }

    #[tokio::test]
    async fn set_autonomy_invalid_level_ignored() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                bus: bus.clone(),
            },
        );

        // AutonomyLevel::from_str_loose returns Medium for unknown strings
        bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: "unknown_level".to_string(),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // from_str_loose defaults to Medium for unknown strings
        assert_eq!(autonomy.read().await.level, AutonomyLevel::Medium);

        handle.abort();
    }

    #[tokio::test]
    async fn set_external_agent_empty_string_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(Some(external_agent::AgentBackend::Codex)));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                bus: bus.clone(),
            },
        );

        // Send SetExternalAgent with empty string -- should clear
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: Some(String::new()),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(external_agent.read().await.is_none());

        handle.abort();
    }
}
