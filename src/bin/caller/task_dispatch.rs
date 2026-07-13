//! Backend task dispatcher.
//!
//! Listens on the EventBus for `AppEvent::ControlCommand` and handles the
//! legacy single-session command set: task/follow-up dispatch plus interrupt,
//! steer, and cancellation fanout. Targeted managed sessions are owned by
//! `SessionSupervisor`; this dispatcher only routes commands for the channels
//! it already owns.
//!
//! This module replaces the routing logic that previously lived in the TUI's
//! `handle_control_command`. The TUI is now display-only — it observes phase
//! changes and renders updates, but no longer owns dispatch authority.
//!
//! Routing policy for a task {text, direct, metadata}:
//!   1. If `direct != true` AND `presence_tx` is available: send text to
//!      `presence_tx`. The presence LLM decides whether to forward as a real
//!      task (via its own `submit_task` tool -> task_tx) or respond in-line.
//!   2. Else if `task_tx` is available: wrap in `TaskEnvelope` and send.
//!      `force_direct` is derived from the `direct` flag (plus legacy
//!      `orchestrate == Some(false)` for StartTask).
//!   3. Else if `follow_up_tx` is available: send a follow-up message. Metadata is
//!      dropped (non-presence mode has no CU-first routing anyway).
//!   4. Else: warn and drop.
//!
//! Presence's own `submit_task` tool keeps direct `task_tx` access for
//! synchronous tool-result semantics — the dispatcher coordinates frontend
//! → backend routing, not presence-internal LLM tool calls.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::FollowUpMessage;

/// Senders the dispatcher owns. Clone to populate these from the channels
/// already created in `main.rs` (e.g. for presence task loop / agent loop).
#[derive(Clone)]
pub struct Dispatcher {
    /// Presence user-input channel. When `Some`, non-direct tasks route here
    /// so the presence LLM can mediate.
    pub presence_tx: Option<mpsc::Sender<String>>,
    /// Task envelope channel consumed by `run_with_presence`. When `Some`,
    /// direct tasks go here (full metadata preserved).
    pub task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
    /// Follow-up channel consumed by `run_direct_mode` /
    /// `run_external_agent_mode` in non-presence mode.
    pub follow_up_tx: Option<mpsc::Sender<FollowUpMessage>>,
    /// Session id owned by the legacy single-session loop. When set, targeted
    /// commands for any other session are left for the session supervisor.
    ///
    /// External backends upgrade their session address mid-flight
    /// (`persist_native_backend_session_id`): frontends may target the session
    /// by either the wrapper/log id or the backend-native id afterwards. The
    /// spawned dispatcher watches `SessionIdentity` events and accepts every
    /// id in the primary session's identity group — an exact-match here used
    /// to silently drop every post-upgrade Steer/Interrupt/StartTask from the
    /// dashboard.
    pub primary_session_id: Option<String>,
}

