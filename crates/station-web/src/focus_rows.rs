//! Agent focus-panel content, derived once for both surfaces (derive,
//! don't mirror): the screen panel (`hud::focus::draw_agent_focus`) and
//! the world pane (`scene::add_agent_focus_pane`) render the same rows
//! and pills from this module, so the two presentations cannot drift.
//! Everything here is pure string/color derivation from a
//! `StationAgent` — no canvas, no GPU, no clock reads (the caller passes
//! the epoch so the cache countdown stays testable).

use crate::input::HitAction;
use crate::model::StationAgent;
use crate::util::{
    fmt_compact, fmt_countdown, goal_status_color_css, nonempty, pct_label, percent,
    phase_color_css, pressure_color, truncate, C_AMBER_CSS, C_GREEN_CSS, C_IRIS2_CSS, C_IRIS_CSS,
    C_ROSE_CSS, C_SKY_CSS, C_TEXT2_CSS, C_VIOLET_CSS,
};

/// One labeled row: label column, value text, label color.
pub(crate) struct AgentFocusRow {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) color_css: &'static str,
    /// Budget fraction (0–1, `util::percent` units) rendered as a meter
    /// bar under the row.
    pub(crate) meter: Option<f32>,
}

/// One action pill: label, accent color, and the click action (the
/// existing `HitAction` vocabulary — no new dispatch shapes).
pub(crate) struct AgentFocusPill {
    pub(crate) label: &'static str,
    pub(crate) color_css: &'static str,
    pub(crate) action: HitAction,
}

/// An actionable pending approval: the command row plus the ids the
/// approve/deny pills dispatch with.
pub(crate) struct AgentApproval {
    pub(crate) row: AgentFocusRow,
    pub(crate) host_id: String,
    pub(crate) approval_id: String,
}

pub(crate) struct AgentFocusContent {
    pub(crate) subtitle: String,
    pub(crate) rows: Vec<AgentFocusRow>,
    /// Session action pills (log / resume / target / steer / stop /
    /// compact / fork per advertised ops); empty for non-session nodes.
    /// The steer-composer pill is per-surface chrome, not listed here.
    pub(crate) pills: Vec<AgentFocusPill>,
    /// Present when the pending approval is actionable from this UI.
    pub(crate) approval: Option<AgentApproval>,
}

