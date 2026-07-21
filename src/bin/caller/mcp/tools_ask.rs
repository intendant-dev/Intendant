//! The agent→user interaction tools: `ask_user` (blocking structured
//! question) and `notify_user` (fire-and-forget notification).
//!
//! `ask_user` gives every supervised agent — native, Codex, Claude Code, or
//! any `intendant ctl` caller — the same question rail the native loop's
//! askHuman and supervised Claude Code's AskUserQuestion already use: it
//! emits [`AppEvent::UserQuestionRequired`] so the existing dashboard panel
//! renders it, then **blocks** until a frontend resolves it. A question is
//! a request for *input*, not permission: autonomy policy never
//! auto-resolves it, and a bare approve never widens command autonomy.
//!
//! Resolution rides the same wire as every other frontend intent: the
//! waiter subscribes to the event bus and reacts to the
//! `ControlMsg::AnswerQuestion` / approval-verb `ControlCommand`s that name
//! its id. Ask ids use the same process-wide allocator as every approval and
//! structured-question rail, so concurrent sessions cannot collide.
//!
//! `notify_user` is the opposite shape: it broadcasts
//! [`AppEvent::UserNotification`] and returns immediately. Urgency levels
//! escalate delivery (dashboard toast/transcript → attention center →
//! immediate Connect push nudge via `attention_nudge`); the nudge payload
//! stays content-free (kind + session label only — never the text).

use super::*;

/// Maximum question text size — the rail renders it as a panel, not a page.
pub(crate) const ASK_USER_MAX_QUESTION_BYTES: usize = 2048;
/// Maximum number of structured options per question.
pub(crate) const ASK_USER_MAX_OPTIONS: usize = 4;
/// Maximum questions per ask (multi-question form).
pub(crate) const ASK_USER_MAX_QUESTIONS: usize = 4;
/// Maximum option-label size (the label is also the returned answer value).
pub(crate) const ASK_USER_MAX_OPTION_LABEL_BYTES: usize = 120;
/// Default blocking wait for an answer.
pub(crate) const ASK_USER_DEFAULT_WAIT_SECS: u64 = 300;
/// Hard cap on the blocking wait.
pub(crate) const ASK_USER_MAX_WAIT_SECS: u64 = 900;
/// Maximum preview cards per ask.
pub(crate) const ASK_USER_MAX_PREVIEWS: usize = 4;
/// Maximum size of one self-contained HTML preview document.
pub(crate) const ASK_USER_MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
/// Maximum size of one inline text preview snippet.
pub(crate) const ASK_USER_MAX_TEXT_PREVIEW_BYTES: usize = 4096;
/// Maximum total preview payload (decoded) per ask.
pub(crate) const ASK_USER_MAX_TOTAL_PREVIEW_BYTES: usize = 8 * 1024 * 1024;
/// Maximum preview-card label size (truncated, not refused — the label is
/// a caption, unlike an option label it is never an answer value).
const ASK_USER_MAX_PREVIEW_LABEL_BYTES: usize = 80;
/// Maximum notification text size. Notifications are alerts, not documents.
pub(crate) const NOTIFY_USER_MAX_TEXT_BYTES: usize = 4096;

/// Ids of `ask_user` questions currently blocked on an answer. Advisory
/// registry: resolution happens in the waiter (via the bus), but other
/// resolution paths (the session supervisor's per-session approval
/// registries) consult this set so an MCP ask id is not misreported as an
/// unknown/expired approval.
fn pending_asks() -> &'static std::sync::Mutex<HashSet<u64>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<HashSet<u64>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

/// Whether `id` is an `ask_user` question still waiting for its answer.
/// Consulted by the session supervisor before warning about an approval id
/// it does not know (the ask's own waiter resolves it and emits
/// `ApprovalResolved`), and by the agenda handle to stamp `inline_waiter`
/// on ask outcomes — a live waiter returns the outcome inline, so the
/// supervisor must not also deliver it into the asking session.
pub(crate) fn ask_user_question_pending(id: u64) -> bool {
    pending_asks()
        .lock()
        .map(|set| set.contains(&id))
        .unwrap_or(false)
}

/// Register a blocking waiter for `id`. The agenda handle calls this from
/// `park_ask_for_waiter` UNDER the store lock, before the parked item is
/// broadcast — so every outcome recorded on the item sees the waiter and
/// stamps `inline_waiter: true` until the waiter deregisters.
pub(crate) fn register_pending_ask(id: u64) {
    if let Ok(mut set) = pending_asks().lock() {
        set.insert(id);
    }
}

/// Drop a blocking waiter's registration (idempotent).
pub(crate) fn unregister_pending_ask(id: u64) {
    if let Ok(mut set) = pending_asks().lock() {
        set.remove(&id);
    }
}

/// Waiter-side bookkeeping for one agenda-backed blocking ask
/// (blocking-as-sugar). Unlike [`PendingAskGuard`], resolution ordering is
/// owned by the AGENDA — the handle records outcomes and emits
/// `ApprovalResolved`/`AgendaAskOutcome`; this guard never clears the rail.
/// Its two jobs:
/// - drop the pending-registry entry exactly once, and
/// - when the waiter ABANDONS the wait (timeout, transport dropping the
///   future mid-wait), convert the rail card into its parked form (same
///   id, no expiry, not held) — the question is durable now and must not
///   evaporate with the waiter.
struct AgendaAskGuard {
    id: u64,
    session_id: Option<String>,
    questions: Vec<crate::types::UserQuestion>,
    bus: EventBus,
    resolved: bool,
}

impl AgendaAskGuard {
    /// Adopt the registration `park_ask_for_waiter` already made.
    fn adopt(
        id: u64,
        session_id: Option<String>,
        questions: Vec<crate::types::UserQuestion>,
        bus: EventBus,
    ) -> Self {
        Self {
            id,
            session_id,
            questions,
            bus,
            resolved: false,
        }
    }

    /// The item resolved (answer/dismissal observed): deregister only —
    /// the agenda handle already cleared the rails.
    fn resolve(&mut self) {
        if std::mem::replace(&mut self.resolved, true) {
            return;
        }
        unregister_pending_ask(self.id);
    }

    /// The waiter stops waiting on a still-open question: deregister and
    /// re-announce the card in parked form so every rail (and the
    /// reconnect state-line cache) shows a durable question instead of a
    /// dead countdown.
    fn abandon_to_parked(&mut self) {
        if std::mem::replace(&mut self.resolved, true) {
            return;
        }
        unregister_pending_ask(self.id);
        self.bus.send(AppEvent::UserQuestionRequired {
            session_id: self.session_id.clone(),
            id: self.id,
            questions: self.questions.clone(),
            expires_at_ms: None,
            held: false,
        });
    }
}

impl Drop for AgendaAskGuard {
    fn drop(&mut self) {
        self.abandon_to_parked();
    }
}

/// Registration + cleanup for one blocked ask. Whatever way the tool
/// returns — answered, timed out, or the transport dropping the future
/// mid-wait (ctl killed, HTTP connection gone) — the pending entry is
/// removed and the rail is cleared with an `ApprovalResolved`, so a dead
/// ask can never leave a zombie question pending on dashboards.
struct PendingAskGuard {
    id: u64,
    session_id: Option<String>,
    bus: EventBus,
    resolved: bool,
}

impl PendingAskGuard {
    fn register(id: u64, session_id: Option<String>, bus: EventBus) -> Self {
        if let Ok(mut set) = pending_asks().lock() {
            set.insert(id);
        }
        Self {
            id,
            session_id,
            bus,
            resolved: false,
        }
    }

    /// Mark the ask resolved as `action`: drop the pending entry and tell
    /// every frontend to clear the rail. Idempotent.
    fn resolve(&mut self, action: &str) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        if let Ok(mut set) = pending_asks().lock() {
            set.remove(&self.id);
        }
        self.bus.send(AppEvent::ApprovalResolved {
            session_id: self.session_id.clone(),
            id: self.id,
            action: action.to_string(),
        });
    }
}

impl Drop for PendingAskGuard {
    fn drop(&mut self) {
        self.resolve("cancelled");
    }
}

/// The best-judgment text handed to the agent when nobody answers —
/// headless auto-answer and timeout share it (mirrors the supervised
/// Claude Code headless semantic in `external_events.rs`).
const NO_ANSWER_GUIDANCE: &str = "No user is connected to answer right now. Proceed using your \
     best judgment based on the context so far; you can re-ask later if it \
     is still relevant.";

const DISMISSED_GUIDANCE: &str = "The user dismissed the question without answering. Proceed \
     with your best judgment; you can re-ask later if it is still relevant.";

const PASS_GUIDANCE: &str = "The supervisor let this question through without selecting an \
     option. Proceed using your best judgment.";

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Deadline bookkeeping for one blocked ask: a countdown the user can
/// suspend (`hold_question`) and later resume with exactly the time that
/// remained. Pure math over injected instants so the hold semantics are
/// unit-testable without a waiter.
pub(crate) struct AskDeadline {
    /// Armed deadline while counting down; `None` while held.
    deadline: Option<tokio::time::Instant>,
    /// Time left, captured at the moment of the last hold; meaningful
    /// (and kept current) only while held.
    remaining: std::time::Duration,
}

impl AskDeadline {
    pub(crate) fn new(now: tokio::time::Instant, wait: std::time::Duration) -> Self {
        Self {
            deadline: Some(now + wait),
            remaining: wait,
        }
    }

    pub(crate) fn held(&self) -> bool {
        self.deadline.is_none()
    }

    /// Apply a hold flip. Returns `true` when the state actually changed
    /// (the waiter re-announces the question only then).
    pub(crate) fn set_held(&mut self, held: bool, now: tokio::time::Instant) -> bool {
        match (held, self.deadline) {
            (true, Some(deadline)) => {
                self.remaining = deadline.saturating_duration_since(now);
                self.deadline = None;
                true
            }
            (false, None) => {
                self.deadline = Some(now + self.remaining);
                true
            }
            _ => false,
        }
    }

    /// The instant the waiter's `timeout_at` should fire. While held there
    /// is no deadline — park far enough out that it never fires (re-armed
    /// every loop iteration anyway).
    pub(crate) fn wake_at(&self, now: tokio::time::Instant) -> tokio::time::Instant {
        self.deadline
            .unwrap_or_else(|| now + std::time::Duration::from_secs(365 * 86_400))
    }

    /// Wall-clock expiry for the wire (`expires_at_ms`); `None` while held.
    pub(crate) fn expires_at_unix_ms(&self, now: tokio::time::Instant, now_ms: u64) -> Option<u64> {
        self.deadline
            .map(|d| now_ms + d.saturating_duration_since(now).as_millis() as u64)
    }
}

/// One validated `ask_user` preview card, decoded but not yet committed —
/// blob kinds still carry their bytes; `ask_user_inner` commits them into
/// the calling session's upload store once the session is resolved.
#[derive(Debug)]
pub(crate) struct DecodedPreview {
    pub(crate) label: String,
    pub(crate) source: DecodedPreviewSource,
}

