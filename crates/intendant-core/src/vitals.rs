//! Session vitals: the git / prompt-cache / rate-limit chips shown by
//! the dashboard and Station (the operator-statusline port). Plain
//! data, shared by providers, the session supervisor, and frontends.

use serde::{Deserialize, Serialize};

/// Per-session working-tree/branch state for the vitals chips (the
/// statusline-port git segment). All probes are fetch-free reads of local
/// refs; `merge_parity` is `"clean"` / `"conflict"` from an in-memory
/// `git merge-tree`, or empty when not applicable (identical refs, old
/// git). `unpushed` is `None` when no upstream is configured — a visible
/// zero means "checked and synced", absence means "nothing to check".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGitVitals {
    pub branch: String,
    pub dirty_files: u32,
    pub ahead: u32,
    pub behind: u32,
    /// The comparison ref for ahead/behind (e.g. `origin/main`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub primary_ref: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub merge_parity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpushed: Option<u32>,
    /// Primary branch's unpushed count, shown when the session is not on
    /// the primary branch itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_unpushed: Option<u32>,
}

/// Prompt-cache lifecycle for the vitals chips: `hit_pct` is the share of
/// the latest request's prompt read from cache; the TTL countdown derives
/// client-side from `last_activity_epoch + ttl_seconds` so no per-second
/// events are needed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCacheVitals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_pct: Option<u8>,
    /// Unix seconds of the last usage-bearing provider activity.
    pub last_activity_epoch: u64,
    /// Provider prompt-cache TTL in seconds; `None` when the provider's TTL
    /// is unknown (the countdown is hidden, the hit receipt still shows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u32>,
}

/// One provider rate-limit window (subscription 5h/7d, per-minute API
/// windows, …) for the vitals gauges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLimitWindow {
    pub label: String,
    /// Percent of the window consumed. `None` when the provider reports the
    /// window without a utilization figure (Claude Code 2.1.2xx dropped it
    /// from `rate_limit_event` in normal operation) — the gauge then shows
    /// the window's status/reset instead of a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_epoch: Option<u64>,
    /// Provider-reported window status ("allowed", "allowed_warning",
    /// "rejected", …) when the wire carries one; severity fallback for
    /// utilization-less windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// What a session's model/backend is verifiably doing right now — the
/// states of the per-session activity machine (`session_activity.rs`).
/// Every value is claimed from wire facts (stream deltas, item
/// transitions, rate-limit events, our own dispatch writes), never from
/// timing heuristics over content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionActivityState {
    /// Reasoning/thinking evidence is on the wire: live thinking deltas
    /// (Claude Code with partial messages) or an open reasoning item
    /// (Codex — which promises no mid-item bytes, so
    /// `stalled_after_seconds` is absent there).
    Reasoning,
    /// Assistant text / tool-call arguments are streaming.
    Responding,
    /// One or more tools are executing. Tool silence is normal (a quiet
    /// long-running command), so this state never degrades to stalled.
    ToolRunning,
    /// A turn is dispatched (or the model must be called again after
    /// tools settled) and no response bytes have arrived yet.
    AwaitingApi,
    /// No turn is running, but backend-announced background tasks the
    /// session started are — it parked to wait and wakes itself when one
    /// finishes. `background_tasks` carries their short descriptions.
    /// Claimed only from wire evidence (the backend's own task events),
    /// never guessed; quiet is normal here, so it never degrades to
    /// stalled.
    ParkedOnTasks,
    /// The provider reported a non-allowed rate-limit status while a turn
    /// is active; `resets_at_epoch` carries the countdown when known.
    RateLimited,
    /// Derived display state only: a state that promises a byte stream
    /// (`stalled_after_seconds` present) went quiet past the threshold.
    /// Producers never store or send it — `ActivityMachine::effective_state`
    /// and the dashboard both derive it from the epochs, so the wire needs
    /// no per-second traffic.
    Stalled,
    /// No turn is active.
    #[default]
    Idle,
}

/// The activity section of [`SessionVitals`]: raw state + epochs on the
/// wire; elapsed/quiet/stall all tick client-side (the cache-countdown
/// pattern — no per-second events).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivityVitals {
    pub state: SessionActivityState,
    /// Unix seconds when `state` was entered (elapsed ticks client-side).
    pub since_epoch: u64,
    /// Unix seconds of the freshest evidence of forward motion: the last
    /// stream byte/notification observed for the current state (a turn
    /// dispatch seeds it). Quantized by the producer so heartbeats don't
    /// emit per-delta.
    pub last_stream_byte_epoch: u64,
    /// Quiet threshold for the stalled degradation, present only when the
    /// current state's evidence includes a live byte stream (so quiet
    /// means trouble). Absent = quiet is normal here and "stalled" must
    /// not be claimed (honest degradation for delta-less backends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_after_seconds: Option<u32>,
    /// Configured reasoning effort, first-hand only: the backend's own
    /// echo when it states one, else the value Intendant itself passed at
    /// launch. Never inferred from output volume or timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Rate-limit reset epoch while `state` is `rate-limited`, when the
    /// wire carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_epoch: Option<u64>,
    /// Short descriptions of the backend-announced background tasks this
    /// session has running (count = length). Non-empty while any are
    /// armed, in any state; the `parked-on-tasks` claim additionally
    /// requires no turn to be running. Only ever filled from the
    /// backend's own task events — absent means "none announced", which
    /// on backends without background primitives is simply always.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<String>,
}

/// Per-session vitals (git / prompt-cache / rate limits / live activity)
/// shown by the dashboard and Station — the port of the operator
/// statusline. Sections are independent: producers fill what they know,
/// frontends hide what is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionVitals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<SessionGitVitals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<SessionCacheVitals>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<SessionLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<SessionActivityVitals>,
}
