//! Scene builder: dashboard snapshot → spatial batches.
//!
//! The careful port happens here. Cards carry exactly what the regular
//! dashboard's session windows carry (status, goal, context pressure,
//! approval state), re-typeset for the medium: a mid-field fleet shelf
//! grouped by host, a near-field workbench for the focused session, and
//! an approval banner with real gravity. Pure and host-testable — text
//! width comes through the `TextMeasure` trait, pixels stay in `gl.rs`.

use crate::atlas::TextMeasure;
use crate::kit::{
    self, front_panel_basis, rgba, shelf_slot, status_color, HitKind, HitTarget, MonitorInstance,
    PanelInstance, SceneBatches, TextAlign, TextRun,
};
use crate::math::{v3, Panel, Vec3};
use crate::model::{XrAgent, XrSnapshot};

/// Max cards per host row before the overflow marker (≈ ±42° arc).
const ROW_CAP: usize = 7;

/// Layer lifts off a panel plane (meters) so co-planar content never
/// z-fights: panel < meter/pill < text.
const LIFT_DECOR: f32 = 0.0018;
const LIFT_TEXT: f32 = 0.0036;

fn dim(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * f]
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_scene(
    snap: &XrSnapshot,
    displays: &[(String, String)],
    selected_id: Option<&str>,
    hover_id: Option<&str>,
    confirm: Option<(&str, f32)>,
    passthrough: bool,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    out.clear();
    backdrop(passthrough, floor_y, out);
    // Screens are independent of the agents feed — they show even while
    // the snapshot warms up.
    monitors(displays, floor_y, out);

    // Hosts: connected first, local pinned to the top row. (The early
    // return below keeps monitors/backdrop — only shelf content needs
    // the feed.)
    let mut hosts: Vec<_> = snap.hosts.iter().collect();
    hosts.sort_by_key(|h| (!h.connected, h.id != "local", h.name.clone()));
    if hosts.is_empty() {
        // Feed not warm yet: keep the space honest, not empty.
        out.texts.push(TextRun {
            origin: v3(0.0, kit::WORKBENCH_Y + floor_y, -kit::WORKBENCH_DIST),
            right: v3(1.0, 0.0, 0.0),
            up: v3(0.0, 1.0, 0.0),
            height: 0.03,
            color: kit::TEXT_2,
            align: TextAlign::Center,
            max_width: 0.0,
            text: "waiting for the dashboard feed…".into(),
        });
        return;
    }

    let mut pending_approvals: Vec<&XrAgent> = Vec::new();

    for (row, host) in hosts.iter().enumerate() {
        let mut agents: Vec<_> = snap
            .agents
            .iter()
            .filter(|a| a.host_id == host.id)
            .collect();
        agents.sort_by_key(|a| (!a.needs_approval, a.recent, a.id.clone()));
        pending_approvals.extend(agents.iter().filter(|a| a.needs_approval));

        let shown = agents.len().min(ROW_CAP);
        // Host label rides above the row's left-most card slot.
        if shown > 0 {
            let (slot0, right, up) = shelf_slot(0, shown, row);
            let label_origin = slot0 + up.scale(kit::CARD_H / 2.0 + 0.035)
                - right.scale(kit::CARD_W / 2.0);
            let host_color = if host.connected {
                kit::TEXT_2
            } else {
                kit::TEXT_3
            };
            out.texts.push(TextRun {
                origin: lift(label_origin, right, up, LIFT_TEXT, floor_y),
                right,
                up,
                height: 0.026,
                color: host_color,
                align: TextAlign::Left,
                max_width: 0.6,
                text: if host.connected {
                    format!("{} — {}", host.name, host.platform)
                } else {
                    format!("{} — offline", host.name)
                },
            });
        }

        for (i, agent) in agents.iter().take(ROW_CAP).enumerate() {
            let (center, right, up) = shelf_slot(i, shown, row);
            card(
                agent,
                center,
                right,
                up,
                selected_id,
                hover_id,
                floor_y,
                out,
            );
        }
        if agents.len() > ROW_CAP {
            let (last, right, up) = shelf_slot(ROW_CAP - 1, shown, row);
            let more = last + right.scale(kit::CARD_W / 2.0 + 0.05);
            out.texts.push(TextRun {
                origin: lift(more, right, up, LIFT_TEXT, floor_y),
                right,
                up,
                height: 0.024,
                color: kit::TEXT_3,
                align: TextAlign::Left,
                max_width: 0.0,
                text: format!("+{} more", agents.len() - ROW_CAP),
            });
        }
    }

    if let Some(agent) = selected_id.and_then(|id| snap.agents.iter().find(|a| a.id == id)) {
        workbench(agent, hover_id, confirm, floor_y, measure, out);
    }

    if !pending_approvals.is_empty() {
        banner(&pending_approvals, hover_id, floor_y, out);
    }
}

