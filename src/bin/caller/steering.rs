//! External-agent steering: resolving steer targets (primary vs side
//! threads), the pending/queued runtime-steer bookkeeping, and draining
//! queued steers into follow-up messages.

use crate::error::CallerError;
use crate::event::{self, AppEvent, EventBus};
use crate::{event_targets_session_or_alias, slog, DrainConfig};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExternalSteerTargetKind {
    Primary,
    Side,
}

pub(crate) fn resolve_external_steer_target_session(
    target: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
    side_threads: Option<&HashMap<String, String>>,
) -> Option<(Option<String>, ExternalSteerTargetKind)> {
    match target.as_deref() {
        Some(target) if side_threads.is_some_and(|threads| threads.contains_key(target)) => {
            Some((Some(target.to_string()), ExternalSteerTargetKind::Side))
        }
        Some(_) if event_targets_session_or_alias(target, session_id, alias_session_id) => {
            Some((session_id.clone(), ExternalSteerTargetKind::Primary))
        }
        Some(_) => None,
        None => Some((session_id.clone(), ExternalSteerTargetKind::Primary)),
    }
}

pub(crate) fn external_steer_queue_reason(agent_name: &str, err: &CallerError) -> String {
    let unsupported = match err {
        CallerError::ExternalAgent(message) => {
            message.contains("mid-turn steering not supported")
                || message.contains("steering not supported")
        }
        _ => false,
    };
    if unsupported {
        // The expected path for backends without mid-turn injection
        // (Claude Code) — say what happens next, not why it "failed".
        format!("{agent_name} applies messages between turns — delivers when this turn ends")
    } else {
        format!("{agent_name} native mid-turn steering failed ({err}); queued as follow-up")
    }
}

pub(crate) fn external_steer_error_is_no_active_turn(err: &CallerError) -> bool {
    match err {
        CallerError::ExternalAgent(message) => message.contains("no active turn"),
        _ => false,
    }
}

pub(crate) fn external_steer_targets_idle_side_thread(
    target_kind: ExternalSteerTargetKind,
    target_session_id: Option<&str>,
    active_side_turns: &HashSet<String>,
) -> bool {
    target_kind == ExternalSteerTargetKind::Side
        && target_session_id.is_some_and(|id| !active_side_turns.contains(id))
}

pub(crate) struct PendingRuntimeSteer {
    pub(crate) session_id: Option<String>,
    pub(crate) id: String,
    pub(crate) text: String,
}

pub(crate) fn steer_id_has_been_handled(
    handled_steer_ids: &std::collections::HashSet<String>,
    id: &str,
) -> bool {
    !id.trim().is_empty() && handled_steer_ids.contains(id)
}

pub(crate) fn mark_steer_id_handled(
    handled_steer_ids: &mut std::collections::HashSet<String>,
    id: &str,
) {
    if !id.trim().is_empty() {
        handled_steer_ids.insert(id.to_string());
    }
}

pub(crate) fn pending_runtime_steer_targets_session(
    pending: &PendingRuntimeSteer,
    session_id: &Option<String>,
) -> bool {
    pending.session_id.as_deref() == session_id.as_deref()
}

pub(crate) fn queued_steer_targets_session(
    injection: &event::ContextInjection,
    session_id: Option<&str>,
    alias_session_id: Option<&str>,
) -> bool {
    if injection.steer_id.is_none() {
        return false;
    }
    let Some(target) = injection
        .target_session_id
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return true;
    };
    session_id == Some(target) || alias_session_id == Some(target)
}

pub(crate) fn has_queued_steers_for_session(
    context_injection: &event::ContextInjectionQueue,
    session_id: Option<&str>,
    alias_session_id: Option<&str>,
) -> bool {
    context_injection
        .lock()
        .map(|queue| {
            queue.iter().any(|injection| {
                queued_steer_targets_session(injection, session_id, alias_session_id)
            })
        })
        .unwrap_or(false)
}

pub(crate) fn queued_steer_matches_cancel(
    injection: &event::ContextInjection,
    session_id: Option<&str>,
    alias_session_id: Option<&str>,
    steer_id: Option<&str>,
) -> bool {
    if !queued_steer_targets_session(injection, session_id, alias_session_id) {
        return false;
    }
    match steer_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(steer_id) => injection.steer_id.as_deref() == Some(steer_id),
        None => true,
    }
}

