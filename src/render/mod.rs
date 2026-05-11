mod convert;
mod init;
mod overlay;
mod post;
mod quad;
mod text;

use std::sync::Arc;

use anyhow::Result;
use wgpu::{
    CommandEncoderDescriptor, CurrentSurfaceTexture, LoadOp, Operations, RenderPassColorAttachment,
    RenderPassDescriptor, StoreOp, SurfaceConfiguration, TextureViewDescriptor,
};
use winit::window::Window;

use crate::color::default_bg;
use crate::config::{CursorShape, config as live_config, resolve_shader_source};
use crate::term::Term;

use convert::clear_color_for;
use init::{GpuInit, init_gpu, metrics_for};
use post::PostProcessor;
use quad::QuadPipeline;
use text::cosmic::CosmicEngine;
use text::{TextEngine, build_runs, cursor_cell};

pub struct Renderer {
    pub window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: SurfaceConfiguration,
    /// Text shaping + rasterization. Boxed dyn so the cosmic-text impl
    /// can be swapped for an alternative (e.g. CoreText) without
    /// touching the Renderer.
    engine: Box<dyn TextEngine>,
    quads: QuadPipeline,
    post: Option<PostProcessor>,
    scale: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub cell_width: f32,
    /// Window inset around the grid in surface pixels. `live_config().window.padding`
    /// scaled to physical pixels.
    padding: f32,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let GpuInit {
            device,
            queue,
            surface,
            config,
            format,
        } = init_gpu(&window)?;

        let scale = window.scale_factor() as f32;
        let (font_size, line_height) = metrics_for(scale);
        let padding = (live_config().window.padding * scale).round();

        let engine = CosmicEngine::new(&device, &queue, format, font_size, line_height);
        let cell_width = engine.cell_width();
        let face_count = engine.font_count();

        let quads = QuadPipeline::new(&device, format);
        let post = resolve_shader_source().map(|src| {
            PostProcessor::new(&device, &queue, format, config.width, config.height, &src)
        });

        eprintln!(
            "[evelyn] surface={}x{} scale={} font={}px cell={}x{}",
            config.width, config.height, scale, font_size, cell_width, line_height
        );
        eprintln!("[evelyn] fonts loaded: {face_count}");

