use glyphon::{
    Attrs, Family, Shaping, Weight,
    cosmic_text::{FeatureTag, FontFeatures},
};

use crate::color::{Rgb, default_fg};
use crate::config::config as live_config;
use crate::term::Term;

use super::Renderer;
use super::convert::rgb_to_color;
use super::init::{BUNDLED_FONT_NAME, make_buffer};

#[derive(Debug)]
pub(super) struct Run {
    pub col: u16,
    pub row: u16,
    pub text: String,
    pub attrs: Attrs<'static>,
}

impl Renderer {
    /// Grow the row buffer pool to fit `count` runs and shape each one. The
    /// pool is reused across frames; we re-set_text on every render so old
    /// content beyond `runs.len()` is harmless (just not referenced).
    pub(super) fn shape_runs(&mut self, runs: &[Run]) {
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
    pub(super) fn shape_preedit(
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
}

/// Walk the grid and emit one `Run` per maximal stretch of cells that can be
/// shaped together: same attrs, no wide character interrupting. Each wide
/// character becomes its own single-cell run so the next run can start at an
/// exact grid column without depending on the wide glyph's natural advance.
/// `cursor_override` forces the matching cell into its own run with the given
/// attrs, used for the inverted cursor character.
pub(super) fn build_runs(
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

pub(super) fn cursor_cell(term: &Term) -> Option<(char, bool)> {
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

pub(super) fn font_attrs() -> Attrs<'static> {
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
