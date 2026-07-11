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
//! its id. Ask ids are allocated from [`ASK_USER_ID_BASE`], a range
//! disjoint from the per-session loop counters (which count turns/approvals
//! from 1), so an MCP-armed ask can never collide with a session's own
//! pending approvals in the shared id space.
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
/// Maximum option-label size (the label is also the returned answer value).
pub(crate) const ASK_USER_MAX_OPTION_LABEL_BYTES: usize = 120;
/// Default blocking wait for an answer.
pub(crate) const ASK_USER_DEFAULT_WAIT_SECS: u64 = 300;
/// Hard cap on the blocking wait.
pub(crate) const ASK_USER_MAX_WAIT_SECS: u64 = 900;
/// Maximum notification text size. Notifications are alerts, not documents.
pub(crate) const NOTIFY_USER_MAX_TEXT_BYTES: usize = 4096;

/// Base of the MCP ask id range. Per-session agent loops allocate approval
/// and question ids from small local counters (turn numbers, counters from
/// 1); MCP-armed asks share the same `u64` id space on the wire, so they
/// draw from a disjoint high range instead. Still well below JS's
/// `Number.MAX_SAFE_INTEGER` (2^53) — dashboards handle the id as a number.
pub(crate) const ASK_USER_ID_BASE: u64 = 1 << 40;

static ASK_ID_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(ASK_USER_ID_BASE);

