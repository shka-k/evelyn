use crate::color::Color;

use super::{Cell, HISTORY_CAP, Term};

#[derive(Clone, Copy)]
pub(super) struct SavedCursor {
    pub cur_x: u16,
    pub cur_y: u16,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub reverse: bool,
}

pub(super) struct SavedScreen {
    pub cells: Vec<Cell>,
    pub cur_x: u16,
    pub cur_y: u16,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub reverse: bool,
}

impl Term {
    /// Adjust the scrollback view. Positive = scroll back (older content),
    /// negative = scroll forward toward the live bottom. Clamped to the
    /// history length. No-op while in alt screen — apps own that surface
    /// and we don't have history rows there.
    pub fn scroll_view(&mut self, delta_lines: i32) {
        if self.is_alt_screen() {
            return;
        }
        let cur = self.view_offset as i64;
        let new = (cur + delta_lines as i64).max(0).min(self.history.len() as i64);
        let new = new as usize;
        if new != self.view_offset {
            self.view_offset = new;
            self.dirty = true;
        }
    }

    pub fn reset_view(&mut self) {
        if self.view_offset != 0 {
            self.view_offset = 0;
            self.dirty = true;
        }
    }

    /// Enter alt screen — apps like helix expect this to give them a clean
    /// canvas and snap cursor to (0,0). We snapshot main so `\\e[?1049l`
    /// can restore the shell prompt.
    pub(super) fn enter_alt_screen(&mut self) {
        if self.saved.is_none() {
            self.saved = Some(SavedScreen {
                cells: self.cells.clone(),
                cur_x: self.cur_x,
                cur_y: self.cur_y,
                fg: self.fg,
                bg: self.bg,
                bold: self.bold,
                reverse: self.reverse,
            });
        }
        let blank = self.blank_cell();
        for cell in &mut self.cells {
            *cell = blank;
        }
        self.cur_x = 0;
        self.cur_y = 0;
        // Don't carry the previous app's scroll region or saved cursor
        // into the alt screen — apps assume a clean state on entry.
        self.scroll_top = 0;
        self.scroll_bot = self.rows.saturating_sub(1);
        self.saved_cursor = None;
        self.pending_wrap = false;
        // Snap any active scrollback view back to the live bottom — alt
        // screen owns its own surface and history is meaningless there.
        self.view_offset = 0;
        self.dirty = true;
    }

    pub(super) fn exit_alt_screen(&mut self) {
        let Some(s) = self.saved.take() else { return };
        let needed = (self.cols as usize) * (self.rows as usize);
        // Window may have been resized while we were in alt screen.
        // If so, just clear; the shell will repaint when it gets focus back.
        if s.cells.len() == needed {
            self.cells = s.cells;
            self.cur_x = s.cur_x.min(self.cols.saturating_sub(1));
            self.cur_y = s.cur_y.min(self.rows.saturating_sub(1));
            self.fg = s.fg;
            self.bg = s.bg;
            self.bold = s.bold;
            self.reverse = s.reverse;
        } else {
            let blank = self.blank_cell();
            for cell in &mut self.cells {
                *cell = blank;
            }
            self.cur_x = 0;
            self.cur_y = 0;
        }
        // Reset the alt-screen-only scroll region / saved cursor on exit
        // so they don't leak into the shell.
        self.scroll_top = 0;
        self.scroll_bot = self.rows.saturating_sub(1);
        self.saved_cursor = None;
        self.pending_wrap = false;
        self.dirty = true;
    }