/// Live display streams as floating screens on the operator's left —
/// the fleet's actual desktops in the room. Capped at two stacked
/// 16:9 screens in v1; the rest are counted honestly.
fn monitors(displays: &[(String, String)], floor_y: f32, out: &mut SceneBatches) {
    const AZ: f32 = -0.66; // ≈ −38°: left of the shelf, inside the arc
    const DIST: f32 = 1.85;
    const HALF_W: f32 = 0.60;
    const HALF_H: f32 = HALF_W * 9.0 / 16.0;
    let right = v3(AZ.cos(), 0.0, AZ.sin());
    let up = v3(0.0, 1.0, 0.0);
    for (i, (id, label)) in displays.iter().take(2).enumerate() {
        let y = 1.52 - i as f32 * (HALF_H * 2.0 + 0.12);
        let center = v3(DIST * AZ.sin(), y, -DIST * AZ.cos());
        // Bezel sits just behind the video plane.
        out.panels.push(PanelInstance {
            center: lift(center, right, up, -0.004, floor_y),
            right,
            up,
            half_w: HALF_W + 0.016,
            half_h: HALF_H + 0.016,
            radius: 0.024,
            fill: kit::SURFACE_2,
            border: kit::LINE_2,
            border_w: 0.003,
        });
        out.monitors.push(MonitorInstance {
            id: id.clone(),
            center: at_floor(center, floor_y),
            right,
            up,
            half_w: HALF_W,
            half_h: HALF_H,
        });
        out.texts.push(TextRun {
            origin: lift(
                center - up.scale(HALF_H + 0.045),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: 0.024,
            color: kit::TEXT_2,
            align: TextAlign::Center,
            max_width: HALF_W * 2.0,
            text: label.clone(),
        });
    }
    if displays.len() > 2 {
        let center = v3(DIST * AZ.sin(), 0.62, -DIST * AZ.cos());
        out.texts.push(TextRun {
            origin: lift(center, right, up, LIFT_TEXT, floor_y),
            right,
            up,
            height: 0.024,
            color: kit::TEXT_3,
            align: TextAlign::Center,
            max_width: 0.0,
            text: format!("+{} more displays on the dashboard", displays.len() - 2),
        });
    }
}

/// Shift a point along a panel's outward normal and apply the floor
/// offset — the co-planarity lift for decor and text.
fn lift(p: Vec3, right: Vec3, up: Vec3, amount: f32, floor_y: f32) -> Vec3 {
    let n = right.cross(up).normalize();
    let lifted = p + n.scale(amount);
    v3(lifted.x, lifted.y + floor_y, lifted.z)
}

fn at_floor(p: Vec3, floor_y: f32) -> Vec3 {
    v3(p.x, p.y + floor_y, p.z)
}

fn backdrop(passthrough: bool, floor_y: f32, out: &mut SceneBatches) {
    if passthrough {
        // AR: the operator's real room is the backdrop — just a quiet
        // floor ring marking the anchor.
        let color = kit::rgba(0xE6ECF7, 0.14);
        let segments = 48;
        let radius = 0.55;
        for i in 0..segments {
            let a0 = (i as f32 / segments as f32) * std::f32::consts::TAU;
            let a1 = ((i + 1) as f32 / segments as f32) * std::f32::consts::TAU;
            out.frame.push_line(
                v3(radius * a0.cos(), floor_y + 0.002, radius * a0.sin()),
                v3(radius * a1.cos(), floor_y + 0.002, radius * a1.sin()),
                color,
            );
        }
    } else {
        // VR: the composed environment is a later milestone; a calm grid
        // grounds the void meanwhile.
        let grid = rgba(0x596580, 0.42);
        let extent = 4.0f32;
        let step = 0.5f32;
        let mut d = -extent;
        while d <= extent + 1e-4 {
            out.frame.push_line(
                v3(d, floor_y, -extent),
                v3(d, floor_y, extent),
                grid,
            );
            out.frame.push_line(
                v3(-extent, floor_y, d),
                v3(extent, floor_y, d),
                grid,
            );
            d += step;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn card(
    agent: &XrAgent,
    center: Vec3,
    right: Vec3,
    up: Vec3,
    selected_id: Option<&str>,
    hover_id: Option<&str>,
    floor_y: f32,
    out: &mut SceneBatches,
) {
    let hw = kit::CARD_W / 2.0;
    let hh = kit::CARD_H / 2.0;
    let dimf: f32 = if agent.recent { 0.5 } else { 1.0 };
    let is_selected = selected_id == Some(agent.id.as_str());
    // Hover carries hit-target ids ("card:<agent>"), not agent ids.
    let hover_key = format!("card:{}", agent.id);
    let is_hover = hover_id == Some(hover_key.as_str());

    let border = if is_selected {
        kit::IRIS
    } else if agent.needs_approval {
        kit::AMBER
    } else if is_hover {
        kit::IRIS_SOFT
    } else {
        kit::LINE_2
    };

    out.panels.push(PanelInstance {
        center: at_floor(center, floor_y),
        right,
        up,
        half_w: hw,
        half_h: hh,
        radius: 0.022,
        fill: dim(kit::SURFACE, dimf.max(0.7)),
        border: dim(border, dimf),
        border_w: if is_selected || agent.needs_approval || is_hover {
            0.004
        } else {
            0.002
        },
    });

    let accent = status_color(&agent.status, &agent.phase);

    // Status dot.
    out.panels.push(PanelInstance {
        center: lift(
            center + right.scale(-hw + 0.030) + up.scale(hh - 0.036),
            right,
            up,
            LIFT_DECOR,
            floor_y,
        ),
        right,
        up,
        half_w: 0.009,
        half_h: 0.009,
        radius: 0.009,
        fill: dim(accent, dimf),
        border: [0.0; 4],
        border_w: 0.0,
    });

    // Label.
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-hw + 0.052) + up.scale(hh - 0.048),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: 0.027,
        color: dim(kit::TEXT, dimf),
        align: TextAlign::Left,
        max_width: kit::CARD_W - 0.075,
        text: agent.label(),
    });

    // Status / phase line.
    let status_line = if agent.recent {
        "recent session".to_string()
    } else if agent.status == agent.phase || agent.phase.is_empty() {
        agent.status.clone()
    } else {
        format!("{} · {}", agent.status, agent.phase)
    };
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-hw + 0.052) + up.scale(hh - 0.082),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: 0.019,
        color: dim(accent, dimf),
        align: TextAlign::Left,
        max_width: kit::CARD_W - 0.075,
        text: status_line,
    });

    // Goal (fallback: task) line.
    let line = if !agent.goal_objective.is_empty() {
        agent.goal_objective.clone()
    } else {
        agent.task.clone()
    };
    if !line.is_empty() {
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(-hw + 0.024) + up.scale(-0.014),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: 0.019,
            color: dim(kit::TEXT_2, dimf),
            align: TextAlign::Left,
            max_width: kit::CARD_W - 0.048,
            text: line,
        });
    }

    // Context-pressure meter along the bottom edge (live sessions only).
    if !agent.recent {
        let pressure = agent.context_pressure();
        let track_y = -hh + 0.026;
        let track_hw = hw - 0.026;
        out.panels.push(PanelInstance {
            center: lift(center + up.scale(track_y), right, up, LIFT_DECOR, floor_y),
            right,
            up,
            half_w: track_hw,
            half_h: 0.0035,
            radius: 0.0035,
            fill: kit::LINE,
            border: [0.0; 4],
            border_w: 0.0,
        });
        if pressure > 0.01 {
            let fill_hw = track_hw * pressure;
            let color = if pressure > 0.85 {
                kit::RED
            } else if pressure > 0.65 {
                kit::AMBER
            } else {
                kit::IRIS
            };
            out.panels.push(PanelInstance {
                center: lift(
                    center + up.scale(track_y) + right.scale(-track_hw + fill_hw),
                    right,
                    up,
                    LIFT_DECOR * 1.5,
                    floor_y,
                ),
                right,
                up,
                half_w: fill_hw,
                half_h: 0.0035,
                radius: 0.0035,
                fill: color,
                border: [0.0; 4],
                border_w: 0.0,
            });
        }
    }

    out.hits.push(HitTarget {
        id: format!("card:{}", agent.id),
        kind: HitKind::Card,
        agent_id: agent.id.clone(),
        panel: Panel {
            center: at_floor(center, floor_y),
            right,
            up,
            half_w: hw,
            half_h: hh,
        },
    });
}

