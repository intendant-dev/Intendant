//! The WebGL2 stereo encoder — the universal XR presentation floor.
//!
//! Every WebXR browser can composite an `XRWebGLLayer` (Quest's Horizon
//! Browser presents XR *only* through WebGL as of 2026-08), so this
//! encoder is the path that must always work: it draws the spatial kit's
//! vertex streams into the layer's opaque framebuffer, once per view,
//! with the view-projection matrix the headset supplies. The
//! WebXR-WebGPU binding encoder slots in beside it later behind the same
//! `SceneFrame` seam — the kit never knows which encoder ran.
//!
//! Deliberately boring GL: one interleaved position+color program for
//! triangles and lines. The textured/text program arrives with the glyph
//! atlas. All buffer uploads go through copying `Float32Array::from` —
//! no `unsafe` views (this repo keeps unsafe confined to its documented
//! platform islands, and the streams are a few kilobytes per frame).

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{WebGl2RenderingContext as Gl, WebGlBuffer, WebGlProgram, WebGlUniformLocation};

use crate::math::{v3, Mat4, Vec3};

/// Floats per vertex: xyz + rgba.
pub const VERTEX_STRIDE: usize = 7;

/// CPU-side vertex streams for one frame, in world space. Rebuilt by the
/// spatial kit whenever the scene changes; uploaded once per XR frame and
/// drawn once per view.
#[derive(Default, Clone)]
pub struct SceneFrame {
    pub tris: Vec<f32>,
    pub lines: Vec<f32>,
}

impl SceneFrame {
    pub fn clear(&mut self) {
        self.tris.clear();
        self.lines.clear();
    }

    pub fn push_line(&mut self, a: Vec3, b: Vec3, color: [f32; 4]) {
        for p in [a, b] {
            self.lines.extend_from_slice(&[p.x, p.y, p.z]);
            self.lines.extend_from_slice(&color);
        }
    }

    pub fn push_tri(&mut self, a: Vec3, b: Vec3, c: Vec3, color: [f32; 4]) {
        for p in [a, b, c] {
            self.tris.extend_from_slice(&[p.x, p.y, p.z]);
            self.tris.extend_from_slice(&color);
        }
    }

    /// Two triangles spanning an oriented rectangle.
    pub fn push_quad(
        &mut self,
        center: Vec3,
        right: Vec3,
        up: Vec3,
        half_w: f32,
        half_h: f32,
        color: [f32; 4],
    ) {
        let r = right.scale(half_w);
        let u = up.scale(half_h);
        let a = center - r - u;
        let b = center + r - u;
        let c = center + r + u;
        let d = center - r + u;
        self.push_tri(a, b, c, color);
        self.push_tri(a, c, d, color);
    }

    pub fn tri_vertex_count(&self) -> i32 {
        (self.tris.len() / VERTEX_STRIDE) as i32
    }

    pub fn line_vertex_count(&self) -> i32 {
        (self.lines.len() / VERTEX_STRIDE) as i32
    }
}

/// The engine-interim reference scene: a floor grid at y=0, axis gizmo at
/// the origin, and one panel at head height 1.2 m ahead. Proves stereo,
/// depth, and world-lock on hardware before the spatial kit lands; the
/// kit replaces this as the frame source.
pub fn reference_scene() -> SceneFrame {
    let mut frame = SceneFrame::default();
    let grid = [0.35, 0.42, 0.55, 0.55f32];
    let extent = 3.0f32;
    let step = 0.5f32;
    let mut d = -extent;
    while d <= extent + 1e-4 {
        frame.push_line(v3(d, 0.0, -extent), v3(d, 0.0, extent), grid);
        frame.push_line(v3(-extent, 0.0, d), v3(extent, 0.0, d), grid);
        d += step;
    }
    // Axis gizmo: X red, Y green, Z blue, 0.3 m.
    frame.push_line(v3(0.0, 0.001, 0.0), v3(0.3, 0.001, 0.0), [0.95, 0.35, 0.35, 1.0]);
    frame.push_line(v3(0.0, 0.001, 0.0), v3(0.0, 0.301, 0.0), [0.42, 0.85, 0.48, 1.0]);
    frame.push_line(v3(0.0, 0.001, 0.0), v3(0.0, 0.001, 0.3), [0.45, 0.55, 0.98, 1.0]);
    // Reference panel: 0.8 × 0.5 m, 1.2 m ahead at 1.4 m height, with a
    // brighter frame so both depth write and line pass are visible.
    let center = v3(0.0, 1.4, -1.2);
    let right = v3(1.0, 0.0, 0.0);
    let up = v3(0.0, 1.0, 0.0);
    frame.push_quad(center, right, up, 0.4, 0.25, [0.08, 0.09, 0.13, 0.92]);
    let corners = [
        center - right.scale(0.4) - up.scale(0.25),
        center + right.scale(0.4) - up.scale(0.25),
        center + right.scale(0.4) + up.scale(0.25),
        center - right.scale(0.4) + up.scale(0.25),
    ];
    let edge = [0.49, 0.55, 0.98, 1.0];
    for i in 0..4 {
        frame.push_line(corners[i], corners[(i + 1) % 4], edge);
    }
    frame
}

