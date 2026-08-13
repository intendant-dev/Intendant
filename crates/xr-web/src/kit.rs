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
pub(crate) const IRIS_2: [f32; 4] = rgba(0xA6AEFF, 1.0); // --iris-2
pub(crate) const SURFACE_3: [f32; 4] = rgba(0x232834, 0.96); // --surface-3
pub(crate) const GREEN: [f32; 4] = rgba(0x69D58C, 1.0);
pub(crate) const AMBER: [f32; 4] = rgba(0xE8C476, 1.0);
pub(crate) const RED: [f32; 4] = rgba(0xE87A8B, 1.0);
pub(crate) const SKY: [f32; 4] = rgba(0x6FB5EC, 1.0); // --sky (brightened for the medium like the other status hues)

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
/// dashboard's action vocabulary. The terminal toggle/close kinds are
/// scene-local (pane summon/dismiss — light acts, no daemon action
/// behind them); open/kill reach the dashboard's terminal machinery and
/// are hold-tier. The interaction grammar is absolute: quick pinch =
/// light/reversible, 900 ms hold = destructive or trust-critical.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HitKind {
    Card,
    Approve,
    Deny,
    TerminalToggle,
    TerminalClose,
    /// Open (or restart) the dashboard's standalone shell — the daemon's
    /// `open_or_attach` SPAWNS a PTY when none exists, so this is a
    /// deliberate hold-tier act.
    TerminalOpen,
    /// Kill the watched PTY (`terminal_close` on the wire) — hold tier,
    /// labeled honestly in-scene.
    TerminalKill,
    /// Stop a live turn (`session_action: interrupt`) — hold tier.
    Interrupt,
    /// One advertised thread-action op (compact/fork) — quick pinch; the
    /// op rides the target id's last segment.
    ThreadAction,
    /// Complete an open agenda item — semi-destructive, hold tier.
    AgendaComplete,
    /// Reopen a just-completed agenda item — the undo, quick pinch.
    AgendaReopen,
    /// Layout-strip visibility toggle for one surface family.
    LayoutToggle,
    /// Per-surface dismiss x-pill (agenda rail, single monitors).
    SurfaceClose,
    /// A surface's grab bar: pinch-hold and move the ray to reposition
    /// the surface on its cylinder band; release drops it.
    Grab,
    /// Transcript paging on the workbench: toward older rows.
    ScrollOlder,
    /// Transcript paging on the workbench: back toward the live tail.
    ScrollNewer,
}

#[derive(Clone, Debug)]
pub(crate) struct HitTarget {
    pub id: String,
    pub kind: HitKind,
    pub agent_id: String,
    pub panel: Panel,
}

/// One live display stream shown as a floating screen. The id keys the
/// encoder's video-texture map; geometry is panel-like.
#[derive(Clone, Debug)]
pub(crate) struct MonitorInstance {
    pub id: String,
    pub center: Vec3,
    pub right: Vec3,
    pub up: Vec3,
    pub half_w: f32,
    pub half_h: f32,
}

/// Everything one scene build produces: panel instances, text runs, raw
/// line/tri streams (meters, edges, backdrop), video monitors, and the
/// hit targets the input layer raycasts.
#[derive(Default)]
pub(crate) struct SceneBatches {
    pub panels: Vec<PanelInstance>,
    pub texts: Vec<TextRun>,
    pub frame: SceneFrame,
    pub monitors: Vec<MonitorInstance>,
    pub hits: Vec<HitTarget>,
    /// Wrapped transcript rows the focused workbench had available (0 =
    /// no transcript this build) and the scroll offset actually applied
    /// after clamping — the frame loop writes these back to the facade
    /// so paging stays bounded and `debug_json` reports truth.
    pub transcript_rows: usize,
    pub transcript_scroll: usize,
}

impl SceneBatches {
    pub fn clear(&mut self) {
        self.panels.clear();
        self.texts.clear();
        self.frame.clear();
        self.monitors.clear();
        self.hits.clear();
        self.transcript_rows = 0;
        self.transcript_scroll = 0;
    }
}