    /// Scroll the DECSTBM region up by `n` lines: rows [top, bot] shift
    /// upward, the bottom `n` rows are blanked with current SGR bg. Used
    /// for LF at the region bottom and for CSI S.
    pub(super) fn scroll_up_in_region(&mut self, n: u16) {
        let cols = self.cols as usize;
        let top = self.scroll_top as usize;
        let bot = self.scroll_bot as usize;
        if top > bot || cols == 0 {
            return;
        }
        let n = (n as usize).min(bot - top + 1);
        let band_start = top * cols;
        let band_end = (bot + 1) * cols;
        let shift = n * cols;

        // Full-screen scroll on the main screen → the displaced top rows
        // become scrollback history. Region scrolls (zellij panes, CSI L /
        // CSI M from a non-zero cursor row) and alt-screen scrolls don't
        // contribute to history.
        let full_screen = top == 0 && bot + 1 == self.rows as usize;
        if full_screen && self.saved.is_none() {
            for row in 0..n {
                let row_start = band_start + row * cols;
                let row_cells = self.cells[row_start..row_start + cols].to_vec();
                if self.history.len() == HISTORY_CAP {
                    self.history.pop_front();
                    // Stays monotonic — feeds the global line index that
                    // anchors selections across history rolling.
                    self.history_dropped += 1;
                } else if self.view_offset > 0 {
                    // Anchor the scrollback view to the same content as
                    // new lines push in. Only bump while we still have
                    // headroom; once history hits the cap, the oldest
                    // row drops and the view naturally drifts forward.
                    self.view_offset += 1;
                }
                self.history.push_back(row_cells);
            }
        }

        if shift < band_end - band_start {
            self.cells
                .copy_within(band_start + shift..band_end, band_start);
        }
        let blank_start = band_end - shift;
        let blank = self.blank_cell();
        for cell in &mut self.cells[blank_start..band_end] {
            *cell = blank;
        }
    }

    /// Scroll the DECSTBM region down by `n` lines: rows [top, bot] shift
    /// downward, the top `n` rows are blanked. Used for CSI T and reverse
    /// index (RI / `\\eM`).
    pub(super) fn scroll_down_in_region(&mut self, n: u16) {
        let cols = self.cols as usize;
        let top = self.scroll_top as usize;
        let bot = self.scroll_bot as usize;
        if top > bot || cols == 0 {
            return;
        }
        let n = (n as usize).min(bot - top + 1);
        let band_start = top * cols;
        let band_end = (bot + 1) * cols;
        let shift = n * cols;
        if shift < band_end - band_start {
            self.cells
                .copy_within(band_start..band_end - shift, band_start + shift);
        }
        let blank = self.blank_cell();
        for cell in &mut self.cells[band_start..band_start + shift] {
            *cell = blank;
        }
    }

    pub(super) fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            cur_x: self.cur_x,
            cur_y: self.cur_y,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            reverse: self.reverse,
        });
    }

    pub(super) fn restore_cursor(&mut self) {
        if let Some(s) = self.saved_cursor {
            self.cur_x = s.cur_x.min(self.cols.saturating_sub(1));
            self.cur_y = s.cur_y.min(self.rows.saturating_sub(1));
            self.fg = s.fg;
            self.bg = s.bg;
            self.bold = s.bold;
            self.reverse = s.reverse;
        } else {
            self.cur_x = 0;
            self.cur_y = 0;
        }
        self.pending_wrap = false;
        self.dirty = true;
    }

    pub(super) fn set_scroll_region(&mut self, top: u16, bot: u16) {
        let last = self.rows.saturating_sub(1);
        let top = top.min(last);
        let bot = bot.min(last);
        // `top == bot` is a valid 1-row region; only `top > bot` is
        // invalid and falls back to the full screen.
        if top <= bot {
            self.scroll_top = top;
            self.scroll_bot = bot;
        } else {
            self.scroll_top = 0;
            self.scroll_bot = last;
        }
        // DECSTBM moves cursor to (1,1).
        self.cur_x = 0;
        self.cur_y = 0;
        self.pending_wrap = false;
    }

    /// Insert `n` blank lines at the cursor row, pushing the rows below
    /// (down to `scroll_bot`) downward. Used for CSI L. No-op when the
    /// cursor is outside the scroll region.
    pub(super) fn insert_lines(&mut self, n: u16) {
        if self.cur_y < self.scroll_top || self.cur_y > self.scroll_bot {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cur_y;
        self.scroll_down_in_region(n);
        self.scroll_top = saved_top;
        self.cur_x = 0;
        self.dirty = true;
    }

    /// Delete `n` lines at the cursor row, pulling the rows below upward.
    /// Used for CSI M.
    pub(super) fn delete_lines(&mut self, n: u16) {
        if self.cur_y < self.scroll_top || self.cur_y > self.scroll_bot {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cur_y;
        self.scroll_up_in_region(n);
        self.scroll_top = saved_top;
        self.cur_x = 0;
        self.dirty = true;
    }

    /// Reverse Index (`\\eM`). Move cursor up; if at scroll region top,
    /// scroll the region down by one. Used by less / man / vim.
    pub(super) fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.cur_y == self.scroll_top {
            self.scroll_down_in_region(1);
        } else if self.cur_y > 0 {
            self.cur_y -= 1;
        }
        self.dirty = true;
    }
}
