//! Native usage rail: derive per-session `AppEvent::UsageSnapshot`s from
//! the bus's `ModelResponse` stream.
//!
//! Until the TUI was retired this derivation lived in the TUI app state
//! (single-session counters + `main_usage_snapshot()`); gutting the TUI
//! dropped it, which silently killed the dashboard usage meter and the
//! cache/limits vitals for native sessions (caught by the
//! session-vitals smoke). This is its backend-neutral home: a bus
//! listener with **per-session** counters — which also covers
//! supervisor-spawned native children the TUI's single-session state
//! never did.
//!
//! External backends attach no usage to their `ModelResponse` events
//! (their usage arrives pre-built via `AgentEvent::Usage` →
//! `AppEvent::UsageSnapshot` from the drains), so the zero-usage guard
//! keeps this rail from rebroadcasting zeros over their real numbers —
//! the same guard the TUI derivation carried.

use crate::event::{AppEvent, EventBus};
use crate::provider::TokenUsage;
use std::collections::HashMap;

/// Identity of the startup-resolved native provider, stamped on every
/// derived snapshot. Sessions all share the daemon's provider
/// resolution today (per-session provider overrides don't exist for
/// native sessions), matching the fidelity the TUI-era rail had.
#[derive(Clone, Debug, Default)]
pub struct ProviderIdentity {
    pub provider: String,
    pub model: String,
    pub context_window: u64,
}

impl ProviderIdentity {
    pub fn from_provider(provider: Option<&dyn crate::provider::ChatProvider>) -> Self {
        match provider {
            Some(p) => Self {
                provider: p.name().to_string(),
                model: p.model().to_string(),
                context_window: p.context_window(),
            },
            None => Self::default(),
        }
    }
}

/// Bound on tracked sessions. `SessionEnded` is the normal prune; this
/// only guards against a runaway session-id source.
const MAX_TRACKED_SESSIONS: usize = 256;

#[derive(Default)]
struct SessionCounters {
    total_tokens: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    cache_creation_tokens: u64,
    budget_pct: f64,
    last: Option<TokenUsage>,
}

/// Pure fold: feed it the bus stream, it returns the `UsageSnapshot`
/// events to emit. Kept free of the bus so tests drive it directly.
#[derive(Default)]
pub struct UsageRailState {
    identity: ProviderIdentity,
    sessions: HashMap<String, SessionCounters>,
    /// The foreground/primary session id, learned from
    /// `SessionStarted`/`SessionAttached` — the fallback scope for
    /// events that carry no session id (exactly how the TUI-era rail
    /// scoped them via its own `session_id` state).
    current_session: String,
}

impl UsageRailState {
    pub fn new(identity: ProviderIdentity) -> Self {
        Self {
            identity,
            ..Self::default()
        }
    }

