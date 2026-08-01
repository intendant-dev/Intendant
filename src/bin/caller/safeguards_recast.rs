//! The safeguards-recast owner lane: honest surfaces for sessions whose
//! conversation a provider's safeguards flagged.
//!
//! A safeguards flag is TERMINAL FOR THOSE BYTES: the provider judged
//! the conversation's content, so retrying, parking, or resuming the
//! same context re-flags forever (2026-07-31 specimens: session
//! 69c8535e's flag rode a DoneSignal into a COMPLETED occurrence and
//! died invisible; a resume into session 77c8beaf's flagged context
//! re-flagged immediately, three times in one arc). House law, owner
//! standing: NO auto-retry lane exists for this class, ever, and no
//! model fallback anywhere — switching models is a per-instance owner
//! decision, and the remedy is a FRESH session with the task RECAST in
//! the owner's own words: a judgment act, not mechanics.
//!
//! This module holds the class's entire visible surface — the terminal
//! announcement row, the undelivered-input detail, the attention-class
//! notification, and THE needs-recast agenda task (find-or-create by
//! [`SAFEGUARDS_RECAST_TAG`], the commission sweep's needs-you pattern)
//! — so the external lanes and the boot pass cannot drift apart in
//! wording or behavior. Assisted recast (a draft manifest prepared for
//! owner approval) is deliberately NOT here: v1 ships surfaces and
//! guards only.

use crate::agenda::{
    AgendaActor, AgendaCommand, AgendaHandle, AgendaItem, AgendaKind, AgendaStatus,
};
use crate::event::{AppEvent, EventBus};
use crate::managed_context_ops::truncate_string_copy;

/// The stable tag naming THE needs-recast task (find-or-create key, the
/// commission-sweep pattern): one open item carries every flagged
/// session until the owner completes or retires it.
pub(crate) const SAFEGUARDS_RECAST_TAG: &str = "safeguards-recast";

/// The lane's self-described `source` label on its agenda writes.
const SAFEGUARDS_RECAST_SOURCE: &str = "safeguards-lane";

const NEEDS_RECAST_TITLE: &str = "Safeguards-flagged sessions need a recast";

const NEEDS_RECAST_BODY: &str = "Sessions land here when a provider's safeguards flag their \
     conversation and the run ends on it. A flagged conversation is terminal: retrying the \
     same bytes would be flagged again, so nothing on this list is ever retried, resumed, or \
     re-sent automatically, and the model is never switched automatically (switching models \
     is a per-instance owner decision). Remedy, per entry: start a fresh session and recast \
     the task in your own words. Complete or retire this item once handled — a later flag \
     re-creates it. Each flag, and each boot that finds a flagged session still marked \
     mid-work, appends the current facts below.";

/// Listed entries named in full per annotation; overflow is counted,
/// never dropped silently (the commission sweep's cap convention).
const LIST_DETAIL_CAP: usize = 16;

/// What the flag did to the lane it hit — the two honest shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecastDisposition {
    /// A supervised session ended on the flag (FAILED terminal): the
    /// remedy is a fresh session with the task recast.
    SessionEnded,
    /// A persistent lane survived the flag by closing the flagged
    /// backend conversation: the owner's next message starts a fresh
    /// conversation, which IS the recast lane.
    ConversationClosed,
}

impl RecastDisposition {
    /// What happened, for the notification's cause clause.
    fn happened(&self) -> &'static str {
        match self {
            RecastDisposition::SessionEnded => "ended",
            RecastDisposition::ConversationClosed => "closed its flagged conversation",
        }
    }

    /// The remedy, in the owner's own next-action terms.
    fn remedy(&self) -> &'static str {
        match self {
            RecastDisposition::SessionEnded => {
                "start a fresh session and recast the task in your own words"
            }
            RecastDisposition::ConversationClosed => {
                "your next message starts a fresh conversation — recast the task in your own words"
            }
        }
    }
}