fn next_ask_id() -> u64 {
    ASK_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

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
/// it does not know: the ask's own waiter resolves it and emits
/// `ApprovalResolved`.
pub(crate) fn ask_user_question_pending(id: u64) -> bool {
    pending_asks()
        .lock()
        .map(|set| set.contains(&id))
        .unwrap_or(false)
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

/// Validate `ask_user` parameters into the `UserQuestion` the rail renders.
/// Returns the question plus the effective wait.
pub(crate) fn build_ask_user_question(
    params: &AskUserParams,
) -> Result<(crate::types::UserQuestion, u64), String> {
    let question = params.question.trim();
    if question.is_empty() {
        return Err("question must not be empty".to_string());
    }
    if question.len() > ASK_USER_MAX_QUESTION_BYTES {
        return Err(format!(
            "question is {} bytes; max {} KB",
            question.len(),
            ASK_USER_MAX_QUESTION_BYTES / 1024
        ));
    }
    if params.options.len() > ASK_USER_MAX_OPTIONS {
        return Err(format!(
            "too many options: {} (max {ASK_USER_MAX_OPTIONS}; zero options means free-text only)",
            params.options.len()
        ));
    }
    let mut options = Vec::with_capacity(params.options.len());
    for (index, option) in params.options.iter().enumerate() {
        let label = option.label.trim();
        if label.is_empty() {
            return Err(format!("options[{index}]: label must not be empty"));
        }
        if label.len() > ASK_USER_MAX_OPTION_LABEL_BYTES {
            return Err(format!(
                "options[{index}]: label is {} bytes; max {ASK_USER_MAX_OPTION_LABEL_BYTES} \
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
    let header = params
        .header
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| crate::types::truncate_str(h, 64).to_string())
        .unwrap_or_default();
    let wait_seconds = params
        .wait_seconds
        .unwrap_or(ASK_USER_DEFAULT_WAIT_SECS)
        .clamp(1, ASK_USER_MAX_WAIT_SECS);
    Ok((
        crate::types::UserQuestion {
            question: question.to_string(),
            header,
            options,
            multi_select: params.multi_select.unwrap_or(false),
        },
        wait_seconds,
    ))
}

/// One `ask_user` outcome, shaped for both the returning tool result and
/// the guidance-filled answers map agents read.
fn ask_result(
    status: &str,
    id: u64,
    session_id: &str,
    question: &str,
    answers: std::collections::HashMap<String, String>,
    guidance: Option<&str>,
) -> serde_json::Value {
    let answer = answers
        .get(question)
        .cloned()
        .or_else(|| answers.values().next().cloned())
        .unwrap_or_default();
    let mut result = serde_json::json!({
        "status": status,
        "id": id,
        "session_id": session_id,
        "question": question,
        "answer": answer,
        "answers": answers,
    });
    if let Some(guidance) = guidance {
        result["guidance"] = serde_json::Value::String(guidance.to_string());
    }
    result
}

fn guidance_answers(question: &str, guidance: &str) -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([(question.to_string(), guidance.to_string())])
}

impl IntendantServer {
    #[tool(
        description = "Ask the user one structured question on the dashboard question rail and BLOCK until they answer (or the wait times out). A question requests input, never permission: it is never auto-approved and answering it never widens autonomy. Provide 0-4 options ({label, description?}); with zero options the user types a free-text answer (free text is always allowed on top of options). Returns {status, answer, answers}: status \"answered\" carries the user's choice(s); \"timeout\"/\"dismissed\"/\"pass\" carry best-judgment guidance instead — proceed on your own judgment then. Default wait 300s, max 900. Use it before destructive or hard-to-reverse choices; prefer notify_user when you only need to inform."
    )]
    pub(crate) async fn ask_user(&self, Parameters(params): Parameters<AskUserParams>) -> String {
        match self.ask_user_inner(params).await {
            Ok(value) => value.to_string(),
            Err(message) => format!("ask_user failed: {message}"),
        }
    }

    /// Core of `ask_user`, shared by the stdio `#[tool]` method and the
    /// HTTP dispatch arm (which maps `Err` to an `isError` tool result).
    pub(crate) async fn ask_user_inner(
        &self,
        params: AskUserParams,
    ) -> Result<serde_json::Value, String> {
        let (question, wait_seconds) = build_ask_user_question(&params)?;
        let question_text = question.question.clone();

        // Same session resolution as post_session_note: the HTTP dispatch
        // injects the URL-bound session id (`with_default_mcp_session_id`),
        // an explicit argument wins, then the single-session state fallback.
        let (session_id, interactive_frontends) = {
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
            (session_id, state.interactive_frontends)
        };
        let Some(session_id) = session_id else {
            return Err(
                "no session to ask from; pass session_id (or call through the session-scoped \
                 MCP URL Intendant injected)"
                    .to_string(),
            );
        };

        let id = next_ask_id();

        // No frontend can ever answer in this shape (headless without a web
        // gateway): tell the model to proceed instead of blocking on nobody
        // — the same semantic supervised Claude Code gets headless.
        if !interactive_frontends {
            let content = format!("No user available to answer: {question_text}");
            self.bus.send(AppEvent::LogEntry {
                session_id: Some(session_id.clone()),
                level: "warn".to_string(),
                source: "system".to_string(),
                content,
                turn: None,
            });
            return Ok(ask_result(
                "auto_answered",
                id,
                &session_id,
                &question_text,
                guidance_answers(&question_text, NO_ANSWER_GUIDANCE),
                Some(NO_ANSWER_GUIDANCE),
            ));
        }

        // Subscribe BEFORE announcing the question (same race as approvals:
        // an instant answer must find the waiter listening).
        let mut events = self.bus.subscribe();
        let mut guard = PendingAskGuard::register(id, Some(session_id.clone()), self.bus.clone());
        self.bus.send(AppEvent::UserQuestionRequired {
            session_id: Some(session_id.clone()),
            id,
            questions: vec![question],
        });

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_seconds);
        loop {
            let event = match tokio::time::timeout_at(deadline, events.recv()).await {
                Err(_) => {
                    // Timed out: clear the rail and hand back guidance.
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
                        &question_text,
                        guidance_answers(&question_text, &guidance),
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
                        &question_text,
                        guidance_answers(&question_text, DISMISSED_GUIDANCE),
                        Some(DISMISSED_GUIDANCE),
                    ));
                }
                Ok(Ok(event)) => event,
            };
            // Every frontend intent rides the bus as a ControlCommand
            // (web /ws, dashboard tunnel, control socket, MCP twins), so
            // matching here resolves uniformly across daemon shapes. Ids
            // from the dedicated ask range are globally unique — match on
            // id alone.
            let AppEvent::ControlCommand(msg) = event else {
                continue;
            };
            match msg {
                ControlMsg::AnswerQuestion {
                    id: answer_id,
                    answers,
                    ..
                } if answer_id == id => {
                    guard.resolve("answer");
                    return Ok(ask_result(
                        "answered",
                        id,
                        &session_id,
                        &question_text,
                        answers,
                        None,
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
                        &question_text,
                        guidance_answers(&question_text, PASS_GUIDANCE),
                        Some(PASS_GUIDANCE),
                    ));
                }
                ControlMsg::Deny { id: verb_id, .. } if verb_id == id => {
                    guard.resolve("deny");
                    return Ok(ask_result(
                        "dismissed",
                        id,
                        &session_id,
                        &question_text,
                        guidance_answers(&question_text, DISMISSED_GUIDANCE),
                        Some(DISMISSED_GUIDANCE),
                    ));
                }
                ControlMsg::Skip { id: verb_id, .. } if verb_id == id => {
                    guard.resolve("skip");
                    return Ok(ask_result(
                        "dismissed",
                        id,
                        &session_id,
                        &question_text,
                        guidance_answers(&question_text, DISMISSED_GUIDANCE),
                        Some(DISMISSED_GUIDANCE),
                    ));
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
            multi_select: None,
            wait_seconds: None,
            session_id: None,
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
    fn ask_ids_come_from_the_disjoint_range() {
        // Loop-local approval counters count from 1; the shared id space
        // only stays collision-free if MCP asks never dip below the base.
        assert!(next_ask_id() >= ASK_USER_ID_BASE);
    }

    #[test]
    fn build_ask_user_question_validates_and_normalizes() {
        // Empty question / too many options / empty label all refuse.
        let err = build_ask_user_question(&ask_params("  ")).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");

        let mut params = ask_params("Pick one");
        params.options = (0..ASK_USER_MAX_OPTIONS + 1)
            .map(|i| AskUserOptionParams {
                label: format!("option {i}"),
                description: None,
            })
            .collect();
        let err = build_ask_user_question(&params).unwrap_err();
        assert!(err.contains("too many options"), "{err}");

        let mut params = ask_params("Pick one");
        params.options = vec![AskUserOptionParams {
            label: "  ".into(),
            description: None,
        }];
        let err = build_ask_user_question(&params).unwrap_err();
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
        let (question, wait) = build_ask_user_question(&params).unwrap();
        assert_eq!(question.question, "Deploy now?");
        assert_eq!(question.header, "Release");
        assert_eq!(question.options.len(), 2);
        assert_eq!(question.options[0].label, "Yes");
        assert_eq!(question.options[0].description, "Ship it");
        assert!(question.multi_select);
        assert_eq!(wait, ASK_USER_MAX_WAIT_SECS);

        let mut params = ask_params("Quick?");
        params.wait_seconds = Some(0);
        let (_, wait) = build_ask_user_question(&params).unwrap();
        assert_eq!(wait, 1);
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

        // The rail event announces the question with the armed id.
        let (id, questions) = match next_event(&mut rx, "UserQuestionRequired").await {
            AppEvent::UserQuestionRequired {
                session_id,
                id,
                questions,
            } => {
                assert_eq!(session_id.as_deref(), Some("sess-ask"));
                (id, questions)
            }
            other => panic!("expected UserQuestionRequired, got {other:?}"),
        };
        assert!(id >= ASK_USER_ID_BASE, "ask id {id} below the MCP range");
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
}
