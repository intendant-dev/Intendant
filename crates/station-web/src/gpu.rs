//! WebGPU renderer state and the per-frame geometry buffers.

use bytemuck::{Pod, Zeroable};
use std::f32::consts::PI;
use wasm_bindgen::JsValue;
#[cfg(target_arch = "wasm32")]
use web_sys::HtmlCanvasElement;

use crate::panes::{PaneTarget, PaneZone};
use crate::scene::{rotate_x, rotate_y, Plane, ProjectedNode, Vec2, Vec3};
use crate::text_atlas::TextAtlas;
use crate::util::Color;

#[cfg(target_arch = "wasm32")]
pub(crate) struct GpuState {
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) config: wgpu::SurfaceConfiguration,
    pub(crate) line_pipeline: wgpu::RenderPipeline,
    pub(crate) tri_pipeline: wgpu::RenderPipeline,
    /// World-space panes (Phase C): same shader and vertex layout, but a
    /// real depth compare — the wireframe's written depth occludes panes
    /// and panes occlude it.
    pub(crate) pane_pipeline: wgpu::RenderPipeline,
    /// Glyph-atlas text on those panes (slice 3): textured quads sampled
    /// from the atlas, depth-tested but never depth-written.
    pub(crate) text_pipeline: wgpu::RenderPipeline,
    /// Persistent vertex buffers, uploaded via `Queue::write_buffer` and
    /// grown geometrically on demand; never recreated per frame.
    pub(crate) line_buffer: GpuVertexBuffer,
    pub(crate) tri_buffer: GpuVertexBuffer,
    pub(crate) pane_buffer: GpuVertexBuffer,
    pub(crate) text_buffer: GpuVertexBuffer,
    /// Depth attachment, always sized to `config`. Must be recreated
    /// wherever the surface is resized or every later frame renders
    /// against a stale-sized attachment.
    pub(crate) depth_view: wgpu::TextureView,
    /// Glyph-atlas binding — the crate's only bind group. The layout and
    /// sampler exist from init; the texture + bind group are built by
    /// `ensure_atlas` on the first frame that carries text.
    pub(crate) atlas_layout: wgpu::BindGroupLayout,
    pub(crate) atlas_sampler: wgpu::Sampler,
    pub(crate) atlas_bind: Option<wgpu::BindGroup>,
}

#[cfg(target_arch = "wasm32")]
pub(crate) struct GpuVertexBuffer {
    pub(crate) label: &'static str,
    pub(crate) buffer: wgpu::Buffer,
    pub(crate) capacity: u64,
}

#[cfg(target_arch = "wasm32")]
impl GpuVertexBuffer {
    /// Comfortably holds a typical scene; grows if a frame outsizes it.
    pub(crate) const INITIAL_CAPACITY: u64 = 256 * 1024;

    pub(crate) fn new(device: &wgpu::Device, label: &'static str) -> Self {
        Self {
            label,
            buffer: Self::create(device, label, Self::INITIAL_CAPACITY),
            capacity: Self::INITIAL_CAPACITY,
        }
    }

    pub(crate) fn create(
        device: &wgpu::Device,
        label: &'static str,
        capacity: u64,
    ) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Upload this frame's vertices, growing the buffer if needed.
    pub(crate) fn upload<V: Pod>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        vertices: &[V],
    ) {
        if vertices.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(vertices);
        let needed = bytes.len() as u64;
        if needed > self.capacity {
            self.capacity = needed.next_power_of_two();
            self.buffer = Self::create(device, self.label, self.capacity);
        }
        queue.write_buffer(&self.buffer, 0, bytes);
    }
}

