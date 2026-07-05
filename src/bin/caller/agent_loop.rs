//! Agent-loop types: budget constants, LoopExitReason/LoopStats,
//! user attachments, and the follow-up message plumbing shared by the
//! native loop and the external-agent drain. run_agent_loop itself
//! stays in main.rs until the internal-agent unification re-homes it.

use crate::conversation;
use crate::external_agent;
use crate::provider;
use crate::{ExternalToolFailureLogLimiter, ExternalToolOutputLimiter};

use std::time::Duration;

pub(crate) const SAFETY_CAP: usize = 500;
pub(crate) const MIN_BUDGET_TOKENS: u64 = 4096;
pub(crate) const BUDGET_WARNING_THRESHOLD: f64 = 0.85;
pub(crate) const EXTERNAL_POST_TURN_DRAIN_GRACE: Duration = Duration::from_millis(750);

/// Why the agent loop exited after a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopExitReason {
    /// Agent sent an explicit done signal.
    DoneSignal,
    /// Task completed (no JSON, no commands, etc.).
    TaskComplete,
    /// Context budget exhausted.
    BudgetExhausted,
    /// Hit the safety cap of 500 turns.
    SafetyCapReached,
    /// User denied a command.
    Denied,
    /// An error occurred.
    #[allow(dead_code)]
    Error,
    /// User requested interruption mid-turn.
    Interrupted,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LoopStats {
    pub(crate) turns: usize,
    pub(crate) rounds: usize,
    pub(crate) terminal_outcome: Option<String>,
    pub(crate) usage: provider::TokenUsage,
    pub(crate) codex_subagent_sessions: std::collections::HashSet<String>,
    pub(crate) codex_subagent_parent_threads: std::collections::HashMap<String, String>,
    pub(crate) codex_subagent_rounds: std::collections::HashMap<String, usize>,
    pub(crate) codex_subagent_terminal_sessions: std::collections::HashSet<String>,
    pub(crate) codex_subagent_transcript_offsets: std::collections::HashMap<String, usize>,
    pub(crate) codex_subagent_tool_output_limiters:
        std::collections::HashMap<String, ExternalToolOutputLimiter>,
    pub(crate) codex_subagent_tool_failure_limiters:
        std::collections::HashMap<String, ExternalToolFailureLogLimiter>,
    /// Last model response content (for sub-agent result summaries).
    pub(crate) last_response: Option<String>,
    /// Native backend session id announced during the drained turn
    /// (`AgentEvent::NativeSessionId`). The CLI external-agent loop takes
    /// this after each drain to rotate its primary address, so targeted
    /// controls (thread actions, steer, stop) sent under the upgraded id
    /// keep matching this conversation.
    pub(crate) announced_native_session_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct UserAttachments {
    pub(crate) items: Vec<external_agent::AgentAttachment>,
}

impl UserAttachments {
    pub(crate) fn from_items(items: Vec<external_agent::AgentAttachment>) -> Self {
        Self { items }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn conversation_images(&self) -> Vec<conversation::ImageData> {
        self.items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::Image(img) => Some(conversation::ImageData {
                    media_type: img.mime_type.clone(),
                    data: img.base64.clone(),
                }),
                external_agent::AgentAttachment::File(_) => None,
            })
            .collect()
    }

    pub(crate) fn text_with_file_prelude(&self, text: &str) -> String {
        let files: Vec<&external_agent::AgentFileAttachment> = self
            .items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::File(file) => Some(file),
                external_agent::AgentAttachment::Image(_) => None,
            })
            .collect();
        let prelude = external_agent::format_file_attachments_prelude(&files);
        if prelude.is_empty() {
            text.to_string()
        } else {
            format!("{}{}", prelude, text)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FollowUpMessage {
    pub(crate) text: String,
    pub(crate) attachments: UserAttachments,
    pub(crate) steer_id: Option<String>,
    pub(crate) follow_up_id: Option<String>,
    pub(crate) edit_user_turn_index: Option<u32>,
    pub(crate) edit_user_turn_revision: Option<u32>,
    pub(crate) edit_original_text: Option<String>,
    pub(crate) unresolved_attachment_ids: Vec<String>,
    pub(crate) target_session_id: Option<String>,
    pub(crate) managed_context_recovery_kickstart: bool,
    pub(crate) managed_context_density_handoff: bool,
    pub(crate) managed_context_density_handoff_completed: bool,
}

impl FollowUpMessage {
    pub(crate) fn text(text: String) -> Self {
        Self {
            text,
            attachments: UserAttachments::default(),
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn with_attachments(text: String, attachments: UserAttachments) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn steer(text: String, attachments: UserAttachments, steer_id: String) -> Self {
        Self {
            text,
            attachments,
            steer_id: Some(steer_id),
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn edit_user_message(
        text: String,
        attachments: UserAttachments,
        user_turn_index: u32,
        user_turn_revision: u32,
        original_text: Option<String>,
        unresolved_attachment_ids: Vec<String>,
    ) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: Some(user_turn_index),
            edit_user_turn_revision: Some(user_turn_revision),
            edit_original_text: original_text,
            unresolved_attachment_ids,
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn for_target(mut self, target_session_id: Option<String>) -> Self {
        self.target_session_id = target_session_id;
        self
    }

    pub(crate) fn with_follow_up_id(mut self, follow_up_id: Option<String>) -> Self {
        self.follow_up_id = follow_up_id;
        self
    }

    pub(crate) fn managed_context_recovery_kickstart(mut self) -> Self {
        self.managed_context_recovery_kickstart = true;
        self
    }

    pub(crate) fn managed_context_density_handoff(mut self) -> Self {
        self.managed_context_density_handoff = true;
        self
    }

    pub(crate) fn after_managed_context_density_handoff(mut self) -> Self {
        self.managed_context_density_handoff = false;
        self.managed_context_density_handoff_completed = true;
        self
    }
}

pub(crate) type FollowUpReceiver = tokio::sync::mpsc::Receiver<FollowUpMessage>;
