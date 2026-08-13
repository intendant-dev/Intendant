//! Serde model for the dashboard snapshot feed — the subset the XR
//! surface consumes.
//!
//! Field names mirror the wire schema `static/app.html` already produces
//! for the rendered surfaces (`buildStationSnapshot()`, camelCase); serde
//! ignores the fields XR doesn't read. This is the "port the rails"
//! contract: the XR surface adds NO feed of its own — when the dashboard
//! learns something new, XR inherits it by consuming the same snapshot.

use serde::Deserialize;

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct XrSnapshot {
    pub(crate) hosts: Vec<XrHost>,
    pub(crate) agents: Vec<XrAgent>,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct XrHost {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    pub(crate) connected: bool,
}

impl Default for XrHost {
    fn default() -> Self {
        Self {
            id: "local".into(),
            name: "local".into(),
            platform: "unknown".into(),
            connected: true,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct XrAgent {
    pub(crate) id: String,
    pub(crate) host_id: String,
    pub(crate) role: String,
    pub(crate) phase: String,
    pub(crate) status: String,
    pub(crate) task: String,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) tokens: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) token_cap: f32,
    pub(crate) needs_approval: bool,
    pub(crate) approval_id: Option<String>,
    pub(crate) approval_command: String,
    pub(crate) approval_category: String,
    /// Live session this card projects (empty for synthetic nodes like
    /// the primary agent or peer daemons).
    pub(crate) session_id: String,
    /// Backend source for session cards ("codex", "claude-code", …).
    pub(crate) source: String,
    pub(crate) goal_status: String,
    pub(crate) goal_objective: String,
    /// Advertised thread-action ops; gates the focus panel's pills.
    pub(crate) thread_actions: Vec<String>,
    pub(crate) can_interrupt: bool,
    /// Recent (closed-window) session: dim, inert card.
    pub(crate) recent: bool,
    /// Session vitals (`stationVitalsFields`): git and limits arrive
    /// pre-formatted by the dashboard's chip formatters (single source
    /// of truth for the text), the cache hit as a raw percentage.
    /// Absent on synthetic and recent cards — defaults mean "no data".
    pub(crate) vitals_git: String,
    /// True when merging this card's branch would conflict (the git
    /// parity chip's crit state).
    pub(crate) vitals_git_conflict: bool,
    /// Latest request's cache-hit percentage; negative = no reading
    /// (the feed's own sentinel).
    #[serde(deserialize_with = "f64_or_unset")]
    pub(crate) cache_hit_pct: f64,
    /// Top rate-limit gauge text ("▮49% 7d · ↻6:12:03") and its
    /// severity ("", "warn", "crit").
    pub(crate) vitals_limits: String,
    pub(crate) vitals_limits_state: String,
}

impl Default for XrAgent {
    fn default() -> Self {
        Self {
            id: "agent".into(),
            host_id: "local".into(),
            role: "direct".into(),
            phase: "idle".into(),
            status: "idle".into(),
            task: String::new(),
            tokens: 0.0,
            token_cap: 200_000.0,
            needs_approval: false,
            approval_id: None,
            approval_command: String::new(),
            approval_category: String::new(),
            session_id: String::new(),
            source: String::new(),
            goal_status: String::new(),
            goal_objective: String::new(),
            thread_actions: Vec::new(),
            can_interrupt: false,
            recent: false,
            vitals_git: String::new(),
            vitals_git_conflict: false,
            cache_hit_pct: -1.0,
            vitals_limits: String::new(),
            vitals_limits_state: String::new(),
        }
    }
}

/// The feed occasionally carries numeric fields as strings or null;
/// fold every non-numeric shape to the default instead of failing the
/// whole snapshot (same tolerance the other rendered surface applies).
fn f32_or_default<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Loose {
        Num(f64),
        Str(String),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Loose::deserialize(deserializer) {
        Ok(Loose::Num(n)) if n.is_finite() => n as f32,
        Ok(Loose::Str(s)) => s.trim().parse::<f32>().unwrap_or(0.0),
        _ => 0.0,
    })
}

/// Same tolerance for the cache-hit percentage, folding to the feed's
/// own "no reading" sentinel (-1) instead of a fake 0% hit.
fn f64_or_unset<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Loose {
        Num(f64),
        Str(String),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Loose::deserialize(deserializer) {
        Ok(Loose::Num(n)) if n.is_finite() => n,
        Ok(Loose::Str(s)) => s.trim().parse::<f64>().unwrap_or(-1.0),
        _ => -1.0,
    })
}

