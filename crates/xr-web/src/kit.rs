//! The spatial UI kit: palette, primitives, and layout math.
//!
//! This is where the regular dashboard's design language becomes spatial.
//! Colors port the ui-v2 tokens (`static/app/16-styles-v2-tokens.css`);
//! layout follows headset ergonomics — a mid-field fleet shelf on a
//! cylinder around the operator, a near-field workbench for the focused
//! session, everything world-locked and inside a comfortable frontal arc.
//!
//! Everything in this module is pure data + math (host-testable). The
//! browser-side pieces — glyph atlas, GL programs — consume these batches
//! in `atlas.rs` / `gl.rs`.

use crate::gl::SceneFrame;
use crate::math::{v3, Panel, Vec3};

// ---- palette (ported ui-v2 tokens; RGBA in linear-ish sRGB floats) -----

pub(crate) const fn rgba(hex: u32, alpha: f32) -> [f32; 4] {
    let r = ((hex >> 16) & 0xff) as f32 / 255.0;
    let g = ((hex >> 8) & 0xff) as f32 / 255.0;
    let b = (hex & 0xff) as f32 / 255.0;
    [r, g, b, alpha]
}

pub(crate) const SURFACE: [f32; 4] = rgba(0x14171F, 0.94); // --surface
pub(crate) const SURFACE_2: [f32; 4] = rgba(0x1A1E28, 0.96); // --surface-2
pub(crate) const LINE: [f32; 4] = rgba(0xE6ECF7, 0.10); // --line
pub(crate) const LINE_2: [f32; 4] = rgba(0xE6ECF7, 0.16); // --line-2
pub(crate) const TEXT: [f32; 4] = rgba(0xEAECF2, 1.0); // --text
pub(crate) const TEXT_2: [f32; 4] = rgba(0xA7AEBE, 1.0); // --text-2
pub(crate) const TEXT_3: [f32; 4] = rgba(0x7E8896, 1.0); // --text-3
pub(crate) const IRIS: [f32; 4] = rgba(0x7E8CFA, 1.0); // --iris
pub(crate) const IRIS_SOFT: [f32; 4] = rgba(0x7E8CFA, 0.35);
pub(crate) const GREEN: [f32; 4] = rgba(0x69D58C, 1.0);
pub(crate) const AMBER: [f32; 4] = rgba(0xE8C476, 1.0);
pub(crate) const RED: [f32; 4] = rgba(0xE87A8B, 1.0);

/// Status → accent color, matching the dashboard's status vocabulary.
pub(crate) fn status_color(status: &str, phase: &str) -> [f32; 4] {
    let key = if status.is_empty() { phase } else { status };
    match key {
        s if s.contains("error") || s.contains("fail") || s.contains("halt") => RED,
        s if s.contains("run") || s.contains("active") || s.contains("work") => GREEN,
        s if s.contains("wait") || s.contains("pend") || s.contains("approv") => AMBER,
        s if s.contains("idle") || s.contains("done") || s.contains("closed") => TEXT_3,
        _ => TEXT_2,
    }
}

// ---- primitives ---------------------------------------------------------

/// One rounded panel, drawn with per-instance uniforms (a handful of
/// panels per scene — draw-call count is a non-issue and the data path
/// stays trivial).
#[derive(Clone, Debug)]
pub(crate) struct PanelInstance {
    pub center: Vec3,
    pub right: Vec3,
    pub up: Vec3,
    pub half_w: f32,
    pub half_h: f32,
    /// Corner radius in meters.
    pub radius: f32,
    pub fill: [f32; 4],
    pub border: [f32; 4],
    /// Border band width in meters (0 = no border).
    pub border_w: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum TextAlign {
    Left,
    Center,
}

/// A single line of text on a panel plane. `origin` is the anchor on the
/// baseline (left end or center per `align`); glyph quads extend along
/// `right`/`up`. Converted to textured vertices by the atlas at build.
#[derive(Clone, Debug)]
pub(crate) struct TextRun {
    pub origin: Vec3,
    pub right: Vec3,
    pub up: Vec3,
    /// Glyph height in meters (cap-to-descender box).
    pub height: f32,
    pub color: [f32; 4],
    pub align: TextAlign,
    /// Truncate with an ellipsis beyond this width in meters (0 = no cap).
    pub max_width: f32,
    pub text: String,
}

/// What a ray can land on. The input layer maps (kind, agent) onto the
/// dashboard's action vocabulary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HitKind {
    Card,
    Approve,
    Deny,
}

#[derive(Clone, Debug)]
pub(crate) struct HitTarget {
    pub id: String,
    pub kind: HitKind,
    pub agent_id: String,
    pub panel: Panel,
}

/// Everything one scene build produces: panel instances, text runs, raw
/// line/tri streams (meters, edges, backdrop), and the hit targets the
/// input layer raycasts.
#[derive(Default)]
pub(crate) struct SceneBatches {
    pub panels: Vec<PanelInstance>,
    pub texts: Vec<TextRun>,
    pub frame: SceneFrame,
    pub hits: Vec<HitTarget>,
}