/// Derive the full focus-panel content for an agent node.
/// `local_host_id` is the snapshot's first host (approvals on it are
/// actionable even without an explicit approval id); `now_epoch` feeds
/// the cache-TTL countdown.
pub(crate) fn agent_focus_content(
    agent: &StationAgent,
    local_host_id: Option<&str>,
    now_epoch: f64,
) -> AgentFocusContent {
    let is_session = !agent.session_id.trim().is_empty();
    let subtitle = if is_session && agent.recent {
        format!("recent {} session", nonempty(&agent.source, "intendant"))
    } else if is_session {
        format!("{} session", nonempty(&agent.source, "intendant"))
    } else {
        format!("{} agent", nonempty(&agent.role, "agent"))
    };

    let mut rows = Vec::new();
    rows.push(AgentFocusRow {
        label: "source",
        value: format!(
            "{} / {}",
            nonempty(&agent.provider, "provider"),
            nonempty(&agent.model, "model")
        ),
        color_css: C_IRIS_CSS,
        meter: None,
    });
    rows.push(AgentFocusRow {
        label: "phase",
        value: format!(
            "{} / {}{}",
            nonempty(&agent.phase, "idle"),
            nonempty(&agent.status, "idle"),
            if agent.autonomy.trim().is_empty() {
                String::new()
            } else {
                format!(" / {} autonomy", agent.autonomy.trim())
            }
        ),
        color_css: phase_color_css(&agent.phase),
        meter: None,
    });
    rows.push(AgentFocusRow {
        label: "task",
        value: nonempty(&agent.task, "idle"),
        color_css: C_SKY_CSS,
        meter: None,
    });
    if !agent.relationship_kind.trim().is_empty() {
        let parent = agent
            .parent_id
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| p.strip_prefix("session-").unwrap_or(p))
            .map(|p| truncate(p, 12))
            .unwrap_or_default();
        let lineage = if parent.is_empty() {
            agent.relationship_kind.trim().to_string()
        } else {
            format!("{} of {}", agent.relationship_kind.trim(), parent)
        };
        rows.push(AgentFocusRow {
            label: "lineage",
            value: lineage,
            color_css: C_VIOLET_CSS,
            meter: None,
        });
    }
    if !agent.goal_objective.trim().is_empty() || !agent.goal_status.trim().is_empty() {
        let status = agent.goal_status.trim();
        let mut goal_text = if status.is_empty() {
            agent.goal_objective.trim().to_string()
        } else if agent.goal_objective.trim().is_empty() {
            status.to_string()
        } else {
            format!("{}: {}", status, agent.goal_objective.trim())
        };
        if !agent.goal_tokens.trim().is_empty() {
            goal_text.push_str(&format!(" ({} tok)", agent.goal_tokens.trim()));
        }
        rows.push(AgentFocusRow {
            label: "goal",
            value: goal_text,
            color_css: goal_status_color_css(status),
            meter: None,
        });
    }
    let budget_pct = percent(agent.tokens, agent.token_cap);
    rows.push(AgentFocusRow {
        label: "tokens",
        value: format!(
            "{} / {} ({})",
            fmt_compact(agent.tokens),
            fmt_compact(agent.token_cap),
            pct_label(budget_pct)
        ),
        color_css: pressure_color(budget_pct),
        meter: Some(budget_pct),
    });
    let mut usage = format!(
        "p {} / c {} / cached {}",
        fmt_compact(agent.prompt),
        fmt_compact(agent.completion),
        fmt_compact(agent.cached)
    );
    if agent.cost > 0.0 {
        usage.push_str(&format!(" / ${:.2}", agent.cost));
    }
    if agent.turn_cap > 0 {
        usage.push_str(&format!(" / turn {}/{}", agent.turns, agent.turn_cap));
    } else if agent.turns > 0 {
        usage.push_str(&format!(" / turn {}", agent.turns));
    }
    rows.push(AgentFocusRow {
        label: "usage",
        value: usage,
        color_css: C_IRIS2_CSS,
        meter: None,
    });
    if !agent.vitals_git.trim().is_empty() {
        rows.push(AgentFocusRow {
            label: "git",
            value: agent.vitals_git.trim().to_string(),
            color_css: if agent.vitals_git_conflict {
                C_ROSE_CSS
            } else {
                C_SKY_CSS
            },
            meter: None,
        });
    }
    if agent.cache_hit_pct >= 0.0 || agent.cache_ttl_seconds > 0.0 {
        // Same tiers as the dashboard chip (fragment 39, tones ok/warn/
        // crit): hit green ≥90 / amber ≥50 / rose below — the old peach
        // bottom tier collapsed into amber under the Iris palette, so the
        // crit tier moves to rose, which is also what the chip's crit tone
        // renders. Countdown rose in its final minute, cold dimmed.
        let mut text = String::new();
        let mut color = C_TEXT2_CSS;
        if agent.cache_hit_pct >= 0.0 {
            let hit = agent.cache_hit_pct.clamp(0.0, 100.0);
            text.push_str(&format!("⚡{}%", hit.round() as u32));
            color = if hit >= 90.0 {
                C_GREEN_CSS
            } else if hit >= 50.0 {
                C_AMBER_CSS
            } else {
                C_ROSE_CSS
            };
        }
        if agent.cache_ttl_seconds > 0.0 && agent.cache_last_activity_epoch > 0.0 && now_epoch > 0.0
        {
            let remaining = agent.cache_ttl_seconds - (now_epoch - agent.cache_last_activity_epoch);
            if !text.is_empty() {
                text.push(' ');
            }
            if remaining > 0.0 {
                text.push_str(&format!("⏳{}", fmt_countdown(remaining)));
                if remaining <= 60.0 {
                    color = C_ROSE_CSS;
                }
            } else {
                text.push_str("✗ cold");
                color = C_TEXT2_CSS;
            }
        }
        if !text.is_empty() {
            rows.push(AgentFocusRow {
                label: "cache",
                value: text,
                color_css: color,
                meter: None,
            });
        }
    }
    if !agent.vitals_limits.trim().is_empty() {
        rows.push(AgentFocusRow {
            label: "limits",
            value: agent.vitals_limits.trim().to_string(),
            color_css: match agent.vitals_limits_state.trim() {
                "crit" => C_ROSE_CSS,
                "warn" => C_AMBER_CSS,
                _ => C_TEXT2_CSS,
            },
            meter: None,
        });
    }
    if !agent.worktree.trim().is_empty() {
        rows.push(AgentFocusRow {
            label: "worktree",
            value: agent.worktree.trim().to_string(),
            color_css: C_VIOLET_CSS,
            meter: None,
        });
    }

    // Per-node action pills at session-window-kebab parity: the universal
    // basics plus whatever thread-action ops the session advertises. Every
    // pill dispatches through the dashboard's real session-action handler.
    let mut pills = Vec::new();
    if is_session {
        let sid = agent.session_id.trim().to_string();
        let pill = |label: &'static str, color_css: &'static str, action: &str| AgentFocusPill {
            label,
            color_css,
            action: HitAction::SessionAction {
                action: action.to_string(),
                id: sid.clone(),
            },
        };
        pills.push(pill("log", C_IRIS_CSS, "station-log"));
        if agent.recent {
            // A closed session can be read or brought back — nothing else
            // applies to it.
            pills.push(pill("resume", C_GREEN_CSS, "resume"));
        } else {
            pills.push(pill("target", C_SKY_CSS, "target"));
            pills.push(pill("steer", C_IRIS2_CSS, "steer"));
            if agent.can_interrupt {
                pills.push(pill("stop", C_ROSE_CSS, "interrupt"));
            }
            if agent.thread_actions.iter().any(|op| op == "compact") {
                pills.push(pill("compact", C_VIOLET_CSS, "thread-compact"));
            }
            if agent.thread_actions.iter().any(|op| op == "fork") {
                pills.push(pill("fork", C_AMBER_CSS, "thread-fork"));
            }
        }
    }

    // An approval is actionable when it is local (or on the snapshot's
    // own host), or when the feed carried an explicit approval id.
    let actionable = agent.needs_approval
        && (agent.host_id == "local"
            || local_host_id == Some(agent.host_id.as_str())
            || agent
                .approval_id
                .as_deref()
                .is_some_and(|id| !id.is_empty()));
    let approval = actionable.then(|| AgentApproval {
        row: AgentFocusRow {
            label: "approval",
            value: format!(
                "{}{}",
                nonempty(&agent.approval_command, "approval required"),
                if agent.approval_category.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({})", agent.approval_category.trim())
                }
            ),
            color_css: C_AMBER_CSS,
            meter: None,
        },
        host_id: agent.host_id.clone(),
        approval_id: agent.approval_id.clone().unwrap_or_default(),
    });

    AgentFocusContent {
        subtitle,
        rows,
        pills,
        approval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_agent() -> StationAgent {
        StationAgent {
            id: "worker-1".into(),
            provider: "anthropic".into(),
            model: "claude".into(),
            tokens: 50_000.0,
            token_cap: 100_000.0,
            prompt: 40_000.0,
            completion: 10_000.0,
            cached: 30_000.0,
            ..StationAgent::default()
        }
    }

    fn labels(content: &AgentFocusContent) -> Vec<&'static str> {
        content.rows.iter().map(|r| r.label).collect()
    }

    #[test]
    fn minimal_agent_has_five_rows_and_no_pills() {
        let content = agent_focus_content(&base_agent(), None, 0.0);
        assert_eq!(
            labels(&content),
            vec!["source", "phase", "task", "tokens", "usage"]
        );
        assert_eq!(content.subtitle, "direct agent");
        assert!(content.pills.is_empty());
        assert!(content.approval.is_none());
        assert_eq!(content.rows[0].value, "anthropic / claude");
        // Only the tokens row carries the budget meter.
        let meters: Vec<bool> = content.rows.iter().map(|r| r.meter.is_some()).collect();
        assert_eq!(meters, vec![false, false, false, true, false]);
        assert_eq!(content.rows[3].meter, Some(0.5));
    }

    #[test]
    fn conditional_rows_appear_in_panel_order() {
        let mut agent = base_agent();
        agent.worktree = "wt/fix".into();
        agent.relationship_kind = "subagent".into();
        agent.parent_id = Some("session-abcdef".into());
        agent.goal_status = "active".into();
        agent.goal_objective = "land slice 5".into();
        agent.goal_tokens = "12k".into();
        agent.vitals_git = "main +2 ~1".into();
        agent.cache_hit_pct = 95.0;
        agent.vitals_limits = "▮49% 7d".into();
        agent.vitals_limits_state = "warn".into();
        let content = agent_focus_content(&agent, None, 0.0);
        assert_eq!(
            labels(&content),
            vec![
                "source", "phase", "task", "lineage", "goal", "tokens", "usage", "git", "cache",
                "limits", "worktree"
            ]
        );
        let lineage = &content.rows[3];
        assert_eq!(lineage.value, "subagent of abcdef");
        let goal = &content.rows[4];
        assert_eq!(goal.value, "active: land slice 5 (12k tok)");
        let cache = &content.rows[8];
        assert_eq!(cache.value, "⚡95%");
        assert_eq!(cache.color_css, C_GREEN_CSS);
        let limits = &content.rows[9];
        assert_eq!(limits.color_css, C_AMBER_CSS);
    }

    #[test]
    fn cache_countdown_uses_caller_epoch_and_reds_final_minute() {
        let mut agent = base_agent();
        agent.cache_hit_pct = 60.0;
        agent.cache_ttl_seconds = 300.0;
        agent.cache_last_activity_epoch = 1_000.0;
        let content = agent_focus_content(&agent, None, 1_260.0);
        let cache = content.rows.iter().find(|r| r.label == "cache").unwrap();
        assert!(cache.value.starts_with("⚡60%"), "{}", cache.value);
        assert!(cache.value.contains('⏳'), "{}", cache.value);
        assert_eq!(cache.color_css, C_ROSE_CSS);
        // Expired TTL reads cold and dims.
        let cold = agent_focus_content(&agent, None, 2_000.0);
        let row = cold.rows.iter().find(|r| r.label == "cache").unwrap();
        assert!(row.value.ends_with("✗ cold"), "{}", row.value);
        assert_eq!(row.color_css, C_TEXT2_CSS);
    }

    #[test]
    fn session_pills_follow_state_and_advertised_ops() {
        let mut agent = base_agent();
        agent.session_id = "sess-9".into();
        agent.source = "codex".into();
        agent.can_interrupt = true;
        agent.thread_actions = vec!["compact".into(), "fork".into()];
        let content = agent_focus_content(&agent, None, 0.0);
        assert_eq!(content.subtitle, "codex session");
        let labels: Vec<&str> = content.pills.iter().map(|p| p.label).collect();
        assert_eq!(
            labels,
            vec!["log", "target", "steer", "stop", "compact", "fork"]
        );
        for pill in &content.pills {
            match &pill.action {
                HitAction::SessionAction { id, .. } => assert_eq!(id, "sess-9"),
                _ => panic!("session pill must dispatch a SessionAction"),
            }
        }
        agent.recent = true;
        let recent = agent_focus_content(&agent, None, 0.0);
        assert_eq!(recent.subtitle, "recent codex session");
        let labels: Vec<&str> = recent.pills.iter().map(|p| p.label).collect();
        assert_eq!(labels, vec!["log", "resume"]);
    }

    #[test]
    fn approval_requires_local_host_or_explicit_id() {
        let mut agent = base_agent();
        agent.needs_approval = true;
        agent.approval_command = "rm -rf build".into();
        agent.approval_category = "shell".into();
        let local = agent_focus_content(&agent, None, 0.0);
        let appr = local.approval.expect("local host approval is actionable");
        assert_eq!(appr.row.value, "rm -rf build (shell)");
        assert_eq!(appr.host_id, "local");
        assert_eq!(appr.approval_id, "");

        agent.host_id = "peer-a".into();
        let foreign = agent_focus_content(&agent, None, 0.0);
        assert!(foreign.approval.is_none(), "foreign host without id");
        let own_host = agent_focus_content(&agent, Some("peer-a"), 0.0);
        assert!(own_host.approval.is_some(), "snapshot's own host");
        agent.approval_id = Some("ap-1".into());
        let with_id = agent_focus_content(&agent, None, 0.0);
        assert_eq!(with_id.approval.unwrap().approval_id, "ap-1");
    }
}
