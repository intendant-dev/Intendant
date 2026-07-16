//! Per-session activity state machine — the honest, first-hand "is the
//! model actually doing something right now" signal behind the vitals
//! `activity` section.
//!
//! Every adapter (Claude Code reader, Codex reader, the native loop)
//! owns one [`ActivityMachine`] per session and feeds it
//! [`ActivityObservation`]s at the exact wire seams: our own dispatch
//! writes, stream deltas, item/tool transitions, rate-limit events, turn
//! results. The machine holds the raw state + evidence epochs and decides
//! when the *published* snapshot changed enough to re-emit — state flips
//! always, liveness heartbeats only when the epoch advanced by
//! [`HEARTBEAT_QUANTUM_SECS`] — so a minutes-long thinking block costs a
//! handful of vitals emissions, not one per delta.
//!
//! The `stalled` state is *derived*, never stored: a state whose evidence
//! includes a live byte stream carries `stalled_after_seconds`, and both
//! [`ActivityMachine::effective_state`] and the dashboard apply the same
//! rule (quiet past the threshold ⇒ stalled) to the wire epochs. States
//! without a byte-stream promise (tool execution, Codex reasoning items)
//! carry no threshold and never degrade — honest silence over a fake
//! alarm, and no per-second wire traffic either way.
//!
//! Derivation doctrine: state claims come ONLY from wire facts. In
//! particular "thinking" is claimed exactly while thinking deltas arrive
//! (Claude Code) or while the backend reports an open reasoning item
//! (Codex); it is never guessed from elapsed time, output volume, or
//! optimistic client bookkeeping.

use crate::types::{SessionActivityState, SessionActivityVitals};

/// Quiet threshold before a state that promises a live byte stream
/// (reasoning/text deltas, an awaited API response) degrades to
/// `stalled`. Streaming providers emit deltas sub-second; 20s of silence
/// on an armed stream means the call is stalled, retrying, or backing
/// off — while staying comfortably above [`HEARTBEAT_QUANTUM_SECS`] so a
/// quantized heartbeat can never false-trip it.
pub(crate) const STALL_AFTER_SECS: u32 = 20;

/// Liveness re-publish quantum: while bytes flow without a state change,
/// the published `last_stream_byte_epoch` only advances in steps of this
/// many seconds. Bounds vitals emissions to ~1 per quantum per streaming
/// session; must stay well below [`STALL_AFTER_SECS`].
pub(crate) const HEARTBEAT_QUANTUM_SECS: u64 = 5;

/// One wire fact, as observed by an adapter at its protocol seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivityObservation {
    /// A turn was dispatched: Intendant wrote the user message (Claude
    /// Code), the backend announced `turn/started` (Codex), or the native
    /// loop is about to call the provider. Seeds the liveness epoch —
    /// the stall clock for the awaited first byte starts here.
    TurnDispatched,
    /// Reasoning began on the wire. `delta_heartbeat` says whether this
    /// backend streams reasoning bytes (Claude Code thinking deltas:
    /// yes; Codex reasoning items: no mid-item bytes are promised, so
    /// quiet must not read as stalled).
    ReasoningStarted { delta_heartbeat: bool },
    /// A live reasoning byte (Claude Code `thinking_delta`) — the only
    /// evidence that keeps a heartbeat-armed `Reasoning` claim honest.
    ReasoningDelta,
    /// Response bytes: text deltas, streamed tool-call arguments, or a
    /// response item opening.
    ResponseDelta,
    /// One or more tools are executing (also re-asserted by tool output).
    ToolsRunning,
    /// The turn's open tools (or the current stream segment) settled and
    /// the model has to be called again — back to awaiting the API.
    SegmentSettled,
    /// A liveness-only wire byte that implies no state (message_start,
    /// bookkeeping notifications).
    StreamByte,
    /// The provider reported a non-allowed rate-limit status.
    RateLimited { resets_at_epoch: Option<u64> },
    /// The provider reported the limit window allowed again.
    RateLimitCleared,
    /// The turn ended (result, turn/completed, interrupt settled, process
    /// gone) — back to idle.
    TurnSettled,
}

