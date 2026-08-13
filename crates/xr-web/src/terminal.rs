//! In-scene terminal pane: read-only watching of the dashboard's
//! standalone shell (slice 1).
//!
//! The pane is a *mirror* of the flat Terminal tab's session — the same
//! PTY the dashboard's terminal machinery attaches (`terminal_open` /
//! `terminal_output` over the dashboard-control tunnel, terminal.rs on
//! the daemon). The dashboard side owns the attach and the xterm buffer;
//! the JS glue paints that buffer onto an offscreen canvas and registers
//! it here, so the daemon protocol is reused verbatim and XR adds no
//! second listener (a second `terminal_open` from the same page would
//! double every output frame into the page's single handler).
//!
//! Read-only is the slice: XR never sends `terminal_open` — the daemon's
//! `open_or_attach` SPAWNS a shell when none exists, and a watch surface
//! must not create PTYs on the machine. No page-side session means the
//! honest empty state, and the remedy is named in-scene. Input stays on
//! the dashboard (hardware keyboards in immersive sessions are
//! unverified on the Quest); the pane says so in a visible line rather
//! than a tooltip.
//!
//! Every feed edge is fail-soft: a malformed state push is dropped and
//! counted, a missing canvas renders the status line instead, and
//! nothing here can take the immersive session down.

use serde::Deserialize;
use wasm_bindgen::JsValue;

use crate::atlas::TextMeasure;
use crate::kit::{
    self, HitKind, HitTarget, MonitorInstance, PanelInstance, SceneBatches, TextAlign, TextRun,
};
use crate::math::{v3, Panel, Vec3};
use crate::Inner;

/// The read-only contract, rendered where the operator can see it.
const WATCHING_LINE: &str = "watching — input on the dashboard";
/// Empty state: the page has no terminal session to mirror.
const EMPTY_LINE: &str = "no terminals — open one on the dashboard";
/// Attach mirrored but the page's xterm (and so the canvas) not up yet.
const WARMING_LINE: &str = "waiting for terminal output…";

/// Co-planarity lifts, matching `ui.rs` (panel < decor < text).
const LIFT_DECOR: f32 = 0.0018;
const LIFT_TEXT: f32 = 0.0036;

/// Dashboard-fed pane state (the JS glue derives it from the flat
/// Terminal tab's own machinery every pump tick). Field names mirror the
/// wire convention (camelCase); unknown fields are ignored and numeric
/// oddities fold to defaults — same tolerance as the snapshot model.
#[derive(Clone, Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct TerminalView {
    /// The page has a terminal session to mirror (the flat tab armed
    /// its shell this page-load).
    pub(crate) present: bool,
    /// The PTY session is open and acked (drives the pill's status dot).
    pub(crate) live: bool,
    /// Pane label carrying the PTY's real id + host, composed dashboard-
    /// side ("shell-0 · This daemon").
    pub(crate) label: String,
    /// The flat tab's own status line, ported verbatim.
    pub(crate) status: String,
    /// "ok" / "warn" / "error" / "" — the flat tab's status classes.
    pub(crate) status_kind: String,
    /// Painted canvas height/width; 0 until the first paint reports it.
    pub(crate) aspect: f32,
}

/// Facade-owned pane state living in [`Inner`]. Host-constructible: the
/// canvas stays `None` off-browser so inline tests can exercise the pure
/// parts.
#[derive(Default)]
pub(crate) struct TerminalPane {
    /// Pane summoned (scene-local; survives snapshot churn).
    pub(crate) open: bool,
    pub(crate) view: TerminalView,
    /// Registered offscreen canvas the JS painter keeps fresh.
    pub(crate) canvas: Option<(String, web_sys::HtmlCanvasElement)>,
    /// Bumped on register + every `markTerminalCanvasDirty`; the encoder
    /// re-uploads only when it trails.
    pub(crate) canvas_generation: u64,
    /// Malformed `updateTerminal` pushes, dropped and counted.
    pub(crate) parse_errors: u64,
}

