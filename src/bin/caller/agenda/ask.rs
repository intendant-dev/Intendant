//! Agenda-backed asks: the durable sibling of the blocking `ask_user`
//! waiter (ask↔agenda unification, slice 1).
//!
//! A parked rich question surfaces on every dashboard's question rail via
//! the exact `UserQuestionRequired` pipeline live asks use — but nothing
//! blocks on it, so nothing holds the ask id. Resolution therefore lives
//! in the daemon: [`spawn_ask_resolver`] subscribes to the event bus (the
//! same uniform `ControlCommand` lane the MCP waiter matches) and, when an
//! `AnswerQuestion` names an open agenda-backed ask, records the
//! structured answer on the item (completing it); skip/deny/approve are
//! dismissals — a marker in the log, the item stays OPEN. Both paths end
//! in `ApprovalResolved` so every connected rail clears.
//!
//! [`agenda_ask_pending`] is the advisory registry the session
//! supervisor's approval routing consults (exactly like
//! `mcp::ask_user_question_pending`) so an agenda ask id is never
//! misreported as an unknown approval.

use super::handle::AgendaHandle;
use super::types::{AgendaAskResolution, AgendaStatus};
use crate::event::{AppEvent, ControlMsg};
use std::collections::HashMap;
use std::sync::Arc;

/// Open agenda-backed asks: ask id → item id. Process-global for the same
/// reason `mcp::pending_asks` is — the supervisor consult has no route to
/// the handle — and reconciled from the fold after every store mutation,
/// so concurrent daemons on one home converge on each other's parks.
fn open_asks() -> &'static std::sync::Mutex<HashMap<u64, String>> {
    static OPEN: std::sync::OnceLock<std::sync::Mutex<HashMap<u64, String>>> =
        std::sync::OnceLock::new();
    OPEN.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Whether `id` is an open agenda-backed question. Consulted by the
/// session supervisor before warning about an approval id it does not
/// know: the agenda resolver owns it and emits `ApprovalResolved`.
pub(crate) fn agenda_ask_pending(id: u64) -> bool {
    open_asks()
        .lock()
        .map(|map| map.contains_key(&id))
        .unwrap_or(false)
}

/// Reconcile the registry against one store's folded items: every
/// ask-backed item is (de)registered per its status. Ids belonging to
/// items this fold does not know are never touched (ask ids are unique
/// process-wide, so stores on different homes cannot collide).
pub(crate) fn sync_open_asks<'a>(items: impl Iterator<Item = &'a super::types::AgendaItem>) {
    let Ok(mut map) = open_asks().lock() else {
        return;
    };
    for item in items {
        let Some(ask) = &item.ask else { continue };
        if item.status == AgendaStatus::Open {
            map.insert(ask.ask_id, item.id.clone());
        } else {
            map.remove(&ask.ask_id);
        }
    }
}

/// The joined human-readable summary recorded as `AgendaAnswer.text`,
/// built in the ITEM's question order (the maps key by question text).
/// Single answered question → the bare answer; otherwise one line per
/// engaged question, follow-ups and annotation counts included so a
/// text-only surface still sees everything that arrived.
pub(crate) fn answer_summary(
    questions: &[crate::types::UserQuestion],
    resolution: &AgendaAskResolution,
) -> String {
    let name = |q: &crate::types::UserQuestion| {
        if q.header.is_empty() {
            q.question.clone()
        } else {
            q.header.clone()
        }
    };
    let engaged: Vec<&crate::types::UserQuestion> = questions
        .iter()
        .filter(|q| {
            resolution.answers.contains_key(&q.question)
                || resolution.followups.contains_key(&q.question)
                || resolution.annotations.contains_key(&q.question)
        })
        .collect();
    if let [only] = engaged.as_slice() {
        if let Some(answer) = resolution.answers.get(&only.question) {
            if !resolution.followups.contains_key(&only.question)
                && !resolution.annotations.contains_key(&only.question)
            {
                return answer.clone();
            }
        }
    }
    let mut lines = Vec::new();
    for q in engaged {
        let label = name(q);
        if let Some(answer) = resolution.answers.get(&q.question) {
            lines.push(format!("{label}: {answer}"));
        }
        if let Some(followup) = resolution.followups.get(&q.question) {
            lines.push(format!("follow-up ({label}): {followup}"));
        }
        if let Some(notes) = resolution.annotations.get(&q.question) {
            for note in notes {
                lines.push(format!("note on {} ({label}): {}", note.preview, note.note));
            }
        }
    }
    lines.join("\n")
}

