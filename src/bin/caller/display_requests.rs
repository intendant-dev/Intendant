//! The user-display request rail ("doorbell"): a scoped agent that wants
//! the user's real display (display 0, `user_session`) raises a request via
//! the `request_user_display` MCP tool; a dedicated dashboard popup shows
//! the reason, and the user's click — and only that — mints the grant.
//!
//! Trust invariants (do not weaken):
//! - This module never grants anything. It is bookkeeping for pending
//!   requests plus the oneshot that tells the blocked tool the outcome.
//!   The grant itself is minted by the control plane's
//!   [`ControlMsg::ResolveDisplayRequest`](crate::event::ControlMsg) arm —
//!   the same single-writer state flip + events the owner's own
//!   `grant_user_display` uses.
//! - A request is NEVER auto-approved. It deliberately does not share the
//!   command-approval id space or [`crate::event::ApprovalRegistry`]:
//!   `approve` / `approve_all` / any autonomy rule cannot reach a display
//!   request by construction — the only resolution path is the dedicated
//!   `resolve_display_request` control message from an owner surface.
//! - Fail closed: with no owner surface in the process (a `--no-web`
//!   headless daemon), a request is refused immediately — the
//!   headless-mode "denied-no-approver" semantics — instead of hanging.
//!
//! Spam resistance: at most one pending request per session; a deny (or a
//! timeout, which is a decline by absence) starts a per-session cooldown;
//! "Deny for this session" suppresses the session server-side until it
//! ends. The pending set also feeds the attention chain
//! (`static/app/57-attention-notifications.js` + `attention_nudge.rs`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

/// Default wait for a raised request when the caller does not say.
pub(crate) const DISPLAY_REQUEST_DEFAULT_WAIT_SECS: u64 = 120;
/// Hard cap on how long one tool call may block waiting for the user.
pub(crate) const DISPLAY_REQUEST_MAX_WAIT_SECS: u64 = 600;
/// Floor for the wait window (also what makes short timeouts testable).
pub(crate) const DISPLAY_REQUEST_MIN_WAIT_SECS: u64 = 1;
/// Reason cap: the popup shows a short justification, not a document.
pub(crate) const DISPLAY_REQUEST_REASON_MAX_BYTES: usize = 280;
/// After a deny (or a timeout — declined by absence), new requests from
/// the same session are refused for this long without raising a popup.
pub(crate) const DISPLAY_REQUEST_DENY_COOLDOWN_SECS: u64 = 5 * 60;
/// The "15 minutes" grant duration offered by the popup.
pub(crate) const DISPLAY_REQUEST_TIMED_GRANT_SECS: u64 = 15 * 60;

/// Access level a request asks for. `View` shares the user display's
/// stream with the agent (frames / dashboard visibility) WITHOUT flipping
/// the autonomy `user_display_granted` flag — computer-use input and
/// screenshots against `user_session` stay denied at the
/// `computer_use::execute_actions` chokepoint. `ViewAndControl` is the
/// full grant (today's flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayRequestAccess {
    View,
    ViewAndControl,
}

impl DisplayRequestAccess {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "view" => Some(Self::View),
            "view_and_control" | "control" | "view-and-control" => Some(Self::ViewAndControl),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::ViewAndControl => "view_and_control",
        }
    }
}

/// How long an approved grant lasts. `ThisSession` auto-revokes when the
/// requesting session ends; `Timed` revokes after
/// [`DISPLAY_REQUEST_TIMED_GRANT_SECS`]; `UntilRevoked` matches the
/// classic grant. Every auto-revocation goes through the existing
/// `RevokeUserDisplay` path (guard clear + `UserDisplayRevoked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayGrantDuration {
    ThisSession,
    Timed,
    UntilRevoked,
}

impl DisplayGrantDuration {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "this_session" | "session" | "this-session" => Some(Self::ThisSession),
            "15m" | "timed" | "15_minutes" | "15min" => Some(Self::Timed),
            "" | "until_revoked" | "until-revoked" | "unlimited" => Some(Self::UntilRevoked),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ThisSession => "this_session",
            Self::Timed => "15m",
            Self::UntilRevoked => "until_revoked",
        }
    }
}