impl TerminalPane {
    /// Upload list for the encoder's canvas-texture pass (0 or 1 entry).
    pub(crate) fn canvas_uploads(&self) -> Vec<(String, web_sys::HtmlCanvasElement, u64)> {
        self.canvas
            .as_ref()
            .map(|(id, el)| (id.clone(), el.clone(), self.canvas_generation))
            .into_iter()
            .collect()
    }

    /// Cheap clone of what the scene build needs (pure data — no JS
    /// handles cross into the builder).
    pub(crate) fn pane_view(&self) -> PaneView {
        PaneView {
            open: self.open,
            view: self.view.clone(),
            canvas_id: self.canvas.as_ref().map(|(id, _)| id.clone()),
        }
    }
}

/// Snapshot of pane state for one scene build. Pure and host-testable.
#[derive(Clone, Default, Debug)]
pub(crate) struct PaneView {
    pub(crate) open: bool,
    pub(crate) view: TerminalView,
    pub(crate) canvas_id: Option<String>,
}

// ---- facade entry points -------------------------------------------------

/// Ingest one dashboard-side state push. Parse failures keep the
/// previous view and count — the feed must never take the session down.
pub(crate) fn apply_update(inner: &mut Inner, state: JsValue) {
    match serde_wasm_bindgen::from_value::<TerminalView>(state) {
        Ok(view) => apply_view(inner, view),
        Err(_) => inner.terminal.parse_errors += 1,
    }
}

/// Pure half of [`apply_update`], host-testable.
pub(crate) fn apply_view(inner: &mut Inner, view: TerminalView) {
    let changed = inner.terminal.view.present != view.present
        || inner.terminal.view.live != view.live
        || inner.terminal.view.label != view.label
        || inner.terminal.view.status != view.status
        || inner.terminal.view.status_kind != view.status_kind
        || (inner.terminal.view.aspect - view.aspect).abs() > 1e-3;
    inner.terminal.view = view;
    if changed {
        inner.ui_dirty = true;
    }
}

pub(crate) fn register_canvas(
    inner: &mut Inner,
    source_id: String,
    canvas: web_sys::HtmlCanvasElement,
) {
    inner.terminal.canvas = Some((source_id, canvas));
    // Registration implies a paintable frame; count it so the first
    // upload happens without a separate dirty mark.
    inner.terminal.canvas_generation += 1;
    inner.ui_dirty = true;
}

/// New painted content on the registered canvas. Texture-only: the next
/// frame re-uploads, no scene rebuild needed.
pub(crate) fn mark_canvas_dirty(inner: &mut Inner, source_id: &str) {
    if inner
        .terminal
        .canvas
        .as_ref()
        .is_some_and(|(id, _)| id == source_id)
    {
        inner.terminal.canvas_generation += 1;
    }
}

pub(crate) fn unregister_canvas(inner: &mut Inner, source_id: &str) {
    if inner
        .terminal
        .canvas
        .as_ref()
        .is_some_and(|(id, _)| id == source_id)
    {
        inner.terminal.canvas = None;
        inner.ui_dirty = true;
    }
}

/// Resolve a released (or name-activated) terminal hit. Light acts —
/// summon and dismiss — so a quick pinch is enough; no hold. Returns
/// true when the pane state changed.
pub(crate) fn handle_release(inner: &mut Inner, kind: HitKind) -> bool {
    match kind {
        HitKind::TerminalToggle => {
            inner.terminal.open = !inner.terminal.open;
            inner.ui_dirty = true;
            true
        }
        HitKind::TerminalClose if inner.terminal.open => {
            inner.terminal.open = false;
            inner.ui_dirty = true;
            true
        }
        _ => false,
    }
}

// ---- scene build ---------------------------------------------------------

