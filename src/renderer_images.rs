// src/renderer_images.rs
//
// Draws inline images (src/graphics.rs) for the renderer: one textured
// quad per image strip, with textures uploaded on first use and cached
// per image (several placements of one image share a texture). Textures
// unused for a while are dropped.

use std::collections::HashMap;
use std::sync::Arc;

use wgpu::util::DeviceExt;

use crate::graphics::Image;
use crate::renderer::Rect;

/// One strip of an image to draw: where (pixels), and which part of the
/// image (u0, v0, u1, v1 in 0..1).
pub struct ImageDraw {
    pub image: Arc<Image>,
    pub dst: Rect,
    pub uv: [f32; 4],
}

const SHADER: &str = r#"
struct Uniforms {
    screen_size: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct InstanceInput {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: InstanceInput) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index];
    let px = instance.rect.x + corner.x * instance.rect.z;
    let py = instance.rect.y + corner.y * instance.rect.w;
    var out: VertexOutput;
    out.clip_position = vec4<f32>(
        (px / u.screen_size.x) * 2.0 - 1.0,
        1.0 - (py / u.screen_size.y) * 2.0,
        0.0,
        1.0,
    );
    out.uv = vec2<f32>(
        mix(instance.uv.x, instance.uv.z, corner.x),
        mix(instance.uv.y, instance.uv.w, corner.y),
    );
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

/// Frames a texture may go unused before it's dropped.
const KEEP_FRAMES: u64 = 300;

struct Cached {
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

pub struct ImagePainter {
    pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture_format: wgpu::TextureFormat,
    textures: HashMap<u64, Cached>,
    frame: u64,
}

impl ImagePainter {
    /// `uniform_layout` is the quad pipeline's (screen size) layout.
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        uniform_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cyberterm image shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cyberterm image texture layout"),
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cyberterm image pipeline layout"),
            bind_group_layouts: &[uniform_layout, &texture_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cyberterm image pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 32,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // Images are straight alpha; this writes correct
                    // premultiplied results over the (premultiplied) frame.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("cyberterm image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // Match how the rest of the frame treats color: sRGB surfaces
        // decode/encode, others take the bytes as they are.
        let texture_format = if format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        Self {
            pipeline,
            texture_layout,
            sampler,
            texture_format,
            textures: HashMap::new(),
            frame: 0,
        }
    }

    fn ensure(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, image: &Image) {
        if let Some(c) = self.textures.get_mut(&image.key) {
            c.last_used = self.frame;
            return;
        }
        let size = wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("cyberterm image"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.texture_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &image.rgba,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cyberterm image bind group"),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.textures.insert(
            image.key,
            Cached {
                bind_group,
                last_used: self.frame,
            },
        );
    }

    /// Uploads what's needed and returns the instance buffer for `draws`
    /// (call before the render pass; then `draw` inside it).
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        draws: &[ImageDraw],
    ) -> Option<wgpu::Buffer> {
        self.frame += 1;
        let frame = self.frame;
        self.textures
            .retain(|_, c| frame.saturating_sub(c.last_used) < KEEP_FRAMES);
        if draws.is_empty() {
            return None;
        }
        let mut data: Vec<f32> = Vec::with_capacity(draws.len() * 8);
        for d in draws {
            self.ensure(device, queue, &d.image);
            data.extend_from_slice(&[d.dst.x, d.dst.y, d.dst.w, d.dst.h]);
            data.extend_from_slice(&d.uv);
        }
        let bytes: Vec<u8> = data.iter().flat_map(|f| f.to_le_bytes()).collect();
        Some(
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cyberterm image quads"),
                contents: &bytes,
                usage: wgpu::BufferUsages::VERTEX,
            }),
        )
    }

    pub fn draw<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        uniforms: &'a wgpu::BindGroup,
        buffer: &'a wgpu::Buffer,
        draws: &[ImageDraw],
    ) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, uniforms, &[]);
        pass.set_vertex_buffer(0, buffer.slice(..));
        for (i, d) in draws.iter().enumerate() {
            if let Some(c) = self.textures.get(&d.image.key) {
                pass.set_bind_group(1, &c.bind_group, &[]);
                let i = i as u32;
                pass.draw(0..6, i..i + 1);
            }
        }
    }
}
