use crate::color::{Rgb, cursor_color};
use crate::config::CursorShape;
use crate::term::Term;

use super::Renderer;
use super::convert::{rgb_to_rgba, srgb_to_linear};
use super::quad::Rect;

const PREEDIT_UNDERLINE_PT: f32 = 2.0;

impl Renderer {
    /// Cursor in the configured shape, or — when IME is composing — the
    /// preedit underline plus a Bar caret that tracks the IME's reported
    /// caret position inside the preedit.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn overlay_quads(
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
    pub(super) fn build_selection_quads(&self, term: &Term) -> Vec<Rect> {
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
    pub(super) fn build_bg_quads(
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
                if let Some((my, ms, me)) = mask
                    && y == my
                    && (x as u16) >= ms
                    && (x as u16) < me
                {
                    x += 1;
                    continue;
                }
                let bg = term.cell_at(x as u16, y).bg_eff();
                let mut end = x + 1;
                while end < cols && term.cell_at(end as u16, y).bg_eff() == bg {
                    if let Some((my, ms, me)) = mask
                        && y == my
                        && (end as u16) >= ms
                        && (end as u16) < me
                    {
                        break;
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