/// One compiled+linked program with the attribute/uniform locations the
/// encoder binds every frame.
struct Program {
    program: WebGlProgram,
    u_view_proj: WebGlUniformLocation,
}

/// The encoder: owns the XR-compatible context and the vertex buffers.
pub struct GlEncoder {
    canvas: web_sys::HtmlCanvasElement,
    gl: Gl,
    solid: Program,
    vbo_tris: WebGlBuffer,
    vbo_lines: WebGlBuffer,
    tri_count: i32,
    line_count: i32,
}

const VS_SOLID: &str = r"#version 300 es
layout(location = 0) in vec3 aPos;
layout(location = 1) in vec4 aColor;
uniform mat4 uViewProj;
out vec4 vColor;
void main() {
    vColor = aColor;
    gl_Position = uViewProj * vec4(aPos, 1.0);
}
";

const FS_SOLID: &str = r"#version 300 es
precision mediump float;
in vec4 vColor;
out vec4 outColor;
void main() {
    outColor = vColor;
}
";

impl GlEncoder {
    /// Create the hidden canvas + XR-compatible WebGL2 context and compile
    /// the base program. The canvas is never attached to the DOM — the
    /// context exists to feed `XRWebGLLayer`.
    pub fn new() -> Result<GlEncoder, JsValue> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| JsValue::from_str("xr-web: no document"))?;
        let canvas: web_sys::HtmlCanvasElement = document
            .create_element("canvas")?
            .dyn_into()
            .map_err(|_| JsValue::from_str("xr-web: canvas creation failed"))?;

        let attrs = js_sys::Object::new();
        js_sys::Reflect::set(&attrs, &"xrCompatible".into(), &JsValue::TRUE)?;
        let gl: Gl = canvas
            .get_context_with_context_options("webgl2", &attrs)?
            .ok_or_else(|| JsValue::from_str("xr-web: WebGL2 unavailable"))?
            .dyn_into()
            .map_err(|_| JsValue::from_str("xr-web: WebGL2 context has unexpected type"))?;

        let solid = compile_program(&gl, VS_SOLID, FS_SOLID)?;
        let vbo_tris = gl
            .create_buffer()
            .ok_or_else(|| JsValue::from_str("xr-web: buffer alloc failed"))?;
        let vbo_lines = gl
            .create_buffer()
            .ok_or_else(|| JsValue::from_str("xr-web: buffer alloc failed"))?;

        gl.enable(Gl::DEPTH_TEST);
        gl.depth_func(Gl::LEQUAL);
        gl.enable(Gl::BLEND);
        gl.blend_func_separate(
            Gl::SRC_ALPHA,
            Gl::ONE_MINUS_SRC_ALPHA,
            Gl::ONE,
            Gl::ONE_MINUS_SRC_ALPHA,
        );

        Ok(GlEncoder {
            canvas,
            gl,
            solid,
            vbo_tris,
            vbo_lines,
            tri_count: 0,
            line_count: 0,
        })
    }

    /// The raw context, for `XRWebGLLayer` construction.
    pub fn gl_context_js(&self) -> JsValue {
        self.gl.clone().into()
    }

    /// `gl.makeXRCompatible()` — required when the adapter changed after
    /// context creation; guarded because a few runtimes predate it. The
    /// returned promise (when present) must be awaited before layer
    /// construction.
    pub fn make_xr_compatible_promise(&self) -> Option<js_sys::Promise> {
        let gl_js: &JsValue = self.gl.as_ref();
        let f = js_sys::Reflect::get(gl_js, &"makeXRCompatible".into()).ok()?;
        let f: js_sys::Function = f.dyn_into().ok()?;
        let ret = f.call0(gl_js).ok()?;
        ret.dyn_into().ok()
    }

    /// Keep the hidden canvas alive with the context (dropping it is
    /// harmless, but ownership documents intent).
    pub fn canvas(&self) -> &web_sys::HtmlCanvasElement {
        &self.canvas
    }

    /// Upload the frame's vertex streams (copying — no unsafe views).
    pub fn upload(&mut self, frame: &SceneFrame) {
        let gl = &self.gl;
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_tris));
        let data = js_sys::Float32Array::from(frame.tris.as_slice());
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &data, Gl::DYNAMIC_DRAW);
        self.tri_count = frame.tri_vertex_count();

        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_lines));
        let data = js_sys::Float32Array::from(frame.lines.as_slice());
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &data, Gl::DYNAMIC_DRAW);
        self.line_count = frame.line_vertex_count();
    }

    /// Bind the XR layer's framebuffer and clear it. AR sessions clear to
    /// transparent (passthrough shows through); VR clears to a deep
    /// neutral so the void reads intentional.
    pub fn begin_frame(
        &self,
        framebuffer: Option<&web_sys::WebGlFramebuffer>,
        width: i32,
        height: i32,
        passthrough: bool,
    ) {
        let gl = &self.gl;
        gl.bind_framebuffer(Gl::FRAMEBUFFER, framebuffer);
        gl.viewport(0, 0, width, height);
        if passthrough {
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
        } else {
            gl.clear_color(0.043, 0.047, 0.063, 1.0);
        }
        gl.clear(Gl::COLOR_BUFFER_BIT | Gl::DEPTH_BUFFER_BIT);
    }

    /// Draw the uploaded streams for one view.
    pub fn draw_view(&self, view_proj: &Mat4, viewport: (i32, i32, i32, i32)) {
        let gl = &self.gl;
        let (x, y, w, h) = viewport;
        gl.viewport(x, y, w, h);
        gl.use_program(Some(&self.solid.program));
        gl.uniform_matrix4fv_with_f32_array(Some(&self.solid.u_view_proj), false, &view_proj.0);

        if self.tri_count > 0 {
            gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_tris));
            bind_pos_color_layout(gl);
            gl.draw_arrays(Gl::TRIANGLES, 0, self.tri_count);
        }
        if self.line_count > 0 {
            gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_lines));
            bind_pos_color_layout(gl);
            gl.draw_arrays(Gl::LINES, 0, self.line_count);
        }
    }
}

