//! Glyph atlas for in-scene text (Phase C slice 3). The HUD font is baked
//! once into an offscreen 2D canvas and its alpha channel uploaded as an
//! R8 coverage texture — the crate's first texture + sampler bind group
//! (`gpu::GpuState::ensure_atlas`). World-space pane text is laid out here
//! as textured quads sampled from that atlas by the text pipeline. The
//! 2D-canvas HUD (`hud::text`) stays the screen-space presentation and the
//! Canvas-renderer fallback; this module only serves world-space panes.
//!
//! Pane text draws well below the baked glyph size at typical camera
//! distances, so the upload carries a full CPU-generated mip chain
//! (`mip_chain`) — bilinear-only sampling at 3–4× minification visibly
//! drops thin strokes. Magnification past the bake stays plain bilinear
//! (soft, rare); revisit with an SDF bake if close-up text ever matters.

use std::collections::HashMap;

use wasm_bindgen::{JsCast, JsValue};

use crate::gpu::{GpuFrame, TextVertex};
use crate::scene::{Vec2, Vec3};
use crate::util::Color;

/// Bake size in atlas px. Pane text draws at roughly half this on screen
/// at the default camera distance, so bilinear sampling stays in its
/// comfort zone in both directions.
const FONT_PX: f32 = 24.0;
/// Glyph cell height in atlas px: the em plus leading, with room for
/// ascenders and descenders around the baseline.
const CELL_H: f32 = 32.0;
/// Baseline offset from the cell top (0.75 × cell: ~0.8 em of ascent fits
/// above, ~0.25 em of descent below).
const BASELINE: f32 = 24.0;
/// Transparent padding around each glyph slot. Bilinear sampling at a quad
/// edge reads at most one texel beyond it, so 2px isolates neighbors.
const PAD: f32 = 2.0;
/// Fixed atlas width; glyph rows wrap into it.
const ATLAS_W: u32 = 1024;

/// Everything pane text renders today: printable ASCII plus the ellipsis
/// `fit_to_width` appends.
fn charset() -> impl Iterator<Item = char> {
    (32u8..=126).map(char::from).chain(['…'])
}

/// Clip-depth bias (toward the camera) applied to every text quad. Text
/// shares its pane's plane, but the rasterizer interpolates depth linearly
/// in screen space per triangle, and the pane's triangulation disagrees
/// with a glyph quad's by the plane's perspective nonlinearity — exact
/// depth equality can't be trusted. The bias sits well above that error
/// and well below any real inter-object spacing.
pub(crate) const TEXT_DEPTH_BIAS: f32 = 0.0015;

#[derive(Clone, Copy)]
pub(crate) struct Glyph {
    /// Normalized atlas UV rect of the padded slot.
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    /// Horizontal advance in atlas px (excludes padding).
    advance: f32,
    /// Slot width in atlas px (advance plus both pads) — the quad's width.
    slot_w: f32,
}

pub(crate) struct TextAtlas {
    glyphs: HashMap<char, Glyph>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// R8 coverage bytes (the baked canvas's alpha channel), row-major.
    pub(crate) pixels: Vec<u8>,
}