/// Basis for a panel on the mid-field cylinder at azimuth `az` (radians,
/// +right of the initial facing), distance `dist`, height `y` — the same
/// math the monitor stack uses on the left.
fn side_basis(az: f32, dist: f32, y: f32) -> (Vec3, Vec3, Vec3) {
    (
        v3(dist * az.sin(), y, -dist * az.cos()),
        v3(az.cos(), 0.0, az.sin()),
        v3(0.0, 1.0, 0.0),
    )
}

fn lift(p: Vec3, right: Vec3, up: Vec3, amount: f32, floor_y: f32) -> Vec3 {
    let n = right.cross(up).normalize();
    let lifted = p + n.scale(amount);
    v3(lifted.x, lifted.y + floor_y, lifted.z)
}

fn at_floor(p: Vec3, floor_y: f32) -> Vec3 {
    v3(p.x, p.y + floor_y, p.z)
}

fn dim(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * f]
}

/// Status-kind accent, matching the flat tab's status classes.
fn status_kind_color(kind: &str) -> [f32; 4] {
    match kind {
        "ok" => kit::GREEN,
        "warn" => kit::AMBER,
        "error" => kit::RED,
        _ => kit::TEXT_2,
    }
}

/// Append the terminal affordances to a built scene: the summon pill
/// always, the pane when open. Called from the frame loop's scene
/// rebuild, after `ui::build_scene`, into the same batches.
pub(crate) fn build_pane(
    pane: &PaneView,
    hover_id: Option<&str>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    summon_pill(pane, hover_id, floor_y, measure, out);
    if !pane.open {
        return;
    }

    let (center, right, up) = side_basis(kit::TERMINAL_AZ, kit::TERMINAL_DIST, kit::TERMINAL_Y);
    let hw = kit::TERMINAL_HALF_W;
    let aspect = if pane.view.aspect > 0.05 {
        pane.view.aspect
    } else {
        kit::TERMINAL_DEFAULT_ASPECT
    };
    let hh = hw * aspect;

    // Bezel, matching the monitor stack's chrome.
    out.panels.push(PanelInstance {
        center: lift(center, right, up, -0.004, floor_y),
        right,
        up,
        half_w: hw + 0.016,
        half_h: hh + 0.016,
        radius: 0.024,
        fill: kit::SURFACE_2,
        border: kit::LINE_2,
        border_w: 0.003,
    });

    // Header row above the top edge: the PTY's real id/host on the left,
    // the dismiss pill on the right.
    let header_y = hh + 0.048;
    let label = if pane.view.label.is_empty() {
        "terminal".to_string()
    } else {
        pane.view.label.clone()
    };
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-hw) + up.scale(header_y),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: 0.026,
        color: kit::TEXT,
        align: TextAlign::Left,
        max_width: hw * 2.0 - 0.22,
        text: label,
    });
    close_pill(
        center, right, up, hw, header_y, hover_id, floor_y, measure, out,
    );

    if pane.view.present {
        match &pane.canvas_id {
            Some(id) => {
                // The live screen: the painter's canvas as a textured
                // quad through the encoder's canvas-source seam.
                out.monitors.push(MonitorInstance {
                    id: id.clone(),
                    center: at_floor(center, floor_y),
                    right,
                    up,
                    half_w: hw,
                    half_h: hh,
                });
            }
            None => {
                // Attach mirrored but nothing painted yet (xterm still
                // loading dashboard-side): keep the surface honest.
                out.texts.push(TextRun {
                    origin: lift(center, right, up, LIFT_TEXT, floor_y),
                    right,
                    up,
                    height: 0.024,
                    color: kit::TEXT_3,
                    align: TextAlign::Center,
                    max_width: hw * 2.0 - 0.06,
                    text: WARMING_LINE.to_string(),
                });
            }
        }
    } else {
        // No session to mirror — say so, and name the remedy.
        out.texts.push(TextRun {
            origin: lift(center, right, up, LIFT_TEXT, floor_y),
            right,
            up,
            height: 0.024,
            color: kit::TEXT_2,
            align: TextAlign::Center,
            max_width: hw * 2.0 - 0.06,
            text: EMPTY_LINE.to_string(),
        });
    }

    // Below the pane: the flat tab's status line verbatim, then the
    // read-only contract while something is being watched.
    let mut below_y = -hh - 0.045;
    if !pane.view.status.is_empty() {
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(-hw) + up.scale(below_y),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: 0.021,
            color: status_kind_color(&pane.view.status_kind),
            align: TextAlign::Left,
            max_width: hw * 2.0,
            text: pane.view.status.clone(),
        });
        below_y -= 0.036;
    }
    if pane.view.present {
        out.texts.push(TextRun {
            origin: lift(
                center + right.scale(-hw) + up.scale(below_y),
                right,
                up,
                LIFT_TEXT,
                floor_y,
            ),
            right,
            up,
            height: 0.019,
            color: kit::TEXT_3,
            align: TextAlign::Left,
            max_width: hw * 2.0,
            text: WATCHING_LINE.to_string(),
        });
    }
}

