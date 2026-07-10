//! Pending-request attention nudges: when an agent→user request (a command
//! approval or a structured user question) sits unanswered and **nobody is
//! watching a dashboard**, nudge the owner through the Connect rendezvous so
//! their opted-in browsers get a Web Push — the closed-tab leg of the
//! attention chain (open-but-hidden tabs are handled browser-side with the
//! title badge + Notification API; see `static/app/57-attention-notifications.js`).
//!
//! PRIVACY: the nudge carries only a request *kind* and a session *display
//! label* — never command text, question text, file paths, or any other work
//! content. The rendezvous adds the daemon's display label from its own
//! record and stays zero-knowledge about the work itself
//! (docs/src/self-hosted-rendezvous.md).
//!
//! The trigger is deliberately conservative and spam-proof:
//! - a request must have been pending for [`NUDGE_GRACE_MS`] before it can
//!   nudge (fast approvals never leave the machine);
//! - no dashboard may be connected now, and none may have connected since
//!   the request appeared (someone who saw it arrive is already informed);
//! - one nudge per session per [`NUDGE_SESSION_COOLDOWN_MS`] (bursts
//!   collapse);
//! - if the daemon is unclaimed or Connect is unreachable, degrade silently
//!   (debug log only).

use crate::event::{AppEvent, EventBus};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// How long a request must sit pending before it can trigger a nudge.
/// Approvals answered within this window (the common case with a dashboard
/// open) never generate one.
pub(crate) const NUDGE_GRACE_MS: u64 = 45_000;

/// Minimum spacing between nudges for the same session, so a burst of
/// approvals (or a retry loop) collapses into one push.
pub(crate) const NUDGE_SESSION_COOLDOWN_MS: u64 = 10 * 60 * 1000;

/// How often the monitor re-evaluates pending requests.
const MONITOR_TICK_MS: u64 = 5_000;

/// Live local-dashboard `/ws` connections (the "somebody is watching"
/// signal). Maintained by the web gateway's WS session lifecycle.
static DASHBOARD_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Last unix-ms at which at least one dashboard connection was observed
/// (0 = never in this process). Refreshed on connect/disconnect edges and
/// on every monitor tick while a connection is open.
static LAST_DASHBOARD_SEEN_UNIX_MS: AtomicU64 = AtomicU64::new(0);

/// The monitor is spawned once per process (the gateway can rebind its
/// listener; the monitor must not duplicate).
static MONITOR_SPAWNED: AtomicBool = AtomicBool::new(false);

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A local dashboard WebSocket attached. Called by the gateway when a `/ws`
/// session starts.
pub(crate) fn dashboard_connected() {
    DASHBOARD_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
    LAST_DASHBOARD_SEEN_UNIX_MS.store(now_unix_ms(), Ordering::SeqCst);
}

/// A local dashboard WebSocket detached (tab closed, network drop).
pub(crate) fn dashboard_disconnected() {
    // Saturating: a spurious extra decrement must not wrap to "billions
    // of dashboards connected" and permanently suppress nudges.
    let _ = DASHBOARD_CONNECTIONS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        Some(n.saturating_sub(1))
    });
    LAST_DASHBOARD_SEEN_UNIX_MS.store(now_unix_ms(), Ordering::SeqCst);
}

fn dashboards_connected() -> bool {
    DASHBOARD_CONNECTIONS.load(Ordering::SeqCst) > 0
}

fn last_dashboard_seen() -> Option<u64> {
    match LAST_DASHBOARD_SEEN_UNIX_MS.load(Ordering::SeqCst) {
        0 => None,
        ms => Some(ms),
    }
}

/// The kind of agent→user request pending. This is the whole vocabulary the
/// nudge wire carries about the request — deliberately a category, not a
/// payload. New attention kinds (display requests) extend this enum + the
/// service-side whitelist (`NOTIFY_KINDS` in `src/bin/connect/push.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionKind {
    Approval,
    Question,
    /// An urgent `notify_user` notification — an explicit agent escalation,
    /// not a pending request: it skips the grace period (nothing to
    /// "answer quickly"), fires at most once, and still respects the
    /// per-session cooldown.
    Notify,
}