#[derive(Debug)]
pub(crate) enum DecodedPreviewSource {
    Html(String),
    Image { mime: &'static str, bytes: Vec<u8> },
    Text(String),
}

/// Decode and validate the preview cards of an `ask_user` call: exactly
/// one source per card, the session-note MIME allowlist for images, and
/// the per-kind plus total size caps.
/// `total_bytes` is the per-ASK running preview budget: the multi-question
/// form decodes each question's cards against one shared cap.
pub(crate) fn decode_ask_previews(
    previews: &[AskUserPreviewParams],
    total_bytes: &mut usize,
) -> Result<Vec<DecodedPreview>, String> {
    if previews.len() > ASK_USER_MAX_PREVIEWS {
        return Err(format!(
            "too many previews: {} (max {ASK_USER_MAX_PREVIEWS} per question)",
            previews.len()
        ));
    }
    let mut decoded = Vec::with_capacity(previews.len());
    let total_bytes = &mut *total_bytes;
    for (index, preview) in previews.iter().enumerate() {
        let label = preview.label.trim();
        if label.is_empty() {
            return Err(format!("previews[{index}]: label must not be empty"));
        }
        let label = crate::types::truncate_str(label, ASK_USER_MAX_PREVIEW_LABEL_BYTES).to_string();
        let sources = usize::from(preview.html.is_some())
            + usize::from(preview.image.is_some())
            + usize::from(preview.text.is_some());
        if sources != 1 {
            return Err(format!(
                "previews[{index}]: provide exactly one of html, image, or text"
            ));
        }
        let source = if let Some(html) = preview.html.as_deref() {
            if html.trim().is_empty() {
                return Err(format!("previews[{index}]: html must not be empty"));
            }
            if html.len() > ASK_USER_MAX_HTML_BYTES {
                return Err(format!(
                    "previews[{index}]: html is {} bytes; max {} MB (send a self-contained \
                     document with small inline assets)",
                    html.len(),
                    ASK_USER_MAX_HTML_BYTES / (1024 * 1024)
                ));
            }
            *total_bytes = total_bytes.saturating_add(html.len());
            DecodedPreviewSource::Html(html.to_string())
        } else if let Some(image) = preview.image.as_deref() {
            let media_type = preview
                .media_type
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .ok_or_else(|| format!("previews[{index}]: media_type is required with image"))?;
            let mime = super::tools_notes::canonical_note_image_mime(media_type)
                .map_err(|e| format!("previews[{index}]: {e}"))?;
            let bytes = super::tools_notes::decode_flexible_base64(image)
                .map_err(|e| format!("previews[{index}]: {e}"))?;
            if bytes.len() > super::tools_notes::SESSION_NOTE_MAX_IMAGE_BYTES {
                return Err(format!(
                    "previews[{index}]: {} bytes exceeds the {} MB per-image cap",
                    bytes.len(),
                    super::tools_notes::SESSION_NOTE_MAX_IMAGE_BYTES / (1024 * 1024)
                ));
            }
            *total_bytes = total_bytes.saturating_add(bytes.len());
            DecodedPreviewSource::Image { mime, bytes }
        } else {
            let text = preview.text.as_deref().unwrap_or_default().trim();
            if text.is_empty() {
                return Err(format!("previews[{index}]: text must not be empty"));
            }
            if text.len() > ASK_USER_MAX_TEXT_PREVIEW_BYTES {
                return Err(format!(
                    "previews[{index}]: text is {} bytes; max {} KB (use an html or image \
                     preview for larger content)",
                    text.len(),
                    ASK_USER_MAX_TEXT_PREVIEW_BYTES / 1024
                ));
            }
            *total_bytes = total_bytes.saturating_add(text.len());
            DecodedPreviewSource::Text(text.to_string())
        };
        if *total_bytes > ASK_USER_MAX_TOTAL_PREVIEW_BYTES {
            return Err(format!(
                "total preview payload exceeds the {} MB per-ask cap",
                ASK_USER_MAX_TOTAL_PREVIEW_BYTES / (1024 * 1024)
            ));
        }
        decoded.push(DecodedPreview { label, source });
    }
    Ok(decoded)
}

/// Commit one preview blob into the calling session's upload store and
/// return its descriptor. Mirrors the session-note attachment path: blobs
/// travel onward as references, never inline bytes on the bus, in the
/// state-line replay cache, or in the session log.
fn commit_preview_blob(
    bytes: &[u8],
    label: &str,
    extension: &str,
    mime: &str,
    log_dir: &std::path::Path,
    session_id: &str,
    scope: &crate::global_store::StoreScope,
) -> Result<crate::upload_store::UploadDescriptor, String> {
    let name = crate::upload_store::sanitize_name(&format!("{label}.{extension}"));
    super::tools_notes::write_blob_tempfile(bytes).and_then(|tmp| {
        crate::upload_store::commit_upload(
            tmp,
            &name,
            mime,
            bytes.len() as u64,
            crate::upload_store::UploadDestination::Task,
            log_dir,
            session_id,
            scope,
        )
        .map_err(|e| format!("failed to store preview '{label}': {e}"))
    })
}

/// The upload-store `/raw` URL every browser (live and replay) resolves
/// for a committed preview blob — same shape as session-note attachments.
fn preview_raw_url(upload_id: &str) -> String {
    format!("/api/session/current/uploads/{upload_id}/raw")
}

/// Validate `ask_user` parameters into the `UserQuestion` the rail renders.
/// Returns the question (previews still empty), the decoded-but-uncommitted
/// preview cards, and the effective wait.
/// One validated ask question paired with its decoded-but-uncommitted
/// preview cards.
pub(crate) type BuiltAskQuestion = (crate::types::UserQuestion, Vec<DecodedPreview>);

/// Validate one question of an ask — flat or multi form — into the
/// `UserQuestion` the rail renders plus its decoded-but-uncommitted preview
/// cards. `at` prefixes error messages ("" for the flat form,
/// "questions[N]: " for the multi form); `preview_budget` is the per-ask
/// running byte total shared across questions.
#[allow(clippy::too_many_arguments)]
fn build_one_ask_question(
    at: &str,
    question_text: &str,
    header: Option<&str>,
    option_params: &[AskUserOptionParams],
    preview_params: &[AskUserPreviewParams],
    multi_select: bool,
    pick_min: Option<u8>,
    pick_max: Option<u8>,
    free_text: Option<bool>,
    preview_budget: &mut usize,
) -> Result<BuiltAskQuestion, String> {
    let question = question_text.trim();
    if question.is_empty() {
        return Err(format!("{at}question must not be empty"));
    }
    if question.len() > ASK_USER_MAX_QUESTION_BYTES {
        return Err(format!(
            "{at}question is {} bytes; max {} KB",
            question.len(),
            ASK_USER_MAX_QUESTION_BYTES / 1024
        ));
    }
    if option_params.len() > ASK_USER_MAX_OPTIONS {
        return Err(format!(
            "{at}too many options: {} (max {ASK_USER_MAX_OPTIONS}; zero options means free-text \
             only)",
            option_params.len()
        ));
    }
    let mut options = Vec::with_capacity(option_params.len());
    for (index, option) in option_params.iter().enumerate() {
        let label = option.label.trim();
        if label.is_empty() {
            return Err(format!("{at}options[{index}]: label must not be empty"));
        }
        if label.len() > ASK_USER_MAX_OPTION_LABEL_BYTES {
            return Err(format!(
                "{at}options[{index}]: label is {} bytes; max {ASK_USER_MAX_OPTION_LABEL_BYTES} \
                 (the label is the answer value — keep it short)",
                label.len()
            ));
        }
        let description = option
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| crate::types::truncate_str(d, 256).to_string())
            .unwrap_or_default();
        options.push(crate::types::UserQuestionOption {
            label: label.to_string(),
            description,
        });
    }
    // Pick-bound sanity: bounds describe selections of the offered
    // options, so they need options to select; a question that forbids
    // free text must leave something answerable.
    if options.is_empty() {
        if pick_max.is_some() {
            return Err(format!("{at}pick_max needs options to pick from"));
        }
        if pick_min.is_some_and(|m| m > 1) {
            return Err(format!("{at}pick_min above 1 needs options to pick from"));
        }
        if free_text == Some(false) {
            return Err(format!(
                "{at}free_text: false with no options leaves nothing to answer"
            ));
        }
    } else {
        let max = pick_max.unwrap_or(if multi_select { options.len() as u8 } else { 1 });
        if max == 0 {
            return Err(format!("{at}pick_max must be at least 1"));
        }
        if (max as usize) > options.len() {
            return Err(format!(
                "{at}pick_max {max} exceeds the {} offered options",
                options.len()
            ));
        }
        if pick_min.is_some_and(|min| min > max) {
            return Err(format!(
                "{at}pick_min {} exceeds pick_max {max}",
                pick_min.unwrap_or_default()
            ));
        }
    }
    let header = header
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| crate::types::truncate_str(h, 64).to_string())
        .unwrap_or_default();
    let previews =
        decode_ask_previews(preview_params, preview_budget).map_err(|e| format!("{at}{e}"))?;
    Ok((
        crate::types::UserQuestion {
            question: question.to_string(),
            header,
            options,
            multi_select,
            pick_min,
            pick_max,
            free_text,
            previews: Vec::new(),
        },
        previews,
    ))
}

/// Validate `ask_user` parameters into the question list the rail renders:
/// the flat single-question form or the `questions` multi form (up to
/// [`ASK_USER_MAX_QUESTIONS`]), never both. Returns per-question
/// (question, decoded-uncommitted previews) pairs and the effective wait.
pub(crate) fn build_ask_user_questions(
    params: &AskUserParams,
) -> Result<(Vec<BuiltAskQuestion>, u64), String> {
    let wait_seconds = params
        .wait_seconds
        .unwrap_or(ASK_USER_DEFAULT_WAIT_SECS)
        .clamp(1, ASK_USER_MAX_WAIT_SECS);
    let flat = !params.question.trim().is_empty();
    if flat && !params.questions.is_empty() {
        return Err("provide either question or questions, not both".to_string());
    }
    let mut preview_budget = 0usize;
    if params.questions.is_empty() {
        let built = build_one_ask_question(
            "",
            &params.question,
            params.header.as_deref(),
            &params.options,
            &params.previews,
            params.multi_select.unwrap_or(false),
            params.pick_min,
            params.pick_max,
            params.free_text,
            &mut preview_budget,
        )?;
        return Ok((vec![built], wait_seconds));
    }
    if params.questions.len() > ASK_USER_MAX_QUESTIONS {
        return Err(format!(
            "too many questions: {} (max {ASK_USER_MAX_QUESTIONS})",
            params.questions.len()
        ));
    }
    let mut built = Vec::with_capacity(params.questions.len());
    for (index, q) in params.questions.iter().enumerate() {
        let at = format!("questions[{index}]: ");
        built.push(build_one_ask_question(
            &at,
            &q.question,
            q.header.as_deref(),
            &q.options,
            &q.previews,
            false,
            q.pick_min,
            q.pick_max,
            q.free_text,
            &mut preview_budget,
        )?);
    }
    Ok((built, wait_seconds))
}

/// The agenda actor recorded on an `ask_user`-created item: the caller's
/// gate-resolved binding, with the ASKING session stamped as the acting
/// session. `ask_user` acts as the session it asks for (explicit
/// parameter → URL-injected id → single-session fallback — the same
/// resolution that attributes the rail card), and that session is what
/// late-answer delivery targets; the binding's principal/kind ride along
/// as recorded by the gate.
fn park_actor(
    binding: &crate::access::actor::ActorBinding,
    session_id: &str,
) -> crate::agenda::AgendaActor {
    let mut actor = crate::agenda::AgendaActor::from_binding(binding).unwrap_or_default();
    actor.session_id = Some(session_id.to_string());
    if actor.kind.is_none() {
        actor.kind = Some("agent_session".to_string());
    }
    actor
}

/// Map `ask_user` parameters (flat or multi form) into the per-question
/// park vocabulary the agenda's `Ask` command speaks — the same mapping
/// `intendant ctl ask --park` performs client-side (`ask_park_command`):
/// call-level fields are dropped and the flat form's `multi_select` sugar
/// becomes explicit pick bounds, because the park wire speaks the precise
/// per-question vocabulary only.
pub(crate) fn park_questions(params: &AskUserParams) -> Result<Vec<AskUserQuestionParams>, String> {
    let flat = !params.question.trim().is_empty();
    if flat && !params.questions.is_empty() {
        return Err("provide either question or questions, not both".to_string());
    }
    if !params.questions.is_empty() {
        return Ok(params.questions.clone());
    }
    let mut pick_min = params.pick_min;
    let mut pick_max = params.pick_max;
    if params.multi_select.unwrap_or(false) && pick_max.is_none() && !params.options.is_empty() {
        pick_min = pick_min.or(Some(1));
        pick_max = Some(params.options.len().min(u8::MAX as usize) as u8);
    }
    Ok(vec![AskUserQuestionParams {
        question: params.question.clone(),
        header: params.header.clone(),
        options: params.options.clone(),
        previews: params.previews.clone(),
        pick_min,
        pick_max,
        free_text: params.free_text,
    }])
}

/// One `ask_user` outcome, shaped for both the returning tool result and
/// the guidance-filled answers map agents read. The legacy top-level
/// `question`/`answer` name the FIRST question; `questions` carries the
/// per-question breakdown (answer + structured `selected` labels when the
/// frontend reported them).
fn ask_result(
    status: &str,
    id: u64,
    session_id: &str,
    questions: &[crate::types::UserQuestion],
    answers: std::collections::HashMap<String, String>,
    selections: Option<&std::collections::HashMap<String, Vec<String>>>,
    guidance: Option<&str>,
) -> serde_json::Value {
    let first_question = questions.first().map(|q| q.question.as_str()).unwrap_or("");
    let answer = answers
        .get(first_question)
        .cloned()
        .or_else(|| answers.values().next().cloned())
        .unwrap_or_default();
    let per_question: Vec<serde_json::Value> = questions
        .iter()
        .map(|q| {
            let mut entry = serde_json::json!({
                "question": q.question,
                "header": q.header,
                "answer": answers.get(&q.question).cloned().unwrap_or_default(),
            });
            if let Some(selected) = selections.and_then(|s| s.get(&q.question)) {
                entry["selected"] = serde_json::json!(selected);
            }
            entry
        })
        .collect();
    let mut result = serde_json::json!({
        "status": status,
        "id": id,
        "session_id": session_id,
        "question": first_question,
        "answer": answer,
        "answers": answers,
        "questions": per_question,
    });
    if let Some(guidance) = guidance {
        result["guidance"] = serde_json::Value::String(guidance.to_string());
    }
    result
}

