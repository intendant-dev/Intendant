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

/// Per-session vitals (git / prompt-cache / rate limits) shown by the
/// dashboard and Station — the port of the operator statusline. Sections
/// are independent: producers fill what they know, frontends hide what is
/// absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionVitals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<SessionGitVitals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<SessionCacheVitals>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<SessionLimitWindow>,
}