/// Map the `AnswerQuestion` wire maps into the durable resolution shape
/// (BTreeMaps for byte-deterministic log lines).
pub(crate) fn resolution_from_wire(
    answers: HashMap<String, String>,
    selections: HashMap<String, Vec<String>>,
    followups: HashMap<String, String>,
    annotations: HashMap<String, Vec<crate::types::QuestionAnnotation>>,
) -> AgendaAskResolution {
    AgendaAskResolution {
        answers: answers.into_iter().collect(),
        selections: selections.into_iter().collect(),
        followups: followups.into_iter().collect(),
        annotations: annotations.into_iter().collect(),
    }
}

/// Spawn the daemon-wide resolver for agenda-backed asks. One per daemon,
/// next to the reminder scheduler; detaches on drop like the mode
/// listeners.
pub(crate) fn spawn_ask_resolver(handle: Arc<AgendaHandle>) -> tokio::task::JoinHandle<()> {
    let mut events = handle.bus().subscribe();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let AppEvent::ControlCommand(msg) = event else {
                continue;
            };
            match msg {
                ControlMsg::AnswerQuestion {
                    id,
                    answers,
                    selections,
                    followups,
                    annotations,
                    ..
                } if agenda_ask_pending(id) => {
                    let resolution =
                        resolution_from_wire(answers, selections, followups, annotations);
                    if resolution.is_empty() {
                        // Nothing to record: leave the rail card standing
                        // rather than completing an item with no content.
                        eprintln!("[agenda] empty answer for ask {id} ignored");
                        continue;
                    }
                    if let Err(err) = handle.answer_ask(id, resolution) {
                        eprintln!("[agenda] recording answer for ask {id}: {err}");
                    }
                }
                // Dismissal verbs. Approve/ApproveAll come from callers
                // that only speak the approval vocabulary — like the MCP
                // waiter's "pass", they carry no choice: record a
                // dismissal and clear the rails; the item stays open.
                ControlMsg::Skip { id, .. } if agenda_ask_pending(id) => {
                    dismiss(&handle, id, "skip");
                }
                ControlMsg::Deny { id, .. } if agenda_ask_pending(id) => {
                    dismiss(&handle, id, "deny");
                }
                ControlMsg::Approve { id, .. } if agenda_ask_pending(id) => {
                    dismiss(&handle, id, "approve");
                }
                ControlMsg::ApproveAll { id, .. } if agenda_ask_pending(id) => {
                    dismiss(&handle, id, "approve_all");
                }
                _ => {}
            }
        }
    })
}

fn dismiss(handle: &AgendaHandle, ask_id: u64, action: &str) {
    if let Err(err) = handle.dismiss_ask(ask_id, action) {
        eprintln!("[agenda] recording dismissal for ask {ask_id}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(text: &str, header: &str) -> crate::types::UserQuestion {
        crate::types::UserQuestion {
            question: text.to_string(),
            header: header.to_string(),
            options: Vec::new(),
            multi_select: false,
            pick_min: None,
            pick_max: None,
            free_text: None,
            previews: Vec::new(),
        }
    }

    #[test]
    fn summary_is_bare_answer_for_single_question() {
        let questions = vec![question("Which grid?", "Grid")];
        let resolution = resolution_from_wire(
            HashMap::from([("Which grid?".to_string(), "A".to_string())]),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        assert_eq!(answer_summary(&questions, &resolution), "A");
    }

    #[test]
    fn summary_joins_multi_question_and_followups_in_item_order() {
        let questions = vec![
            question("Which grid?", "Grid"),
            question("Keep the sidebar?", ""),
            question("Optional third?", "Third"),
        ];
        let resolution = resolution_from_wire(
            HashMap::from([
                ("Keep the sidebar?".to_string(), "yes".to_string()),
                ("Which grid?".to_string(), "A".to_string()),
            ]),
            HashMap::new(),
            HashMap::from([("Which grid?".to_string(), "can B keep it?".to_string())]),
            HashMap::from([(
                "Keep the sidebar?".to_string(),
                vec![crate::types::QuestionAnnotation {
                    preview: "B".into(),
                    note: "too faint".into(),
                }],
            )]),
        );
        assert_eq!(
            answer_summary(&questions, &resolution),
            "Grid: A\nfollow-up (Grid): can B keep it?\nKeep the sidebar?: yes\nnote on B (Keep the sidebar?): too faint"
        );
    }

    #[test]
    fn summary_of_followup_only_submission_is_nonempty() {
        let questions = vec![question("Which grid?", "Grid")];
        let resolution = resolution_from_wire(
            HashMap::new(),
            HashMap::new(),
            HashMap::from([("Which grid?".to_string(), "neither — rethink".to_string())]),
            HashMap::new(),
        );
        assert_eq!(
            answer_summary(&questions, &resolution),
            "follow-up (Grid): neither — rethink"
        );
    }
}