fn workbench(
    agent: &XrAgent,
    hover_id: Option<&str>,
    confirm: Option<(&str, f32)>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    let (center, right, up) = front_panel_basis(kit::WORKBENCH_DIST, kit::WORKBENCH_Y);
    let hw = kit::WORKBENCH_HALF_W;
    let hh = kit::WORKBENCH_HALF_H;

    out.panels.push(PanelInstance {
        center: at_floor(center, floor_y),
        right,
        up,
        half_w: hw,
        half_h: hh,
        radius: 0.03,
        fill: kit::SURFACE_2,
        border: kit::LINE_2,
        border_w: 0.003,
    });

    let pad = 0.035;
    let left = -hw + pad;
    let mut y = hh - 0.055;
    let mut line = |txt: &str, height: f32, color: [f32; 4], y_ref: &mut f32| {
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(left) + up.scale(*y_ref),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height,
            color,
            align: TextAlign::Left,
            max_width: hw * 2.0 - pad * 2.0,
            text: txt.to_string(),
        });
        *y_ref -= height + 0.020;
    };

    line(&agent.label(), 0.034, kit::TEXT, &mut y);
    let accent = status_color(&agent.status, &agent.phase);
    let status = if agent.phase.is_empty() || agent.phase == agent.status {
        agent.status.clone()
    } else {
        format!("{} · {}", agent.status, agent.phase)
    };
    line(&status, 0.022, accent, &mut y);
    if !agent.goal_objective.is_empty() {
        let goal = if agent.goal_status.is_empty() {
            format!("goal: {}", agent.goal_objective)
        } else {
            format!("goal ({}): {}", agent.goal_status, agent.goal_objective)
        };
        line(&goal, 0.022, kit::TEXT_2, &mut y);
    }
    if !agent.task.is_empty() && agent.task != agent.goal_objective {
        line(&agent.task, 0.020, kit::TEXT_2, &mut y);
    }
    if agent.needs_approval && !agent.approval_command.is_empty() {
        line(
            &format!("wants: {}", agent.approval_command),
            0.020,
            kit::AMBER,
            &mut y,
        );
    }

    // Context meter across the panel.
    let pressure = agent.context_pressure();
    let meter_y = -hh + 0.075;
    let meter_hw = hw - pad;
    out.panels.push(PanelInstance {
        center: lift(center + up.scale(meter_y), right, up, LIFT_DECOR, floor_y),
        right,
        up,
        half_w: meter_hw,
        half_h: 0.004,
        radius: 0.004,
        fill: kit::LINE,
        border: [0.0; 4],
        border_w: 0.0,
    });
    if pressure > 0.01 {
        out.panels.push(PanelInstance {
            center: lift(
                center + up.scale(meter_y) + right.scale(-meter_hw + meter_hw * pressure),
                right,
                up,
                LIFT_DECOR * 1.5,
                floor_y,
            ),
            right,
            up,
            half_w: meter_hw * pressure,
            half_h: 0.004,
            radius: 0.004,
            fill: if pressure > 0.85 {
                kit::RED
            } else if pressure > 0.65 {
                kit::AMBER
            } else {
                kit::IRIS
            },
            border: [0.0; 4],
            border_w: 0.0,
        });
    }
    // Approve / deny pills (drawn now; the pinch-hold interaction arms
    // them in the input pass). Widths fit their labels.
    if agent.needs_approval {
        let pill_y = -hh + 0.028;
        let pill_hh = 0.020;
        let label_h = 0.021;
        let specs: [(&str, HitKind, [f32; 4]); 2] = [
            ("approve", HitKind::Approve, kit::GREEN),
            ("deny", HitKind::Deny, kit::RED),
        ];
        let mut pen = left;
        for (label, kind, color) in specs.iter() {
            let pill_hw = (measure.measure(label, label_h) / 2.0 + 0.022).max(0.045);
            let x = pen + pill_hw;
            pen = x + pill_hw + 0.024;
            let pcenter = center + right.scale(x) + up.scale(pill_y);
            let pill_id = format!("pill:{}:{}", agent.id, label);
            let is_hover = hover_id == Some(pill_id.as_str());
            let held = confirm.filter(|(id, _)| *id == pill_id.as_str());
            out.panels.push(PanelInstance {
                center: lift(pcenter, right, up, LIFT_DECOR, floor_y),
                right,
                up,
                half_w: pill_hw,
                half_h: pill_hh,
                radius: pill_hh,
                fill: if held.is_some() {
                    dim(*color, 0.18)
                } else if is_hover {
                    dim(*color, 0.30)
                } else {
                    kit::SURFACE
                },
                border: *color,
                border_w: if is_hover || held.is_some() { 0.0035 } else { 0.0025 },
            });
            // Deliberate-confirm feedback: the pill fills left→right over
            // the hold window; release early and it drains away.
            if let Some((_, progress)) = held {
                let fill_hw = pill_hw * progress.clamp(0.0, 1.0);
                if fill_hw > 0.003 {
                    out.panels.push(PanelInstance {
                        center: lift(
                            pcenter + right.scale(-pill_hw + fill_hw),
                            right,
                            up,
                            LIFT_DECOR * 2.0,
                            floor_y,
                        ),
                        right,
                        up,
                        half_w: fill_hw,
                        half_h: pill_hh,
                        radius: pill_hh.min(fill_hw),
                        fill: dim(*color, 0.55),
                        border: [0.0; 4],
                        border_w: 0.0,
                    });
                }
            }
            out.texts.push(TextRun {
                origin: lift(
                    pcenter - up.scale(0.008),
                    right,
                    up,
                    LIFT_TEXT + LIFT_DECOR,
                    floor_y,
                ),
                right,
                up,
                height: 0.021,
                color: *color,
                align: TextAlign::Center,
                max_width: pill_hw * 2.0 - 0.01,
                text: (*label).to_string(),
            });
            out.hits.push(HitTarget {
                id: pill_id,
                kind: *kind,
                agent_id: agent.id.clone(),
                panel: Panel {
                    center: at_floor(pcenter, floor_y),
                    right,
                    up,
                    half_w: pill_hw,
                    half_h: pill_hh,
                },
            });
        }
    }
}