#[cfg(target_arch = "wasm32")]
impl GpuState {
    pub(crate) async fn new(canvas: HtmlCanvasElement) -> Result<Self, JsValue> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::BROWSER_WEBGPU;
        let instance = wgpu::Instance::new(instance_desc);
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| JsValue::from_str(&format!("create WebGPU surface failed: {e:?}")))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("no WebGPU adapter available: {e:?}")))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("Intendant Station Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                ..Default::default()
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("request WebGPU device failed: {e:?}")))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Shader/pipeline validation errors surface asynchronously on
        // WebGPU; without an error scope a broken shader yields pipelines
        // that silently no-op every render pass while init "succeeds".
        // Scope the whole pipeline setup so we fail loudly into the canvas
        // fallback instead.
        let error_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Station Shader"),
            source: wgpu::ShaderSource::Wgsl(STATION_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Station Pipeline Layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let make_pipeline = |topology, depth_compare: wgpu::CompareFunction| {
            let vertex_layout = GpuVertex::layout();
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Station Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[vertex_layout],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                // The wireframe pipelines keep compare Always (their
                // painter's-order alpha blending predates depth and must
                // stay untouched) while still WRITING depth; the pane
                // pipeline runs a real compare against those values.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(depth_compare),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let line_pipeline = make_pipeline(
            wgpu::PrimitiveTopology::LineList,
            wgpu::CompareFunction::Always,
        );
        let tri_pipeline = make_pipeline(
            wgpu::PrimitiveTopology::TriangleList,
            wgpu::CompareFunction::Always,
        );
        let pane_pipeline = make_pipeline(
            wgpu::PrimitiveTopology::TriangleList,
            wgpu::CompareFunction::LessEqual,
        );

        // The text pipeline is the one consumer of a bind group: the glyph
        // atlas texture + sampler. It tests against the depth the panes and
        // wireframe wrote but never writes its own — glyph quads are mostly
        // transparent, and writing would occlude by invisible bounding
        // boxes.
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Station Atlas Layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let text_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Station Text Pipeline Layout"),
            bind_group_layouts: &[Some(&atlas_layout)],
            immediate_size: 0,
        });
        let text_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Station Text Pipeline"),
            layout: Some(&text_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_text"),
                buffers: &[TextVertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_text"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Station Atlas Sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Trilinear across the CPU-baked mip chain: pane text draws
            // well below the baked glyph size, where bilinear-only
            // sampling visibly drops thin strokes.
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        if let Some(error) = error_scope.pop().await {
            return Err(JsValue::from_str(&format!(
                "WebGPU pipeline validation failed: {error}"
            )));
        }
        let line_buffer = GpuVertexBuffer::new(&device, "Station Lines");
        let tri_buffer = GpuVertexBuffer::new(&device, "Station Triangles");
        let pane_buffer = GpuVertexBuffer::new(&device, "Station Panes");
        let text_buffer = GpuVertexBuffer::new(&device, "Station Text");
        let depth_view = Self::create_depth_view(&device, width, height);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            line_pipeline,
            tri_pipeline,
            pane_pipeline,
            text_pipeline,
            line_buffer,
            tri_buffer,
            pane_buffer,
            text_buffer,
            depth_view,
            atlas_layout,
            atlas_sampler,
            atlas_bind: None,
        })
    }

    /// Create the glyph-atlas texture + bind group from the CPU-side bake,
    /// uploading the full mip chain. Idempotent per GpuState — a rebuilt
    /// GpuState (context loss) simply re-uploads on its next text frame.
    pub(crate) fn ensure_atlas(&mut self, atlas: &TextAtlas) {
        if self.atlas_bind.is_some() {
            return;
        }
        let mips = atlas.mip_chain();
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Station Glyph Atlas"),
            size: wgpu::Extent3d {
                width: atlas.width,
                height: atlas.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, (width, height, pixels)) in mips.iter().enumerate() {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(*width),
                    rows_per_image: Some(*height),
                },
                wgpu::Extent3d {
                    width: *width,
                    height: *height,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.atlas_bind = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Station Glyph Atlas Bind"),
            layout: &self.atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.atlas_sampler),
                },
            ],
        }));
    }

    fn create_depth_view(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Station Depth"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        texture.create_view(&wgpu::TextureViewDescriptor::default())
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if width == self.config.width && height == self.config.height {
            return;
        }
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        self.depth_view =
            Self::create_depth_view(&self.device, self.config.width, self.config.height);
    }

    pub(crate) fn render(
        &mut self,
        frame: &GpuFrame,
        atlas: Option<&TextAtlas>,
    ) -> Result<(), JsValue> {
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output)
            | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(output)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
                    state => {
                        return Err(JsValue::from_str(&format!(
                            "surface unavailable after reconfigure: {state:?}"
                        )))
                    }
                }
            }
            state => {
                return Err(JsValue::from_str(&format!(
                    "surface unavailable: {state:?}"
                )))
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Station Encoder"),
            });

        self.line_buffer
            .upload(&self.device, &self.queue, &frame.line_vertices);
        self.tri_buffer
            .upload(&self.device, &self.queue, &frame.tri_vertices);
        self.pane_buffer
            .upload(&self.device, &self.queue, &frame.pane_vertices);
        self.text_buffer
            .upload(&self.device, &self.queue, &frame.text_vertices);
        if let Some(atlas) = atlas.filter(|_| !frame.text_vertices.is_empty()) {
            self.ensure_atlas(atlas);
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Station Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.030,
                            g: 0.030,
                            b: 0.055,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    // Nothing reads the attachment after the pass, so the
                    // store is discarded; within-pass depth tests (the pane
                    // pipelines) are unaffected by the store op.
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !frame.line_vertices.is_empty() {
                let bytes = std::mem::size_of_val(frame.line_vertices.as_slice()) as u64;
                pass.set_pipeline(&self.line_pipeline);
                pass.set_vertex_buffer(0, self.line_buffer.buffer.slice(..bytes));
                pass.draw(0..frame.line_vertices.len() as u32, 0..1);
            }
            if !frame.tri_vertices.is_empty() {
                let bytes = std::mem::size_of_val(frame.tri_vertices.as_slice()) as u64;
                pass.set_pipeline(&self.tri_pipeline);
                pass.set_vertex_buffer(0, self.tri_buffer.buffer.slice(..bytes));
                pass.draw(0..frame.tri_vertices.len() as u32, 0..1);
            }
            // Panes draw after the wireframe: it has written its depth by
            // now, so the pane pipeline's LessEqual compare sorts panes
            // against the whole scene per-pixel.
            if !frame.pane_vertices.is_empty() {
                let bytes = std::mem::size_of_val(frame.pane_vertices.as_slice()) as u64;
                pass.set_pipeline(&self.pane_pipeline);
                pass.set_vertex_buffer(0, self.pane_buffer.buffer.slice(..bytes));
                pass.draw(0..frame.pane_vertices.len() as u32, 0..1);
            }
            // Text draws last, over its pane, against everything's depth.
            // Skipped until the atlas bind exists (first text frame builds
            // it just above, so in practice this never lags a frame).
            if !frame.text_vertices.is_empty() {
                if let Some(bind) = self.atlas_bind.as_ref() {
                    let bytes = std::mem::size_of_val(frame.text_vertices.as_slice()) as u64;
                    pass.set_pipeline(&self.text_pipeline);
                    pass.set_bind_group(0, bind, &[]);
                    pass.set_vertex_buffer(0, self.text_buffer.buffer.slice(..bytes));
                    pass.draw(0..frame.text_vertices.len() as u32, 0..1);
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct GpuState;

#[cfg(not(target_arch = "wasm32"))]
impl GpuState {
    pub(crate) fn resize(&mut self, _width: u32, _height: u32) {}

    pub(crate) fn render(
        &mut self,
        _frame: &GpuFrame,
        _atlas: Option<&TextAtlas>,
    ) -> Result<(), JsValue> {
        Ok(())
    }
}

/// Depth attachment format, shared by the pipelines and the texture.
#[cfg(target_arch = "wasm32")]
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

#[cfg(target_arch = "wasm32")]
const STATION_WGSL: &str = r#"
struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
  @location(0) position: vec2<f32>,
  @location(1) depth: f32,
  @location(2) color: vec4<f32>,
) -> VertexOut {
  var out: VertexOut;
  out.position = vec4<f32>(position, depth, 1.0);
  out.color = color;
  return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  return in.color;
}

// Glyph-atlas text (Phase C slice 3): the atlas is an R8 coverage mask
// (white text baked on transparent), so the sample's red channel scales
// the vertex color's alpha and the tint stays fully vertex-driven.
@group(0) @binding(0) var atlas_tex: texture_2d<f32>;
@group(0) @binding(1) var atlas_smp: sampler;

struct TextOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
};

@vertex
fn vs_text(
  @location(0) position: vec2<f32>,
  @location(1) depth: f32,
  @location(2) uv: vec2<f32>,
  @location(3) color: vec4<f32>,
) -> TextOut {
  var out: TextOut;
  out.position = vec4<f32>(position, depth, 1.0);
  out.uv = uv;
  out.color = color;
  return out;
}

@fragment
fn fs_text(in: TextOut) -> @location(0) vec4<f32> {
  let coverage = textureSample(atlas_tex, atlas_smp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * coverage);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuVertex {
    pub(crate) pos: [f32; 2],
    /// Clip-space z in [0, 1) — `scene::ndc_depth` of the view depth for
    /// projected geometry, 0.0 (nearest) for screen-space geometry.
    /// Out-of-range values clip the vertex, so producers stay inside the
    /// helper.
    pub(crate) depth: f32,
    pub(crate) color: [f32; 4],
}

impl GpuVertex {
    #[cfg(target_arch = "wasm32")]
    pub(crate) const ATTRS: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32x4];

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn layout<'a>() -> wgpu::VertexBufferLayout<'a> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

/// Vertex for the textured text pipeline: `GpuVertex`'s NDC-plus-clip-depth
/// scheme with an atlas UV. Producers stay inside `text_atlas`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct TextVertex {
    pub(crate) pos: [f32; 2],
    pub(crate) depth: f32,
    pub(crate) uv: [f32; 2],
    pub(crate) color: [f32; 4],
}

impl TextVertex {
    #[cfg(target_arch = "wasm32")]
    pub(crate) const ATTRS: [wgpu::VertexAttribute; 4] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32x2, 3 => Float32x4];

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn layout<'a>() -> wgpu::VertexBufferLayout<'a> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TextVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

#[derive(Default)]
pub(crate) struct GpuFrame {
    pub(crate) line_vertices: Vec<GpuVertex>,
    pub(crate) tri_vertices: Vec<GpuVertex>,
    /// World-space pane geometry (panes.rs) — drawn through the
    /// depth-tested pane pipeline, after the wireframe.
    pub(crate) pane_vertices: Vec<GpuVertex>,
    /// Glyph quads on those panes (text_atlas.rs) — drawn last through
    /// the textured text pipeline.
    pub(crate) text_vertices: Vec<TextVertex>,
    pub(crate) projected_nodes: Vec<ProjectedNode>,
    /// World-space pick targets for the panes — the raycast counterpart
    /// of `projected_nodes` (`input::pick_pane` intersects pointer rays
    /// with these).
    pub(crate) pane_targets: Vec<PaneTarget>,
    /// Projected screen rects for pane pills (panes.rs) — adopted into
    /// `hit_zones` by the HUD pass when a world pane replaces the
    /// screen focus panel.
    pub(crate) pane_zones: Vec<PaneZone>,
}

impl GpuFrame {
    /// Empty the frame while keeping the buffers' capacity for reuse.
    pub(crate) fn clear(&mut self) {
        self.line_vertices.clear();
        self.tri_vertices.clear();
        self.pane_vertices.clear();
        self.text_vertices.clear();
        self.projected_nodes.clear();
        self.pane_targets.clear();
        self.pane_zones.clear();
    }

    pub(crate) fn add_line_ndc(&mut self, a: Vec2, za: f32, b: Vec2, zb: f32, color: Color) {
        self.line_vertices.push(GpuVertex {
            pos: [a.x, a.y],
            depth: za,
            color: color.into(),
        });
        self.line_vertices.push(GpuVertex {
            pos: [b.x, b.y],
            depth: zb,
            color: color.into(),
        });
    }

    /// Project and append one line segment. The projector returns the NDC
    /// position, a depth-cue brightness multiplier — the segment's alpha
    /// is scaled by the endpoints' mean so nearer edges draw brighter —
    /// and the clip depth written to the depth attachment.
    pub(crate) fn add_line_projected(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        a: Vec3,
        b: Vec3,
        color: Color,
    ) {
        if let (Some((pa, ca, za)), Some((pb, cb, zb))) = (project(a), project(b)) {
            let cue = (ca + cb) * 0.5;
            self.add_line_ndc(pa, za, pb, zb, color.with_alpha((color.a * cue).min(1.0)));
        }
    }

    /// Two-pass glow segment: a thick low-alpha quad under a thin bright
    /// line. Same pipelines, just extra vertices — far cheaper than any
    /// post-process blur. `width` is the quad half-width in NDC.
    pub(crate) fn add_glow_line_ndc(
        &mut self,
        a: Vec2,
        za: f32,
        b: Vec2,
        zb: f32,
        color: Color,
        width: f32,
    ) {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let len = (dx * dx + dy * dy).sqrt();
        if len > 1e-6 {
            let px = -dy / len * width;
            let py = dx / len * width;
            let halo: [f32; 4] = color.with_alpha(color.a * 0.17).into();
            let quad = [
                ([a.x - px, a.y - py], za),
                ([b.x - px, b.y - py], zb),
                ([b.x + px, b.y + py], zb),
                ([a.x - px, a.y - py], za),
                ([b.x + px, b.y + py], zb),
                ([a.x + px, a.y + py], za),
            ];
            for (pos, depth) in quad {
                self.tri_vertices.push(GpuVertex {
                    pos,
                    depth,
                    color: halo,
                });
            }
        }
        self.add_line_ndc(a, za, b, zb, color);
    }

    pub(crate) fn add_quad_ndc(&mut self, x: f32, y: f32, size: f32, color: [f32; 4], depth: f32) {
        let s = size;
        let verts = [
            [x - s, y - s],
            [x + s, y - s],
            [x + s, y + s],
            [x - s, y - s],
            [x + s, y + s],
            [x - s, y + s],
        ];
        for pos in verts {
            self.tri_vertices.push(GpuVertex { pos, depth, color });
        }
    }

    pub(crate) fn add_ring(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        radius: f32,
        color: Color,
        plane: Plane,
    ) {
        self.ring_inner(project, center, radius, color, plane, None);
    }

    /// Ring with the two-pass glow treatment on every segment.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_glow_ring(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        radius: f32,
        color: Color,
        plane: Plane,
        width: f32,
    ) {
        self.ring_inner(project, center, radius, color, plane, Some(width));
    }

    #[allow(clippy::too_many_arguments)]
    fn ring_inner(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        radius: f32,
        color: Color,
        plane: Plane,
        glow_width: Option<f32>,
    ) {
        let seg = 64;
        let mut prev = None;
        for i in 0..=seg {
            let t = i as f32 / seg as f32 * PI * 2.0;
            let local = match plane {
                Plane::XY => Vec3::new(t.cos() * radius, t.sin() * radius, 0.0),
                Plane::XZ => Vec3::new(t.cos() * radius, 0.0, t.sin() * radius),
                Plane::YZ => Vec3::new(0.0, t.cos() * radius, t.sin() * radius),
            };
            let p = center + local;
            if let Some(prev_p) = prev {
                match glow_width {
                    Some(width) => {
                        if let (Some((pa, ca, za)), Some((pb, cb, zb))) =
                            (project(prev_p), project(p))
                        {
                            let cue = (ca + cb) * 0.5;
                            self.add_glow_line_ndc(
                                pa,
                                za,
                                pb,
                                zb,
                                color.with_alpha((color.a * cue).min(1.0)),
                                width,
                            );
                        }
                    }
                    None => self.add_line_projected(project, prev_p, p, color),
                }
            }
            prev = Some(p);
        }
    }

    pub(crate) fn add_wire_octa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, -1.0, 0.0),
        ];
        let edges = [
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (5, 1),
            (5, 2),
            (5, 3),
            (5, 4),
            (1, 2),
            (2, 3),
            (3, 4),
            (4, 1),
        ];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    pub(crate) fn add_wire_tetra(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(-1.0, -1.0, 1.0),
            Vec3::new(-1.0, 1.0, -1.0),
            Vec3::new(1.0, -1.0, -1.0),
        ];
        let edges = [(0, 1), (0, 2), (0, 3), (1, 2), (2, 3), (3, 1)];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    pub(crate) fn add_wire_icosa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let phi = 1.618;
        let verts = [
            Vec3::new(-1.0, phi, 0.0),
            Vec3::new(1.0, phi, 0.0),
            Vec3::new(-1.0, -phi, 0.0),
            Vec3::new(1.0, -phi, 0.0),
            Vec3::new(0.0, -1.0, phi),
            Vec3::new(0.0, 1.0, phi),
            Vec3::new(0.0, -1.0, -phi),
            Vec3::new(0.0, 1.0, -phi),
            Vec3::new(phi, 0.0, -1.0),
            Vec3::new(phi, 0.0, 1.0),
            Vec3::new(-phi, 0.0, -1.0),
            Vec3::new(-phi, 0.0, 1.0),
        ];
        let edges = [
            (0, 1),
            (0, 5),
            (0, 7),
            (0, 10),
            (0, 11),
            (1, 5),
            (1, 7),
            (1, 8),
            (1, 9),
            (2, 3),
            (2, 4),
            (2, 6),
            (2, 10),
            (2, 11),
            (3, 4),
            (3, 6),
            (3, 8),
            (3, 9),
            (4, 5),
            (4, 9),
            (4, 11),
            (5, 9),
            (5, 11),
            (6, 7),
            (6, 8),
            (6, 10),
            (7, 8),
            (7, 10),
            (8, 9),
            (10, 11),
        ];
        self.add_edges(project, center, scale * 0.55, spin, &verts, &edges, color);
    }

    pub(crate) fn add_wire_hex(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        radius: f32,
        height: f32,
        spin: f32,
        color: Color,
    ) {
        let mut top = Vec::with_capacity(6);
        let mut bottom = Vec::with_capacity(6);
        for i in 0..6 {
            let a = i as f32 / 6.0 * PI * 2.0 + spin;
            top.push(center + Vec3::new(a.cos() * radius, height * 0.5, a.sin() * radius));
            bottom.push(center + Vec3::new(a.cos() * radius, -height * 0.5, a.sin() * radius));
        }
        for i in 0..6 {
            let n = (i + 1) % 6;
            self.add_line_projected(project, top[i], top[n], color);
            self.add_line_projected(
                project,
                bottom[i],
                bottom[n],
                color.with_alpha(color.a * 0.7),
            );
            self.add_line_projected(project, top[i], bottom[i], color.with_alpha(color.a * 0.6));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_edges(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<(Vec2, f32, f32)>,
        center: Vec3,
        scale: f32,
        spin: f32,
        verts: &[Vec3],
        edges: &[(usize, usize)],
        color: Color,
    ) {
        let transformed = verts
            .iter()
            .map(|v| center + rotate_y(rotate_x(*v * scale, spin * 0.7), spin))
            .collect::<Vec<_>>();
        for (a, b) in edges {
            self.add_line_projected(project, transformed[*a], transformed[*b], color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Color;

    #[test]
    fn quads_push_two_triangles() {
        let mut frame = GpuFrame::default();
        frame.add_quad_ndc(0.0, 0.0, 0.1, [1.0, 1.0, 1.0, 1.0], 0.25);
        assert_eq!(frame.tri_vertices.len(), 6);
        assert!(frame.line_vertices.is_empty());
        assert!(frame.tri_vertices.iter().all(|v| v.depth == 0.25));
    }

    #[test]
    fn lines_push_vertex_pairs_and_projection_culls() {
        let mut frame = GpuFrame::default();
        let color = Color::rgb(255, 0, 0);
        frame.add_line_ndc(Vec2::new(0.0, 0.0), 0.0, Vec2::new(1.0, 1.0), 0.0, color);
        assert_eq!(frame.line_vertices.len(), 2);
        // A projector that culls everything adds nothing.
        let mut cull = |_: Vec3| -> Option<(Vec2, f32, f32)> { None };
        frame.add_line_projected(&mut cull, Vec3::ZERO, Vec3::Y, color);
        assert_eq!(frame.line_vertices.len(), 2);
    }

    #[test]
    fn projected_lines_apply_depth_cue_to_alpha() {
        let mut frame = GpuFrame::default();
        let mut dim = |v: Vec3| Some((Vec2::new(v.x, v.y), 0.5, 0.5));
        frame.add_line_projected(&mut dim, Vec3::ZERO, Vec3::Y, Color::rgb(255, 0, 0));
        assert!((frame.line_vertices[0].color[3] - 0.5).abs() < 1e-6);
        // A cue above 1.0 brightens but never exceeds full alpha.
        let mut hot = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.5, 0.5));
        frame.add_line_projected(&mut hot, Vec3::ZERO, Vec3::Y, Color::rgb(255, 0, 0));
        assert_eq!(frame.line_vertices[2].color[3], 1.0);
    }

    #[test]
    fn projected_depth_reaches_the_vertices() {
        let mut frame = GpuFrame::default();
        // Per-endpoint depth: y=0 endpoint near, y=1 endpoint far.
        let mut slope = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.1 + v.y * 0.6));
        frame.add_line_projected(&mut slope, Vec3::ZERO, Vec3::Y, Color::rgb(255, 0, 0));
        assert!((frame.line_vertices[0].depth - 0.1).abs() < 1e-6);
        assert!((frame.line_vertices[1].depth - 0.7).abs() < 1e-6);
        // The glow halo's quad corners inherit their own endpoint's depth.
        frame.add_glow_line_ndc(
            Vec2::new(0.0, 0.0),
            0.2,
            Vec2::new(1.0, 0.0),
            0.8,
            Color::rgb(0, 0, 255),
            0.01,
        );
        let quad = &frame.tri_vertices[..6];
        assert_eq!(
            quad.iter().map(|v| v.depth).collect::<Vec<_>>(),
            vec![0.2, 0.8, 0.8, 0.2, 0.8, 0.2]
        );
    }

    #[test]
    fn ring_segments_share_endpoints() {
        let mut frame = GpuFrame::default();
        let mut identity = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.5));
        frame.add_ring(
            &mut identity,
            Vec3::ZERO,
            1.0,
            Color::rgb(0, 255, 0),
            Plane::XY,
        );
        // 64 segments, two vertices each.
        assert_eq!(frame.line_vertices.len(), 128);
    }

    #[test]
    fn glow_lines_add_quad_plus_bright_core() {
        let mut frame = GpuFrame::default();
        let color = Color::rgb(0, 0, 255);
        frame.add_glow_line_ndc(
            Vec2::new(0.0, 0.0),
            0.0,
            Vec2::new(1.0, 0.0),
            0.0,
            color,
            0.01,
        );
        assert_eq!(frame.tri_vertices.len(), 6, "thick halo quad");
        assert_eq!(frame.line_vertices.len(), 2, "thin bright core");
        assert!(frame.tri_vertices[0].color[3] < frame.line_vertices[0].color[3]);
        // Degenerate (zero-length) glow segments skip the quad, not crash.
        frame.add_glow_line_ndc(
            Vec2::new(0.5, 0.5),
            0.0,
            Vec2::new(0.5, 0.5),
            0.0,
            color,
            0.01,
        );
        assert_eq!(frame.tri_vertices.len(), 6);
        assert_eq!(frame.line_vertices.len(), 4);

        let mut glow_ring = GpuFrame::default();
        let mut identity = |v: Vec3| Some((Vec2::new(v.x, v.y), 1.0, 0.5));
        glow_ring.add_glow_ring(&mut identity, Vec3::ZERO, 1.0, color, Plane::XY, 0.01);
        assert_eq!(glow_ring.line_vertices.len(), 128);
        assert_eq!(glow_ring.tri_vertices.len(), 64 * 6);
    }

    #[test]
    fn clear_empties_but_keeps_capacity() {
        let mut frame = GpuFrame::default();
        frame.add_quad_ndc(0.0, 0.0, 0.1, [1.0; 4], 0.0);
        frame.text_vertices.push(TextVertex {
            pos: [0.0, 0.0],
            depth: 0.5,
            uv: [0.0, 0.0],
            color: [1.0; 4],
        });
        frame.pane_targets.push(crate::panes::PaneTarget {
            id: "op".into(),
            anchor: Vec3::ZERO,
            right: Vec3::new(1.0, 0.0, 0.0),
            up: Vec3::Y,
            half_w: 1.0,
            half_h: 1.0,
        });
        let cap = frame.tri_vertices.capacity();
        frame.clear();
        assert!(frame.tri_vertices.is_empty());
        assert!(frame.line_vertices.is_empty());
        assert!(frame.text_vertices.is_empty());
        assert!(frame.projected_nodes.is_empty());
        assert!(frame.pane_targets.is_empty());
        assert_eq!(frame.tri_vertices.capacity(), cap);
    }
}
