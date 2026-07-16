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
/// payload. New attention kinds extend this enum + the service-side
/// whitelist (`NOTIFY_KINDS` in `src/bin/connect/push.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionKind {
    Approval,
    Question,
    /// A scoped agent asked to access the user's display
    /// (`request_user_display`); the popup waits for the owner's click.
    DisplayRequest,
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
            AttentionKind::DisplayRequest => "display_request",
            AttentionKind::Notify => "notify",
        }
    }
}

/// Which id space a pending entry's `id` belongs to. Approvals and
/// questions share the approval registry's id space (one `ApprovalResolved`
/// clears either); display requests have their own registry and counter,
/// so `id` values can collide numerically — the space keeps a display
/// request's resolution from clearing an unrelated approval and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum IdSpace {
    Approval,
    DisplayRequest,
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
    let age = input
        .now_unix_ms
        .saturating_sub(input.pending_since_unix_ms);
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
    /// (session key, id space, request id) → pending request.
    pending: HashMap<(String, IdSpace, u64), PendingRequest>,
    /// session key → oldest undelivered urgent-notification escalation
    /// (unix-ms it appeared). One slot per session: an urgent burst
    /// collapses into one nudge like every other burst. An entry outlives
    /// TaskComplete/Interrupted (the final-turn escalation race) and leaves
    /// only by dispatch, by dashboard-seen, or with its session
    /// (SessionEnded).
    escalations: HashMap<String, u64>,
    /// session key → last nudge unix-ms.
    last_nudge: HashMap<String, u64>,
    /// session key → user-set display name (explicit renames only).
    names: HashMap<String, String>,
    /// superseded session id → canonical session id, from `SessionIdentity`
    /// rotations: an external backend announcing its native id re-keys
    /// lifecycle events (TaskComplete/SessionEnded) while the fixed
    /// session-scoped MCP URL keeps `notify_user` emitting the wrapper id
    /// for the session's whole life. Every map touch resolves through
    /// these links so the two id streams meet on one key — without that,
    /// SessionEnded under the backend id would strand a wrapper-keyed
    /// escalation that then pushes for a dead session. Bounded: one entry
    /// per announced identity, and the links die with their session on
    /// SessionEnded.
    aliases: HashMap<String, String>,
}

impl MonitorState {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            escalations: HashMap::new(),
            last_nudge: HashMap::new(),
            names: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    /// Resolve a session key through the identity links. Chains stay short
    /// (wrapper → native → resumed-native); the hop cap keeps a malformed
    /// cycle from wedging the monitor.
    fn canon(&self, key: &str) -> String {
        let mut current = key;
        for _ in 0..8 {
            match self.aliases.get(current) {
                Some(next) => current = next,
                None => break,
            }
        }
        current.to_string()
    }

    fn canon_key(&self, session_id: &Option<String>) -> String {
        self.canon(&session_key(session_id))
    }

    /// Record an identity rotation `old → new` and migrate any state
    /// already parked under the old key. Migration alone is not enough —
    /// the wrapper id keeps arriving after rotation (`notify_user` is
    /// scoped to a fixed MCP URL) — and the alias alone is not enough —
    /// pre-rotation entries already sit under the old key. So: both.
    fn link_identity(&mut self, old_id: &str, new_id: &str) {
        let old = old_id.trim();
        let new = new_id.trim();
        if old.is_empty() || new.is_empty() || old == new {
            return;
        }
        let source_key = self.canon(old);
        let target = self.canon(new);
        if source_key == target {
            // Already linked: just keep the direct edge short. `target`
            // can equal `old` here (the announcement inverted an existing
            // link) — never record a self-edge, so no cycle can form.
            if old != target {
                self.aliases.insert(old.to_string(), target);
            }
            return;
        }
        self.aliases.insert(old.to_string(), target.clone());
        if source_key != old {
            // A superseded canonical (chained rotation) forwards too.
            self.aliases.insert(source_key.clone(), target.clone());
        }
        // Migrate pending requests…
        let stranded: Vec<_> = self
            .pending
            .keys()
            .filter(|(k, _, _)| *k == source_key)
            .cloned()
            .collect();
        for (k, space, id) in stranded {
            if let Some(request) = self.pending.remove(&(k, space, id)) {
                self.pending
                    .entry((target.clone(), space, id))
                    .or_insert(request);
            }
        }
        // …the escalation slot (keep the oldest undelivered)…
        if let Some(since) = self.escalations.remove(&source_key) {
            let slot = self.escalations.entry(target.clone()).or_insert(since);
            *slot = (*slot).min(since);
        }
        // …the cooldown (keep the most recent nudge — strongest pacing)…
        if let Some(nudged) = self.last_nudge.remove(&source_key) {
            let slot = self.last_nudge.entry(target.clone()).or_insert(nudged);
            *slot = (*slot).max(nudged);
        }
        // …and the display name (a name set under the new key is newer).
        if let Some(name) = self.names.remove(&source_key) {
            self.names.entry(target).or_insert(name);
        }
    }

