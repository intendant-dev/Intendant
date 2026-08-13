//! Glyph atlas: the dashboard's type, baked for world-space text.
//!
//! Same technique the other rendered surface proved (bake once via a 2D
//! canvas, upload the coverage as a single-channel texture, sample with
//! trilinear mips), retargeted at WebGL2. Baked at 48 px in the ui-v2 UI
//! face so near-field text stays crisp at headset pixel densities; glyph
//! quads carry per-run color, so one atlas serves every text role.
//!
//! ASCII 32..=126 plus the scene's own punctuation ([`SPECIAL_GLYPHS`]).
//! Unknown characters fall back to '?' — the feed is UTF-8 but the
//! operator surface vocabulary is ASCII-dominant; full shaping is
//! explicitly out of scope for milestone 1.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::WebGl2RenderingContext as Gl;

use crate::kit::{TextAlign, TextRun};

/// Baked font size in canvas px; text height in meters scales off this.
const FONT_PX: f32 = 48.0;
/// Atlas cell height: room for ascenders + descenders at 48 px.
const CELL_H: u32 = 64;
/// Baseline offset from the cell top.
const BASELINE_PX: f32 = 46.0;
/// Horizontal pad inside each cell (px) so mips never bleed neighbors.
const CELL_PAD: f32 = 3.0;
const COLS: u32 = 16;
/// The ui-v2 UI face with honest fallbacks (the dashboard self-hosts
/// Hanken Grotesk; inside the same page the atlas bake sees it loaded).
const FONT_SPEC: &str = "600 48px 'Hanken Grotesk', ui-sans-serif, system-ui, sans-serif";

const GLYPH_FIRST: u32 = 32; // ' '
const GLYPH_LAST: u32 = 126; // '~'
/// Non-ASCII punctuation the scene vocabulary actually uses (host-row
/// and status separators, truncation, the terminal pane's copy) — baked
/// after the ASCII range so it never hits the '?' fallback. Everything
/// else still does; full shaping stays out of scope.
const SPECIAL_GLYPHS: [char; 3] = ['…', '—', '·'];
const ELLIPSIS_INDEX: usize = (GLYPH_LAST - GLYPH_FIRST + 1) as usize;
const GLYPH_COUNT: usize = ELLIPSIS_INDEX + SPECIAL_GLYPHS.len();

/// Measurement abstraction so layout code (and its host tests) never
/// needs a live atlas.
pub(crate) trait TextMeasure {
    /// Width in meters of `text` rendered at `height` meters.
    fn measure(&self, text: &str, height: f32) -> f32;
}

/// Fixed-advance stand-in for host tests and pre-atlas layout guesses.
pub(crate) struct ApproxMeasure;

impl TextMeasure for ApproxMeasure {
    fn measure(&self, text: &str, height: f32) -> f32 {
        text.chars().count() as f32 * height * 0.52
    }
}

pub(crate) struct GlyphAtlas {
    texture: web_sys::WebGlTexture,
    advances_px: Vec<f32>,
    cell_w: f32,
    tex_w: f32,
    tex_h: f32,
}

fn glyph_index(c: char) -> usize {
    let code = c as u32;
    if (GLYPH_FIRST..=GLYPH_LAST).contains(&code) {
        (code - GLYPH_FIRST) as usize
    } else if let Some(i) = SPECIAL_GLYPHS.iter().position(|&s| s == c) {
        ELLIPSIS_INDEX + i
    } else {
        ('?' as u32 - GLYPH_FIRST) as usize
    }
}

fn glyph_char(index: usize) -> char {
    if index >= ELLIPSIS_INDEX {
        SPECIAL_GLYPHS
            .get(index - ELLIPSIS_INDEX)
            .copied()
            .unwrap_or('?')
    } else {
        char::from_u32(GLYPH_FIRST + index as u32).unwrap_or('?')
    }
}