/// One safeguards-flagged session, as the terminal lane named it.
#[derive(Debug, Clone)]
pub(crate) struct RecastRef {
    pub(crate) session_id: String,
    /// Backend label ("claude-code", "codex", …).
    pub(crate) source: String,
    /// The formatted cause (agent name + code + provider text).
    pub(crate) reason: String,
    pub(crate) disposition: RecastDisposition,
}

impl RecastRef {
    /// The durable session-meta marker for this flag (what the boot
    /// pass and the session catalog read).
    pub(crate) fn meta(&self, flagged_at_epoch: u64) -> crate::session_log::SessionSafeguardsFlagMeta {
        crate::session_log::SessionSafeguardsFlagMeta {
            flagged_at_epoch,
            reason_preview: reason_preview(&self.reason),
        }
    }
}

/// First line of the cause, truncated for one-row surfaces. The full
/// text is in the session log's error row.
fn reason_preview(reason: &str) -> String {
    let first_line = reason.lines().next().unwrap_or("").trim();
    truncate_string_copy(first_line, 160)
}

/// The session-log/activity row announcing the safeguards terminal —
/// one place so the external lanes cannot drift. States what happens
/// NOW: the session ends; nothing retries; the model is never switched;
/// the owner recasts.
pub(crate) fn safeguards_flag_line(reason: &str) -> String {
    format!(
        "Provider safeguards flagged this conversation — session ended ({}). Nothing is \
         retried and the model is never switched automatically: the same bytes would be \
         flagged again. Remedy: start a fresh session and recast the task in your own words.",
        reason_preview(reason),
    )
}

/// The persistent-lane variant of [`safeguards_flag_line`]: the daemon
/// session survives; the flagged backend conversation is closed instead,
/// and the owner's next message starts fresh.
pub(crate) fn safeguards_flag_line_conversation_closed(reason: &str) -> String {
    format!(
        "Provider safeguards flagged this conversation — the flagged conversation is closed \
         ({}). Nothing is retried and the model is never switched automatically: the same \
         bytes would be flagged again. Your next message starts a fresh conversation — recast \
         the task in your own words.",
        reason_preview(reason),
    )
}

/// The readopt guard ladder's left-dead reason for this class — and the
/// boot pass's filter key for its needs-recast listing (the commission
/// sweep inherits it through the shared ladder as its own listed
/// reason). States the whole law in one line: listed, never nudged,
/// recast is the lane out.
pub(crate) const SAFEGUARDS_LEFT_DEAD_REASON: &str =
    "safeguards-flagged — needs a recast in a fresh session; never auto-resumed";

/// The named reason stamped on input that was queued toward — or already
/// injected into — a turn the safeguards flag killed. Surfaced as
/// undelivered, never re-armed: re-delivery into a flagged context is
/// the re-flag loop.
pub(crate) const SAFEGUARDS_UNDELIVERED_DETAIL: &str =
    "undelivered — provider safeguards flagged the session and it ended; recast the task in \
     a fresh session";

/// The attention-class notification a safeguards flag raises: names the
/// session, the cause, and the remedy in plain language. Session-scoped
/// with a per-session stable id, so a re-flag of the same conversation
/// replaces its entry instead of stacking.
pub(crate) fn safeguards_flag_notification(entry: &RecastRef) -> AppEvent {
    AppEvent::UserNotification {
        session_id: Some(entry.session_id.clone()),
        id: format!("safeguards-flag-{}", entry.session_id),
        title: Some("Session flagged by provider safeguards".to_string()),
        text: format!(
            "Session {} ({}) {} — {}. Intendant will not retry it and never switches \
             models on its own. Remedy: {}.",
            short_id(&entry.session_id),
            entry.source,
            entry.disposition.happened(),
            reason_preview(&entry.reason),
            entry.disposition.remedy(),
        ),
        urgency: crate::types::NotificationUrgency::Attention,
        ts: now_ms(),
    }
}

