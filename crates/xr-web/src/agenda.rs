//! Agenda rail: the daemon's parked intent in the room.
//!
//! The careful port of the flat Agenda tab's card decisions, re-typeset
//! for the medium: a stack of item cards on the operator's RIGHT
//! (mirroring the monitors on the left), each carrying the tab's kind
//! mark (the amber `?` circle for questions, the open circle for
//! tasks/notes), its due-urgency chip (sky for upcoming, amber for
//! overdue), the blocked chip (rose), and the answered state (green).
//! Ordering flattens the tab's Now-lens precedence: questions awaiting
//! the owner first, then overdue reminders, then the rest in the feed's
//! newest-first order (recently completed items sink to the bottom as
//! muted "done" cards so the reopen undo has a target). Overflow past
//! the rail cap is counted honestly, and empty/unavailable states render
//! as in-scene text — a headset has no tooltips and no console.
//!
//! Quick-verbs this slice: the selected (expanded) card carries
//! `complete` (900 ms hold — semi-destructive) or `reopen` (quick pinch,
//! the undo on a done card); both route through the dashboard's own
//! `api_agenda_op` projection via the JS seam, and the seam feeds honest
//! per-item status back onto the rail. Answer/park-with-text wait for
//! the keyboard seat. The rail is also a floating surface: grab bar to
//! move it, x-pill or layout strip to dismiss it. Item titles are DATA:
//! they render as plain atlas text, never as instructions.

use crate::atlas::TextMeasure;
use crate::kit::{self, HitKind, HitTarget, PanelInstance, SceneBatches, TextAlign, TextRun};
use crate::math::{v3, Panel, Vec3};
use crate::model::{XrAgenda, XrAgendaItem, XrAgendaOpStatus};

/// Layer lifts off the card plane (meters) so co-planar content never
/// z-fights — private mirrors of `ui.rs`'s values (kept local so this
/// module stays additive; unify when the scene builder's next carve
/// makes them shared).
const LIFT_DECOR: f32 = 0.0018;
const LIFT_TEXT: f32 = 0.0036;

/// Type scale at rail distance (1.95 m): everything comfortably above
/// the ~0.016 m legibility floor the headset findings set (the rail
/// sits a step deeper than the monitors, so the small line gets a
/// notch more height to hold the same angular size).
const TITLE_H: f32 = 0.023;
const META_H: f32 = 0.018;
const HEADER_H: f32 = 0.026;
const NOTE_H: f32 = 0.019;
/// Baseline pitch between wrapped title lines.
const LINE_PITCH: f32 = 0.030;
/// Wrapped title lines on the expanded (selected) card.
const EXPAND_MAX_LINES: usize = 3;
/// Extra height the selected card grows for its quick-verb row.
const VERB_ROW_H: f32 = 0.048;
/// Left inset for text after the kind mark; right pad.
const PAD_MARK: f32 = 0.052;
const PAD_R: f32 = 0.024;
/// Gap between state chips on the meta line.
const CHIP_GAP: f32 = 0.014;

fn dim(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * f]
}

/// Shift a point along the rail plane's outward normal and apply the
/// floor offset (the same co-planarity lift the rest of the scene uses).
fn lift(p: Vec3, right: Vec3, up: Vec3, amount: f32, floor_y: f32) -> Vec3 {
    let n = right.cross(up).normalize();
    let lifted = p + n.scale(amount);
    v3(lifted.x, lifted.y + floor_y, lifted.z)
}

fn at_floor(p: Vec3, floor_y: f32) -> Vec3 {
    v3(p.x, p.y + floor_y, p.z)
}

/// Rail plane basis: `right`/`up` for a panel at azimuth `az` facing the
/// operator column (the mirror of the monitors' basis). The azimuth
/// comes from the layout pose — the rail is a movable surface.
fn rail_basis(az: f32) -> (Vec3, Vec3) {
    (v3(az.cos(), 0.0, az.sin()), v3(0.0, 1.0, 0.0))
}

/// Card-stack center for a given height `y` at azimuth `az`.
fn rail_center(az: f32, y: f32) -> Vec3 {
    v3(kit::AGENDA_DIST * az.sin(), y, -kit::AGENDA_DIST * az.cos())
}

