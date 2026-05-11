mod convert;
mod init;
mod overlay;
mod post;
mod quad;
mod text;

use std::sync::Arc;

use anyhow::Result;
use glyphon::{
    Buffer, FontSystem, Metrics, Resolution, SwashCache, TextArea, TextAtlas, TextBounds,
    TextRenderer, Viewport,
};
use wgpu::{
    CommandEncoderDescriptor, CurrentSurfaceTexture, LoadOp, Operations, RenderPassColorAttachment,
    RenderPassDescriptor, StoreOp, SurfaceConfiguration, TextureViewDescriptor,
};
use winit::window::Window;

use crate::color::{cursor_color, cursor_text_color, default_bg, default_fg};
use crate::config::{CursorShape, config as live_config, resolve_shader_source};
use crate::term::Term;

use convert::{clear_color_for, rgb_to_color};
use init::{GpuInit, TextInit, init_gpu, init_text_stack, make_buffer, measure_cell_width, metrics_for};
use post::PostProcessor;
use quad::QuadPipeline;
use text::{build_runs, cursor_cell, font_attrs};

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
    /// One Buffer per text run. Each run is a maximal stretch of cells that
    /// can be shaped together (uniform attrs, no wide char in the middle) and
    /// is positioned at an exact grid column. Pool grows as needed.
    row_buffers: Vec<Buffer>,
    preedit_buffer: Buffer,
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
        let size = window.inner_size();
        let GpuInit {
            device,
            queue,
            surface,
            config,
            format,
        } = init_gpu(&window)?;
        let TextInit {
            mut font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
        } = init_text_stack(&device, &queue, format);
        let quads = QuadPipeline::new(&device, format);
        let post = resolve_shader_source().map(|src| {
            PostProcessor::new(&device, &queue, format, config.width, config.height, &src)
        });

        let scale = window.scale_factor() as f32;
        let (font_size, line_height) = metrics_for(scale);
        let padding = (live_config().window.padding * scale).round();

        let row_buffers: Vec<Buffer> = Vec::new();
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
            row_buffers,
            preedit_buffer,
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
        let m = Metrics::new(font_size, line_height);
        for buf in &mut self.row_buffers {
            buf.set_metrics(&mut self.font_system, m);
        }
        self.preedit_buffer.set_metrics(&mut self.font_system, m);
        self.cell_width = measure_cell_width(&mut self.font_system, font_size, line_height);
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
        let m = Metrics::new(font_size, line_height);
        for buf in &mut self.row_buffers {
            buf.set_metrics(&mut self.font_system, m);
        }
        self.preedit_buffer.set_metrics(&mut self.font_system, m);
        self.cell_width = measure_cell_width(&mut self.font_system, font_size, line_height);

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
        // The old atlas is keyed by the previous font/size — drop cached
        // glyph rasters so freshly-shaped runs don't sample stale entries.
        self.atlas.trim();

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
        let base = font_attrs();
        let has_preedit = !preedit.is_empty();
        let (preedit_w, preedit_caret_x) = if has_preedit {
            self.shape_preedit(preedit, preedit_cursor, base.clone())
        } else {
            (0.0, 0.0)
        };
        let preedit_mask = if has_preedit {
            let span = ((preedit_w / self.cell_width).ceil() as u16).max(1);
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
        let cursor_override = if show_cursor && focused && cursor_shape == CursorShape::Block {
            Some((
                term.cur_x,
                term.cur_y,
                base.clone().color(rgb_to_color(cursor_text_color())),
            ))
        } else {
            None
        };
        let runs = build_runs(term, base.clone(), cursor_override, preedit_mask);
        self.shape_runs(&runs);

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        let pre_left = term.cur_x as f32 * self.cell_width + self.padding;
        let pre_top = term.cur_y as f32 * self.line_height + self.padding;

        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        // Row TextAreas: one per run, positioned at exact grid column.
        let cell_width = self.cell_width;
        let line_height = self.line_height;
        let padding = self.padding;
        let mut areas: Vec<TextArea> = runs
            .iter()
            .enumerate()
            .map(|(i, run)| TextArea {
                buffer: &self.row_buffers[i],
                left: run.col as f32 * cell_width + padding,
                top: run.row as f32 * line_height + padding,
                scale: 1.0,
                bounds,
                default_color: rgb_to_color(default_fg()),
                custom_glyphs: &[],
            })
            .collect();
        if has_preedit {
            areas.push(TextArea {
                buffer: &self.preedit_buffer,
                left: pre_left,
                top: pre_top,
                scale: 1.0,
                bounds,
                default_color: rgb_to_color(cursor_color()),
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

        let Some(frame) = self.acquire_frame() else {
            return Ok(());
        };
        let surface_view = frame.texture.create_view(&TextureViewDescriptor::default());
        let overlay = self.overlay_quads(
            term,
            has_preedit,
            preedit_w,
            preedit_caret_x,
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
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)?;
        }

        if let Some(post) = self.post.as_mut() {
            post.apply(&mut encoder, &surface_view);
        }

        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        frame.present();
        self.atlas.trim();
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