    fn resolve_session(&self, session_id: Option<&str>) -> Option<String> {
        match session_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sid) => Some(sid.to_string()),
            None if !self.current_session.is_empty() => Some(self.current_session.clone()),
            None => None,
        }
    }

    fn snapshot(
        identity: &ProviderIdentity,
        counters: &SessionCounters,
    ) -> crate::frontend::ModelUsageSnapshot {
        let last = counters.last.as_ref();
        crate::frontend::ModelUsageSnapshot {
            provider: identity.provider.clone(),
            model: identity.model.clone(),
            tokens_used: counters.total_tokens,
            context_window: identity.context_window,
            hard_context_window: Some(identity.context_window),
            usage_pct: counters.budget_pct,
            prompt_tokens: counters.prompt_tokens,
            completion_tokens: counters.completion_tokens,
            cached_tokens: counters.cached_tokens,
            cache_creation_tokens: counters.cache_creation_tokens,
            last_cache_read_tokens: last.map(|u| u.cached_tokens).unwrap_or(0),
            last_cache_creation_tokens: last.map(|u| u.cache_creation_tokens).unwrap_or(0),
            last_uncached_input_tokens: last
                .map(|u| {
                    u.prompt_tokens
                        .saturating_sub(u.cached_tokens + u.cache_creation_tokens)
                })
                .unwrap_or(0),
            cache_ttl_seconds: last.and_then(|u| u.cache_ttl_seconds),
            limits: last
                .map(|u| u.rate_limit_windows.clone())
                .unwrap_or_default(),
        }
    }

    fn evict_over_cap(&mut self) {
        while self.sessions.len() > MAX_TRACKED_SESSIONS {
            // Arbitrary-key eviction: purely defensive, normal flow
            // prunes via SessionEnded.
            let Some(key) = self.sessions.keys().next().cloned() else {
                break;
            };
            self.sessions.remove(&key);
        }
    }

    /// Fold one bus event; returns a derived `UsageSnapshot` to emit,
    /// if this event changed anything worth broadcasting.
    pub fn on_event(&mut self, event: &AppEvent) -> Option<AppEvent> {
        match event {
            AppEvent::SessionStarted { session_id, .. }
            | AppEvent::SessionAttached { session_id, .. } => {
                self.current_session = session_id.clone();
                None
            }
            AppEvent::SessionEnded { session_id, .. } => {
                self.sessions.remove(session_id);
                if self.current_session == *session_id {
                    self.current_session.clear();
                }
                None
            }
            AppEvent::TurnStarted {
                session_id,
                budget_pct,
                ..
            } => {
                if let Some(sid) = self.resolve_session(session_id.as_deref()) {
                    self.sessions.entry(sid).or_default().budget_pct = *budget_pct;
                    self.evict_over_cap();
                }
                None
            }
            AppEvent::ModelResponse {
                session_id, usage, ..
            } => {
                // External backends attach no usage to ModelResponse —
                // deriving a snapshot from untouched counters would
                // rebroadcast zeros over their real numbers.
                let has_usage = usage.total_tokens > 0
                    || usage.prompt_tokens > 0
                    || usage.completion_tokens > 0
                    || usage.cached_tokens > 0
                    || usage.cache_creation_tokens > 0;
                if !has_usage {
                    return None;
                }
                let resolved = self.resolve_session(session_id.as_deref());
                let counters = self
                    .sessions
                    .entry(resolved.clone().unwrap_or_default())
                    .or_default();
                counters.total_tokens += usage.total_tokens;
                counters.prompt_tokens += usage.prompt_tokens;
                counters.completion_tokens += usage.completion_tokens;
                counters.cached_tokens += usage.cached_tokens;
                counters.cache_creation_tokens += usage.cache_creation_tokens;
                counters.last = Some(usage.clone());
                let main = Self::snapshot(&self.identity, counters);
                self.evict_over_cap();
                Some(AppEvent::UsageSnapshot {
                    session_id: resolved,
                    main,
                    presence: None,
                })
            }
            _ => None,
        }
    }
}