/// The outcome delivered to the blocked `request_user_display` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DisplayRequestOutcome {
    Approved {
        access: DisplayRequestAccess,
        duration: DisplayGrantDuration,
    },
    Denied,
    /// "Deny for this session": denied now AND suppressed until the
    /// session ends.
    DeniedForSession,
    /// The request evaporated without a user decision (requesting session
    /// ended).
    Cancelled { reason: String },
}

impl DisplayRequestOutcome {
    /// Wire label used by the `display_request_resolved` event and the
    /// structured tool result.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Approved { .. } => "approved",
            Self::Denied => "denied",
            Self::DeniedForSession => "denied_for_session",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

/// What `raise` decided.
pub(crate) enum RaiseOutcome {
    /// A new request was registered: emit the raised event, then block on
    /// `rx` (with the caller's timeout).
    Raised {
        id: u64,
        rx: tokio::sync::oneshot::Receiver<DisplayRequestOutcome>,
        expires_unix_ms: u64,
    },
    /// This session already has a pending request; its status is returned
    /// without raising anything new.
    AlreadyPending {
        id: u64,
        access: DisplayRequestAccess,
        expires_unix_ms: u64,
    },
    /// The user chose "Deny for this session" earlier: refuse silently.
    Suppressed,
    /// Inside the post-deny (or post-timeout) cooldown window.
    Cooldown { retry_after_secs: u64 },
    /// No owner surface exists in this process (headless daemon): refuse
    /// immediately instead of blocking on a popup nobody can see.
    NoApprover,
}

/// What the control plane must do after a successful `resolve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveAction {
    /// Mint the grant (through the existing grant path) and, for a timed
    /// duration, arm the auto-revoke timer carrying `grant_token`.
    MintGrant {
        access: DisplayRequestAccess,
        duration: DisplayGrantDuration,
        grant_token: u64,
    },
    /// No grant: the request was denied (cooldown armed) or denied for
    /// the whole session (suppression armed).
    NoGrant,
}

/// Why a `resolve` did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// No pending request under that (session, id) — already resolved,
    /// timed out, or never existed.
    NotPending,
}

/// The user's decision, parsed from `ControlMsg::ResolveDisplayRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayRequestDecision {
    Approve,
    Deny,
    DenyForSession,
}

impl DisplayRequestDecision {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "approve" | "allow" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            "deny_session" | "deny_for_session" | "deny-for-session" => Some(Self::DenyForSession),
            _ => None,
        }
    }
}

/// Did the tool's wait window close before a decision arrived?
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TimeoutOutcome {
    /// The entry was still pending: it is now removed and the deny
    /// cooldown armed (declined by absence). The caller emits the
    /// `display_request_resolved` timeout event.
    TimedOut,
    /// A resolution won the race; its outcome is already on the oneshot.
    AlreadyResolved,
}

/// Actions `on_session_ended` tells the caller to perform.
#[derive(Debug, Default)]
pub(crate) struct SessionEndActions {
    /// A pending request from the session was cancelled (its waiter was
    /// notified); emit `display_request_resolved` with outcome
    /// "cancelled" for this id so dashboards drop the popup.
    pub(crate) cancelled_request_id: Option<u64>,
    /// An active this-session grant originated from this session: revoke
    /// it (dispatch the existing `RevokeUserDisplay` path).
    pub(crate) revoke_display_id: Option<u32>,
}

struct PendingRequest {
    id: u64,
    access: DisplayRequestAccess,
    reason: String,
    expires_unix_ms: u64,
    responder: tokio::sync::oneshot::Sender<DisplayRequestOutcome>,
}

/// A grant minted by the request rail that may need auto-revocation. A
/// manual grant/revoke from any owner path clears it (the arrangement is
/// superseded), so a stale timer or session end can never revoke an
/// arrangement the user made independently.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveGrant {
    token: u64,
    origin_session: String,
    duration: DisplayGrantDuration,
    display_id: u32,
}

#[derive(Default)]
struct RegistryInner {
    next_id: u64,
    /// session key → the (single) pending request for that session.
    pending: HashMap<String, PendingRequest>,
    /// Sessions the user chose "Deny for this session" for.
    suppressed: HashSet<String>,
    /// session key → unix-ms until which new requests are refused.
    cooldown_until: HashMap<String, u64>,
    active_grant: Option<ActiveGrant>,
}

