use std::sync::Arc;

use anyhow::Result;
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::{
    CommandEncoderDescriptor, CompositeAlphaMode, CurrentSurfaceTexture, DeviceDescriptor,
    Instance, InstanceDescriptor, LoadOp, MultisampleState, Operations, PowerPreference,
    PresentMode, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions, StoreOp,
    SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};
use winit::window::Window;

use crate::quad::{QuadPipeline, Rect};
use crate::term::{Term, DEFAULT_BG};

pub struct Renderer {
    pub window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: SurfaceConfiguration,
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    buffer: Buffer,
    quads: QuadPipeline,
    scale: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub cell_width: f32,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone())?;

        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;

        let (device, queue) =
            pollster::block_on(adapter.request_device(&DeviceDescriptor::default()))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);
        let quads = QuadPipeline::new(&device, format);

        let scale = window.scale_factor() as f32;
        let font_size_pt: f32 = 14.0;
        let font_size: f32 = (font_size_pt * scale).round();
        let line_height: f32 = (font_size * 1.3_f32).round();
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        buffer.set_size(
            &mut font_system,
            Some(size.width as f32),
            Some(size.height as f32),
        );
        let cell_width = measure_cell_width(&mut font_system, font_size, line_height);
        eprintln!(
            "[evelyn] surface={}x{} scale={} font={}px cell={}x{}",
            config.width, config.height, scale, font_size, cell_width, line_height
        );
        eprintln!(
            "[evelyn] fonts loaded: {}",
            font_system.db().faces().count()
        );

        Ok(Self {
            window,
            device,
            queue,
            surface,
            config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            buffer,
            quads,
            scale,
            font_size,
            line_height,
            cell_width,
        })
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < 1e-3 {
            return;
        }
        self.scale = scale;
        let font_size_pt = 14.0_f32;
        self.font_size = (font_size_pt * scale).round();
        self.line_height = (self.font_size * 1.3_f32).round();
        self.buffer
            .set_metrics(&mut self.font_system, Metrics::new(self.font_size, self.line_height));
        self.cell_width = measure_cell_width(&mut self.font_system, self.font_size, self.line_height);
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.config.width = w.max(1);
        self.config.height = h.max(1);
        self.surface.configure(&self.device, &self.config);
        self.buffer.set_size(
            &mut self.font_system,
            Some(self.config.width as f32),
            Some(self.config.height as f32),
        );
    }

    pub fn grid_size(&self) -> (u16, u16) {
        let cols = ((self.config.width as f32 / self.cell_width).floor() as u16).max(1);
        let rows = ((self.config.height as f32 / self.line_height).floor() as u16).max(1);
        (cols, rows)
    }

    pub fn render(&mut self, term: &Term) -> Result<()> {
        let mut text = String::with_capacity(term.cells.len() + term.rows as usize);
        for y in 0..term.rows {
            for x in 0..term.cols {
                let i = (y as usize) * (term.cols as usize) + (x as usize);
                let ch = term.cells[i].ch;
                text.push(if ch == '\0' { ' ' } else { ch });
            }
            if y + 1 < term.rows {
                text.push('\n');
            }
        }

        let attrs = Attrs::new().family(Family::Monospace);
        self.buffer
            .set_text(&mut self.font_system, &text, &attrs, Shaping::Advanced, None);
        self.buffer
            .shape_until_scroll(&mut self.font_system, false);

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            [TextArea {
                buffer: &self.buffer,
                left: 0.0,
                top: 0.0,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: self.config.width as i32,
                    bottom: self.config.height as i32,
                },
                default_color: Color::rgb(0xd0, 0xd0, 0xd0),
                custom_glyphs: &[],
            }],
            &mut self.swash_cache,
        )?;

        let frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(f) | CurrentSurfaceTexture::Suboptimal(f) => f,
            CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            CurrentSurfaceTexture::Timeout => return Ok(()),
            CurrentSurfaceTexture::Occluded => return Ok(()),
            CurrentSurfaceTexture::Validation => {
                anyhow::bail!("surface validation error");
            }
        };
        let view = frame
            .texture
            .create_view(&TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("evelyn-pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color {
                            r: srgb_to_linear(DEFAULT_BG.0),
                            g: srgb_to_linear(DEFAULT_BG.1),
                            b: srgb_to_linear(DEFAULT_BG.2),
                            a: 1.0,
                        }),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)?;

            // Cursor outline drawn over the text so it stays visible regardless of glyph color.
            let cursor_rects = build_cursor_outline(
                term.cur_x as f32 * self.cell_width,
                term.cur_y as f32 * self.line_height,
                self.cell_width,
                self.line_height,
                (2.0 * self.scale).round().max(1.0),
            );
            self.quads.draw(
                &self.device,
                &self.queue,
                &mut pass,
                self.config.width as f32,
                self.config.height as f32,
                &cursor_rects,
            );
        }
        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        frame.present();
        self.atlas.trim();
        Ok(())
    }
}

fn measure_cell_width(fs: &mut FontSystem, font_size: f32, line_height: f32) -> f32 {
    const PROBE: &str = "MMMMMMMMMM";
    let mut buf = Buffer::new(fs, Metrics::new(font_size, line_height));
    buf.set_size(fs, Some(10_000.0), Some(line_height * 2.0));
    let attrs = Attrs::new().family(Family::Monospace);
    buf.set_text(fs, PROBE, &attrs, Shaping::Advanced, None);
    buf.shape_until_scroll(fs, false);
    let mut max_x: f32 = 0.0;
    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            max_x = max_x.max(glyph.x + glyph.w);
        }
    }
    if max_x > 0.0 {
        max_x / PROBE.len() as f32
    } else {
        font_size * 0.6
    }
}

fn build_cursor_outline(x: f32, y: f32, w: f32, h: f32, t: f32) -> [Rect; 4] {
    const COLOR: [f32; 4] = [0.90, 0.78, 0.20, 1.0]; // amber/yellow
    [
        Rect { x, y, w, h: t, color: COLOR },                       // top
        Rect { x, y: y + h - t, w, h: t, color: COLOR },            // bottom
        Rect { x, y, w: t, h, color: COLOR },                       // left
        Rect { x: x + w - t, y, w: t, h, color: COLOR },            // right
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
