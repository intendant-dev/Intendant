//! World-space pane geometry (Phase C). Slice 2: an untextured
//! billboarded quad anchored in the scene, drawn through the pane
//! pipeline — the first geometry that runs a real depth compare against
//! the wireframe's written depth, so nodes and edges occlude it (and it
//! them) correctly. Later slices grow this into textured, text-bearing
//! panel surfaces.

use crate::gpu::{GpuFrame, GpuVertex};
use crate::scene::{Vec2, Vec3};
use crate::util::Color;

/// Append one camera-facing quad centered on `anchor`, spanned by the
/// camera basis (`right`, `up`). Corners project independently and keep
/// their own clip depth, so a pane at an angle to the view still sorts
/// per-pixel against the scene. A pane with any corner culled is skipped
/// whole — a partially projected billboard warps unpredictably near the
/// frustum edge. Returns whether the pane was emitted.
pub(crate) fn add_world_pane(
    frame: &mut GpuFrame,
    project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
    right: Vec3,
    up: Vec3,
    anchor: Vec3,
    half_w: f32,
    half_h: f32,
    color: Color,
) -> bool {
    let corners = [
        anchor - right * half_w - up * half_h,
        anchor + right * half_w - up * half_h,
        anchor + right * half_w + up * half_h,
        anchor - right * half_w + up * half_h,
    ];
    let mut projected = [(Vec2::new(0.0, 0.0), 0.0f32); 4];
    for (slot, corner) in projected.iter_mut().zip(corners) {
        match project(corner) {
            Some((ndc, _cue, depth)) => *slot = (ndc, depth),
            None => return false,
        }
    }
    let [bl, br, tr, tl] = projected;
    let rgba: [f32; 4] = color.into();
    for (ndc, depth) in [bl, br, tr, bl, tr, tl] {
        frame.pane_vertices.push(GpuVertex {
            pos: [ndc.x, ndc.y],
            depth,
            color: rgba,
        });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basis() -> (Vec3, Vec3) {
        (Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0))
    }

    #[test]
    fn pane_emits_two_triangles_with_corner_depths() {
        let mut frame = GpuFrame::default();
        let (right, up) = basis();
        // Projector: NDC from x/y, depth rises with x so the two right
        // corners carry a deeper value than the left ones.
        let mut project =
            |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.5 + v.x * 0.1));
        let emitted = add_world_pane(
            &mut frame,
            &mut project,
            right,
            up,
            Vec3::new(0.0, 0.0, 0.0),
            1.0,
            0.5,
            Color::rgb(16, 18, 32).with_alpha(0.86),
        );
        assert!(emitted);
        assert_eq!(frame.pane_vertices.len(), 6);
        assert!(frame.tri_vertices.is_empty() && frame.line_vertices.is_empty());
        // Triangle order [bl, br, tr, bl, tr, tl]: left corners at
        // depth 0.4, right corners at 0.6.
        let depths: Vec<f32> = frame.pane_vertices.iter().map(|v| v.depth).collect();
        assert_eq!(depths, vec![0.4, 0.6, 0.6, 0.4, 0.6, 0.4]);
        assert!((frame.pane_vertices[0].color[3] - 0.86).abs() < 1e-6);
    }

    #[test]
    fn pane_with_any_culled_corner_is_skipped_whole() {
        let mut frame = GpuFrame::default();
        let (right, up) = basis();
        // Cull anything left of x = 0 — the two left corners fail.
        let mut project = |v: Vec3| {
            (v.x >= 0.0).then(|| (Vec2::new(v.x, v.y), 1.0, 0.5))
        };
        let emitted = add_world_pane(
            &mut frame,
            &mut project,
            right,
            up,
            Vec3::new(0.5, 0.0, 0.0),
            1.0,
            0.5,
            Color::rgb(255, 255, 255),
        );
        assert!(!emitted);
        assert!(frame.pane_vertices.is_empty());
    }
}
