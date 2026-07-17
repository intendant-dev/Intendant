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

/// Session configuration facts — model, reasoning effort, permission
/// mode — the vitals `config` section. Wire-first: every value is either
/// the backend's own echo (Claude Code's `system:init`, a Codex
/// thread-settings notification, the native loop's own provider
/// selection) or the session's launch config awaiting one
/// (`permission_echoed` distinguishes, for the datum where the difference
/// is live — Claude Code resolves its mode through a 3-layer chain, so
/// the launch value alone can lie). Absent fields mean "not reported":
/// frontends render the row with a degraded value, never a guess.
///
/// Producers emit *partial* facts at each protocol seam; the vitals hub
/// folds them sticky per field (a known value is never blanked by a later
/// partial emission that omits it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigVitals {
    /// Model identifier in the provider's own vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Configured reasoning effort, first-hand only: the backend's own
    /// echo when it states one, else the value Intendant itself passed at
    /// launch. Never inferred from output volume or timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Raw permission/approval mode in the backend's own vocabulary
    /// (`bypassPermissions`, `workspace-write · on-request`, an autonomy
    /// level) — the power-user truth, always displayed verbatim one tap
    /// away from the plain-language label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Backend-agnostic display class for the mode — one of
    /// [`PERMISSION_DISPLAY_KINDS`], computed once daemon-side by the
    /// per-backend catalog functions ([`claude_permission_kind`],
    /// [`codex_permission_kind`]; native uses [`PERMISSION_KIND_AUTONOMY`]).
    /// Absent for unknown modes: frontends then show the raw mode without
    /// a plain-language claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_kind: Option<String>,
    /// Whether the backend itself vouched for `permission_mode` (an init
    /// echo, an accepted settings request) or the value is launch config
    /// the backend has not yet confirmed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub permission_echoed: bool,
}

/// The backend-agnostic permission display classes — the one catalog
/// (`permission_kind` vocabulary). The dashboard's plain-language copy is
/// keyed by exactly these strings; its symbol-catalog parity test pins
/// them, so an addition here that forgets the frontend fails the suite.
pub const PERMISSION_KIND_BYPASS: &str = "bypass";
pub const PERMISSION_KIND_AUTO_EDITS: &str = "auto-edits";
pub const PERMISSION_KIND_AUTO_SAFE: &str = "auto-safe";
pub const PERMISSION_KIND_AUTO_SANDBOXED: &str = "auto-sandboxed";
pub const PERMISSION_KIND_ASK: &str = "ask";
pub const PERMISSION_KIND_DENY_ASKS: &str = "deny-asks";
pub const PERMISSION_KIND_READ_ONLY: &str = "read-only";
pub const PERMISSION_KIND_PLAN: &str = "plan";
/// Native sessions: the label derives from the Intendant autonomy
/// vocabulary keyed by the raw level in `permission_mode`.
pub const PERMISSION_KIND_AUTONOMY: &str = "autonomy";

pub const PERMISSION_DISPLAY_KINDS: [&str; 9] = [
    PERMISSION_KIND_BYPASS,
    PERMISSION_KIND_AUTO_EDITS,
    PERMISSION_KIND_AUTO_SAFE,
    PERMISSION_KIND_AUTO_SANDBOXED,
    PERMISSION_KIND_ASK,
    PERMISSION_KIND_DENY_ASKS,
    PERMISSION_KIND_READ_ONLY,
    PERMISSION_KIND_PLAN,
    PERMISSION_KIND_AUTONOMY,
];

/// Display class for a Claude Code permission mode. The semantics mirror
/// the CLI's own behavior (and the dashboard's mode-picker help text):
/// `default` asks, `acceptEdits` auto-approves file edits only, `auto`
/// auto-approves classifier-safe calls, `dontAsk` *declines* anything
/// that would ask, `bypassPermissions` never asks at all, `plan` is
/// read-only planning. Unknown modes map to `None` — display the raw
/// name rather than guess.
pub fn claude_permission_kind(mode: &str) -> Option<&'static str> {
    match mode.trim() {
        "bypassPermissions" => Some(PERMISSION_KIND_BYPASS),
        "acceptEdits" => Some(PERMISSION_KIND_AUTO_EDITS),
        "auto" => Some(PERMISSION_KIND_AUTO_SAFE),
        "dontAsk" => Some(PERMISSION_KIND_DENY_ASKS),
        "default" => Some(PERMISSION_KIND_ASK),
        "plan" => Some(PERMISSION_KIND_PLAN),
        _ => None,
    }
}

/// Display class for a Codex approval-policy + sandbox pair. The sandbox
/// leads: `danger-full-access` bypasses approvals and the sandbox
/// entirely (the launch layer forces approval `never` alongside it), and
/// a `read-only` sandbox cannot change anything regardless of policy.
/// Inside `workspace-write`, `never`/`on-failure` act without asking
/// (the full-auto/yolo class), `on-request` works alone in the sandbox
/// and asks beyond it, `untrusted` asks first. Unknown combinations map
/// to `None` — display the raw pair rather than guess.
pub fn codex_permission_kind(approval_policy: &str, sandbox: &str) -> Option<&'static str> {
    match (sandbox.trim(), approval_policy.trim()) {
        ("danger-full-access", _) => Some(PERMISSION_KIND_BYPASS),
        ("read-only", _) => Some(PERMISSION_KIND_READ_ONLY),
        ("workspace-write", "never") | ("workspace-write", "on-failure") => {
            Some(PERMISSION_KIND_BYPASS)
        }
        ("workspace-write", "on-request") => Some(PERMISSION_KIND_AUTO_SANDBOXED),
        ("workspace-write", "untrusted") => Some(PERMISSION_KIND_ASK),
        _ => None,
    }
}

