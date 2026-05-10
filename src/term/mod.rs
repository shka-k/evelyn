mod parser;

use crate::color::{Rgb, DEFAULT_BG, DEFAULT_FG};
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
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
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
    /// Bytes the terminal needs to send back to the host program (DA, DSR, …).
    /// Drained by the application after each parser advance.
    pub replies: Vec<u8>,
}

impl Term {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); (cols as usize) * (rows as usize)],
            cur_x: 0,
            cur_y: 0,
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            dirty: true,
            replies: Vec::new(),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![Cell::default(); (cols as usize) * (rows as usize)];
        self.cur_x = self.cur_x.min(cols.saturating_sub(1));
        self.cur_y = self.cur_y.min(rows.saturating_sub(1));
        self.dirty = true;
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        (y as usize) * (self.cols as usize) + (x as usize)
    }

    fn reset_attrs(&mut self) {
        self.fg = DEFAULT_FG;
        self.bg = DEFAULT_BG;
        self.bold = false;
    }

    fn put_char(&mut self, c: char) {
        let wide = is_wide(c);
        let needed = if wide { 2 } else { 1 };
        if self.cur_x + needed > self.cols {
            self.cur_x = 0;
            self.line_feed();
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
        self.cur_x += needed;
        self.dirty = true;
    }

    fn line_feed(&mut self) {
        if self.cur_y + 1 >= self.rows {
            // Scroll up by one line.
            let cols = self.cols as usize;
            self.cells.copy_within(cols.., 0);
            let n = self.cells.len();
            for cell in &mut self.cells[n - cols..] {
                *cell = Cell::default();
            }
        } else {
            self.cur_y += 1;
        }
        self.dirty = true;
    }

    fn carriage_return(&mut self) {
        self.cur_x = 0;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cur_x > 0 {
            self.cur_x -= 1;
            self.dirty = true;
        }
    }

    fn tab(&mut self) {
        let next = ((self.cur_x / 8) + 1) * 8;
        self.cur_x = next.min(self.cols.saturating_sub(1));
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
        for cell in &mut self.cells[start..end] {
            *cell = Cell::default();
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
        for cell in &mut self.cells[start..end] {
            *cell = Cell::default();
        }
        self.dirty = true;
    }
}