/// The annotation a live flag appends to the needs-recast task.
pub(crate) fn live_flag_annotation(entry: &RecastRef) -> String {
    format!(
        "Safeguards flag: session {} ({}) {} — {}. Remedy: {}; never auto-retried.",
        short_id(&entry.session_id),
        entry.source,
        entry.disposition.happened(),
        reason_preview(&entry.reason),
        entry.disposition.remedy(),
    )
}

/// The annotation the boot pass appends for flagged sessions it found
/// still marked mid-work and deliberately left down (listed, never
/// nudged).
pub(crate) fn boot_sweep_annotation(entries: &[RecastRef]) -> String {
    let mut lines = vec![format!(
        "Boot sweep: {} safeguards-flagged session(s) left down — needs recast, never \
         auto-resumed.",
        entries.len()
    )];
    for entry in entries.iter().take(LIST_DETAIL_CAP) {
        lines.push(format!(
            "- session {} ({}) — {}",
            short_id(&entry.session_id),
            entry.source,
            reason_preview(&entry.reason),
        ));
    }
    if entries.len() > LIST_DETAIL_CAP {
        lines.push(format!("…and {} more.", entries.len() - LIST_DETAIL_CAP));
    }
    lines.join("\n")
}

/// Find or create THE needs-recast task (oldest open item carrying the
/// tag — ULID order) and append `text`, skipping an identical
/// consecutive annotation so a crash-looping daemon never stacks copies
/// of the same facts.
fn park_needs_recast(handle: &AgendaHandle, text: String) -> Result<String, String> {
    let snapshot = handle.snapshot();
    let mut anchors: Vec<&AgendaItem> = snapshot
        .iter()
        .filter(|item| {
            item.status == AgendaStatus::Open
                && item.tags.iter().any(|tag| tag == SAFEGUARDS_RECAST_TAG)
        })
        .collect();
    anchors.sort_by(|a, b| a.id.cmp(&b.id));
    let item_id = match anchors.first() {
        Some(item) => {
            if item.annotations.last().is_some_and(|note| note.text == text) {
                return Ok(item.id.clone());
            }
            item.id.clone()
        }
        None => {
            handle
                .apply(
                    AgendaCommand::Add {
                        kind: AgendaKind::Task,
                        title: NEEDS_RECAST_TITLE.to_string(),
                        body: NEEDS_RECAST_BODY.to_string(),
                        tags: vec![SAFEGUARDS_RECAST_TAG.to_string()],
                        due_ms: None,
                        source: Some(SAFEGUARDS_RECAST_SOURCE.to_string()),
                        refs: Vec::new(),
                    },
                    Some(AgendaActor::daemon()),
                )
                .map_err(|err| format!("park needs-recast task: {err}"))?
                .id
        }
    };
    handle
        .apply(
            AgendaCommand::Annotate {
                id: item_id.clone(),
                text,
                source: Some(SAFEGUARDS_RECAST_SOURCE.to_string()),
            },
            Some(AgendaActor::daemon()),
        )
        .map_err(|err| format!("annotate needs-recast task: {err}"))?;
    Ok(item_id)
}

/// The live lane's whole visible surface for one flag: the durable
/// needs-recast entry plus the attention-class notification. A missing
/// agenda handle degrades to the notification alone; a parking failure
/// never swallows the notification (the commission sweep's degradation
/// contract).
pub(crate) fn report_safeguards_flag(
    bus: &EventBus,
    handle: Option<&AgendaHandle>,
    entry: &RecastRef,
) {
    match handle {
        Some(handle) => {
            if let Err(err) = park_needs_recast(handle, live_flag_annotation(entry)) {
                eprintln!("[safeguards-lane] {err} — the notification carries the facts");
            }
        }
        None => {
            eprintln!("[safeguards-lane] agenda handle unavailable — needs-recast entry not parked")
        }
    }
    bus.send(safeguards_flag_notification(entry));
}

