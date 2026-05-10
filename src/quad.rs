use bytemuck::{Pod, Zeroable};
use wgpu::{
    BlendState, BufferAddress, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
    Device, FragmentState, MultisampleState, PipelineLayoutDescriptor, PrimitiveState, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource,
    TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

const SHADER: &str = r#"
struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn vs_main(in: VsIn) -> VsOut {
    var o: VsOut;
    o.clip = vec4<f32>(in.pos, 0.0, 1.0);
    o.color = in.color;
    return o;
}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Vert {
    pos: [f32; 2],
    color: [f32; 4],
}

unsafe impl Pod for Vert {}
unsafe impl Zeroable for Vert {}

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
}

pub struct QuadPipeline {
    pipeline: RenderPipeline,
    vbuf: wgpu::Buffer,
    capacity: u64,
}

impl QuadPipeline {
    pub fn new(device: &Device, format: TextureFormat) -> Self {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("quad-shader"),
            source: ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("quad-pl-layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("quad-pipeline"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vert>() as BufferAddress,
                    step_mode: VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                }],
            },
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let capacity: u64 = 256;
        let vbuf = device.create_buffer(&BufferDescriptor {
            label: Some("quad-vbuf"),
            size: capacity * std::mem::size_of::<Vert>() as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            vbuf,
            capacity,
        }
    }

    pub fn draw(
        &mut self,
        device: &Device,
        queue: &Queue,
        pass: &mut RenderPass<'_>,
        viewport_w: f32,
        viewport_h: f32,
        quads: &[Rect],
    ) {
        if quads.is_empty() {
            return;
        }
        let mut verts: Vec<Vert> = Vec::with_capacity(quads.len() * 6);
        for q in quads {
            let x0 = q.x / viewport_w * 2.0 - 1.0;
            let x1 = (q.x + q.w) / viewport_w * 2.0 - 1.0;
            let y0 = 1.0 - q.y / viewport_h * 2.0;
            let y1 = 1.0 - (q.y + q.h) / viewport_h * 2.0;
            let c = q.color;
            verts.push(Vert { pos: [x0, y0], color: c });
            verts.push(Vert { pos: [x1, y0], color: c });
            verts.push(Vert { pos: [x0, y1], color: c });
            verts.push(Vert { pos: [x1, y0], color: c });
            verts.push(Vert { pos: [x1, y1], color: c });
            verts.push(Vert { pos: [x0, y1], color: c });
        }

        let needed = verts.len() as u64;
        if needed > self.capacity {
            self.capacity = needed.next_power_of_two();
            self.vbuf = device.create_buffer(&BufferDescriptor {
                label: Some("quad-vbuf"),
                size: self.capacity * std::mem::size_of::<Vert>() as u64,
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        queue.write_buffer(&self.vbuf, 0, bytemuck::cast_slice(&verts));
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.vbuf.slice(..));
        pass.draw(0..verts.len() as u32, 0..1);
    }
}
