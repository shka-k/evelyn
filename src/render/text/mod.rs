//! Text shaping + rasterization backend.
//!
//! `TextEngine` is the abstraction the rest of the renderer talks to. The
//! current implementation lives in [`cosmic`] and is built on cosmic-text
//! via glyphon. Future backends (e.g. CoreText for native macOS emoji-
//! presentation handling) plug in by implementing the same trait — the
//! Renderer holds a `Box<dyn TextEngine>` and never names a backend type.
//!
//! Backend-agnostic helpers (`build_runs`, `cursor_cell`) and value types
//! (`Run`, `PreeditMetrics`) live here so backends only need to translate
//! these into their own shaping primitives.

pub mod cosmic;

use anyhow::Result;

use crate::color::{Rgb, default_fg};
use crate::term::Term;

/// Bundled monospace font shipped with the binary so first-run looks
/// consistent regardless of host. Backends are free to load these into
/// whatever font registry they use.
pub const FONT_PRIMARY_REGULAR_BYTES: &[u8] =
    include_bytes!("../../../assets/fonts/GeistMonoNerdFontMono-Regular.otf");
pub const FONT_PRIMARY_BOLD_BYTES: &[u8] =
    include_bytes!("../../../assets/fonts/GeistMonoNerdFontMono-Bold.otf");

/// Family name reported by the bundled font. Used as the default when
/// `config.font.family` is unset.
pub const BUNDLED_FONT_NAME: &str = "GeistMono NFM";

/// One maximal stretch of cells that can be shaped together: same fg /
/// bold, no wide character interrupting. Each wide cell becomes its own
/// single-cell run so the next run starts at an exact grid column without
/// depending on the wide glyph's natural advance.
#[derive(Debug, Clone)]
pub struct Run {
    pub col: u16,
    pub row: u16,
    pub text: String,
    pub fg: Rgb,
    pub bold: bool,
}

/// Result of shaping the IME preedit string. `width` is the rendered
/// pixel width of the whole preedit; `caret_x` is the offset of the IME
/// caret within it (clamped to the trailing edge when the IME parks the
/// caret past the last glyph, which macOS routinely does).
#[derive(Debug, Clone, Copy, Default)]
pub struct PreeditMetrics {
    pub width: f32,
    pub caret_x: f32,
}

/// Backend trait: owns font registry, shaping, glyph atlas, and the wgpu
/// pipeline that draws shaped text. Lifetimes on `render` mirror
/// `glyphon::TextRenderer::render` so the implementor can borrow its
/// atlas/viewport for the duration of the pass.
pub trait TextEngine {
    /// Update font size + line height. Implementations should reshape any
    /// internally-cached buffers and re-measure `cell_width`.
    fn set_metrics(&mut self, font_size: f32, line_height: f32);

    /// Average advance for the bundled monospace face at the current
    /// metrics. Cached internally — cheap to call every frame.
    fn cell_width(&self) -> f32;

    /// Drop cached glyph rasters. Called after a config reload that may
    /// have changed font/size, so newly-shaped runs don't sample stale
    /// entries.
    fn trim(&mut self);

    /// Shape the IME preedit string into the engine's internal buffer.
    /// The caller uses the returned width to derive the preedit mask.
    fn shape_preedit(&mut self, preedit: &str, preedit_cursor: usize) -> PreeditMetrics;

    /// Shape grid runs into per-row buffers.
    ///
    /// `cursor_pos`, when set, marks one cell whose glyph should be drawn
    /// in `cursor_text_color` instead of its run's fg — used for the
    /// inverted block-cursor character.
    fn shape_runs(&mut self, runs: &[Run], cursor_pos: Option<(u16, u16)>);

    /// Stage shaped glyphs into the atlas, ready for `render`. Called
    /// once per frame after both `shape_*` calls. `preedit_origin` is the
    /// (left, top) where the preedit buffer should be drawn in surface
    /// pixels; `None` means no preedit this frame.
    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_size: (u32, u32),
        cell_width: f32,
        line_height: f32,
        padding: f32,
        runs: &[Run],
        preedit_origin: Option<(f32, f32)>,
    ) -> Result<()>;

    /// Draw the staged text into the given render pass. The `'pass`
    /// lifetime ties the engine's internal atlas/viewport to the pass
    /// the same way glyphon's renderer expects.
    fn render<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) -> Result<()>;
}

/// Walk the grid and emit one `Run` per maximal stretch of cells with
/// uniform fg/bold and no wide char in the middle. Each wide character
/// becomes its own single-cell run so the next run starts at an exact
/// grid column. `cursor_override` forces the matching cell into its own
/// run (color is applied by the engine when `cursor_pos` matches).
pub fn build_runs(
    term: &Term,
    cursor_override: Option<(u16, u16)>,
    mask: Option<(u16, u16, u16)>,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();

    for y in 0..term.rows {
        let mut run_col: u16 = 0;
        let mut run_fg: Rgb = default_fg();
        let mut run_bold: bool = false;
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
                        fg: run_fg,
                        bold: run_bold,
                    });
                }
                continue;
            }
            let is_cursor = cursor_override
                .map(|(cx, cy)| cx == x && cy == y)
                .unwrap_or(false);
            let cell_fg = cell.fg_eff();
            let cell_bold = cell.bold;
            // Cursor or wide cells always get their own single-cell run.
            let solo = is_cursor || cell.wide;

            if solo {
                if !run_text.is_empty() {
                    runs.push(Run {
                        col: run_col,
                        row: y,
                        text: std::mem::take(&mut run_text),
                        fg: run_fg,
                        bold: run_bold,
                    });
                }
                runs.push(Run {
                    col: x,
                    row: y,
                    text: cell.ch.to_string(),
                    fg: cell_fg,
                    bold: cell_bold,
                });
                continue;
            }

            // Narrow non-cursor cell — extend or restart the current run.
            if run_text.is_empty() {
                run_col = x;
                run_fg = cell_fg;
                run_bold = cell_bold;
                run_text.push(cell.ch);
            } else if cell_fg == run_fg && cell_bold == run_bold {
                run_text.push(cell.ch);
            } else {
                runs.push(Run {
                    col: run_col,
                    row: y,
                    text: std::mem::take(&mut run_text),
                    fg: run_fg,
                    bold: run_bold,
                });
                run_col = x;
                run_fg = cell_fg;
                run_bold = cell_bold;
                run_text.push(cell.ch);
            }
        }
        if !run_text.is_empty() {
            runs.push(Run {
                col: run_col,
                row: y,
                text: run_text,
                fg: run_fg,
                bold: run_bold,
            });
        }
    }
    runs
}

/// Pull the character + wide-flag at the cursor position, suppressing
/// wide-char continuation cells so the cursor doesn't paint blanks.
pub fn cursor_cell(term: &Term) -> Option<(char, bool)> {
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