impl AttentionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AttentionKind::Approval => "approval",
            AttentionKind::Question => "question",
            AttentionKind::Notify => "notify",
        }
    }
}

#[derive(Debug, Clone)]
struct PendingRequest {
    kind: AttentionKind,
    since_unix_ms: u64,
}

/// Everything the nudge decision depends on, gathered so the rule is a pure
/// function ([`should_nudge`]) with unit tests.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NudgeInput {
    pub(crate) now_unix_ms: u64,
    /// When the oldest still-pending request of this session appeared.
    pub(crate) pending_since_unix_ms: u64,
    /// Is any dashboard connected right now?
    pub(crate) dashboards_connected: bool,
    /// Last time any dashboard was observed connected (None = never).
    pub(crate) last_dashboard_seen_unix_ms: Option<u64>,
    /// Last nudge sent for this session (None = never).
    pub(crate) last_nudge_unix_ms: Option<u64>,
}

/// The conservative nudge rule. True only when the request has aged past
/// the grace period with no dashboard connected at any point since it
/// appeared, and the session's cooldown has elapsed.
pub(crate) fn should_nudge(input: NudgeInput) -> bool {
    should_nudge_with_grace(input, NUDGE_GRACE_MS)
}

/// The urgent-escalation rule (`notify_user` with `urgency: urgent`): the
/// agent explicitly asked to reach the owner, so there is no grace period —
/// but the dashboard suppressions and the per-session cooldown still hold.
/// The attention chain stays layered: an open tab renders the toast, a
/// hidden tab raises the browser notification, and only the nobody-watching
/// case leaves the machine.
pub(crate) fn should_nudge_escalation(input: NudgeInput) -> bool {
    should_nudge_with_grace(input, 0)
}

fn should_nudge_with_grace(input: NudgeInput, grace_ms: u64) -> bool {
    let age = input.now_unix_ms.saturating_sub(input.pending_since_unix_ms);
    if age < grace_ms {
        return false;
    }
    if input.dashboards_connected {
        return false;
    }
    // A dashboard that connected after the request appeared has seen it
    // (the session log replays pending approvals on connect) — its owner
    // is informed; don't also push.
    if let Some(seen) = input.last_dashboard_seen_unix_ms {
        if seen >= input.pending_since_unix_ms {
            return false;
        }
    }
    if let Some(last) = input.last_nudge_unix_ms {
        if input.now_unix_ms.saturating_sub(last) < NUDGE_SESSION_COOLDOWN_MS {
            return false;
        }
    }
    true
}

/// Session key for requests that carry no session id (single-session /
/// foreground shapes).
fn session_key(session_id: &Option<String>) -> String {
    session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("main")
        .to_string()
}

/// Content-free fallback label: a short session-id prefix. Replaced by the
/// user's explicit rename when one exists — the only case where a
/// user-chosen string rides the nudge.
fn fallback_label(key: &str) -> String {
    if key == "main" {
        "main session".to_string()
    } else {
        let prefix: String = key.chars().take(8).collect();
        format!("session {prefix}")
    }
}

struct MonitorState {
    /// (session key, request id) → pending request.
    pending: HashMap<(String, u64), PendingRequest>,
    /// session key → oldest undelivered urgent-notification escalation
    /// (unix-ms it appeared). One slot per session: an urgent burst
    /// collapses into one nudge like every other burst.
    escalations: HashMap<String, u64>,
    /// session key → last nudge unix-ms.
    last_nudge: HashMap<String, u64>,
    /// session key → user-set display name (explicit renames only).
    names: HashMap<String, String>,
}