// ---- scene layout state (dismiss/summon + grab-to-move) -----------------

/// The four surface families the layout strip toggles. `sessions` covers
/// the shelf + workbench (never the approval banner — an urgent ask is
/// not tidy-away-able); the rest are the floating surfaces.
pub(crate) const LAYOUT_SURFACES: [&str; 4] = ["sessions", "terminal", "agenda", "monitors"];

/// Comfortable cylinder band for movable surfaces: azimuth stays inside
/// a wide frontal arc, height between knee and reach.
pub(crate) const SURFACE_AZ_MIN: f32 = -1.45;
pub(crate) const SURFACE_AZ_MAX: f32 = 1.45;
pub(crate) const SURFACE_Y_MIN: f32 = 0.75;
pub(crate) const SURFACE_Y_MAX: f32 = 2.05;

/// Cap on stored hidden keys (per-monitor entries are open-ended) so a
/// hostile feed can't grow the set without bound.
const HIDDEN_KEY_CAP: usize = 64;

/// One movable surface's anchor on the cylinder: azimuth (radians, +right
/// of the initial facing) and the anchor height in meters. What the
/// anchor means per surface: terminal = pane center, monitors = top
/// monitor center, agenda = top edge of the first card.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct SurfacePose {
    pub az: f32,
    pub y: f32,
}

/// Scene-local visibility + placement state. Lives in `Inner` so it
/// survives snapshot ticks; `ui2-xr.js` persists it to localStorage and
/// restores it on entry. Pure and host-testable.
#[derive(Default, Clone)]
pub(crate) struct LayoutState {
    /// Hidden keys: a surface family name from [`LAYOUT_SURFACES`], or a
    /// single monitor as `monitor:<source_id>`.
    hidden: std::collections::BTreeSet<String>,
    /// Stored poses for the movable surfaces (terminal / agenda /
    /// monitors); absent = the surface's default slot.
    poses: std::collections::BTreeMap<String, SurfacePose>,
}

/// Whether `name` is a movable floating surface (has a pose).
pub(crate) fn surface_movable(name: &str) -> bool {
    matches!(name, "terminal" | "agenda" | "monitors")
}

/// The default anchor for a movable surface — the constants the fixed
/// layout used before grab-to-move existed.
pub(crate) fn surface_default_pose(name: &str) -> Option<SurfacePose> {
    match name {
        "terminal" => Some(SurfacePose {
            az: TERMINAL_AZ,
            y: TERMINAL_Y,
        }),
        "agenda" => Some(SurfacePose {
            az: AGENDA_AZ,
            y: AGENDA_TOP_Y,
        }),
        "monitors" => Some(SurfacePose {
            az: MONITORS_AZ,
            y: MONITORS_TOP_Y,
        }),
        _ => None,
    }
}

fn clamp_pose(pose: SurfacePose) -> SurfacePose {
    SurfacePose {
        az: pose.az.clamp(SURFACE_AZ_MIN, SURFACE_AZ_MAX),
        y: pose.y.clamp(SURFACE_Y_MIN, SURFACE_Y_MAX),
    }
}

impl LayoutState {
    /// Effective pose for a movable surface (stored, else default).
    pub(crate) fn pose(&self, name: &str) -> SurfacePose {
        self.poses
            .get(name)
            .copied()
            .or_else(|| surface_default_pose(name))
            .unwrap_or(SurfacePose { az: 0.0, y: 1.4 })
    }

    pub(crate) fn is_hidden(&self, name: &str) -> bool {
        self.hidden.contains(name)
    }

    /// A single monitor is hidden by its own x-pill or by the family
    /// toggle.
    pub(crate) fn monitor_hidden(&self, source_id: &str) -> bool {
        self.is_hidden("monitors") || self.hidden.contains(&format!("monitor:{source_id}"))
    }

