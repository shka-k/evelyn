use crate::color::Color;
use crate::width::cell_width;

use super::{Cell, Term};
use super::charset::{Charset, dec_special_graphics};

impl Term {
    pub(super) fn reset_attrs(&mut self) {
        self.fg = Color::Default;
        self.bg = Color::Default;
        self.bold = false;
        self.reverse = false;
    }

    pub(super) fn put_char(&mut self, c: char) {
        let c = if self.active_cs() == Charset::DecSpecialGraphics {
            dec_special_graphics(c)
        } else {
            c
        };
        // Zero-width codepoints — combining marks, variation selectors
        // (incl. VS16 `\u{FE0F}` after `⚠`, `☂`, etc.), ZWJ, and other
        // default-ignorables — must not advance the cursor. Allocating a
        // cell for them shifts every subsequent cell on the row one
        // column right, which is exactly what made zellij's pane borders
        // land in the wrong place whenever the inner pane streamed an
        // emoji presentation sequence past us. We don't yet store
        // combining sequences in Cell, so drop them; the previous base
        // glyph stays put.
        let width = cell_width(c);
        if width == 0 {
            return;
        }
        let wide = width == 2;
        let needed = u16::from(width);
        // Consume a deferred wrap from a previous print at the right edge.
        if self.auto_wrap && self.pending_wrap {
            self.pending_wrap = false;
            self.cur_x = 0;
            self.line_feed();
        }
        if self.cur_x + needed > self.cols {
            if self.auto_wrap {
                self.cur_x = 0;
                self.line_feed();
            } else {
                // Overwrite mode: clamp to the last cell that will fit.
                self.cur_x = self.cols.saturating_sub(needed);
            }
        }
        let i = self.idx(self.cur_x, self.cur_y);
        self.cells[i] = Cell {
            ch: c,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            reverse: self.reverse,
            wide,
        };
        if wide && self.cur_x + 1 < self.cols {
            // Mark the right half so the renderer knows to skip it.
            let j = self.idx(self.cur_x + 1, self.cur_y);
            self.cells[j] = Cell {
                ch: '\0',
                fg: self.fg,
                bg: self.bg,
                bold: self.bold,
                reverse: self.reverse,
                wide: false,
            };
        }
        let new_x = self.cur_x + needed;
        if new_x >= self.cols {
            // Park the cursor on the last written cell. With DECAWM, set the
            // pending-wrap flag so the *next* print performs the wrap; this
            // is what lets TUIs draw the bottom-right corner without
            // scrolling the screen.
            self.cur_x = self.cols.saturating_sub(needed);
            if self.auto_wrap {
                self.pending_wrap = true;
            }
        } else {
            self.cur_x = new_x;
        }
        self.mark_row(self.cur_y);
    }

    pub(super) fn line_feed(&mut self) {
        self.pending_wrap = false;
        // Scroll the DECSTBM region only when the cursor sits exactly on
        // its bottom row. If the cursor is below the region (e.g. zellij
        // drawing into a status row past `scroll_bot`), LF must just walk
        // the cursor down — scrolling the region from there would push
        // pane content up under the status bar, which is what made the
        // inner shell prompt sometimes vanish on startup.
        if self.cur_y == self.scroll_bot {
            self.scroll_up_in_region(1);
        } else if self.cur_y + 1 < self.rows {
            self.cur_y += 1;
        }
        self.dirty = true;
    }

    pub(super) fn carriage_return(&mut self) {
        self.cur_x = 0;
        self.pending_wrap = false;
        self.dirty = true;
    }

    pub(super) fn backspace(&mut self) {
        self.pending_wrap = false;
        if self.cur_x > 0 {
            self.cur_x -= 1;
            self.dirty = true;
        }
    }

    pub(super) fn tab(&mut self) {
        let next = ((self.cur_x / 8) + 1) * 8;
        self.cur_x = next.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
        self.dirty = true;
    }