/// The rail's display order — the flat tab's Now-lens precedence
/// flattened to the summary's vocabulary: (0) questions still awaiting
/// an answer, (1) overdue reminders, (2) everything else, (3) recently
/// completed cards (the reopen targets) at the bottom. Stable within
/// bands, preserving the feed's newest-first order (the JS seam sorts
/// with the tab's own created_ms ordering before capping).
fn band(item: &XrAgendaItem) -> u8 {
    if item.done {
        3
    } else if item.kind == "question" && !item.answered {
        0
    } else if item.overdue {
        1
    } else {
        2
    }
}

/// Indices of `items` in rail order.
fn rail_order(items: &[XrAgendaItem]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| band(&items[i]));
    order
}

/// Greedy word wrap against the live measurer: up to `max_lines` lines
/// within `max_w` meters. The last line carries whatever remains — the
/// text run's ellipsis cap truncates it honestly at draw time.
fn wrap_title(
    measure: &dyn TextMeasure,
    text: &str,
    height: f32,
    max_w: f32,
    max_lines: usize,
) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, word) in words.iter().enumerate() {
        let candidate = if current.is_empty() {
            (*word).to_string()
        } else {
            format!("{current} {word}")
        };
        if !current.is_empty() && measure.measure(&candidate, height) > max_w {
            lines.push(current);
            if lines.len() == max_lines {
                // Out of lines: the remainder rides the last line and
                // the ellipsis cap deals with it.
                let rest = words[i..].join(" ");
                let last = lines.pop().unwrap_or_default();
                lines.push(format!("{last} {rest}"));
                return lines;
            }
            current = (*word).to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Build the rail into the scene. `agenda == None` (a feed that carries
/// no agenda block) renders nothing; a dismissed rail renders nothing
/// (the layout strip is the way back); empty and error states render
/// honest in-scene text. Called from `ui.rs::build_scene` next to the
/// monitors — the rail, like the screens, is independent of the agents
/// feed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rail(
    agenda: Option<&XrAgenda>,
    selected_id: Option<&str>,
    hover_id: Option<&str>,
    confirm: Option<(&str, f32)>,
    layout: &kit::LayoutState,
    grab: Option<&str>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    let Some(agenda) = agenda else {
        return;
    };
    if layout.is_hidden("agenda") {
        return;
    }
    let pose = layout.pose("agenda");
    let az = pose.az;
    let top_y = pose.y;
    let (right, up) = rail_basis(az);
    let hw = kit::AGENDA_CARD_W / 2.0;

    // Grab bar above the header: the whole rail moves as one.
    grab_bar_for_rail(az, top_y, right, up, grab, hover_id, floor_y, out);

    // Header rides above the first card slot, host-label style, with the
    // dismiss x-pill on its right.
    let header_origin = rail_center(az, top_y + 0.030) - right.scale(hw);
    out.texts.push(TextRun {
        origin: lift(header_origin, right, up, LIFT_TEXT, floor_y),
        right,
        up,
        height: HEADER_H,
        color: kit::TEXT_2,
        align: TextAlign::Left,
        max_width: kit::AGENDA_CARD_W - 0.06,
        text: "Agenda".into(),
    });
    crate::ui::dismiss_pill(
        "close:agenda",
        "agenda",
        rail_center(az, top_y + 0.040) + right.scale(hw - 0.022),
        right,
        up,
        hover_id,
        floor_y,
        measure,
        out,
    );

    // Unavailable beats everything: say so where the operator can read
    // it, never a silent blank (the flat tab's load-error law).
    if !agenda.error.is_empty() {
        note(
            az,
            top_y - 0.030,
            "agenda unavailable",
            kit::AMBER,
            right,
            up,
            floor_y,
            out,
        );
        note(
            az,
            top_y - 0.062,
            &agenda.error,
            kit::TEXT_3,
            right,
            up,
            floor_y,
            out,
        );
        return;
    }

    // Honest per-item op feedback: the JS seam's live status for the
    // last complete/reopen fired from this room.
    let mut pen = top_y;
    if let Some(status) = agenda.op_status.as_ref().filter(|s| !s.state.is_empty()) {
        let (text, color) = op_status_line(status);
        note(az, pen - 0.006, &text, color, right, up, floor_y, out);
        pen -= 0.042;
    }

    if agenda.items.is_empty() {
        note(
            az,
            pen - 0.030,
            "agenda is empty",
            kit::TEXT_3,
            right,
            up,
            floor_y,
            out,
        );
        return;
    }

    let order = rail_order(&agenda.items);
    let shown = order.len().min(kit::AGENDA_RAIL_CAP);
    // `pen` walks the top edge of each card down the stack.
    for &idx in order.iter().take(kit::AGENDA_RAIL_CAP) {
        let item = &agenda.items[idx];
        let key = format!("agenda:{}", item.id);
        let is_selected = !item.id.is_empty() && selected_id == Some(key.as_str());
        let title_lines = if is_selected {
            wrap_title(
                measure,
                &item.title,
                TITLE_H,
                kit::AGENDA_CARD_W - PAD_MARK - PAD_R,
                EXPAND_MAX_LINES,
            )
        } else {
            vec![item.title.clone()]
        };
        // The selected card grows one more row for its quick-verb pill.
        let verbs = is_selected && !item.id.is_empty();
        let card_h = kit::AGENDA_CARD_H
            + (title_lines.len() - 1) as f32 * LINE_PITCH
            + if verbs { VERB_ROW_H } else { 0.0 };
        let hh = card_h / 2.0;
        let center = rail_center(az, pen - hh);
        card(
            item,
            &key,
            &title_lines,
            center,
            right,
            up,
            hh,
            is_selected,
            hover_id,
            confirm,
            floor_y,
            measure,
            out,
        );
        pen -= card_h + kit::AGENDA_CARD_GAP;
    }

    // Honest overflow: total open on the daemon minus what the rail
    // shows (the monitors' "+N more" convention).
    let more = (agenda.open as usize).saturating_sub(shown);
    if more > 0 {
        note(
            az,
            pen - 0.012,
            &format!("+{more} more on the dashboard"),
            kit::TEXT_3,
            right,
            up,
            floor_y,
            out,
        );
    }
}

/// The rail's grab bar, sized to the card width.
#[allow(clippy::too_many_arguments)]
fn grab_bar_for_rail(
    az: f32,
    top_y: f32,
    right: Vec3,
    up: Vec3,
    grab: Option<&str>,
    hover_id: Option<&str>,
    floor_y: f32,
    out: &mut SceneBatches,
) {
    crate::ui::grab_bar(
        "agenda",
        rail_center(az, top_y + 0.078),
        right,
        up,
        kit::AGENDA_CARD_W * 0.35,
        grab,
        hover_id,
        floor_y,
        out,
    );
}

/// The op-status line's text + accent: "completing…" while pending,
/// "completed" / "reopened" green, the daemon's refusal text rose.
fn op_status_line(status: &XrAgendaOpStatus) -> (String, [f32; 4]) {
    let verb = match status.op.as_str() {
        "reopen" => ("reopening…", "reopened"),
        _ => ("completing…", "completed"),
    };
    match status.state.as_str() {
        "pending" => (verb.0.to_string(), kit::TEXT_2),
        "ok" => (verb.1.to_string(), kit::GREEN),
        _ => {
            let detail = if status.detail.is_empty() {
                "agenda op failed".to_string()
            } else {
                status.detail.clone()
            };
            (detail, kit::RED)
        }
    }
}

/// One dim status line on the rail plane (empty state, errors, the
/// overflow count, op feedback).
#[allow(clippy::too_many_arguments)]
fn note(
    az: f32,
    y: f32,
    text: &str,
    color: [f32; 4],
    right: Vec3,
    up: Vec3,
    floor_y: f32,
    out: &mut SceneBatches,
) {
    let origin = rail_center(az, y) - right.scale(kit::AGENDA_CARD_W / 2.0);
    out.texts.push(TextRun {
        origin: lift(origin, right, up, LIFT_TEXT, floor_y),
        right,
        up,
        height: NOTE_H,
        color,
        align: TextAlign::Left,
        max_width: kit::AGENDA_CARD_W,
        text: text.to_string(),
    });
}

#[allow(clippy::too_many_arguments)]
fn card(
    item: &XrAgendaItem,
    key: &str,
    title_lines: &[String],
    center: Vec3,
    right: Vec3,
    up: Vec3,
    hh: f32,
    is_selected: bool,
    hover_id: Option<&str>,
    confirm: Option<(&str, f32)>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    let hw = kit::AGENDA_CARD_W / 2.0;
    let is_hover = !item.id.is_empty() && hover_id == Some(key);

    // Border vocabulary ports the session cards': loud iris on
    // hover/selection (the on-device legibility finding), amber for the
    // needs-you state — here, an overdue reminder. A done card is a
    // quiet undo target, never a warning.
    let border = if is_selected || is_hover {
        kit::IRIS
    } else if item.overdue && !item.done {
        kit::AMBER
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
        fill: kit::SURFACE,
        border,
        border_w: if is_selected || is_hover {
            0.004
        } else {
            0.002
        },
    });

    // Kind mark, the flat tab's ctl circle re-drawn: questions wear the
    // amber `?` badge (green once answered); tasks and notes the open
    // circle. The kind word on the meta line keeps task vs note honest.
    let mark_center = center + right.scale(-hw + 0.030) + up.scale(hh - 0.030);
    let is_question = item.kind == "question";
    let mark_color = if is_question && item.answered {
        kit::GREEN
    } else if is_question {
        kit::AMBER
    } else {
        kit::TEXT_3
    };
    out.panels.push(PanelInstance {
        center: lift(mark_center, right, up, LIFT_DECOR, floor_y),
        right,
        up,
        half_w: 0.011,
        half_h: 0.011,
        radius: 0.011,
        fill: if is_question {
            dim(mark_color, 0.12)
        } else {
            [0.0; 4]
        },
        border: dim(mark_color, 0.75),
        border_w: 0.0018,
    });
    if is_question {
        out.texts.push(TextRun {
            origin: lift(
                mark_center - up.scale(0.007),
                right,
                up,
                LIFT_TEXT + LIFT_DECOR,
                floor_y,
            ),
            right,
            up,
            height: 0.018,
            color: mark_color,
            align: TextAlign::Center,
            max_width: 0.0,
            text: "?".into(),
        });
    }

    // Title (wrapped only on the expanded card); a done card mutes.
    for (i, line) in title_lines.iter().enumerate() {
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(-hw + PAD_MARK) + up.scale(hh - 0.034 - i as f32 * LINE_PITCH),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: TITLE_H,
            color: if item.done { kit::TEXT_3 } else { kit::TEXT },
            align: TextAlign::Left,
            max_width: kit::AGENDA_CARD_W - PAD_MARK - PAD_R,
            text: line.clone(),
        });
    }

    // Meta line: the kind word, then the ported state chips — due
    // (sky / amber once overdue), blocked (rose), answered (green);
    // a done card carries just the green "done" state — pen-advanced
    // left to right; chips past the card's edge drop rather than
    // collide. The selected card's verb row sits below the meta line.
    let verbs = is_selected && !item.id.is_empty();
    let mut chips: Vec<(String, [f32; 4])> = vec![(item.kind.clone(), kit::TEXT_3)];
    if item.done {
        chips.push(("done".into(), kit::GREEN));
    } else {
        if !item.due.is_empty() {
            chips.push((
                item.due.clone(),
                if item.overdue { kit::AMBER } else { kit::SKY },
            ));
        }
        if item.blocked {
            chips.push(("blocked".into(), kit::RED));
        }
        if is_question {
            chips.push(if item.answered {
                ("answered".into(), kit::GREEN)
            } else {
                ("awaiting answer".into(), kit::AMBER)
            });
        }
    }
    let meta_y = -hh + 0.022 + if verbs { VERB_ROW_H } else { 0.0 };
    let mut pen = -hw + PAD_MARK;
    for (text, color) in chips {
        let w = measure.measure(&text, META_H);
        if pen + w > hw - PAD_R {
            break;
        }
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(pen) + up.scale(meta_y),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: META_H,
            color,
            align: TextAlign::Left,
            max_width: kit::AGENDA_CARD_W - PAD_R,
            text,
        });
        pen += w + CHIP_GAP;
    }

    // Quick-verb row on the expanded card: complete is semi-destructive
    // (900 ms hold with the confirm fill); reopen is the undo on a done
    // card (quick pinch). One verb per state — never both.
    if verbs {
        let (verb_id, label, kind, color) = if item.done {
            (
                format!("agendaop:{}:reopen", item.id),
                "reopen",
                HitKind::AgendaReopen,
                kit::IRIS_2,
            )
        } else {
            (
                format!("agendaop:{}:complete", item.id),
                "complete",
                HitKind::AgendaComplete,
                kit::GREEN,
            )
        };
        let label_h = 0.019;
        let pill_hw = (measure.measure(label, label_h) / 2.0 + 0.020).max(0.045);
        crate::ui::action_pill(
            &verb_id,
            label,
            kind,
            &item.id,
            color,
            center + right.scale(-hw + PAD_MARK + pill_hw) + up.scale(-hh + 0.028),
            right,
            up,
            hover_id,
            confirm,
            floor_y,
            measure,
            out,
        );
    }

    // A pinch selects (and expands) the card — local presentation only;
    // the id keys selection stably across feed reorders. Items without
    // ids render but aren't targets.
    if !item.id.is_empty() {
        out.hits.push(HitTarget {
            id: key.to_string(),
            kind: HitKind::Card,
            agent_id: key.to_string(),
            panel: Panel {
                center: at_floor(center, floor_y),
                right,
                up,
                half_w: hw,
                half_h: hh,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::ApproxMeasure;
    use crate::model::XrSnapshot;

    fn item(id: &str, kind: &str, overdue: bool, answered: bool) -> XrAgendaItem {
        XrAgendaItem {
            id: id.into(),
            title: format!("item {id}"),
            kind: kind.into(),
            due: if overdue {
                "overdue 2h".into()
            } else {
                String::new()
            },
            overdue,
            blocked: false,
            answered,
            done: false,
        }
    }

    fn agenda(items: Vec<XrAgendaItem>, open: u32) -> XrAgenda {
        XrAgenda {
            error: String::new(),
            open,
            items,
            op_status: None,
        }
    }

    fn build_rail(
        agenda: Option<&XrAgenda>,
        selected: Option<&str>,
        layout: &kit::LayoutState,
    ) -> SceneBatches {
        let mut out = SceneBatches::default();
        rail(
            agenda,
            selected,
            None,
            None,
            layout,
            None,
            0.0,
            &ApproxMeasure,
            &mut out,
        );
        out
    }

    fn texts_of(out: &SceneBatches) -> Vec<&str> {
        out.texts.iter().map(|t| t.text.as_str()).collect()
    }

    /// The rail's own card panels (skipping the grab bar / pill chrome).
    fn card_panels(out: &SceneBatches) -> Vec<&PanelInstance> {
        out.panels
            .iter()
            .filter(|p| (p.half_w - kit::AGENDA_CARD_W / 2.0).abs() < 1e-6)
            .collect()
    }

    #[test]
    fn rail_orders_questions_then_overdue_then_rest_then_done() {
        let mut done = item("d1", "task", false, false);
        done.done = true;
        let items = vec![
            done,
            item("t1", "task", false, false),
            item("t2", "task", true, false),
            item("q1", "question", false, false),
            item("q2", "question", false, true), // answered → out of band 0
            item("n1", "note", false, false),
        ];
        let order = rail_order(&items);
        let ids: Vec<&str> = order.iter().map(|&i| items[i].id.as_str()).collect();
        // Awaiting question first, overdue second; the rest keep their
        // feed order (stable sort); done cards sink to the bottom.
        assert_eq!(ids, vec!["q1", "t2", "t1", "q2", "n1", "d1"]);
    }

    #[test]
    fn rail_builds_cards_hits_chrome_and_overflow() {
        let mut items: Vec<XrAgendaItem> = (0..10)
            .map(|i| item(&format!("i{i}"), "task", false, false))
            .collect();
        items[0].blocked = true;
        let agenda = agenda(items, 12);
        let out = build_rail(Some(&agenda), None, &kit::LayoutState::default());

        // Rail cap: 8 cards, each a hit target keyed by item id, plus
        // the dismiss x-pill and the grab bar.
        let hits: Vec<&str> = out.hits.iter().map(|h| h.id.as_str()).collect();
        let cards = out.hits.iter().filter(|h| h.kind == HitKind::Card).count();
        assert_eq!(cards, kit::AGENDA_RAIL_CAP);
        assert!(hits.contains(&"agenda:i0"));
        assert!(hits.contains(&"close:agenda"));
        assert!(hits.contains(&"grab:agenda"));
        let close = out.hits.iter().find(|h| h.id == "close:agenda").unwrap();
        assert_eq!(close.kind, HitKind::SurfaceClose);
        assert_eq!(close.agent_id, "agenda");
        assert_eq!(
            out.hits
                .iter()
                .find(|h| h.id == "grab:agenda")
                .unwrap()
                .kind,
            HitKind::Grab
        );
        // Header, a blocked chip, and the honest overflow count (12 open
        // minus 8 shown).
        let texts = texts_of(&out);
        assert!(texts.contains(&"Agenda"));
        assert!(texts.contains(&"blocked"));
        assert!(texts.contains(&"+4 more on the dashboard"));
        // Cards sit on the operator's right, inside the frontal arc.
        for h in &out.hits {
            assert!(h.panel.center.x > 0.0, "rail is on the right");
            assert!(h.panel.center.z < 0.0, "rail is in front");
        }
        // Rail text never drops below the legibility floor.
        assert!(out.texts.iter().all(|t| t.height >= 0.016));
    }

    #[test]
    fn hidden_rail_builds_nothing_and_pose_moves_it() {
        let ag = agenda(vec![item("t1", "task", false, false)], 1);
        let mut layout = kit::LayoutState::default();
        layout.hide("agenda");
        let out = build_rail(Some(&ag), None, &layout);
        assert!(out.texts.is_empty() && out.panels.is_empty() && out.hits.is_empty());

        // A moved pose relocates every rail target along the band.
        let mut moved = kit::LayoutState::default();
        moved.set_pose("agenda", 0.3, 1.2);
        let default_out = build_rail(Some(&ag), None, &kit::LayoutState::default());
        let moved_out = build_rail(Some(&ag), None, &moved);
        let card_at = |out: &SceneBatches| {
            out.hits
                .iter()
                .find(|h| h.id == "agenda:t1")
                .unwrap()
                .panel
                .center
        };
        let a = card_at(&default_out);
        let b = card_at(&moved_out);
        assert!(a.x != b.x || a.y != b.y, "pose moved the rail");
        assert!(b.y < a.y, "lowered top edge lowers the cards");
    }

    #[test]
    fn question_and_due_chips_follow_the_tab_vocabulary() {
        let mut q = item("q1", "question", false, false);
        q.due = "due in 2h".into();
        let ag = agenda(vec![q], 1);
        let out = build_rail(Some(&ag), None, &kit::LayoutState::default());
        let texts = texts_of(&out);
        assert!(texts.contains(&"?"), "question mark badge");
        assert!(texts.contains(&"awaiting answer"));
        // Upcoming due wears sky; overdue flips to amber and the card
        // border warns.
        let due = out.texts.iter().find(|t| t.text == "due in 2h").unwrap();
        assert_eq!(due.color, kit::SKY);
        assert_eq!(card_panels(&out)[0].border, kit::LINE_2);

        let ov = item("t1", "task", true, false);
        let ag = agenda(vec![ov], 1);
        let out = build_rail(Some(&ag), None, &kit::LayoutState::default());
        let due = out.texts.iter().find(|t| t.text == "overdue 2h").unwrap();
        assert_eq!(due.color, kit::AMBER);
        assert_eq!(card_panels(&out)[0].border, kit::AMBER);
    }

    #[test]
    fn answered_question_reads_green() {
        let ag = agenda(vec![item("q1", "question", false, true)], 1);
        let out = build_rail(Some(&ag), None, &kit::LayoutState::default());
        let answered = out.texts.iter().find(|t| t.text == "answered").unwrap();
        assert_eq!(answered.color, kit::GREEN);
    }

    #[test]
    fn selection_expands_the_card_in_place() {
        let mut long = item("q1", "question", false, false);
        long.title =
            "a genuinely long parked question title that cannot fit one rail line".repeat(2);
        let ag = agenda(vec![long, item("t1", "task", false, false)], 2);

        let collapsed = build_rail(Some(&ag), None, &kit::LayoutState::default());
        let expanded = build_rail(Some(&ag), Some("agenda:q1"), &kit::LayoutState::default());

        let h = |out: &SceneBatches| {
            out.hits
                .iter()
                .find(|t| t.id == "agenda:q1")
                .unwrap()
                .panel
                .half_h
        };
        assert!(h(&expanded) > h(&collapsed), "selected card grows");
        assert!(
            expanded.texts.len() > collapsed.texts.len(),
            "wrapped title lines render"
        );
        // Selection wears the loud iris border.
        assert_eq!(card_panels(&expanded)[0].border, kit::IRIS);
        // The card below shifts down to make room.
        let below = |out: &SceneBatches| {
            out.hits
                .iter()
                .find(|t| t.id == "agenda:t1")
                .unwrap()
                .panel
                .center
                .y
        };
        assert!(below(&expanded) < below(&collapsed));
    }

    #[test]
    fn selected_open_card_arms_the_complete_hold() {
        let ag = agenda(vec![item("t1", "task", false, false)], 1);
        // Unselected: no verb targets at all.
        let collapsed = build_rail(Some(&ag), None, &kit::LayoutState::default());
        assert!(collapsed
            .hits
            .iter()
            .all(|h| !h.id.starts_with("agendaop:")));
        let out = build_rail(Some(&ag), Some("agenda:t1"), &kit::LayoutState::default());
        let verb = out
            .hits
            .iter()
            .find(|h| h.id == "agendaop:t1:complete")
            .expect("complete pill armed");
        assert_eq!(verb.kind, HitKind::AgendaComplete);
        assert_eq!(verb.agent_id, "t1");
        assert!(out.hits.iter().all(|h| h.id != "agendaop:t1:reopen"));
        assert!(texts_of(&out).contains(&"complete"));
    }

    #[test]
    fn selected_done_card_arms_the_reopen_undo() {
        let mut done = item("d1", "task", true, false);
        done.done = true;
        let ag = agenda(vec![done], 1);
        let out = build_rail(Some(&ag), Some("agenda:d1"), &kit::LayoutState::default());
        let verb = out
            .hits
            .iter()
            .find(|h| h.id == "agendaop:d1:reopen")
            .expect("reopen pill armed");
        assert_eq!(verb.kind, HitKind::AgendaReopen);
        assert!(out.hits.iter().all(|h| h.id != "agendaop:d1:complete"));
        let texts = texts_of(&out);
        assert!(texts.contains(&"reopen"));
        assert!(texts.contains(&"done"), "done chip: {texts:?}");
        // A done card never warns, whatever its old due state.
        assert_eq!(card_panels(&out)[0].border, kit::IRIS, "selected border");
        assert!(!texts.contains(&"overdue 2h"), "due chip drops on done");
    }

    #[test]
    fn op_status_renders_honest_feedback() {
        let mut ag = agenda(vec![item("t1", "task", false, false)], 1);
        ag.op_status = Some(XrAgendaOpStatus {
            id: "t1".into(),
            op: "complete".into(),
            state: "pending".into(),
            detail: String::new(),
        });
        let out = build_rail(Some(&ag), None, &kit::LayoutState::default());
        assert!(texts_of(&out).contains(&"completing…"));

        ag.op_status.as_mut().unwrap().state = "ok".into();
        let out = build_rail(Some(&ag), None, &kit::LayoutState::default());
        let line = out.texts.iter().find(|t| t.text == "completed").unwrap();
        assert_eq!(line.color, kit::GREEN);

        ag.op_status = Some(XrAgendaOpStatus {
            id: "t1".into(),
            op: "reopen".into(),
            state: "error".into(),
            detail: "agenda op failed (403)".into(),
        });
        let out = build_rail(Some(&ag), None, &kit::LayoutState::default());
        let line = out
            .texts
            .iter()
            .find(|t| t.text == "agenda op failed (403)")
            .expect("refusal text rendered");
        assert_eq!(line.color, kit::RED);
    }

    #[test]
    fn empty_error_and_absent_states_are_honest() {
        let out = build_rail(
            Some(&agenda(Vec::new(), 0)),
            None,
            &kit::LayoutState::default(),
        );
        let texts = texts_of(&out);
        assert!(texts.contains(&"Agenda"));
        assert!(texts.contains(&"agenda is empty"));
        assert!(out.hits.iter().all(|h| h.kind != HitKind::Card));

        let err = XrAgenda {
            error: "agenda unavailable (503)".into(),
            open: 0,
            items: Vec::new(),
            op_status: None,
        };
        let out = build_rail(Some(&err), None, &kit::LayoutState::default());
        let texts = texts_of(&out);
        assert!(texts.contains(&"agenda unavailable"));
        assert!(texts.contains(&"agenda unavailable (503)"));

        // No agenda block at all → no rail, not even a header.
        let out = build_rail(None, None, &kit::LayoutState::default());
        assert!(out.texts.is_empty() && out.panels.is_empty() && out.hits.is_empty());
    }

    #[test]
    fn wrap_title_fills_lines_and_parks_the_rest_on_the_last() {
        let m = ApproxMeasure;
        // ApproxMeasure: width = chars * height * 0.52 → at height 1.0
        // and max_w 5.2, ten characters fit per line.
        let lines = wrap_title(&m, "alpha beta gamma delta epsilon", 1.0, 5.2, 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "alpha beta");
        assert_eq!(lines[1], "gamma delta epsilon");
        assert_eq!(wrap_title(&m, "", 1.0, 5.2, 2), vec![String::new()]);
        assert_eq!(wrap_title(&m, "one", 1.0, 5.2, 3), vec!["one".to_string()]);
    }

    #[test]
    fn build_scene_carries_the_rail_beside_the_shelf() {
        // The one ui.rs insertion: a snapshot with agenda data renders
        // the rail; one without renders exactly the scene it used to.
        let with: XrSnapshot = serde_json::from_value(serde_json::json!({
            "hosts": [{"id": "local", "name": "mac", "platform": "macos", "connected": true}],
            "agents": [{"id": "a1", "hostId": "local", "status": "running"}],
            "agenda": {"open": 1, "items": [
                {"id": "g1", "title": "water the plants", "kind": "task"}
            ]}
        }))
        .unwrap();
        let mut out = SceneBatches::default();
        crate::ui::build_scene(
            &with,
            &[],
            None,
            None,
            None,
            0,
            &Default::default(),
            true,
            0.0,
            &kit::LayoutState::default(),
            None,
            &ApproxMeasure,
            &mut out,
        );
        assert!(out.hits.iter().any(|h| h.id == "agenda:g1"));
        assert!(texts_of(&out).contains(&"water the plants"));

        let without: XrSnapshot = serde_json::from_value(serde_json::json!({
            "hosts": [{"id": "local", "name": "mac", "platform": "macos", "connected": true}],
            "agents": [{"id": "a1", "hostId": "local", "status": "running"}]
        }))
        .unwrap();
        let mut out = SceneBatches::default();
        crate::ui::build_scene(
            &without,
            &[],
            None,
            None,
            None,
            0,
            &Default::default(),
            true,
            0.0,
            &kit::LayoutState::default(),
            None,
            &ApproxMeasure,
            &mut out,
        );
        assert!(!out.hits.iter().any(|h| h.id.starts_with("agenda:")));
        assert!(!texts_of(&out).contains(&"Agenda"));
    }
}
