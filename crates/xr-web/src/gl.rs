//! The WebGL2 stereo encoder — the universal XR presentation floor.
//!
//! Every WebXR browser can composite an `XRWebGLLayer` (Quest's Horizon
//! Browser presents XR *only* through WebGL as of 2026-08), so this
//! encoder is the path that must always work: it draws the spatial kit's
//! batches into the layer's opaque framebuffer, once per view, with the
//! view-projection matrix the headset supplies. The WebXR-WebGPU binding
//! encoder slots in beside it later behind the same batch seam — the kit
//! never knows which encoder ran.
//!
//! Three deliberately boring programs: interleaved pos+color for
//! triangles/lines, a rounded-rect SDF program drawing one panel per
//! call (a scene holds dozens of panels, not thousands — the trivial
//! data path wins), and a glyph-atlas text program. All buffer uploads
//! go through copying `Float32Array::from` — no `unsafe` views (this
//! repo keeps unsafe confined to its documented platform islands, and
//! the streams are a few kilobytes per frame).

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{WebGl2RenderingContext as Gl, WebGlBuffer, WebGlProgram, WebGlUniformLocation};

use crate::atlas::{GlyphAtlas, TEXT_VERTEX_STRIDE};
use crate::kit::{MonitorInstance, PanelInstance, SceneBatches};
use crate::math::{v3, Mat4, Vec3};

/// Floats per pos+color vertex: xyz + rgba.
pub const VERTEX_STRIDE: usize = 7;

/// CPU-side vertex streams for one frame, in world space. Rebuilt by the
/// spatial kit whenever the scene changes; uploaded once and drawn once
/// per XR view.
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

/// The engine's hardware smoke scene: a floor grid, axis gizmo, and one
/// panel at head height. Used by the validator probe and kept as the
/// canonical "is stereo/depth/world-lock sane" reference; the spatial
/// kit is the live frame source.
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
    frame.push_line(
        v3(0.0, 0.001, 0.0),
        v3(0.3, 0.001, 0.0),
        [0.95, 0.35, 0.35, 1.0],
    );
    frame.push_line(
        v3(0.0, 0.001, 0.0),
        v3(0.0, 0.301, 0.0),
        [0.42, 0.85, 0.48, 1.0],
    );
    frame.push_line(
        v3(0.0, 0.001, 0.0),
        v3(0.0, 0.001, 0.3),
        [0.45, 0.55, 0.98, 1.0],
    );
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

// ---- shaders -------------------------------------------------------------

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
precision highp float;
in vec4 vColor;
out vec4 outColor;
void main() {
    outColor = vColor;
}
";

const VS_PANEL: &str = r"#version 300 es
layout(location = 0) in vec2 aUnit;
uniform mat4 uViewProj;
uniform vec3 uCenter;
uniform vec3 uRight;
uniform vec3 uUp;
uniform vec2 uHalf;
out vec2 vLocal;
void main() {
    vLocal = aUnit * uHalf;
    vec3 world = uCenter + uRight * vLocal.x + uUp * vLocal.y;
    gl_Position = uViewProj * vec4(world, 1.0);
}
";

const FS_PANEL: &str = r"#version 300 es
precision highp float;
in vec2 vLocal;
uniform vec2 uHalf;
uniform float uRadius;
uniform vec4 uFill;
uniform vec4 uBorder;
uniform float uBorderW;
out vec4 outColor;
void main() {
    // Rounded-box SDF in panel-local meters.
    vec2 b = uHalf - vec2(uRadius);
    vec2 q = abs(vLocal) - b;
    float d = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - uRadius;
    float aa = fwidth(d) * 1.25 + 1e-6;
    float inside = 1.0 - smoothstep(0.0, aa, d);
    vec4 col = uFill;
    if (uBorderW > 0.0) {
        float t = smoothstep(-uBorderW - aa, -uBorderW + aa, d);
        col = mix(uFill, uBorder, t);
    }
    float alpha = col.a * inside;
    // Transparent corners must not write depth — they'd clip content
    // behind the rounded cutout.
    if (alpha <= 0.004) discard;
    outColor = vec4(col.rgb, alpha);
}
";

