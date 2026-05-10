//! Post-processing: render the cell grid to an offscreen texture, then run
//! a full-screen shader (CRT effect) into the surface. The CRT pass also
//! writes a copy of its output into a ping-pong "history" texture so the
//! next frame can sample it as phosphor afterglow.

use wgpu::{
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType,
    BlendState, Buffer, BufferBindingType, BufferUsages, ColorTargetState, ColorWrites,
    CommandEncoder, CommandEncoderDescriptor, Device, Extent3d, FilterMode, FragmentState,
    LoadOp, MipmapFilterMode, MultisampleState, Operations, PipelineLayoutDescriptor,
    PrimitiveState, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor,
    ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp, Texture, TextureDescriptor,
    TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureView,
    TextureViewDescriptor, TextureViewDimension, VertexState,
};
use wgpu::util::{BufferInitDescriptor, DeviceExt};

use crate::color::{default_bg, Rgb};

pub struct PostProcessor {
    /// Offscreen color target. The cell grid renders into this; the
    /// post-pass samples it.
    texture: Texture,
    view: TextureView,
    /// Ping-pong history textures for phosphor persistence. Each frame the
    /// CRT pass reads `history[history_idx]` (the previous frame's CRT
    /// output) and writes the current frame into the other slot via MRT.
    /// Then `history_idx` flips so next frame reads what we just wrote.
    history: [Texture; 2],
    history_views: [TextureView; 2],
    history_idx: usize,
    sampler: Sampler,
    /// Uniform buffer holding the theme background as a linear-space
    /// vec4 — shaders can lerp toward it for the corner fade.
    uniforms: Buffer,
    bind_group_layout: BindGroupLayout,
    /// One bind group per ping-pong direction. Index `i` binds
    /// `history[i]` as the "previous" sample source.
    bind_groups: [BindGroup; 2],
    pipeline: RenderPipeline,
    width: u32,
    height: u32,
    format: TextureFormat,
}

