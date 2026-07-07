//! HUD overlay: 2D canvas panels, draw primitives, and the memoized
//! style wrapper.

use std::cell::RefCell;
use std::collections::HashMap;

use web_sys::CanvasRenderingContext2d;

use crate::input::{HitAction, HitZone, ViewSliderKey};
use crate::model::activity_retained_count;
use crate::scene::{ndc_to_screen, LayoutName, Mood, NodeKind, ProjectedNode, Vec2};
use crate::util::{
    attention_level_color_css, css_rgba, epoch_seconds_now, fmt_compact, fmt_countdown,
    goal_status_color_css, hex_color, level_color_css, nonempty, pct_label, percent,
    phase_color_css, pressure_color, tone_color_css, truncate, Color, C_BLUE, C_BLUE_CSS,
    C_GREEN_CSS, C_LAVENDER, C_LAVENDER_CSS, C_MAUVE_CSS, C_OVERLAY1, C_OVERLAY1_CSS, C_PEACH,
    C_PEACH_CSS, C_RED_CSS, C_SUBTEXT0_CSS, C_TEAL, C_TEAL_CSS, C_TEXT_CSS, C_YELLOW_CSS,
};
use crate::StationInner;

/// The HUD 2D context plus memoized style state. Canvas style setters are
/// expensive to spam and the HUD repeats the same handful of fills, strokes,
/// and fonts hundreds of times per frame, so each setter only touches the
/// context when the value actually changes. Font strings are interned per
/// (size, weight). Interior mutability keeps the draw helpers callable
/// through `&self`.
pub(crate) struct Hud {
    pub(crate) ctx: CanvasRenderingContext2d,
    pub(crate) style: RefCell<HudStyle>,
}

mod stage;
mod panels;
mod focus;
mod widgets;

#[derive(Default)]
pub(crate) struct HudStyle {
    pub(crate) fill: String,
    pub(crate) stroke: String,
    pub(crate) font: (u32, bool),
    pub(crate) fonts: HashMap<(u32, bool), String>,
    pub(crate) vignette: Option<Vignette>,
}

pub(crate) struct Vignette {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) mood: Mood,
    pub(crate) gradient: web_sys::CanvasGradient,
}

impl Hud {
    pub(crate) fn new(ctx: CanvasRenderingContext2d) -> Self {
        Self {
            ctx,
            style: RefCell::new(HudStyle::default()),
        }
    }

    pub(crate) fn set_fill(&self, css: &str) {
        let mut style = self.style.borrow_mut();
        if style.fill != css {
            style.fill.clear();
            style.fill.push_str(css);
            self.ctx.set_fill_style_str(css);
        }
    }

    pub(crate) fn set_stroke(&self, css: &str) {
        let mut style = self.style.borrow_mut();
        if style.stroke != css {
            style.stroke.clear();
            style.stroke.push_str(css);
            self.ctx.set_stroke_style_str(css);
        }
    }

    pub(crate) fn set_font(&self, px: f32, bold: bool) {
        let key = ((px * 10.0).round() as u32, bold);
        let mut style = self.style.borrow_mut();
        if style.font == key {
            return;
        }
        style.font = key;
        let font = style.fonts.entry(key).or_insert_with(|| {
            format!(
                "{} {px}px 'SF Mono', Menlo, Consolas, monospace",
                if bold { "bold" } else { "normal" }
            )
        });
        self.ctx.set_font(font);
    }

    /// The fill was set to a non-string paint (e.g. a gradient) behind the
    /// memo's back; force the next `set_fill` through.
    pub(crate) fn note_fill_unknown(&self) {
        self.style.borrow_mut().fill.clear();
    }

    /// Radial vignette gradient, rebuilt only when the size or mood changes.
    pub(crate) fn vignette(&self, w: f32, h: f32, mood: Mood) -> Option<web_sys::CanvasGradient> {
        let mut style = self.style.borrow_mut();
        if let Some(v) = style.vignette.as_ref() {
            if v.width == w && v.height == h && v.mood == mood {
                return Some(v.gradient.clone());
            }
        }
        let gradient = self
            .ctx
            .create_radial_gradient(
                (w / 2.0) as f64,
                (h / 2.0) as f64,
                20.0,
                (w / 2.0) as f64,
                (h / 2.0) as f64,
                (w.max(h) * 0.72) as f64,
            )
            .ok()?;
        for (offset, color) in mood.vignette_stops() {
            let _ = gradient.add_color_stop(offset as f32, color);
        }
        style.vignette = Some(Vignette {
            width: w,
            height: h,
            mood,
            gradient: gradient.clone(),
        });
        Some(gradient)
    }