impl SceneBatches {
    pub fn clear(&mut self) {
        self.panels.clear();
        self.texts.clear();
        self.frame.clear();
        self.hits.clear();
    }
}

// ---- ergonomic layout constants -----------------------------------------

/// Fleet shelf: mid-field cylinder around the operator.
pub(crate) const SHELF_RADIUS: f32 = 2.0;
/// Session card size (meters) — readable at shelf distance without head
/// motion (≈12° wide at 2 m).
pub(crate) const CARD_W: f32 = 0.44;
pub(crate) const CARD_H: f32 = 0.26;
/// Arc step between card centers (radians) — card width plus breathing
/// room at shelf radius.
pub(crate) const CARD_ARC_STEP: f32 = (CARD_W + 0.06) / SHELF_RADIUS;
/// Host rows top-down: first host's card row height, per-row drop.
pub(crate) const SHELF_TOP_ROW_Y: f32 = 1.58;
pub(crate) const SHELF_ROW_DROP: f32 = CARD_H + 0.10;

/// Near-field workbench (the focused session's detail surface).
pub(crate) const WORKBENCH_DIST: f32 = 1.05;
pub(crate) const WORKBENCH_Y: f32 = 1.32;
pub(crate) const WORKBENCH_HALF_W: f32 = 0.33;
pub(crate) const WORKBENCH_HALF_H: f32 = 0.23;

/// Approval banner floats above the workbench line.
pub(crate) const BANNER_DIST: f32 = 1.1;
pub(crate) const BANNER_Y: f32 = 1.78;

/// A slot on the shelf cylinder: `index` within `count` cards on `row`
/// (0 = top). Returns (center, right, up); the panel normal faces the
/// operator column. Cards are centered on the -Z (initial facing) axis.
pub(crate) fn shelf_slot(index: usize, count: usize, row: usize) -> (Vec3, Vec3, Vec3) {
    let n = count.max(1) as f32;
    let az = (index as f32 - (n - 1.0) / 2.0) * CARD_ARC_STEP;
    let y = SHELF_TOP_ROW_Y - row as f32 * SHELF_ROW_DROP;
    let center = v3(
        SHELF_RADIUS * az.sin(),
        y,
        -SHELF_RADIUS * az.cos(),
    );
    let right = v3(az.cos(), 0.0, az.sin());
    let up = v3(0.0, 1.0, 0.0);
    (center, right, up)
}

/// A flat panel straight ahead at `dist`/`y` facing the operator.
pub(crate) fn front_panel_basis(dist: f32, y: f32) -> (Vec3, Vec3, Vec3) {
    (v3(0.0, y, -dist), v3(1.0, 0.0, 0.0), v3(0.0, 1.0, 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Ray;

    #[test]
    fn shelf_slots_sit_on_the_cylinder_facing_in() {
        for (i, n) in [(0usize, 1usize), (0, 5), (4, 5), (2, 5)] {
            let (center, right, up) = shelf_slot(i, n, 0);
            let horiz = (center.x * center.x + center.z * center.z).sqrt();
            assert!((horiz - SHELF_RADIUS).abs() < 1e-4, "on cylinder");
            // Panel normal (right × up) points back toward the operator
            // column: dot with (origin - center) must be positive.
            let normal = right.cross(up);
            let inward = v3(-center.x, 0.0, -center.z);
            assert!(normal.dot(inward) > 0.0, "faces the operator");
            // Cards never spawn behind the user (z must be negative for
            // sane counts).
            assert!(center.z < 0.0, "in the frontal arc");
        }
        // Odd count centers the middle card dead ahead.
        let (center, _, _) = shelf_slot(2, 5, 0);
        assert!(center.x.abs() < 1e-5);
    }

    #[test]
    fn shelf_rows_descend() {
        let (top, _, _) = shelf_slot(0, 1, 0);
        let (below, _, _) = shelf_slot(0, 1, 1);
        assert!(below.y < top.y);
        assert!((top.y - below.y - SHELF_ROW_DROP).abs() < 1e-6);
    }

    #[test]
    fn shelf_card_is_hittable_from_origin_gaze() {
        let (center, right, up) = shelf_slot(1, 3, 0);
        let panel = Panel {
            center,
            right,
            up,
            half_w: CARD_W / 2.0,
            half_h: CARD_H / 2.0,
        };
        // Ray from eye height at the panel center.
        let eye = v3(0.0, 1.5, 0.0);
        let dir = (center - eye).normalize();
        let hit = panel.raycast(&Ray { origin: eye, dir });
        let (t, u, v) = hit.expect("card visible from the operator");
        assert!(t > 1.0 && t < 3.0);
        assert!(u.abs() < 0.05 && v.abs() < 0.05, "dead-center hit");
    }

    #[test]
    fn status_colors_follow_dashboard_vocabulary() {
        assert_eq!(status_color("running", ""), GREEN);
        assert_eq!(status_color("", "waiting-approval"), AMBER);
        assert_eq!(status_color("error", "running"), RED);
        assert_eq!(status_color("idle", ""), TEXT_3);
        assert_eq!(status_color("mystery", ""), TEXT_2);
    }
}