impl Dispatcher {
    /// Spawn a background task that consumes the bus's lossless intent lane
    /// ([`EventBus::subscribe_intents`]) and routes task dispatch commands.
    /// The handle is aborted on session end.
    ///
    /// The lane — not the lossy broadcast ring — because a dropped
    /// `StartTask`/`FollowUp`/`Interrupt` is an unrecoverable lost user
    /// action: the dispatcher is the only consumer that acts on it.
    pub fn spawn(self, bus: EventBus) -> JoinHandle<()> {
        let mut intent_rx = bus.subscribe_intents();
        let bus_for_log = bus.clone();
        let accepted = Arc::new(RwLock::new(
            self.primary_session_id
                .iter()
                .cloned()
                .collect::<HashSet<String>>(),
        ));
        if self.primary_session_id.is_some() {
            // Identity listener: fold backend-native ids into the accepted
            // set as sessions upgrade their address. The broadcast lane is
            // LOSSY: a lagged receiver has permanently dropped events, and a
            // `SessionIdentity` among them is forfeited — commands addressed
            // to that backend-native id are then silently ignored until some
            // later announcement re-links it (the wrapper id keeps working
            // regardless). No cheap authoritative snapshot of announced
            // identities exists in-process (they persist per-session-dir in
            // `session.jsonl`), so the honest response is to say so loudly.
            let accepted = accepted.clone();
            let mut identity_rx = bus.subscribe();
            let lag_bus = bus.clone();
            let lag_session_id = self.primary_session_id.clone();
            tokio::spawn(async move {
                loop {
                    match identity_rx.recv().await {
                        Ok(AppEvent::SessionIdentity {
                            session_id,
                            backend_session_id,
                            ..
                        }) => {
                            let mut ids = accepted.write().expect("dispatcher alias lock");
                            if ids.contains(&session_id) || ids.contains(&backend_session_id) {
                                ids.insert(session_id);
                                ids.insert(backend_session_id);
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            lag_bus.send(AppEvent::LogEntry {
                                session_id: lag_session_id.clone(),
                                level: "warn".to_string(),
                                source: "system".to_string(),
                                content: format!(
                                    "Dispatcher identity listener lagged; {} dropped event(s) — a missed SessionIdentity means commands targeting the backend-native id may be ignored until the next announcement (session {})",
                                    n,
                                    lag_session_id.as_deref().unwrap_or("?")
                                ),
                                turn: None,
                            });
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        let arc = Arc::new(self);

        tokio::spawn(async move {
            while let Some(event) = intent_rx.recv().await {
                if let AppEvent::ControlCommand(msg) = event {
                    arc.route(msg, &bus_for_log, &accepted).await;
                }
            }
        })
    }

    async fn route(&self, msg: ControlMsg, bus: &EventBus, accepted: &RwLock<HashSet<String>>) {
        if let Some(target_session_id) = control_target_session_id(&msg) {
            if !self.handles_target_session_in(target_session_id, accepted) {
                return;
            }
        }

        match msg {
            ControlMsg::CreateSession { .. } => {
                // New sessions are owned by SessionSupervisor. The legacy
                // single-session dispatcher only routes work into channels it
                // already owns, so accepting this here would collapse a
                // parallel-session request back into the active session.
            }

            ControlMsg::StartTask {
                session_id: _,
                task,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                follow_up_id,
                // The legacy single-session dispatcher deliberately does
                // NOT acknowledge peer delegations: it routes into an
                // existing loop (presence/task/follow-up channels), so
                // there is no fresh dispatch identity to report. A
                // delegating daemon falls back to its fire-and-forget
                // path exactly as against a pre-receipt build — see the
                // compatibility matrix in peer::transport::intendant.
                delegation_id: _,
            } => {
                let is_direct = direct.unwrap_or(false) || orchestrate == Some(false);
                let has_metadata = !reference_frame_ids.is_empty()
                    || display_target.is_some()
                    || !attachments.is_empty();

                // If the task has metadata (attachments, frame refs, display
                // target), it MUST go via task_tx to preserve that data. Non-
                // direct is overridden in that case — presence can't carry
                // metadata through its text channel.
                let prefer_task_tx = is_direct || has_metadata;

                if !prefer_task_tx {
                    if let Some(ref tx) = self.presence_tx {
                        if tx.try_send(task.clone()).is_ok() {
                            return;
                        }
                    }
                }

                if let Some(ref tx) = self.task_tx {
                    let envelope = presence_core::TaskEnvelope {
                        task: task.clone(),
                        force_direct: is_direct,
                        context_hints: vec![],
                        reference_frame_ids,
                        display_target,
                        attachment_frame_ids: attachments,
                        steer_id: None,
                    };
                    if tx.try_send(envelope).is_ok() {
                        return;
                    }
                }

                if let Some(ref tx) = self.follow_up_tx {
                    if tx
                        .try_send(
                            FollowUpMessage::text(task.clone())
                                .with_follow_up_id(follow_up_id.clone()),
                        )
                        .is_ok()
                    {
                        return;
                    }
                }

                self.warn_drop(bus, "StartTask", &task);
            }

            ControlMsg::ResumeSession { .. } => {
                // The daemon loop owns session reattachment because it needs
                // to choose the log dir, project root, and backend-native id.
            }

            ControlMsg::FollowUp {
                text,
                direct,
                follow_up_id,
                ..
            } => {
                let is_direct = direct.unwrap_or(false);

                if !is_direct {
                    if let Some(ref tx) = self.presence_tx {
                        if tx.try_send(text.clone()).is_ok() {
                            return;
                        }
                    }
                }

                if let Some(ref tx) = self.task_tx {
                    let envelope = presence_core::TaskEnvelope {
                        task: text.clone(),
                        force_direct: is_direct,
                        context_hints: vec![],
                        reference_frame_ids: vec![],
                        display_target: None,
                        attachment_frame_ids: vec![],
                        steer_id: None,
                    };
                    if tx.try_send(envelope).is_ok() {
                        return;
                    }
                }

                if let Some(ref tx) = self.follow_up_tx {
                    if tx
                        .try_send(
                            FollowUpMessage::text(text.clone())
                                .with_follow_up_id(follow_up_id.clone()),
                        )
                        .is_ok()
                    {
                        return;
                    }
                }

                self.warn_drop(bus, "FollowUp", &text);
            }

            ControlMsg::CancelFollowUp {
                session_id,
                id,
                reason,
            } => {
                bus.send(AppEvent::FollowUpCancelRequested {
                    session_id,
                    id,
                    reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
                });
            }

            ControlMsg::Interrupt {
                session_id,
                expected_turn: _,
            } => {
                // Re-emit as AppEvent::InterruptRequested so agent loops can subscribe
                // and cancel their own work. The dispatcher itself doesn't hold loop
                // handles — loops register interest via the bus.
                bus.send(AppEvent::InterruptRequested { session_id });
            }

            ControlMsg::Steer {
                session_id,
                text,
                id,
                attachments,
            } => {
                if !attachments.is_empty() {
                    let steer_id = id.unwrap_or_default();
                    if let Some(ref tx) = self.task_tx {
                        let envelope = presence_core::TaskEnvelope {
                            task: text.clone(),
                            force_direct: true,
                            context_hints: vec![],
                            reference_frame_ids: vec![],
                            display_target: None,
                            attachment_frame_ids: attachments.clone(),
                            steer_id: if steer_id.is_empty() {
                                None
                            } else {
                                Some(steer_id.clone())
                            },
                        };
                        if tx.try_send(envelope).is_ok() {
                            bus.send(AppEvent::SteerQueued {
                                session_id,
                                id: steer_id,
                                reason: "attachments are queued for the next turn".to_string(),
                            });
                            return;
                        }
                    }
                    // External-agent shape: no task channel, but the
                    // follow-up lane carries attachments (as unresolved
                    // frame ids) and keeps the steer id so cancel and
                    // delivery receipts stay addressable. Dropping the
                    // steer here lost attachment steers on every external
                    // session.
                    if let Some(ref tx) = self.follow_up_tx {
                        let mut follow_up = FollowUpMessage::steer(
                            text.clone(),
                            Default::default(),
                            steer_id.clone(),
                        );
                        follow_up.unresolved_attachment_ids = attachments;
                        follow_up.target_session_id = session_id.clone();
                        if tx.try_send(follow_up).is_ok() {
                            bus.send(AppEvent::SteerQueued {
                                session_id,
                                id: steer_id,
                                reason: "attachments are queued for the next turn".to_string(),
                            });
                            return;
                        }
                    }
                    self.warn_drop(bus, "Steer", &text);
                    return;
                }
                // Re-emit as AppEvent::SteerRequested so agent loops can
                // subscribe and either inject the text into the active turn
                // (native mid-turn steering) or queue it onto
                // `context_injection` for the next turn. `id` defaults to
                // "" so downstream consumers never have to handle an Option.
                bus.send(AppEvent::SteerRequested {
                    session_id,
                    text,
                    id: id.unwrap_or_default(),
                });
            }

            ControlMsg::CancelSteer {
                session_id,
                id,
                reason,
            } => {
                bus.send(AppEvent::SteerCancelRequested {
                    session_id,
                    id,
                    reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
                });
            }

            _ => {
                // Not a task-dispatch command — ignore.
            }
        }
    }

    fn warn_drop(&self, bus: &EventBus, kind: &str, preview: &str) {
        let trunc: String = preview.chars().take(80).collect();
        bus.send(AppEvent::LogEntry {
            session_id: None,
            level: "warn".to_string(),
            source: "system".to_string(),
            content: format!("{} dropped (no dispatch target available): {}", kind, trunc),
            turn: None,
        });
    }

    fn handles_target_session_in(
        &self,
        session_id: &str,
        accepted: &RwLock<HashSet<String>>,
    ) -> bool {
        if self.primary_session_id.is_none() {
            return true;
        }
        accepted
            .read()
            .expect("dispatcher alias lock")
            .contains(session_id)
    }
}

fn control_target_session_id(msg: &ControlMsg) -> Option<&str> {
    match msg {
        ControlMsg::Status { session_id }
        | ControlMsg::Approve { session_id, .. }
        | ControlMsg::Deny { session_id, .. }
        | ControlMsg::Skip { session_id, .. }
        | ControlMsg::ApproveAll { session_id, .. }
        | ControlMsg::AnswerQuestion { session_id, .. }
        | ControlMsg::Interrupt { session_id, .. }
        | ControlMsg::Steer { session_id, .. }
        | ControlMsg::CancelSteer { session_id, .. }
        | ControlMsg::StartTask { session_id, .. }
        | ControlMsg::FollowUp { session_id, .. }
        | ControlMsg::CancelFollowUp { session_id, .. } => session_id.as_deref(),
        ControlMsg::ConfigureSessionAgent { session_id, .. } => Some(session_id.as_str()),
        ControlMsg::StopSession { session_id } => Some(session_id.as_str()),
        ControlMsg::ResumeSession { .. } | ControlMsg::RestartSession { .. } => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn make_test_bus() -> EventBus {
        EventBus::new()
    }

    #[tokio::test]
    async fn start_task_with_metadata_prefers_task_tx() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel::<FollowUpMessage>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: Some(follow_up_tx),
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: None,
            task: "do thing".into(),
            orchestrate: None,
            direct: None,
            reference_frame_ids: vec!["f1".into()],
            display_target: None,
            attachments: vec![],
            follow_up_id: None,
            delegation_id: None,
        }));

        let envelope = tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(envelope.task, "do thing");
        assert_eq!(envelope.reference_frame_ids, vec!["f1".to_string()]);
        assert!(!envelope.force_direct);

        // Presence and follow_up NOT consulted for metadata tasks
        assert!(presence_rx.try_recv().is_err());
        assert!(follow_up_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn start_task_non_direct_with_presence_routes_to_presence() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: None,
            task: "chat with me".into(),
            orchestrate: None,
            direct: None,
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
            follow_up_id: None,
            delegation_id: None,
        }));

        let text = tokio::time::timeout(std::time::Duration::from_millis(200), presence_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "chat with me");
        assert!(task_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn start_task_direct_bypasses_presence() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: None,
            task: "code thing".into(),
            orchestrate: None,
            direct: Some(true),
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
            follow_up_id: None,
            delegation_id: None,
        }));

        let envelope = tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(envelope.task, "code thing");
        assert!(envelope.force_direct);
        assert!(presence_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn follow_up_direct_to_task_tx() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, _presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::FollowUp {
            session_id: None,
            text: "more please".into(),
            direct: Some(true),
            follow_up_id: None,
        }));