impl MonitorState {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            escalations: HashMap::new(),
            last_nudge: HashMap::new(),
            names: HashMap::new(),
        }
    }

    fn observe(&mut self, event: &AppEvent) {
        match event {
            AppEvent::ApprovalRequired { session_id, id, .. } => {
                self.pending
                    .entry((session_key(session_id), *id))
                    .or_insert(PendingRequest {
                        kind: AttentionKind::Approval,
                        since_unix_ms: now_unix_ms(),
                    });
            }
            AppEvent::UserQuestionRequired { session_id, id, .. } => {
                self.pending
                    .entry((session_key(session_id), *id))
                    .or_insert(PendingRequest {
                        kind: AttentionKind::Question,
                        since_unix_ms: now_unix_ms(),
                    });
            }
            // Only urgent notifications escalate off the machine; info and
            // attention stay browser-side by design.
            AppEvent::UserNotification {
                session_id,
                urgency: crate::types::NotificationUrgency::Urgent,
                ..
            } => {
                self.escalations
                    .entry(session_key(session_id))
                    .or_insert_with(now_unix_ms);
            }
            AppEvent::ApprovalResolved { session_id, id, .. } => {
                self.pending.remove(&(session_key(session_id), *id));
            }
            // A finished/ended/interrupted session cannot still be waiting:
            // its blocked loop returned. Some exit paths (interrupt drain,
            // headless deny) skip ApprovalResolved, so clear by session.
            // A dead session's undelivered escalation dies with it too.
            AppEvent::TaskComplete { session_id, .. }
            | AppEvent::Interrupted { session_id, .. } => {
                let key = session_key(session_id);
                self.pending.retain(|(k, _), _| *k != key);
                self.escalations.remove(&key);
            }
            AppEvent::SessionEnded { session_id, .. } => {
                self.pending.retain(|(k, _), _| k != session_id);
                self.escalations.remove(session_id);
                self.names.remove(session_id);
                self.last_nudge.remove(session_id);
            }
            AppEvent::SessionRenameResult {
                session_id,
                name: Some(name),
                success: true,
                ..
            } => {
                self.names.insert(session_id.clone(), name.clone());
            }
            _ => {}
        }
    }

    fn label(&self, key: &str) -> String {
        self.names
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback_label(key))
    }

    /// Advance the one-shot escalations: returns the sessions to nudge NOW
    /// (kind `notify`, no grace) and drops every escalation whose owner is
    /// already informed — a dashboard connected now, or one seen since the
    /// notification appeared (the toast/transcript row rendered there).
    /// Escalations blocked only by the cooldown stay queued for a later
    /// tick: the owner is genuinely away and the escalation must still
    /// reach them once the pacing allows.
    fn take_due_escalations(
        &mut self,
        now: u64,
        connected: bool,
        last_seen: Option<u64>,
    ) -> Vec<(String, String)> {
        let mut due: Vec<(String, String)> = Vec::new();
        let mut drop_keys: Vec<String> = Vec::new();
        for (key, since) in &self.escalations {
            let input = NudgeInput {
                now_unix_ms: now,
                pending_since_unix_ms: *since,
                dashboards_connected: connected,
                last_dashboard_seen_unix_ms: last_seen,
                last_nudge_unix_ms: self.last_nudge.get(key).copied(),
            };
            if should_nudge_escalation(input) {
                due.push((key.clone(), self.label(key)));
            } else if connected || last_seen.is_some_and(|seen| seen >= *since) {
                // Seen on a dashboard: the escalation delivered browser-side.
                drop_keys.push(key.clone());
            }
            // Else: only the cooldown holds it — keep for a later tick.
        }
        for key in due.iter().map(|(key, _)| key).chain(drop_keys.iter()) {
            self.escalations.remove(key);
        }
        due.sort_by(|a, b| a.0.cmp(&b.0));
        due
    }

    /// Sessions due a nudge now: `(session key, kind, display label)` for
    /// the oldest pending request per session that passes [`should_nudge`].
    fn due(&self, now: u64, connected: bool, last_seen: Option<u64>) -> Vec<(String, AttentionKind, String)> {
        let mut oldest: HashMap<&str, &PendingRequest> = HashMap::new();
        for ((key, _), request) in &self.pending {
            let slot = oldest.entry(key.as_str()).or_insert(request);
            if request.since_unix_ms < slot.since_unix_ms {
                *slot = request;
            }
        }
        let mut due: Vec<(String, AttentionKind, String)> = oldest
            .into_iter()
            .filter(|(key, request)| {
                should_nudge(NudgeInput {
                    now_unix_ms: now,
                    pending_since_unix_ms: request.since_unix_ms,
                    dashboards_connected: connected,
                    last_dashboard_seen_unix_ms: last_seen,
                    last_nudge_unix_ms: self.last_nudge.get(*key).copied(),
                })
            })
            .map(|(key, request)| (key.to_string(), request.kind, self.label(key)))
            .collect();
        due.sort_by(|a, b| a.0.cmp(&b.0));
        due
    }
}