pub(crate) fn cancel_queued_steers_for_session(
    context_injection: &event::ContextInjectionQueue,
    bus: &EventBus,
    session_id: Option<&str>,
    alias_session_id: Option<&str>,
    steer_id: Option<&str>,
    reason: &str,
) -> usize {
    let mut cancelled = 0usize;
    if let Ok(mut q) = context_injection.lock() {
        let mut kept = Vec::with_capacity(q.len());
        for inj in q.drain(..) {
            if queued_steer_matches_cancel(&inj, session_id, alias_session_id, steer_id) {
                cancelled += 1;
                bus.send(AppEvent::SteerCancelled {
                    session_id: inj
                        .target_session_id
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| session_id.map(str::to_string)),
                    id: inj.steer_id.clone().unwrap_or_default(),
                    reason: reason.to_string(),
                });
            } else {
                kept.push(inj);
            }
        }
        *q = kept;
    }
    cancelled
}

pub(crate) fn flush_pending_runtime_steers_for_session(
    bus: &EventBus,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    session_id: &Option<String>,
) -> usize {
    let mut delivered = 0usize;
    let mut retained = std::collections::VecDeque::with_capacity(pending_runtime_steers.len());
    while let Some(pending) = pending_runtime_steers.pop_front() {
        if pending_runtime_steer_targets_session(&pending, session_id) {
            delivered += 1;
            bus.send(AppEvent::SteerDelivered {
                session_id: pending.session_id,
                id: pending.id,
                mid_turn: true,
            });
        } else {
            retained.push_back(pending);
        }
    }
    *pending_runtime_steers = retained;
    delivered
}

pub(crate) fn cancel_pending_runtime_steers_for_session(
    bus: &EventBus,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    session_id: Option<&str>,
    alias_session_id: Option<&str>,
    steer_id: Option<&str>,
    reason: &str,
) -> usize {
    let mut cancelled = 0usize;
    let mut retained = std::collections::VecDeque::with_capacity(pending_runtime_steers.len());
    while let Some(pending) = pending_runtime_steers.pop_front() {
        let target_matches = match session_id {
            Some(session_id) => {
                pending.session_id.as_deref() == Some(session_id)
                    || pending.session_id.as_deref() == alias_session_id
            }
            None => true,
        };
        let id_matches = match steer_id.map(str::trim).filter(|id| !id.is_empty()) {
            Some(steer_id) => pending.id == steer_id,
            None => true,
        };
        if target_matches && id_matches {
            cancelled += 1;
            bus.send(AppEvent::SteerCancelled {
                session_id: pending.session_id,
                id: pending.id,
                reason: reason.to_string(),
            });
        } else {
            retained.push_back(pending);
        }
    }
    *pending_runtime_steers = retained;
    cancelled
}

pub(crate) fn flush_pending_runtime_steers_for_model_checkpoint(
    bus: &EventBus,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    session_id: &Option<String>,
) -> usize {
    flush_pending_runtime_steers_for_session(bus, pending_runtime_steers, session_id)
}

pub(crate) fn mark_pending_runtime_steers_delivered_at_model_checkpoint(
    config: &DrainConfig<'_>,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    agent_name: &str,
) {
    // Codex records turn/steer input in its conversation log, but does not
    // reliably echo it back through the app-server stream as a userMessage.
    // Once the model produces new output, the runtime checkpoint happened and
    // the accepted steer should no longer stay pinned in the dashboard queue.
    let delivered = flush_pending_runtime_steers_for_model_checkpoint(
        config.bus,
        pending_runtime_steers,
        &config.session_id,
    );
    if delivered > 0 {
        slog(config.session_log, |l| {
            l.info(&format!(
                "Marked {} accepted {} steer(s) delivered at model checkpoint",
                delivered, agent_name
            ))
        });
    }
}

