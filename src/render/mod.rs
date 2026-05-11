mod init;
mod post;
mod quad;

use std::sync::Arc;

use anyhow::Result;
use glyphon::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
    cosmic_text::{FeatureTag, FontFeatures},
};
use wgpu::{
    CommandEncoderDescriptor, CurrentSurfaceTexture, LoadOp, Operations, RenderPassColorAttachment,
    RenderPassDescriptor, StoreOp, SurfaceConfiguration, TextureViewDescriptor,
};
use winit::window::Window;

use crate::color::{Rgb, cursor_color, cursor_text_color, default_bg, default_fg};
use crate::config::{CursorShape, config as live_config, resolve_shader_source};
use crate::term::Term;

use init::{
    BUNDLED_FONT_NAME, GpuInit, TextInit, init_gpu, init_text_stack, make_buffer,
    measure_cell_width, metrics_for,
};
use post::PostProcessor;
use quad::{QuadPipeline, Rect};

const PREEDIT_UNDERLINE_PT: f32 = 2.0;

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

#[derive(Debug)]
struct Run {
    col: u16,
    row: u16,
    text: String,
    attrs: Attrs<'static>,
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

    /// Grow the row buffer pool to fit `count` runs and shape each one. The
    /// pool is reused across frames; we re-set_text on every render so old
    /// content beyond `runs.len()` is harmless (just not referenced).
    fn shape_runs(&mut self, runs: &[Run]) {
        let needed = runs.len();
        while self.row_buffers.len() < needed {
            let buf = make_buffer(&mut self.font_system, self.font_size, self.line_height);
            self.row_buffers.push(buf);
        }
        for (i, run) in runs.iter().enumerate() {
            let buf = &mut self.row_buffers[i];
            // Unbounded width so a row run never wraps internally; a stale
            // buffer width after a window resize would otherwise cause the
            // overflow to render as a second line in the cell below — which
            // looks exactly like "the frame got newlined."
            buf.set_size(&mut self.font_system, None, Some(self.line_height));
            buf.set_text(
                &mut self.font_system,
                &run.text,
                &run.attrs,
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
        }
    }

    /// Returns `(total_width, caret_x)` where `caret_x` is the pixel offset
    /// of the IME caret within the preedit. The caret sits at the leading
    /// edge of the first glyph whose byte index is `>= preedit_cursor`, or
    /// at the trailing edge of the last glyph when the caret is past the
    /// end (e.g. macOS reports caret == text.len() during composition).
    fn shape_preedit(
        &mut self,
        preedit: &str,
        preedit_cursor: usize,
        base: Attrs<'_>,
    ) -> (f32, f32) {
        self.preedit_buffer
            .set_monospace_width(&mut self.font_system, Some(self.cell_width));
        self.preedit_buffer.set_text(
            &mut self.font_system,
            preedit,
            &base,
            Shaping::Advanced,
            None,
        );
        self.preedit_buffer
            .shape_until_scroll(&mut self.font_system, false);
        let mut max_x: f32 = 0.0;
        let mut caret_x: f32 = 0.0;
        let mut caret_set = false;
        for run in self.preedit_buffer.layout_runs() {
            for g in run.glyphs.iter() {
                if !caret_set && g.start >= preedit_cursor {
                    caret_x = g.x;
                    caret_set = true;
                }
                max_x = max_x.max(g.x + g.w);
            }
        }
        if !caret_set {
            caret_x = max_x;
        }
        (max_x, caret_x)
    }

    /// Cursor in the configured shape, or — when IME is composing — the
    /// preedit underline plus a Bar caret that tracks the IME's reported
    /// caret position inside the preedit.
    #[allow(clippy::too_many_arguments)]
    fn overlay_quads(
        &self,
        term: &Term,
        has_preedit: bool,
        preedit_w: f32,
        preedit_caret_x: f32,
        show_cursor: bool,
        cursor_wide: bool,
        cursor_shape: CursorShape,
        focused: bool,
        pre_left: f32,
        pre_top: f32,
    ) -> Vec<Rect> {
        if has_preedit {
            let underline_h = (PREEDIT_UNDERLINE_PT * self.scale).round().max(1.0);
            let w = preedit_w.max(self.cell_width);
            let stripe = (2.0 * self.scale).round().max(1.0);
            let color = rgb_to_rgba(cursor_color(), 1.0);
            return vec![
                Rect {
                    x: pre_left,
                    y: pre_top + self.line_height - underline_h,
                    w,
                    h: underline_h,
                    color,
                },
                // Bar caret inside the preedit — clamped to the trailing
                // edge so it stays visible when the IME parks the caret
                // past the last glyph (typical macOS behavior).
                Rect {
                    x: pre_left + preedit_caret_x.min(w - stripe).max(0.0),
                    y: pre_top,
                    w: stripe,
                    h: self.line_height,
                    color,
                },
            ];
        }
        if !show_cursor {
            return Vec::new();
        }
        let cell_w = if cursor_wide {
            self.cell_width * 2.0
        } else {
            self.cell_width
        };
        let x = term.cur_x as f32 * self.cell_width + self.padding;
        let y = term.cur_y as f32 * self.line_height + self.padding;
        // Stripe thickness for bar/underline + the unfocused-block outline
        // — 2pt scaled to physical pixels with a 1px floor so it never
        // disappears at low DPI.
        let stripe = (2.0 * self.scale).round().max(1.0);
        let color = rgb_to_rgba(cursor_color(), 1.0);
        // Unfocused Block downgrades to Hollow so the user can still
        // see *where* the cursor is without it competing with the
        // foreground app's own focus indicators. Bar/Underline are
        // already thin lines — they read fine without a filled glyph
        // behind them, so they stay as-is.
        let effective_shape = if !focused && cursor_shape == CursorShape::Block {
            CursorShape::Hollow
        } else {
            cursor_shape
        };
        match effective_shape {
            CursorShape::Block => vec![Rect {
                x,
                y,
                w: cell_w,
                h: self.line_height,
                color,
            }],
            CursorShape::Underline => vec![Rect {
                x,
                y: y + self.line_height - stripe,
                w: cell_w,
                h: stripe,
                color,
            }],
            CursorShape::Bar => vec![Rect {
                x,
                y,
                w: stripe,
                h: self.line_height,
                color,
            }],
            CursorShape::Hollow => vec![
                Rect {
                    x,
                    y,
                    w: cell_w,
                    h: stripe,
                    color,
                },
                Rect {
                    x,
                    y: y + self.line_height - stripe,
                    w: cell_w,
                    h: stripe,
                    color,
                },
                Rect {
                    x,
                    y,
                    w: stripe,
                    h: self.line_height,
                    color,
                },
                Rect {
                    x: x + cell_w - stripe,
                    y,
                    w: stripe,
                    h: self.line_height,
                    color,
                },
            ],
        }
    }

    /// One alpha-blended rect per maximal run of selected cells in each
    /// row. Rendered on top of the SGR backgrounds so colored cells still
    /// show their hue under the highlight. Skipped entirely when nothing
    /// is selected so the common path stays free of per-cell checks.
    fn build_selection_quads(&self, term: &Term) -> Vec<Rect> {
        if term.selection.is_none() {
            return Vec::new();
        }
        let color = selection_overlay_color();
        let mut quads = Vec::new();
        for y in 0..term.rows {
            let mut x: u16 = 0;
            while x < term.cols {
                if !term.cell_in_selection(x, y) {
                    x += 1;
                    continue;
                }
                let mut end = x + 1;
                while end < term.cols && term.cell_in_selection(end, y) {
                    end += 1;
                }
                quads.push(Rect {
                    x: x as f32 * self.cell_width + self.padding,
                    y: y as f32 * self.line_height + self.padding,
                    w: (end - x) as f32 * self.cell_width,
                    h: self.line_height,
                    color,
                });
                x = end;
            }
        }
        quads
    }

    /// One opaque rect per maximal stretch of cells that share an SGR
    /// background different from `screen_bg`. Cells whose bg matches
    /// `screen_bg` are skipped because the surface clear already paints
    /// them — and `screen_bg` is whatever we cleared the cell pass with,
    /// not necessarily the theme default.
    fn build_bg_quads(
        &self,
        term: &Term,
        screen_bg: Rgb,
        mask: Option<(u16, u16, u16)>,
    ) -> Vec<Rect> {
        let cols = term.cols as usize;
        let mut quads = Vec::new();
        for y in 0..term.rows {
            let mut x: usize = 0;
            while x < cols {
                if let Some((my, ms, me)) = mask {
                    if y == my && (x as u16) >= ms && (x as u16) < me {
                        x += 1;
                        continue;
                    }
                }
                let bg = term.cell_at(x as u16, y).bg_eff();
                let mut end = x + 1;
                while end < cols && term.cell_at(end as u16, y).bg_eff() == bg {
                    if let Some((my, ms, me)) = mask {
                        if y == my && (end as u16) >= ms && (end as u16) < me {
                            break;
                        }
                    }
                    end += 1;
                }
                if bg != screen_bg {
                    quads.push(Rect {
                        x: x as f32 * self.cell_width + self.padding,
                        y: y as f32 * self.line_height + self.padding,
                        w: (end - x) as f32 * self.cell_width,
                        h: self.line_height,
                        color: rgb_to_rgba(bg, 1.0),
                    });
                }
                x = end;
            }
        }
        quads
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

/// Walk the grid and emit one `Run` per maximal stretch of cells that can be
/// shaped together: same attrs, no wide character interrupting. Each wide
/// character becomes its own single-cell run so the next run can start at an
/// exact grid column without depending on the wide glyph's natural advance.
/// `cursor_override` forces the matching cell into its own run with the given
/// attrs, used for the inverted cursor character.
fn build_runs(
    term: &Term,
    base: Attrs<'static>,
    cursor_override: Option<(u16, u16, Attrs<'static>)>,
    mask: Option<(u16, u16, u16)>,
) -> Vec<Run> {
    let mut runs = Vec::new();

    for y in 0..term.rows {
        let mut run_col: u16 = 0;
        let mut run_attrs = attrs_for_cell(base.clone(), default_fg(), false);
        let mut run_text = String::new();

        for x in 0..term.cols {
            let cell = term.cell_at(x, y);
            if cell.ch == '\0' {
                continue; // wide-char continuation
            }
            // Cells under the IME preedit are dropped so any app-drawn
            // cursor (inverted cell from helix, Claude Code, etc.) sitting
            // on the cursor row doesn't bleed through the preedit glyphs.
            let masked = mask
                .map(|(my, ms, me)| y == my && x >= ms && x < me)
                .unwrap_or(false);
            if masked {
                if !run_text.is_empty() {
                    runs.push(Run {
                        col: run_col,
                        row: y,
                        text: std::mem::take(&mut run_text),
                        attrs: run_attrs.clone(),
                    });
                }
                continue;
            }
            let is_cursor = cursor_override
                .as_ref()
                .map(|(cx, cy, _)| *cx == x && *cy == y)
                .unwrap_or(false);
            let cell_attrs = if is_cursor {
                cursor_override.as_ref().unwrap().2.clone()
            } else {
                attrs_for_cell(base.clone(), cell.fg_eff(), cell.bold)
            };
            // Cursor or wide cells always get their own single-cell run.
            let solo = is_cursor || cell.wide;

            if solo {
                if !run_text.is_empty() {
                    runs.push(Run {
                        col: run_col,
                        row: y,
                        text: std::mem::take(&mut run_text),
                        attrs: run_attrs.clone(),
                    });
                }
                runs.push(Run {
                    col: x,
                    row: y,
                    text: cell.ch.to_string(),
                    attrs: cell_attrs,
                });
                continue;
            }

            // Narrow non-cursor cell — extend or restart the current run.
            if run_text.is_empty() {
                run_col = x;
                run_attrs = cell_attrs;
                run_text.push(cell.ch);
            } else if attrs_eq(&run_attrs, &cell_attrs) {
                run_text.push(cell.ch);
            } else {
                runs.push(Run {
                    col: run_col,
                    row: y,
                    text: std::mem::take(&mut run_text),
                    attrs: run_attrs.clone(),
                });
                run_col = x;
                run_attrs = cell_attrs;
                run_text.push(cell.ch);
            }
        }
        if !run_text.is_empty() {
            runs.push(Run {
                col: run_col,
                row: y,
                text: run_text,
                attrs: run_attrs,
            });
        }
    }
    runs
}

fn cursor_cell(term: &Term) -> Option<(char, bool)> {
    if term.cur_x >= term.cols || term.cur_y >= term.rows {
        return None;
    }
    let i = (term.cur_y as usize) * (term.cols as usize) + (term.cur_x as usize);
    let cell = term.cells.get(i)?;
    if cell.ch == '\0' {
        return None; // landed on a wide-char continuation, suppress
    }
    let ch = if cell.ch.is_whitespace() {
        ' '
    } else {
        cell.ch
    };
    Some((ch, cell.wide))
}

fn attrs_for_cell<'a>(base: Attrs<'a>, fg: Rgb, bold: bool) -> Attrs<'a> {
    let mut a = base.color(rgb_to_color(fg));
    if bold {
        a = a.weight(Weight::BOLD);
    }
    a
}

/// Field-wise equality on the bits we actually vary (color + weight).
fn attrs_eq(a: &Attrs<'_>, b: &Attrs<'_>) -> bool {
    a.color_opt == b.color_opt && a.weight == b.weight
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

/// Pre-linearise sRGB color values for the quad pipeline. The wgpu surface
/// format is sRGB, so the fragment shader output is treated as linear and
/// gamma-encoded on write. Without this conversion every solid quad would
/// render visibly lighter than the source color.
fn rgb_to_rgba(c: Rgb, a: f32) -> [f32; 4] {
    [
        srgb_to_linear(c.0) as f32,
        srgb_to_linear(c.1) as f32,
        srgb_to_linear(c.2) as f32,
        a,
    ]
}

fn font_attrs() -> Attrs<'static> {
    let cfg = live_config();
    let name = current_family_name(cfg.font.family.as_deref().unwrap_or(BUNDLED_FONT_NAME));
    let mut a = Attrs::new().family(Family::Name(name));
    if !cfg.font.ligatures {
        a = a.font_features(ligatures_off());
    }
    a
}

/// Intern the active font-family name as a `&'static str` so `Attrs<'static>`
/// (and the per-row `Run` it lands in) stays valid across hot reloads. The
/// double-check on the read side avoids leaking on every render — only a
/// genuine family change leaks one new string. `BUNDLED_FONT_NAME` is
/// already static so the common case never allocates.
fn current_family_name(want: &str) -> &'static str {
    use std::sync::RwLock;
    static SLOT: std::sync::OnceLock<RwLock<&'static str>> = std::sync::OnceLock::new();
    let slot = SLOT.get_or_init(|| RwLock::new(BUNDLED_FONT_NAME));
    {
        let cur = slot.read().unwrap();
        if *cur == want {
            return *cur;
        }
    }
    let leaked: &'static str = if want == BUNDLED_FONT_NAME {
        BUNDLED_FONT_NAME
    } else {
        Box::leak(want.to_string().into_boxed_str())
    };
    *slot.write().unwrap() = leaked;
    leaked
}

fn ligatures_off() -> FontFeatures {
    let mut f = FontFeatures::new();
    f.disable(FeatureTag::STANDARD_LIGATURES);
    f.disable(FeatureTag::CONTEXTUAL_LIGATURES);
    f.disable(FeatureTag::CONTEXTUAL_ALTERNATES);
    f.disable(FeatureTag::DISCRETIONARY_LIGATURES);
    f
}

/// Highlight color for the mouse selection overlay. Uses the cursor color
/// (which is already chosen to stand out against the background) at
/// reduced alpha so glyphs remain readable through it. Pre-linearised here
/// because the surface format is sRGB and the quad pipeline outputs its
/// fragment color as if it were linear.
fn selection_overlay_color() -> [f32; 4] {
    let cur = cursor_color();
    [
        srgb_to_linear(cur.0) as f32,
        srgb_to_linear(cur.1) as f32,
        srgb_to_linear(cur.2) as f32,
        0.35,
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