/// Per-session activity state machine. Pure and clock-injected: every
/// entry point takes `now_epoch` (unix seconds), so tests drive it with
/// synthetic clocks and no timers exist anywhere.
#[derive(Debug, Default)]
pub(crate) struct ActivityMachine {
    state: SessionActivityState,
    since_epoch: u64,
    last_stream_byte_epoch: u64,
    delta_heartbeat: bool,
    effort: Option<String>,
    resets_at_epoch: Option<u64>,
    turn_active: bool,
    /// The snapshot consumers last saw; `observe` returns the next one
    /// only when it materially differs.
    published: Option<SessionActivityVitals>,
}

impl ActivityMachine {
    pub(crate) fn new(effort: Option<String>) -> Self {
        Self {
            effort,
            ..Self::default()
        }
    }

    /// Adopt the backend's own effort echo (first-hand only — callers
    /// pass values the backend itself stated, or the launch config).
    pub(crate) fn set_effort(&mut self, effort: Option<String>) {
        if let Some(effort) = effort
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
        {
            self.effort = Some(effort);
        }
    }

    /// Feed one wire observation. Returns the snapshot to publish when
    /// the observable section changed (state/effort/reset flips always;
    /// pure liveness only per [`HEARTBEAT_QUANTUM_SECS`]).
    pub(crate) fn observe(
        &mut self,
        obs: ActivityObservation,
        now_epoch: u64,
    ) -> Option<SessionActivityVitals> {
        // Between turns only a dispatch means anything: ambient wire
        // traffic (idle rate-limit refreshes, bookkeeping notifications)
        // must not resurrect an activity claim.
        if !self.turn_active && !matches!(obs, ActivityObservation::TurnDispatched) {
            return self.maybe_publish();
        }
        match obs {
            ActivityObservation::TurnDispatched => {
                self.turn_active = true;
                self.enter(SessionActivityState::AwaitingApi, true, now_epoch);
                self.mark_byte(now_epoch);
            }
            ActivityObservation::ReasoningStarted { delta_heartbeat } => {
                self.enter(SessionActivityState::Reasoning, delta_heartbeat, now_epoch);
                self.mark_byte(now_epoch);
            }
            ActivityObservation::ReasoningDelta => {
                self.enter(SessionActivityState::Reasoning, true, now_epoch);
                self.mark_byte(now_epoch);
            }
            ActivityObservation::ResponseDelta => {
                self.enter(SessionActivityState::Responding, true, now_epoch);
                self.mark_byte(now_epoch);
            }
            ActivityObservation::ToolsRunning => {
                self.enter(SessionActivityState::ToolRunning, false, now_epoch);
                self.mark_byte(now_epoch);
            }
            ActivityObservation::SegmentSettled => {
                self.enter(SessionActivityState::AwaitingApi, true, now_epoch);
                self.mark_byte(now_epoch);
            }
            ActivityObservation::StreamByte => {
                self.mark_byte(now_epoch);
            }
            ActivityObservation::RateLimited { resets_at_epoch } => {
                self.enter(SessionActivityState::RateLimited, false, now_epoch);
                self.resets_at_epoch = resets_at_epoch;
                self.mark_byte(now_epoch);
            }
            ActivityObservation::RateLimitCleared => {
                if self.state == SessionActivityState::RateLimited {
                    // The turn is still active and the stream is not
                    // flowing yet — honestly back to awaiting the API.
                    self.enter(SessionActivityState::AwaitingApi, true, now_epoch);
                    self.mark_byte(now_epoch);
                }
            }
            ActivityObservation::TurnSettled => {
                self.turn_active = false;
                self.enter(SessionActivityState::Idle, false, now_epoch);
            }
        }
        self.maybe_publish()
    }