const VS_VIDEO: &str = r"#version 300 es
layout(location = 0) in vec2 aUnit;
uniform mat4 uViewProj;
uniform vec3 uCenter;
uniform vec3 uRight;
uniform vec3 uUp;
uniform vec2 uHalf;
out vec2 vUv;
void main() {
    // Video rows land top-first in the texture: v=0 at the screen's top.
    vUv = vec2(aUnit.x * 0.5 + 0.5, 0.5 - aUnit.y * 0.5);
    vec3 world = uCenter + uRight * (aUnit.x * uHalf.x) + uUp * (aUnit.y * uHalf.y);
    gl_Position = uViewProj * vec4(world, 1.0);
}
";

const FS_VIDEO: &str = r"#version 300 es
precision highp float;
in vec2 vUv;
uniform sampler2D uTex;
out vec4 outColor;
void main() {
    outColor = vec4(texture(uTex, vUv).rgb, 1.0);
}
";

const VS_TEXT: &str = r"#version 300 es
layout(location = 0) in vec3 aPos;
layout(location = 1) in vec2 aUv;
layout(location = 2) in vec4 aColor;
uniform mat4 uViewProj;
out vec2 vUv;
out vec4 vColor;
void main() {
    vUv = aUv;
    vColor = aColor;
    gl_Position = uViewProj * vec4(aPos, 1.0);
}
";

const FS_TEXT: &str = r"#version 300 es
precision highp float;
in vec2 vUv;
in vec4 vColor;
uniform sampler2D uAtlas;
out vec4 outColor;
void main() {
    float coverage = texture(uAtlas, vUv).r;
    outColor = vec4(vColor.rgb, vColor.a * coverage);
}
";

// ---- programs ------------------------------------------------------------

struct SolidProgram {
    program: WebGlProgram,
    u_view_proj: WebGlUniformLocation,
}

struct PanelProgram {
    program: WebGlProgram,
    u_view_proj: WebGlUniformLocation,
    u_center: WebGlUniformLocation,
    u_right: WebGlUniformLocation,
    u_up: WebGlUniformLocation,
    u_half: WebGlUniformLocation,
    u_radius: WebGlUniformLocation,
    u_fill: WebGlUniformLocation,
    u_border: WebGlUniformLocation,
    u_border_w: WebGlUniformLocation,
}

struct TextProgram {
    program: WebGlProgram,
    u_view_proj: WebGlUniformLocation,
    u_atlas: WebGlUniformLocation,
}

struct VideoProgram {
    program: WebGlProgram,
    u_view_proj: WebGlUniformLocation,
    u_center: WebGlUniformLocation,
    u_right: WebGlUniformLocation,
    u_up: WebGlUniformLocation,
    u_half: WebGlUniformLocation,
    u_tex: WebGlUniformLocation,
}

/// The encoder: owns the XR-compatible context, the programs, and the
/// per-scene vertex buffers.
pub struct GlEncoder {
    canvas: web_sys::HtmlCanvasElement,
    gl: Gl,
    solid: SolidProgram,
    panel: PanelProgram,
    text: TextProgram,
    video: VideoProgram,
    vbo_tris: WebGlBuffer,
    vbo_lines: WebGlBuffer,
    vbo_text: WebGlBuffer,
    vbo_unit: WebGlBuffer,
    vbo_pointer: WebGlBuffer,
    tri_count: i32,
    line_count: i32,
    text_count: i32,
    pointer_count: i32,
    panels: Vec<PanelInstance>,
    monitors: Vec<MonitorInstance>,
    video_textures: std::collections::HashMap<String, web_sys::WebGlTexture>,
    atlas: Option<GlyphAtlas>,
}

impl GlEncoder {
    /// Create the hidden canvas + XR-compatible WebGL2 context and compile
    /// the programs. The canvas is never attached to the DOM — the context
    /// exists to feed `XRWebGLLayer`.
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