        let envelope = tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(envelope.task, "more please");
        assert!(envelope.force_direct);
    }

    #[tokio::test]
    async fn follow_up_non_presence_goes_to_follow_up_tx() {
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel::<FollowUpMessage>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: Some(follow_up_tx),
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::FollowUp {
            session_id: None,
            text: "keep going".into(),
            direct: None,
            follow_up_id: None,
        }));

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), follow_up_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.text, "keep going");
        assert!(msg.attachments.is_empty());
    }

    #[tokio::test]
    async fn targeted_task_for_non_primary_session_is_left_to_supervisor() {
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel::<FollowUpMessage>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: Some(follow_up_tx),
            primary_session_id: Some("primary".to_string()),
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: Some("external".into()),
            task: "continue there".into(),
            orchestrate: None,
            direct: Some(true),
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
            follow_up_id: None,
            delegation_id: None,
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(follow_up_rx.try_recv().is_err());
    }

    /// External backends re-key their session to the backend-native id
    /// mid-flight; frontends then target that id. The dispatcher must treat
    /// every id in the primary session's identity group as primary — an
    /// exact match silently dropped every post-upgrade dashboard command.
    #[tokio::test]
    async fn native_id_alias_joins_primary_after_identity_upgrade() {
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel::<FollowUpMessage>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: Some(follow_up_tx),
            primary_session_id: Some("wrapper-id".to_string()),
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::SessionIdentity {
            session_id: "wrapper-id".into(),
            source: "claude-code".into(),
            backend_session_id: "native-id".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let start_task = |session_id: &str, task: &str| {
            AppEvent::ControlCommand(ControlMsg::StartTask {
                session_id: Some(session_id.into()),
                task: task.into(),
                orchestrate: None,
                direct: Some(true),
                reference_frame_ids: vec![],
                display_target: None,
                attachments: vec![],
                follow_up_id: None,
                delegation_id: None,
            })
        };

        bus.send(start_task("native-id", "via native id"));
        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), follow_up_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.text, "via native id");

        // A foreign session's native id must not leak into the group.
        bus.send(AppEvent::SessionIdentity {
            session_id: "other-wrapper".into(),
            source: "codex".into(),
            backend_session_id: "other-native".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        bus.send(start_task("other-native", "foreign session"));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(follow_up_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn orchestrate_false_implies_direct() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: None,
            task: "legacy direct".into(),
            orchestrate: Some(false),
            direct: None,
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
            follow_up_id: None,
            delegation_id: None,
        }));

        let envelope = tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(envelope.force_direct);
        assert!(presence_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_task_control_messages_ignored() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: Some(task_tx),
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Status {
            session_id: None,
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(task_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn interrupt_emits_interrupt_requested() {
        let bus = make_test_bus();
        let mut rx = bus.subscribe();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Interrupt {
            session_id: Some("sess-a".into()),
            expected_turn: None,
        }));

        // Drain events until we see an InterruptRequested, or time out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut saw_interrupt_requested = false;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::InterruptRequested { session_id })) => {
                    assert_eq!(session_id.as_deref(), Some("sess-a"));
                    saw_interrupt_requested = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(
            saw_interrupt_requested,
            "expected AppEvent::InterruptRequested to be emitted"
        );
    }

    #[tokio::test]
    async fn steer_emits_steer_requested_with_id() {
        // The dispatcher re-emits `ControlMsg::Steer` as
        // `AppEvent::SteerRequested`, defaulting a missing id to "" so
        // downstream consumers never have to handle an Option.
        let bus = make_test_bus();
        let mut rx = bus.subscribe();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Steer {
            session_id: Some("sess-b".into()),
            text: "use SQLite instead".into(),
            attachments: vec![],
            id: Some("s1".into()),
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut seen: Option<(Option<String>, String, String)> = None;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SteerRequested {
                    session_id,
                    text,
                    id,
                })) => {
                    seen = Some((session_id, text, id));
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        let (session_id, text, id) = seen.expect("expected AppEvent::SteerRequested");
        assert_eq!(session_id.as_deref(), Some("sess-b"));
        assert_eq!(text, "use SQLite instead");
        assert_eq!(id, "s1");
    }

    #[tokio::test]
    async fn steer_without_id_defaults_to_empty_string() {
        let bus = make_test_bus();
        let mut rx = bus.subscribe();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Steer {
            session_id: None,
            text: "never mind".into(),
            attachments: vec![],
            id: None,
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut seen_id: Option<String> = None;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SteerRequested { id, .. })) => {
                    seen_id = Some(id);
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert_eq!(seen_id.as_deref(), Some(""));
    }

    /// External `--agent` shape: no task channel, but the follow-up lane is
    /// real (run_external_agent_mode consumes it) — the attachment steer
    /// rides it with its steer id and unresolved frame ids intact.
    #[tokio::test]
    async fn steer_with_attachments_falls_back_to_follow_up_lane() {
        let bus = make_test_bus();
        let mut rx = bus.subscribe();
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel::<FollowUpMessage>(4);

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: Some(follow_up_tx),
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Steer {
            session_id: Some("ext-sess".into()),
            text: "see the attached frame".into(),
            attachments: vec!["frame:latest".into()],
            id: Some("s3".into()),
        }));

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), follow_up_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.text, "see the attached frame");
        assert_eq!(msg.steer_id.as_deref(), Some("s3"));
        assert_eq!(msg.unresolved_attachment_ids, vec!["frame:latest"]);
        assert_eq!(msg.target_session_id.as_deref(), Some("ext-sess"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut saw_queued = false;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SteerQueued { id, .. })) => {
                    assert_eq!(id, "s3");
                    saw_queued = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(saw_queued, "expected SteerQueued for follow-up-lane steer");
    }

    /// Presence shape: the headless wiring hands the dispatcher NO
    /// follow_up_tx (nothing reads that channel under run_with_presence), so
    /// when the task lane is unavailable the steer reaches the explicit
    /// warn+drop — never a phantom `SteerQueued` receipt for a message
    /// sitting in a channel nothing drains.
    #[tokio::test]
    async fn steer_with_attachments_without_consumer_drops_honestly() {
        let bus = make_test_bus();
        let mut rx = bus.subscribe();
        // Task lane exists but is FULL (capacity 1, pre-filled) — the
        // presence loop is busy and try_send fails.
        let (task_tx, _task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(1);
        task_tx
            .try_send(presence_core::TaskEnvelope {
                task: "occupies the only slot".into(),
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            })
            .unwrap();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: Some(task_tx),
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Steer {
            session_id: None,
            text: "attachment steer with nowhere to go".into(),
            attachments: vec!["frame:latest".into()],
            id: Some("s4".into()),
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut saw_drop_warning = false;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SteerQueued { .. })) => {
                    panic!("phantom SteerQueued for an undeliverable steer");
                }
                Ok(Ok(AppEvent::LogEntry { content, level, .. }))
                    if level == "warn" && content.contains("Steer dropped") =>
                {
                    saw_drop_warning = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(saw_drop_warning, "expected the explicit warn+drop");
    }

    #[tokio::test]
    async fn steer_with_attachments_routes_task_envelope() {
        let bus = make_test_bus();
        let mut rx = bus.subscribe();
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: Some(task_tx),
            follow_up_tx: None,
            primary_session_id: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Steer {
            session_id: Some("sess-c".into()),
            text: "look at this screenshot".into(),
            attachments: vec!["frame:latest".into()],
            id: Some("s2".into()),
        }));

        let envelope = tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(envelope.task, "look at this screenshot");
        assert!(envelope.force_direct);
        assert_eq!(envelope.attachment_frame_ids, vec!["frame:latest"]);
        assert_eq!(envelope.steer_id.as_deref(), Some("s2"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut saw_queued = false;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SteerQueued { id, .. })) => {
                    assert_eq!(id, "s2");
                    saw_queued = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(saw_queued, "expected SteerQueued for attached steer");
    }
}