    /// Insert `n` blank cells at the cursor (CSI @, ICH). Cells from the
    /// cursor to the row end shift right; ones falling off the right are
    /// lost. Cursor stays put.
    pub(super) fn insert_chars(&mut self, n: u16) {
        let cols = self.cols as usize;
        if cols == 0 {
            return;
        }
        let row_start = (self.cur_y as usize) * cols;
        let cur = (self.cur_x as usize).min(cols);
        let n = (n as usize).min(cols - cur);
        if n == 0 {
            return;
        }
        let row_end = row_start + cols;
        let move_src_end = row_end - n;
        let move_src_start = row_start + cur;
        if move_src_start < move_src_end {
            self.cells.copy_within(move_src_start..move_src_end, move_src_start + n);
        }
        let blank = self.blank_cell();
        for cell in &mut self.cells[move_src_start..move_src_start + n] {
            *cell = blank;
        }
        self.pending_wrap = false;
        self.mark_row(self.cur_y);
    }

    /// Delete `n` cells at the cursor (CSI P, DCH). Cells right of the
    /// cursor shift left; the right end is filled with blanks.
    pub(super) fn delete_chars(&mut self, n: u16) {
        let cols = self.cols as usize;
        if cols == 0 {
            return;
        }
        let row_start = (self.cur_y as usize) * cols;
        let cur = (self.cur_x as usize).min(cols);
        let n = (n as usize).min(cols - cur);
        if n == 0 {
            return;
        }
        let row_end = row_start + cols;
        let src_start = row_start + cur + n;
        let dst_start = row_start + cur;
        if src_start < row_end {
            self.cells.copy_within(src_start..row_end, dst_start);
        }
        let blank = self.blank_cell();
        let blank_start = row_end - n;
        for cell in &mut self.cells[blank_start..row_end] {
            *cell = blank;
        }
        self.pending_wrap = false;
        self.mark_row(self.cur_y);
    }

    /// Erase `n` cells in place at the cursor (CSI X, ECH). Cells stay
    /// where they are, just blanked with current SGR bg.
    pub(super) fn erase_chars(&mut self, n: u16) {
        let cols = self.cols as usize;
        if cols == 0 {
            return;
        }
        let row_start = (self.cur_y as usize) * cols;
        let cur = (self.cur_x as usize).min(cols);
        let n = (n as usize).min(cols - cur);
        let blank = self.blank_cell();
        for cell in &mut self.cells[row_start + cur..row_start + cur + n] {
            *cell = blank;
        }
        self.pending_wrap = false;
        self.mark_row(self.cur_y);
    }

    pub(super) fn erase_in_display(&mut self, mode: u16) {
        let total = self.cells.len();
        let cur = self.idx(self.cur_x, self.cur_y);
        let (start, end) = match mode {
            0 => (cur, total),
            1 => (0, (cur + 1).min(total)),
            2 | 3 => (0, total),
            _ => return,
        };
        let blank = self.blank_cell();
        for cell in &mut self.cells[start..end] {
            *cell = blank;
        }
        // erase_in_display can hit anywhere from a single row (mode 0/1
        // when the cursor is on row 0 or rows-1) up to the entire screen
        // (mode 2/3). Computing the precise row range is fiddly; just
        // mark every row dirty — full-screen erase is the common case
        // and per-row erase already marks via mark_row in erase_in_line.
        self.mark_all_rows();
    }

    pub(super) fn erase_in_line(&mut self, mode: u16) {
        let row_start = self.idx(0, self.cur_y);
        let row_end = row_start + self.cols as usize;
        let cur = self.idx(self.cur_x, self.cur_y);
        let (start, end) = match mode {
            0 => (cur, row_end),
            1 => (row_start, (cur + 1).min(row_end)),
            2 => (row_start, row_end),
            _ => return,
        };
        let blank = self.blank_cell();
        for cell in &mut self.cells[start..end] {
            *cell = blank;
        }
        self.mark_row(self.cur_y);
    }
}