/// Snapshot of one pending request, for the `/ws` bootstrap re-send so a
/// late-connecting dashboard still shows the popup.
#[derive(Debug, Clone)]
pub(crate) struct PendingSnapshot {
    pub(crate) session_key: String,
    pub(crate) id: u64,
    pub(crate) access: DisplayRequestAccess,
    pub(crate) reason: String,
    pub(crate) expires_unix_ms: u64,
}

/// Pending-request registry. Instance methods carry the whole behavior so
/// unit tests run against their own instances; production uses the
/// process-global [`registry`].
pub(crate) struct DisplayRequestRegistry {
    inner: Mutex<RegistryInner>,
}

impl DisplayRequestRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                next_id: 1,
                ..Default::default()
            }),
        }
    }

    /// Register a request for `session_key`. See [`RaiseOutcome`].
    pub(crate) fn raise(
        &self,
        session_key: &str,
        access: DisplayRequestAccess,
        reason: &str,
        wait_secs: u64,
        approver_available: bool,
        now_unix_ms: u64,
    ) -> RaiseOutcome {
        if !approver_available {
            return RaiseOutcome::NoApprover;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.suppressed.contains(session_key) {
            return RaiseOutcome::Suppressed;
        }
        if let Some(until) = inner.cooldown_until.get(session_key).copied() {
            if until > now_unix_ms {
                return RaiseOutcome::Cooldown {
                    retry_after_secs: (until - now_unix_ms).div_ceil(1000),
                };
            }
            inner.cooldown_until.remove(session_key);
        }
        if let Some(existing) = inner.pending.get(session_key) {
            return RaiseOutcome::AlreadyPending {
                id: existing.id,
                access: existing.access,
                expires_unix_ms: existing.expires_unix_ms,
            };
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let expires_unix_ms = now_unix_ms + wait_secs.saturating_mul(1000);
        inner.pending.insert(
            session_key.to_string(),
            PendingRequest {
                id,
                access,
                reason: reason.to_string(),
                expires_unix_ms,
                responder: tx,
            },
        );
        RaiseOutcome::Raised {
            id,
            rx,
            expires_unix_ms,
        }
    }

    /// Apply an owner decision to the pending request `(session_key, id)`.
    /// The waiting tool is notified on its oneshot; deny arms the
    /// cooldown; deny-for-session arms suppression. On approve the caller
    /// (the control plane) mints the grant.
    pub(crate) fn resolve(
        &self,
        session_key: &str,
        id: u64,
        decision: DisplayRequestDecision,
        duration: DisplayGrantDuration,
        now_unix_ms: u64,
    ) -> Result<ResolveAction, ResolveError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let matches = inner
            .pending
            .get(session_key)
            .is_some_and(|pending| pending.id == id);
        if !matches {
            return Err(ResolveError::NotPending);
        }
        let pending = inner
            .pending
            .remove(session_key)
            .expect("checked above under the same lock");
        match decision {
            DisplayRequestDecision::Approve => {
                let token = inner.next_id;
                inner.next_id += 1;
                inner.active_grant = Some(ActiveGrant {
                    token,
                    origin_session: session_key.to_string(),
                    duration,
                    display_id: 0,
                });
                // Send inside the lock: a concurrent timeout_pending that
                // observes the entry gone is then guaranteed to find the
                // outcome already on the channel.
                let _ = pending.responder.send(DisplayRequestOutcome::Approved {
                    access: pending.access,
                    duration,
                });
                Ok(ResolveAction::MintGrant {
                    access: pending.access,
                    duration,
                    grant_token: token,
                })
            }
            DisplayRequestDecision::Deny => {
                inner.cooldown_until.insert(
                    session_key.to_string(),
                    now_unix_ms + DISPLAY_REQUEST_DENY_COOLDOWN_SECS * 1000,
                );
                let _ = pending.responder.send(DisplayRequestOutcome::Denied);
                Ok(ResolveAction::NoGrant)
            }
            DisplayRequestDecision::DenyForSession => {
                inner.suppressed.insert(session_key.to_string());
                let _ = pending
                    .responder
                    .send(DisplayRequestOutcome::DeniedForSession);
                Ok(ResolveAction::NoGrant)
            }
        }
    }

    /// The blocked tool's wait window elapsed. Removes the entry (if a
    /// resolution didn't win the race) and arms the deny cooldown —
    /// a timeout is a decline by absence.
    pub(crate) fn timeout_pending(&self, session_key: &str, id: u64, now_unix_ms: u64) -> TimeoutOutcome {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let matches = inner
            .pending
            .get(session_key)
            .is_some_and(|pending| pending.id == id);
        if !matches {
            return TimeoutOutcome::AlreadyResolved;
        }
        inner.pending.remove(session_key);
        inner.cooldown_until.insert(
            session_key.to_string(),
            now_unix_ms + DISPLAY_REQUEST_DENY_COOLDOWN_SECS * 1000,
        );
        TimeoutOutcome::TimedOut
    }

    /// Take the active grant if `token` still names it — the timed
    /// auto-revoke's compare-and-take, so a stale timer can never revoke
    /// a newer arrangement.
    pub(crate) fn take_grant_if_current(&self, token: u64) -> Option<u32> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.active_grant.as_ref().is_some_and(|g| g.token == token) {
            return inner.active_grant.take().map(|g| g.display_id);
        }
        None
    }

    /// An owner granted the display manually (dashboard toggle, MCP
    /// `grant_user_display`, control message): any rail-minted arrangement
    /// (timed / this-session auto-revoke) is superseded.
    pub(crate) fn note_manual_grant(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.active_grant = None;
    }

    /// The user display was revoked (any path): the rail's arrangement is
    /// over.
    pub(crate) fn note_revoked(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.active_grant = None;
    }

    /// The requesting/originating session ended: cancel its pending
    /// request, clear its suppression + cooldown, and revoke a
    /// this-session grant it originated.
    pub(crate) fn on_session_ended(&self, session_key: &str) -> SessionEndActions {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut actions = SessionEndActions::default();
        if let Some(pending) = inner.pending.remove(session_key) {
            actions.cancelled_request_id = Some(pending.id);
            let _ = pending.responder.send(DisplayRequestOutcome::Cancelled {
                reason: "requesting session ended".to_string(),
            });
        }
        inner.suppressed.remove(session_key);
        inner.cooldown_until.remove(session_key);
        let ends_grant = inner.active_grant.as_ref().is_some_and(|grant| {
            grant.duration == DisplayGrantDuration::ThisSession
                && grant.origin_session == session_key
        });
        if ends_grant {
            actions.revoke_display_id = inner.active_grant.take().map(|g| g.display_id);
        }
        actions
    }

    /// Still-pending requests, for the `/ws` bootstrap re-send. Entries
    /// whose wait window already passed are skipped (their waiter is
    /// about to time them out).
    pub(crate) fn pending_snapshot(&self, now_unix_ms: u64) -> Vec<PendingSnapshot> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<PendingSnapshot> = inner
            .pending
            .iter()
            .filter(|(_, pending)| pending.expires_unix_ms > now_unix_ms)
            .map(|(session_key, pending)| PendingSnapshot {
                session_key: session_key.clone(),
                id: pending.id,
                access: pending.access,
                reason: pending.reason.clone(),
                expires_unix_ms: pending.expires_unix_ms,
            })
            .collect();
        rows.sort_by_key(|row| row.id);
        rows
    }
}

