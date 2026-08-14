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
    /// The dashboard's coalesced activity feed (session-window history +
    /// live log events, ≤80). The workbench transcript filters these by
    /// the focused agent — no XR-only feed exists.
    pub(crate) events: Vec<XrEvent>,
    /// Daemon Agenda summary the `ui2-xr.js` pump attaches beside the
    /// Station snapshot (`xrAgendaSummary()`). Absent/null on feeds that
    /// don't carry it — the rail simply doesn't render.
    pub(crate) agenda: Option<XrAgenda>,
}

/// One activity-feed line (the dashboard's bounded event shape).
#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct XrEvent {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    pub(crate) source: String,
    pub(crate) level: String,
    pub(crate) ts: String,
    pub(crate) msg: String,
}

impl XrEvent {
    /// Whether this line belongs to the given agent's thread: session id
    /// when the card projects a live session, agent id otherwise (the
    /// primary agent and peer nodes have no session of their own).
    pub(crate) fn belongs_to(&self, agent: &XrAgent) -> bool {
        if !agent.session_id.is_empty() {
            self.session_id == agent.session_id
        } else {
            self.session_id.is_empty() && self.agent_id == agent.id
        }
    }
}

/// The Agenda rail's feed: a capped list of open items plus the total
/// open count (for the honest overflow line), or an error string when
/// the dashboard could not reach the agenda at all. Composed by the JS
/// seam from the Agenda tab's own client state (or its bounded
/// `api_agenda_list` fallback) — the rail adds no fetch path of its own.
#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct XrAgenda {
    /// Non-empty → the agenda is unavailable; rendered as in-scene text,
    /// never a silent blank.
    pub(crate) error: String,
    /// Total open items on the daemon (the rail shows at most a few).
    pub(crate) open: u32,
    pub(crate) items: Vec<XrAgendaItem>,
    /// Live feedback for the last agenda op fired from XR (complete /
    /// reopen). Owned by the JS seam — it runs the `api_agenda_op`
    /// request and rides the result here so the rail can render honest
    /// per-item status instead of a silent maybe.
    pub(crate) op_status: Option<XrAgendaOpStatus>,
}

/// One in-flight or settled agenda op: `state` is "pending" / "ok" /
/// "error", `detail` carries the refusal text when the daemon said no.
#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct XrAgendaOpStatus {
    pub(crate) id: String,
    pub(crate) op: String,
    pub(crate) state: String,
    pub(crate) detail: String,
}

/// One agenda item, pre-digested for the medium: the title (JS caps it),
/// the kind word (task / note / question), a preformatted due label
/// ("due in 2h" / "overdue 3h" — relative time needs the browser clock,
/// so it's minted where the flat tab's formatter lives), and the served
/// state flags the flat tab's chips render.
#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct XrAgendaItem {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) due: String,
    pub(crate) overdue: bool,
    pub(crate) blocked: bool,
    pub(crate) answered: bool,
    /// A recently completed item the JS seam keeps on the rail briefly
    /// so the reopen (undo) pinch has a target; done cards render muted
    /// at the rail's bottom.
    pub(crate) done: bool,
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
            "events": [
                {"id": "e1", "sessionId": "abc123def456", "agentId": "session-abc",
                 "source": "agent_output", "level": "info", "ts": "12:01",
                 "msg": "wired the encoder pool", "action": "log"},
                {"id": "e2"}
            ],
            "controls": {"whatever": 1}
        });
        let snap: XrSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.hosts.len(), 1);
        assert_eq!(snap.hosts[0].name, "macbook");
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.events.len(), 2, "unknown event fields tolerated");
        assert_eq!(snap.events[0].msg, "wired the encoder pool");
        assert!(snap.events[0].belongs_to(&snap.agents[0]));
        assert!(!snap.events[1].belongs_to(&snap.agents[0]));
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
        assert!(snap.agenda.is_none(), "absent agenda reads as no rail");
    }

    #[test]
    fn agenda_parses_with_tolerant_defaults() {
        let snap: XrSnapshot = serde_json::from_value(serde_json::json!({
            "agenda": {
                "open": 5,
                "items": [
                    {"id": "01H", "title": "fix the boiler", "kind": "task",
                     "due": "overdue 3h", "overdue": true, "blocked": true,
                     "answered": false, "someFutureField": 1},
                    // Sparse row: every missing field folds to a default.
                    {"title": "which color?", "kind": "question"}
                ]
            }
        }))
        .unwrap();
        let agenda = snap.agenda.expect("agenda present");
        assert_eq!(agenda.open, 5);
        assert!(agenda.error.is_empty());
        assert_eq!(agenda.items.len(), 2);
        let first = &agenda.items[0];
        assert_eq!(first.due, "overdue 3h");
        assert!(first.overdue && first.blocked && !first.answered);
        let sparse = &agenda.items[1];
        assert_eq!(sparse.kind, "question");
        assert!(sparse.id.is_empty() && sparse.due.is_empty());
        assert!(!sparse.overdue && !sparse.blocked && !sparse.answered);
    }

    #[test]
    fn agenda_done_and_op_status_parse_with_defaults() {
        let snap: XrSnapshot = serde_json::from_value(serde_json::json!({
            "agenda": {
                "open": 1,
                "items": [
                    {"id": "01A", "title": "shipped", "kind": "task", "done": true},
                    {"id": "01B", "title": "live", "kind": "task"}
                ],
                "opStatus": {"id": "01A", "op": "complete", "state": "ok", "detail": ""}
            }
        }))
        .unwrap();
        let agenda = snap.agenda.expect("agenda present");
        assert!(agenda.items[0].done);
        assert!(!agenda.items[1].done, "done defaults false");
        let status = agenda.op_status.expect("op status parsed");
        assert_eq!(
            (status.id.as_str(), status.op.as_str()),
            ("01A", "complete")
        );
        assert_eq!(status.state, "ok");

        // Absent opStatus reads as none — old feeds unchanged.
        let snap: XrSnapshot =
            serde_json::from_value(serde_json::json!({"agenda": {"open": 0, "items": []}}))
                .unwrap();
        assert!(snap.agenda.unwrap().op_status.is_none());
    }

    #[test]
    fn agenda_null_and_error_shapes_parse() {
        let snap: XrSnapshot = serde_json::from_value(serde_json::json!({"agenda": null})).unwrap();
        assert!(snap.agenda.is_none(), "null agenda reads as no rail");

        let snap: XrSnapshot = serde_json::from_value(serde_json::json!({
            "agenda": {"error": "agenda unavailable (503)"}
        }))
        .unwrap();
        let agenda = snap.agenda.expect("error shape still parses");
        assert_eq!(agenda.error, "agenda unavailable (503)");
        assert!(agenda.items.is_empty());
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