/// Spawn the pending-request monitor (once per process). Subscribes to the
/// event bus, tracks pending approvals/questions, and posts a signed nudge
/// to the Connect rendezvous when [`should_nudge`] fires.
pub(crate) fn spawn_attention_nudge_monitor(bus: EventBus) {
    if MONITOR_SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        let mut events = bus.subscribe();
        let mut state = MonitorState::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(MONITOR_TICK_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = events.recv() => {
                    match event {
                        Ok(event) => state.observe(&event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = tick.tick() => {
                    let now = now_unix_ms();
                    let connected = dashboards_connected();
                    if connected {
                        LAST_DASHBOARD_SEEN_UNIX_MS.store(now, Ordering::SeqCst);
                    }
                    if state.pending.is_empty() && state.escalations.is_empty() {
                        continue;
                    }
                    // Escalations first: marking last_nudge here also
                    // cooldown-suppresses a same-tick pending nudge for the
                    // session, so one push carries the attention.
                    let mut nudges: Vec<(String, AttentionKind, String)> = state
                        .take_due_escalations(now, connected, last_dashboard_seen())
                        .into_iter()
                        .map(|(key, label)| (key, AttentionKind::Notify, label))
                        .collect();
                    for (key, _, _) in &nudges {
                        state.last_nudge.insert(key.clone(), now);
                    }
                    nudges.extend(state.due(now, connected, last_dashboard_seen()));
                    for (key, kind, label) in nudges {
                        // Mark before sending: a failing rendezvous must not
                        // turn into a hammer loop — the cooldown paces retries.
                        state.last_nudge.insert(key.clone(), now);
                        // Cooldown-paced, so no arm can chat more than
                        // once per session per NUDGE_SESSION_COOLDOWN_MS.
                        match crate::connect_rendezvous::notify_attention(kind.as_str(), &label).await {
                            Ok(()) => eprintln!(
                                "[attention] nudged rendezvous: {} pending in {key}",
                                kind.as_str()
                            ),
                            // Unclaimed daemon / no rendezvous configured /
                            // network trouble: degrade silently — this daemon
                            // log line is the only trace.
                            Err(e) => eprintln!("[attention] nudge skipped: {e}"),
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> NudgeInput {
        NudgeInput {
            now_unix_ms: 1_000_000_000,
            pending_since_unix_ms: 1_000_000_000 - NUDGE_GRACE_MS,
            dashboards_connected: false,
            last_dashboard_seen_unix_ms: None,
            last_nudge_unix_ms: None,
        }
    }

    #[test]
    fn nudges_after_grace_with_no_dashboard_ever() {
        assert!(should_nudge(base_input()));
    }

    #[test]
    fn holds_inside_the_grace_period() {
        let mut input = base_input();
        input.pending_since_unix_ms = input.now_unix_ms - NUDGE_GRACE_MS + 1;
        assert!(!should_nudge(input));
    }

    #[test]
    fn a_connected_dashboard_suppresses() {
        let mut input = base_input();
        input.dashboards_connected = true;
        assert!(!should_nudge(input));
    }

    #[test]
    fn a_dashboard_seen_since_the_request_suppresses() {
        let mut input = base_input();
        // Connected (and disconnected) after the request appeared: the
        // owner saw it; no push.
        input.last_dashboard_seen_unix_ms = Some(input.pending_since_unix_ms + 1);
        assert!(!should_nudge(input));
        // Seen only BEFORE the request appeared: the closed-tab case; push.
        input.last_dashboard_seen_unix_ms = Some(input.pending_since_unix_ms - 1);
        assert!(should_nudge(input));
    }

    #[test]
    fn the_session_cooldown_collapses_bursts() {
        let mut input = base_input();
        input.last_nudge_unix_ms = Some(input.now_unix_ms - NUDGE_SESSION_COOLDOWN_MS + 1);
        assert!(!should_nudge(input));
        input.last_nudge_unix_ms = Some(input.now_unix_ms - NUDGE_SESSION_COOLDOWN_MS);
        assert!(should_nudge(input));
    }

    #[test]
    fn state_tracks_pending_lifecycle_and_oldest_per_session() {
        let mut state = MonitorState::new();
        let sid = Some("abc12345-XYZ".to_string());
        state.observe(&AppEvent::ApprovalRequired {
            session_id: sid.clone(),
            id: 1,
            command_preview: "secret command".to_string(),
            category: crate::autonomy::ActionCategory::CommandExec,
        });
        state.observe(&AppEvent::UserQuestionRequired {
            session_id: sid.clone(),
            id: 2,
            questions: Vec::new(),
        });
        assert_eq!(state.pending.len(), 2);

        // One entry per session (the oldest), well past grace, no dashboards.
        let now = now_unix_ms() + NUDGE_GRACE_MS + 1;
        let due = state.due(now, false, None);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "abc12345-XYZ");
        // Content never rides along: the label is the id-prefix fallback.
        assert_eq!(due[0].2, "session abc12345");

        // A connected dashboard suppresses everything.
        assert!(state.due(now, true, Some(now)).is_empty());

        state.observe(&AppEvent::ApprovalResolved {
            session_id: sid.clone(),
            id: 1,
            action: "approve".to_string(),
        });
        assert_eq!(state.pending.len(), 1);

        // Session end clears the rest.
        state.observe(&AppEvent::SessionEnded {
            session_id: "abc12345-XYZ".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        assert!(state.pending.is_empty());
    }

    #[test]
    fn task_complete_clears_a_sessions_requests() {
        let mut state = MonitorState::new();
        state.observe(&AppEvent::ApprovalRequired {
            session_id: None,
            id: 7,
            command_preview: String::new(),
            category: crate::autonomy::ActionCategory::NetworkRequest,
        });
        assert_eq!(state.pending.len(), 1);
        state.observe(&AppEvent::TaskComplete {
            session_id: None,
            reason: "Denied by user".to_string(),
            summary: None,
        });
        assert!(state.pending.is_empty());
    }

    #[test]
    fn explicit_renames_become_the_label() {
        let mut state = MonitorState::new();
        state.observe(&AppEvent::SessionRenameResult {
            session_id: "abc".to_string(),
            source: None,
            name: Some("deploy review".to_string()),
            success: true,
            message: String::new(),
        });
        state.observe(&AppEvent::ApprovalRequired {
            session_id: Some("abc".to_string()),
            id: 1,
            command_preview: String::new(),
            category: crate::autonomy::ActionCategory::CommandExec,
        });
        let now = now_unix_ms() + NUDGE_GRACE_MS + 1;
        let due = state.due(now, false, None);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].2, "deploy review");
    }

    #[test]
    fn escalations_skip_grace_but_respect_cooldown_and_dashboards() {
        // The urgent rule is the pending rule minus the grace period.
        let mut input = base_input();
        input.pending_since_unix_ms = input.now_unix_ms; // brand new
        assert!(!should_nudge(input));
        assert!(should_nudge_escalation(input));
        // A connected dashboard still suppresses (the toast delivered).
        input.dashboards_connected = true;
        assert!(!should_nudge_escalation(input));
        input.dashboards_connected = false;
        // The per-session cooldown still paces explicit escalations.
        input.last_nudge_unix_ms = Some(input.now_unix_ms - NUDGE_SESSION_COOLDOWN_MS + 1);
        assert!(!should_nudge_escalation(input));
    }

    fn urgent_notification(session_id: &str) -> AppEvent {
        AppEvent::UserNotification {
            session_id: Some(session_id.to_string()),
            id: "notif-1".to_string(),
            title: None,
            text: "the deploy is blocked on credentials".to_string(),
            urgency: crate::types::NotificationUrgency::Urgent,
            ts: 1,
        }
    }

    #[test]
    fn urgent_notifications_escalate_once_with_a_content_free_label() {
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc12345-XYZ"));
        // Only urgent escalates: info/attention stay browser-side.
        state.observe(&AppEvent::UserNotification {
            session_id: Some("other-session".to_string()),
            id: "notif-2".to_string(),
            title: None,
            text: "fyi".to_string(),
            urgency: crate::types::NotificationUrgency::Attention,
            ts: 1,
        });
        assert_eq!(state.escalations.len(), 1);

        // Immediately due — no grace — and the label is the content-free
        // id-prefix fallback, never the notification text.
        let now = now_unix_ms() + 1;
        let due = state.take_due_escalations(now, false, None);
        assert_eq!(due, vec![("abc12345-XYZ".to_string(), "session abc12345".to_string())]);
        // One-shot: dispatched escalations leave the queue.
        assert!(state.escalations.is_empty());
        assert!(state.take_due_escalations(now, false, None).is_empty());
    }

    #[test]
    fn escalations_seen_on_a_dashboard_drop_without_pushing() {
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc"));
        let now = now_unix_ms() + 1;
        // Connected right now: the toast rendered — drop, no push.
        assert!(state.take_due_escalations(now, true, Some(now)).is_empty());
        assert!(state.escalations.is_empty());

        // Seen since (connected after the notification, since disconnected):
        // the transcript row replayed — drop, no push.
        state.observe(&urgent_notification("abc"));
        let since = state.escalations["abc"];
        assert!(state.take_due_escalations(now, false, Some(since + 1)).is_empty());
        assert!(state.escalations.is_empty());
    }

    #[test]
    fn cooldown_blocked_escalations_stay_queued_for_a_later_tick() {
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc"));
        let now = now_unix_ms() + 1;
        state.last_nudge.insert("abc".to_string(), now - 1);
        // Cooldown holds it — but the owner is genuinely away, so it must
        // not be dropped: it fires once the cooldown elapses.
        assert!(state.take_due_escalations(now, false, None).is_empty());
        assert_eq!(state.escalations.len(), 1);
        let later = now + NUDGE_SESSION_COOLDOWN_MS;
        assert_eq!(state.take_due_escalations(later, false, None).len(), 1);
        assert!(state.escalations.is_empty());
    }

    #[test]
    fn session_teardown_clears_escalations() {
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc"));
        state.observe(&AppEvent::SessionEnded {
            session_id: "abc".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        assert!(state.escalations.is_empty());

        state.observe(&urgent_notification("def"));
        state.observe(&AppEvent::TaskComplete {
            session_id: Some("def".to_string()),
            reason: "done".to_string(),
            summary: None,
        });
        assert!(state.escalations.is_empty());
    }

    /// The daemon-side kind vocabulary is CLOSED and must match the
    /// service-side whitelist (`NOTIFY_KINDS` in `src/bin/connect/push.rs`)
    /// value for value — a kind added on one side only either never
    /// delivers or gets rejected as free text.
    #[test]
    fn attention_kind_vocabulary_is_pinned() {
        for kind in [
            AttentionKind::Approval,
            AttentionKind::Question,
            AttentionKind::Notify,
        ] {
            assert!(matches!(kind.as_str(), "approval" | "question" | "notify"));
        }
        assert_eq!(AttentionKind::Notify.as_str(), "notify");
    }

    #[test]
    fn dashboard_counter_never_underflows() {
        // Regardless of a stray extra disconnect, the counter stays sane.
        dashboard_disconnected();
        dashboard_connected();
        assert!(dashboards_connected());
        dashboard_disconnected();
        assert!(!dashboards_connected());
    }
}