    /// Hide one key (a family name or `monitor:<id>`). Unknown family
    /// names are refused so a malformed target can't grow the set.
    pub(crate) fn hide(&mut self, key: &str) -> bool {
        let valid = LAYOUT_SURFACES.contains(&key)
            || key
                .strip_prefix("monitor:")
                .is_some_and(|id| !id.is_empty());
        if !valid || self.hidden.len() >= HIDDEN_KEY_CAP {
            return false;
        }
        self.hidden.insert(key.to_string())
    }

    /// Layout-strip toggle for one surface family. For `monitors` the
    /// summon direction also clears every per-monitor dismissal — the
    /// strip pill is the one place that brings the whole stack back.
    /// Returns the new "anything hidden" state for the family.
    pub(crate) fn toggle(&mut self, family: &str) -> bool {
        if !LAYOUT_SURFACES.contains(&family) {
            return false;
        }
        let any_hidden = if family == "monitors" {
            self.is_hidden("monitors") || self.hidden.iter().any(|k| k.starts_with("monitor:"))
        } else {
            self.is_hidden(family)
        };
        if any_hidden {
            self.hidden.remove(family);
            if family == "monitors" {
                self.hidden.retain(|k| !k.starts_with("monitor:"));
            }
            false
        } else {
            self.hidden.insert(family.to_string());
            true
        }
    }

    /// Absolute reposition (the grab path), clamped to the band.
    pub(crate) fn set_pose(&mut self, name: &str, az: f32, y: f32) -> bool {
        if !surface_movable(name) || !az.is_finite() || !y.is_finite() {
            return false;
        }
        self.poses
            .insert(name.to_string(), clamp_pose(SurfacePose { az, y }));
        true
    }

    /// Relative nudge (the QA facade / probe path), clamped.
    pub(crate) fn move_by(&mut self, name: &str, d_az: f32, d_y: f32) -> bool {
        if !surface_movable(name) || !d_az.is_finite() || !d_y.is_finite() {
            return false;
        }
        let cur = self.pose(name);
        self.set_pose(name, cur.az + d_az, cur.y + d_y)
    }

    /// JSON snapshot for persistence + `debug_json`:
    /// `{"hidden":[...],"poses":{"terminal":{"az":..,"y":..}}}`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "hidden": self.hidden.iter().collect::<Vec<_>>(),
            "poses": self
                .poses
                .iter()
                .map(|(k, p)| (k.clone(), serde_json::json!({ "az": p.az, "y": p.y })))
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        })
    }

    /// Restore from the persisted shape. Tolerant: unknown keys are
    /// dropped, values clamp, malformed JSON restores nothing. Returns
    /// whether anything was applied.
    pub(crate) fn apply_json(&mut self, raw: &str) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return false;
        };
        let mut applied = false;
        if let Some(hidden) = value.get("hidden").and_then(|h| h.as_array()) {
            self.hidden.clear();
            for key in hidden.iter().filter_map(|k| k.as_str()) {
                applied |= self.hide(key);
            }
        }
        if let Some(poses) = value.get("poses").and_then(|p| p.as_object()) {
            for (name, pose) in poses {
                let (Some(az), Some(y)) = (
                    pose.get("az").and_then(|v| v.as_f64()),
                    pose.get("y").and_then(|v| v.as_f64()),
                ) else {
                    continue;
                };
                applied |= self.set_pose(name, az as f32, y as f32);
            }
        }
        applied
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
/// Deep workbench: the focused session with a live transcript grows into
/// a reading surface (top stays under the banner line).
pub(crate) const WORKBENCH_DEEP_HALF_W: f32 = 0.44;
pub(crate) const WORKBENCH_DEEP_HALF_H: f32 = 0.33;
pub(crate) const WORKBENCH_DEEP_Y: f32 = 1.30;
/// Transcript typography: row glyph height + vertical pitch (meters).
pub(crate) const TRANSCRIPT_ROW_H: f32 = 0.0165;
pub(crate) const TRANSCRIPT_ROW_PITCH: f32 = 0.0225;
/// Rows a single older/newer page step moves.
pub(crate) const TRANSCRIPT_PAGE_ROWS: usize = 6;