/// Spawn the rail: subscribe to the bus, fold, and re-broadcast the
/// derived snapshots. Safe against self-feedback — the fold only reacts
/// to `ModelResponse`/lifecycle events, never to `UsageSnapshot`.
pub fn spawn_native_usage_rail(
    bus: EventBus,
    identity: ProviderIdentity,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        let mut state = UsageRailState::new(identity);
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(derived) = state.on_event(&event) {
                        bus.send(derived);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ProviderIdentity {
        ProviderIdentity {
            provider: "mock".into(),
            model: "mock-model".into(),
            context_window: 200_000,
        }
    }

    fn usage(prompt: u64, completion: u64, cached: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cached_tokens: cached,
            ..Default::default()
        }
    }

    fn response(session_id: Option<&str>, u: TokenUsage) -> AppEvent {
        AppEvent::ModelResponse {
            session_id: session_id.map(str::to_string),
            turn: 1,
            content: "hi".into(),
            usage: u,
            reasoning: None,
            source: None,
        }
    }

    /// Cumulative counters + the freshest per-request sample (cache
    /// fields, limits) — the faithful port of the TUI-era derivation.
    #[test]
    fn accumulates_per_session_and_carries_cache_and_limits() {
        let mut rail = UsageRailState::new(identity());
        let first = rail
            .on_event(&response(Some("s1"), usage(100, 20, 0)))
            .expect("first response derives a snapshot");
        match &first {
            AppEvent::UsageSnapshot {
                session_id, main, ..
            } => {
                assert_eq!(session_id.as_deref(), Some("s1"));
                assert_eq!(main.provider, "mock");
                assert_eq!(main.context_window, 200_000);
                assert_eq!(main.prompt_tokens, 100);
                assert_eq!(main.tokens_used, 120);
            }
            other => panic!("expected UsageSnapshot, got {other:?}"),
        }

        let mut with_limits = usage(200, 30, 150);
        with_limits.cache_ttl_seconds = Some(300);
        with_limits.rate_limit_windows = vec![crate::types::SessionLimitWindow {
            label: "5h".into(),
            used_pct: Some(42),
            resets_at_epoch: None,
            status: None,
        }];
        let second = rail
            .on_event(&response(Some("s1"), with_limits))
            .expect("second response derives a snapshot");
        match &second {
            AppEvent::UsageSnapshot { main, .. } => {
                assert_eq!(main.prompt_tokens, 300, "counters are cumulative");
                assert_eq!(main.tokens_used, 350);
                assert_eq!(main.last_cache_read_tokens, 150, "last-sample, not sum");
                assert_eq!(main.last_uncached_input_tokens, 50);
                assert_eq!(main.cache_ttl_seconds, Some(300));
                assert_eq!(main.limits.len(), 1);
                assert_eq!(main.limits[0].used_pct, Some(42));
            }
            other => panic!("expected UsageSnapshot, got {other:?}"),
        }
    }

    /// Zero-usage ModelResponse (external backends) must derive nothing.
    #[test]
    fn zero_usage_responses_are_ignored() {
        let mut rail = UsageRailState::new(identity());
        assert!(rail
            .on_event(&response(Some("s1"), usage(0, 0, 0)))
            .is_none());
    }

    /// Events without a session id scope to the foreground session
    /// announced by SessionStarted — the TUI-era behavior.
    #[test]
    fn session_id_falls_back_to_the_announced_session() {
        let mut rail = UsageRailState::new(identity());
        let _ = rail.on_event(&AppEvent::SessionStarted {
            session_id: "fg-1".into(),
            task: None,
        });
        let derived = rail
            .on_event(&response(None, usage(10, 5, 0)))
            .expect("derives against the foreground session");
        match derived {
            AppEvent::UsageSnapshot { session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some("fg-1"));
            }
            other => panic!("expected UsageSnapshot, got {other:?}"),
        }
    }

    /// TurnStarted's budget rides the next snapshot as usage_pct, and
    /// SessionEnded retires the counters.
    #[test]
    fn budget_folds_and_ended_prunes() {
        let mut rail = UsageRailState::new(identity());
        let _ = rail.on_event(&AppEvent::TurnStarted {
            session_id: Some("s1".into()),
            turn: 3,
            budget_pct: 41.5,
            remaining: 10,
        });
        let derived = rail
            .on_event(&response(Some("s1"), usage(10, 5, 0)))
            .expect("snapshot");
        match derived {
            AppEvent::UsageSnapshot { main, .. } => assert_eq!(main.usage_pct, 41.5),
            other => panic!("expected UsageSnapshot, got {other:?}"),
        }
        let _ = rail.on_event(&AppEvent::SessionEnded {
            session_id: "s1".into(),
            reason: "done".into(),
            error_kind: None,
        });
        assert!(rail.sessions.is_empty());
    }
}