/// The answered-path result: `ask_result` plus per-question `followup`
/// and `annotations` — and a nudge in `guidance` when any follow-up
/// arrived, so agents reliably address them (in chat or with a narrowed
/// re-ask) instead of skimming past.
#[allow(clippy::too_many_arguments)]
fn ask_result_with_followups(
    status: &str,
    id: u64,
    session_id: &str,
    questions: &[crate::types::UserQuestion],
    answers: std::collections::HashMap<String, String>,
    selections: Option<&std::collections::HashMap<String, Vec<String>>>,
    followups: &std::collections::HashMap<String, String>,
    annotations: &std::collections::HashMap<String, Vec<crate::types::QuestionAnnotation>>,
) -> serde_json::Value {
    let mut result = ask_result(status, id, session_id, questions, answers, selections, None);
    if let Some(entries) = result["questions"].as_array_mut() {
        for entry in entries {
            let Some(question) = entry["question"].as_str().map(str::to_string) else {
                continue;
            };
            if let Some(followup) = followups.get(&question) {
                entry["followup"] = serde_json::Value::String(followup.clone());
            }
            if let Some(notes) = annotations.get(&question) {
                if !notes.is_empty() {
                    entry["annotations"] = serde_json::json!(notes);
                }
            }
        }
    }
    if !followups.is_empty() {
        result["guidance"] = serde_json::Value::String(
            "The user wrote follow-up question(s)/notes on specific questions (see              questions[].followup and questions[].annotations). Address them — reply in the              conversation or raise a narrowed re-ask — before treating the unanswered parts              as settled."
                .to_string(),
        );
    }
    result
}

fn guidance_answers(question: &str, guidance: &str) -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([(question.to_string(), guidance.to_string())])
}

/// The "answered" result of an agenda-backed ask, built from the ITEM's
/// recorded answer (the resolver is the single writer; the waiter observes
/// what was durably recorded). A structured resolution expands into the
/// full per-question breakdown; a plain text answer (typed on the Agenda
/// tab) rides as the first question's answer.
fn ask_result_from_item_answer(
    id: u64,
    session_id: &str,
    questions: &[crate::types::UserQuestion],
    answer: &crate::agenda::AgendaAnswer,
) -> serde_json::Value {
    match &answer.structured {
        Some(resolution) => {
            let answers: std::collections::HashMap<String, String> =
                resolution.answers.clone().into_iter().collect();
            let selections: std::collections::HashMap<String, Vec<String>> =
                resolution.selections.clone().into_iter().collect();
            let followups: std::collections::HashMap<String, String> =
                resolution.followups.clone().into_iter().collect();
            let annotations: std::collections::HashMap<
                String,
                Vec<crate::types::QuestionAnnotation>,
            > = resolution.annotations.clone().into_iter().collect();
            ask_result_with_followups(
                "answered",
                id,
                session_id,
                questions,
                answers,
                Some(&selections),
                &followups,
                &annotations,
            )
        }
        None => {
            let first = questions.first().map(|q| q.question.as_str()).unwrap_or("");
            ask_result(
                "answered",
                id,
                session_id,
                questions,
                guidance_answers(first, &answer.text),
                None,
                None,
            )
        }
    }
}

/// Stamp the agenda item id onto an agenda-backed ask result so agents can
/// reference the durable question (`agenda_list`, `ctl agenda`) from every
/// outcome shape.
fn with_item_id(mut result: serde_json::Value, item_id: &str) -> serde_json::Value {
    result["item_id"] = serde_json::Value::String(item_id.to_string());
    result
}