        let solid_raw = compile_program(&gl, VS_SOLID, FS_SOLID)?;
        let solid = SolidProgram {
            u_view_proj: uniform(&gl, &solid_raw, "uViewProj")?,
            program: solid_raw,
        };
        let panel_raw = compile_program(&gl, VS_PANEL, FS_PANEL)?;
        let panel = PanelProgram {
            u_view_proj: uniform(&gl, &panel_raw, "uViewProj")?,
            u_center: uniform(&gl, &panel_raw, "uCenter")?,
            u_right: uniform(&gl, &panel_raw, "uRight")?,
            u_up: uniform(&gl, &panel_raw, "uUp")?,
            u_half: uniform(&gl, &panel_raw, "uHalf")?,
            u_radius: uniform(&gl, &panel_raw, "uRadius")?,
            u_fill: uniform(&gl, &panel_raw, "uFill")?,
            u_border: uniform(&gl, &panel_raw, "uBorder")?,
            u_border_w: uniform(&gl, &panel_raw, "uBorderW")?,
            program: panel_raw,
        };
        let text_raw = compile_program(&gl, VS_TEXT, FS_TEXT)?;
        let text = TextProgram {
            u_view_proj: uniform(&gl, &text_raw, "uViewProj")?,
            u_atlas: uniform(&gl, &text_raw, "uAtlas")?,
            program: text_raw,
        };
        let video_raw = compile_program(&gl, VS_VIDEO, FS_VIDEO)?;
        let video = VideoProgram {
            u_view_proj: uniform(&gl, &video_raw, "uViewProj")?,
            u_center: uniform(&gl, &video_raw, "uCenter")?,
            u_right: uniform(&gl, &video_raw, "uRight")?,
            u_up: uniform(&gl, &video_raw, "uUp")?,
            u_half: uniform(&gl, &video_raw, "uHalf")?,
            u_tex: uniform(&gl, &video_raw, "uTex")?,
            program: video_raw,
        };

        let make_buffer = || {
            gl.create_buffer()
                .ok_or_else(|| JsValue::from_str("xr-web: buffer alloc failed"))
        };
        let vbo_tris = make_buffer()?;
        let vbo_lines = make_buffer()?;
        let vbo_text = make_buffer()?;
        let vbo_unit = make_buffer()?;
        let vbo_pointer = make_buffer()?;

