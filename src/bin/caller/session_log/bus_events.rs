//! The event-bus-driven logging methods of [`SessionLog`]: typed writers
//! called by the session-log writer task for `AppEvent`s that flow through
//! the bus — session lifecycle/steer/approval/display/recording/usage events,
//! turn artifacts (model responses, agent I/O, context snapshots), and the
//! session summary/interrupt markers.

use super::*;

/// Opt-out for the latest-only context-snapshot rotation: keep every
/// per-turn sidecar (the pre-rotation behavior) for debugging sessions
/// where the full history of exact request payloads matters more than
/// disk. Read once per process, at [`SessionLog::open`] — the resolved
/// policy lives on the log as injected state so tests pin the policy they
/// exercise instead of inheriting the shell's environment.
pub(super) fn context_snapshot_keep_all() -> bool {
    static KEEP_ALL: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("INTENDANT_CONTEXT_SNAPSHOT_KEEP_ALL")
            .map(|v| {
                let v = v.trim();
                !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
            })
            .unwrap_or(false)
    });
    *KEEP_ALL
}

impl SessionLog {
    // ---- Event-bus-driven logging methods ----
    // These are called by spawn_session_log_writer() for events that flow
    // through the AppEvent bus but were not previously persisted to disk.

    /// Log a done signal from the agent.
    pub fn done_signal_for_session(&mut self, session_id: Option<&str>, message: Option<&str>) {
        let data = session_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|session_id| serde_json::json!({ "session_id": session_id }));
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "done_signal".to_string(),
            level: Some("info".to_string()),
            message: Some(message.unwrap_or("Agent signalled done").to_string()),
            data,
            file: None,
            file2: None,
        });
    }

    /// Log task completion.
    pub fn task_complete_for_session(
        &mut self,
        session_id: Option<&str>,
        reason: &str,
        summary: Option<&str>,
    ) {
        let mut data = serde_json::json!({
            "reason": reason,
            "summary": summary,
        });
        if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            data["session_id"] = serde_json::Value::String(session_id.to_string());
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "task_complete".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Task complete: {}", reason)),
            data: Some(data),
            file: None,
            file2: None,
        });
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) fn steer_event(
        &mut self,
        event: &str,
        level: &str,
        session_id: Option<&str>,
        id: &str,
        text: Option<&str>,
        reason: Option<&str>,
        status: &str,
        mid_turn: Option<bool>,
    ) {
        let mut data = serde_json::Map::new();
        if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
        }
        data.insert("id".to_string(), serde_json::Value::String(id.to_string()));
        data.insert(
            "status".to_string(),
            serde_json::Value::String(status.to_string()),
        );
        if let Some(text) = text {
            data.insert(
                "text".to_string(),
                serde_json::Value::String(text.to_string()),
            );
        }
        if let Some(reason) = reason {
            data.insert(
                "reason".to_string(),
                serde_json::Value::String(reason.to_string()),
            );
        }
        if let Some(mid_turn) = mid_turn {
            data.insert("mid_turn".to_string(), serde_json::Value::Bool(mid_turn));
        }

        let message = match event {
            "steer_requested" => {
                format!(
                    "Steer requested: {}",
                    log_preview(text.unwrap_or_default(), 160)
                )
            }
            "steer_queued" => reason
                .map(|reason| format!("Steer queued: {reason}"))
                .unwrap_or_else(|| "Steer queued".to_string()),
            "steer_accepted" => reason
                .map(|reason| format!("Steer accepted: {reason}"))
                .unwrap_or_else(|| "Steer accepted".to_string()),
            "steer_delivered" => {
                let where_ = if mid_turn.unwrap_or(false) {
                    "mid-turn"
                } else {
                    "at turn boundary"
                };
                format!("Steer delivered ({where_})")
            }
            "steer_cancelled" => reason
                .map(|reason| format!("Steer cancelled: {reason}"))
                .unwrap_or_else(|| "Steer cancelled".to_string()),
            "steer_cancel_failed" => reason
                .map(|reason| format!("Steer cancel failed: {reason}"))
                .unwrap_or_else(|| "Steer cancel failed".to_string()),
            _ => format!("Steer {status}"),
        };

        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: event.to_string(),
            level: Some(level.to_string()),
            message: Some(message),
            data: Some(serde_json::Value::Object(data)),
            file: None,
            file2: None,
        });
    }

    pub fn steer_requested(&mut self, session_id: Option<&str>, id: &str, text: &str) {
        self.steer_event(
            "steer_requested",
            "info",
            session_id,
            id,
            Some(text),
            None,
            "pending",
            None,
        );
    }

    pub fn steer_queued(&mut self, session_id: Option<&str>, id: &str, reason: &str) {
        self.steer_event(
            "steer_queued",
            "warn",
            session_id,
            id,
            None,
            Some(reason),
            "queued",
            None,
        );
    }

    pub fn steer_accepted(&mut self, session_id: Option<&str>, id: &str, reason: &str) {
        self.steer_event(
            "steer_accepted",
            "info",
            session_id,
            id,
            None,
            Some(reason),
            "accepted",
            None,
        );
    }

    pub fn steer_delivered(&mut self, session_id: Option<&str>, id: &str, mid_turn: bool) {
        self.steer_event(
            "steer_delivered",
            "info",
            session_id,
            id,
            None,
            None,
            "delivered",
            Some(mid_turn),
        );
    }

    pub fn steer_cancelled(&mut self, session_id: Option<&str>, id: &str, reason: &str) {
        self.steer_event(
            "steer_cancelled",
            "warn",
            session_id,
            id,
            None,
            Some(reason),
            "cancelled",
            None,
        );
    }

    /// A cancel found nothing to clear (`AppEvent::SteerCancelFailed`) —
    /// terminal like `steer_cancelled`, but the message reached or will
    /// reach the model. Structured so replay retires the pending row as a
    /// failed clear instead of resurrecting it (or worse, a fabricated
    /// successful one) on reload.
    pub fn steer_cancel_failed(&mut self, session_id: Option<&str>, id: &str, reason: &str) {
        self.steer_event(
            "steer_cancel_failed",
            "warn",
            session_id,
            id,
            None,
            Some(reason),
            "failed",
            None,
        );
    }

    /// Persist a display-only session note (`AppEvent::SessionNote`).
    ///
    /// The full note text and the attachment *references* live in `data`;
    /// `message` carries a short preview for plain-log readers. Replay
    /// reconstructs the event via `session_log_entry_to_app_event` so a
    /// rehydrated session renders the note exactly like the live path.
    /// Attachment blobs themselves persist in the session upload store.
    pub fn session_note(
        &mut self,
        session_id: Option<&str>,
        note_id: &str,
        text: &str,
        attachments: &[crate::types::SessionNoteAttachment],
        source: Option<&str>,
        ts_ms: u64,
    ) {
        let mut data = serde_json::Map::new();
        if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
        }
        data.insert(
            "note_id".to_string(),
            serde_json::Value::String(note_id.to_string()),
        );
        data.insert(
            "text".to_string(),
            serde_json::Value::String(text.to_string()),
        );
        if let Some(source) = source.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "source".to_string(),
                serde_json::Value::String(source.to_string()),
            );
        }
        data.insert("ts_ms".to_string(), serde_json::Value::from(ts_ms));
        if !attachments.is_empty() {
            data.insert(
                "attachments".to_string(),
                serde_json::to_value(attachments).unwrap_or(serde_json::Value::Null),
            );
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "session_note".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Note: {}", log_preview(text, 160))),
            data: Some(serde_json::Value::Object(data)),
            file: None,
            file2: None,
        });
    }

    /// Persist a fire-and-forget agent→user notification
    /// (`AppEvent::UserNotification`).
    ///
    /// The full text/title/urgency live in `data`; `message` carries a
    /// short preview for plain-log readers. Replay reconstructs the event
    /// via `session_log_entry_to_app_event` so a rehydrated session
    /// renders the notification row exactly like the live path.
    pub fn user_notification(
        &mut self,
        session_id: Option<&str>,
        notification_id: &str,
        title: Option<&str>,
        text: &str,
        urgency: crate::types::NotificationUrgency,
        ts_ms: u64,
    ) {
        let mut data = serde_json::Map::new();
        if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
        }
        data.insert(
            "notification_id".to_string(),
            serde_json::Value::String(notification_id.to_string()),
        );
        if let Some(title) = title.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "title".to_string(),
                serde_json::Value::String(title.to_string()),
            );
        }
        data.insert(
            "text".to_string(),
            serde_json::Value::String(text.to_string()),
        );
        data.insert(
            "urgency".to_string(),
            serde_json::Value::String(urgency.as_str().to_string()),
        );
        data.insert("ts_ms".to_string(), serde_json::Value::from(ts_ms));
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "user_notification".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Notification: {}", log_preview(text, 160))),
            data: Some(serde_json::Value::Object(data)),
            file: None,
            file2: None,
        });
    }

    /// Log a new session starting (MCP multi-task).
    pub fn session_started(&mut self, session_id: &str, task: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_started".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session started: {}", session_id)),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "task": task,
            })),
            file: None,
            file2: None,
        });
    }

    /// Link an Intendant-visible session id to a backend-native id.
    pub fn session_identity(&mut self, session_id: &str, source: &str, backend_session_id: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_identity".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Session identity: {} -> {}:{}",
                session_id, source, backend_session_id
            )),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "source": source,
                "backend_session_id": backend_session_id,
            })),
            file: None,
            file2: None,
        });
        let _ = crate::external_wrapper_index::upsert_from_log_dir(
            source,
            backend_session_id,
            session_id,
            &self.dir,
        );
    }

    /// Log that a frontend-visible session is attached to an external agent.
    pub fn session_attached(&mut self, session_id: &str, source: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_attached".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session attached: {} ({})", session_id, source)),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "source": source,
            })),
            file: None,
            file2: None,
        });
    }

    /// Persist a visible parent/child session relationship.
    pub fn session_relationship(
        &mut self,
        parent_session_id: &str,
        child_session_id: &str,
        relationship: &str,
        ephemeral: bool,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_relationship".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Session relationship: {} -> {} ({})",
                parent_session_id, child_session_id, relationship
            )),
            data: Some(serde_json::json!({
                "parent_session_id": parent_session_id,
                "child_session_id": child_session_id,
                "relationship": relationship,
                "ephemeral": ephemeral,
            })),
            file: None,
            file2: None,
        });
    }

    /// Persist per-session frontend affordances.
    pub fn session_capabilities(
        &mut self,
        session_id: &str,
        capabilities: &crate::types::SessionCapabilities,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_capabilities".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session capabilities: {}", session_id)),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "capabilities": capabilities,
            })),
            file: None,
            file2: None,
        });
    }

    /// Persist the latest visible Codex `/goal` state for a session.
    pub fn session_goal(&mut self, session_id: &str, goal: Option<&crate::types::SessionGoal>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_goal".to_string(),
            level: Some("info".to_string()),
            message: Some(match goal {
                Some(goal) => format!("Session goal: {} ({})", session_id, goal.objective),
                None => format!("Session goal cleared: {}", session_id),
            }),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "goal": goal,
            })),
            file: None,
            file2: None,
        });
    }

    pub fn session_vitals(&mut self, session_id: &str, vitals: &crate::types::SessionVitals) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_vitals".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!("Session vitals: {}", session_id)),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "vitals": vitals,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a session ending (MCP multi-task).
    pub fn session_ended(&mut self, session_id: &str, reason: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_ended".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session ended: {} ({})", session_id, reason)),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "reason": reason,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log agent execution starting.
    pub fn agent_started_with_session_id(
        &mut self,
        session_id: Option<&str>,
        turn: usize,
        commands_preview: &str,
        item_id: Option<&str>,
        source: Option<&str>,
    ) {
        let mut data = serde_json::Map::new();
        if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.to_string()),
            );
        }
        if let Some(item_id) = item_id.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "item_id".to_string(),
                serde_json::Value::String(item_id.to_string()),
            );
        }
        if let Some(source) = source.map(str::trim).filter(|s| !s.is_empty()) {
            data.insert(
                "source".to_string(),
                serde_json::Value::String(source.to_string()),
            );
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(turn),
            event: "agent_started".to_string(),
            level: Some("info".to_string()),
            message: Some(commands_preview.to_string()),
            data: if data.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(data))
            },
            file: None,
            file2: None,
        });
    }

    /// Log an auto-approved command.
    pub fn auto_approved(&mut self, preview: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "auto_approved".to_string(),
            level: Some("info".to_string()),
            message: Some(preview.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a resolved approval decision.
    pub fn approval_resolved(&mut self, id: u64, action: &str) {
        if self
            .last_approval_resolved
            .as_ref()
            .is_some_and(|(last_id, last_action)| *last_id == id && last_action == action)
        {
            return;
        }
        self.last_approval_resolved = Some((id, action.to_string()));
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(id as usize),
            event: "approval_resolved".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Approval {} (turn {})", action, id)),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a human question (askHuman).
    pub fn human_question(&mut self, question: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "human_question".to_string(),
            level: Some("info".to_string()),
            message: Some(question.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log that a human response was sent.
    pub fn human_response_sent(&mut self) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "human_response_sent".to_string(),
            level: Some("info".to_string()),
            message: Some("Human response sent".to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log round completion (orchestrator mode).
    pub fn round_complete(&mut self, round: usize, turns_in_round: usize) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "round_complete".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Round {} complete ({} turns)",
                round, turns_in_round
            )),
            data: Some(serde_json::json!({
                "round": round,
                "turns_in_round": turns_in_round,
            })),
            file: None,
            file2: None,
        });
        update_session_meta_after_round_complete(&self.dir, Some(self.current_turn), Some(round));
    }

    /// Log creation of a per-round file snapshot.
    pub fn snapshot_created(&mut self, round_id: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "snapshot_created".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Snapshot {} created", round_id)),
            data: Some(serde_json::json!({ "round_id": round_id })),
            file: None,
            file2: None,
        });
    }

    /// Log a rollback to a prior round.
    pub fn rolled_back(&mut self, from_id: u64, to_id: u64, files_reverted: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "rolled_back".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Rolled back from round {} to {} ({} files reverted)",
                from_id, to_id, files_reverted
            )),
            data: Some(serde_json::json!({
                "from_id": from_id,
                "to_id": to_id,
                "files_reverted": files_reverted,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a redo along the linear history.
    pub fn redone(&mut self, to_id: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "redone".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Redone to round {}", to_id)),
            data: Some(serde_json::json!({ "to_id": to_id })),
            file: None,
            file2: None,
        });
    }

    /// Log a pruning of abandoned branches + orphaned objects.
    pub fn history_pruned(&mut self, branches_removed: u32, bytes_freed: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "history_pruned".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Pruned {} branch(es), freed {} bytes",
                branches_removed, bytes_freed
            )),
            data: Some(serde_json::json!({
                "branches_removed": branches_removed,
                "bytes_freed": bytes_freed,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a conversation rollback (truncated or session-reset).
    pub fn conversation_rolled_back(
        &mut self,
        round_id: u64,
        turns_removed: u32,
        backend: &str,
        method: &str,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "conversation_rolled_back".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Conversation rolled back to round {} via {} ({} turns removed, backend: {})",
                round_id, method, turns_removed, backend
            )),
            data: Some(serde_json::json!({
                "round_id": round_id,
                "turns_removed": turns_removed,
                "backend": backend,
                "method": method,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log display ready. `agent_visible == false` marks a private user
    /// view (streams to the owner's dashboards only; agents can't see it).
    pub fn display_ready(&mut self, display_id: u32, width: u32, height: u32, agent_visible: bool) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "display_ready".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Display :{} ready ({}x{}){}",
                display_id,
                width,
                height,
                if agent_visible {
                    ""
                } else {
                    " [private user view]"
                }
            )),
            data: Some(serde_json::json!({
                "display_id": display_id,
                "width": width,
                "height": height,
                "agent_visible": agent_visible,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log display resolution change.
    pub fn display_resize(&mut self, display_id: u32, width: u32, height: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "display_resize".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Display :{} resized to {}x{}",
                display_id, width, height
            )),
            data: Some(serde_json::json!({
                "display_id": display_id,
                "width": width,
                "height": height,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log display takeover.
    pub fn display_taken(&mut self, display_id: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "display_taken".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Display :{} taken over", display_id)),
            data: Some(serde_json::json!({ "display_id": display_id })),
            file: None,
            file2: None,
        });
    }

    /// Log display released.
    pub fn display_released(&mut self, display_id: u32, note: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "display_released".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Display :{} released{}",
                display_id,
                note.map(|n| format!(": {}", n)).unwrap_or_default()
            )),
            data: Some(serde_json::json!({
                "display_id": display_id,
                "note": note,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log debug screen ready.
    pub fn debug_screen_ready(&mut self, display_id: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "debug_screen_ready".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Debug screen :{} ready", display_id)),
            data: Some(serde_json::json!({
                "display_id": display_id,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log debug screen torn down.
    pub fn debug_screen_torn_down(&mut self, display_id: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "debug_screen_torn_down".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Debug screen :{} torn down", display_id)),
            data: Some(serde_json::json!({ "display_id": display_id })),
            file: None,
            file2: None,
        });
    }

    /// Log safety cap reached.
    pub fn safety_cap_reached(&mut self) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "safety_cap_reached".to_string(),
            level: Some("warn".to_string()),
            message: Some("Safety cap reached".to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log recording started.
    pub fn recording_started(&mut self, stream_name: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "recording_started".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Recording started: {}", stream_name)),
            data: Some(serde_json::json!({ "stream_name": stream_name })),
            file: None,
            file2: None,
        });
    }

    /// Log recording stopped.
    pub fn recording_stopped(&mut self, stream_name: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "recording_stopped".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Recording stopped: {}", stream_name)),
            data: Some(serde_json::json!({ "stream_name": stream_name })),
            file: None,
            file2: None,
        });
    }

    /// Log recording error.
    pub fn recording_error(&mut self, stream_name: &str, message: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "recording_error".to_string(),
            level: Some("warn".to_string()),
            message: Some(format!("Recording error ({}): {}", stream_name, message)),
            data: Some(serde_json::json!({
                "stream_name": stream_name,
                "error": message,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log recording deleted.
    pub fn recording_deleted(&mut self, stream_name: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "recording_deleted".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Recording deleted: {}", stream_name)),
            data: Some(serde_json::json!({ "stream_name": stream_name })),
            file: None,
            file2: None,
        });
    }

    /// Log sub-agent result.
    pub fn sub_agent_result(&mut self, summary: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "sub_agent_result".to_string(),
            level: Some("info".to_string()),
            message: Some(summary.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log presence layer log message.
    pub fn presence_log(&mut self, message: &str, level: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "presence_log".to_string(),
            level: Some(level.unwrap_or("info").to_string()),
            message: Some(message.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log presence layer usage update.
    pub fn presence_usage_update(
        &mut self,
        provider: &str,
        model: &str,
        total_tokens: u64,
        context_window: u64,
        usage_pct: f64,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "presence_usage_update".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!(
                "Presence usage: {:.0}% ({} tokens, {}:{})",
                usage_pct * 100.0,
                total_tokens,
                provider,
                model
            )),
            data: Some(serde_json::json!({
                "provider": provider,
                "model": model,
                "total_tokens": total_tokens,
                "context_window": context_window,
                "usage_pct": usage_pct,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live model (Gemini Live / OpenAI Realtime) usage update.
    pub fn live_usage_update(&mut self, provider: &str, model: &str, total_tokens: u64) {
        // Track cumulative live model tokens
        if total_tokens > self.summary_builder.total_tokens {
            self.summary_builder.total_tokens = total_tokens;
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "live_usage_update".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!(
                "Live usage: {} tokens ({}:{})",
                total_tokens, provider, model
            )),
            data: Some(serde_json::json!({
                "provider": provider,
                "model": model,
                "total_tokens": total_tokens,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live audio sub-agent started.
    pub fn live_audio_started(&mut self, id: &str, provider: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "live_audio_started".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Live audio started: {} ({})", id, provider)),
            data: Some(serde_json::json!({
                "id": id,
                "provider": provider,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live audio sub-agent progress.
    pub fn live_audio_progress(
        &mut self,
        id: &str,
        state: &str,
        elapsed_secs: f64,
        transcript_preview: &str,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "live_audio_progress".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!(
                "Live audio {}: {} ({:.1}s) {}",
                id, state, elapsed_secs, transcript_preview
            )),
            data: Some(serde_json::json!({
                "id": id,
                "state": state,
                "elapsed_secs": elapsed_secs,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live audio sub-agent completed.
    pub fn live_audio_completed(&mut self, id: &str, status: &str, quarantine_count: usize) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "live_audio_completed".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Live audio completed: {} ({}, {} quarantined)",
                id, status, quarantine_count
            )),
            data: Some(serde_json::json!({
                "id": id,
                "status": status,
                "quarantine_count": quarantine_count,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a tool request received from the browser presence model.
    #[allow(dead_code)]
    pub fn tool_request(&mut self, tool: &str, args: &serde_json::Value) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "tool_request".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!(
                "{}({})",
                tool,
                serde_json::to_string(args).unwrap_or_default()
            )),
            data: Some(serde_json::json!({
                "tool": tool,
                "args": args,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a tool response sent back to the browser presence model.
    #[allow(dead_code)]
    pub fn tool_response(&mut self, tool: &str, result: &str) {
        let preview = if result.len() > 200 {
            truncate_str(result, 200)
        } else {
            result
        };
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "tool_response".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!("{} → {}", tool, preview)),
            data: None,
            file: None,
            file2: None,
        });
    }

    pub fn error(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "error".to_string(),
            level: Some("error".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    pub fn debug(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "debug".to_string(),
            level: Some("debug".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a turn boundary.
    pub fn turn_start(&mut self, turn: usize, budget_pct: f64, remaining: u64) {
        self.current_turn = turn;
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(turn),
            event: "turn_start".to_string(),
            level: Some("info".to_string()),
            message: None,
            data: Some(serde_json::json!({
                "budget_pct": budget_pct,
                "remaining_tokens": remaining,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log the full messages array sent to the API for this turn.
    pub fn messages_input(&mut self, messages_json: &str) {
        let file = self.write_turn_file("messages.json", messages_json);
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "messages_input".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!("Messages logged ({} bytes)", messages_json.len())),
            data: Some(serde_json::json!({
                "json_length": messages_json.len(),
            })),
            file,
            file2: None,
        });
    }

    /// Log a parsed raw model-context snapshot for dashboard inspection.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub fn context_snapshot(
        &mut self,
        source: &str,
        label: &str,
        turn: Option<usize>,
        format: &str,
        token_count: Option<u64>,
        token_count_kind: Option<&str>,
        context_window: Option<u64>,
        hard_context_window: Option<u64>,
        item_count: Option<usize>,
        raw: &serde_json::Value,
    ) {
        self.context_snapshot_for_session(
            None,
            source,
            label,
            None,
            None,
            turn,
            format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            item_count,
            raw,
        );
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub fn context_snapshot_for_session(
        &mut self,
        session_id: Option<&str>,
        source: &str,
        label: &str,
        request_id: Option<&str>,
        request_index: Option<u64>,
        turn: Option<usize>,
        format: &str,
        token_count: Option<u64>,
        token_count_kind: Option<&str>,
        context_window: Option<u64>,
        hard_context_window: Option<u64>,
        item_count: Option<usize>,
        raw: &serde_json::Value,
    ) {
        // Compact, not pretty: consumers parse the sidecar as JSON (replay,
        // the Context tab, rewind preflight); pretty-printing multi-MB
        // context trees per turn was a fourth full serialization and ~30%
        // extra disk for nothing.
        let rendered = serde_json::to_string(raw).unwrap_or_else(|_| raw.to_string());
        let effective_turn = turn.or(if self.current_turn > 0 {
            Some(self.current_turn)
        } else {
            None
        });
        let snapshot_id = Uuid::new_v4();
        let relative = if let Some(file_turn) = effective_turn {
            format!("turns/turn_{:03}_context_{}.json", file_turn, snapshot_id)
        } else {
            format!("turns/context_{}.json", snapshot_id)
        };
        let file = if fs::write(self.dir.join(&relative), &rendered).is_ok() {
            Some(relative)
        } else {
            None
        };
        // Latest-only rotation, keyed per (source, session id) stream so
        // distinct snapshot streams folding through one log — a native and
        // an external archive snapshot of the same wrapper session, or
        // per-session sub-streams — never delete each other. Historical
        // rows in session.jsonl keep their metadata (tokens, item counts,
        // labels) and the dashboard replay already degrades a missing raw
        // to a raw-less entry — it only ever eagerly loads the latest
        // snapshot per session. Keeping every per-turn context dump was
        // the measured majority of this fleet's log-store disk (O(turns ×
        // context) per session). INTENDANT_CONTEXT_SNAPSHOT_KEEP_ALL=1
        // opts back into archive-everything (resolved once at open; see
        // `keep_all_context_snapshots`).
        //
        // The predecessor is deleted only AFTER the new row is CONFIRMED
        // durable (checked emit below): a crash or write failure between
        // the new-file write and the row landing must strand an orphan
        // file, never leave the last durable row pointing at a deleted
        // sidecar.
        let mut rotation: Option<(String, Option<String>)> = None;
        if let Some(ref new_file) = file {
            if !self.keep_all_context_snapshots {
                let stream_key = super::context_snapshot_stream_key(
                    source,
                    session_id.map(str::trim).filter(|s| !s.is_empty()),
                );
                let previous = self
                    .last_context_snapshots
                    .insert(stream_key.clone(), new_file.clone());
                rotation = Some((stream_key, previous));
            }
        }
        let new_file_for_rotation = file.clone();
        let item_suffix = item_count
            .map(|n| format!(" ({} items)", n))
            .unwrap_or_default();
        let row = LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: effective_turn,
            event: "context_snapshot".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!("Context snapshot: {}{}", label, item_suffix)),
            data: Some({
                let mut data = serde_json::json!({
                    "source": source,
                    "label": label,
                    "request_id": request_id,
                    "request_index": request_index,
                    "format": format,
                    "token_count": token_count,
                    "token_count_kind": token_count_kind,
                    "context_window": context_window,
                    "hard_context_window": hard_context_window,
                    "item_count": item_count,
                });
                if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
                    data["session_id"] = serde_json::Value::String(session_id.to_string());
                }
                data
            }),
            file,
            file2: None,
        };
        match self.emit_checked(row) {
            Ok(()) => {
                if let Some((_, Some(previous))) = rotation {
                    if new_file_for_rotation.as_deref() != Some(previous.as_str()) {
                        let _ = fs::remove_file(self.dir.join(&previous));
                    }
                }
            }
            Err(e) => {
                eprintln!("session_log: failed to write log event: {}", e);
                // The row never durably landed, so the last durable row
                // still references the PREVIOUS sidecar — it must survive.
                // Restore the rotation map to the durable state so the
                // next successful snapshot rotates it out; the just-written
                // new file is the accepted orphan.
                if let Some((stream_key, previous)) = rotation {
                    match previous {
                        Some(previous) => {
                            self.last_context_snapshots.insert(stream_key, previous);
                        }
                        None => {
                            self.last_context_snapshots.remove(&stream_key);
                        }
                    }
                }
            }
        }
    }

    /// Log the full model response. Content is written to a per-turn file.
    #[allow(clippy::too_many_arguments)] // token billing buckets remain explicit in the log schema
    pub fn model_response(
        &mut self,
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        cached_tokens: u64,
        cache_creation_tokens: u64,
        source: Option<&str>,
    ) -> Option<TurnFileSpan> {
        self.model_response_for_session(
            None,
            content,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
            cache_creation_tokens,
            source,
        )
    }

    /// Returns the sidecar span the response text was appended to, so the
    /// combined op ([`Self::model_response_with_message`]) can reference the
    /// same bytes from the canonical `conversation_message` record without a
    /// second write.
    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub fn model_response_for_session(
        &mut self,
        session_id: Option<&str>,
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        cached_tokens: u64,
        cache_creation_tokens: u64,
        source: Option<&str>,
    ) -> Option<TurnFileSpan> {
        self.summary_builder.total_tokens += total_tokens;
        // Codex fires multiple `model_response` events per turn (one per
        // assistant message in the same turn). Appending keeps the full
        // sequence; truncating would leave only the last chunk on disk
        // while session.jsonl's event stream references all of them.
        let span = self.append_turn_file_span("model.txt", content);
        let file = span.as_ref().map(|span| span.relative.clone());
        let preview: String = content.chars().take(200).collect();
        let mut data = serde_json::json!({
            "tokens": {
                "prompt": prompt_tokens,
                "completion": completion_tokens,
                "total": total_tokens,
                "cached": cached_tokens,
                "cache_creation": cache_creation_tokens,
            },
            "content_length": content.len(),
        });
        if let Some(span) = span.as_ref() {
            data["model_offset"] = serde_json::Value::from(span.offset);
            data["model_bytes"] = serde_json::Value::from(span.len);
        }
        if let Some(src) = source {
            data["source"] = serde_json::Value::String(src.to_string());
        }
        if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            data["session_id"] = serde_json::Value::String(session_id.to_string());
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "model_response".to_string(),
            level: Some("info".to_string()),
            message: Some(preview),
            data: Some(data),
            file,
            file2: None,
        });
        span
    }

    /// Combined assistant logging op (message-search plan §4): ONE call
    /// writes the sidecar span, the diagnostic `model_response` event, and
    /// the canonical `conversation_message` record — no crash window between
    /// diagnostic and canonical, and no second copy of the text. Native
    /// acceptance-point only; external wrappers keep plain `model_response`
    /// (their messages are canonical in the NATIVE backend logs, and the
    /// intendant extractor skips wrapper sessions to avoid duplicates).
    #[allow(clippy::too_many_arguments)] // token billing buckets remain explicit in the log schema
    pub fn model_response_with_message(
        &mut self,
        seq: u64,
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        cached_tokens: u64,
        cache_creation_tokens: u64,
    ) -> String {
        let span = self.model_response_for_session(
            None,
            content,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
            cache_creation_tokens,
            None,
        );
        let message_id = Uuid::new_v4().to_string();
        let mut data = serde_json::json!({
            "message_id": message_id,
            "message_seq": seq,
            "role": "assistant",
            "provenance": crate::conversation::MessageProvenance::Assistant,
        });
        let file = span.as_ref().map(|span| {
            data["model_offset"] = serde_json::Value::from(span.offset);
            data["model_bytes"] = serde_json::Value::from(span.len);
            span.relative.clone()
        });
        let preview: String = content.chars().take(200).collect();
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "conversation_message".to_string(),
            level: Some("info".to_string()),
            message: Some(preview),
            data: Some(data),
            file,
            file2: None,
        });
        message_id
    }

    /// Canonical message-lane record for a user-side conversation entry
    /// (task / resume task / follow-up / delivered steer / askHuman answer).
    /// Emitted only where text genuinely enters the worker conversation;
    /// system injections, tool output, and context summaries are
    /// deliberately absent. `text` is the RAW user text — attachment
    /// preludes and `[Session resumed]`/`[New Task]`/`[User]` wrappers are
    /// the conversation's concern, not the record's. `ref_seq` marks a
    /// projection: the text entered the conversation inside another entry
    /// (the native-tool askHuman answer rides a tool result) whose seq it
    /// references for rewind-cut semantics.
    pub fn conversation_message_user(
        &mut self,
        seq: u64,
        provenance: crate::conversation::MessageProvenance,
        text: &str,
        ref_seq: Option<u64>,
    ) -> String {
        let message_id = Uuid::new_v4().to_string();
        let mut data = serde_json::json!({
            "message_id": message_id,
            "message_seq": seq,
            "role": "user",
            "provenance": provenance,
            "text": text,
        });
        if let Some(ref_seq) = ref_seq {
            data["ref_seq"] = serde_json::Value::from(ref_seq);
        }
        let preview: String = text.chars().take(200).collect();
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "conversation_message".to_string(),
            level: Some("info".to_string()),
            message: Some(preview),
            data: Some(data),
            file: None,
            file2: None,
        });
        message_id
    }

    /// A rewind/tail-rollback cut: messages with `seq > cut_after_seq` are
    /// superseded. Compaction (`drop_turns`/`summarize_turns`) deliberately
    /// does NOT emit this — a compacted message was still said and remains
    /// canonical history (message-search plan D2).
    pub fn conversation_rewound(&mut self, cut_after_seq: u64, kind: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "conversation_rewound".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Conversation rewound ({}): messages after seq {} superseded",
                kind, cut_after_seq
            )),
            data: Some(serde_json::json!({
                "cut_after_seq": cut_after_seq,
                "kind": kind,
                "superseded_at_ms": Self::ts_ms(),
            })),
            file: None,
            file2: None,
        });
    }

    /// Mixed-version cutover marker: emitted when a resumed legacy
    /// conversation gets seqs assigned (`ensure_seqs_assigned`). `mapping`
    /// is `(seq, role, content-hash)` per message, in order — extractors use
    /// legacy extraction strictly before this marker and only
    /// `conversation_message` records after it, correlating legacy records
    /// through the hashes.
    pub fn conversation_message_epoch(&mut self, mapping: &[(u64, String, String)]) {
        let rows: Vec<serde_json::Value> = mapping
            .iter()
            .map(|(seq, role, hash)| serde_json::json!([seq, role, hash]))
            .collect();
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "conversation_message_epoch".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Assigned seqs to {} legacy conversation messages",
                mapping.len()
            )),
            data: Some(serde_json::json!({ "mapping": rows })),
            file: None,
            file2: None,
        });
    }

    /// Log the full JSON sent to the agent runtime.
    pub fn agent_input(&mut self, json: &str) {
        // Parse once; the pretty file body and the function-name summary
        // both derive from the same DOM (this used to re-tokenize the full
        // command batch a second time).
        let parsed = serde_json::from_str::<serde_json::Value>(json).ok();
        let pretty = parsed
            .as_ref()
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| json.to_string());
        let file = self.write_turn_file("agent_in.json", &pretty);

        // Extract function names for the summary
        let functions: Vec<String> = parsed
            .as_ref()
            .and_then(|v| v.get("commands")?.as_array())
            .map(|commands| {
                commands
                    .iter()
                    .filter_map(|cmd| {
                        cmd.get("function")
                            .and_then(|f| f.as_str())
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();

        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "agent_input".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Commands: {}", functions.join(", "))),
            data: Some(serde_json::json!({
                "functions": functions,
                "json_length": json.len(),
            })),
            file,
            file2: None,
        });
    }

    /// Log agent runtime output. Written to per-turn files.
    ///
    /// A single turn may run many commands, each producing its own output
    /// chunk; we append so the file reflects the full turn history rather
    /// than only the last chunk.
    #[allow(dead_code)]
    pub fn agent_output(&mut self, stdout: &str, stderr: &str, source: Option<&str>) {
        self.agent_output_with_id(stdout, stderr, source, None);
    }

    pub fn agent_output_with_id(
        &mut self,
        stdout: &str,
        stderr: &str,
        source: Option<&str>,
        output_id: Option<&str>,
    ) {
        self.agent_output_with_session_id(None, stdout, stderr, source, output_id, None);
    }

    pub fn agent_output_with_session_id(
        &mut self,
        session_id: Option<&str>,
        stdout: &str,
        stderr: &str,
        source: Option<&str>,
        output_id: Option<&str>,
        item_id: Option<&str>,
    ) {
        // Invalidates the gateway's negative agent-output memo for this
        // logs root (see `replay::agent_output_generation`).
        super::replay::note_agent_output_appended(self.dir());
        let stdout_span = if !stdout.is_empty() {
            self.append_turn_file_span("stdout.txt", stdout)
        } else {
            None
        };
        let stderr_span = if !stderr.is_empty() {
            self.append_turn_file_span("stderr.txt", stderr)
        } else {
            None
        };

        let preview: String = stdout.chars().take(200).collect();
        let mut data = serde_json::json!({
            "stdout_length": stdout.len(),
            "stderr_length": stderr.len(),
        });
        if let Some(id) = output_id {
            data["output_id"] = serde_json::Value::String(id.to_string());
        }
        if let Some(id) = item_id.map(str::trim).filter(|id| !id.is_empty()) {
            data["item_id"] = serde_json::Value::String(id.to_string());
        }
        if let Some(src) = source {
            data["source"] = serde_json::Value::String(src.to_string());
        }
        if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            data["session_id"] = serde_json::Value::String(session_id.to_string());
        }
        if let Some(span) = stdout_span.as_ref() {
            data["stdout_offset"] = serde_json::Value::from(span.offset);
            data["stdout_bytes"] = serde_json::Value::from(span.len);
        }
        if let Some(span) = stderr_span.as_ref() {
            data["stderr_offset"] = serde_json::Value::from(span.offset);
            data["stderr_bytes"] = serde_json::Value::from(span.len);
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "agent_output".to_string(),
            level: if stderr.is_empty() {
                Some("info".to_string())
            } else {
                Some("warn".to_string())
            },
            message: if stdout.is_empty() && stderr.is_empty() {
                Some("(no output)".to_string())
            } else {
                Some(preview)
            },
            data: Some(data),
            file: stdout_span.map(|span| span.relative),
            file2: stderr_span.map(|span| span.relative),
        });
    }

    /// Log reasoning content from the model (full reasoning, not just summary).
    pub fn reasoning_content(&mut self, summary: Option<&str>, full_content: Option<&str>) {
        let file = full_content.and_then(|c| self.append_turn_file("reasoning.txt", c));
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "reasoning".to_string(),
            level: Some("info".to_string()),
            message: summary.map(|s| s.to_string()),
            data: Some(serde_json::json!({
                "has_summary": summary.is_some(),
                "has_full_content": full_content.is_some(),
                "full_content_length": full_content.map(|c| c.len()).unwrap_or(0),
            })),
            file,
            file2: None,
        });
    }

    /// Log an approval event.
    pub fn approval(&mut self, category: &str, preview: &str, decision: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "approval".to_string(),
            level: Some("warn".to_string()),
            message: Some(format!("{} -> {}", preview, decision)),
            data: Some(serde_json::json!({
                "category": category,
                "decision": decision,
                "preview": preview,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log the JSON extracted from a model response.
    pub fn json_extracted(&mut self, json: &str) {
        // Parse once; both the function names and the done flag derive
        // from the same DOM.
        let parsed = serde_json::from_str::<serde_json::Value>(json).ok();
        let functions: Vec<String> = parsed
            .as_ref()
            .and_then(|v| v.get("commands")?.as_array())
            .map(|commands| {
                commands
                    .iter()
                    .filter_map(|cmd| {
                        cmd.get("function")
                            .and_then(|f| f.as_str())
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let done = parsed
            .as_ref()
            .and_then(|v| v.get("done")?.as_bool())
            .unwrap_or(false);

        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: Some(self.current_turn),
            event: "json_extracted".to_string(),
            level: Some("debug".to_string()),
            message: Some(if functions.is_empty() {
                if done {
                    "done signal".to_string()
                } else {
                    "no commands".to_string()
                }
            } else {
                functions.join(", ")
            }),
            data: Some(serde_json::json!({
                "functions": functions,
                "done": done,
                "json_length": json.len(),
            })),
            file: None,
            file2: None,
        });
    }

    /// Write the session summary (call at end of session).
    /// Also updates session_meta.json with completion status.
    pub fn write_summary(&mut self, task: &str, outcome: &str, total_turns: usize) {
        self.write_summary_with_rounds(task, outcome, total_turns, None);
    }

    /// Write session summary with optional round count.
    pub fn write_summary_with_rounds(
        &mut self,
        task: &str,
        outcome: &str,
        total_turns: usize,
        rounds: Option<usize>,
    ) {
        let mut summary = serde_json::json!({
            "task": task,
            "outcome": outcome,
            "total_turns": total_turns,
            "session_id": self.session_id,
            "session_dir": self.dir.to_string_lossy(),
            "ended_at": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        if let Some(r) = rounds {
            summary["rounds"] = serde_json::json!(r);
        }
        let path = self.dir.join("summary.json");
        if let Ok(pretty) = serde_json::to_string_pretty(&summary) {
            if let Err(e) = fs::write(&path, &pretty) {
                eprintln!("session_log: failed to write summary.json: {}", e);
            }
        }

        // Update session_meta.json with completion status
        let meta_path = self.dir.join("session_meta.json");
        if let Ok(meta_str) = fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                meta.status = Some("completed".to_string());
                meta.last_turn = Some(total_turns);
                meta.rounds = rounds;
                if let Ok(json) = serde_json::to_string_pretty(&meta) {
                    if let Err(e) = fs::write(&meta_path, &json) {
                        eprintln!("session_log: failed to update session_meta.json: {}", e);
                    }
                }
            }
        }

        self.emit(LogEvent {
            ts: Self::ts(),
            ts_ms: Self::ts_ms(),
            turn: None,
            event: "session_end".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Session ended: {} ({} turns)",
                outcome, total_turns
            )),
            data: None,
            file: Some("summary.json".to_string()),
            file2: None,
        });

        // Write the rich session summary alongside the simple one
        self.write_session_summary();
    }

    /// Mark the session as interrupted and flush logs.
    /// Called from signal handlers (SIGTERM) where Drop may not run.
    pub fn mark_interrupted(&mut self) {
        self.flush_voice_utterance();
        let _ = self.writer.flush();
        mark_session_meta_interrupted(&self.dir, Some(self.current_turn));
        // Write partial session summary even on interrupt
        self.write_session_summary();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::tests::read_last_event;

    /// Relative sidecar paths of every context_snapshot event in
    /// session.jsonl, in emit order.
    fn snapshot_files(log_dir: &std::path::Path) -> Vec<String> {
        fs::read_to_string(log_dir.join("session.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("context_snapshot"))
            .filter_map(|v| v.get("file").and_then(|f| f.as_str()).map(String::from))
            .collect()
    }

    #[test]
    fn context_snapshots_rotate_to_latest_only_per_source() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        // Hermetic: the policy is injected at construction; ambient
        // INTENDANT_CONTEXT_SNAPSHOT_KEEP_ALL can't affect the test.
        let mut log = SessionLog::open_with_retention(log_dir.clone(), false).unwrap();
        log.turn_start(1, 0.0, 0);
        let raw1 = serde_json::json!({"messages": ["turn one"]});
        log.context_snapshot(
            "native",
            "req 1",
            Some(1),
            "test.v1",
            None,
            None,
            None,
            None,
            Some(1),
            &raw1,
        );
        log.turn_start(2, 0.0, 0);
        let raw2 = serde_json::json!({"messages": ["turn one", "turn two"]});
        log.context_snapshot(
            "native",
            "req 2",
            Some(2),
            "test.v1",
            None,
            None,
            None,
            None,
            Some(2),
            &raw2,
        );
        // A different source in the same session must not be rotated away
        // by the native snapshots.
        let raw_other = serde_json::json!({"summary": "external"});
        log.context_snapshot(
            "codex",
            "external archive",
            Some(2),
            "codex.v1",
            None,
            None,
            None,
            None,
            None,
            &raw_other,
        );

        let files = snapshot_files(&log_dir);
        assert_eq!(files.len(), 3, "every event row keeps its file field");
        // The older native sidecar is gone; the latest native one and the
        // other-source one remain, parseable, and compact (not pretty).
        assert!(!log_dir.join(&files[0]).exists(), "rotated: {}", files[0]);
        let latest_native = fs::read_to_string(log_dir.join(&files[1])).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&latest_native).unwrap(),
            raw2
        );
        assert!(
            !latest_native.contains('\n'),
            "sidecar is compact single-line JSON"
        );
        assert!(log_dir.join(&files[2]).exists(), "other source kept");
    }

    #[test]
    fn rotation_keeps_the_predecessor_when_the_row_fails_to_persist() {
        // Disk-full contract: if the new context_snapshot row cannot be
        // made durable, the last durable row still references the OLD
        // sidecar — it must not be deleted. The just-written new file is
        // the accepted orphan.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open_with_retention(log_dir.clone(), false).unwrap();
        log.turn_start(1, 0.0, 0);
        log.context_snapshot(
            "native",
            "req 1",
            Some(1),
            "test.v1",
            None,
            None,
            None,
            None,
            Some(1),
            &serde_json::json!({"messages": ["durable turn"]}),
        );
        let files = snapshot_files(&log_dir);
        assert_eq!(files.len(), 1);
        let durable_sidecar = files[0].clone();

        log.sabotage_writer_for_tests();
        log.context_snapshot(
            "native",
            "req 2",
            Some(1),
            "test.v1",
            None,
            None,
            None,
            None,
            Some(1),
            &serde_json::json!({"messages": ["never lands"]}),
        );

        // The durable row set is unchanged, and its sidecar survives.
        let files = snapshot_files(&log_dir);
        assert_eq!(files, vec![durable_sidecar.clone()]);
        assert!(
            log_dir.join(&durable_sidecar).exists(),
            "predecessor must survive a failed row emit"
        );
        // Both sidecar files exist: the durable one + the accepted orphan.
        let sidecars = fs::read_dir(log_dir.join("turns"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains("context"))
            .count();
        assert_eq!(sidecars, 2, "orphaned new file is the accepted side");
    }

    #[test]
    fn context_snapshot_rotation_survives_a_session_reopen() {
        // The rotation map is seeded from persisted rows at open — without
        // that, every restart/--continue strands the previous process's
        // latest sidecar forever (retention O(reopenings)).
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        {
            let mut log = SessionLog::open_with_retention(log_dir.clone(), false).unwrap();
            log.turn_start(1, 0.0, 0);
            log.context_snapshot(
                "native",
                "req 1",
                Some(1),
                "test.v1",
                None,
                None,
                None,
                None,
                Some(1),
                &serde_json::json!({"messages": ["first process"]}),
            );
        }
        let first_files = snapshot_files(&log_dir);
        assert_eq!(first_files.len(), 1);
        assert!(log_dir.join(&first_files[0]).exists());

        // Second process resumes the same session dir.
        let mut log = SessionLog::open_with_retention(log_dir.clone(), false).unwrap();
        log.turn_start(2, 0.0, 0);
        log.context_snapshot(
            "native",
            "req 2",
            Some(2),
            "test.v1",
            None,
            None,
            None,
            None,
            Some(2),
            &serde_json::json!({"messages": ["first process", "second process"]}),
        );

        let files = snapshot_files(&log_dir);
        assert_eq!(files.len(), 2);
        assert!(
            !log_dir.join(&files[0]).exists(),
            "predecessor sidecar rotated out across the reopen: {}",
            files[0]
        );
        assert!(log_dir.join(&files[1]).exists(), "latest sidecar kept");
    }

    #[test]
    fn append_turn_file_accumulates_with_separator() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        // Move past turn 0 so the file suffix stabilises.
        log.turn_start(1, 0.0, 0);
        log.agent_output("first\n", "", None);
        log.agent_output("second\n", "", None);
        let turn_file = log_dir.join("turns/turn_001_stdout.txt");
        let body = fs::read_to_string(&turn_file).unwrap();
        assert!(body.contains("first"), "missing first write: {}", body);
        assert!(body.contains("second"), "missing second write: {}", body);
        // Separator: the two entries are distinct.
        assert!(
            body.find("first").unwrap() < body.find("second").unwrap(),
            "second entry must come after first"
        );
    }

    #[test]
    fn append_turn_file_skips_separator_on_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(2, 0.0, 0);
        log.agent_output("only\n", "", None);
        let body = fs::read_to_string(log_dir.join("turns/turn_002_stdout.txt")).unwrap();
        // No leading blank line before the first chunk.
        assert!(
            !body.starts_with('\n'),
            "unexpected leading newline: {:?}",
            body
        );
    }

    #[test]
    fn tool_response_preview_truncates_multibyte_on_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        let result = format!("{}{}tail", "a".repeat(199), "\u{00e9}");
        log.tool_response("inspect", &result);

        let body = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let event: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
        assert_eq!(
            event["message"].as_str().unwrap(),
            format!("inspect \u{2192} {}", "a".repeat(199))
        );
    }

    #[test]
    fn events_are_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.info("test info");
        log.warn("test warn");
        log.error("test error");
        log.debug("test debug");
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        for line in content.lines() {
            let parsed: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("Invalid JSON line: {}\n  {}", line, e));
            assert!(parsed.get("ts").is_some());
            assert!(parsed.get("event").is_some());
        }
    }

    #[test]
    fn turn_start_sets_current_turn() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(3, 25.5, 150_000);
        log.info("should have turn 3");
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["turn"], 3);
    }

    #[test]
    fn model_response_writes_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.model_response(
            "Hello, I will help you.\nHere is my plan.",
            100,
            50,
            150,
            0,
            0,
            None,
        );
        drop(log);

        let model_file = log_dir.join("turns/turn_001_model.txt");
        assert!(model_file.exists());
        let content = fs::read_to_string(&model_file).unwrap();
        assert!(content.contains("Hello, I will help you."));
        assert!(content.contains("Here is my plan."));
    }

    #[test]
    fn agent_input_creates_pretty_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(2, 10.0, 180_000);
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#);
        drop(log);

        let agent_file = log_dir.join("turns/turn_002_agent_in.json");
        assert!(agent_file.exists());
        let content = fs::read_to_string(&agent_file).unwrap();
        assert!(content.contains("execAsAgent"));
        // Should be pretty-printed (has newlines)
        assert!(content.contains('\n'));
    }

    #[test]
    fn agent_output_creates_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.agent_output("stdout content", "stderr content", None);
        drop(log);

        assert!(log_dir.join("turns/turn_001_stdout.txt").exists());
        assert!(log_dir.join("turns/turn_001_stderr.txt").exists());
        let stdout = fs::read_to_string(log_dir.join("turns/turn_001_stdout.txt")).unwrap();
        assert_eq!(stdout, "stdout content");
    }

    #[test]
    fn agent_output_skips_empty_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.agent_output("stdout only", "", None);
        drop(log);

        assert!(log_dir.join("turns/turn_001_stdout.txt").exists());
        assert!(!log_dir.join("turns/turn_001_stderr.txt").exists());
    }

    #[test]
    fn approval_log_is_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(5, 30.0, 140_000);
        log.approval("file_write", "writeFile: /tmp/test.rs", "approved");
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(content.contains("\"event\":\"approval\""));
        assert!(content.contains("\"category\":\"file_write\""));
        assert!(content.contains("\"decision\":\"approved\""));
    }

    #[test]
    fn json_extracted_shows_functions() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.json_extracted(r#"{"commands":[{"function":"execAsAgent","nonce":1},{"function":"writeFile","nonce":2}]}"#);
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(content.contains("execAsAgent"));
        assert!(content.contains("writeFile"));
    }

    #[test]
    fn write_summary_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_summary("test task", "completed", 5);
        drop(log);

        let summary_path = log_dir.join("summary.json");
        assert!(summary_path.exists());
        let content = fs::read_to_string(&summary_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["task"], "test task");
        assert_eq!(parsed["outcome"], "completed");
        assert_eq!(parsed["total_turns"], 5);
    }

    #[test]
    fn write_summary_updates_meta() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.write_summary("task", "completed", 3);
        drop(log);

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("completed"));
        assert_eq!(meta.last_turn, Some(3));
    }

    #[test]
    fn multiple_turns_create_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.turn_start(1, 0.0, 200_000);
        log.model_response("Response 1", 100, 50, 150, 0, 0, None);
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#);
        log.agent_output("out1", "", None);

        log.turn_start(2, 5.0, 190_000);
        log.model_response("Response 2", 200, 100, 300, 0, 0, None);
        log.agent_input(r#"{"commands":[{"function":"writeFile","nonce":2}]}"#);
        log.agent_output("out2", "err2", None);

        drop(log);

        assert!(log_dir.join("turns/turn_001_model.txt").exists());
        assert!(log_dir.join("turns/turn_002_model.txt").exists());
        assert!(log_dir.join("turns/turn_001_agent_in.json").exists());
        assert!(log_dir.join("turns/turn_002_agent_in.json").exists());
        assert!(log_dir.join("turns/turn_001_stdout.txt").exists());
        assert!(log_dir.join("turns/turn_002_stdout.txt").exists());
        assert!(!log_dir.join("turns/turn_001_stderr.txt").exists());
        assert!(log_dir.join("turns/turn_002_stderr.txt").exists());
    }

    #[test]
    fn messages_input_writes_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.messages_input(
            r#"[{"role":"system","content":"You are an AI."},{"role":"user","content":"Hello"}]"#,
        );
        drop(log);

        let messages_file = log_dir.join("turns/turn_001_messages.json");
        assert!(messages_file.exists());
        let content = fs::read_to_string(&messages_file).unwrap();
        assert!(content.contains("system"));
        assert!(content.contains("Hello"));
    }

    #[test]
    fn reasoning_content_writes_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.reasoning_content(
            Some("The model is thinking about X"),
            Some("Full detailed reasoning about X and Y"),
        );
        drop(log);

        let reasoning_file = log_dir.join("turns/turn_001_reasoning.txt");
        assert!(reasoning_file.exists());
        let content = fs::read_to_string(&reasoning_file).unwrap();
        assert!(content.contains("Full detailed reasoning"));

        let session = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(session.contains("\"event\":\"reasoning\""));
        assert!(session.contains("has_summary"));
    }

    #[test]
    fn reasoning_content_summary_only() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.reasoning_content(Some("Summary only"), None);
        drop(log);

        // No reasoning file created when no full content
        assert!(!log_dir.join("turns/turn_001_reasoning.txt").exists());
    }

    #[test]
    fn drop_updates_running_to_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.turn_start(3, 10.0, 180_000);
        drop(log);

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("interrupted"));
        assert_eq!(meta.last_turn, Some(3));
    }

    #[test]
    fn drop_does_not_overwrite_completed() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.write_summary("task", "completed", 5);
        drop(log);

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("completed"));
        assert_eq!(meta.last_turn, Some(5));
    }

    #[test]
    fn round_complete_marks_running_session_idle() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.turn_start(3, 0.0, 100000);
        log.round_complete(2, 1);
        drop(log);

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("idle"));
        assert_eq!(meta.last_turn, Some(3));
        assert_eq!(meta.rounds, Some(2));
    }

    #[test]
    fn mark_interrupted_updates_running_session() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.turn_start(7, 0.0, 100000);

        // Explicitly mark interrupted (simulates SIGTERM handler)
        log.mark_interrupted();

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("interrupted"));
        assert_eq!(meta.last_turn, Some(7));
    }

    #[test]
    fn mark_interrupted_does_not_overwrite_completed() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.write_summary("task", "completed", 5);

        // mark_interrupted should not overwrite "completed"
        log.mark_interrupted();

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("completed"));
    }

    #[test]
    fn steer_lifecycle_logs_full_text_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        let text = "Quick interjectory note:\nPause before any Station merge/push. Preserve the exact full prompt for recovery.";
        log.steer_requested(Some("thread-1"), "steer-1", text);
        log.steer_queued(
            Some("thread-1"),
            "steer-1",
            "codex native mid-turn steering failed",
        );
        log.steer_delivered(Some("thread-1"), "steer-1", false);
        log.steer_cancelled(Some("thread-1"), "steer-2", "cleared by user");
        drop(log);

        let requested = read_last_event(&log_dir, "steer_requested");
        assert_eq!(requested["data"]["session_id"], "thread-1");
        assert_eq!(requested["data"]["id"], "steer-1");
        assert_eq!(requested["data"]["text"], text);
        assert_eq!(requested["data"]["status"], "pending");

        let queued = read_last_event(&log_dir, "steer_queued");
        assert_eq!(queued["level"], "warn");
        assert_eq!(
            queued["data"]["reason"],
            "codex native mid-turn steering failed"
        );

        let delivered = read_last_event(&log_dir, "steer_delivered");
        assert_eq!(delivered["data"]["mid_turn"], false);

        let cancelled = read_last_event(&log_dir, "steer_cancelled");
        assert_eq!(cancelled["level"], "warn");
        assert_eq!(cancelled["data"]["session_id"], "thread-1");
        assert_eq!(cancelled["data"]["id"], "steer-2");
        assert_eq!(cancelled["data"]["status"], "cancelled");
        assert_eq!(cancelled["data"]["reason"], "cleared by user");
    }

    #[test]
    fn tool_request_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        let args = serde_json::json!({"id": 42});
        log.tool_request("approve_action", &args);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "tool_request");
        assert_eq!(last["data"]["tool"], "approve_action");
        assert_eq!(last["data"]["args"]["id"], 42);
    }

    #[test]
    fn tool_response_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.tool_response("check_status", "Phase: idle, Turn: 0");

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "tool_response");
        assert!(last["message"].as_str().unwrap().contains("check_status"));
    }

    #[test]
    fn approval_resolved_dedupes_repeated_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.approval_resolved(7, "approve");
        log.approval_resolved(7, "approve");
        log.approval_resolved(7, "reject");
        log.approval_resolved(8, "approve");
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|entry| {
                entry.get("event").and_then(|event| event.as_str()) == Some("approval_resolved")
            })
            .collect();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["turn"], 7);
        assert_eq!(entries[0]["message"], "Approval approve (turn 7)");
        assert_eq!(entries[1]["turn"], 7);
        assert_eq!(entries[1]["message"], "Approval reject (turn 7)");
        assert_eq!(entries[2]["turn"], 8);
        assert_eq!(entries[2]["message"], "Approval approve (turn 8)");
    }
}