    fn observe(&mut self, event: &AppEvent) {
        match event {
            AppEvent::ApprovalRequired { session_id, id, .. } => {
                let key = self.canon_key(session_id);
                self.pending
                    .entry((key, IdSpace::Approval, *id))
                    .or_insert(PendingRequest {
                        kind: AttentionKind::Approval,
                        since_unix_ms: now_unix_ms(),
                    });
            }
            AppEvent::UserQuestionRequired { session_id, id, .. } => {
                let key = self.canon_key(session_id);
                self.pending
                    .entry((key, IdSpace::Approval, *id))
                    .or_insert(PendingRequest {
                        kind: AttentionKind::Question,
                        since_unix_ms: now_unix_ms(),
                    });
            }
            // The display-request doorbell has its own registry and id
            // counter (deliberately outside the approval registry), so its
            // raise/resolve pair tracks in its own id space.
            AppEvent::DisplayRequestRaised { session_id, id, .. } => {
                let key = self.canon_key(session_id);
                self.pending
                    .entry((key, IdSpace::DisplayRequest, *id))
                    .or_insert(PendingRequest {
                        kind: AttentionKind::DisplayRequest,
                        since_unix_ms: now_unix_ms(),
                    });
            }
            AppEvent::DisplayRequestResolved { session_id, id, .. } => {
                let key = self.canon_key(session_id);
                self.pending.remove(&(key, IdSpace::DisplayRequest, *id));
            }
            // Only urgent notifications escalate off the machine; info and
            // attention stay browser-side by design.
            AppEvent::UserNotification {
                session_id,
                urgency: crate::types::NotificationUrgency::Urgent,
                ts,
                ..
            } => {
                // Stamp with EMISSION time, not observation time: the tick
                // arm awaits rendezvous sends inline, so the monitor can
                // observe a queued notification well after it was emitted,
                // and a dashboard that displayed the toast and disconnected
                // inside that gap must still count as "seen since" —
                // otherwise the owner who watched it gets pushed anyway.
                // ts == 0 (absent) falls back to now; a future ts (clock
                // skew) clamps to now so it cannot dodge the seen-since
                // check — same clock domain, cheap insurance.
                let now = now_unix_ms();
                let since = match *ts {
                    0 => now,
                    emitted => emitted.min(now),
                };
                let key = self.canon_key(session_id);
                self.escalations.entry(key).or_insert(since);
            }
            AppEvent::ApprovalResolved { session_id, id, .. } => {
                let key = self.canon_key(session_id);
                self.pending.remove(&(key, IdSpace::Approval, *id));
            }
            // A finished/interrupted task cannot still be waiting on an
            // approval or question: its blocked loop returned. Some exit
            // paths (interrupt drain, headless deny) skip ApprovalResolved,
            // so clear those by session. Two kinds deliberately survive
            // TaskComplete/Interrupted: display requests — their waiter is
            // the blocked MCP call, not the agent loop's turn — and
            // undelivered urgent escalations, which have no waiter at all.
            // `notify_user` is fire-and-forget, so the canonical final-turn
            // urgent "come look, I'm done/blocked" is chased by TaskComplete
            // (or an interrupt — those can fire with nobody attending:
            // external-agent aborts, automation) within one monitor tick,
            // and an away owner must still get the push. Survival cannot
            // re-deliver: dispatch stays one-shot ([`Self::take_due_escalations`]
            // removes an escalation when it fires or a dashboard sees it).
            // Both kinds clear on SessionEnded like everything else — a
            // dead session's undelivered escalation dies with it.
            AppEvent::TaskComplete { session_id, .. }
            | AppEvent::Interrupted { session_id, .. } => {
                let key = self.canon_key(session_id);
                self.pending
                    .retain(|(k, space, _), _| *k != key || *space == IdSpace::DisplayRequest);
            }
            AppEvent::SessionEnded { session_id, .. } => {
                let key = self.canon(session_id);
                self.pending.retain(|(k, _, _), _| *k != key);
                self.escalations.remove(&key);
                self.names.remove(&key);
                self.last_nudge.remove(&key);
                // The identity links die with their session: sweep every
                // alias that resolves to the ended canonical key (wrapper
                // ids and superseded backend ids alike).
                let dead: Vec<String> = self
                    .aliases
                    .keys()
                    .filter(|alias| self.canon(alias) == key)
                    .cloned()
                    .collect();
                for alias in dead {
                    self.aliases.remove(&alias);
                }
            }
            AppEvent::SessionRenameResult {
                session_id,
                name: Some(name),
                success: true,
                ..
            } => {
                let key = self.canon(session_id);
                self.names.insert(key, name.clone());
            }
            // External backends announce their native id mid-session:
            // lifecycle events re-key to it (`rotate_external_identity`,
            // the supervisor's `apply_session_identity`) while the
            // session-scoped MCP URL keeps `notify_user` emitting the
            // wrapper id. Alias exactly when the supervisor re-keys (same
            // canonicality gate), so this monitor's canonical key is the
            // id SessionEnded will actually carry.
            AppEvent::SessionIdentity {
                session_id,
                source,
                backend_session_id,
            } if crate::external_agent::source_session_id_is_canonical(
                source,
                backend_session_id,
            ) =>
            {
                self.link_identity(session_id, backend_session_id);
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
    /// (A dashboard that connects DURING the send can render the replayed
    /// notification a push is already in flight for — benign overlap: the
    /// escalation left the queue when it dispatched, so the POST itself
    /// still fires at most once.)
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
    fn due(
        &self,
        now: u64,
        connected: bool,
        last_seen: Option<u64>,
    ) -> Vec<(String, AttentionKind, String)> {
        let mut oldest: HashMap<&str, &PendingRequest> = HashMap::new();
        for ((key, _, _), request) in &self.pending {
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
    fn display_requests_track_in_their_own_id_space() {
        let mut state = MonitorState::new();
        let sid = Some("sess-1".to_string());
        // Same numeric id in both spaces: a display request and a command
        // approval must not clobber each other.
        state.observe(&AppEvent::ApprovalRequired {
            session_id: sid.clone(),
            id: 1,
            command_preview: String::new(),
            category: crate::autonomy::ActionCategory::CommandExec,
        });
        state.observe(&AppEvent::DisplayRequestRaised {
            session_id: sid.clone(),
            id: 1,
            access: "view".to_string(),
            reason: "verify the fix on your screen".to_string(),
            expires_unix_ms: now_unix_ms() + 120_000,
        });
        assert_eq!(state.pending.len(), 2);

        // The nudge label carries only the KIND, never the reason text.
        let now = now_unix_ms() + NUDGE_GRACE_MS + 1;
        let due = state.due(now, false, None);
        assert_eq!(due.len(), 1, "one nudge per session");

        // Resolving the display request leaves the approval pending…
        state.observe(&AppEvent::DisplayRequestResolved {
            session_id: sid.clone(),
            id: 1,
            outcome: "denied".to_string(),
            access: None,
            duration: None,
        });
        assert_eq!(state.pending.len(), 1);
        // …and its surviving entry is the approval, not the request.
        assert!(state
            .pending
            .contains_key(&("sess-1".to_string(), IdSpace::Approval, 1)));

        // ApprovalResolved on the same id clears the approval only.
        state.observe(&AppEvent::DisplayRequestRaised {
            session_id: sid.clone(),
            id: 2,
            access: "view".to_string(),
            reason: "again".to_string(),
            expires_unix_ms: now_unix_ms() + 120_000,
        });
        state.observe(&AppEvent::ApprovalResolved {
            session_id: sid.clone(),
            id: 1,
            action: "approve".to_string(),
        });
        assert_eq!(state.pending.len(), 1);
        assert!(state
            .pending
            .contains_key(&("sess-1".to_string(), IdSpace::DisplayRequest, 2)));

        // TaskComplete clears loop-blocked requests but NOT the display
        // request (its waiter is the blocked MCP call, not the turn).
        state.observe(&AppEvent::TaskComplete {
            session_id: sid.clone(),
            reason: "done".to_string(),
            summary: None,
        });
        assert!(state
            .pending
            .contains_key(&("sess-1".to_string(), IdSpace::DisplayRequest, 2)));
        // SessionEnded clears everything for the session.
        state.observe(&AppEvent::SessionEnded {
            session_id: "sess-1".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        assert!(state.pending.is_empty());
    }

    #[test]
    fn display_request_kind_rides_the_nudge_wire_vocabulary() {
        // The Connect service whitelists kinds (NOTIFY_KINDS in
        // src/bin/connect/push.rs); this pin keeps the daemon-side string
        // in lockstep with the service-side vocabulary.
        assert_eq!(AttentionKind::DisplayRequest.as_str(), "display_request");
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
        urgent_notification_at(session_id, now_unix_ms())
    }

    fn urgent_notification_at(session_id: &str, ts: u64) -> AppEvent {
        AppEvent::UserNotification {
            session_id: Some(session_id.to_string()),
            id: "notif-1".to_string(),
            title: None,
            text: "the deploy is blocked on credentials".to_string(),
            urgency: crate::types::NotificationUrgency::Urgent,
            ts,
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
        assert_eq!(
            due,
            vec![("abc12345-XYZ".to_string(), "session abc12345".to_string())]
        );
        // One-shot: dispatched escalations leave the queue.
        assert!(state.escalations.is_empty());
        assert!(state.take_due_escalations(now, false, None).is_empty());
    }

    #[test]
    fn escalations_keep_their_emission_time_when_observed_late() {
        // The tick arm awaits rendezvous sends inline, so the monitor can
        // observe a queued urgent notification well after it was emitted.
        // The seen-since suppression must compare against EMISSION time:
        // a dashboard that displayed the toast and disconnected inside
        // that gap counts as informed — no push.
        let emitted = now_unix_ms() - 60_000;
        let seen = emitted + 5_000; // disconnect stamped after the toast
        let mut state = MonitorState::new();
        state.observe(&urgent_notification_at("abc", emitted)); // observed "now"
        assert_eq!(state.escalations["abc"], emitted);
        assert!(state
            .take_due_escalations(now_unix_ms(), false, Some(seen))
            .is_empty());
        assert!(state.escalations.is_empty(), "seen browser-side: dropped");

        // Seen strictly BEFORE emission: the owner has not seen this one;
        // it still pushes.
        state.observe(&urgent_notification_at("abc", emitted));
        assert_eq!(
            state
                .take_due_escalations(now_unix_ms(), false, Some(emitted - 1))
                .len(),
            1
        );
    }

    #[test]
    fn escalation_timestamps_fall_back_and_clamp() {
        // ts == 0 (absent) falls back to observation time; a future ts
        // (clock skew) clamps to now so it cannot dodge the seen-since
        // suppression by sitting "in the future".
        let mut state = MonitorState::new();
        let before = now_unix_ms();
        state.observe(&urgent_notification_at("zero", 0));
        state.observe(&urgent_notification_at("future", before + 3_600_000));
        let after = now_unix_ms();
        for key in ["zero", "future"] {
            let since = state.escalations[key];
            assert!(since >= before && since <= after, "{key}: {since}");
        }
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
        assert!(state
            .take_due_escalations(now, false, Some(since + 1))
            .is_empty());
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
    fn final_turn_escalations_survive_task_complete_and_still_deliver() {
        // The canonical race: `notify_user` is fire-and-forget, so an
        // urgent "come look, I'm done/blocked" in the final turn is chased
        // by TaskComplete within the same monitor tick. The escalation
        // must survive it and still reach the away owner.
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc12345-XYZ"));
        state.observe(&AppEvent::TaskComplete {
            session_id: Some("abc12345-XYZ".to_string()),
            reason: "done".to_string(),
            summary: None,
        });
        assert_eq!(state.escalations.len(), 1);
        let now = now_unix_ms() + 1;
        assert_eq!(
            state.take_due_escalations(now, false, None),
            vec![("abc12345-XYZ".to_string(), "session abc12345".to_string())]
        );

        // Interrupts share the teardown arm and can fire with nobody
        // attending (external-agent aborts, automation): same survival.
        state.observe(&urgent_notification("def"));
        state.observe(&AppEvent::Interrupted {
            session_id: Some("def".to_string()),
            reason: "user requested".to_string(),
        });
        assert_eq!(state.take_due_escalations(now, false, None).len(), 1);
    }

    #[test]
    fn task_complete_survivors_deliver_exactly_once() {
        // One-shot: dispatch removes the escalation, so later ticks —
        // even past the cooldown — and later TaskCompletes cannot turn
        // the survival into a re-push loop.
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc"));
        state.observe(&AppEvent::TaskComplete {
            session_id: Some("abc".to_string()),
            reason: "done".to_string(),
            summary: None,
        });
        let now = now_unix_ms() + 1;
        assert_eq!(state.take_due_escalations(now, false, None).len(), 1);
        assert!(state.escalations.is_empty());
        assert!(state.take_due_escalations(now, false, None).is_empty());
        let later = now + NUDGE_SESSION_COOLDOWN_MS + 1;
        assert!(state.take_due_escalations(later, false, None).is_empty());

        // A TaskComplete arriving after delivery resurrects nothing.
        state.observe(&AppEvent::TaskComplete {
            session_id: Some("abc".to_string()),
            reason: "done".to_string(),
            summary: None,
        });
        assert!(state.take_due_escalations(later, false, None).is_empty());
    }

    #[test]
    fn session_ended_clears_a_task_complete_survivor_undelivered() {
        // SessionEnded stays the teardown: a survivor that has not fired
        // by the time its session dies is dropped, never pushed.
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc"));
        state.observe(&AppEvent::TaskComplete {
            session_id: Some("abc".to_string()),
            reason: "done".to_string(),
            summary: None,
        });
        assert_eq!(state.escalations.len(), 1);
        state.observe(&AppEvent::SessionEnded {
            session_id: "abc".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        assert!(state.escalations.is_empty());
        assert!(state
            .take_due_escalations(now_unix_ms() + 1, false, None)
            .is_empty());
    }

    fn identity(old: &str, new: &str) -> AppEvent {
        AppEvent::SessionIdentity {
            session_id: old.to_string(),
            source: "codex".to_string(),
            backend_session_id: new.to_string(),
        }
    }

    #[test]
    fn identity_rotation_canonicalizes_every_touch() {
        // External sessions rotate ids: lifecycle events re-key to the
        // backend-native id while the fixed session-scoped MCP URL keeps
        // notify_user emitting the wrapper id. Every map touch resolves
        // through the identity link so the two id streams meet on one key.
        let mut state = MonitorState::new();
        // Pre-rotation state parks under the wrapper id…
        state.observe(&AppEvent::SessionRenameResult {
            session_id: "wrapper-1".to_string(),
            source: None,
            name: Some("deploy review".to_string()),
            success: true,
            message: String::new(),
        });
        state.observe(&urgent_notification("wrapper-1"));
        state.observe(&identity("wrapper-1", "backend-1"));
        // …and migrates to the canonical key at rotation.
        assert_eq!(state.escalations.len(), 1);
        assert!(state.escalations.contains_key("backend-1"));
        assert_eq!(
            state.names.get("backend-1").map(String::as_str),
            Some("deploy review")
        );

        // A post-rotation urgent under the WRAPPER id lands on the same
        // canonical slot (no duplicate), keeps the oldest timestamp, and
        // the due label uses the migrated rename.
        let first_since = state.escalations["backend-1"];
        state.observe(&urgent_notification("wrapper-1"));
        assert_eq!(state.escalations.len(), 1);
        assert_eq!(state.escalations["backend-1"], first_since);
        let due = state.take_due_escalations(now_unix_ms() + 1, false, None);
        assert_eq!(
            due,
            vec![("backend-1".to_string(), "deploy review".to_string())]
        );
        assert!(state.escalations.is_empty(), "delivered exactly once");
        assert!(state
            .take_due_escalations(now_unix_ms() + 1, false, None)
            .is_empty());

        // TaskComplete arrives under the BACKEND id and still clears a
        // wrapper-keyed pending approval (canonicalized on insert).
        state.observe(&AppEvent::ApprovalRequired {
            session_id: Some("wrapper-1".to_string()),
            id: 9,
            command_preview: String::new(),
            category: crate::autonomy::ActionCategory::CommandExec,
        });
        assert!(state
            .pending
            .contains_key(&("backend-1".to_string(), IdSpace::Approval, 9)));
        state.observe(&AppEvent::TaskComplete {
            session_id: Some("backend-1".to_string()),
            reason: "done".to_string(),
            summary: None,
        });
        assert!(state.pending.is_empty());
    }

    #[test]
    fn session_ended_clears_rotated_identity_state() {
        // The reunification exists exactly so this holds: notify_user
        // keeps the wrapper id for the session's whole life, lifecycle
        // events rotate to the backend id, and SessionEnded — which
        // carries the backend id — must still tear down EVERYTHING:
        // pending, escalation, name, cooldown, and the identity links
        // themselves. Chained rotations (a resume announcing another
        // native id) resolve transitively.
        let mut state = MonitorState::new();
        state.observe(&identity("wrapper-1", "backend-1"));
        state.observe(&identity("backend-1", "backend-2"));
        state.observe(&urgent_notification("wrapper-1")); // undelivered
        state.observe(&AppEvent::ApprovalRequired {
            session_id: Some("wrapper-1".to_string()),
            id: 3,
            command_preview: String::new(),
            category: crate::autonomy::ActionCategory::CommandExec,
        });
        state.observe(&AppEvent::SessionRenameResult {
            session_id: "wrapper-1".to_string(),
            source: None,
            name: Some("rotating".to_string()),
            success: true,
            message: String::new(),
        });
        state.last_nudge.insert("backend-2".to_string(), 5);
        assert!(state.escalations.contains_key("backend-2"));
        assert!(state
            .pending
            .contains_key(&("backend-2".to_string(), IdSpace::Approval, 3)));

        state.observe(&AppEvent::SessionEnded {
            session_id: "backend-2".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        assert!(state.pending.is_empty());
        assert!(
            state.escalations.is_empty(),
            "undelivered escalation dies with the session"
        );
        assert!(state.names.is_empty());
        assert!(state.last_nudge.is_empty());
        assert!(
            state.aliases.is_empty(),
            "identity links die with the session"
        );
    }

    #[test]
    fn identity_links_mirror_the_supervisor_gate() {
        // The alias must move exactly when the supervisor's canonical key
        // moves (apply_session_identity): a placeholder announcement the
        // supervisor rejects must not redirect attention state toward an
        // id lifecycle events will never carry.
        let mut state = MonitorState::new();
        state.observe(&AppEvent::SessionIdentity {
            session_id: "wrapper-1".to_string(),
            source: "claude-code".to_string(),
            backend_session_id: "claude-code-session".to_string(), // placeholder
        });
        state.observe(&AppEvent::SessionIdentity {
            session_id: "wrapper-1".to_string(),
            source: "unknown-backend".to_string(),
            backend_session_id: "backend-9".to_string(),
        });
        assert!(state.aliases.is_empty());
        // Self-links are refused too (a backend re-announcing the id the
        // monitor already treats as canonical).
        state.observe(&identity("same", "same"));
        state.observe(&identity("wrapper-1", "backend-1"));
        state.observe(&identity("backend-1", "wrapper-1")); // inversion
        assert_eq!(state.canon("wrapper-1"), "backend-1");
        assert_eq!(state.canon("backend-1"), "backend-1");
    }

    #[test]
    fn session_teardown_clears_escalations() {
        // SessionEnded is the teardown that drops an undelivered
        // escalation: the session is gone, there is nothing left to come
        // look at.
        let mut state = MonitorState::new();
        state.observe(&urgent_notification("abc"));
        state.observe(&AppEvent::SessionEnded {
            session_id: "abc".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        assert!(state.escalations.is_empty());

        // TaskComplete is NOT teardown: the session lives on, and the
        // final-turn urgent escalation racing it must survive to reach an
        // away owner (delivery itself is pinned by
        // final_turn_escalations_survive_task_complete_and_still_deliver).
        state.observe(&urgent_notification("def"));
        state.observe(&AppEvent::TaskComplete {
            session_id: Some("def".to_string()),
            reason: "done".to_string(),
            summary: None,
        });
        assert_eq!(state.escalations.len(), 1);
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