        // Static unit quad for the panel program.
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&vbo_unit));
        let unit: [f32; 12] = [
            -1.0, -1.0, 1.0, -1.0, 1.0, 1.0, //
            -1.0, -1.0, 1.0, 1.0, -1.0, 1.0,
        ];
        let unit_view = js_sys::Float32Array::from(unit.as_slice());
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &unit_view, Gl::STATIC_DRAW);

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
            panel,
            text,
            video,
            vbo_tris,
            vbo_lines,
            vbo_text,
            vbo_unit,
            vbo_pointer,
            tri_count: 0,
            line_count: 0,
            text_count: 0,
            pointer_count: 0,
            panels: Vec::new(),
            monitors: Vec::new(),
            video_textures: std::collections::HashMap::new(),
            atlas: None,
        })
    }

    /// The raw context, for `XRWebGLLayer` construction.
    pub fn gl_context_js(&self) -> JsValue {
        self.gl.clone().into()
    }

    /// Keep the hidden canvas alive with the context (dropping it is
    /// harmless, but ownership documents intent).
    pub fn canvas(&self) -> &web_sys::HtmlCanvasElement {
        &self.canvas
    }

    /// Bake the glyph atlas on first use. Failure is survivable: text
    /// simply doesn't upload, and the caller falls back to approximate
    /// measurement.
    pub(crate) fn ensure_atlas(&mut self) -> bool {
        if self.atlas.is_some() {
            return true;
        }
        match GlyphAtlas::bake(&self.gl) {
            Ok(atlas) => {
                self.atlas = Some(atlas);
                true
            }
            Err(err) => {
                web_sys::console::warn_2(&"xr-web: glyph atlas bake failed".into(), &err);
                false
            }
        }
    }

    pub(crate) fn atlas(&self) -> Option<&GlyphAtlas> {
        self.atlas.as_ref()
    }

    /// Refresh (or create/retire) the video textures backing the scene's
    /// monitors from their live `<video>` elements. Called every frame —
    /// video frames advance regardless of scene rebuilds. Upload errors
    /// (tainted/cross-origin sources) log once per element state change
    /// at worst and skip the frame.
    pub(crate) fn update_video_textures(
        &mut self,
        displays: &[(String, web_sys::HtmlVideoElement)],
    ) {
        let gl = &self.gl;
        // Retire textures whose source vanished.
        let live: std::collections::HashSet<&str> =
            displays.iter().map(|(id, _)| id.as_str()).collect();
        self.video_textures.retain(|id, tex| {
            let keep = live.contains(id.as_str());
            if !keep {
                gl.delete_texture(Some(tex));
            }
            keep
        });
        for (id, video) in displays {
            // HAVE_CURRENT_DATA(2)+ means a frame is uploadable.
            if video.ready_state() < 2 {
                continue;
            }
            let texture = match self.video_textures.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let Some(tex) = gl.create_texture() else {
                        continue;
                    };
                    gl.bind_texture(Gl::TEXTURE_2D, Some(&tex));
                    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::LINEAR as i32);
                    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::LINEAR as i32);
                    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
                    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);
                    e.insert(tex)
                }
            };
            gl.bind_texture(Gl::TEXTURE_2D, Some(texture));
            let _ = gl.tex_image_2d_with_u32_and_u32_and_html_video_element(
                Gl::TEXTURE_2D,
                0,
                Gl::RGBA as i32,
                Gl::RGBA,
                Gl::UNSIGNED_BYTE,
                video,
            );
        }
    }

    /// Upload a full scene build: line/tri streams, panel instances,
    /// monitor placements, and atlas-laid text (copying — no unsafe
    /// views).
    pub(crate) fn upload_batches(&mut self, batches: &SceneBatches) {
        self.upload_streams(&batches.frame);
        self.panels = batches.panels.clone();
        self.monitors = batches.monitors.clone();

        let mut text_vertices: Vec<f32> = Vec::new();
        if let Some(atlas) = self.atlas.as_ref() {
            for run in &batches.texts {
                atlas.layout_run(run, &mut text_vertices);
            }
        }
        let gl = &self.gl;
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_text));
        let view = js_sys::Float32Array::from(text_vertices.as_slice());
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &view, Gl::DYNAMIC_DRAW);
        self.text_count = (text_vertices.len() / TEXT_VERTEX_STRIDE) as i32;
    }

    /// Upload the per-frame pointer overlay (controller ray beams + hit
    /// markers). Refreshed every XR frame — a few dozen vertices.
    pub(crate) fn upload_pointer(&mut self, lines: &[f32]) {
        let gl = &self.gl;
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_pointer));
        let data = js_sys::Float32Array::from(lines);
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &data, Gl::DYNAMIC_DRAW);
        self.pointer_count = (lines.len() / VERTEX_STRIDE) as i32;
    }

    /// Upload only the raw line/tri streams (the hardware smoke path).
    pub fn upload_streams(&mut self, frame: &SceneFrame) {
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
        gl.depth_mask(true);
        if passthrough {
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
        } else {
            gl.clear_color(0.043, 0.047, 0.063, 1.0);
        }
        gl.clear(Gl::COLOR_BUFFER_BIT | Gl::DEPTH_BUFFER_BIT);
    }

    /// Draw the uploaded scene for one view: panels (depth-writing),
    /// then streams, then text (depth-tested, non-writing).
    pub fn draw_view(&self, view_proj: &Mat4, viewport: (i32, i32, i32, i32)) {
        let gl = &self.gl;
        let (x, y, w, h) = viewport;
        gl.viewport(x, y, w, h);

        if !self.panels.is_empty() {
            gl.use_program(Some(&self.panel.program));
            gl.uniform_matrix4fv_with_f32_array(Some(&self.panel.u_view_proj), false, &view_proj.0);
            gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_unit));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_with_i32(0, 2, Gl::FLOAT, false, 8, 0);
            gl.disable_vertex_attrib_array(1);
            for p in &self.panels {
                gl.uniform3f(
                    Some(&self.panel.u_center),
                    p.center.x,
                    p.center.y,
                    p.center.z,
                );
                gl.uniform3f(Some(&self.panel.u_right), p.right.x, p.right.y, p.right.z);
                gl.uniform3f(Some(&self.panel.u_up), p.up.x, p.up.y, p.up.z);
                gl.uniform2f(Some(&self.panel.u_half), p.half_w, p.half_h);
                gl.uniform1f(
                    Some(&self.panel.u_radius),
                    p.radius.min(p.half_w).min(p.half_h),
                );
                gl.uniform4f(
                    Some(&self.panel.u_fill),
                    p.fill[0],
                    p.fill[1],
                    p.fill[2],
                    p.fill[3],
                );
                gl.uniform4f(
                    Some(&self.panel.u_border),
                    p.border[0],
                    p.border[1],
                    p.border[2],
                    p.border[3],
                );
                gl.uniform1f(Some(&self.panel.u_border_w), p.border_w);
                gl.draw_arrays(Gl::TRIANGLES, 0, 6);
            }
        }

        if !self.monitors.is_empty() {
            gl.use_program(Some(&self.video.program));
            gl.uniform_matrix4fv_with_f32_array(Some(&self.video.u_view_proj), false, &view_proj.0);
            gl.active_texture(Gl::TEXTURE0);
            gl.uniform1i(Some(&self.video.u_tex), 0);
            gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_unit));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_with_i32(0, 2, Gl::FLOAT, false, 8, 0);
            gl.disable_vertex_attrib_array(1);
            for m in &self.monitors {
                let Some(texture) = self.video_textures.get(&m.id) else {
                    continue;
                };
                gl.bind_texture(Gl::TEXTURE_2D, Some(texture));
                gl.uniform3f(
                    Some(&self.video.u_center),
                    m.center.x,
                    m.center.y,
                    m.center.z,
                );
                gl.uniform3f(Some(&self.video.u_right), m.right.x, m.right.y, m.right.z);
                gl.uniform3f(Some(&self.video.u_up), m.up.x, m.up.y, m.up.z);
                gl.uniform2f(Some(&self.video.u_half), m.half_w, m.half_h);
                gl.draw_arrays(Gl::TRIANGLES, 0, 6);
            }
        }

        if self.tri_count > 0 || self.line_count > 0 || self.pointer_count > 0 {
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
            if self.pointer_count > 0 {
                gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_pointer));
                bind_pos_color_layout(gl);
                gl.draw_arrays(Gl::LINES, 0, self.pointer_count);
            }
        }

        if self.text_count > 0 {
            if let Some(atlas) = self.atlas.as_ref() {
                gl.use_program(Some(&self.text.program));
                gl.uniform_matrix4fv_with_f32_array(
                    Some(&self.text.u_view_proj),
                    false,
                    &view_proj.0,
                );
                gl.active_texture(Gl::TEXTURE0);
                gl.bind_texture(Gl::TEXTURE_2D, Some(atlas.texture()));
                gl.uniform1i(Some(&self.text.u_atlas), 0);
                gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo_text));
                let stride = (TEXT_VERTEX_STRIDE * 4) as i32;
                gl.enable_vertex_attrib_array(0);
                gl.vertex_attrib_pointer_with_i32(0, 3, Gl::FLOAT, false, stride, 0);
                gl.enable_vertex_attrib_array(1);
                gl.vertex_attrib_pointer_with_i32(1, 2, Gl::FLOAT, false, stride, 12);
                gl.enable_vertex_attrib_array(2);
                gl.vertex_attrib_pointer_with_i32(2, 4, Gl::FLOAT, false, stride, 20);
                gl.depth_mask(false);
                gl.draw_arrays(Gl::TRIANGLES, 0, self.text_count);
                gl.depth_mask(true);
                gl.disable_vertex_attrib_array(2);
            }
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

fn uniform(gl: &Gl, program: &WebGlProgram, name: &str) -> Result<WebGlUniformLocation, JsValue> {
    gl.get_uniform_location(program, name)
        .ok_or_else(|| JsValue::from_str(&format!("xr-web: uniform {name} missing")))
}

fn compile_program(gl: &Gl, vs: &str, fs: &str) -> Result<WebGlProgram, JsValue> {
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
    Ok(program)
}

fn compile_shader(gl: &Gl, kind: u32, source: &str) -> Result<web_sys::WebGlShader, JsValue> {
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