impl TextAtlas {
    pub(crate) fn from_parts(
        glyphs: HashMap<char, Glyph>,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Self {
        Self {
            glyphs,
            width,
            height,
            pixels,
        }
    }

    /// Bake the charset with the HUD font stack into an offscreen canvas
    /// and keep its alpha channel as coverage. Compiles on every target
    /// (web-sys stubs on native) but only runs in the browser; native code
    /// never constructs `StationInner`.
    pub(crate) fn bake() -> Result<Self, JsValue> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| JsValue::from_str("no document for the atlas canvas"))?;
        let canvas = document
            .create_element("canvas")?
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .map_err(|_| JsValue::from_str("atlas canvas creation failed"))?;
        let ctx = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("atlas canvas has no 2D context"))?
            .dyn_into::<web_sys::CanvasRenderingContext2d>()
            .map_err(|_| JsValue::from_str("atlas context is not 2D"))?;
        // Same stack as Hud::set_font, so world panes match the HUD type.
        let font = format!("{FONT_PX}px 'SF Mono', Menlo, Consolas, monospace");
        ctx.set_font(&font);

        // Measure and pack first; the canvas is sized to the packing after
        // (sizing resets all context state, so the font is set again below).
        let mut slots: Vec<(char, f32, f32, f32)> = Vec::new();
        let (mut x, mut y) = (0.0f32, 0.0f32);
        for ch in charset() {
            let advance = ctx.measure_text(&ch.to_string())?.width() as f32;
            let slot_w = (advance + PAD * 2.0).ceil();
            if x + slot_w > ATLAS_W as f32 {
                x = 0.0;
                y += CELL_H;
            }
            slots.push((ch, x, y, advance));
            x += slot_w;
        }
        let height = (y + CELL_H) as u32;
        canvas.set_width(ATLAS_W);
        canvas.set_height(height);
        ctx.set_font(&font);
        ctx.set_fill_style_str("#ffffff");

        let mut glyphs = HashMap::new();
        for (ch, x, y, advance) in slots {
            let slot_w = (advance + PAD * 2.0).ceil();
            let _ = ctx.fill_text(&ch.to_string(), (x + PAD) as f64, (y + BASELINE) as f64);
            glyphs.insert(
                ch,
                Glyph {
                    u0: x / ATLAS_W as f32,
                    v0: y / height as f32,
                    u1: (x + slot_w) / ATLAS_W as f32,
                    v1: (y + CELL_H) / height as f32,
                    advance,
                    slot_w,
                },
            );
        }
        let rgba = ctx
            .get_image_data(0.0, 0.0, ATLAS_W as f64, height as f64)?
            .data();
        let pixels = rgba.chunks_exact(4).map(|px| px[3]).collect();
        Ok(Self::from_parts(glyphs, ATLAS_W, height, pixels))
    }

    /// A char's glyph, falling back to `?` so unknown input renders as a
    /// visible placeholder instead of silently shifting layout.
    fn lookup(&self, ch: char) -> Option<Glyph> {
        self.glyphs
            .get(&ch)
            .or_else(|| self.glyphs.get(&'?'))
            .copied()
    }

    /// Width of `text` in atlas px (advance sum, fallback included —
    /// matching what layout draws).
    pub(crate) fn measure(&self, text: &str) -> f32 {
        text.chars()
            .filter_map(|ch| self.lookup(ch))
            .map(|glyph| glyph.advance)
            .sum()
    }

    /// Width of `text` in world units when drawn at cell height `height`.
    pub(crate) fn measure_world(&self, text: &str, height: f32) -> f32 {
        self.measure(text) * height / CELL_H
    }

    /// The full mip pyramid for the coverage texture, base level included:
    /// `(width, height, pixels)` per level down to 1×1. Box-filtered on
    /// the CPU — a render-pass generator is overkill for a small R8 mask.
    /// (Only the wasm-only `GpuState::ensure_atlas` consumes it outside
    /// tests, so the native lint sees it as dead.)
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn mip_chain(&self) -> Vec<(u32, u32, Vec<u8>)> {
        let mut levels = vec![(self.width, self.height, self.pixels.clone())];
        while let Some((w, h, pixels)) = levels.last() {
            if *w == 1 && *h == 1 {
                break;
            }
            levels.push(downsample(*w, *h, pixels));
        }
        levels
    }

    /// `text` unchanged when it fits `max_w` world units at `height`;
    /// otherwise truncated with a trailing ellipsis to fit.
    pub(crate) fn fit_to_width(&self, text: &str, height: f32, max_w: f32) -> String {
        if self.measure_world(text, height) <= max_w {
            return text.to_string();
        }
        let mut kept: Vec<char> = text.chars().collect();
        while kept.pop().is_some() {
            let candidate = kept.iter().collect::<String>() + "…";
            if kept.is_empty() || self.measure_world(&candidate, height) <= max_w {
                return candidate;
            }
        }
        "…".to_string()
    }
}

/// One mip step: halve both dimensions (floor at 1), each output texel
/// the box average of its source block. Odd tails fold into the last
/// output texel's block, so no source row/column is dropped.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn downsample(w: u32, h: u32, pixels: &[u8]) -> (u32, u32, Vec<u8>) {
    let out_w = (w / 2).max(1);
    let out_h = (h / 2).max(1);
    let mut out = Vec::with_capacity((out_w * out_h) as usize);
    for oy in 0..out_h {
        let y0 = oy * 2;
        let y1 = if oy == out_h - 1 { h } else { y0 + 2 };
        for ox in 0..out_w {
            let x0 = ox * 2;
            let x1 = if ox == out_w - 1 { w } else { x0 + 2 };
            let mut sum = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += pixels[(y * w + x) as usize] as u32;
                }
            }
            let count = (y1 - y0) * (x1 - x0);
            out.push((sum / count.max(1)) as u8);
        }
    }
    (out_w, out_h, out)
}