/// The boot pass's durable listing for flagged sessions left down. The
/// boot summary notification already names them (the left-dead lines),
/// so this parks the agenda entry only.
pub(crate) fn report_boot_needs_recast(handle: Option<&AgendaHandle>, entries: &[RecastRef]) {
    if entries.is_empty() {
        return;
    }
    match handle {
        Some(handle) => {
            if let Err(err) = park_needs_recast(handle, boot_sweep_annotation(entries)) {
                eprintln!("[safeguards-lane] {err} — boot needs-recast list not parked");
            }
        }
        None => {
            eprintln!(
                "[safeguards-lane] agenda handle unavailable — boot needs-recast list not parked"
            )
        }
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> RecastRef {
        RecastRef {
            session_id: "77c8beaf-c66a-4b11-8354-9a5b6678324a".to_string(),
            source: "claude-code".to_string(),
            reason: "claude-code backend error (success): API Error: Fable 5's safeguards \
                     flagged this message (https://www.anthropic.com/legal/aup). Our \
                     intentionally broad safeguards allow us to deliver more capabilities \
                     faster, but can sometimes flag legitimate coding, cybersecurity, and \
                     biology tasks."
                .to_string(),
            disposition: RecastDisposition::SessionEnded,
        }
    }

    #[test]
    fn terminal_line_states_present_behavior_and_the_remedy() {
        let line = safeguards_flag_line(&entry().reason);
        assert!(line.contains("session ended"), "{line}");
        assert!(line.contains("Nothing is retried"), "{line}");
        assert!(line.contains("never switched automatically"), "{line}");
        assert!(line.contains("recast the task in your own words"), "{line}");
    }

    #[test]
    fn notification_names_session_cause_and_remedy() {
        let AppEvent::UserNotification {
            session_id,
            id,
            title,
            text,
            urgency,
            ..
        } = safeguards_flag_notification(&entry())
        else {
            panic!("notification shape");
        };
        assert_eq!(session_id.as_deref(), Some(entry().session_id.as_str()));
        assert_eq!(id, format!("safeguards-flag-{}", entry().session_id));
        assert_eq!(
            title.as_deref(),
            Some("Session flagged by provider safeguards")
        );
        assert!(text.contains("77c8beaf"), "{text}");
        assert!(text.contains("claude-code"), "{text}");
        assert!(text.contains("safeguards flagged"), "{text}");
        assert!(text.contains("never switches"), "{text}");
        assert!(text.contains("recast the task in your own words"), "{text}");
        assert!(matches!(urgency, crate::types::NotificationUrgency::Attention));
    }

    #[test]
    fn boot_annotation_caps_detail_and_counts_overflow() {
        let entries: Vec<RecastRef> = (0..LIST_DETAIL_CAP + 3)
            .map(|i| RecastRef {
                session_id: format!("{i:08}-0000-0000-0000-000000000000"),
                source: "claude-code".to_string(),
                reason: "safeguards flagged".to_string(),
                disposition: RecastDisposition::SessionEnded,
            })
            .collect();
        let text = boot_sweep_annotation(&entries);
        assert!(
            text.starts_with(&format!(
                "Boot sweep: {} safeguards-flagged session(s) left down",
                entries.len()
            )),
            "{text}"
        );
        assert!(text.contains("never \nauto-resumed") || text.contains("never auto-resumed"));
        assert!(text.contains("…and 3 more."), "{text}");
        assert_eq!(
            text.lines().count(),
            1 + LIST_DETAIL_CAP + 1,
            "header + capped entries + overflow line: {text}"
        );
    }

    #[test]
    fn undelivered_detail_names_the_cause_and_remedy() {
        assert!(SAFEGUARDS_UNDELIVERED_DETAIL.contains("undelivered"));
        assert!(SAFEGUARDS_UNDELIVERED_DETAIL.contains("safeguards flagged"));
        assert!(SAFEGUARDS_UNDELIVERED_DETAIL.contains("fresh session"));
    }
}