impl PostProcessor {
    pub fn new(
        device: &Device,
        queue: &Queue,
        format: TextureFormat,
        width: u32,
        height: u32,
        wgsl: &str,
    ) -> Self {
        let (texture, view) = create_offscreen(device, format, width, height, "post-offscreen");
        let (h0_tex, h0_view) = create_offscreen(device, format, width, height, "post-history-0");
        let (h1_tex, h1_view) = create_offscreen(device, format, width, height, "post-history-1");
        clear_history(device, queue, &h0_view, &h1_view);
        let history = [h0_tex, h1_tex];
        let history_views = [h0_view, h1_view];

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("post-sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("post-bgl"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 3,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        // Theme bg in linear space, padded to vec4. Written once at startup
        // — the theme is fixed for the lifetime of the process.
        let uniforms = device.create_buffer_init(&BufferInitDescriptor {
            label: Some("post-uniforms"),
            contents: bytemuck::cast_slice(&[theme_bg_linear_vec4(default_bg())]),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });
        let bind_groups = [
            make_bind_group(
                device,
                &bind_group_layout,
                &view,
                &sampler,
                &uniforms,
                &history_views[0],
            ),
            make_bind_group(
                device,
                &bind_group_layout,
                &view,
                &sampler,
                &uniforms,
                &history_views[1],
            ),
        ];

        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("post-shader"),
            source: ShaderSource::Wgsl(wgsl.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("post-pl-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("post-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                // Two color targets: surface and the new history slot.
                // Both share `format` so the same fragment value can be
                // emitted at @location(0) and @location(1).
                targets: &[
                    Some(ColorTargetState {
                        format,
                        blend: Some(BlendState::REPLACE),
                        write_mask: ColorWrites::ALL,
                    }),
                    Some(ColorTargetState {
                        format,
                        blend: Some(BlendState::REPLACE),
                        write_mask: ColorWrites::ALL,
                    }),
                ],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            texture,
            view,
            history,
            history_views,
            history_idx: 0,
            sampler,
            uniforms,
            bind_group_layout,
            bind_groups,
            pipeline,
            width,
            height,
            format,
        }
    }

    pub fn resize(&mut self, device: &Device, queue: &Queue, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        let (texture, view) = create_offscreen(device, self.format, width, height, "post-offscreen");
        self.texture = texture;
        self.view = view;

        let h0 = create_offscreen(device, self.format, width, height, "post-history-0");
        let h1 = create_offscreen(device, self.format, width, height, "post-history-1");
        clear_history(device, queue, &h0.1, &h1.1);
        self.history = [h0.0, h1.0];
        self.history_views = [h0.1, h1.1];
        // Reset the ping-pong cursor — the old history is gone.
        self.history_idx = 0;

        self.bind_groups = [
            make_bind_group(
                device,
                &self.bind_group_layout,
                &self.view,
                &self.sampler,
                &self.uniforms,
                &self.history_views[0],
            ),
            make_bind_group(
                device,
                &self.bind_group_layout,
                &self.view,
                &self.sampler,
                &self.uniforms,
                &self.history_views[1],
            ),
        ];
        self.width = width;
        self.height = height;
    }

    pub fn offscreen_view(&self) -> &TextureView {
        &self.view
    }

    /// Run the post-pass: sample the offscreen texture and write to
    /// `surface_view` through the CRT shader, while also stamping the
    /// result into the next history slot for the following frame.
    pub fn apply(&mut self, encoder: &mut CommandEncoder, surface_view: &TextureView) {
        let prev = self.history_idx;
        let curr = 1 - prev;
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("post-pass"),
                color_attachments: &[
                    Some(RenderPassColorAttachment {
                        view: surface_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Clear(wgpu::Color::BLACK),
                            store: StoreOp::Store,
                        },
                    }),
                    Some(RenderPassColorAttachment {
                        view: &self.history_views[curr],
                        depth_slice: None,
                        resolve_target: None,
                        ops: Operations {
                            // The shader overwrites every pixel, so any
                            // load is fine; clear is the cheapest hint.
                            load: LoadOp::Clear(wgpu::Color::BLACK),
                            store: StoreOp::Store,
                        },
                    }),
                ],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_groups[prev], &[]);
            // Three-vertex full-screen triangle; the vertex shader hard-codes positions.
            pass.draw(0..3, 0..1);
        }
        self.history_idx = curr;
    }
}

fn create_offscreen(
    device: &Device,
    format: TextureFormat,
    width: u32,
    height: u32,
    label: &str,
) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&TextureViewDescriptor::default());
    (texture, view)
}

/// Zero out both history textures with a no-op render pass. wgpu does
/// not guarantee initial texture contents, so without this the first
/// frame would sample garbage and bleed it into the phosphor trail.
fn clear_history(device: &Device, queue: &Queue, view0: &TextureView, view1: &TextureView) {
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("post-history-clear"),
    });
    for v in [view0, view1] {
        encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("post-history-clear-pass"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: v,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(wgpu::Color::BLACK),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
            multiview_mask: None,
        });
    }
    queue.submit(Some(encoder.finish()));
}

fn make_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    view: &TextureView,
    sampler: &Sampler,
    uniforms: &Buffer,
    history_view: &TextureView,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("post-bg"),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: BindingResource::TextureView(view),
            },
            BindGroupEntry {
                binding: 1,
                resource: BindingResource::Sampler(sampler),
            },
            BindGroupEntry {
                binding: 2,
                resource: uniforms.as_entire_binding(),
            },
            BindGroupEntry {
                binding: 3,
                resource: BindingResource::TextureView(history_view),
            },
        ],
    })
}

/// sRGB → linear conversion matching the cell-pass clear color, packed
/// into a `vec4` for the uniform buffer (alpha unused, set to 1).
fn theme_bg_linear_vec4(bg: Rgb) -> [f32; 4] {
    [
        srgb_to_linear(bg.0) as f32,
        srgb_to_linear(bg.1) as f32,
        srgb_to_linear(bg.2) as f32,
        1.0,
    ]
}

fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}