    pub(crate) fn invalidate_vignette(&self) {
        self.style.borrow_mut().vignette = None;
    }

    /// Forget memoized style state after the real context state was reset
    /// (canvas resize) or mutated outside the memo (scene underlay).
    pub(crate) fn invalidate_styles(&self) {
        let mut style = self.style.borrow_mut();
        style.fill.clear();
        style.stroke.clear();
        style.font = (0, false);
    }

    /// Full reset: styles and the size-dependent vignette.
    pub(crate) fn invalidate(&self) {
        self.invalidate_styles();
        self.invalidate_vignette();
    }
}

/// Thumbnail placement rect in CSS px: `(x, y, w, h)`.
pub(crate) type ThumbRect = (f32, f32, f32, f32);

/// Activity-lane metrics for a density setting: `(rows, row_pitch,
/// lane_height)`. Density meaningfully packs the HUD: 0.5 shows 2 event
/// rows, 1.0 the classic 3, 1.8 up to 5 (with a tighter pitch). Short
/// panes cap at 3 so the lane never eats the scene. At the default
/// density the legacy 78/68px lane height is preserved exactly.
pub(crate) fn lane_metrics(density: f32, h: f32) -> (usize, f32, f32) {
    let mut rows = (3.0 * density).round() as i32;
    if h < 640.0 {
        rows = rows.min(3);
    }
    let rows = rows.clamp(2, 5) as usize;
    let pitch = if rows > 3 { 15.5 } else { 18.0 };
    let base = if h < 640.0 { 68.0 } else { 78.0 };
    (rows, pitch, base + (rows as f32 - 3.0) * pitch)
}

/// Compact (narrow) surface tile grid for a density setting and panel
/// height: `(tile_count, row_pitch, tile_height)`. The strip previously
/// hard-dropped the 9th system target; now all nine fit whenever the
/// panel has the rows for them, wrapping two per row. Density shrinks the
/// pitch (more rows fit) and scales how many tiles are wanted — sparse
/// 0.5 shows ~5, the default 1.0 all nine at the legacy 58px pitch.
pub(crate) fn compact_grid(density: f32, panel_h: f32) -> (usize, f32, f32) {
    let pitch = (58.0 / density.max(0.5)).clamp(40.0, 72.0);
    let rows = (((panel_h - 66.0) / pitch).floor() as i32).max(1) as usize;
    let preferred = ((9.0 * density).round() as i32).clamp(4, 9) as usize;
    (preferred.min(rows * 2), pitch, pitch - 10.0)
}

/// Uniform row height in the scrollable focus panels.
pub(crate) const PANEL_ROW_H: f32 = 30.0;
/// Line pitch in the transcript viewer.
pub(crate) const TRANSCRIPT_LINE_H: f32 = 13.0;

/// One header pill in a rows panel (panel-wide operation).
pub(crate) struct HeaderPill {
    pub(crate) label: String,
    pub(crate) color: &'static str,
    pub(crate) active: bool,
    pub(crate) action: HitAction,
}

impl HeaderPill {
    pub(crate) fn new(label: &str, color: &'static str, action: HitAction) -> Self {
        Self {
            label: label.to_string(),
            color,
            active: false,
            action,
        }
    }

    pub(crate) fn new_owned(label: String, color: &'static str, action: HitAction) -> Self {
        Self {
            label,
            color,
            active: false,
            action,
        }
    }
}

/// One pill attached to a panel row.
pub(crate) struct RowPill {
    pub(crate) label: String,
    pub(crate) color: &'static str,
    pub(crate) action: HitAction,
}

/// One row in a scrollable focus panel.
pub(crate) struct PanelRow {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) color: &'static str,
    pub(crate) click: Option<HitAction>,
    pub(crate) pills: Vec<RowPill>,
    /// When non-empty the row renders choice pills instead of value text
    /// (autonomy / backend / toggle rows).
    pub(crate) choices: Vec<(String, bool, HitAction)>,
}

impl PanelRow {
    pub(crate) fn new(label: String, value: String, color: &'static str) -> Self {
        Self {
            label,
            value,
            color,
            click: None,
            pills: Vec::new(),
            choices: Vec::new(),
        }
    }