impl IntendantServer {
    #[tool(
        description = "Ask the user one structured question on the dashboard question rail and BLOCK until they answer (or the wait times out). A question requests input, never permission: it is never auto-approved and answering it never widens autonomy. Provide 0-4 options ({label, description?}); with zero options the user types a free-text answer (free text is always allowed on top of options). Optionally attach up to 4 preview cards (previews: [{label, html | image+media_type | text}]) rendered above the options — show, then ask: prototype variants to pick between, or before/after states to judge. html must be one self-contained document (rendered in a locked-down sandboxed frame — external fetches will not resolve; inline CSS/JS, use data: URLs for images); image is base64. Caps: 2 MB per html, 4 MB per image, 4 KB per text, 8 MB total per ask. Or ask up to 4 questions on ONE panel via questions: [{question, header?, options?, pick_min?, pick_max?, free_text?, previews?}] — pick_min/pick_max bound how many options may be selected (minimum 0 = optional question; default exactly one), free_text: false disables typed answers, and every answer returns together. The user can also attach a follow-up per question and anchored preview notes; a follow-up may STAND IN for an answer — address it (reply in conversation or raise a narrowed re-ask) before treating that part as settled. Returns {status, answer, answers, questions: [{question, header, answer, selected?, followup?, annotations?: [{preview, note}]}]}: status \"answered\" carries the user's choice(s); \"timeout\"/\"dismissed\"/\"pass\" carry best-judgment guidance instead — proceed on your own judgment then. Default wait 300s, max 900; the dashboard shows the expiry as a live countdown, and the user may hold the question open — a held ask blocks past the wait until answered or dismissed. On a daemon with the durable agenda (the default daemon shape), a timed-out or abandoned question does NOT evaporate: it stays open on the agenda — the result carries its item_id — and a later answer is delivered back into this session as a user message at a turn boundary. Set park: true to skip blocking entirely: the question files as a durable agenda item and {status:\"parked\", item_id, ask_id} returns immediately (don't combine with wait_seconds); the reply lands on the item and is delivered the same way. Use it before destructive or hard-to-reverse choices; prefer notify_user when you only need to inform."
    )]
    pub(crate) async fn ask_user(&self, Parameters(params): Parameters<AskUserParams>) -> String {
        match self.ask_user_inner(params).await {
            Ok(value) => value.to_string(),
            Err(message) => format!("ask_user failed: {message}"),
        }
    }

    /// Core of `ask_user`, shared by the stdio `#[tool]` method (which has
    /// no gate-resolved actor) and the HTTP dispatch arm (which passes the
    /// caller's binding via [`Self::ask_user_inner_as_actor`] and maps
    /// `Err` to an `isError` tool result).
    pub(crate) async fn ask_user_inner(
        &self,
        params: AskUserParams,
    ) -> Result<serde_json::Value, String> {
        self.ask_user_inner_as_actor(params, &crate::access::actor::ActorBinding::unattributed())
            .await
    }

    pub(crate) async fn ask_user_inner_as_actor(
        &self,
        params: AskUserParams,
        actor: &crate::access::actor::ActorBinding,
    ) -> Result<serde_json::Value, String> {
        if params.park && params.wait_seconds.is_some() {
            return Err(
                "park doesn't wait — drop wait_seconds (parked questions never expire)".to_string(),
            );
        }
        let wait_seconds = params
            .wait_seconds
            .unwrap_or(ASK_USER_DEFAULT_WAIT_SECS)
            .clamp(1, ASK_USER_MAX_WAIT_SECS);

        // Same session resolution as post_session_note: the HTTP dispatch
        // injects the URL-bound session id (`with_default_mcp_session_id`),
        // an explicit argument wins, then the single-session state fallback.
        let (session_id, interactive_frontends, project_root, log_dir, agenda) = {
            let state = self.state.read().await;
            let session_id = params
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    let fallback = state.session_id.trim();
                    if fallback.is_empty() {
                        None
                    } else {
                        Some(fallback.to_string())
                    }
                });
            (
                session_id,
                state.interactive_frontends,
                state.project_root.clone(),
                state.log_dir.clone(),
                state.agenda.clone(),
            )
        };
        let Some(session_id) = session_id else {
            return Err(
                "no session to ask from; pass session_id (or call through the session-scoped \
                 MCP URL Intendant injected)"
                    .to_string(),
            );
        };

        // park: file the question on the durable agenda INSTEAD of
        // blocking — the same item, blob custody, and rail pipeline as
        // `intendant ctl ask --park`. The reply lands on the item; while
        // the asking session lives it is also delivered back as ordinary
        // follow-up input at a turn boundary.
        if params.park {
            let Some(agenda) = agenda else {
                return Err(
                    "cannot park: agenda unavailable on this daemon (no durable agenda \
                     store in this server shape)"
                        .to_string(),
                );
            };
            let questions = park_questions(&params)?;
            let item = agenda
                .apply(
                    crate::agenda::AgendaCommand::Ask { questions },
                    Some(park_actor(actor, &session_id)),
                )
                .map_err(|e| e.to_string())?;
            let ask_id = item.ask.as_ref().map(|ask| ask.ask_id).unwrap_or_default();
            return Ok(serde_json::json!({
                "status": "parked",
                "item_id": item.id,
                "ask_id": ask_id,
                "session_id": session_id,
            }));
        }

        // No frontend can ever answer in this shape (headless without a web
        // gateway): tell the model to proceed instead of blocking on nobody
        // — the same semantic supervised Claude Code gets headless.
        if !interactive_frontends {
            let (built, _) = build_ask_user_questions(&params)?;
            let question_text = built
                .first()
                .map(|(q, _)| q.question.clone())
                .unwrap_or_default();
            let id = crate::event::next_approval_id();
            let content = format!("No user available to answer: {question_text}");
            self.bus.send(AppEvent::LogEntry {
                session_id: Some(session_id.clone()),
                level: "warn".to_string(),
                source: "system".to_string(),
                content,
                turn: None,
            });
            let questions: Vec<crate::types::UserQuestion> =
                built.into_iter().map(|(q, _)| q).collect();
            return Ok(ask_result(
                "auto_answered",
                id,
                &session_id,
                &questions,
                guidance_answers(&question_text, NO_ANSWER_GUIDANCE),
                None,
                Some(NO_ANSWER_GUIDANCE),
            ));
        }

        // Blocking-as-sugar: with a durable agenda available, a blocking
        // ask IS a parked agenda item plus a live waiter on its outcome —
        // the question survives timeouts and dropped transports instead of
        // evaporating with the waiter.
        if let Some(agenda) = agenda {
            return self
                .ask_user_blocking_agenda(agenda, &params, session_id, wait_seconds, actor)
                .await;
        }

        // Legacy ephemeral blocking ask: no agenda store in this server
        // shape (stdio MCP), so the question lives and dies with this
        // waiter exactly as before.
        let (built, _) = build_ask_user_questions(&params)?;
        let question_text = built
            .first()
            .map(|(q, _)| q.question.clone())
            .unwrap_or_default();
        let id = crate::event::next_approval_id();

        // Commit blob previews into the calling session's upload store —
        // references only from here on (mirrors post_session_note): the
        // broadcast, the reconnect state-line cache, and the session log
        // stay small, and browsers fetch the bytes lazily via /raw. A
        // failure rolls back every blob committed for this ask — across
        // all its questions — so a refused ask strands nothing.
        let scope = crate::global_store::StoreScope::resolve(project_root.as_deref());
        let mut questions: Vec<crate::types::UserQuestion> = Vec::with_capacity(built.len());
        let mut committed_ids: Vec<String> = Vec::new();
        let mut commit_error: Option<String> = None;
        'questions: for (mut question, decoded_previews) in built {
            let mut committed: Vec<crate::types::QuestionPreview> = Vec::new();
            for preview in decoded_previews {
                let source =
                    match preview.source {
                        DecodedPreviewSource::Text(content) => {
                            Ok(crate::types::QuestionPreviewSource::Text { content })
                        }
                        DecodedPreviewSource::Html(html) => commit_preview_blob(
                            html.as_bytes(),
                            &preview.label,
                            "html",
                            "text/html",
                            &log_dir,
                            &session_id,
                            &scope,
                        )
                        .map(|descriptor| crate::types::QuestionPreviewSource::Html {
                            url: preview_raw_url(&descriptor.id),
                            upload_id: descriptor.id,
                        }),
                        DecodedPreviewSource::Image { mime, bytes } => commit_preview_blob(
                            &bytes,
                            &preview.label,
                            super::tools_notes::note_image_extension(mime),
                            mime,
                            &log_dir,
                            &session_id,
                            &scope,
                        )
                        .map(|descriptor| crate::types::QuestionPreviewSource::Image {
                            url: preview_raw_url(&descriptor.id),
                            upload_id: descriptor.id,
                            mime: mime.to_string(),
                        }),
                    };
                match source {
                    Ok(source) => {
                        if let crate::types::QuestionPreviewSource::Html { upload_id, .. }
                        | crate::types::QuestionPreviewSource::Image { upload_id, .. } = &source
                        {
                            committed_ids.push(upload_id.clone());
                        }
                        committed.push(crate::types::QuestionPreview {
                            label: preview.label,
                            source,
                        });
                    }
                    Err(message) => {
                        commit_error = Some(message);
                        break 'questions;
                    }
                }
            }
            question.previews = committed;
            questions.push(question);
        }
        if let Some(message) = commit_error {
            for upload_id in &committed_ids {
                let _ = crate::upload_store::delete_upload(upload_id, &log_dir, &scope);
            }
            return Err(message);
        }

        // Subscribe BEFORE announcing the question (same race as approvals:
        // an instant answer must find the waiter listening).
        let mut events = self.bus.subscribe();
        let mut guard = PendingAskGuard::register(id, Some(session_id.clone()), self.bus.clone());
        let mut ask_deadline = AskDeadline::new(
            tokio::time::Instant::now(),
            std::time::Duration::from_secs(wait_seconds),
        );
        self.bus.send(AppEvent::UserQuestionRequired {
            session_id: Some(session_id.clone()),
            id,
            questions: questions.clone(),
            expires_at_ms: ask_deadline
                .expires_at_unix_ms(tokio::time::Instant::now(), now_unix_ms()),
            held: false,
        });

        loop {
            let wake_at = ask_deadline.wake_at(tokio::time::Instant::now());
            let event = match tokio::time::timeout_at(wake_at, events.recv()).await {
                Err(_) => {
                    // Timed out: clear the rail and hand back guidance.
                    // Unreachable while held — `wake_at` parks a year out
                    // and is re-armed every iteration.
                    guard.resolve("timeout");
                    let guidance = format!(
                        "No answer arrived within {wait_seconds}s. Proceed using your best \
                         judgment based on the context so far; you can re-ask later if it is \
                         still relevant."
                    );
                    return Ok(ask_result(
                        "timeout",
                        id,
                        &session_id,
                        &questions,
                        guidance_answers(&question_text, &guidance),
                        None,
                        Some(&guidance),
                    ));
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    guard.resolve("cancelled");
                    return Ok(ask_result(
                        "dismissed",
                        id,
                        &session_id,
                        &questions,
                        guidance_answers(&question_text, DISMISSED_GUIDANCE),
                        None,
                        Some(DISMISSED_GUIDANCE),
                    ));
                }
                Ok(Ok(event)) => event,
            };
            // Every frontend intent rides the bus as a ControlCommand
            // (web /ws, dashboard tunnel, control socket, MCP twins), so
            // matching here resolves uniformly across daemon shapes. Ids
            // from the process-wide allocator are globally unique — match
            // on id alone.
            let AppEvent::ControlCommand(msg) = event else {
                continue;
            };
            match msg {
                // Hold flip: suspend or resume the countdown, then
                // re-announce the question with the same id so every
                // frontend (and the reconnect state-line cache) carries the
                // current hold state. A same-state repeat is a no-op.
                ControlMsg::HoldQuestion {
                    id: verb_id, held, ..
                } if verb_id == id => {
                    let now = tokio::time::Instant::now();
                    if ask_deadline.set_held(held, now) {
                        self.bus.send(AppEvent::UserQuestionRequired {
                            session_id: Some(session_id.clone()),
                            id,
                            questions: questions.clone(),
                            expires_at_ms: ask_deadline.expires_at_unix_ms(now, now_unix_ms()),
                            held: ask_deadline.held(),
                        });
                    }
                    continue;
                }
                ControlMsg::AnswerQuestion {
                    id: answer_id,
                    answers,
                    selections,
                    followups,
                    annotations,
                    ..
                } if answer_id == id => {
                    guard.resolve("answer");
                    return Ok(ask_result_with_followups(
                        "answered",
                        id,
                        &session_id,
                        &questions,
                        answers,
                        Some(&selections),
                        &followups,
                        &annotations,
                    ));
                }
                // A bare approve comes from callers that only speak the
                // approval verbs. It cannot carry a choice — let the model
                // proceed on its own judgment rather than fabricating one.
                // ApproveAll on a question deliberately does NOT widen
                // autonomy: this waiter only returns text.
                ControlMsg::Approve { id: verb_id, .. }
                | ControlMsg::ApproveAll { id: verb_id, .. }
                    if verb_id == id =>
                {
                    guard.resolve("approve");
                    return Ok(ask_result(
                        "pass",
                        id,
                        &session_id,
                        &questions,
                        guidance_answers(&question_text, PASS_GUIDANCE),
                        None,
                        Some(PASS_GUIDANCE),
                    ));
                }
                ControlMsg::Deny { id: verb_id, .. } if verb_id == id => {
                    guard.resolve("deny");
                    return Ok(ask_result(
                        "dismissed",
                        id,
                        &session_id,
                        &questions,
                        guidance_answers(&question_text, DISMISSED_GUIDANCE),
                        None,
                        Some(DISMISSED_GUIDANCE),
                    ));
                }
                ControlMsg::Skip { id: verb_id, .. } if verb_id == id => {
                    guard.resolve("skip");
                    return Ok(ask_result(
                        "dismissed",
                        id,
                        &session_id,
                        &questions,
                        guidance_answers(&question_text, DISMISSED_GUIDANCE),
                        None,
                        Some(DISMISSED_GUIDANCE),
                    ));
                }
                _ => continue,
            }
        }
    }

    /// Agenda-backed blocking ask (blocking-as-sugar over a parked item).
    ///
    /// The item is created exactly like a park — same validation, blob
    /// custody in the agenda store, minted item + rail ids — then this
    /// waiter announces the question WITH its deadline and waits for the
    /// ITEM's outcome. Resolution ordering has one owner: the daemon-side
    /// resolver (`agenda/ask.rs`) and the Agenda surfaces write outcomes
    /// onto the item; this waiter only OBSERVES the resulting
    /// `AgendaAskOutcome` — so a rail answer, an Agenda-tab answer, and a
    /// complete/retire all resolve the wait identically, and the waiter
    /// can never double-record. On timeout the question does not
    /// evaporate: the item stays open, the rail card converts to its
    /// parked (non-expiring) form, and a later answer flows back into the
    /// still-live session through the supervisor's follow-up delivery.
    async fn ask_user_blocking_agenda(
        &self,
        agenda: std::sync::Arc<crate::agenda::AgendaHandle>,
        params: &AskUserParams,
        session_id: String,
        wait_seconds: u64,
        actor: &crate::access::actor::ActorBinding,
    ) -> Result<serde_json::Value, String> {
        let park = park_questions(params)?;
        let item = agenda
            .park_ask_for_waiter(park, Some(park_actor(actor, &session_id)))
            .map_err(|e| e.to_string())?;
        let ask = item
            .ask
            .clone()
            .ok_or_else(|| "internal: parked item carries no ask payload".to_string())?;
        let id = ask.ask_id;
        let questions = ask.questions;
        let item_id = item.id.clone();
        let question_text = questions
            .first()
            .map(|q| q.question.clone())
            .unwrap_or_default();

        // Subscribe BEFORE announcing (same race discipline as approvals:
        // an instant outcome must find the waiter listening). The waiter
        // itself was registered by `park_ask_for_waiter` before the item
        // became visible.
        let mut events = self.bus.subscribe();
        let mut guard = AgendaAskGuard::adopt(
            id,
            Some(session_id.clone()),
            questions.clone(),
            self.bus.clone(),
        );
        let mut ask_deadline = AskDeadline::new(
            tokio::time::Instant::now(),
            std::time::Duration::from_secs(wait_seconds),
        );
        self.bus.send(AppEvent::UserQuestionRequired {
            session_id: Some(session_id.clone()),
            id,
            questions: questions.clone(),
            expires_at_ms: ask_deadline
                .expires_at_unix_ms(tokio::time::Instant::now(), now_unix_ms()),
            held: false,
        });

        let dismissed_result = |item_id: &str| {
            with_item_id(
                ask_result(
                    "dismissed",
                    id,
                    &session_id,
                    &questions,
                    guidance_answers(&question_text, DISMISSED_GUIDANCE),
                    None,
                    Some(DISMISSED_GUIDANCE),
                ),
                item_id,
            )
        };

        loop {
            let wake_at = ask_deadline.wake_at(tokio::time::Instant::now());
            let event = match tokio::time::timeout_at(wake_at, events.recv()).await {
                Err(_) => {
                    // The wait lapsed. Heal a lagged broadcast first: an
                    // outcome recorded moments ago is read back from the
                    // ledger instead of being reported as a timeout.
                    if let Some(current) = agenda.item_by_id(&item_id) {
                        if current.status != crate::agenda::AgendaStatus::Open {
                            guard.resolve();
                            if let Some(answer) = &current.answer {
                                return Ok(with_item_id(
                                    ask_result_from_item_answer(
                                        id,
                                        &session_id,
                                        &questions,
                                        answer,
                                    ),
                                    &item_id,
                                ));
                            }
                            return Ok(dismissed_result(&item_id));
                        }
                    }
                    // Genuine timeout: stop waiting, keep the question.
                    // The guard converts the rail card to its parked form.
                    guard.abandon_to_parked();
                    let guidance = format!(
                        "No answer arrived within {wait_seconds}s, so you stopped waiting — \
                         but the question did not expire: it stays OPEN on the agenda as \
                         item {item_id}, and the owner can still answer it from the question \
                         rail or the Agenda tab. A later answer will be delivered into this \
                         session as a user message while the session is running. If you \
                         proceed provisionally, say so in the conversation; you can note \
                         the provisional choice on the item with `intendant ctl agenda \
                         patch`."
                    );
                    return Ok(with_item_id(
                        ask_result(
                            "timeout",
                            id,
                            &session_id,
                            &questions,
                            guidance_answers(&question_text, &guidance),
                            None,
                            Some(&guidance),
                        ),
                        &item_id,
                    ));
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    // Bus gone (shutdown): nothing left to announce to.
                    guard.resolve();
                    return Ok(dismissed_result(&item_id));
                }
                Ok(Ok(event)) => event,
            };
            match event {
                // Hold flip: suspend or resume the countdown, then
                // re-announce with the same id so every frontend (and the
                // reconnect state-line cache) carries the current state.
                AppEvent::ControlCommand(ControlMsg::HoldQuestion {
                    id: verb_id, held, ..
                }) if verb_id == id => {
                    let now = tokio::time::Instant::now();
                    if ask_deadline.set_held(held, now) {
                        self.bus.send(AppEvent::UserQuestionRequired {
                            session_id: Some(session_id.clone()),
                            id,
                            questions: questions.clone(),
                            expires_at_ms: ask_deadline.expires_at_unix_ms(now, now_unix_ms()),
                            held: ask_deadline.held(),
                        });
                    }
                }
                // The item's recorded outcome — the single source of
                // resolution, whatever surface wrote it.
                AppEvent::AgendaAskOutcome {
                    item: outcome,
                    action,
                    ..
                } if outcome.id == item_id => {
                    guard.resolve();
                    return Ok(match action.as_str() {
                        "answer" => match &outcome.answer {
                            Some(answer) => with_item_id(
                                ask_result_from_item_answer(id, &session_id, &questions, answer),
                                &item_id,
                            ),
                            None => dismissed_result(&item_id),
                        },
                        // A bare approve carries no choice — proceed on
                        // best judgment (never widens autonomy).
                        "approve" | "approve_all" => with_item_id(
                            ask_result(
                                "pass",
                                id,
                                &session_id,
                                &questions,
                                guidance_answers(&question_text, PASS_GUIDANCE),
                                None,
                                Some(PASS_GUIDANCE),
                            ),
                            &item_id,
                        ),
                        // skip/deny (item stays open, dismissal marker
                        // recorded) and complete/retire (closed without an
                        // answer) all read as a dismissal to the agent.
                        _ => dismissed_result(&item_id),
                    });
                }
                _ => continue,
            }
        }
    }

    #[tool(
        description = "Send the user a fire-and-forget notification and return immediately (never blocks, never enters model context). urgency escalates delivery: \"info\" (default) renders a dashboard toast + transcript row; \"attention\" additionally badges the tab and raises a browser notification when the tab is hidden; \"urgent\" additionally pushes an immediate content-free nudge to the owner's opted-in browsers via the rendezvous — reserve urgent for being blocked or something requiring prompt human action. Caps: 4 KB text. Use ask_user instead when you need an answer."
    )]
    pub(crate) async fn notify_user(
        &self,
        Parameters(params): Parameters<NotifyUserParams>,
    ) -> String {
        match self.notify_user_inner(params).await {
            Ok(value) => value.to_string(),
            Err(message) => format!("notify_user failed: {message}"),
        }
    }

    /// Core of `notify_user`, shared by the stdio `#[tool]` method and the
    /// HTTP dispatch arm (which maps `Err` to an `isError` tool result).
    pub(crate) async fn notify_user_inner(
        &self,
        params: NotifyUserParams,
    ) -> Result<serde_json::Value, String> {
        let text = params.text.trim();
        if text.is_empty() {
            return Err("notification text must not be empty".to_string());
        }
        if text.len() > NOTIFY_USER_MAX_TEXT_BYTES {
            return Err(format!(
                "notification text is {} bytes; max {} KB",
                text.len(),
                NOTIFY_USER_MAX_TEXT_BYTES / 1024
            ));
        }
        let urgency = crate::types::NotificationUrgency::parse(params.urgency.as_deref())?;
        let title = params
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| crate::types::truncate_str(t, 120).to_string());

        let session_id = {
            let state = self.state.read().await;
            params
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    let fallback = state.session_id.trim();
                    if fallback.is_empty() {
                        None
                    } else {
                        Some(fallback.to_string())
                    }
                })
        };
        let Some(session_id) = session_id else {
            return Err(
                "no session to notify from; pass session_id (or call through the \
                 session-scoped MCP URL Intendant injected)"
                    .to_string(),
            );
        };

        let id = format!("notif-{}", Uuid::new_v4().simple());
        self.bus.send(AppEvent::UserNotification {
            session_id: Some(session_id.clone()),
            id: id.clone(),
            title,
            text: text.to_string(),
            urgency,
            ts: now_unix_ms(),
        });

        Ok(serde_json::json!({
            "status": "sent",
            "id": id,
            "session_id": session_id,
            "urgency": urgency.as_str(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NotificationUrgency;

    /// Hold suspends the deadline; resume re-arms it with exactly the time
    /// that remained at the moment of the hold — however long the hold
    /// lasted in wall time.
    #[tokio::test]
    async fn ask_deadline_hold_preserves_remaining() {
        let t0 = tokio::time::Instant::now();
        let wait = std::time::Duration::from_secs(300);
        let mut d = AskDeadline::new(t0, wait);
        assert!(!d.held());
        assert_eq!(d.wake_at(t0), t0 + wait);
        assert_eq!(
            d.expires_at_unix_ms(t0, 1_000_000),
            Some(1_000_000 + 300_000)
        );

        // Hold with 100s elapsed → 200s remain.
        let t1 = t0 + std::time::Duration::from_secs(100);
        assert!(d.set_held(true, t1));
        assert!(d.held());
        assert_eq!(d.expires_at_unix_ms(t1, 2_000_000), None);
        // While held the wake instant is parked far out (≥ a day).
        assert!(d.wake_at(t1) >= t1 + std::time::Duration::from_secs(86_400));
        // Same-state repeat is a no-op (waiter must not re-announce).
        assert!(!d.set_held(true, t1));

        // Resume much later: countdown re-arms with the 200s that remained.
        let t2 = t1 + std::time::Duration::from_secs(9_999);
        assert!(d.set_held(false, t2));
        assert!(!d.held());
        assert_eq!(d.wake_at(t2), t2 + std::time::Duration::from_secs(200));
        assert_eq!(
            d.expires_at_unix_ms(t2, 3_000_000),
            Some(3_000_000 + 200_000)
        );
        assert!(!d.set_held(false, t2));
    }

    /// Holding after the deadline already passed resumes with zero
    /// remaining (saturating) — the next un-held wake fires immediately
    /// instead of underflowing.
    #[tokio::test]
    async fn ask_deadline_hold_after_expiry_saturates() {
        let t0 = tokio::time::Instant::now();
        let mut d = AskDeadline::new(t0, std::time::Duration::from_secs(10));
        let late = t0 + std::time::Duration::from_secs(60);
        assert!(d.set_held(true, late));
        assert!(d.set_held(false, late));
        assert_eq!(d.wake_at(late), late);
    }

    fn question_param(question: &str) -> crate::mcp::tool_params::AskUserQuestionParams {
        crate::mcp::tool_params::AskUserQuestionParams {
            question: question.to_string(),
            header: None,
            options: vec![],
            previews: vec![],
            pick_min: None,
            pick_max: None,
            free_text: None,
        }
    }

    fn labeled_options(labels: &[&str]) -> Vec<AskUserOptionParams> {
        labels
            .iter()
            .map(|l| AskUserOptionParams {
                label: l.to_string(),
                description: None,
            })
            .collect()
    }

    #[test]
    fn multi_question_form_builds_and_prefixes_errors() {
        let mut params = ask_params("");
        params.questions = vec![question_param("Which lineage?"), question_param("Headers?")];
        params.questions[0].header = Some("Lineage".into());
        params.questions[0].options = labeled_options(&["A", "B", "C"]);
        params.questions[0].pick_min = Some(1);
        params.questions[0].pick_max = Some(2);
        params.questions[1].pick_min = Some(0);
        let (built, _) = build_ask_user_questions(&params).unwrap();
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].0.header, "Lineage");
        assert_eq!(built[0].0.pick_bounds(), (1, 2));
        // Optional second question (min 0, free text only).
        assert_eq!(built[1].0.pick_bounds(), (0, 1));

        // Errors carry the questions[N] prefix.
        let mut params = ask_params("");
        params.questions = vec![question_param("ok"), question_param("  ")];
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.starts_with("questions[1]: "), "{err}");
    }

    #[test]
    fn multi_question_form_validates_shape() {
        // Both forms at once refuse.
        let mut params = ask_params("flat question");
        params.questions = vec![question_param("also this")];
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.contains("not both"), "{err}");

        // Too many questions refuse.
        let mut params = ask_params("");
        params.questions = (0..ASK_USER_MAX_QUESTIONS + 1)
            .map(|i| question_param(&format!("q{i}")))
            .collect();
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.contains("too many questions"), "{err}");
    }

    #[test]
    fn pick_bound_validation_refuses_impossible_schemas() {
        // pick_max above the option count.
        let mut params = ask_params("");
        params.questions = vec![question_param("q")];
        params.questions[0].options = labeled_options(&["A", "B"]);
        params.questions[0].pick_max = Some(3);
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.contains("pick_max 3 exceeds"), "{err}");

        // min above max.
        let mut params = ask_params("");
        params.questions = vec![question_param("q")];
        params.questions[0].options = labeled_options(&["A", "B", "C"]);
        params.questions[0].pick_min = Some(3);
        params.questions[0].pick_max = Some(2);
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.contains("pick_min 3 exceeds pick_max 2"), "{err}");

        // Bounds without options.
        let mut params = ask_params("");
        params.questions = vec![question_param("q")];
        params.questions[0].pick_max = Some(2);
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.contains("needs options"), "{err}");

        // free_text: false with nothing to pick.
        let mut params = ask_params("");
        params.questions = vec![question_param("q")];
        params.questions[0].free_text = Some(false);
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.contains("leaves nothing to answer"), "{err}");
    }

    #[test]
    fn user_question_pick_bounds_derivation() {
        // Legacy default: exactly one.
        let mut q = crate::types::UserQuestion {
            question: "q".into(),
            header: String::new(),
            options: vec![],
            multi_select: false,
            pick_min: None,
            pick_max: None,
            free_text: None,
            previews: Vec::new(),
        };
        assert_eq!(q.pick_bounds(), (1, 1));
        assert!(q.free_text_allowed());
        // Legacy multi_select: any number, at least one.
        q.options = vec![
            crate::types::UserQuestionOption {
                label: "A".into(),
                description: String::new(),
            },
            crate::types::UserQuestionOption {
                label: "B".into(),
                description: String::new(),
            },
            crate::types::UserQuestionOption {
                label: "C".into(),
                description: String::new(),
            },
        ];
        q.multi_select = true;
        assert_eq!(q.pick_bounds(), (1, 3));
        // Explicit bounds win; clamped to the option count.
        q.pick_min = Some(0);
        q.pick_max = Some(9);
        assert_eq!(q.pick_bounds(), (0, 3));
        q.free_text = Some(false);
        assert!(!q.free_text_allowed());
    }

    #[test]
    fn preview_budget_spans_the_whole_ask() {
        // Each html stays under the 2 MB per-document cap, but the 8 MB
        // per-ASK budget crosses on the fifth card — in the SECOND
        // question, proving the budget is shared, not per-question.
        let big = "x".repeat(1_900_000);
        let mut params = ask_params("");
        let mut q1 = question_param("first");
        q1.previews = vec![html_preview("p1", &big), html_preview("p2", &big)];
        let mut q2 = question_param("second");
        q2.previews = vec![
            html_preview("p3", &big),
            html_preview("p4", &big),
            html_preview("p5", &big),
        ];
        params.questions = vec![q1, q2];
        let err = build_ask_user_questions(&params).unwrap_err();
        assert!(err.starts_with("questions[1]: "), "{err}");
        assert!(err.contains("total preview payload"), "{err}");
    }

    #[test]
    fn ask_result_followups_and_annotations_ride_their_questions() {
        let questions = vec![
            crate::types::UserQuestion {
                question: "Which lineage?".into(),
                header: "Lineage".into(),
                options: vec![],
                multi_select: false,
                pick_min: None,
                pick_max: None,
                free_text: None,
                previews: Vec::new(),
            },
            crate::types::UserQuestion {
                question: "Which headers?".into(),
                header: "Headers".into(),
                options: vec![],
                multi_select: false,
                pick_min: None,
                pick_max: None,
                free_text: None,
                previews: Vec::new(),
            },
        ];
        // Q1 answered with an anchored note; Q2 unanswered, follow-up only.
        let answers =
            std::collections::HashMap::from([("Which lineage?".to_string(), "B".to_string())]);
        let followups = std::collections::HashMap::from([(
            "Which headers?".to_string(),
            "What does 'monoline' mean here?".to_string(),
        )]);
        let annotations = std::collections::HashMap::from([(
            "Which lineage?".to_string(),
            vec![crate::types::QuestionAnnotation {
                preview: "B · Lineage lanes".to_string(),
                note: "rails too faint".to_string(),
            }],
        )]);
        let result = ask_result_with_followups(
            "answered",
            4,
            "sess",
            &questions,
            answers,
            None,
            &followups,
            &annotations,
        );
        let per = result["questions"].as_array().unwrap();
        assert_eq!(per[0]["answer"], "B");
        assert_eq!(per[0]["annotations"][0]["preview"], "B · Lineage lanes");
        assert!(per[0].get("followup").is_none());
        assert_eq!(per[1]["answer"], "");
        assert_eq!(per[1]["followup"], "What does 'monoline' mean here?");
        // The guidance nudge exists exactly because a follow-up arrived.
        assert!(result["guidance"]
            .as_str()
            .unwrap()
            .contains("follow-up question(s)"));
    }

    #[test]
    fn ask_result_carries_per_question_breakdown() {
        let questions = vec![
            crate::types::UserQuestion {
                question: "Which lineage?".into(),
                header: "Lineage".into(),
                options: vec![],
                multi_select: false,
                pick_min: None,
                pick_max: None,
                free_text: None,
                previews: Vec::new(),
            },
            crate::types::UserQuestion {
                question: "Which headers?".into(),
                header: "Headers".into(),
                options: vec![],
                multi_select: false,
                pick_min: Some(0),
                pick_max: None,
                free_text: None,
                previews: Vec::new(),
            },
        ];
        let answers = std::collections::HashMap::from([
            ("Which lineage?".to_string(), "B".to_string()),
            ("Which headers?".to_string(), "icons, one-pill".to_string()),
        ]);
        let selections = std::collections::HashMap::from([(
            "Which headers?".to_string(),
            vec!["icons".to_string(), "one-pill".to_string()],
        )]);
        let result = ask_result(
            "answered",
            9,
            "sess",
            &questions,
            answers,
            Some(&selections),
            None,
        );
        assert_eq!(result["question"], "Which lineage?");
        assert_eq!(result["answer"], "B");
        let per = result["questions"].as_array().unwrap();
        assert_eq!(per.len(), 2);
        assert_eq!(per[0]["header"], "Lineage");
        assert_eq!(per[0]["answer"], "B");
        assert!(per[0].get("selected").is_none());
        assert_eq!(per[1]["selected"][1], "one-pill");
    }

    fn test_server(session_id: &str, interactive: bool) -> (IntendantServer, EventBus) {
        let bus = EventBus::new();
        let mut state = McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        state.session_id = session_id.to_string();
        state.interactive_frontends = interactive;
        let server = IntendantServer::new(Arc::new(RwLock::new(state)), bus.clone());
        (server, bus)
    }

    fn ask_params(question: &str) -> AskUserParams {
        AskUserParams {
            question: question.to_string(),
            header: None,
            options: vec![],
            previews: vec![],
            multi_select: None,
            pick_min: None,
            pick_max: None,
            free_text: None,
            questions: vec![],
            wait_seconds: None,
            park: false,
            session_id: None,
        }
    }

    /// Flat-form shim over the multi-form builder for the legacy tests:
    /// same tuple shape the old single-question builder returned.
    fn build_flat(
        params: &AskUserParams,
    ) -> Result<(crate::types::UserQuestion, Vec<DecodedPreview>, u64), String> {
        let (mut built, wait) = build_ask_user_questions(params)?;
        assert_eq!(built.len(), 1, "flat form builds one question");
        let (question, previews) = built.remove(0);
        Ok((question, previews, wait))
    }

    fn preview_params(label: &str) -> AskUserPreviewParams {
        AskUserPreviewParams {
            label: label.to_string(),
            html: None,
            image: None,
            media_type: None,
            text: None,
        }
    }

    fn html_preview(label: &str, html: &str) -> AskUserPreviewParams {
        AskUserPreviewParams {
            html: Some(html.to_string()),
            ..preview_params(label)
        }
    }

    fn text_preview(label: &str, text: &str) -> AskUserPreviewParams {
        AskUserPreviewParams {
            text: Some(text.to_string()),
            ..preview_params(label)
        }
    }

    fn image_preview(label: &str, media_type: Option<&str>, bytes: &[u8]) -> AskUserPreviewParams {
        use base64::Engine as _;
        AskUserPreviewParams {
            image: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            media_type: media_type.map(str::to_string),
            ..preview_params(label)
        }
    }

    async fn next_event(
        rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        what: &str,
    ) -> AppEvent {
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("bus open")
    }

    #[test]
    fn ask_ids_use_the_process_wide_wire_allocator() {
        let first = crate::event::next_approval_id();
        let second = crate::event::next_approval_id();
        assert!(second > first);
        assert!(second <= (1_u64 << 53) - 1);
    }

    #[test]
    fn build_ask_user_question_validates_and_normalizes() {
        // Empty question / too many options / empty label all refuse.
        let err = build_flat(&ask_params("  ")).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");

        let mut params = ask_params("Pick one");
        params.options = (0..ASK_USER_MAX_OPTIONS + 1)
            .map(|i| AskUserOptionParams {
                label: format!("option {i}"),
                description: None,
            })
            .collect();
        let err = build_flat(&params).unwrap_err();
        assert!(err.contains("too many options"), "{err}");

        let mut params = ask_params("Pick one");
        params.options = vec![AskUserOptionParams {
            label: "  ".into(),
            description: None,
        }];
        let err = build_flat(&params).unwrap_err();
        assert!(err.contains("label must not be empty"), "{err}");

        // Happy path: trims, defaults, clamps.
        let mut params = ask_params("  Deploy now?  ");
        params.header = Some(" Release ".into());
        params.options = vec![
            AskUserOptionParams {
                label: " Yes ".into(),
                description: Some(" Ship it ".into()),
            },
            AskUserOptionParams {
                label: "No".into(),
                description: None,
            },
        ];
        params.multi_select = Some(true);
        params.wait_seconds = Some(10_000);
        let (question, previews, wait) = build_flat(&params).unwrap();
        assert_eq!(question.question, "Deploy now?");
        assert_eq!(question.header, "Release");
        assert_eq!(question.options.len(), 2);
        assert_eq!(question.options[0].label, "Yes");
        assert_eq!(question.options[0].description, "Ship it");
        assert!(question.multi_select);
        assert!(previews.is_empty());
        // Previews stay empty on the question until the inner path commits
        // the decoded cards into the calling session's upload store.
        assert!(question.previews.is_empty());
        assert_eq!(wait, ASK_USER_MAX_WAIT_SECS);

        let mut params = ask_params("Quick?");
        params.wait_seconds = Some(0);
        let (_, _, wait) = build_flat(&params).unwrap();
        assert_eq!(wait, 1);
    }

    #[test]
    fn decode_ask_previews_validates_kinds_and_caps() {
        // Count cap.
        let too_many: Vec<AskUserPreviewParams> = (0..ASK_USER_MAX_PREVIEWS + 1)
            .map(|i| text_preview(&format!("p{i}"), "snippet"))
            .collect();
        let err = decode_ask_previews(&too_many, &mut 0).unwrap_err();
        assert!(err.contains("too many previews"), "{err}");

        // Label required; exactly one source required.
        let err = decode_ask_previews(&[text_preview("  ", "snippet")], &mut 0).unwrap_err();
        assert!(err.contains("label must not be empty"), "{err}");
        let err = decode_ask_previews(&[preview_params("A")], &mut 0).unwrap_err();
        assert!(err.contains("exactly one of html, image, or text"), "{err}");
        let both = AskUserPreviewParams {
            text: Some("t".into()),
            ..html_preview("A", "<html></html>")
        };
        let err = decode_ask_previews(&[both], &mut 0).unwrap_err();
        assert!(err.contains("exactly one of html, image, or text"), "{err}");

        // Per-kind caps and validation.
        let err = decode_ask_previews(&[html_preview("A", "   ")], &mut 0).unwrap_err();
        assert!(err.contains("html must not be empty"), "{err}");
        let err = decode_ask_previews(
            &[html_preview("A", &"x".repeat(ASK_USER_MAX_HTML_BYTES + 1))],
            &mut 0,
        )
        .unwrap_err();
        assert!(err.contains("max 2 MB"), "{err}");
        let err = decode_ask_previews(&[image_preview("A", None, b"png")], &mut 0).unwrap_err();
        assert!(err.contains("media_type is required"), "{err}");
        let err = decode_ask_previews(
            &[image_preview("A", Some("image/svg+xml"), b"<svg/>")],
            &mut 0,
        )
        .unwrap_err();
        assert!(err.contains("unsupported media_type"), "{err}");
        let err = decode_ask_previews(
            &[text_preview(
                "A",
                &"x".repeat(ASK_USER_MAX_TEXT_PREVIEW_BYTES + 1),
            )],
            &mut 0,
        )
        .unwrap_err();
        assert!(err.contains("max 4 KB"), "{err}");

        // Total cap: individually legal cards that exceed the ask budget
        // together (2 × 4 MB images + one html crosses 8 MB).
        let big_image = vec![0u8; super::super::tools_notes::SESSION_NOTE_MAX_IMAGE_BYTES];
        let err = decode_ask_previews(
            &[
                image_preview("A", Some("image/png"), &big_image),
                image_preview("B", Some("image/png"), &big_image),
                html_preview("C", "<html><body>c</body></html>"),
            ],
            &mut 0,
        )
        .unwrap_err();
        assert!(err.contains("total preview payload"), "{err}");

        // Happy path: one of each kind; labels trim and truncate.
        let decoded = decode_ask_previews(
            &[
                html_preview("  A — dense layout  ", "<html><body>a</body></html>"),
                image_preview("B", Some("image/jpg"), b"\xff\xd8jpeg"),
                text_preview(&"L".repeat(200), "diff --git a b"),
            ],
            &mut 0,
        )
        .unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].label, "A — dense layout");
        assert!(
            matches!(&decoded[0].source, DecodedPreviewSource::Html(h) if h.contains("<body>a</body>"))
        );
        // image/jpg canonicalizes to image/jpeg like note attachments.
        assert!(
            matches!(&decoded[1].source, DecodedPreviewSource::Image { mime, bytes } if *mime == "image/jpeg" && bytes.starts_with(b"\xff\xd8"))
        );
        assert_eq!(decoded[2].label.len(), 80);
        assert!(
            matches!(&decoded[2].source, DecodedPreviewSource::Text(t) if t == "diff --git a b")
        );
    }

    /// The documented preview caps must always fit inside the `/mcp` body
    /// cap. Images inflate 4/3 in base64; html and text ride as JSON
    /// strings, whose escaping stays under ~3/2 for real documents (the
    /// pathological all-control-character document would exceed it and is
    /// simply refused by the gateway's body cap with a 413 — the contract
    /// documents self-contained HTML, not binary blobs).
    #[test]
    fn documented_preview_caps_fit_inside_mcp_body_cap() {
        let worst_wire = ASK_USER_MAX_TOTAL_PREVIEW_BYTES.div_ceil(2) * 3;
        let envelope_headroom = 64 * 1024;
        assert!(
            worst_wire + ASK_USER_MAX_QUESTION_BYTES + envelope_headroom
                < crate::gateway_routes::MCP_BODY_CAP_BYTES,
            "ask preview caps ({worst_wire} wire bytes) exceed the /mcp body cap {}",
            crate::gateway_routes::MCP_BODY_CAP_BYTES,
        );
    }

    /// Pins the preview wire shape the dashboard reads (flattened
    /// `kind`-tagged source alongside `label`).
    #[test]
    fn question_preview_wire_shape_is_pinned() {
        let question = crate::types::UserQuestion {
            question: "Which?".into(),
            header: String::new(),
            options: Vec::new(),
            multi_select: false,
            pick_min: None,
            pick_max: None,
            free_text: None,
            previews: vec![
                crate::types::QuestionPreview {
                    label: "A".into(),
                    source: crate::types::QuestionPreviewSource::Html {
                        upload_id: "u1".into(),
                        url: "/api/session/current/uploads/u1/raw".into(),
                    },
                },
                crate::types::QuestionPreview {
                    label: "B".into(),
                    source: crate::types::QuestionPreviewSource::Image {
                        upload_id: "u2".into(),
                        mime: "image/png".into(),
                        url: "/api/session/current/uploads/u2/raw".into(),
                    },
                },
                crate::types::QuestionPreview {
                    label: "C".into(),
                    source: crate::types::QuestionPreviewSource::Text {
                        content: "snippet".into(),
                    },
                },
            ],
        };
        let wire = serde_json::to_value(&question).unwrap();
        assert_eq!(
            wire["previews"],
            serde_json::json!([
                {"label": "A", "kind": "html", "upload_id": "u1", "url": "/api/session/current/uploads/u1/raw"},
                {"label": "B", "kind": "image", "upload_id": "u2", "mime": "image/png", "url": "/api/session/current/uploads/u2/raw"},
                {"label": "C", "kind": "text", "content": "snippet"},
            ])
        );
        let back: crate::types::UserQuestion = serde_json::from_value(wire).unwrap();
        assert_eq!(back, question);

        // A previewless question serializes without the key at all, so
        // older dashboards and the state-line replay cache see the exact
        // pre-preview wire shape.
        let bare = crate::types::UserQuestion {
            previews: Vec::new(),
            ..question
        };
        let wire = serde_json::to_value(&bare).unwrap();
        assert!(wire.get("previews").is_none());
    }

    #[tokio::test]
    async fn ask_user_blocks_until_answered_and_clears_the_rail() {
        let (server, bus) = test_server("sess-ask", true);
        let mut rx = bus.subscribe();

        let ask_server = server.clone();
        let mut params = ask_params("Which color?");
        params.options = vec![AskUserOptionParams {
            label: "blue".into(),
            description: None,
        }];
        let ask = tokio::spawn(async move { ask_server.ask_user_inner(params).await });

        // The rail event announces the question with the armed id, a real
        // wall-clock expiry, and no hold.
        let (id, questions) = match next_event(&mut rx, "UserQuestionRequired").await {
            AppEvent::UserQuestionRequired {
                session_id,
                id,
                questions,
                expires_at_ms,
                held,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-ask"));
                assert!(
                    expires_at_ms.is_some(),
                    "ask_user always arms a daemon-side deadline"
                );
                assert!(!held);
                (id, questions)
            }
            other => panic!("expected UserQuestionRequired, got {other:?}"),
        };
        assert!(id <= (1_u64 << 53) - 1, "ask id is not wire-safe: {id}");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Which color?");
        assert!(ask_user_question_pending(id));

        // Free-text answers pass through verbatim (the rail's free-text
        // field and multi-select joins both arrive as plain map values).
        bus.send(AppEvent::ControlCommand(ControlMsg::AnswerQuestion {
            session_id: Some("sess-ask".into()),
            id,
            answers: std::collections::HashMap::from([(
                "Which color?".to_string(),
                "blue, or cerulean if available".to_string(),
            )]),
            selections: Default::default(),
            followups: Default::default(),
            annotations: Default::default(),
        }));

        let result = ask.await.expect("join").expect("ask_user result");
        assert_eq!(result["status"], "answered", "{result}");
        assert_eq!(result["answer"], "blue, or cerulean if available");
        assert_eq!(
            result["answers"]["Which color?"],
            "blue, or cerulean if available"
        );
        assert_eq!(result["session_id"], "sess-ask");
        assert!(!ask_user_question_pending(id));

        // The waiter reported the resolution so every dashboard clears.
        loop {
            match next_event(&mut rx, "ApprovalResolved").await {
                AppEvent::ApprovalResolved {
                    id: resolved,
                    action,
                    ..
                } if resolved == id => {
                    assert_eq!(action, "answer");
                    break;
                }
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn ask_user_commits_previews_and_broadcasts_references() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        let log_dir = tmp.path().join("logs");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let bus = EventBus::new();
        let mut state = McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            log_dir.clone(),
        );
        state.session_id = "sess-preview".to_string();
        state.project_root = Some(project_root.clone());
        state.interactive_frontends = true;
        let server = IntendantServer::new(Arc::new(RwLock::new(state)), bus.clone());
        let mut rx = bus.subscribe();

        let html = "<!doctype html><html><body>proto A</body></html>";
        let png = [0x89u8, b'P', b'N', b'G'];
        let mut params = ask_params("Which prototype?");
        params.options = vec![
            AskUserOptionParams {
                label: "A".into(),
                description: None,
            },
            AskUserOptionParams {
                label: "B".into(),
                description: None,
            },
        ];
        params.previews = vec![
            html_preview("A", html),
            image_preview("B", Some("image/png"), &png),
            text_preview("Diff", "- old\n+ new"),
        ];
        let ask_server = server.clone();
        let ask = tokio::spawn(async move { ask_server.ask_user_inner(params).await });

        let (id, questions) = match next_event(&mut rx, "UserQuestionRequired").await {
            AppEvent::UserQuestionRequired { id, questions, .. } => (id, questions),
            other => panic!("expected UserQuestionRequired, got {other:?}"),
        };

        // The broadcast carries references + inline text only — the blobs
        // themselves live in the session's upload store, in the same scope
        // the gateway's /raw route resolves for this project root.
        let previews = &questions[0].previews;
        assert_eq!(previews.len(), 3);
        let scope = crate::global_store::StoreScope::resolve(Some(&project_root));
        match &previews[0].source {
            crate::types::QuestionPreviewSource::Html { upload_id, url } => {
                assert_eq!(
                    url,
                    &format!("/api/session/current/uploads/{upload_id}/raw")
                );
                let descriptor = crate::upload_store::find_upload(upload_id, &log_dir, &scope)
                    .expect("html blob committed");
                assert_eq!(descriptor.session_id, "sess-preview");
                assert_eq!(descriptor.mime, "text/html");
                assert_eq!(std::fs::read(&descriptor.path).unwrap(), html.as_bytes());
            }
            other => panic!("expected html preview, got {other:?}"),
        }
        match &previews[1].source {
            crate::types::QuestionPreviewSource::Image {
                upload_id,
                mime,
                url,
            } => {
                assert_eq!(mime, "image/png");
                assert_eq!(
                    url,
                    &format!("/api/session/current/uploads/{upload_id}/raw")
                );
                let descriptor = crate::upload_store::find_upload(upload_id, &log_dir, &scope)
                    .expect("image blob committed");
                assert_eq!(std::fs::read(&descriptor.path).unwrap(), png);
            }
            other => panic!("expected image preview, got {other:?}"),
        }
        assert!(matches!(
            &previews[2].source,
            crate::types::QuestionPreviewSource::Text { content } if content == "- old\n+ new"
        ));
        assert_eq!(previews[0].label, "A");
        assert_eq!(previews[1].label, "B");
        assert_eq!(previews[2].label, "Diff");

        bus.send(AppEvent::ControlCommand(ControlMsg::AnswerQuestion {
            session_id: Some("sess-preview".into()),
            id,
            answers: std::collections::HashMap::from([(
                "Which prototype?".to_string(),
                "A".to_string(),
            )]),
            selections: Default::default(),
            followups: Default::default(),
            annotations: Default::default(),
        }));
        let result = ask.await.expect("join").expect("ask_user result");
        assert_eq!(result["status"], "answered", "{result}");
        assert_eq!(result["answer"], "A");
    }

    #[tokio::test]
    async fn ask_user_times_out_with_best_judgment_guidance() {
        let (server, bus) = test_server("sess-timeout", true);
        let mut rx = bus.subscribe();

        let mut params = ask_params("Anyone there?");
        params.wait_seconds = Some(1);
        let result = server.ask_user_inner(params).await.expect("result");
        assert_eq!(result["status"], "timeout", "{result}");
        let guidance = result["guidance"].as_str().unwrap();
        assert!(guidance.contains("best judgment"), "{guidance}");
        assert_eq!(result["answers"]["Anyone there?"], guidance);

        let id = result["id"].as_u64().unwrap();
        assert!(!ask_user_question_pending(id));
        // Rail cleanup: UserQuestionRequired then ApprovalResolved(timeout).
        loop {
            match next_event(&mut rx, "ApprovalResolved").await {
                AppEvent::ApprovalResolved {
                    id: resolved,
                    action,
                    ..
                } if resolved == id => {
                    assert_eq!(action, "timeout");
                    break;
                }
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn ask_user_auto_answers_immediately_without_frontends() {
        let (server, bus) = test_server("sess-headless", false);
        let mut rx = bus.subscribe();

        let result = server
            .ask_user_inner(ask_params("Which db?"))
            .await
            .expect("result");
        assert_eq!(result["status"], "auto_answered", "{result}");
        assert_eq!(result["answer"].as_str().unwrap(), NO_ANSWER_GUIDANCE);

        // No question was ever armed or announced: the only trace is the
        // warn log entry.
        match next_event(&mut rx, "LogEntry").await {
            AppEvent::LogEntry { level, content, .. } => {
                assert_eq!(level, "warn");
                assert!(content.contains("No user available to answer"), "{content}");
            }
            other => panic!("expected LogEntry, got {other:?}"),
        }
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn ask_user_maps_bare_verbs_to_pass_and_dismissed() {
        for (msg, status, action) in [
            (
                |id| ControlMsg::Approve {
                    session_id: None,
                    id,
                },
                "pass",
                "approve",
            ),
            (
                |id| ControlMsg::Skip {
                    session_id: None,
                    id,
                },
                "dismissed",
                "skip",
            ),
            (
                |id| ControlMsg::Deny {
                    session_id: None,
                    id,
                },
                "dismissed",
                "deny",
            ),
        ] as [(fn(u64) -> ControlMsg, &str, &str); 3]
        {
            let (server, bus) = test_server("sess-verbs", true);
            let mut rx = bus.subscribe();
            let ask_server = server.clone();
            let ask =
                tokio::spawn(async move { ask_server.ask_user_inner(ask_params("Go?")).await });
            let id = match next_event(&mut rx, "UserQuestionRequired").await {
                AppEvent::UserQuestionRequired { id, .. } => id,
                other => panic!("expected UserQuestionRequired, got {other:?}"),
            };
            bus.send(AppEvent::ControlCommand(msg(id)));
            let result = ask.await.expect("join").expect("result");
            assert_eq!(result["status"], status, "{result}");
            assert!(result["guidance"].as_str().unwrap().contains("judgment"));
            loop {
                match next_event(&mut rx, "ApprovalResolved").await {
                    AppEvent::ApprovalResolved {
                        id: resolved,
                        action: resolved_action,
                        ..
                    } if resolved == id => {
                        assert_eq!(resolved_action, action);
                        break;
                    }
                    _ => continue,
                }
            }
        }
    }

    #[tokio::test]
    async fn ask_user_ignores_other_ids_while_waiting() {
        let (server, bus) = test_server("sess-other", true);
        let mut rx = bus.subscribe();
        let ask_server = server.clone();
        let ask = tokio::spawn(async move { ask_server.ask_user_inner(ask_params("Mine?")).await });
        let id = match next_event(&mut rx, "UserQuestionRequired").await {
            AppEvent::UserQuestionRequired { id, .. } => id,
            other => panic!("expected UserQuestionRequired, got {other:?}"),
        };
        // Answers and verbs for other ids (a session's own approvals) must
        // not resolve this ask.
        bus.send(AppEvent::ControlCommand(ControlMsg::AnswerQuestion {
            session_id: None,
            id: 1,
            answers: std::collections::HashMap::from([("Mine?".into(), "wrong".into())]),
            selections: Default::default(),
            followups: Default::default(),
            annotations: Default::default(),
        }));
        bus.send(AppEvent::ControlCommand(ControlMsg::Approve {
            session_id: None,
            id: 2,
        }));
        assert!(ask_user_question_pending(id));
        bus.send(AppEvent::ControlCommand(ControlMsg::AnswerQuestion {
            session_id: Some("sess-other".into()),
            id,
            answers: std::collections::HashMap::from([("Mine?".into(), "yes".into())]),
            selections: Default::default(),
            followups: Default::default(),
            annotations: Default::default(),
        }));
        let result = ask.await.expect("join").expect("result");
        assert_eq!(result["status"], "answered");
        assert_eq!(result["answer"], "yes");
    }

    #[tokio::test]
    async fn ask_user_requires_a_session() {
        let (server, _bus) = test_server("", true);
        let err = server
            .ask_user_inner(ask_params("Anyone?"))
            .await
            .unwrap_err();
        assert!(err.contains("no session"), "{err}");
    }

    #[tokio::test]
    async fn notify_user_emits_the_event_and_validates() {
        let (server, bus) = test_server("sess-notify", true);
        let mut rx = bus.subscribe();

        let result = server
            .notify_user_inner(NotifyUserParams {
                text: " Build finished ".into(),
                title: Some(" CI ".into()),
                urgency: Some("attention".into()),
                session_id: None,
            })
            .await
            .expect("result");
        assert_eq!(result["status"], "sent");
        assert_eq!(result["session_id"], "sess-notify");
        assert_eq!(result["urgency"], "attention");
        let id = result["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("notif-"), "{id}");

        match next_event(&mut rx, "UserNotification").await {
            AppEvent::UserNotification {
                session_id,
                id: event_id,
                title,
                text,
                urgency,
                ts,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-notify"));
                assert_eq!(event_id, id);
                assert_eq!(title.as_deref(), Some("CI"));
                assert_eq!(text, "Build finished");
                assert_eq!(urgency, NotificationUrgency::Attention);
                assert!(ts > 0);
            }
            other => panic!("expected UserNotification, got {other:?}"),
        }

        // Default urgency is info; unknown urgency and empty text refuse.
        let result = server
            .notify_user_inner(NotifyUserParams {
                text: "plain".into(),
                title: None,
                urgency: None,
                session_id: Some("s2".into()),
            })
            .await
            .expect("result");
        assert_eq!(result["urgency"], "info");

        let err = server
            .notify_user_inner(NotifyUserParams {
                text: "x".into(),
                title: None,
                urgency: Some("loud".into()),
                session_id: Some("s2".into()),
            })
            .await
            .unwrap_err();
        assert!(err.contains("unknown urgency"), "{err}");

        let err = server
            .notify_user_inner(NotifyUserParams {
                text: "  ".into(),
                title: None,
                urgency: None,
                session_id: Some("s2".into()),
            })
            .await
            .unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");

        let err = server
            .notify_user_inner(NotifyUserParams {
                text: "x".repeat(NOTIFY_USER_MAX_TEXT_BYTES + 1),
                title: None,
                urgency: None,
                session_id: Some("s2".into()),
            })
            .await
            .unwrap_err();
        assert!(err.contains("max"), "{err}");
    }

    /// Both tools must be callable through the generic HTTP dispatch path
    /// (the `/mcp` transport ctl and supervised agents use), with the
    /// URL-bound session id injected when the args omit one, and must
    /// surface validation failures as `isError` tool results.
    #[tokio::test]
    async fn tools_dispatch_by_name_with_url_session() {
        let (server, bus) = test_server("", true);
        let mut rx = bus.subscribe();

        let result = server
            .call_tool_by_name_for_session(
                "notify_user",
                serde_json::json!({ "text": "from dispatch" }),
                Some("url-sess"),
                None,
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        match next_event(&mut rx, "UserNotification").await {
            AppEvent::UserNotification { session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some("url-sess"));
            }
            other => panic!("expected UserNotification, got {other:?}"),
        }
        let result = server
            .call_tool_by_name_for_session(
                "notify_user",
                serde_json::json!({ "text": "" }),
                Some("url-sess"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));

        // ask_user over dispatch: answer it from the bus like a frontend.
        let ask_server = server.clone();
        let ask = tokio::spawn(async move {
            ask_server
                .call_tool_by_name_for_session(
                    "ask_user",
                    serde_json::json!({ "question": "Dispatch ok?" }),
                    Some("url-sess"),
                    None,
                )
                .await
        });
        let id = loop {
            match next_event(&mut rx, "UserQuestionRequired").await {
                AppEvent::UserQuestionRequired { session_id, id, .. } => {
                    assert_eq!(session_id.as_deref(), Some("url-sess"));
                    break id;
                }
                _ => continue,
            }
        };
        bus.send(AppEvent::ControlCommand(ControlMsg::AnswerQuestion {
            session_id: Some("url-sess".into()),
            id,
            answers: std::collections::HashMap::from([("Dispatch ok?".into(), "yes".into())]),
            selections: Default::default(),
            followups: Default::default(),
            annotations: Default::default(),
        }));
        let result = ask.await.expect("join").expect("dispatch result");
        assert_ne!(result.is_error, Some(true));
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default();
        assert!(text.contains("\"answered\""), "{text}");

        let result = server
            .call_tool_by_name_for_session(
                "ask_user",
                serde_json::json!({ "question": "" }),
                Some("url-sess"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    // ---- agenda-backed asks (ask↔agenda unification, slice 2) ----

    fn test_server_with_agenda(
        session_id: &str,
    ) -> (
        IntendantServer,
        EventBus,
        Arc<crate::agenda::AgendaHandle>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let agenda = Arc::new(crate::agenda::AgendaHandle::new(
            crate::agenda::AgendaStore::open(dir.path()).unwrap(),
            bus.clone(),
            dir.path(),
        ));
        let mut state = McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        state.session_id = session_id.to_string();
        state.interactive_frontends = true;
        state.agenda = Some(agenda.clone());
        let server = IntendantServer::new(Arc::new(RwLock::new(state)), bus.clone());
        (server, bus, agenda, dir)
    }

    /// Skip to the next rail announcement (the agenda lanes interleave
    /// AgendaChanged and notification events on the same bus).
    async fn next_user_question(
        rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    ) -> (
        Option<String>,
        u64,
        Vec<crate::types::UserQuestion>,
        Option<u64>,
        bool,
    ) {
        loop {
            if let AppEvent::UserQuestionRequired {
                session_id,
                id,
                questions,
                expires_at_ms,
                held,
            } = next_event(rx, "UserQuestionRequired").await
            {
                return (session_id, id, questions, expires_at_ms, held);
            }
        }
    }

    /// Blocking-as-sugar end to end: the blocking ask creates the agenda
    /// item (park semantics), announces WITH a deadline, and the rail
    /// answer resolves waiter and item exactly once — the resolver writes,
    /// the waiter observes the recorded outcome.
    #[tokio::test]
    async fn agenda_blocking_ask_answers_via_the_item() {
        let (server, bus, agenda, _dir) = test_server_with_agenda("sess-agenda");
        let _resolver = crate::agenda::spawn_ask_resolver(agenda.clone());
        let mut rx = bus.subscribe();

        let ask_server = server.clone();
        let mut params = ask_params("Which color?");
        params.options = vec![AskUserOptionParams {
            label: "blue".into(),
            description: None,
        }];
        let ask = tokio::spawn(async move { ask_server.ask_user_inner(params).await });

        let (session, id, questions, expires_at_ms, held) = next_user_question(&mut rx).await;
        assert_eq!(session.as_deref(), Some("sess-agenda"));
        assert!(expires_at_ms.is_some(), "blocking asks arm a deadline");
        assert!(!held);
        assert_eq!(questions[0].question, "Which color?");
        // The item exists, open, attributed to the asking session; both
        // registries know the id.
        assert!(ask_user_question_pending(id));
        assert!(crate::agenda::agenda_ask_pending(id));
        let item = agenda.open_ask_item(id).expect("open item backs the ask");
        assert_eq!(item.provenance.session_id.as_deref(), Some("sess-agenda"));

        bus.send(AppEvent::ControlCommand(ControlMsg::AnswerQuestion {
            session_id: Some("sess-agenda".into()),
            id,
            answers: std::collections::HashMap::from([(
                "Which color?".to_string(),
                "blue".to_string(),
            )]),
            selections: std::collections::HashMap::from([(
                "Which color?".to_string(),
                vec!["blue".to_string()],
            )]),
            followups: Default::default(),
            annotations: Default::default(),
        }));

        let result = ask.await.expect("join").expect("ask_user result");
        assert_eq!(result["status"], "answered", "{result}");
        assert_eq!(result["answer"], "blue");
        assert_eq!(result["item_id"], item.id, "{result}");
        assert_eq!(result["questions"][0]["selected"][0], "blue");
        assert!(!ask_user_question_pending(id), "waiter deregistered");
        assert!(
            !crate::agenda::agenda_ask_pending(id),
            "answered item left the open-ask registry"
        );

        // The item completed exactly once, with both answer forms.
        let done = agenda.item_by_id(&item.id).unwrap();
        assert_eq!(done.status, crate::agenda::AgendaStatus::Done);
        let recorded = done.answer.as_ref().expect("answer recorded");
        assert_eq!(recorded.text, "blue");
        assert!(recorded
            .structured
            .as_ref()
            .is_some_and(|s| s.answers["Which color?"] == "blue"));

        // The single writer cleared the rails (waiter emits nothing).
        // Bounded by a sentinel: everything before it is already queued.
        bus.send(AppEvent::ControlCommand(ControlMsg::Skip {
            session_id: None,
            id: 0,
        }));
        let mut resolved = 0;
        loop {
            match next_event(&mut rx, "rail clear before the sentinel").await {
                AppEvent::ApprovalResolved {
                    id: rid, action, ..
                } if rid == id => {
                    assert_eq!(action, "answer");
                    resolved += 1;
                }
                AppEvent::ControlCommand(ControlMsg::Skip { id: 0, .. }) => break,
                _ => continue,
            }
        }
        assert_eq!(resolved, 1, "exactly one rail clear for the answer");
    }

    /// Timeout no longer evaporates the question: the waiter returns
    /// "timeout" naming the agenda item, the item stays OPEN, and the
    /// rail card converts to its parked (non-expiring) form. A later
    /// answer is recorded by the resolver with no inline waiter — the
    /// supervisor's delivery signal.
    #[tokio::test(start_paused = true)]
    async fn agenda_blocking_timeout_keeps_item_open_then_late_answer_records() {
        let (server, bus, agenda, _dir) = test_server_with_agenda("sess-late");
        let _resolver = crate::agenda::spawn_ask_resolver(agenda.clone());
        let mut rx = bus.subscribe();

        let ask_server = server.clone();
        let mut params = ask_params("Grid A or B?");
        params.wait_seconds = Some(5);
        let ask = tokio::spawn(async move { ask_server.ask_user_inner(params).await });

        let result = ask.await.expect("join").expect("ask_user result");
        assert_eq!(result["status"], "timeout", "{result}");
        let item_id = result["item_id"].as_str().expect("item id on timeout");
        let guidance = result["guidance"].as_str().unwrap_or_default();
        assert!(
            guidance.contains(item_id),
            "guidance names the item: {guidance}"
        );
        assert!(guidance.contains("stays OPEN on the agenda"), "{guidance}");
        assert!(guidance.contains("ctl agenda patch"), "{guidance}");

        // Item open; waiter gone; open-ask registry still holds the id.
        let item = agenda.item_by_id(item_id).unwrap();
        assert_eq!(item.status, crate::agenda::AgendaStatus::Open);
        let ask_id = item.ask.as_ref().unwrap().ask_id;
        assert!(!ask_user_question_pending(ask_id));
        assert!(crate::agenda::agenda_ask_pending(ask_id));

        // The rail card converted to parked form (same id, no expiry).
        let mut saw_deadline = false;
        let mut saw_parked = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::UserQuestionRequired {
                id, expires_at_ms, ..
            } = event
            {
                if id == ask_id {
                    match expires_at_ms {
                        Some(_) => saw_deadline = true,
                        None => {
                            assert!(saw_deadline, "deadline announcement precedes the park");
                            saw_parked = true;
                        }
                    }
                }
            }
        }
        assert!(saw_parked, "timeout re-announces the parked card");

        // Post-timeout answer: the resolver records it; the outcome event
        // carries inline_waiter=false, so the supervisor delivers it.
        bus.send(AppEvent::ControlCommand(ControlMsg::AnswerQuestion {
            session_id: None,
            id: ask_id,
            answers: std::collections::HashMap::from([(
                "Grid A or B?".to_string(),
                "B".to_string(),
            )]),
            selections: Default::default(),
            followups: Default::default(),
            annotations: Default::default(),
        }));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(AppEvent::AgendaAskOutcome {
                    item,
                    action,
                    inline_waiter,
                })) if item.id == item_id => {
                    assert_eq!(action, "answer");
                    assert!(!inline_waiter, "no waiter holds a timed-out ask");
                    break;
                }
                Ok(Ok(_)) => continue,
                other => panic!("no outcome for the late answer: {other:?}"),
            }
        }
        let done = agenda.item_by_id(item_id).unwrap();
        assert_eq!(done.status, crate::agenda::AgendaStatus::Done);
        assert_eq!(done.answer.as_ref().unwrap().text, "B");
    }

    /// An Agenda-tab answer (the `answer` op, no rail ControlMsg at all)
    /// resolves a live blocking waiter — the waiter observes the ITEM.
    #[tokio::test]
    async fn agenda_tab_answer_resolves_blocking_waiter() {
        let (server, bus, agenda, _dir) = test_server_with_agenda("sess-tab");
        let mut rx = bus.subscribe();

        let ask_server = server.clone();
        let ask =
            tokio::spawn(async move { ask_server.ask_user_inner(ask_params("Tab ok?")).await });
        let (_, id, _, _, _) = next_user_question(&mut rx).await;
        let item = agenda.open_ask_item(id).unwrap();

        agenda
            .apply(
                crate::agenda::AgendaCommand::Answer {
                    id: item.id.clone(),
                    text: "yes — from the tab".into(),
                    structured: None,
                },
                None,
            )
            .unwrap();

        let result = ask.await.expect("join").expect("ask_user result");
        assert_eq!(result["status"], "answered", "{result}");
        assert_eq!(result["answer"], "yes — from the tab");
        assert_eq!(result["item_id"], item.id);
        assert!(!ask_user_question_pending(id));
    }

    /// Rail dismissal while blocked: "dismissed" to the agent, the item
    /// stays OPEN with the dismissal marker (slice 1 semantics).
    #[tokio::test]
    async fn agenda_blocking_dismissal_keeps_item_open() {
        let (server, bus, agenda, _dir) = test_server_with_agenda("sess-skip");
        let _resolver = crate::agenda::spawn_ask_resolver(agenda.clone());
        let mut rx = bus.subscribe();

        let ask_server = server.clone();
        let ask =
            tokio::spawn(async move { ask_server.ask_user_inner(ask_params("Skippable?")).await });
        let (_, id, _, _, _) = next_user_question(&mut rx).await;
        let item = agenda.open_ask_item(id).unwrap();

        bus.send(AppEvent::ControlCommand(ControlMsg::Skip {
            session_id: None,
            id,
        }));
        let result = ask.await.expect("join").expect("ask_user result");
        assert_eq!(result["status"], "dismissed", "{result}");
        assert_eq!(result["item_id"], item.id);

        let after = agenda.item_by_id(&item.id).unwrap();
        assert_eq!(after.status, crate::agenda::AgendaStatus::Open);
        assert_eq!(after.dismissed.as_ref().unwrap().action, "skip");
        assert!(!ask_user_question_pending(id));
        assert!(crate::agenda::agenda_ask_pending(id), "still answerable");
    }

    /// MCP park parity: park=true files the item and returns immediately
    /// with the ctl-shaped {status:"parked", item_id, ask_id}; the parked
    /// rail announcement (no deadline) goes out; no waiter registers.
    #[tokio::test]
    async fn ask_user_park_files_the_item_and_returns() {
        let (server, bus, agenda, _dir) = test_server_with_agenda("sess-park");
        let mut rx = bus.subscribe();
        let mut params = ask_params("Park me?");
        params.park = true;
        let result = server.ask_user_inner(params).await.expect("park result");
        assert_eq!(result["status"], "parked", "{result}");
        let item_id = result["item_id"].as_str().expect("item id");
        let ask_id = result["ask_id"].as_u64().expect("ask id");

        let item = agenda.item_by_id(item_id).unwrap();
        assert_eq!(item.status, crate::agenda::AgendaStatus::Open);
        assert_eq!(item.provenance.session_id.as_deref(), Some("sess-park"));
        assert_eq!(item.ask.as_ref().unwrap().ask_id, ask_id);
        assert!(!ask_user_question_pending(ask_id), "park never waits");
        assert!(crate::agenda::agenda_ask_pending(ask_id));

        let (_, id, _, expires_at_ms, held) = next_user_question(&mut rx).await;
        assert_eq!(id, ask_id);
        assert_eq!(expires_at_ms, None, "parked asks never expire");
        assert!(!held);
    }

    /// park + wait_seconds is a contradiction and refuses (ctl parity);
    /// park without an agenda store names the limitation.
    #[tokio::test]
    async fn ask_user_park_validates_shape_and_availability() {
        let (server, _bus, _agenda, _dir) = test_server_with_agenda("sess-parkval");
        let mut params = ask_params("Park me?");
        params.park = true;
        params.wait_seconds = Some(60);
        let err = server.ask_user_inner(params).await.unwrap_err();
        assert!(err.contains("park doesn't wait"), "{err}");

        // No agenda store (stdio shape): park is refused honestly.
        let (server, _bus) = test_server("sess-noagenda", true);
        let mut params = ask_params("Park me?");
        params.park = true;
        let err = server.ask_user_inner(params).await.unwrap_err();
        assert!(err.contains("agenda unavailable"), "{err}");
    }

    /// The flat form's multi_select sugar becomes explicit pick bounds in
    /// the park vocabulary (ctl's ask_park_command mapping, server-side).
    #[test]
    fn park_questions_maps_flat_multi_select_sugar() {
        let mut params = ask_params("Pick some");
        params.options = labeled_options(&["A", "B", "C"]);
        params.multi_select = Some(true);
        let questions = park_questions(&params).unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].pick_min, Some(1));
        assert_eq!(questions[0].pick_max, Some(3));

        // Explicit bounds win over the sugar.
        params.pick_min = Some(0);
        params.pick_max = Some(2);
        let questions = park_questions(&params).unwrap();
        assert_eq!(questions[0].pick_min, Some(0));
        assert_eq!(questions[0].pick_max, Some(2));

        // The multi form passes through verbatim; both forms refuse.
        let mut params = ask_params("");
        params.questions = vec![question_param("q1"), question_param("q2")];
        assert_eq!(park_questions(&params).unwrap().len(), 2);
        let mut params = ask_params("flat");
        params.questions = vec![question_param("also")];
        assert!(park_questions(&params).unwrap_err().contains("not both"));
    }
}
