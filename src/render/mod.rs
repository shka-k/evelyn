mod init;
mod quad;

use std::sync::Arc;

use anyhow::Result;
use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::{
    CommandEncoderDescriptor, CurrentSurfaceTexture, LoadOp, Operations,
    RenderPassColorAttachment, RenderPassDescriptor, StoreOp, SurfaceConfiguration,
    TextureViewDescriptor,
};
use winit::window::Window;

use crate::color::{Rgb, DEFAULT_BG, DEFAULT_FG};
use crate::config::CONFIG;
use crate::term::Term;

use init::{init_gpu, init_text_stack, make_buffer, measure_cell_width, metrics_for, GpuInit, TextInit};
use quad::{QuadPipeline, Rect};

const CURSOR_COLOR_RGBA: [f32; 4] = [0.90, 0.78, 0.20, 1.0];
const CURSOR_STROKE_PT: f32 = 2.0;
const PREEDIT_COLOR: Rgb = Rgb(0xff, 0xe0, 0x70);
const PREEDIT_UNDERLINE_RGBA: [f32; 4] = [1.0, 0.88, 0.44, 1.0];

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
    preedit_buffer: Buffer,
    quads: QuadPipeline,
    scale: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub cell_width: f32,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let GpuInit { device, queue, surface, config, format } = init_gpu(&window)?;
        let TextInit {
            mut font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
        } = init_text_stack(&device, &queue, format);
        let quads = QuadPipeline::new(&device, format);

        let scale = window.scale_factor() as f32;
        let (font_size, line_height) = metrics_for(scale);

        let mut buffer = make_buffer(&mut font_system, font_size, line_height);
        buffer.set_size(
            &mut font_system,
            Some(size.width as f32),
            Some(size.height as f32),
        );
        let mut preedit_buffer = make_buffer(&mut font_system, font_size, line_height);
        preedit_buffer.set_size(
            &mut font_system,
            Some(size.width as f32),
            Some(line_height * 2.0),
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
            preedit_buffer,
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
        let (font_size, line_height) = metrics_for(scale);
        self.font_size = font_size;
        self.line_height = line_height;
        let m = Metrics::new(font_size, line_height);
        self.buffer.set_metrics(&mut self.font_system, m);
        self.preedit_buffer.set_metrics(&mut self.font_system, m);
        self.cell_width = measure_cell_width(&mut self.font_system, font_size, line_height);
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

    pub fn render(&mut self, term: &Term, preedit: &str) -> Result<()> {
        let (has_preedit, preedit_w) = self.shape(term, preedit);

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        let pre_left = term.cur_x as f32 * self.cell_width;
        let pre_top = term.cur_y as f32 * self.line_height;

        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let mut areas: Vec<TextArea> = Vec::with_capacity(2);
        areas.push(TextArea {
            buffer: &self.buffer,
            left: 0.0,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: rgb_to_color(DEFAULT_FG),
            custom_glyphs: &[],
        });
        if has_preedit {
            areas.push(TextArea {
                buffer: &self.preedit_buffer,
                left: pre_left,
                top: pre_top,
                scale: 1.0,
                bounds,
                default_color: rgb_to_color(PREEDIT_COLOR),
                custom_glyphs: &[],
            });
        }

        self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        )?;

        let Some(frame) = self.acquire_frame() else { return Ok(()); };
        let view = frame
            .texture
            .create_view(&TextureViewDescriptor::default());
        let overlay = self.overlay_quads(term, has_preedit, preedit_w, pre_left, pre_top);

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
                        load: LoadOp::Clear(clear_color_for(DEFAULT_BG)),
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
            self.quads.draw(
                &self.device,
                &self.queue,
                &mut pass,
                self.config.width as f32,
                self.config.height as f32,
                &overlay,
            );
        }
        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        frame.present();
        self.atlas.trim();
        Ok(())
    }

    /// Push grid + preedit text into the buffers and shape them. Returns
    /// `(has_preedit, preedit_pixel_width)`.
    fn shape(&mut self, term: &Term, preedit: &str) -> (bool, f32) {
        let attrs = font_attrs();
        let text = build_grid_text(term);
        self.buffer
            .set_text(&mut self.font_system, &text, &attrs, Shaping::Advanced, None);
        self.buffer
            .shape_until_scroll(&mut self.font_system, false);

        let has_preedit = !preedit.is_empty();
        let mut preedit_w: f32 = 0.0;
        if has_preedit {
            self.preedit_buffer.set_text(
                &mut self.font_system,
                preedit,
                &attrs,
                Shaping::Advanced,
                None,
            );
            self.preedit_buffer
                .shape_until_scroll(&mut self.font_system, false);
            for run in self.preedit_buffer.layout_runs() {
                for g in run.glyphs.iter() {
                    preedit_w = preedit_w.max(g.x + g.w);
                }
            }
        }
        (has_preedit, preedit_w)
    }

    /// Cursor outline, or — when IME is composing — the preedit underline.
    fn overlay_quads(
        &self,
        term: &Term,
        has_preedit: bool,
        preedit_w: f32,
        pre_left: f32,
        pre_top: f32,
    ) -> Vec<Rect> {
        let stroke = (CURSOR_STROKE_PT * self.scale).round().max(1.0);
        if has_preedit {
            let w = preedit_w.max(self.cell_width);
            vec![Rect {
                x: pre_left,
                y: pre_top + self.line_height - stroke,
                w,
                h: stroke,
                color: PREEDIT_UNDERLINE_RGBA,
            }]
        } else {
            build_cursor_outline(
                term.cur_x as f32 * self.cell_width,
                term.cur_y as f32 * self.line_height,
                self.cell_width,
                self.line_height,
                stroke,
            )
            .to_vec()
        }
    }

    /// Acquire the next surface texture, recovering transient surface losses.
    /// Returns `None` to skip this frame.
    fn acquire_frame(&self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(f) | CurrentSurfaceTexture::Suboptimal(f) => Some(f),
            CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                None
            }
            CurrentSurfaceTexture::Timeout
            | CurrentSurfaceTexture::Occluded
            | CurrentSurfaceTexture::Validation => None,
        }
    }
}

fn build_grid_text(term: &Term) -> String {
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
    text
}

fn build_cursor_outline(x: f32, y: f32, w: f32, h: f32, t: f32) -> [Rect; 4] {
    let c = CURSOR_COLOR_RGBA;
    [
        Rect { x, y, w, h: t, color: c },
        Rect { x, y: y + h - t, w, h: t, color: c },
        Rect { x, y, w: t, h, color: c },
        Rect { x: x + w - t, y, w: t, h, color: c },
    ]
}

fn clear_color_for(bg: Rgb) -> wgpu::Color {
    wgpu::Color {
        r: srgb_to_linear(bg.0),
        g: srgb_to_linear(bg.1),
        b: srgb_to_linear(bg.2),
        a: 1.0,
    }
}

fn rgb_to_color(c: Rgb) -> Color {
    Color::rgb(c.0, c.1, c.2)
}

fn font_attrs() -> Attrs<'static> {
    match CONFIG.font.family.as_deref() {
        Some(name) => Attrs::new().family(Family::Name(name)),
        None => Attrs::new().family(Family::Monospace),
    }
}

fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}