/// Approval banner floats above the workbench line.
pub(crate) const BANNER_DIST: f32 = 1.1;
pub(crate) const BANNER_Y: f32 = 1.78;

/// Monitor stack defaults (the movable-surface anchor: the top monitor's
/// center). Historically inline in `ui.rs::monitors`; lifted here so the
/// layout state can own the default pose.
pub(crate) const MONITORS_AZ: f32 = -0.66; // ≈ −38°: left of the shelf
pub(crate) const MONITORS_DIST: f32 = 1.85;
pub(crate) const MONITORS_TOP_Y: f32 = 1.52;

/// Layout strip: a compact fixed row of visibility toggles, low-front
/// (below the workbench, inside the resting downward glance) — the one
/// surface that never hides, so anything dismissed can come back.
pub(crate) const LAYOUT_STRIP_DIST: f32 = 1.0;
pub(crate) const LAYOUT_STRIP_Y: f32 = 0.84;

/// Terminal pane: a fixed slot on the operator's right, mirroring how
/// the monitor stack sits on the left (same distance, opposite azimuth).
pub(crate) const TERMINAL_AZ: f32 = 0.66;
pub(crate) const TERMINAL_DIST: f32 = 1.85;
pub(crate) const TERMINAL_HALF_W: f32 = 0.60;
pub(crate) const TERMINAL_Y: f32 = 1.42;
/// Height/width fallback before the first painted frame reports the real
/// canvas aspect (80x24 cells at the painter's cell metrics).
pub(crate) const TERMINAL_DEFAULT_ASPECT: f32 = 0.625;
/// Summon pill: nearer and lower than the pane, on the workbench's
/// right sightline.
pub(crate) const TERMINAL_PILL_AZ: f32 = 0.50;
pub(crate) const TERMINAL_PILL_DIST: f32 = 1.30;
pub(crate) const TERMINAL_PILL_Y: f32 = 1.05;

/// Agenda rail: parked intent on the operator's RIGHT, outboard of the
/// terminal slot (the +38° mirror of the monitors went to the summoned
/// pane) — one band further around the ring and a step deeper, so the
/// always-on rail never fights the pane's plane: closed pane leaves the
/// rail fully visible, an open pane cleanly occludes only the rail's
/// nearest edge.
pub(crate) const AGENDA_AZ: f32 = 1.05; // ≈ +60°
pub(crate) const AGENDA_DIST: f32 = 1.95;
/// Top edge of the first card; the stack grows downward.
pub(crate) const AGENDA_TOP_Y: f32 = 1.66;
pub(crate) const AGENDA_CARD_W: f32 = 0.50;
/// Collapsed card height (title line + state line); the selected card
/// grows per wrapped title line.
pub(crate) const AGENDA_CARD_H: f32 = 0.096;
pub(crate) const AGENDA_CARD_GAP: f32 = 0.022;
/// Cards on the rail before the honest "+N more" overflow line.
pub(crate) const AGENDA_RAIL_CAP: usize = 8;