impl XrAgent {
    /// Context pressure in [0, 1]; 0 when the cap is unknown.
    pub(crate) fn context_pressure(&self) -> f32 {
        if self.token_cap > 0.0 {
            (self.tokens / self.token_cap).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// The card's display label: the session's short id when it projects
    /// a live session, otherwise the agent id/role.
    pub(crate) fn label(&self) -> String {
        if !self.session_id.is_empty() {
            let sid = self.session_id.as_str();
            let short: String = sid.chars().take(10).collect();
            if !self.source.is_empty() {
                format!("{} {}", self.source, short)
            } else {
                short
            }
        } else {
            self.id.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_parses_the_dashboard_shape_and_ignores_extras() {
        let json = serde_json::json!({
            "hosts": [
                {"id": "local", "name": "macbook", "platform": "macos",
                 "connected": true, "cpu": 12.5, "mem": null}
            ],
            "agents": [
                {"id": "session-abc", "hostId": "local", "role": "worker",
                 "phase": "running", "status": "running",
                 "task": "port the dashboard", "tokens": "1500",
                 "tokenCap": 200000, "needsApproval": true,
                 "approvalId": "ap-1", "approvalCommand": "rm -rf build",
                 "approvalCategory": "shell", "sessionId": "abc123def456",
                 "source": "claude-code", "goalStatus": "on-track",
                 "goalObjective": "ship it", "threadActions": ["compact"],
                 "canInterrupt": true, "recent": false,
                 "someFutureField": {"nested": true}}
            ],
            "events": [{"id": "e1"}],
            "controls": {"whatever": 1}
        });
        let snap: XrSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.hosts.len(), 1);
        assert_eq!(snap.hosts[0].name, "macbook");
        assert_eq!(snap.agents.len(), 1);
        let a = &snap.agents[0];
        assert_eq!(a.host_id, "local");
        assert_eq!(a.tokens, 1500.0);
        assert!(a.needs_approval);
        assert_eq!(a.approval_id.as_deref(), Some("ap-1"));
        assert_eq!(a.source, "claude-code");
        assert!((a.context_pressure() - 0.0075).abs() < 1e-6);
        assert_eq!(a.label(), "claude-code abc123def4");
    }

    #[test]
    fn empty_snapshot_defaults_cleanly() {
        let snap: XrSnapshot = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(snap.hosts.is_empty());
        assert!(snap.agents.is_empty());
    }

    #[test]
    fn vitals_fields_parse_the_station_shape() {
        let json = serde_json::json!({
            "agents": [
                {"id": "a1", "hostId": "local",
                 "vitalsGit": "⎇ main ●3 +2/−1 ⚠", "vitalsGitConflict": true,
                 "cacheHitPct": "62", "cacheLastActivityEpoch": 1754000000,
                 "cacheTtlSeconds": 300,
                 "vitalsLimits": "▮95% 7d · ↻6:12:03", "vitalsLimitsState": "crit"}
            ]
        });
        let snap: XrSnapshot = serde_json::from_value(json).unwrap();
        let a = &snap.agents[0];
        assert_eq!(a.vitals_git, "⎇ main ●3 +2/−1 ⚠");
        assert!(a.vitals_git_conflict);
        assert_eq!(a.cache_hit_pct, 62.0);
        assert_eq!(a.vitals_limits, "▮95% 7d · ↻6:12:03");
        assert_eq!(a.vitals_limits_state, "crit");
    }

    #[test]
    fn vitals_fields_default_to_no_data_on_old_feeds() {
        let json = serde_json::json!({
            "agents": [{"id": "a1", "hostId": "local", "status": "running"}]
        });
        let snap: XrSnapshot = serde_json::from_value(json).unwrap();
        let a = &snap.agents[0];
        assert!(a.vitals_git.is_empty());
        assert!(!a.vitals_git_conflict);
        assert_eq!(a.cache_hit_pct, -1.0);
        assert!(a.vitals_limits.is_empty());
        assert!(a.vitals_limits_state.is_empty());
        // A garbage cache reading folds to the unset sentinel, never 0%.
        let json = serde_json::json!({
            "agents": [{"id": "a2", "hostId": "local", "cacheHitPct": null}]
        });
        let snap: XrSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.agents[0].cache_hit_pct, -1.0);
    }
}