/// The always-present summon pill on the operator's right. Quick pinch
/// toggles the pane; the dot carries the session's liveness.
fn summon_pill(
    pane: &PaneView,
    hover_id: Option<&str>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    let (center, right, up) = side_basis(
        kit::TERMINAL_PILL_AZ,
        kit::TERMINAL_PILL_DIST,
        kit::TERMINAL_PILL_Y,
    );
    let text_h = 0.021;
    let label = "terminal";
    let dot_r = 0.006;
    let dot_gap = 0.012;
    let text_w = measure.measure(label, text_h);
    let pill_hw = ((text_w + dot_r * 2.0 + dot_gap) / 2.0 + 0.024).max(0.055);
    let pill_hh = 0.021;
    let is_hover = hover_id == Some("terminal:toggle");

    let border = if pane.open || is_hover {
        kit::IRIS
    } else {
        kit::LINE_2
    };
    out.panels.push(PanelInstance {
        center: at_floor(center, floor_y),
        right,
        up,
        half_w: pill_hw,
        half_h: pill_hh,
        radius: pill_hh,
        fill: if pane.open {
            dim(kit::IRIS, 0.18)
        } else if is_hover {
            dim(kit::IRIS, 0.30)
        } else {
            kit::SURFACE
        },
        border,
        border_w: if pane.open || is_hover {
            0.0035
        } else {
            0.0025
        },
    });
    // Liveness dot: green while a live PTY is being watched, muted
    // otherwise (mirrors the dashboard's status vocabulary).
    let content_hw = (dot_r * 2.0 + dot_gap + text_w) / 2.0;
    out.panels.push(PanelInstance {
        center: lift(
            center + right.scale(-content_hw + dot_r),
            right,
            up,
            LIFT_DECOR,
            floor_y,
        ),
        right,
        up,
        half_w: dot_r,
        half_h: dot_r,
        radius: dot_r,
        fill: if pane.view.present && pane.view.live {
            kit::GREEN
        } else {
            kit::TEXT_3
        },
        border: [0.0; 4],
        border_w: 0.0,
    });
    out.texts.push(TextRun {
        origin: lift(
            center + right.scale(-content_hw + dot_r * 2.0 + dot_gap) - up.scale(0.008),
            right,
            up,
            LIFT_TEXT,
            floor_y,
        ),
        right,
        up,
        height: text_h,
        color: kit::TEXT,
        align: TextAlign::Left,
        max_width: pill_hw * 2.0 - 0.02,
        text: label.to_string(),
    });
    out.hits.push(HitTarget {
        id: "terminal:toggle".to_string(),
        kind: HitKind::TerminalToggle,
        agent_id: String::new(),
        panel: Panel {
            center: at_floor(center, floor_y),
            right,
            up,
            half_w: pill_hw,
            half_h: pill_hh,
        },
    });
}