    fn enter(&mut self, state: SessionActivityState, delta_heartbeat: bool, now_epoch: u64) {
        if self.state != state {
            self.state = state;
            self.since_epoch = now_epoch;
        }
        self.delta_heartbeat = delta_heartbeat;
        if state != SessionActivityState::RateLimited {
            self.resets_at_epoch = None;
        }
    }

    fn mark_byte(&mut self, now_epoch: u64) {
        self.last_stream_byte_epoch = self.last_stream_byte_epoch.max(now_epoch);
    }

    /// The current section value (raw state — `stalled` is derived, see
    /// [`Self::effective_state`]).
    pub(crate) fn snapshot(&self) -> SessionActivityVitals {
        let stall_armed = self.delta_heartbeat
            && matches!(
                self.state,
                SessionActivityState::Reasoning
                    | SessionActivityState::Responding
                    | SessionActivityState::AwaitingApi
            );
        SessionActivityVitals {
            state: self.state,
            since_epoch: self.since_epoch,
            last_stream_byte_epoch: self.last_stream_byte_epoch,
            stalled_after_seconds: stall_armed.then_some(STALL_AFTER_SECS),
            effort: self.effort.clone(),
            resets_at_epoch: self.resets_at_epoch,
        }
    }

    /// The state with the time degradation applied — the rule the
    /// dashboard mirrors client-side: a byte-stream-armed state quiet
    /// past its threshold reads as `Stalled` (with the silence duration
    /// derivable from `last_stream_byte_epoch`). States without a
    /// threshold never degrade. Production display derives this
    /// client-side (39-session-windows.js `deriveSessionActivity`); this
    /// twin pins the rule in the test suite.
    #[cfg(test)]
    pub(crate) fn effective_state(&self, now_epoch: u64) -> SessionActivityState {
        effective_activity_state(&self.snapshot(), now_epoch)
    }

    fn maybe_publish(&mut self) -> Option<SessionActivityVitals> {
        let next = self.snapshot();
        let changed = match self.published.as_ref() {
            None => next.state != SessionActivityState::Idle || next.since_epoch != 0,
            Some(prev) => {
                let liveness_moved = next
                    .last_stream_byte_epoch
                    .saturating_sub(prev.last_stream_byte_epoch)
                    >= HEARTBEAT_QUANTUM_SECS;
                let rest_changed = SessionActivityVitals {
                    last_stream_byte_epoch: prev.last_stream_byte_epoch,
                    ..next.clone()
                } != *prev;
                liveness_moved || rest_changed
            }
        };
        if !changed {
            return None;
        }
        self.published = Some(next.clone());
        Some(next)
    }
}

/// The shared stalled-degradation rule (the dashboard implements the same
/// derivation in `static/app/39-session-windows.js`; server-side it is
/// exercised only by tests, which pin it against the wire fields).
#[cfg(test)]
pub(crate) fn effective_activity_state(
    activity: &SessionActivityVitals,
    now_epoch: u64,
) -> SessionActivityState {
    if let Some(threshold) = activity.stalled_after_seconds {
        if !matches!(
            activity.state,
            SessionActivityState::Idle | SessionActivityState::RateLimited
        ) && now_epoch.saturating_sub(activity.last_stream_byte_epoch) > u64::from(threshold)
        {
            return SessionActivityState::Stalled;
        }
    }
    activity.state
}

/// Unix seconds now — the adapters' clock edge (tests inject their own).
pub(crate) fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Bus-publishing wrapper for the native loop: one machine per
/// `run_agent_loop` invocation, publishing hub-bound
/// [`crate::event::AppEvent::SessionActivity`] snapshots directly — the
/// native loop has no drain between it and the bus. External adapters
/// instead ride `AgentEvent::ActivityUpdate` through their drains.
pub(crate) struct ActivityPublisher {
    machine: std::sync::Mutex<ActivityMachine>,
    bus: crate::event::EventBus,
    session_id: String,
}

