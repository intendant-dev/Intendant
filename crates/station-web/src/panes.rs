//! World-space pane geometry (Phase C). Slice 2: an untextured
//! billboarded quad anchored in the scene, drawn through the pane
//! pipeline — the first geometry that runs a real depth compare against
//! the wireframe's written depth, so nodes and edges occlude it (and it
//! them) correctly. Later slices grow this into textured, text-bearing
//! panel surfaces.

use crate::gpu::{GpuFrame, GpuVertex};
use crate::scene::{Vec2, Vec3};
use crate::util::Color;

/// A pane's world geometry registered for raycast picking — the 3D
/// counterpart of `ProjectedNode` (which carries screen-space pick
/// anchors for nodes). The scene pushes one alongside each emitted quad;
/// `input::pick_pane` intersects pointer rays with them.
pub(crate) struct PaneTarget {
    pub(crate) id: String,
    pub(crate) anchor: Vec3,
    pub(crate) right: Vec3,
    pub(crate) up: Vec3,
    pub(crate) half_w: f32,
    pub(crate) half_h: f32,
}

/// Ray-vs-pane intersection: the distance along `dir` where the ray
/// pierces the pane's rectangle, or None on a miss, a parallel ray, or a
/// pane behind the ray. The near cutoff mirrors `Camera::project_depth`'s
/// cull, so a pane the projector refuses to draw can't be picked either.
pub(crate) fn ray_hit(target: &PaneTarget, origin: Vec3, dir: Vec3) -> Option<f32> {
    let normal = target.right.cross(target.up);
    let denom = dir.dot(normal);
    if denom.abs() < 1e-6 {
        return None;
    }
    let t = (target.anchor - origin).dot(normal) / denom;
    if t <= 0.12 {
        return None;
    }
    let local = (origin + dir * t) - target.anchor;
    (local.dot(target.right).abs() <= target.half_w
        && local.dot(target.up).abs() <= target.half_h)
        .then_some(t)
}

/// Append one camera-facing quad centered on `anchor`, spanned by the
/// camera basis (`right`, `up`). Corners project independently and keep
/// their own clip depth, so a pane at an angle to the view still sorts
/// per-pixel against the scene. A pane with any corner culled is skipped
/// whole — a partially projected billboard warps unpredictably near the
/// frustum edge. Returns whether the pane was emitted.
#[allow(clippy::too_many_arguments)]
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

    fn target(anchor: Vec3, half_w: f32, half_h: f32) -> PaneTarget {
        let (right, up) = basis();
        PaneTarget {
            id: "op".into(),
            anchor,
            right,
            up,
            half_w,
            half_h,
        }
    }

    #[test]
    fn ray_hits_inside_and_misses_outside() {
        let pane = target(Vec3::new(0.0, 0.0, -5.0), 1.0, 0.5);
        let origin = Vec3::ZERO;
        // Straight through the center: hit at the plane distance.
        let t = ray_hit(&pane, origin, Vec3::new(0.0, 0.0, -1.0)).unwrap();
        assert!((t - 5.0).abs() < 1e-5);
        // Angled rays landing inside the half extents hit; past them, miss.
        let inside = (Vec3::new(0.9, 0.0, -5.0) - origin).normalized();
        assert!(ray_hit(&pane, origin, inside).is_some());
        let outside = (Vec3::new(1.1, 0.0, -5.0) - origin).normalized();
        assert!(ray_hit(&pane, origin, outside).is_none());
        let above = (Vec3::new(0.0, 0.6, -5.0) - origin).normalized();
        assert!(ray_hit(&pane, origin, above).is_none());
    }

    #[test]
    fn ray_ignores_behind_camera_and_parallel_panes() {
        // Pane behind the ray: the plane intersection is at negative t.
        let behind = target(Vec3::new(0.0, 0.0, 5.0), 1.0, 1.0);
        assert!(ray_hit(&behind, Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0)).is_none());
        // A ray parallel to the pane's plane can never pierce it.
        let side = target(Vec3::new(0.0, 0.0, -5.0), 1.0, 1.0);
        assert!(ray_hit(&side, Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)).is_none());
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