/// The pane's dismiss pill, top-right on the header row.
#[allow(clippy::too_many_arguments)]
fn close_pill(
    pane_center: Vec3,
    right: Vec3,
    up: Vec3,
    hw: f32,
    header_y: f32,
    hover_id: Option<&str>,
    floor_y: f32,
    measure: &dyn TextMeasure,
    out: &mut SceneBatches,
) {
    let text_h = 0.019;
    let label = "close";
    let pill_hw = (measure.measure(label, text_h) / 2.0 + 0.020).max(0.045);
    let pill_hh = 0.018;
    let center = pane_center + right.scale(hw - pill_hw) + up.scale(header_y - 0.008);
    let is_hover = hover_id == Some("terminal:close");
    out.panels.push(PanelInstance {
        center: lift(center, right, up, LIFT_DECOR, floor_y),
        right,
        up,
        half_w: pill_hw,
        half_h: pill_hh,
        radius: pill_hh,
        fill: if is_hover {
            dim(kit::IRIS, 0.30)
        } else {
            kit::SURFACE
        },
        border: if is_hover { kit::IRIS } else { kit::LINE_2 },
        border_w: if is_hover { 0.0035 } else { 0.0025 },
    });
    out.texts.push(TextRun {
        origin: lift(
            center - up.scale(0.0075),
            right,
            up,
            LIFT_TEXT + LIFT_DECOR,
            floor_y,
        ),
        right,
        up,
        height: text_h,
        color: if is_hover { kit::TEXT } else { kit::TEXT_2 },
        align: TextAlign::Center,
        max_width: pill_hw * 2.0 - 0.01,
        text: label.to_string(),
    });
    out.hits.push(HitTarget {
        id: "terminal:close".to_string(),
        kind: HitKind::TerminalClose,
        agent_id: String::new(),
        panel: Panel {
            center: at_floor(center, floor_y),
            right,
            up,
            half_w: pill_hw,
            half_h: pill_hh,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::ApproxMeasure;

    fn view(present: bool) -> PaneView {
        PaneView {
            open: true,
            view: TerminalView {
                present,
                live: present,
                label: "shell-0 · This daemon".into(),
                status: if present {
                    "Connected to This daemon".into()
                } else {
                    String::new()
                },
                status_kind: if present { "ok".into() } else { String::new() },
                aspect: 0.0,
            },
            canvas_id: present.then(|| "term:shell".to_string()),
        }
    }

    #[test]
    fn view_parses_the_dashboard_shape_and_ignores_extras() {
        let parsed: TerminalView = serde_json::from_value(serde_json::json!({
            "present": true, "live": true,
            "label": "shell-0 · This daemon",
            "status": "Connected to This daemon", "statusKind": "ok",
            "aspect": 0.7, "someFutureField": {"nested": true}
        }))
        .unwrap();
        assert!(parsed.present && parsed.live);
        assert_eq!(parsed.status_kind, "ok");
        assert!((parsed.aspect - 0.7).abs() < 1e-6);

        let empty: TerminalView = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!empty.present);
        assert_eq!(empty.aspect, 0.0);
    }

    #[test]
    fn closed_pane_offers_only_the_summon_pill() {
        let mut out = SceneBatches::default();
        let pane = PaneView::default();
        build_pane(&pane, None, 0.0, &ApproxMeasure, &mut out);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].id, "terminal:toggle");
        assert_eq!(out.hits[0].kind, HitKind::TerminalToggle);
        assert!(out.monitors.is_empty());
        // Pill + liveness dot only.
        assert_eq!(out.panels.len(), 2);
    }

    #[test]
    fn open_pane_with_canvas_watches_and_stays_honest() {
        let mut out = SceneBatches::default();
        build_pane(&view(true), None, 0.0, &ApproxMeasure, &mut out);
        // The canvas quad rides the monitor path under the canvas id.
        assert_eq!(out.monitors.len(), 1);
        assert_eq!(out.monitors[0].id, "term:shell");
        // Dismiss affordance armed.
        assert!(out.hits.iter().any(|h| h.id == "terminal:close"));
        // Label, status (verbatim), and the read-only contract line.
        assert!(out.texts.iter().any(|t| t.text == "shell-0 · This daemon"));
        assert!(out
            .texts
            .iter()
            .any(|t| t.text == "Connected to This daemon"));
        assert!(out.texts.iter().any(|t| t.text == WATCHING_LINE));
        assert!(!out.texts.iter().any(|t| t.text == EMPTY_LINE));
    }

    #[test]
    fn open_pane_without_session_renders_the_empty_state() {
        let mut out = SceneBatches::default();
        build_pane(&view(false), None, 0.0, &ApproxMeasure, &mut out);
        assert!(out.monitors.is_empty());
        assert!(out.texts.iter().any(|t| t.text == EMPTY_LINE));
        // No session — no watching claim.
        assert!(!out.texts.iter().any(|t| t.text == WATCHING_LINE));
        assert!(out.hits.iter().any(|h| h.id == "terminal:close"));
    }

    #[test]
    fn present_without_canvas_reports_warming_not_empty() {
        let mut out = SceneBatches::default();
        let mut pane = view(true);
        pane.canvas_id = None;
        build_pane(&pane, None, 0.0, &ApproxMeasure, &mut out);
        assert!(out.monitors.is_empty());
        assert!(out.texts.iter().any(|t| t.text == WARMING_LINE));
        assert!(!out.texts.iter().any(|t| t.text == EMPTY_LINE));
    }

    #[test]
    fn canvas_aspect_drives_pane_height() {
        let mut tall = SceneBatches::default();
        let mut pane = view(true);
        pane.view.aspect = 0.9;
        build_pane(&pane, None, 0.0, &ApproxMeasure, &mut tall);
        let mut default = SceneBatches::default();
        pane.view.aspect = 0.0;
        build_pane(&pane, None, 0.0, &ApproxMeasure, &mut default);
        let th = tall.monitors[0].half_h;
        let dh = default.monitors[0].half_h;
        assert!((th - kit::TERMINAL_HALF_W * 0.9).abs() < 1e-5);
        assert!((dh - kit::TERMINAL_HALF_W * kit::TERMINAL_DEFAULT_ASPECT).abs() < 1e-5);
    }

    #[test]
    fn release_toggles_and_close_only_closes() {
        let mut inner = Inner::new();
        assert!(!inner.terminal.open);
        assert!(handle_release(&mut inner, HitKind::TerminalToggle));
        assert!(inner.terminal.open && inner.ui_dirty);
        assert!(handle_release(&mut inner, HitKind::TerminalClose));
        assert!(!inner.terminal.open);
        // Closing a closed pane is a no-op, and card releases are not ours.
        assert!(!handle_release(&mut inner, HitKind::TerminalClose));
        assert!(!handle_release(&mut inner, HitKind::Card));
    }

    #[test]
    fn view_updates_mark_ui_dirty_only_on_change() {
        let mut inner = Inner::new();
        let v = TerminalView {
            present: true,
            live: true,
            label: "shell-0 · This daemon".into(),
            status: "Connected".into(),
            status_kind: "ok".into(),
            aspect: 0.62,
        };
        apply_view(&mut inner, v.clone());
        assert!(inner.ui_dirty);
        inner.ui_dirty = false;
        apply_view(&mut inner, v);
        assert!(!inner.ui_dirty, "identical push must not churn the scene");
    }

    #[test]
    fn dirty_marks_only_touch_the_registered_canvas() {
        let mut inner = Inner::new();
        assert_eq!(inner.terminal.canvas_generation, 0);
        mark_canvas_dirty(&mut inner, "term:shell");
        assert_eq!(
            inner.terminal.canvas_generation, 0,
            "no canvas registered — nothing to dirty"
        );
        unregister_canvas(&mut inner, "term:shell");
        assert!(inner.terminal.canvas.is_none());
    }
}