/// Per-session vitals (git / prompt-cache / rate limits / live activity /
/// config facts) shown by the dashboard and Station — the port of the
/// operator statusline. Sections are independent: producers fill what
/// they know, frontends hide what is absent.
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
    /// Session configuration facts (model / effort / permission mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<SessionConfigVitals>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every known mode maps into the pinned kind vocabulary; unknown
    /// modes map to None (raw passthrough, never a guessed label).
    #[test]
    fn claude_permission_kinds_cover_the_mode_vocabulary() {
        assert_eq!(
            claude_permission_kind("bypassPermissions"),
            Some(PERMISSION_KIND_BYPASS)
        );
        assert_eq!(
            claude_permission_kind("acceptEdits"),
            Some(PERMISSION_KIND_AUTO_EDITS)
        );
        assert_eq!(
            claude_permission_kind("auto"),
            Some(PERMISSION_KIND_AUTO_SAFE)
        );
        assert_eq!(
            claude_permission_kind("dontAsk"),
            Some(PERMISSION_KIND_DENY_ASKS)
        );
        assert_eq!(claude_permission_kind("default"), Some(PERMISSION_KIND_ASK));
        assert_eq!(claude_permission_kind(" plan "), Some(PERMISSION_KIND_PLAN));
        assert_eq!(claude_permission_kind("delegate"), None);
        assert_eq!(claude_permission_kind(""), None);
    }

    #[test]
    fn codex_permission_kinds_cover_the_policy_sandbox_grid() {
        // The danger sandbox is bypass regardless of the configured policy
        // (launch forces approval "never" alongside it).
        assert_eq!(
            codex_permission_kind("untrusted", "danger-full-access"),
            Some(PERMISSION_KIND_BYPASS)
        );
        assert_eq!(
            codex_permission_kind("never", "danger-full-access"),
            Some(PERMISSION_KIND_BYPASS)
        );
        // A read-only sandbox cannot change anything, whatever the policy.
        assert_eq!(
            codex_permission_kind("never", "read-only"),
            Some(PERMISSION_KIND_READ_ONLY)
        );
        assert_eq!(
            codex_permission_kind("on-request", "read-only"),
            Some(PERMISSION_KIND_READ_ONLY)
        );
        // workspace-write: the approval policy decides.
        assert_eq!(
            codex_permission_kind("never", "workspace-write"),
            Some(PERMISSION_KIND_BYPASS)
        );
        assert_eq!(
            codex_permission_kind("on-failure", "workspace-write"),
            Some(PERMISSION_KIND_BYPASS)
        );
        assert_eq!(
            codex_permission_kind("on-request", "workspace-write"),
            Some(PERMISSION_KIND_AUTO_SANDBOXED)
        );
        assert_eq!(
            codex_permission_kind("untrusted", "workspace-write"),
            Some(PERMISSION_KIND_ASK)
        );
        // Unknown vocabulary: raw passthrough, no plain-language claim.
        assert_eq!(codex_permission_kind("sometimes", "workspace-write"), None);
        assert_eq!(codex_permission_kind("never", "chroot"), None);
    }

    /// Every kind the mapping functions can return is in the pinned
    /// vocabulary (the dashboard copy catalog is keyed by these).
    #[test]
    fn permission_kind_outputs_stay_in_the_pinned_vocabulary() {
        let claude_modes = [
            "bypassPermissions",
            "acceptEdits",
            "auto",
            "dontAsk",
            "default",
            "plan",
        ];
        for mode in claude_modes {
            let kind = claude_permission_kind(mode).expect("known mode maps");
            assert!(
                PERMISSION_DISPLAY_KINDS.contains(&kind),
                "claude {mode} mapped outside the pinned vocabulary: {kind}"
            );
        }
        for approval in ["untrusted", "on-request", "on-failure", "never"] {
            for sandbox in ["read-only", "workspace-write", "danger-full-access"] {
                if let Some(kind) = codex_permission_kind(approval, sandbox) {
                    assert!(
                        PERMISSION_DISPLAY_KINDS.contains(&kind),
                        "codex {approval}/{sandbox} mapped outside the pinned vocabulary: {kind}"
                    );
                }
            }
        }
        assert!(PERMISSION_DISPLAY_KINDS.contains(&PERMISSION_KIND_AUTONOMY));
    }

    /// The config section's wire shape: camelCase fields, absent when
    /// unknown, `permissionEchoed` serialized only when true.
    #[test]
    fn config_vitals_wire_shape() {
        let full = SessionConfigVitals {
            model: Some("claude-fable-5".into()),
            effort: Some("max".into()),
            permission_mode: Some("bypassPermissions".into()),
            permission_kind: Some(PERMISSION_KIND_BYPASS.into()),
            permission_echoed: true,
        };
        let wire = serde_json::to_value(&full).expect("serializes");
        assert_eq!(wire["model"], "claude-fable-5");
        assert_eq!(wire["effort"], "max");
        assert_eq!(wire["permissionMode"], "bypassPermissions");
        assert_eq!(wire["permissionKind"], "bypass");
        assert_eq!(wire["permissionEchoed"], true);

        let sparse = SessionConfigVitals::default();
        let wire = serde_json::to_value(&sparse).expect("serializes");
        assert_eq!(
            wire,
            serde_json::json!({}),
            "absent facts serialize to nothing"
        );
    }
}