/// Interleaved layout: vec3 position + vec4 color, tightly packed.
fn bind_pos_color_layout(gl: &Gl) {
    let stride = (VERTEX_STRIDE * 4) as i32;
    gl.enable_vertex_attrib_array(0);
    gl.vertex_attrib_pointer_with_i32(0, 3, Gl::FLOAT, false, stride, 0);
    gl.enable_vertex_attrib_array(1);
    gl.vertex_attrib_pointer_with_i32(1, 4, Gl::FLOAT, false, stride, 12);
}

fn compile_program(gl: &Gl, vs: &str, fs: &str) -> Result<Program, JsValue> {
    let vs = compile_shader(gl, Gl::VERTEX_SHADER, vs)?;
    let fs = compile_shader(gl, Gl::FRAGMENT_SHADER, fs)?;
    let program = gl
        .create_program()
        .ok_or_else(|| JsValue::from_str("xr-web: program alloc failed"))?;
    gl.attach_shader(&program, &vs);
    gl.attach_shader(&program, &fs);
    gl.link_program(&program);
    let linked = gl
        .get_program_parameter(&program, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false);
    if !linked {
        let log = gl
            .get_program_info_log(&program)
            .unwrap_or_else(|| "unknown link error".into());
        return Err(JsValue::from_str(&format!("xr-web: link failed: {log}")));
    }
    let u_view_proj = gl
        .get_uniform_location(&program, "uViewProj")
        .ok_or_else(|| JsValue::from_str("xr-web: uViewProj missing"))?;
    Ok(Program {
        program,
        u_view_proj,
    })
}

fn compile_shader(
    gl: &Gl,
    kind: u32,
    source: &str,
) -> Result<web_sys::WebGlShader, JsValue> {
    let shader = gl
        .create_shader(kind)
        .ok_or_else(|| JsValue::from_str("xr-web: shader alloc failed"))?;
    gl.shader_source(&shader, source);
    gl.compile_shader(&shader);
    let ok = gl
        .get_shader_parameter(&shader, Gl::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false);
    if !ok {
        let log = gl
            .get_shader_info_log(&shader)
            .unwrap_or_else(|| "unknown compile error".into());
        return Err(JsValue::from_str(&format!("xr-web: shader failed: {log}")));
    }
    Ok(shader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_frame_counts_track_stride() {
        let mut f = SceneFrame::default();
        f.push_line(v3(0.0, 0.0, 0.0), v3(1.0, 0.0, 0.0), [1.0; 4]);
        assert_eq!(f.line_vertex_count(), 2);
        assert_eq!(f.lines.len(), 2 * VERTEX_STRIDE);
        f.push_quad(
            v3(0.0, 1.0, -1.0),
            v3(1.0, 0.0, 0.0),
            v3(0.0, 1.0, 0.0),
            0.5,
            0.25,
            [0.5; 4],
        );
        assert_eq!(f.tri_vertex_count(), 6);
        f.clear();
        assert_eq!(f.tri_vertex_count(), 0);
        assert_eq!(f.line_vertex_count(), 0);
    }

    #[test]
    fn reference_scene_has_grid_axes_and_panel() {
        let f = reference_scene();
        // 13 grid lines per direction + 3 axes + 4 panel edges.
        assert_eq!(f.line_vertex_count(), (13 * 2 + 3 + 4) as i32 * 2);
        // One quad = 6 triangle vertices.
        assert_eq!(f.tri_vertex_count(), 6);
    }
}