/// Session key for requests that carry no session id (foreground /
/// single-session shapes). Mirrors `attention_nudge::session_key`.
pub(crate) fn session_key(session_id: Option<&str>) -> String {
    session_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("main")
        .to_string()
}

pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The process-global registry production paths share.
pub(crate) fn registry() -> &'static DisplayRequestRegistry {
    static REGISTRY: LazyLock<DisplayRequestRegistry> = LazyLock::new(DisplayRequestRegistry::new);
    &REGISTRY
}

/// One-way latch: an owner surface (the web gateway, whose dashboards are
/// where the popup renders) exists in this process. Never cleared — a
/// gateway rebind does not un-exist the surface.
static APPROVER_SURFACE_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Called by the web gateway at spawn: dashboards can now (eventually)
/// connect, so requests may block on the popup instead of failing closed.
pub(crate) fn mark_approver_surface_available() {
    APPROVER_SURFACE_AVAILABLE.store(true, Ordering::SeqCst);
}

pub(crate) fn approver_surface_available() -> bool {
    APPROVER_SURFACE_AVAILABLE.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn raise_ok(
        registry: &DisplayRequestRegistry,
        session: &str,
        access: DisplayRequestAccess,
    ) -> (u64, tokio::sync::oneshot::Receiver<DisplayRequestOutcome>) {
        match registry.raise(session, access, "why", 120, true, NOW) {
            RaiseOutcome::Raised { id, rx, .. } => (id, rx),
            _ => panic!("expected Raised"),
        }
    }

    #[test]
    fn approve_resolves_the_waiter_and_returns_a_mint_action() {
        let registry = DisplayRequestRegistry::new();
        let (id, mut rx) = raise_ok(&registry, "sess-a", DisplayRequestAccess::ViewAndControl);
        let action = registry
            .resolve(
                "sess-a",
                id,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::Timed,
                NOW,
            )
            .expect("pending request resolves");
        match action {
            ResolveAction::MintGrant {
                access,
                duration,
                grant_token,
            } => {
                assert_eq!(access, DisplayRequestAccess::ViewAndControl);
                assert_eq!(duration, DisplayGrantDuration::Timed);
                // The timer's compare-and-take honors exactly this token.
                assert_eq!(registry.take_grant_if_current(grant_token), Some(0));
                assert_eq!(registry.take_grant_if_current(grant_token), None);
            }
            other => panic!("expected MintGrant, got {other:?}"),
        }
        assert_eq!(
            rx.try_recv().expect("outcome delivered"),
            DisplayRequestOutcome::Approved {
                access: DisplayRequestAccess::ViewAndControl,
                duration: DisplayGrantDuration::Timed,
            }
        );
    }

    #[test]
    fn deny_arms_the_cooldown_and_cooldown_expires() {
        let registry = DisplayRequestRegistry::new();
        let (id, mut rx) = raise_ok(&registry, "sess-b", DisplayRequestAccess::View);
        let action = registry
            .resolve(
                "sess-b",
                id,
                DisplayRequestDecision::Deny,
                DisplayGrantDuration::UntilRevoked,
                NOW,
            )
            .unwrap();
        assert_eq!(action, ResolveAction::NoGrant);
        assert_eq!(rx.try_recv().unwrap(), DisplayRequestOutcome::Denied);

        // Inside the cooldown window: refused without a new popup.
        match registry.raise("sess-b", DisplayRequestAccess::View, "again", 120, true, NOW + 1000) {
            RaiseOutcome::Cooldown { retry_after_secs } => {
                assert!(retry_after_secs > 0);
                assert!(retry_after_secs <= DISPLAY_REQUEST_DENY_COOLDOWN_SECS);
            }
            _ => panic!("expected Cooldown"),
        }
        // Past the window: a new request raises again.
        let after = NOW + DISPLAY_REQUEST_DENY_COOLDOWN_SECS * 1000 + 1;
        match registry.raise("sess-b", DisplayRequestAccess::View, "again", 120, true, after) {
            RaiseOutcome::Raised { .. } => {}
            _ => panic!("expected Raised after cooldown"),
        }
    }

    #[test]
    fn deny_for_session_suppresses_until_session_end() {
        let registry = DisplayRequestRegistry::new();
        let (id, mut rx) = raise_ok(&registry, "sess-c", DisplayRequestAccess::View);
        registry
            .resolve(
                "sess-c",
                id,
                DisplayRequestDecision::DenyForSession,
                DisplayGrantDuration::UntilRevoked,
                NOW,
            )
            .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DisplayRequestOutcome::DeniedForSession
        );
        // Every later request is insta-refused server-side.
        assert!(matches!(
            registry.raise("sess-c", DisplayRequestAccess::View, "x", 120, true, NOW + 1),
            RaiseOutcome::Suppressed
        ));
        // Other sessions are unaffected.
        assert!(matches!(
            registry.raise("sess-d", DisplayRequestAccess::View, "x", 120, true, NOW + 1),
            RaiseOutcome::Raised { .. }
        ));
        // Session end clears the suppression.
        registry.on_session_ended("sess-c");
        assert!(matches!(
            registry.raise("sess-c", DisplayRequestAccess::View, "x", 120, true, NOW + 2),
            RaiseOutcome::Raised { .. }
        ));
    }

    #[test]
    fn second_request_while_pending_reports_the_existing_one() {
        let registry = DisplayRequestRegistry::new();
        let (id, _rx) = raise_ok(&registry, "sess-e", DisplayRequestAccess::View);
        match registry.raise(
            "sess-e",
            DisplayRequestAccess::ViewAndControl,
            "second",
            120,
            true,
            NOW + 5,
        ) {
            RaiseOutcome::AlreadyPending {
                id: existing,
                access,
                ..
            } => {
                assert_eq!(existing, id);
                // The FIRST request's access is reported, not the new ask.
                assert_eq!(access, DisplayRequestAccess::View);
            }
            _ => panic!("expected AlreadyPending"),
        }
    }

    #[test]
    fn timeout_is_a_decline_by_absence_and_races_safely() {
        let registry = DisplayRequestRegistry::new();
        let (id, _rx) = raise_ok(&registry, "sess-f", DisplayRequestAccess::View);
        assert_eq!(
            registry.timeout_pending("sess-f", id, NOW + 120_000),
            TimeoutOutcome::TimedOut
        );
        // Timeout armed the cooldown.
        assert!(matches!(
            registry.raise("sess-f", DisplayRequestAccess::View, "x", 120, true, NOW + 121_000),
            RaiseOutcome::Cooldown { .. }
        ));
        // A resolution can no longer land on the timed-out id.
        assert_eq!(
            registry.resolve(
                "sess-f",
                id,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::UntilRevoked,
                NOW,
            ),
            Err(ResolveError::NotPending)
        );

        // The other side of the race: resolve wins, then the waiter's
        // timeout must see AlreadyResolved and read the outcome.
        let (id2, mut rx2) = raise_ok(&registry, "sess-g", DisplayRequestAccess::View);
        registry
            .resolve(
                "sess-g",
                id2,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::UntilRevoked,
                NOW,
            )
            .unwrap();
        assert_eq!(
            registry.timeout_pending("sess-g", id2, NOW),
            TimeoutOutcome::AlreadyResolved
        );
        assert!(matches!(
            rx2.try_recv().unwrap(),
            DisplayRequestOutcome::Approved { .. }
        ));
    }

    #[test]
    fn resolve_requires_the_matching_id() {
        let registry = DisplayRequestRegistry::new();
        let (id, _rx) = raise_ok(&registry, "sess-h", DisplayRequestAccess::View);
        assert_eq!(
            registry.resolve(
                "sess-h",
                id + 999,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::UntilRevoked,
                NOW,
            ),
            Err(ResolveError::NotPending)
        );
        // The real id still resolves.
        assert!(registry
            .resolve(
                "sess-h",
                id,
                DisplayRequestDecision::Deny,
                DisplayGrantDuration::UntilRevoked,
                NOW,
            )
            .is_ok());
    }

    #[test]
    fn session_end_cancels_pending_and_revokes_this_session_grants() {
        let registry = DisplayRequestRegistry::new();
        // A this-session grant from sess-i…
        let (id, _rx) = raise_ok(&registry, "sess-i", DisplayRequestAccess::ViewAndControl);
        registry
            .resolve(
                "sess-i",
                id,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::ThisSession,
                NOW,
            )
            .unwrap();
        // …and a pending request from the same session later on.
        let (id2, mut rx2) = raise_ok(&registry, "sess-i", DisplayRequestAccess::View);

        let actions = registry.on_session_ended("sess-i");
        assert_eq!(actions.cancelled_request_id, Some(id2));
        assert_eq!(actions.revoke_display_id, Some(0));
        match rx2.try_recv().unwrap() {
            DisplayRequestOutcome::Cancelled { reason } => {
                assert!(reason.contains("session ended"), "{reason}");
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
        // Idempotent: a second end does nothing.
        let again = registry.on_session_ended("sess-i");
        assert_eq!(again.cancelled_request_id, None);
        assert_eq!(again.revoke_display_id, None);
    }

    #[test]
    fn session_end_leaves_other_grants_alone() {
        let registry = DisplayRequestRegistry::new();
        let (id, _rx) = raise_ok(&registry, "sess-j", DisplayRequestAccess::ViewAndControl);
        registry
            .resolve(
                "sess-j",
                id,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::UntilRevoked,
                NOW,
            )
            .unwrap();
        // Until-revoked grants survive their origin session's end…
        assert_eq!(registry.on_session_ended("sess-j").revoke_display_id, None);
        // …and a DIFFERENT session's end never touches them either.
        let (id2, _rx2) = raise_ok(&registry, "sess-k", DisplayRequestAccess::ViewAndControl);
        registry
            .resolve(
                "sess-k",
                id2,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::ThisSession,
                NOW,
            )
            .unwrap();
        assert_eq!(registry.on_session_ended("sess-j").revoke_display_id, None);
        assert_eq!(
            registry.on_session_ended("sess-k").revoke_display_id,
            Some(0)
        );
    }

    #[test]
    fn manual_grant_and_revoke_supersede_rail_arrangements() {
        let registry = DisplayRequestRegistry::new();
        let (id, _rx) = raise_ok(&registry, "sess-l", DisplayRequestAccess::ViewAndControl);
        let token = match registry
            .resolve(
                "sess-l",
                id,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::Timed,
                NOW,
            )
            .unwrap()
        {
            ResolveAction::MintGrant { grant_token, .. } => grant_token,
            other => panic!("expected MintGrant, got {other:?}"),
        };
        // A manual owner grant supersedes: the timer's take must miss.
        registry.note_manual_grant();
        assert_eq!(registry.take_grant_if_current(token), None);

        // Same for an explicit revoke observed on the bus.
        let (id2, _rx2) = raise_ok(&registry, "sess-m", DisplayRequestAccess::ViewAndControl);
        let token2 = match registry
            .resolve(
                "sess-m",
                id2,
                DisplayRequestDecision::Approve,
                DisplayGrantDuration::Timed,
                NOW,
            )
            .unwrap()
        {
            ResolveAction::MintGrant { grant_token, .. } => grant_token,
            other => panic!("expected MintGrant, got {other:?}"),
        };
        registry.note_revoked();
        assert_eq!(registry.take_grant_if_current(token2), None);
    }

    #[test]
    fn no_approver_refuses_immediately() {
        let registry = DisplayRequestRegistry::new();
        assert!(matches!(
            registry.raise("sess-n", DisplayRequestAccess::View, "x", 120, false, NOW),
            RaiseOutcome::NoApprover
        ));
        // Nothing was registered.
        assert!(registry.pending_snapshot(NOW).is_empty());
    }

    #[test]
    fn pending_snapshot_lists_live_requests_only() {
        let registry = DisplayRequestRegistry::new();
        let (id, _rx) = raise_ok(&registry, "sess-o", DisplayRequestAccess::View);
        let rows = registry.pending_snapshot(NOW + 1000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].session_key, "sess-o");
        assert_eq!(rows[0].reason, "why");
        // Past the wait window the row is omitted (waiter is about to
        // time it out).
        assert!(registry.pending_snapshot(NOW + 121_000).is_empty());
    }

    #[test]
    fn parse_vocabularies_are_closed() {
        assert_eq!(
            DisplayRequestAccess::parse("view"),
            Some(DisplayRequestAccess::View)
        );
        assert_eq!(
            DisplayRequestAccess::parse("view_and_control"),
            Some(DisplayRequestAccess::ViewAndControl)
        );
        assert_eq!(
            DisplayRequestAccess::parse("control"),
            Some(DisplayRequestAccess::ViewAndControl)
        );
        assert_eq!(DisplayRequestAccess::parse("root"), None);

        assert_eq!(
            DisplayGrantDuration::parse("this_session"),
            Some(DisplayGrantDuration::ThisSession)
        );
        assert_eq!(
            DisplayGrantDuration::parse("15m"),
            Some(DisplayGrantDuration::Timed)
        );
        assert_eq!(
            DisplayGrantDuration::parse(""),
            Some(DisplayGrantDuration::UntilRevoked)
        );
        assert_eq!(DisplayGrantDuration::parse("forever and ever"), None);

        assert_eq!(
            DisplayRequestDecision::parse("approve"),
            Some(DisplayRequestDecision::Approve)
        );
        assert_eq!(
            DisplayRequestDecision::parse("deny"),
            Some(DisplayRequestDecision::Deny)
        );
        assert_eq!(
            DisplayRequestDecision::parse("deny_session"),
            Some(DisplayRequestDecision::DenyForSession)
        );
        assert_eq!(DisplayRequestDecision::parse("approve_all"), None);
    }

    #[test]
    fn session_key_defaults_to_main() {
        assert_eq!(session_key(None), "main");
        assert_eq!(session_key(Some("  ")), "main");
        assert_eq!(session_key(Some("sess")), "sess");
    }
}