    pub(crate) fn choices(
        label: &str,
        color: &'static str,
        choices: Vec<(String, bool, HitAction)>,
    ) -> Self {
        Self {
            label: label.to_string(),
            value: String::new(),
            color,
            click: None,
            pills: Vec::new(),
            choices,
        }
    }

    pub(crate) fn click(mut self, action: HitAction) -> Self {
        self.click = Some(action);
        self
    }

    pub(crate) fn pill(mut self, label: &str, color: &'static str, action: HitAction) -> Self {
        self.pills.push(RowPill {
            label: label.to_string(),
            color,
            action,
        });
        self
    }
}

/// Everything a rows panel shows: header pills + rows + empty-state hint.
#[derive(Default)]
pub(crate) struct PanelSurface {
    pub(crate) header: Vec<HeaderPill>,
    pub(crate) rows: Vec<PanelRow>,
    pub(crate) empty: &'static str,
}

/// Cached wrapped-line layout for the transcript viewer.
pub(crate) struct TranscriptLayout {
    pub(crate) sig: u64,
    pub(crate) lines: Vec<TranscriptLine>,
    pub(crate) content_h: f32,
}

pub(crate) struct TranscriptLine {
    pub(crate) y: f32,
    pub(crate) kind: String,
    pub(crate) ts: String,
    pub(crate) text: String,
    pub(crate) first: bool,
}

/// Cheap content signature for the transcript layout cache: row count,
/// total text length, wrap width, and the tail row's length (catches
/// in-place tail growth).
pub(crate) fn transcript_signature(
    transcript: &crate::model::StationTranscript,
    wrap_chars: u32,
) -> u64 {
    let mut sig = transcript.rows.len() as u64;
    sig = sig
        .wrapping_mul(31)
        .wrapping_add(transcript.session_id.len() as u64);
    sig = sig.wrapping_mul(31).wrapping_add(wrap_chars as u64);
    let total: u64 = transcript.rows.iter().map(|r| r.text.len() as u64).sum();
    sig = sig.wrapping_mul(31).wrapping_add(total);
    if let Some(last) = transcript.rows.last() {
        sig = sig.wrapping_mul(31).wrapping_add(last.text.len() as u64);
    }
    sig
}

/// Word-wrap every transcript row into draw lines with precomputed y
/// offsets. Rows are separated by a small gap; the first line of a row
/// carries its kind/ts gutter.
pub(crate) fn layout_transcript(
    transcript: &crate::model::StationTranscript,
    wrap_chars: usize,
) -> TranscriptLayout {
    let mut lines = Vec::new();
    let mut y = TRANSCRIPT_LINE_H;
    for row in &transcript.rows {
        let mut first = true;
        for raw_line in row.text.lines().filter(|l| !l.trim().is_empty()) {
            for piece in wrap_line(raw_line.trim_end(), wrap_chars) {
                lines.push(TranscriptLine {
                    y,
                    kind: row.kind.clone(),
                    ts: row.ts.clone(),
                    text: piece,
                    first,
                });
                first = false;
                y += TRANSCRIPT_LINE_H;
            }
        }
        if first {
            // Whitespace-only payload: keep the row visible as its kind.
            lines.push(TranscriptLine {
                y,
                kind: row.kind.clone(),
                ts: row.ts.clone(),
                text: String::new(),
                first: true,
            });
            y += TRANSCRIPT_LINE_H;
        }
        y += 5.0;
    }
    TranscriptLayout {
        sig: 0,
        lines,
        content_h: y,
    }
}

