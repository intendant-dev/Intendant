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

/// The follow-up text delivered into the still-live ASKING session when an
/// agenda-backed ask resolves (slice 2: late-answer delivery). Plain user
/// INPUT text — it rides the same follow-up lane user messages ride and
/// never widens autonomy. `None` for actions that carry nothing to deliver.
///
/// Answer form (per the ratified brief): a readable summary — the
/// per-question `Header: answer` lines plus the follow-up/annotation lines
/// exactly as [`answer_summary`] builds them — and one trailing line noting
/// the full structure lives on the agenda item.
pub(crate) fn ask_outcome_delivery_text(
    item: &super::types::AgendaItem,
    action: &str,
) -> Option<String> {
    let title = &item.title;
    let id = &item.id;
    match action {
        "answer" => {
            let answer = item.answer.as_ref()?;
            let questions = item
                .ask
                .as_ref()
                .map(|ask| ask.questions.as_slice())
                .unwrap_or_default();
            let summary = answer
                .structured
                .as_ref()
                .map(|resolution| answer_summary(questions, resolution))
                .filter(|summary| !summary.trim().is_empty())
                .unwrap_or_else(|| answer.text.clone());
            let joint = if summary.contains('\n') { ":\n" } else { ": " };
            Some(format!(
                "Answer to your parked question \"{title}\" (agenda {id}){joint}{summary}\n\n\
                 The full structured answer (selections, follow-ups, preview notes) is on \
                 agenda item {id}."
            ))
        }
        "skip" | "deny" | "approve" | "approve_all" => Some(format!(
            "Your parked question \"{title}\" (agenda {id}) was dismissed on the question \
             rail ({action}) without an answer. It remains open on the agenda; the owner \
             can still answer it later."
        )),
        "complete" | "retire" => Some(format!(
            "Your parked question \"{title}\" (agenda {id}) was closed on the agenda \
             ({action}) without an answer."
        )),
        _ => None,
    }
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

    fn answered_item(
        questions: Vec<crate::types::UserQuestion>,
        answer: AgendaAnswer,
    ) -> AgendaItem {
        use super::super::types::*;
        AgendaItem {
            id: "01ITEM".into(),
            kind: AgendaKind::Question,
            title: "Which grid?".into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            provenance: AgendaProvenance {
                principal: None,
                session_id: Some("sess-ask".into()),
                kind: Some("agent_session".into()),
                source: None,
                created_ms: 1,
            },
            status: AgendaStatus::Done,
            updated_ms: 2,
            completed_ms: Some(2),
            answer: Some(answer),
            effects: Vec::new(),
            ask: Some(AgendaAsk {
                ask_id: 7,
                questions,
            }),
            dismissed: None,
            annotations: Vec::new(),
            blockers: Vec::new(),
            relies_on: Vec::new(),
        }
    }

    fn plain_answer(text: &str, structured: Option<AgendaAskResolution>) -> AgendaAnswer {
        AgendaAnswer {
            text: text.into(),
            at_ms: 2,
            principal: None,
            session_id: None,
            kind: None,
            structured,
        }
    }

    use super::super::types::{AgendaAnswer, AgendaItem};

    /// Delivery text for a single-question answer: one line, title +
    /// item id + the bare answer, plus the trailing structure pointer.
    #[test]
    fn delivery_text_single_answer_is_one_line_summary() {
        let item = answered_item(
            vec![question("Which grid?", "Grid")],
            plain_answer(
                "A",
                Some(resolution_from_wire(
                    HashMap::from([("Which grid?".to_string(), "A".to_string())]),
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                )),
            ),
        );
        assert_eq!(
            ask_outcome_delivery_text(&item, "answer").unwrap(),
            "Answer to your parked question \"Which grid?\" (agenda 01ITEM): A\n\n\
             The full structured answer (selections, follow-ups, preview notes) is on \
             agenda item 01ITEM."
        );
    }

    /// Multi-question answers deliver the per-question `Header: answer`
    /// lines with follow-up and annotation lines, exactly as the summary
    /// builder (and ctl) print them.
    #[test]
    fn delivery_text_multi_answer_carries_followup_and_annotation_lines() {
        let questions = vec![
            question("Which grid?", "Grid"),
            question("Keep the sidebar?", ""),
        ];
        let resolution = resolution_from_wire(
            HashMap::from([
                ("Which grid?".to_string(), "A".to_string()),
                ("Keep the sidebar?".to_string(), "yes".to_string()),
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
        let item = answered_item(questions, plain_answer("joined", Some(resolution)));
        let text = ask_outcome_delivery_text(&item, "answer").unwrap();
        assert!(
            text.starts_with(
                "Answer to your parked question \"Which grid?\" (agenda 01ITEM):\n\
                 Grid: A\n\
                 follow-up (Grid): can B keep it?\n\
                 Keep the sidebar?: yes\n\
                 note on B (Keep the sidebar?): too faint"
            ),
            "{text}"
        );
        assert!(
            text.ends_with("is on agenda item 01ITEM."),
            "trailing structure pointer missing: {text}"
        );
    }

    /// A plain text answer (Agenda tab, no structured breakdown) delivers
    /// the recorded text itself.
    #[test]
    fn delivery_text_plain_text_answer_uses_answer_text() {
        let item = answered_item(
            vec![question("Which grid?", "Grid")],
            plain_answer("go with A, but darker rails", None),
        );
        let text = ask_outcome_delivery_text(&item, "answer").unwrap();
        assert!(
            text.contains("(agenda 01ITEM): go with A, but darker rails"),
            "{text}"
        );
    }

    /// Dismissals say the question stays open; administrative closes say
    /// closed; unknown actions and answerless answers deliver nothing.
    #[test]
    fn delivery_text_dismissals_and_closes() {
        let mut item = answered_item(
            vec![question("Which grid?", "Grid")],
            plain_answer("A", None),
        );
        item.answer = None;
        for action in ["skip", "deny", "approve", "approve_all"] {
            let text = ask_outcome_delivery_text(&item, action).unwrap();
            assert!(
                text.contains(&format!("dismissed on the question rail ({action})")),
                "{text}"
            );
            assert!(text.contains("remains open on the agenda"), "{text}");
        }
        for action in ["complete", "retire"] {
            let text = ask_outcome_delivery_text(&item, action).unwrap();
            assert!(
                text.contains(&format!("closed on the agenda ({action})")),
                "{text}"
            );
        }
        assert!(ask_outcome_delivery_text(&item, "answer").is_none());
        assert!(ask_outcome_delivery_text(&item, "reopen").is_none());
    }
}
