mod init;
mod quad;

use std::sync::Arc;

use anyhow::Result;
use glyphon::{
    cosmic_text::{FeatureTag, FontFeatures},
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use wgpu::{
    CommandEncoderDescriptor, CurrentSurfaceTexture, LoadOp, Operations,
    RenderPassColorAttachment, RenderPassDescriptor, StoreOp, SurfaceConfiguration,
    TextureViewDescriptor,
};
use winit::window::Window;

use crate::color::{cursor_color, cursor_text_color, default_bg, default_fg, Rgb};
use crate::config::CONFIG;
use crate::term::Term;

use init::{
    init_gpu, init_text_stack, make_buffer, measure_cell_width, metrics_for, GpuInit, TextInit,
    BUNDLED_FONT_NAME,
};
use quad::{QuadPipeline, Rect};

const PREEDIT_COLOR: Rgb = Rgb(0xff, 0xe0, 0x70);
const PREEDIT_UNDERLINE: Rgb = Rgb(0xff, 0xe0, 0x70);
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
    scale: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub cell_width: f32,
    /// Window inset around the grid in surface pixels. `CONFIG.window.padding`
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
        let padding = (CONFIG.window.padding * scale).round();

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
        self.padding = (CONFIG.window.padding * scale).round();
        let m = Metrics::new(font_size, line_height);
        for buf in &mut self.row_buffers {
            buf.set_metrics(&mut self.font_system, m);
        }
        self.preedit_buffer.set_metrics(&mut self.font_system, m);
        self.cell_width = measure_cell_width(&mut self.font_system, font_size, line_height);
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.config.width = w.max(1);
        self.config.height = h.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn grid_size(&self) -> (u16, u16) {
        let usable_w = (self.config.width as f32 - 2.0 * self.padding).max(0.0);
        let usable_h = (self.config.height as f32 - 2.0 * self.padding).max(0.0);
        let cols = ((usable_w / self.cell_width).floor() as u16).max(1);
        let rows = ((usable_h / self.line_height).floor() as u16).max(1);
        (cols, rows)
    }

    pub fn render(&mut self, term: &Term, preedit: &str) -> Result<()> {
        let show_cursor = preedit.is_empty() && term.cursor_visible;
        let cursor_wide = if show_cursor {
            cursor_cell(term).map(|(_, w)| w).unwrap_or(false)
        } else {
            false
        };

        // Build grid runs and shape each into a dedicated row buffer. This
        // keeps every run pinned to its grid column so font fallback widths
        // can't drift the layout. The cursor cell becomes its own run with
        // an inverted foreground color, painted on top of the solid block.
        let base = font_attrs();
        let cursor_override = if show_cursor {
            Some((
                term.cur_x,
                term.cur_y,
                base.clone().color(rgb_to_color(cursor_text_color())),
            ))
        } else {
            None
        };
        let runs = build_runs(term, base.clone(), cursor_override);
        self.shape_runs(&runs);

        // Preedit
        let has_preedit = !preedit.is_empty();
        let preedit_w = if has_preedit {
            self.shape_preedit(preedit, base.clone())
        } else {
            0.0
        };

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
        let overlay =
            self.overlay_quads(term, has_preedit, preedit_w, show_cursor, cursor_wide, pre_left, pre_top);

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
                        load: LoadOp::Clear(clear_color_for(default_bg())),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            // Per-cell SGR backgrounds first, then the cursor block on top,
            // then text. The cursor cell's char run uses cursor_text as its
            // foreground so it appears inverted against the cursor block.
            let mut quads = self.build_bg_quads(term);
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
            let mut buf = make_buffer(&mut self.font_system, self.font_size, self.line_height);
            // Wide enough for any single run on a single line.
            buf.set_size(
                &mut self.font_system,
                Some(self.config.width as f32),
                Some(self.line_height * 2.0),
            );
            self.row_buffers.push(buf);
        }
        for (i, run) in runs.iter().enumerate() {
            let buf = &mut self.row_buffers[i];
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

    fn shape_preedit(&mut self, preedit: &str, base: Attrs<'_>) -> f32 {
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
        for run in self.preedit_buffer.layout_runs() {
            for g in run.glyphs.iter() {
                max_x = max_x.max(g.x + g.w);
            }
        }
        max_x
    }

    /// Solid cursor block, or — when IME is composing — the preedit underline.
    fn overlay_quads(
        &self,
        term: &Term,
        has_preedit: bool,
        preedit_w: f32,
        show_cursor: bool,
        cursor_wide: bool,
        pre_left: f32,
        pre_top: f32,
    ) -> Vec<Rect> {
        if has_preedit {
            let underline_h = (PREEDIT_UNDERLINE_PT * self.scale).round().max(1.0);
            let w = preedit_w.max(self.cell_width);
            return vec![Rect {
                x: pre_left,
                y: pre_top + self.line_height - underline_h,
                w,
                h: underline_h,
                color: rgb_to_rgba(PREEDIT_UNDERLINE, 1.0),
            }];
        }
        if !show_cursor {
            return Vec::new();
        }
        let block_w = if cursor_wide {
            self.cell_width * 2.0
        } else {
            self.cell_width
        };
        vec![Rect {
            x: term.cur_x as f32 * self.cell_width + self.padding,
            y: term.cur_y as f32 * self.line_height + self.padding,
            w: block_w,
            h: self.line_height,
            color: rgb_to_rgba(cursor_color(), 1.0),
        }]
    }

    /// One opaque rect per maximal stretch of cells that share a non-default
    /// SGR background within a row. Cells whose bg equals the theme
    /// background are skipped — the surface clear already covers them.
    fn build_bg_quads(&self, term: &Term) -> Vec<Rect> {
        let bg_default = default_bg();
        let cols = term.cols as usize;
        let mut quads = Vec::new();
        for y in 0..term.rows {
            let row_start = (y as usize) * cols;
            let mut x: usize = 0;
            while x < cols {
                let bg = term.cells[row_start + x].bg;
                let mut end = x + 1;
                while end < cols && term.cells[row_start + end].bg == bg {
                    end += 1;
                }
                if bg != bg_default {
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
) -> Vec<Run> {
    let mut runs = Vec::new();

    for y in 0..term.rows {
        let mut run_col: u16 = 0;
        let mut run_attrs = attrs_for_cell(base.clone(), default_fg(), false);
        let mut run_text = String::new();

        for x in 0..term.cols {
            let i = (y as usize) * (term.cols as usize) + (x as usize);
            let cell = &term.cells[i];
            if cell.ch == '\0' {
                continue; // wide-char continuation
            }
            let is_cursor = cursor_override
                .as_ref()
                .map(|(cx, cy, _)| *cx == x && *cy == y)
                .unwrap_or(false);
            let cell_attrs = if is_cursor {
                cursor_override.as_ref().unwrap().2.clone()
            } else {
                attrs_for_cell(base.clone(), cell.fg, cell.bold)
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
    let ch = if cell.ch.is_whitespace() { ' ' } else { cell.ch };
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
    let name = CONFIG.font.family.as_deref().unwrap_or(BUNDLED_FONT_NAME);
    let mut a = Attrs::new().family(Family::Name(name));
    if !CONFIG.font.ligatures {
        a = a.font_features(ligatures_off());
    }
    a
}

fn ligatures_off() -> FontFeatures {
    let mut f = FontFeatures::new();
    f.disable(FeatureTag::STANDARD_LIGATURES);
    f.disable(FeatureTag::CONTEXTUAL_LIGATURES);
    f.disable(FeatureTag::CONTEXTUAL_ALTERNATES);
    f.disable(FeatureTag::DISCRETIONARY_LIGATURES);
    f
}

fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}