fn banner(pending: &[&XrAgent], hover_id: Option<&str>, floor_y: f32, out: &mut SceneBatches) {
    let (center, right, up) = front_panel_basis(kit::BANNER_DIST, kit::BANNER_Y);
    let first = pending[0];
    let hw = 0.36;
    let hh = 0.042;
    let id = format!("banner:{}", first.id);
    let is_hover = hover_id == Some(id.as_str());

    out.panels.push(PanelInstance {
        center: at_floor(center, floor_y),
        right,
        up,
        half_w: hw,
        half_h: hh,
        radius: 0.02,
        fill: if is_hover {
            dim(kit::AMBER, 0.22)
        } else {
            kit::SURFACE_2
        },
        border: kit::AMBER,
        border_w: 0.003,
    });
    let label = if pending.len() == 1 {
        format!("approval pending — {}", first.approval_command)
    } else {
        format!(
            "{} approvals pending — {}",
            pending.len(),
            first.approval_command
        )
    };
    out.texts.push(TextRun {
        origin: lift(center - up.scale(0.009), right, up, LIFT_TEXT, floor_y),
        right,
        up,
        height: 0.024,
        color: kit::AMBER,
        align: TextAlign::Center,
        max_width: hw * 2.0 - 0.05,
        text: label,
    });
    out.hits.push(HitTarget {
        id,
        kind: HitKind::Card,
        agent_id: first.id.clone(),
        panel: Panel {
            center: at_floor(center, floor_y),
            right,
            up,
            half_w: hw,
            half_h: hh,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::ApproxMeasure;

    fn snapshot() -> XrSnapshot {
        serde_json::from_value(serde_json::json!({
            "hosts": [
                {"id": "local", "name": "mac", "platform": "macos", "connected": true},
                {"id": "dell", "name": "dell-206", "platform": "linux", "connected": true}
            ],
            "agents": [
                {"id": "a1", "hostId": "local", "status": "running", "phase": "working",
                 "sessionId": "s1", "source": "claude-code", "tokens": 120000,
                 "tokenCap": 200000, "goalObjective": "ship the XR surface"},
                {"id": "a2", "hostId": "local", "status": "waiting", "phase": "approval",
                 "needsApproval": true, "approvalId": "ap9",
                 "approvalCommand": "cargo publish", "sessionId": "s2", "source": "codex"},
                {"id": "a3", "hostId": "dell", "status": "idle", "recent": true}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn scene_builds_cards_banner_and_hits() {
        let snap = snapshot();
        let mut out = SceneBatches::default();
        build_scene(&snap, &[], None, None, None, true, 0.0, &ApproxMeasure, &mut out);

        // 3 cards + 1 banner target; no workbench pills without selection.
        let cards = out
            .hits
            .iter()
            .filter(|h| h.kind == HitKind::Card)
            .count();
        assert_eq!(cards, 4, "3 cards + banner");
        assert!(out.hits.iter().all(|h| h.kind != HitKind::Approve));
        // Approval card gets the amber border; recent card is inert data.
        assert!(out.panels.len() >= 3);
        // AR backdrop = floor ring lines, no grid walls.
        assert!(out.frame.line_vertex_count() == 48 * 2);
    }

    #[test]
    fn selecting_the_approval_agent_arms_pills() {
        let snap = snapshot();
        let mut out = SceneBatches::default();
        build_scene(&snap, &[], Some("a2"), None, None, false, 0.0, &ApproxMeasure, &mut out);
        let approve = out
            .hits
            .iter()
            .find(|h| h.kind == HitKind::Approve)
            .expect("approve pill armed");
        assert_eq!(approve.agent_id, "a2");
        assert_eq!(approve.id, "pill:a2:approve");
        assert!(out.hits.iter().any(|h| h.kind == HitKind::Deny));
        // VR backdrop grid: 17 lines per direction, 2 verts each.
        assert_eq!(out.frame.line_vertex_count(), 17 * 2 * 2);
    }

    #[test]
    fn floor_offset_shifts_everything_down() {
        let snap = snapshot();
        let mut level = SceneBatches::default();
        let mut sunk = SceneBatches::default();
        build_scene(&snap, &[], None, None, None, true, 0.0, &ApproxMeasure, &mut level);
        build_scene(&snap, &[], None, None, None, true, -1.5, &ApproxMeasure, &mut sunk);
        let a = level.panels.first().unwrap().center;
        let b = sunk.panels.first().unwrap().center;
        assert!((a.y - b.y - 1.5).abs() < 1e-5);
    }

    #[test]
    fn empty_feed_renders_waiting_note() {
        let snap = XrSnapshot::default();
        let mut out = SceneBatches::default();
        build_scene(&snap, &[], None, None, None, true, 0.0, &ApproxMeasure, &mut out);
        assert!(out.hits.is_empty());
        assert_eq!(out.texts.len(), 1);
        assert!(out.texts[0].text.contains("waiting"));
    }
}
