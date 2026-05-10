mod parser;

use crate::color::{default_bg, default_fg, Rgb};
use crate::width::is_wide;

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    /// `'\0'` marks the right half of a wide character (continuation).
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    /// Set on the LEFT half of a wide character. Its right neighbour is a
    /// continuation cell with `ch == '\0'`.
    pub wide: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: default_fg(),
            bg: default_bg(),
            bold: false,
            wide: false,
        }
    }
}

pub struct Term {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub cur_x: u16,
    pub cur_y: u16,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub dirty: bool,
    /// `\\e[?25 h/l` — apps like helix or less hide the cursor while
    /// rendering. The renderer skips the block when this is false.
    pub cursor_visible: bool,
    /// DECAWM (`\\e[?7 h/l`). When false, the cursor stops at the right
    /// edge instead of wrapping; subsequent characters overwrite the last
    /// column. zellij and similar TUIs disable this while drawing borders.
    pub auto_wrap: bool,
    /// VT100 "last column" / deferred wrap. Set after a print lands in the
    /// rightmost column with DECAWM on; the wrap is held until the next
    /// print, and any cursor motion (CR/LF/BS/CUP/…) cancels it. Without
    /// this, drawing a box-corner glyph at (rows-1, cols-1) would scroll
    /// the whole screen — zellij/vim/tmux all rely on the deferral.
    pending_wrap: bool,
    /// Bytes the terminal needs to send back to the host program (DA, DSR, …).
    /// Drained by the application after each parser advance.
    pub replies: Vec<u8>,
    /// Snapshot of the main screen kept while we're in alt screen
    /// (`\\e[?1049h`). On exit (`\\e[?1049l`) we restore it.
    saved: Option<SavedScreen>,
}

struct SavedScreen {
    cells: Vec<Cell>,
    cur_x: u16,
    cur_y: u16,
    fg: Rgb,
    bg: Rgb,
    bold: bool,
}

impl Term {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); (cols as usize) * (rows as usize)],
            cur_x: 0,
            cur_y: 0,
            fg: default_fg(),
            bg: default_bg(),
            bold: false,
            dirty: true,
            cursor_visible: true,
            auto_wrap: true,
            pending_wrap: false,
            replies: Vec::new(),
            saved: None,
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
            });
        }
        let blank = self.blank_cell();
        for cell in &mut self.cells {
            *cell = blank;
        }
        self.cur_x = 0;
        self.cur_y = 0;
        self.pending_wrap = false;
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
        } else {
            let blank = self.blank_cell();
            for cell in &mut self.cells {
                *cell = blank;
            }
            self.cur_x = 0;
            self.cur_y = 0;
        }
        self.pending_wrap = false;
        self.dirty = true;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![Cell::default(); (cols as usize) * (rows as usize)];
        self.cur_x = self.cur_x.min(cols.saturating_sub(1));
        self.cur_y = self.cur_y.min(rows.saturating_sub(1));
        self.pending_wrap = false;
        self.dirty = true;
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        (y as usize) * (self.cols as usize) + (x as usize)
    }

    fn reset_attrs(&mut self) {
        self.fg = default_fg();
        self.bg = default_bg();
        self.bold = false;
    }

    fn put_char(&mut self, c: char) {
        let wide = is_wide(c);
        let needed = if wide { 2 } else { 1 };
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
        self.dirty = true;
    }

    fn line_feed(&mut self) {
        self.pending_wrap = false;
        if self.cur_y + 1 >= self.rows {
            // Scroll up by one line. New bottom row inherits current SGR
            // background so apps that paint a row of bg + LF get the bg
            // applied to the freshly-revealed line.
            let cols = self.cols as usize;
            self.cells.copy_within(cols.., 0);
            let n = self.cells.len();
            let blank = self.blank_cell();
            for cell in &mut self.cells[n - cols..] {
                *cell = blank;
            }
        } else {
            self.cur_y += 1;
        }
        self.dirty = true;
    }

    /// A blank cell carrying the current SGR attributes — what `\\e[K` and
    /// `\\e[J` should leave behind so colored erase actually paints.
    fn blank_cell(&self) -> Cell {
        Cell {
            ch: ' ',
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            wide: false,
        }
    }

    fn carriage_return(&mut self) {
        self.cur_x = 0;
        self.pending_wrap = false;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        self.pending_wrap = false;
        if self.cur_x > 0 {
            self.cur_x -= 1;
            self.dirty = true;
        }
    }

    fn tab(&mut self) {
        let next = ((self.cur_x / 8) + 1) * 8;
        self.cur_x = next.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
        self.dirty = true;
    }

    fn erase_in_display(&mut self, mode: u16) {
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
        self.dirty = true;
    }

    fn erase_in_line(&mut self, mode: u16) {
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
        self.dirty = true;
    }
}