impl ActivityPublisher {
    pub(crate) fn new(bus: crate::event::EventBus, session_id: String) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            machine: std::sync::Mutex::new(ActivityMachine::default()),
            bus,
            session_id,
        })
    }

    pub(crate) fn observe(&self, obs: ActivityObservation) {
        let published = {
            let mut machine = match self.machine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            machine.observe(obs, epoch_seconds())
        };
        if let Some(activity) = published {
            self.bus.send(crate::event::AppEvent::SessionActivity {
                session_id: Some(self.session_id.clone()),
                activity,
            });
        }
    }
}

/// Settles the turn when dropped, so every native loop exit path — task
/// complete, interrupt, budget stop, provider error — retires the
/// activity claim instead of leaving a phantom "responding".
pub(crate) struct ActivityTurnGuard(pub(crate) std::sync::Arc<ActivityPublisher>);

impl Drop for ActivityTurnGuard {
    fn drop(&mut self) {
        self.0.observe(ActivityObservation::TurnSettled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ActivityObservation as Obs;

    #[test]
    fn dispatch_then_first_bytes_walk_awaiting_reasoning_responding() {
        let mut m = ActivityMachine::new(Some("max".into()));
        let s = m
            .observe(Obs::TurnDispatched, 100)
            .expect("dispatch publishes");
        assert_eq!(s.state, SessionActivityState::AwaitingApi);
        assert_eq!(s.since_epoch, 100);
        assert_eq!(s.last_stream_byte_epoch, 100);
        assert_eq!(s.effort.as_deref(), Some("max"));
        assert_eq!(
            s.stalled_after_seconds,
            Some(STALL_AFTER_SECS),
            "an awaited API response is a byte-stream promise"
        );

        let s = m
            .observe(
                Obs::ReasoningStarted {
                    delta_heartbeat: true,
                },
                103,
            )
            .expect("state flip publishes");
        assert_eq!(s.state, SessionActivityState::Reasoning);
        assert_eq!(s.since_epoch, 103);
        assert_eq!(s.stalled_after_seconds, Some(STALL_AFTER_SECS));

        // Same-state deltas keep `since` (the thinking block's elapsed
        // anchor) while advancing liveness.
        assert!(
            m.observe(Obs::ReasoningDelta, 104).is_none(),
            "sub-quantum heartbeat"
        );
        let s = m
            .observe(Obs::ReasoningDelta, 103 + HEARTBEAT_QUANTUM_SECS)
            .expect("quantum-aligned heartbeat publishes");
        assert_eq!(s.state, SessionActivityState::Reasoning);
        assert_eq!(s.since_epoch, 103, "since anchors the thinking block");
        assert_eq!(s.last_stream_byte_epoch, 103 + HEARTBEAT_QUANTUM_SECS);

        let s = m
            .observe(Obs::ResponseDelta, 110)
            .expect("state flip publishes");
        assert_eq!(s.state, SessionActivityState::Responding);
        assert_eq!(s.since_epoch, 110);
    }

    #[test]
    fn thinking_requires_recent_deltas_quiet_degrades_to_stalled() {
        let mut m = ActivityMachine::new(None);
        m.observe(Obs::TurnDispatched, 100);
        m.observe(
            Obs::ReasoningStarted {
                delta_heartbeat: true,
            },
            101,
        );
        m.observe(Obs::ReasoningDelta, 102);
        // Deltas recent: the reasoning claim holds.
        let within = 102 + u64::from(STALL_AFTER_SECS);
        assert_eq!(m.effective_state(within), SessionActivityState::Reasoning);
        // Quiet past the threshold: never keep claiming "thinking".
        assert_eq!(
            m.effective_state(within + 1),
            SessionActivityState::Stalled,
            "a heartbeat-armed reasoning claim must degrade when deltas stop"
        );
        // A fresh delta restores the claim.
        m.observe(Obs::ReasoningDelta, within + 30);
        assert_eq!(
            m.effective_state(within + 31),
            SessionActivityState::Reasoning
        );
    }

    #[test]
    fn deltaless_reasoning_never_claims_stalled_or_heartbeat() {
        // Codex shape: reasoning items open on the wire but promise no
        // mid-item bytes — quiet is normal, stalled must not be claimed.
        let mut m = ActivityMachine::new(Some("high".into()));
        m.observe(Obs::TurnDispatched, 100);
        let s = m
            .observe(
                Obs::ReasoningStarted {
                    delta_heartbeat: false,
                },
                101,
            )
            .expect("state flip publishes");
        assert_eq!(s.state, SessionActivityState::Reasoning);
        assert_eq!(
            s.stalled_after_seconds, None,
            "no byte promise, no stall claim"
        );
        assert_eq!(
            m.effective_state(101 + 10 * u64::from(STALL_AFTER_SECS)),
            SessionActivityState::Reasoning,
            "honest degradation: silence stays a reasoning-item claim"
        );
    }

    #[test]
    fn awaiting_api_stalls_but_tools_never_do() {
        let mut m = ActivityMachine::new(None);
        m.observe(Obs::TurnDispatched, 100);
        assert_eq!(
            m.effective_state(100 + u64::from(STALL_AFTER_SECS) + 1),
            SessionActivityState::Stalled,
            "an unanswered API call is the classic stall"
        );

        m.observe(Obs::ToolsRunning, 130);
        assert_eq!(
            m.effective_state(130 + 20 * u64::from(STALL_AFTER_SECS)),
            SessionActivityState::ToolRunning,
            "quiet long-running tools are normal, never 'stalled'"
        );
        // Tools settle → the model must be called again → awaiting-api,
        // whose stall clock starts at settle time.
        let s = m
            .observe(Obs::SegmentSettled, 500)
            .expect("state flip publishes");
        assert_eq!(s.state, SessionActivityState::AwaitingApi);
        assert_eq!(s.last_stream_byte_epoch, 500);
    }

    #[test]
    fn rate_limited_carries_reset_and_clears_to_awaiting() {
        let mut m = ActivityMachine::new(None);
        m.observe(Obs::TurnDispatched, 100);
        let s = m
            .observe(
                Obs::RateLimited {
                    resets_at_epoch: Some(4000),
                },
                110,
            )
            .expect("state flip publishes");
        assert_eq!(s.state, SessionActivityState::RateLimited);
        assert_eq!(s.resets_at_epoch, Some(4000));
        assert_eq!(s.stalled_after_seconds, None, "a countdown, not a stall");
        assert_eq!(
            m.effective_state(110 + 10 * u64::from(STALL_AFTER_SECS)),
            SessionActivityState::RateLimited
        );

        let s = m
            .observe(Obs::RateLimitCleared, 200)
            .expect("state flip publishes");
        assert_eq!(s.state, SessionActivityState::AwaitingApi);
        assert_eq!(
            s.resets_at_epoch, None,
            "reset countdown retired with the state"
        );

        // Cleared while already streaming: no-op (never regress a live state).
        m.observe(Obs::ResponseDelta, 210);
        assert!(m.observe(Obs::RateLimitCleared, 211).is_none());
        assert_eq!(m.snapshot().state, SessionActivityState::Responding);
    }

    #[test]
    fn turn_settled_goes_idle_and_ambient_bytes_stay_idle() {
        let mut m = ActivityMachine::new(None);
        m.observe(Obs::TurnDispatched, 100);
        m.observe(Obs::ResponseDelta, 101);
        let s = m.observe(Obs::TurnSettled, 150).expect("idle publishes");
        assert_eq!(s.state, SessionActivityState::Idle);
        assert_eq!(s.since_epoch, 150);

        // Ambient between-turn traffic (idle rate-limit refreshes, codex
        // bookkeeping) must not resurrect an activity claim.
        assert!(m.observe(Obs::StreamByte, 200).is_none());
        assert!(m
            .observe(
                Obs::RateLimited {
                    resets_at_epoch: Some(9000)
                },
                201
            )
            .is_none());
        assert!(m.observe(Obs::ResponseDelta, 202).is_none());
        assert_eq!(m.snapshot().state, SessionActivityState::Idle);
        assert_eq!(m.effective_state(500), SessionActivityState::Idle);

        // The next dispatch starts a fresh turn.
        let s = m
            .observe(Obs::TurnDispatched, 300)
            .expect("dispatch publishes");
        assert_eq!(s.state, SessionActivityState::AwaitingApi);
        assert_eq!(s.since_epoch, 300);
    }

    #[test]
    fn heartbeat_publishing_is_quantized() {
        let mut m = ActivityMachine::new(None);
        m.observe(Obs::TurnDispatched, 100);
        m.observe(Obs::ResponseDelta, 100);
        // A flood of deltas inside one quantum publishes nothing new.
        for t in 101..(100 + HEARTBEAT_QUANTUM_SECS) {
            assert!(m.observe(Obs::ResponseDelta, t).is_none(), "t={t}");
        }
        let s = m
            .observe(Obs::ResponseDelta, 100 + HEARTBEAT_QUANTUM_SECS)
            .expect("quantum boundary publishes");
        assert_eq!(s.last_stream_byte_epoch, 100 + HEARTBEAT_QUANTUM_SECS);
    }

    #[test]
    fn effort_prefers_backend_echo_and_never_blanks() {
        let mut m = ActivityMachine::new(Some("medium".into()));
        assert_eq!(m.snapshot().effort.as_deref(), Some("medium"));
        // Backend echo upgrades the launch value…
        m.set_effort(Some("xhigh".into()));
        assert_eq!(m.snapshot().effort.as_deref(), Some("xhigh"));
        // …but an absent/blank echo never blanks a known value.
        m.set_effort(None);
        m.set_effort(Some("  ".into()));
        assert_eq!(m.snapshot().effort.as_deref(), Some("xhigh"));
        // Effort changes publish even without a state change.
        m.observe(Obs::TurnDispatched, 100);
        m.set_effort(Some("low".into()));
        let s = m
            .observe(Obs::StreamByte, 101)
            .expect("effort change publishes");
        assert_eq!(s.effort.as_deref(), Some("low"));
    }

    #[test]
    fn idle_machine_publishes_nothing_until_first_turn() {
        let mut m = ActivityMachine::new(None);
        assert!(m.observe(Obs::StreamByte, 50).is_none());
        assert!(
            m.observe(Obs::TurnSettled, 60).is_none(),
            "idle → idle is not news"
        );
        assert!(m.observe(Obs::TurnDispatched, 70).is_some());
    }

    #[test]
    fn wire_shape_serializes_kebab_states_and_camel_fields() {
        // The dashboard consumes these exact strings; pin them.
        let activity = SessionActivityVitals {
            state: SessionActivityState::ToolRunning,
            since_epoch: 1,
            last_stream_byte_epoch: 2,
            stalled_after_seconds: Some(20),
            effort: Some("high".into()),
            resets_at_epoch: None,
        };
        let json = serde_json::to_string(&activity).expect("serializes");
        assert!(json.contains("\"state\":\"tool-running\""), "{json}");
        assert!(json.contains("\"sinceEpoch\":1"), "{json}");
        assert!(json.contains("\"lastStreamByteEpoch\":2"), "{json}");
        assert!(json.contains("\"stalledAfterSeconds\":20"), "{json}");
        for state in [
            SessionActivityState::Reasoning,
            SessionActivityState::Responding,
            SessionActivityState::AwaitingApi,
            SessionActivityState::RateLimited,
            SessionActivityState::Stalled,
            SessionActivityState::Idle,
        ] {
            let s = serde_json::to_string(&state).expect("state serializes");
            assert!(
                [
                    "\"reasoning\"",
                    "\"responding\"",
                    "\"awaiting-api\"",
                    "\"rate-limited\"",
                    "\"stalled\"",
                    "\"idle\""
                ]
                .contains(&s.as_str()),
                "unexpected wire spelling {s}"
            );
        }
    }
}
