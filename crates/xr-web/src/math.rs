//! Minimal linear algebra for the XR surface.
//!
//! Column-major `[f32; 16]` matrices, matching the layout WebXR hands out
//! in `XRRigidTransform.matrix` / `XRView.projectionMatrix` Float32Arrays —
//! values copy straight in with no transpose. Deliberately dependency-free:
//! the crate ships in the dashboard's committed WASM artifacts, and the
//! few operations XR needs (multiply, rigid inverse, point/ray transforms,
//! ray-vs-panel) don't justify a linalg dependency.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub const fn v3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

impl std::ops::Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        v3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        v3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Vec3 {
    pub const ZERO: Vec3 = v3(0.0, 0.0, 0.0);

    pub fn scale(self, s: f32) -> Vec3 {
        v3(self.x * s, self.y * s, self.z * s)
    }

    pub fn dot(self, o: Vec3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        v3(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    pub fn normalize(self) -> Vec3 {
        let len = self.length();
        if len <= f32::EPSILON {
            Vec3::ZERO
        } else {
            self.scale(1.0 / len)
        }
    }
}

/// Column-major 4×4 matrix: element (row, col) lives at `m[col * 4 + row]`,
/// exactly the WebXR / WebGL uniform layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4(pub [f32; 16]);

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4([
        1.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        0.0, 0.0, 0.0, 1.0,
    ]);

    pub fn from_slice(values: &[f32]) -> Option<Mat4> {
        let arr: [f32; 16] = values.try_into().ok()?;
        Some(Mat4(arr))
    }

    fn at(&self, row: usize, col: usize) -> f32 {
        self.0[col * 4 + row]
    }

    /// `self * other` (apply `other` first, then `self`).
    pub fn mul(&self, other: &Mat4) -> Mat4 {
        let mut out = [0.0f32; 16];
        for col in 0..4 {
            for row in 0..4 {
                let mut acc = 0.0;
                for k in 0..4 {
                    acc += self.at(row, k) * other.at(k, col);
                }
                out[col * 4 + row] = acc;
            }
        }
        Mat4(out)
    }

    /// Transform a point (w = 1), including the perspective divide.
    pub fn transform_point(&self, p: Vec3) -> Vec3 {
        let x = self.at(0, 0) * p.x + self.at(0, 1) * p.y + self.at(0, 2) * p.z + self.at(0, 3);
        let y = self.at(1, 0) * p.x + self.at(1, 1) * p.y + self.at(1, 2) * p.z + self.at(1, 3);
        let z = self.at(2, 0) * p.x + self.at(2, 1) * p.y + self.at(2, 2) * p.z + self.at(2, 3);
        let w = self.at(3, 0) * p.x + self.at(3, 1) * p.y + self.at(3, 2) * p.z + self.at(3, 3);
        if w.abs() > f32::EPSILON && (w - 1.0).abs() > f32::EPSILON {
            v3(x / w, y / w, z / w)
        } else {
            v3(x, y, z)
        }
    }

    /// Transform a direction (w = 0, no translation).
    pub fn transform_dir(&self, d: Vec3) -> Vec3 {
        v3(
            self.at(0, 0) * d.x + self.at(0, 1) * d.y + self.at(0, 2) * d.z,
            self.at(1, 0) * d.x + self.at(1, 1) * d.y + self.at(1, 2) * d.z,
            self.at(2, 0) * d.x + self.at(2, 1) * d.y + self.at(2, 2) * d.z,
        )
    }

    /// The translation column.
    pub fn translation(&self) -> Vec3 {
        v3(self.at(0, 3), self.at(1, 3), self.at(2, 3))
    }

    /// Inverse of a rigid transform (pure rotation + translation, the shape
    /// of every `XRRigidTransform.matrix`): transpose the rotation block,
    /// counter-rotate the translation. NOT a general inverse — feeding a
    /// projection matrix through this returns garbage by design.
    pub fn invert_rigid(&self) -> Mat4 {
        let m = &self.0;
        // Rotation columns of the input (basis vectors).
        let rx = v3(m[0], m[1], m[2]);
        let ry = v3(m[4], m[5], m[6]);
        let rz = v3(m[8], m[9], m[10]);
        let t = v3(m[12], m[13], m[14]);
        Mat4([
            rx.x,
            ry.x,
            rz.x,
            0.0, //
            rx.y,
            ry.y,
            rz.y,
            0.0, //
            rx.z,
            ry.z,
            rz.z,
            0.0, //
            -rx.dot(t),
            -ry.dot(t),
            -rz.dot(t),
            1.0,
        ])
    }
}

/// A world-space ray (origin + unit direction).
#[derive(Clone, Copy, Debug)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
}

impl Ray {
    /// Ray from a rigid pose matrix: origin is the translation, direction
    /// is the pose's -Z axis (WebXR target rays point down -Z).
    pub fn from_rigid(pose: &Mat4) -> Ray {
        let m = &pose.0;
        Ray {
            origin: v3(m[12], m[13], m[14]),
            dir: v3(-m[8], -m[9], -m[10]).normalize(),
        }
    }
}

/// An oriented rectangle in world space: `center` with unit `right`/`up`
/// axes and half-extents. The spatial kit's panels, pills, and monitors
/// all hit-test through this.
#[derive(Clone, Copy, Debug)]
pub struct Panel {
    pub center: Vec3,
    pub right: Vec3,
    pub up: Vec3,
    pub half_w: f32,
    pub half_h: f32,
}

impl Panel {
    pub fn normal(&self) -> Vec3 {
        self.right.cross(self.up).normalize()
    }