impl GlyphAtlas {
    /// Bake the glyph set through a throwaway 2D canvas and upload it as
    /// an R8 texture with generated mips.
    pub(crate) fn bake(gl: &Gl) -> Result<GlyphAtlas, JsValue> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| JsValue::from_str("xr-web: no document for atlas bake"))?;
        let canvas: web_sys::HtmlCanvasElement = document
            .create_element("canvas")?
            .dyn_into()
            .map_err(|_| JsValue::from_str("xr-web: atlas canvas failed"))?;
        let ctx: web_sys::CanvasRenderingContext2d = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("xr-web: 2d context unavailable"))?
            .dyn_into()
            .map_err(|_| JsValue::from_str("xr-web: 2d context type"))?;

        // Pass 1: advances.
        ctx.set_font(FONT_SPEC);
        let mut advances_px = Vec::with_capacity(GLYPH_COUNT);
        let mut max_advance = 0.0f32;
        for i in 0..GLYPH_COUNT {
            let ch = glyph_char(i);
            let advance = ctx
                .measure_text(&ch.to_string())
                .map(|m| m.width() as f32)
                .unwrap_or(FONT_PX * 0.5);
            max_advance = max_advance.max(advance);
            advances_px.push(advance);
        }

        let cell_w = (max_advance + CELL_PAD * 2.0).ceil();
        let rows = (GLYPH_COUNT as u32).div_ceil(COLS);
        let tex_w = (COLS as f32 * cell_w) as u32;
        let tex_h = rows * CELL_H;
        canvas.set_width(tex_w);
        canvas.set_height(tex_h);

        // Pass 2: raster. (Canvas resize reset the context state.)
        ctx.set_font(FONT_SPEC);
        ctx.set_fill_style_str("#ffffff");
        ctx.set_text_baseline("alphabetic");
        for (i, _) in advances_px.iter().enumerate() {
            let col = (i as u32 % COLS) as f64;
            let row = (i as u32 / COLS) as f64;
            let x = col * cell_w as f64 + CELL_PAD as f64;
            let y = row * CELL_H as f64 + BASELINE_PX as f64;
            let _ = ctx.fill_text(&glyph_char(i).to_string(), x, y);
        }

        // Alpha channel → tightly packed R8.
        let image = ctx.get_image_data(0.0, 0.0, tex_w as f64, tex_h as f64)?;
        let rgba = image.data();
        let mut coverage = vec![0u8; (tex_w * tex_h) as usize];
        for (dst, px) in coverage.iter_mut().zip(rgba.0.chunks_exact(4)) {
            *dst = px[3];
        }

        let texture = gl
            .create_texture()
            .ok_or_else(|| JsValue::from_str("xr-web: atlas texture alloc"))?;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
        gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 1);
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::R8 as i32,
            tex_w as i32,
            tex_h as i32,
            0,
            Gl::RED,
            Gl::UNSIGNED_BYTE,
            Some(&coverage),
        )?;
        gl.generate_mipmap(Gl::TEXTURE_2D);
        gl.tex_parameteri(
            Gl::TEXTURE_2D,
            Gl::TEXTURE_MIN_FILTER,
            Gl::LINEAR_MIPMAP_LINEAR as i32,
        );
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::LINEAR as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);

        Ok(GlyphAtlas {
            texture,
            advances_px,
            cell_w,
            tex_w: tex_w as f32,
            tex_h: tex_h as f32,
        })
    }

    pub(crate) fn texture(&self) -> &web_sys::WebGlTexture {
        &self.texture
    }

    fn advance_px(&self, c: char) -> f32 {
        self.advances_px[glyph_index(c)]
    }

    /// Append one run's glyph quads as interleaved pos3+uv2+color4
    /// vertices (6 per glyph). Handles alignment and ellipsis truncation.
    pub(crate) fn layout_run(&self, run: &TextRun, out: &mut Vec<f32>) {
        let scale = run.height / FONT_PX;
        // Truncate with '…' when the run overflows its cap.
        let mut chars: Vec<char> = run.text.chars().collect();
        let full_width_px: f32 = chars.iter().map(|&c| self.advance_px(c)).sum();
        let max_px = if run.max_width > 0.0 {
            run.max_width / scale
        } else {
            f32::INFINITY
        };
        if full_width_px > max_px {
            let ell = self.advances_px[ELLIPSIS_INDEX];
            let mut used = 0.0;
            let mut keep = 0;
            for (i, &c) in chars.iter().enumerate() {
                let adv = self.advance_px(c);
                if used + adv + ell > max_px {
                    break;
                }
                used += adv;
                keep = i + 1;
            }
            chars.truncate(keep);
            chars.push('…');
        }
        let width_px: f32 = chars.iter().map(|&c| self.advance_px(c)).sum();

        let start_off_px = match run.align {
            TextAlign::Left => 0.0,
            TextAlign::Center => -width_px / 2.0,
        };

        let mut pen_px = start_off_px;
        for &c in &chars {
            let idx = glyph_index(c);
            let advance = self.advances_px[idx];
            if c != ' ' {
                let col = (idx as u32 % COLS) as f32;
                let row = (idx as u32 / COLS) as f32;
                let u0 = (col * self.cell_w) / self.tex_w;
                let u1 = ((col + 1.0) * self.cell_w) / self.tex_w;
                let v0 = (row * CELL_H as f32) / self.tex_h;
                let v1 = ((row + 1.0) * CELL_H as f32) / self.tex_h;

                // Cell-sized quad anchored to the baseline.
                let left_px = pen_px - CELL_PAD;
                let right_px = left_px + self.cell_w;
                let top_px = BASELINE_PX;
                let bottom_px = BASELINE_PX - CELL_H as f32;

                let base = run.origin;
                let corner = |x_px: f32, y_px: f32| {
                    base + run.right.scale(x_px * scale) + run.up.scale(y_px * scale)
                };
                let bl = corner(left_px, bottom_px);
                let br = corner(right_px, bottom_px);
                let tr = corner(right_px, top_px);
                let tl = corner(left_px, top_px);

                // v0 is the cell's TOP row in the uploaded image.
                let mut push = |p: crate::math::Vec3, u: f32, v: f32| {
                    out.extend_from_slice(&[p.x, p.y, p.z, u, v]);
                    out.extend_from_slice(&run.color);
                };
                push(bl, u0, v1);
                push(br, u1, v1);
                push(tr, u1, v0);
                push(bl, u0, v1);
                push(tr, u1, v0);
                push(tl, u0, v0);
            }
            pen_px += advance;
        }
    }
}

impl TextMeasure for GlyphAtlas {
    fn measure(&self, text: &str, height: f32) -> f32 {
        let scale = height / FONT_PX;
        text.chars().map(|c| self.advance_px(c) * scale).sum()
    }
}

/// Floats per text vertex: pos3 + uv2 + rgba4.
pub(crate) const TEXT_VERTEX_STRIDE: usize = 9;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_mapping_covers_ascii_specials_and_falls_back() {
        assert_eq!(glyph_index(' '), 0);
        assert_eq!(glyph_index('~'), (GLYPH_LAST - GLYPH_FIRST) as usize);
        assert_eq!(glyph_index('…'), ELLIPSIS_INDEX);
        // The separators the scene vocabulary uses must never render '?'.
        let question = glyph_index('?');
        assert_ne!(glyph_index('—'), question);
        assert_ne!(glyph_index('·'), question);
        assert_eq!(glyph_index('☃'), question);
        // Round trip: every index names the char that maps back to it.
        for i in 0..GLYPH_COUNT {
            assert_eq!(glyph_index(glyph_char(i)), i);
        }
    }
}