/// Lay `text` out as textured glyph quads along the camera basis:
/// `origin` is the top-left of the first glyph cell, `height` the world
/// height of one cell (text flows along `right`, cells extend down along
/// `-up`). Corners project independently and carry their own biased clip
/// depth, same rules as `panes::add_world_pane`; a culled corner drops the
/// whole string — pane text is either fully readable or absent. Returns
/// whether anything was drawn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_text_world(
    frame: &mut GpuFrame,
    atlas: &TextAtlas,
    project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
    right: Vec3,
    up: Vec3,
    origin: Vec3,
    height: f32,
    text: &str,
    color: Color,
) -> bool {
    let scale = height / CELL_H;
    let rgba: [f32; 4] = color.into();
    let start = frame.text_vertices.len();
    let mut pen = 0.0f32;
    for ch in text.chars() {
        let Some(glyph) = atlas.lookup(ch) else {
            continue; // atlas without even a fallback glyph: skip the char
        };
        if ch == ' ' {
            pen += glyph.advance;
            continue;
        }
        let x0 = (pen - PAD) * scale;
        let x1 = x0 + glyph.slot_w * scale;
        let corners = [
            (origin + right * x0 - up * height, glyph.u0, glyph.v1),
            (origin + right * x1 - up * height, glyph.u1, glyph.v1),
            (origin + right * x1, glyph.u1, glyph.v0),
            (origin + right * x0, glyph.u0, glyph.v0),
        ];
        let mut quad = [(Vec2::new(0.0, 0.0), 0.0f32, 0.0f32, 0.0f32); 4];
        for (slot, (world, u, v)) in quad.iter_mut().zip(corners) {
            match project(world) {
                Some((ndc, _cue, depth)) => {
                    *slot = (ndc, (depth - TEXT_DEPTH_BIAS).max(0.0), u, v)
                }
                None => {
                    frame.text_vertices.truncate(start);
                    return false;
                }
            }
        }
        let [bl, br, tr, tl] = quad;
        for (ndc, depth, u, v) in [bl, br, tr, bl, tr, tl] {
            frame.text_vertices.push(TextVertex {
                pos: [ndc.x, ndc.y],
                depth,
                uv: [u, v],
                color: rgba,
            });
        }
        pen += glyph.advance;
    }
    frame.text_vertices.len() > start
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic atlas: two visible glyphs, a space, and the `?` fallback
    /// on a 100×50 canvas, with distinct advances so layout math shows.
    fn atlas() -> TextAtlas {
        let glyph = |x: f32, advance: f32| Glyph {
            u0: x / 100.0,
            v0: 0.0,
            u1: (x + advance + PAD * 2.0) / 100.0,
            v1: CELL_H / 50.0,
            advance,
            slot_w: advance + PAD * 2.0,
        };
        let mut glyphs = HashMap::new();
        glyphs.insert('a', glyph(0.0, 10.0));
        glyphs.insert('b', glyph(20.0, 14.0));
        glyphs.insert(' ', glyph(40.0, 8.0));
        glyphs.insert('?', glyph(60.0, 12.0));
        TextAtlas::from_parts(glyphs, 100, 50, vec![0; 100 * 50])
    }

    fn basis() -> (Vec3, Vec3) {
        (Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0))
    }

    #[test]
    fn layout_advances_pen_and_maps_uvs() {
        let atlas = atlas();
        let mut frame = GpuFrame::default();
        let (right, up) = basis();
        // Identity-ish projector at constant clip depth; height = CELL_H
        // makes world units equal atlas px.
        let mut project = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.5));
        let drew = add_text_world(
            &mut frame,
            &atlas,
            &mut project,
            right,
            up,
            Vec3::ZERO,
            CELL_H,
            "ab",
            crate::util::C_TEXT,
        );
        assert!(drew);
        assert_eq!(frame.text_vertices.len(), 12);
        // 'a' slot spans [-PAD, -PAD + 14]; 'b' starts at pen 10.
        let bl_a = &frame.text_vertices[0];
        assert_eq!(bl_a.pos, [-PAD, -CELL_H]);
        let tl_a = &frame.text_vertices[5];
        assert_eq!(tl_a.pos, [-PAD, 0.0]);
        let bl_b = &frame.text_vertices[6];
        assert_eq!(bl_b.pos, [10.0 - PAD, -CELL_H]);
        // Triangle order [bl, br, tr, bl, tr, tl]: bottom corners sample
        // v1, top corners v0; left corners u0, right u1.
        assert_eq!(bl_a.uv, [0.0, CELL_H / 50.0]);
        assert_eq!(tl_a.uv, [0.0, 0.0]);
        let tr_a = &frame.text_vertices[2];
        assert_eq!(tr_a.uv, [14.0 / 100.0, 0.0]);
        // Every corner carries the biased clip depth.
        assert!(frame
            .text_vertices
            .iter()
            .all(|v| (v.depth - (0.5 - TEXT_DEPTH_BIAS)).abs() < 1e-6));
    }

    #[test]
    fn spaces_advance_without_quads() {
        let atlas = atlas();
        let mut frame = GpuFrame::default();
        let (right, up) = basis();
        let mut project = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.5));
        add_text_world(
            &mut frame,
            &atlas,
            &mut project,
            right,
            up,
            Vec3::ZERO,
            CELL_H,
            "a b",
            crate::util::C_TEXT,
        );
        // Two glyph quads; the space only moved the pen (10 + 8).
        assert_eq!(frame.text_vertices.len(), 12);
        assert_eq!(frame.text_vertices[6].pos, [18.0 - PAD, -CELL_H]);
    }

    #[test]
    fn unknown_chars_fall_back_to_question_mark() {
        let atlas = atlas();
        let mut frame = GpuFrame::default();
        let (right, up) = basis();
        let mut project = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.5));
        add_text_world(
            &mut frame,
            &atlas,
            &mut project,
            right,
            up,
            Vec3::ZERO,
            CELL_H,
            "Z",
            crate::util::C_TEXT,
        );
        assert_eq!(frame.text_vertices.len(), 6);
        // The quad samples the '?' slot (u0 = 60/100).
        assert_eq!(frame.text_vertices[0].uv[0], 0.6);
    }

    #[test]
    fn culled_corner_drops_the_whole_string() {
        let atlas = atlas();
        let mut frame = GpuFrame::default();
        let (right, up) = basis();
        // Cull anything right of x = 20: 'a' projects fine, 'b' does not —
        // the already-pushed 'a' quad must be retracted too.
        let mut project = |v: Vec3| (v.x <= 20.0).then(|| (Vec2::new(v.x, v.y), 1.0, 0.5));
        let drew = add_text_world(
            &mut frame,
            &atlas,
            &mut project,
            right,
            up,
            Vec3::ZERO,
            CELL_H,
            "ab",
            crate::util::C_TEXT,
        );
        assert!(!drew);
        assert!(frame.text_vertices.is_empty());
    }

    #[test]
    fn measure_and_fit_to_width() {
        let atlas = atlas();
        assert_eq!(atlas.measure("ab"), 24.0);
        assert_eq!(atlas.measure_world("ab", CELL_H * 2.0), 48.0);
        // Fits: unchanged.
        assert_eq!(atlas.fit_to_width("ab", CELL_H, 24.0), "ab");
        // Too narrow: truncated with the ellipsis (which measures via the
        // '?' fallback here) and actually fitting.
        let fitted = atlas.fit_to_width("abab", CELL_H, 30.0);
        assert!(fitted.ends_with('…'), "got {fitted:?}");
        assert!(atlas.measure_world(&fitted, CELL_H) <= 30.0);
        assert!(fitted.chars().count() < 4);
    }

    #[test]
    fn downsample_box_filters_and_folds_odd_tails() {
        // 4×2 → 2×1: each output texel averages its 2×2 block.
        let (w, h, out) = downsample(4, 2, &[0, 4, 8, 12, 0, 4, 8, 12]);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![2, 10]);
        // 3×1 → 1×1: the odd tail folds into the last block instead of
        // being dropped.
        let (w, h, out) = downsample(3, 1, &[3, 6, 9]);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![6]);
    }

    #[test]
    fn mip_chain_matches_webgpu_level_dims() {
        // 8×3 atlas: WebGPU expects level i to be (max(1, w>>i),
        // max(1, h>>i)) and at most floor(log2(max)) + 1 levels.
        let atlas = TextAtlas::from_parts(HashMap::new(), 8, 3, vec![128; 24]);
        let chain = atlas.mip_chain();
        let dims: Vec<(u32, u32)> = chain.iter().map(|(w, h, _)| (*w, *h)).collect();
        assert_eq!(dims, vec![(8, 3), (4, 1), (2, 1), (1, 1)]);
        for (w, h, pixels) in &chain {
            assert_eq!(pixels.len(), (*w * *h) as usize);
        }
        // A uniform base stays uniform through every level.
        assert!(chain.iter().all(|(_, _, p)| p.iter().all(|px| *px == 128)));
    }

    #[test]
    fn empty_text_draws_nothing() {
        let atlas = atlas();
        let mut frame = GpuFrame::default();
        let (right, up) = basis();
        let mut project = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.5));
        let drew = add_text_world(
            &mut frame,
            &atlas,
            &mut project,
            right,
            up,
            Vec3::ZERO,
            CELL_H,
            "",
            crate::util::C_TEXT,
        );
        assert!(!drew);
        assert!(frame.text_vertices.is_empty());
    }
}