        Ok(Self {
            window,
            device,
            queue,
            surface,
            config,
            engine: Box::new(engine),
            quads,
            post,
            scale,
            font_size,
            line_height,
            cell_width,
            padding,
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
        self.padding = (live_config().window.padding * scale).round();
        self.engine.set_metrics(font_size, line_height);
        self.cell_width = self.engine.cell_width();
    }

    /// Re-derive everything that comes from the config: font metrics, cell
    /// width, padding, post-processor. Called after `config::reload`.
    /// Returns `true` if the cell grid changed, so the caller can re-sync
    /// the term + PTY size and request a redraw.
    pub fn reload_from_config(&mut self) -> bool {
        let old_cell_w = self.cell_width;
        let old_line_h = self.line_height;
        let old_padding = self.padding;

        let (font_size, line_height) = metrics_for(self.scale);
        self.font_size = font_size;
        self.line_height = line_height;
        self.padding = (live_config().window.padding * self.scale).round();
        self.engine.set_metrics(font_size, line_height);
        self.cell_width = self.engine.cell_width();

        // Rebuild the post-pass from scratch — config may have toggled the
        // master switch, swapped the effect, or pointed at a fresh on-disk
        // shader file we want to re-read.
        self.post = resolve_shader_source().map(|src| {
            PostProcessor::new(
                &self.device,
                &self.queue,
                self.config.format,
                self.config.width,
                self.config.height,
                &src,
            )
        });
        self.engine.trim();

        (self.cell_width - old_cell_w).abs() > f32::EPSILON
            || (self.line_height - old_line_h).abs() > f32::EPSILON
            || (self.padding - old_padding).abs() > f32::EPSILON
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.config.width = w.max(1);
        self.config.height = h.max(1);
        self.surface.configure(&self.device, &self.config);
        if let Some(post) = self.post.as_mut() {
            post.resize(
                &self.device,
                &self.queue,
                self.config.width,
                self.config.height,
            );
        }
    }

    pub fn grid_size(&self) -> (u16, u16) {
        let usable_w = (self.config.width as f32 - 2.0 * self.padding).max(0.0);
        let usable_h = (self.config.height as f32 - 2.0 * self.padding).max(0.0);
        let cols = ((usable_w / self.cell_width).floor() as u16).max(1);
        let rows = ((usable_h / self.line_height).floor() as u16).max(1);
        (cols, rows)
    }

    /// Convert a window-relative physical pixel position to a 1-indexed
    /// (col, row) cell coord, matching xterm's mouse-report convention.
    /// Clamped to the visible grid; positions in the padding bezel snap
    /// to the nearest edge cell so wheel events at the window margin
    /// still report a sensible position to the foreground app.
    pub fn pixel_to_cell(&self, x: f64, y: f64) -> (u16, u16) {
        let (cols, rows) = self.grid_size();
        let local_x = (x as f32 - self.padding).max(0.0);
        let local_y = (y as f32 - self.padding).max(0.0);
        let cx = ((local_x / self.cell_width).floor() as i32).clamp(0, cols as i32 - 1) as u16;
        let cy = ((local_y / self.line_height).floor() as i32).clamp(0, rows as i32 - 1) as u16;
        (cx + 1, cy + 1)
    }

    pub fn render(
        &mut self,
        term: &Term,
        preedit: &str,
        preedit_cursor: usize,
        blink_on: bool,
        focused: bool,
    ) -> Result<()> {
        // Hide the cursor while the user is browsing scrollback — the
        // (cur_x, cur_y) position refers to the live screen and would
        // paint at the wrong row inside the historical view. Also gate
        // on the blink phase so a configured blink can hide it.
        let show_cursor =
            preedit.is_empty() && term.cursor_visible && term.view_offset == 0 && blink_on;
        let cursor_wide = if show_cursor {
            cursor_cell(term).map(|(_, w)| w).unwrap_or(false)
        } else {
            false
        };
        let cursor_shape = live_config().cursor.shape;

        // Preedit shaping comes first so we know how many cells it spans;
        // those cells are then masked out of the grid runs and SGR bg quads
        // below. Without the mask, an app that draws its own cursor by
        // writing an inverted cell (Claude Code, helix in some modes) leaves
        // that cell visible behind the preedit — preedit glyphs have gaps
        // and don't fully cover the cell underneath.
        let has_preedit = !preedit.is_empty();
        let preedit_metrics = if has_preedit {
            self.engine.shape_preedit(preedit, preedit_cursor)
        } else {
            text::PreeditMetrics::default()
        };
        let preedit_mask = if has_preedit {
            let span = ((preedit_metrics.width / self.cell_width).ceil() as u16).max(1);
            let end = term.cur_x.saturating_add(span).min(term.cols);
            Some((term.cur_y, term.cur_x, end))
        } else {
            None
        };

        // Build grid runs and shape each into a dedicated row buffer. This
        // keeps every run pinned to its grid column so font fallback widths
        // can't drift the layout. Only the *focused* block shape inverts
        // the cell character; the unfocused outline sits around the glyph
        // so the regular foreground stays correct, same as bar/underline.
        let cursor_pos = if show_cursor && focused && cursor_shape == CursorShape::Block {
            Some((term.cur_x, term.cur_y))
        } else {
            None
        };
        let runs = build_runs(term, cursor_pos, preedit_mask);
        self.engine.shape_runs(&runs, cursor_pos);

        let pre_left = term.cur_x as f32 * self.cell_width + self.padding;
        let pre_top = term.cur_y as f32 * self.line_height + self.padding;

        let preedit_origin = if has_preedit {
            Some((pre_left, pre_top))
        } else {
            None
        };
        self.engine.prepare(
            &self.device,
            &self.queue,
            (self.config.width, self.config.height),
            self.cell_width,
            self.line_height,
            self.padding,
            preedit_origin,
        )?;

        let Some(frame) = self.acquire_frame() else {
            return Ok(());
        };
        let surface_view = frame.texture.create_view(&TextureViewDescriptor::default());
        let overlay = self.overlay_quads(
            term,
            has_preedit,
            preedit_metrics.width,
            preedit_metrics.caret_x,
            show_cursor,
            cursor_wide,
            cursor_shape,
            focused,
            pre_left,
            pre_top,
        );

        // Either render the cell grid directly to the surface, or to an
        // offscreen texture that the post-pass will sample.
        let cell_target: &wgpu::TextureView = match self.post.as_ref() {
            Some(p) => p.offscreen_view(),
            None => &surface_view,
        };

        // Use the theme background for both the surface clear and the
        // build_bg_quads skip key. Stable across app changes (helix /
        // zellij / shell don't shift the padding color), at the cost of
        // a tiny color step at the cell-grid edge for apps that paint a
        // non-default bg. The corner vignette mostly hides it.
        let screen_bg = default_bg();
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("evelyn-cells"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: cell_target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(clear_color_for(screen_bg)),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            // Per-cell SGR backgrounds first, then the selection tint on
            // top of those, then the cursor block, then text. The cursor
            // cell's char run uses cursor_text as its foreground so it
            // appears inverted against the cursor block. Selection sits
            // between bg and cursor so an active drag still shows the
            // cursor block, and is alpha-blended so colored cell bgs
            // remain readable underneath.
            let mut quads = self.build_bg_quads(term, screen_bg, preedit_mask);
            quads.extend_from_slice(&self.build_selection_quads(term));
            quads.extend_from_slice(&overlay);
            self.quads.draw(
                &self.device,
                &self.queue,
                &mut pass,
                self.config.width as f32,
                self.config.height as f32,
                &quads,
            );
            self.engine.render(&mut pass)?;
        }

        if let Some(post) = self.post.as_mut() {
            post.apply(&mut encoder, &surface_view);
        }

        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        frame.present();
        self.engine.trim();
        Ok(())
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