/// Greedy word wrap with hard breaks for words longer than the width.
pub(crate) fn wrap_line(line: &str, max_chars: usize) -> Vec<String> {
    let max = max_chars.max(8);
    let mut out = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        let word_len = word.chars().count();
        let cur_len = current.chars().count();
        if cur_len == 0 && word_len <= max {
            current.push_str(word);
        } else if cur_len + 1 + word_len <= max {
            current.push(' ');
            current.push_str(word);
        } else {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            if word_len <= max {
                current.push_str(word);
            } else {
                // Hard-break an overlong token (path, hash, minified blob).
                let mut chunk = String::with_capacity(max);
                for ch in word.chars() {
                    chunk.push(ch);
                    if chunk.chars().count() == max {
                        out.push(std::mem::take(&mut chunk));
                    }
                }
                current = chunk;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Color for a transcript row kind.
pub(crate) fn transcript_kind_color(kind: &str) -> &'static str {
    match kind {
        "user" => C_GREEN_CSS,
        "model" | "assistant" => C_BLUE_CSS,
        "agent" | "run" => C_TEAL_CSS,
        "tool" | "command" | "detail" => C_LAVENDER_CSS,
        "error" | "diff-del" => C_RED_CSS,
        "warn" | "diff-meta" => C_YELLOW_CSS,
        "diff-add" => C_GREEN_CSS,
        _ => C_SUBTEXT0_CSS,
    }
}

pub(crate) struct LaneAction {
    pub(crate) label: &'static str,
    pub(crate) width: f32,
    pub(crate) color: &'static str,
    pub(crate) hit: HitAction,
}

/// One control-center summary tile, derived from the snapshot. Rebuilt
/// only when the underlying state changes, then reused across frames.
pub(crate) struct SystemTarget {
    pub(crate) id: &'static str,
    pub(crate) kicker: &'static str,
    pub(crate) title: &'static str,
    pub(crate) value: String,
    pub(crate) detail: String,
    pub(crate) color: &'static str,
}

impl LaneAction {
    pub(crate) fn select(
        label: &'static str,
        id: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::Select(id.to_string()),
        }
    }

    pub(crate) fn activity(
        label: &'static str,
        action: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ActivityAction {
                action: action.to_string(),
                id: String::new(),
            },
        }
    }

    pub(crate) fn controls(
        label: &'static str,
        action: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ControlsAction {
                action: action.to_string(),
            },
        }
    }

    pub(crate) fn composer(
        label: &'static str,
        op: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::Composer { op },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_metrics_scale_rows_with_density() {
        // Default density keeps the legacy geometry exactly.
        assert_eq!(lane_metrics(1.0, 900.0), (3, 18.0, 78.0));
        assert_eq!(lane_metrics(1.0, 600.0), (3, 18.0, 68.0));
        // Sparse and dense settings change the row count and lane size.
        let (rows, _, height) = lane_metrics(0.5, 900.0);
        assert_eq!(rows, 2);
        assert!(height < 78.0);
        let (rows, pitch, height) = lane_metrics(1.8, 900.0);
        assert_eq!(rows, 5);
        assert!(pitch < 18.0);
        assert!(height > 78.0);
        // Short panes cap the row count so the lane can't eat the scene.
        assert_eq!(lane_metrics(1.8, 600.0).0, 3);
        // Rows always fit inside the lane: first row at +43, pitch apart.
        for (density, h) in [(0.5, 900.0), (1.0, 900.0), (1.4, 900.0), (1.8, 900.0)] {
            let (rows, pitch, height) = lane_metrics(density, h);
            let last_row = 43.0 + (rows as f32 - 1.0) * pitch;
            assert!(
                last_row <= height + 3.0,
                "density {density}: row {last_row} vs lane {height}"
            );
        }
    }

    #[test]
    fn transcript_kind_colors_cover_the_vocabulary() {
        for kind in [
            "user",
            "model",
            "assistant",
            "agent",
            "tool",
            "error",
            "warn",
            "diff-add",
            "diff-del",
            "diff-meta",
            "info",
            "",
        ] {
            assert!(!transcript_kind_color(kind).is_empty());
        }
    }

    #[test]
    fn compact_grid_fits_all_nine_targets_by_default() {
        // Tall pane at default density: every system target is reachable,
        // at the legacy 58px pitch / 48px tile.
        let (count, pitch, tile_h) = compact_grid(1.0, 700.0);
        assert_eq!((count, pitch, tile_h), (9, 58.0, 48.0));
        // Sparse density prefers fewer tiles; dense packs tighter rows.
        assert!(compact_grid(0.5, 700.0).0 < 9);
        let (count, pitch, _) = compact_grid(1.8, 700.0);
        assert_eq!(count, 9);
        assert!(pitch < 58.0);
        // Short panes cap at what actually fits instead of overflowing.
        let (count, pitch, _) = compact_grid(1.0, 200.0);
        assert!(count <= ((200.0 - 66.0) / pitch) as usize * 2);
        assert!(count >= 2);
        // Never more than the nine system targets.
        assert!(compact_grid(5.0, 2000.0).0 <= 9);
    }
}