/// Drain queued steer items from `context_injection` and merge them into a
/// follow-up user message bound for an external agent.
///
/// Only drains items whose `steer_id` is `Some(_)` — those are the entries
/// that the steer fallback path pushed. Other queue sources (display
/// takeover, presence annotations) are left in place for the native
/// drain-between-turns path used by the internal agent loop.
///
/// For each drained item, emits `AppEvent::SteerDelivered { mid_turn: false }`
/// so the dashboard can retire its pending-steer UI row. The returned
/// string interleaves queued steers (prefixed with `[User]`) above the
/// caller's `followup` text — the result is sent as a single external agent
/// message so the agent sees both in the same turn's input.
///
/// When `followup` is empty and queued steer text exists, the queued steer
/// text becomes the whole follow-up. When both are empty, the return is
/// `None`, meaning "nothing to send".
pub(crate) fn drain_steer_queue_as_followup(
    context_injection: &event::ContextInjectionQueue,
    followup: &str,
    bus: &EventBus,
    session_id: Option<&str>,
    alias_session_id: Option<&str>,
) -> Option<String> {
    let mut prefix_lines: Vec<String> = Vec::new();
    if let Ok(mut q) = context_injection.lock() {
        // Partition: keep non-steer entries and steers for other sessions,
        // pull out steer entries for this session.
        let mut kept = Vec::with_capacity(q.len());
        for inj in q.drain(..) {
            if queued_steer_targets_session(&inj, session_id, alias_session_id) {
                prefix_lines.push(format!("[User] {}", inj.text));
                let id = inj.steer_id.clone().unwrap_or_default();
                bus.send(AppEvent::SteerDelivered {
                    session_id: inj
                        .target_session_id
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| session_id.map(str::to_string)),
                    id,
                    mid_turn: false,
                });
            } else {
                kept.push(inj);
            }
        }
        *q = kept;
    }
    if prefix_lines.is_empty() && followup.is_empty() {
        return None;
    }
    if prefix_lines.is_empty() {
        Some(followup.to_string())
    } else if followup.is_empty() {
        Some(prefix_lines.join("\n"))
    } else {
        Some(format!("{}\n{}", prefix_lines.join("\n"), followup))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    // ── Steer fallback plumbing ──
    //
    // The full `drain_external_agent_events` loop is integration-heavy
    // (needs a backend + event channel); we unit-test the smaller helpers
    // that encapsulate the fallback policy. The end-to-end flow is covered
    // indirectly by the dispatcher / Codex tests.

    #[test]
    fn resolve_external_steer_target_prefers_side_thread_id() {
        let mut side_threads = std::collections::HashMap::new();
        side_threads.insert("side-thread".to_string(), "parent-thread".to_string());

        let resolved = resolve_external_steer_target_session(
            &Some("side-thread".to_string()),
            &Some("parent-thread".to_string()),
            &Some("alias-thread".to_string()),
            Some(&side_threads),
        );

        assert_eq!(
            resolved,
            Some((
                Some("side-thread".to_string()),
                ExternalSteerTargetKind::Side
            ))
        );
    }

    #[test]
    fn resolve_external_steer_target_maps_parent_alias_to_primary_session() {
        let side_threads = std::collections::HashMap::new();

        let alias = resolve_external_steer_target_session(
            &Some("alias-thread".to_string()),
            &Some("parent-thread".to_string()),
            &Some("alias-thread".to_string()),
            Some(&side_threads),
        );
        assert_eq!(
            alias,
            Some((
                Some("parent-thread".to_string()),
                ExternalSteerTargetKind::Primary
            ))
        );

        let untargeted = resolve_external_steer_target_session(
            &None,
            &Some("parent-thread".to_string()),
            &Some("alias-thread".to_string()),
            Some(&side_threads),
        );
        assert_eq!(
            untargeted,
            Some((
                Some("parent-thread".to_string()),
                ExternalSteerTargetKind::Primary
            ))
        );
    }

    #[test]
    fn resolve_external_steer_target_rejects_unrelated_session() {
        let mut side_threads = std::collections::HashMap::new();
        side_threads.insert("side-thread".to_string(), "parent-thread".to_string());

        assert_eq!(
            resolve_external_steer_target_session(
                &Some("other-thread".to_string()),
                &Some("parent-thread".to_string()),
                &Some("alias-thread".to_string()),
                Some(&side_threads),
            ),
            None
        );
    }

    #[test]
    fn external_steer_queue_reason_keeps_unsupported_wording() {
        let reason = external_steer_queue_reason(
            "Claude Code",
            &CallerError::ExternalAgent(
                "mid-turn steering not supported by this backend".to_string(),
            ),
        );

        // The unsupported path is the EXPECTED path for turn-boundary
        // backends: plain what-happens-next copy, no failure vocabulary.
        assert!(reason.contains("delivers when this turn ends"), "{reason}");
        assert!(!reason.to_lowercase().contains("failed"), "{reason}");
    }

    #[test]
    fn external_steer_queue_reason_identifies_native_failure() {
        let reason = external_steer_queue_reason(
            "Codex",
            &CallerError::ExternalAgent(
                "JSON-RPC error -32600: expected active turn id `turn-a` but found `turn-b`"
                    .to_string(),
            ),
        );

        assert!(reason.contains("Codex native mid-turn steering failed"));
        assert!(!reason.contains("doesn't support"));
        assert!(reason.contains("queued as follow-up"));
    }

    #[test]
    fn external_steer_error_identifies_no_active_turn() {
        assert!(external_steer_error_is_no_active_turn(
            &CallerError::ExternalAgent("no active turn to steer".to_string())
        ));
        assert!(!external_steer_error_is_no_active_turn(
            &CallerError::ExternalAgent(
                "JSON-RPC error -32600: expected active turn id `turn-a` but found `turn-b`"
                    .to_string(),
            )
        ));
    }

    #[test]
    fn external_steer_targets_idle_side_thread_only_when_side_is_not_active() {
        let active_side_turns = std::collections::HashSet::from(["active-side".to_string()]);

        assert!(external_steer_targets_idle_side_thread(
            ExternalSteerTargetKind::Side,
            Some("idle-side"),
            &active_side_turns,
        ));
        assert!(!external_steer_targets_idle_side_thread(
            ExternalSteerTargetKind::Side,
            Some("active-side"),
            &active_side_turns,
        ));
        assert!(!external_steer_targets_idle_side_thread(
            ExternalSteerTargetKind::Primary,
            Some("parent"),
            &active_side_turns,
        ));
        assert!(!external_steer_targets_idle_side_thread(
            ExternalSteerTargetKind::Side,
            None,
            &active_side_turns,
        ));
    }

    #[test]
    fn claim_active_side_turn_completion_is_idempotent() {
        let mut active_side_turns = std::collections::HashSet::from(["side-thread".to_string()]);

        assert!(claim_active_side_turn_completion(
            &mut active_side_turns,
            Some("side-thread")
        ));
        assert!(!claim_active_side_turn_completion(
            &mut active_side_turns,
            Some("side-thread")
        ));
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_prefixes_single_queued_item() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let queue = event::ContextInjectionQueue::default();
        queue
            .lock()
            .unwrap()
            .push(event::ContextInjection::text_with_steer_id(
                "switch to Python".into(),
                "steer-1".into(),
            ));

        let merged = drain_steer_queue_as_followup(&queue, "original follow-up", &bus, None, None)
            .expect("should produce a message");

        assert_eq!(merged, "[User] switch to Python\noriginal follow-up");
        // Queue drained.
        assert!(queue.lock().unwrap().is_empty());

        // SteerDelivered emitted for the drained item.
        let ev = rx.try_recv().expect("SteerDelivered event");
        match ev {
            AppEvent::SteerDelivered { id, mid_turn, .. } => {
                assert_eq!(id, "steer-1");
                assert!(!mid_turn, "queued fallback should report mid_turn=false");
            }
            other => panic!("expected SteerDelivered, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_ignores_non_steer_entries() {
        // A ContextInjection without a steer_id (e.g. from display
        // takeover) must be left alone — the native agent loop still owns
        // draining those.
        let bus = EventBus::new();
        let queue = event::ContextInjectionQueue::default();
        queue
            .lock()
            .unwrap()
            .push(event::ContextInjection::text("display grant".into()));

        let merged = drain_steer_queue_as_followup(&queue, "follow-up", &bus, None, None)
            .expect("should produce a message");
        assert_eq!(merged, "follow-up");
        assert_eq!(queue.lock().unwrap().len(), 1, "non-steer entry preserved");
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_no_queue_no_followup_is_none() {
        // Empty queue + empty followup => Some(None) (caller will skip the
        // send). Verifies the "steer only + empty follow-up" degenerate
        // case doesn't produce an empty agent message.
        let bus = EventBus::new();
        let queue = event::ContextInjectionQueue::default();
        assert!(drain_steer_queue_as_followup(&queue, "", &bus, None, None).is_none());
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_combines_multiple_items() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let queue = event::ContextInjectionQueue::default();
        {
            let mut q = queue.lock().unwrap();
            q.push(event::ContextInjection::text_with_steer_id(
                "first".into(),
                "s1".into(),
            ));
            q.push(event::ContextInjection::text_with_steer_id(
                "second".into(),
                "s2".into(),
            ));
        }

        let merged =
            drain_steer_queue_as_followup(&queue, "main", &bus, None, None).expect("merged");
        assert_eq!(merged, "[User] first\n[User] second\nmain");

        let mut delivered_ids: Vec<String> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::SteerDelivered { id, mid_turn, .. } = ev {
                assert!(!mid_turn);
                delivered_ids.push(id);
            }
        }
        assert_eq!(delivered_ids, vec!["s1".to_string(), "s2".to_string()]);
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_keeps_other_session_items() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let queue = event::ContextInjectionQueue::default();
        {
            let mut q = queue.lock().unwrap();
            q.push(event::ContextInjection::text_with_steer_id_for_target(
                "for session a".into(),
                "s-a".into(),
                Some("session-a".into()),
            ));
            q.push(event::ContextInjection::text_with_steer_id_for_target(
                "for session b".into(),
                "s-b".into(),
                Some("session-b".into()),
            ));
        }

        let merged = drain_steer_queue_as_followup(&queue, "main", &bus, Some("session-b"), None)
            .expect("merged");
        assert_eq!(merged, "[User] for session b\nmain");

        let remaining = queue.lock().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "for session a");
        assert_eq!(remaining[0].target_session_id.as_deref(), Some("session-a"));
        drop(remaining);

        let ev = rx.try_recv().expect("SteerDelivered event");
        match ev {
            AppEvent::SteerDelivered {
                session_id,
                id,
                mid_turn,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-b"));
                assert_eq!(id, "s-b");
                assert!(!mid_turn);
            }
            other => panic!("expected SteerDelivered, got {:?}", other),
        }
        assert!(rx.try_recv().is_err(), "only matching steer should drain");
    }

    #[test]
    fn has_queued_steers_for_session_matches_target_or_alias() {
        let queue = event::ContextInjectionQueue::default();
        queue
            .lock()
            .unwrap()
            .push(event::ContextInjection::text_with_steer_id_for_target(
                "for alias".into(),
                "s-alias".into(),
                Some("alias-session".into()),
            ));

        assert!(has_queued_steers_for_session(
            &queue,
            Some("live-session"),
            Some("alias-session")
        ));
        assert!(!has_queued_steers_for_session(
            &queue,
            Some("other-session"),
            None
        ));
    }

    #[test]
    fn cancel_queued_steers_removes_only_matching_session_and_id() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let queue = event::ContextInjectionQueue::default();
        {
            let mut q = queue.lock().unwrap();
            q.push(event::ContextInjection::text_with_steer_id_for_target(
                "cancel this".into(),
                "steer-a".into(),
                Some("session-a".into()),
            ));
            q.push(event::ContextInjection::text_with_steer_id_for_target(
                "keep this".into(),
                "steer-b".into(),
                Some("session-b".into()),
            ));
            q.push(event::ContextInjection::text("display grant".into()));
        }

        let cancelled = cancel_queued_steers_for_session(
            &queue,
            &bus,
            Some("session-a"),
            None,
            Some("steer-a"),
            "cleared by user",
        );

        assert_eq!(cancelled, 1);
        let remaining = queue.lock().unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining
            .iter()
            .any(|inj| inj.steer_id.as_deref() == Some("steer-b")));
        assert!(remaining.iter().any(|inj| inj.steer_id.is_none()));
        drop(remaining);

        let ev = rx.try_recv().expect("SteerCancelled event");
        match ev {
            AppEvent::SteerCancelled {
                session_id,
                id,
                reason,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-a"));
                assert_eq!(id, "steer-a");
                assert_eq!(reason, "cleared by user");
            }
            other => panic!("expected SteerCancelled, got {:?}", other),
        }
        assert!(rx.try_recv().is_err(), "only one steer should cancel");
    }

    #[test]
    fn flush_pending_runtime_steers_delivers_only_matching_session() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut pending = std::collections::VecDeque::from([
            PendingRuntimeSteer {
                session_id: Some("parent".to_string()),
                id: "steer-parent".to_string(),
                text: "parent steer".to_string(),
            },
            PendingRuntimeSteer {
                session_id: Some("side".to_string()),
                id: "steer-side".to_string(),
                text: "side steer".to_string(),
            },
        ]);

        let delivered =
            flush_pending_runtime_steers_for_session(&bus, &mut pending, &Some("side".into()));

        assert_eq!(delivered, 1);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "steer-parent");

        let ev = rx.try_recv().expect("SteerDelivered event");
        match ev {
            AppEvent::SteerDelivered {
                session_id,
                id,
                mid_turn,
            } => {
                assert_eq!(session_id.as_deref(), Some("side"));
                assert_eq!(id, "steer-side");
                assert!(mid_turn);
            }
            other => panic!("expected SteerDelivered, got {:?}", other),
        }
    }

    #[test]
    fn cancel_pending_runtime_steers_removes_only_matching_id() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut pending = std::collections::VecDeque::from([
            PendingRuntimeSteer {
                session_id: Some("parent".to_string()),
                id: "keep".to_string(),
                text: "parent steer".to_string(),
            },
            PendingRuntimeSteer {
                session_id: Some("parent".to_string()),
                id: "cancel".to_string(),
                text: "cancel this".to_string(),
            },
            PendingRuntimeSteer {
                session_id: Some("side".to_string()),
                id: "cancel".to_string(),
                text: "same id, other session".to_string(),
            },
        ]);

        let cancelled = cancel_pending_runtime_steers_for_session(
            &bus,
            &mut pending,
            Some("parent"),
            None,
            Some("cancel"),
            "cleared by user",
        );

        assert_eq!(cancelled, 1);
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|item| item.id == "keep"));
        assert!(pending
            .iter()
            .any(|item| item.session_id.as_deref() == Some("side")));

        let ev = rx.try_recv().expect("SteerCancelled event");
        match ev {
            AppEvent::SteerCancelled {
                session_id,
                id,
                reason,
            } => {
                assert_eq!(session_id.as_deref(), Some("parent"));
                assert_eq!(id, "cancel");
                assert_eq!(reason, "cleared by user");
            }
            other => panic!("expected SteerCancelled, got {:?}", other),
        }
    }

    #[test]
    fn model_checkpoint_flushes_pending_runtime_steers() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut pending = std::collections::VecDeque::from([
            PendingRuntimeSteer {
                session_id: Some("codex-thread".to_string()),
                id: "steer-1".to_string(),
                text: "first steer".to_string(),
            },
            PendingRuntimeSteer {
                session_id: Some("codex-thread".to_string()),
                id: "steer-2".to_string(),
                text: "second steer".to_string(),
            },
        ]);

        let delivered = flush_pending_runtime_steers_for_model_checkpoint(
            &bus,
            &mut pending,
            &Some("codex-thread".into()),
        );

        assert_eq!(delivered, 2);
        assert!(pending.is_empty());

        let mut ids = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::SteerDelivered {
                    session_id,
                    id,
                    mid_turn,
                } => {
                    assert_eq!(session_id.as_deref(), Some("codex-thread"));
                    assert!(mid_turn);
                    ids.push(id);
                }
                other => panic!("expected SteerDelivered, got {:?}", other),
            }
        }
        assert_eq!(ids, vec!["steer-1".to_string(), "steer-2".to_string()]);
    }
}