    /// Ray-vs-panel: returns `(t, u, v)` — distance along the ray and
    /// panel-local coordinates in [-1, 1] — when the ray hits the front OR
    /// back face within the extents. `t` must be positive (in front of the
    /// ray origin), and near-parallel rays miss.
    pub fn raycast(&self, ray: &Ray) -> Option<(f32, f32, f32)> {
        let n = self.normal();
        let denom = n.dot(ray.dir);
        if denom.abs() < 1e-6 {
            return None;
        }
        let t = n.dot(self.center - ray.origin) / denom;
        if t <= 0.0 {
            return None;
        }
        let hit = ray.origin + ray.dir.scale(t);
        let local = hit - self.center;
        let u = local.dot(self.right) / self.half_w;
        let v = local.dot(self.up) / self.half_h;
        if u.abs() <= 1.0 && v.abs() <= 1.0 {
            Some((t, u, v))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: Vec3, b: Vec3) {
        assert!(
            (a.x - b.x).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4 && (a.z - b.z).abs() < 1e-4,
            "{a:?} !~ {b:?}"
        );
    }

    /// Rigid transform: rotate 90° around Y then translate (1, 2, 3).
    fn sample_rigid() -> Mat4 {
        // cos = 0, sin = 1: x-axis → -z, z-axis → x (column-major).
        Mat4([
            0.0, 0.0, -1.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            1.0, 0.0, 0.0, 0.0, //
            1.0, 2.0, 3.0, 1.0,
        ])
    }

    #[test]
    fn identity_mul_is_neutral() {
        let m = sample_rigid();
        assert_eq!(Mat4::IDENTITY.mul(&m), m);
        assert_eq!(m.mul(&Mat4::IDENTITY), m);
    }

    #[test]
    fn rigid_inverse_roundtrips_points() {
        let m = sample_rigid();
        let inv = m.invert_rigid();
        for p in [v3(0.0, 0.0, 0.0), v3(1.0, -2.0, 0.5), v3(-3.0, 0.1, 9.0)] {
            assert_close(inv.transform_point(m.transform_point(p)), p);
        }
        let round = m.mul(&inv);
        for (i, (got, want)) in round.0.iter().zip(Mat4::IDENTITY.0.iter()).enumerate() {
            assert!((got - want).abs() < 1e-5, "element {i}: {got} != {want}");
        }
    }

    #[test]
    fn ray_from_rigid_points_down_negative_z() {
        let ray = Ray::from_rigid(&Mat4::IDENTITY);
        assert_close(ray.origin, Vec3::ZERO);
        assert_close(ray.dir, v3(0.0, 0.0, -1.0));

        // The sample rotation maps -Z to... z-axis column is (1,0,0), so
        // -Z direction is (-1, 0, 0); origin is the translation.
        let ray = Ray::from_rigid(&sample_rigid());
        assert_close(ray.origin, v3(1.0, 2.0, 3.0));
        assert_close(ray.dir, v3(-1.0, 0.0, 0.0));
    }

    #[test]
    fn panel_raycast_hits_center_and_edges() {
        let panel = Panel {
            center: v3(0.0, 1.5, -2.0),
            right: v3(1.0, 0.0, 0.0),
            up: v3(0.0, 1.0, 0.0),
            half_w: 0.5,
            half_h: 0.25,
        };
        let ray = Ray {
            origin: v3(0.0, 1.5, 0.0),
            dir: v3(0.0, 0.0, -1.0),
        };
        let (t, u, v) = panel.raycast(&ray).expect("center hit");
        assert!((t - 2.0).abs() < 1e-4);
        assert!(u.abs() < 1e-4 && v.abs() < 1e-4);

        // Aim at the top-right corner region.
        let ray = Ray {
            origin: v3(0.45, 1.7, 0.0),
            dir: v3(0.0, 0.0, -1.0),
        };
        let (_, u, v) = panel.raycast(&ray).expect("corner hit");
        assert!(u > 0.8 && v > 0.7, "u={u} v={v}");

        // Outside the extents: miss.
        let ray = Ray {
            origin: v3(0.6, 1.5, 0.0),
            dir: v3(0.0, 0.0, -1.0),
        };
        assert!(panel.raycast(&ray).is_none());

        // Behind the origin: miss.
        let ray = Ray {
            origin: v3(0.0, 1.5, -3.0),
            dir: v3(0.0, 0.0, -1.0),
        };
        assert!(panel.raycast(&ray).is_none());
    }

    #[test]
    fn projection_roundtrip_through_mul() {
        // A plausible asymmetric XR projection (off-center frustum).
        let near = 0.05f32;
        let (l, r, b, t) = (-0.9f32, 0.7f32, -0.8f32, 0.85f32);
        let proj = Mat4([
            2.0 * near / (r - l),
            0.0,
            0.0,
            0.0,
            0.0,
            2.0 * near / (t - b),
            0.0,
            0.0,
            (r + l) / (r - l),
            (t + b) / (t - b),
            -1.0,
            -1.0,
            0.0,
            0.0,
            -2.0 * near,
            0.0,
        ]);
        let view = sample_rigid().invert_rigid();
        let vp = proj.mul(&view);
        // A point 2m down the viewer's gaze must land inside clip space.
        // Viewer at (1,2,3) looking down its rotated -Z = (-1,0,0).
        let p = v3(-1.0, 2.0, 3.0);
        let clip = vp.transform_point(p);
        assert!(clip.x.abs() <= 1.0 && clip.y.abs() <= 1.0, "{clip:?}");
    }
}