/// A slot on the shelf cylinder: `index` within `count` cards on `row`
/// (0 = top). Returns (center, right, up); the panel normal faces the
/// operator column. Cards are centered on the -Z (initial facing) axis.
pub(crate) fn shelf_slot(index: usize, count: usize, row: usize) -> (Vec3, Vec3, Vec3) {
    let n = count.max(1) as f32;
    let az = (index as f32 - (n - 1.0) / 2.0) * CARD_ARC_STEP;
    let y = SHELF_TOP_ROW_Y - row as f32 * SHELF_ROW_DROP;
    let center = v3(SHELF_RADIUS * az.sin(), y, -SHELF_RADIUS * az.cos());
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

    #[test]
    fn layout_hide_toggle_and_summon() {
        let mut layout = LayoutState::default();
        for name in LAYOUT_SURFACES {
            assert!(!layout.is_hidden(name), "{name} starts visible");
        }
        // Toggle hides, toggle again summons.
        assert!(layout.toggle("agenda"));
        assert!(layout.is_hidden("agenda"));
        assert!(!layout.toggle("agenda"));
        assert!(!layout.is_hidden("agenda"));
        // Close pills hide directly; unknown families are refused.
        assert!(layout.hide("agenda"));
        assert!(layout.is_hidden("agenda"));
        assert!(!layout.hide("workbench"));
        assert!(!layout.toggle("workbench"));
        assert!(!layout.hide("monitor:"), "empty monitor id refused");
    }

    #[test]
    fn monitors_family_toggle_clears_per_monitor_dismissals() {
        let mut layout = LayoutState::default();
        assert!(layout.hide("monitor:local:1"));
        assert!(layout.monitor_hidden("local:1"));
        assert!(!layout.monitor_hidden("local:2"));
        // The family pill reads "something hidden" and summons everything.
        assert!(!layout.toggle("monitors"));
        assert!(!layout.monitor_hidden("local:1"));
        // With nothing hidden it hides the whole stack.
        assert!(layout.toggle("monitors"));
        assert!(layout.monitor_hidden("local:1"));
        assert!(layout.monitor_hidden("local:2"));
    }

    #[test]
    fn poses_default_move_and_clamp() {
        let mut layout = LayoutState::default();
        let d = layout.pose("terminal");
        assert!((d.az - TERMINAL_AZ).abs() < 1e-6);
        assert!((d.y - TERMINAL_Y).abs() < 1e-6);
        assert!(layout.move_by("terminal", -0.2, 0.1));
        let moved = layout.pose("terminal");
        assert!((moved.az - (TERMINAL_AZ - 0.2)).abs() < 1e-5);
        assert!((moved.y - (TERMINAL_Y + 0.1)).abs() < 1e-5);
        // Clamped to the comfortable band.
        assert!(layout.move_by("terminal", 99.0, 99.0));
        let clamped = layout.pose("terminal");
        assert!((clamped.az - SURFACE_AZ_MAX).abs() < 1e-6);
        assert!((clamped.y - SURFACE_Y_MAX).abs() < 1e-6);
        assert!(layout.set_pose("agenda", -99.0, -99.0));
        let low = layout.pose("agenda");
        assert!((low.az - SURFACE_AZ_MIN).abs() < 1e-6);
        assert!((low.y - SURFACE_Y_MIN).abs() < 1e-6);
        // Non-movable / non-finite refused.
        assert!(!layout.move_by("sessions", 0.1, 0.0));
        assert!(!layout.set_pose("terminal", f32::NAN, 1.0));
    }

    #[test]
    fn layout_json_round_trips_and_tolerates_garbage() {
        let mut layout = LayoutState::default();
        layout.hide("agenda");
        layout.hide("monitor:local:1");
        layout.set_pose("terminal", 0.4, 1.2);
        let json = layout.to_json().to_string();

        let mut restored = LayoutState::default();
        assert!(restored.apply_json(&json));
        assert!(restored.is_hidden("agenda"));
        assert!(restored.monitor_hidden("local:1"));
        let p = restored.pose("terminal");
        assert!((p.az - 0.4).abs() < 1e-6 && (p.y - 1.2).abs() < 1e-6);

        let mut junk = LayoutState::default();
        assert!(!junk.apply_json("not json"));
        assert!(!junk.apply_json("{\"hidden\":[\"nope\"],\"poses\":{\"x\":{}}}"));
        // Restored poses clamp like every other write path.
        assert!(junk.apply_json("{\"poses\":{\"terminal\":{\"az\":9,\"y\":-9}}}"));
        let p = junk.pose("terminal");
        assert!((p.az - SURFACE_AZ_MAX).abs() < 1e-6);
        assert!((p.y - SURFACE_Y_MIN).abs() < 1e-6);
    }

    #[test]
    fn hidden_key_cap_bounds_the_set() {
        let mut layout = LayoutState::default();
        for i in 0..200 {
            layout.hide(&format!("monitor:m{i}"));
        }
        assert!(layout.to_json()["hidden"].as_array().unwrap().len() <= 64);
    }
}
