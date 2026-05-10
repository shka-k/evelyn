use vte::{Params, Perform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const DEFAULT_FG: Rgb = Rgb(0xd0, 0xd0, 0xd0);
pub const DEFAULT_BG: Rgb = Rgb(0x10, 0x10, 0x14);

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
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

    fn put_char(&mut self, c: char) {
        if self.cur_x >= self.cols {
            self.cur_x = 0;
            self.line_feed();
        }
        let i = self.idx(self.cur_x, self.cur_y);
        self.cells[i] = Cell {
            ch: c,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
        };
        self.cur_x += 1;
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

    fn sgr(&mut self, params: &Params) {
        let flat: Vec<u16> = params.iter().flatten().copied().collect();
        if flat.is_empty() {
            self.fg = DEFAULT_FG;
            self.bg = DEFAULT_BG;
            self.bold = false;
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            let p = flat[i];
            match p {
                0 => {
                    self.fg = DEFAULT_FG;
                    self.bg = DEFAULT_BG;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                30..=37 => self.fg = ansi_basic((p - 30) as u8, false),
                90..=97 => self.fg = ansi_basic((p - 90) as u8, true),
                40..=47 => self.bg = ansi_basic((p - 40) as u8, false),
                100..=107 => self.bg = ansi_basic((p - 100) as u8, true),
                39 => self.fg = DEFAULT_FG,
                49 => self.bg = DEFAULT_BG,
                38 | 48 => {
                    // 38;5;n  or 38;2;r;g;b
                    if let Some(&kind) = flat.get(i + 1) {
                        if kind == 5 {
                            if let Some(&n) = flat.get(i + 2) {
                                let c = ansi_256(n as u8);
                                if p == 38 { self.fg = c; } else { self.bg = c; }
                                i += 2;
                            }
                        } else if kind == 2 {
                            if let (Some(&r), Some(&g), Some(&b)) =
                                (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4))
                            {
                                let c = Rgb(r as u8, g as u8, b as u8);
                                if p == 38 { self.fg = c; } else { self.bg = c; }
                                i += 4;
                            }
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn first_param(params: &Params, default: u16) -> u16 {
    params
        .iter()
        .next()
        .and_then(|p| p.first().copied())
        .map(|v| if v == 0 { default } else { v })
        .unwrap_or(default)
}

fn ansi_basic(n: u8, bright: bool) -> Rgb {
    // standard xterm-ish palette
    const BASE: [Rgb; 8] = [
        Rgb(0x00, 0x00, 0x00),
        Rgb(0xcd, 0x31, 0x31),
        Rgb(0x0d, 0xbc, 0x79),
        Rgb(0xe5, 0xe5, 0x10),
        Rgb(0x24, 0x72, 0xc8),
        Rgb(0xbc, 0x3f, 0xbc),
        Rgb(0x11, 0xa8, 0xcd),
        Rgb(0xe5, 0xe5, 0xe5),
    ];
    const BRIGHT: [Rgb; 8] = [
        Rgb(0x66, 0x66, 0x66),
        Rgb(0xf1, 0x4c, 0x4c),
        Rgb(0x23, 0xd1, 0x8b),
        Rgb(0xf5, 0xf5, 0x43),
        Rgb(0x3b, 0x8e, 0xea),
        Rgb(0xd6, 0x70, 0xd6),
        Rgb(0x29, 0xb8, 0xdb),
        Rgb(0xff, 0xff, 0xff),
    ];
    let table = if bright { &BRIGHT } else { &BASE };
    table[(n & 7) as usize]
}

fn ansi_256(n: u8) -> Rgb {
    if n < 16 {
        ansi_basic(n & 7, n >= 8)
    } else if n < 232 {
        let i = n - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        Rgb(scale(r), scale(g), scale(b))
    } else {
        let v = 8 + (n - 232) * 10;
        Rgb(v, v, v)
    }
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.carriage_return(),
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            0x08 => self.backspace(),
            b'\t' => self.tab(),
            0x07 => {} // bell
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            'c' => {
                // Device Attributes. Reply so apps like fish don't time out.
                if intermediates.is_empty() {
                    // Primary DA — VT102 with Advanced Video Option.
                    self.replies.extend_from_slice(b"\x1b[?6c");
                } else if intermediates == b">" {
                    // Secondary DA — pose as xterm patch level 0.
                    self.replies.extend_from_slice(b"\x1b[>0;0;0c");
                }
            }
            'n' => {
                // Device Status Report.
                let mode = first_param(params, 0);
                match mode {
                    5 => self.replies.extend_from_slice(b"\x1b[0n"),
                    6 => {
                        let s = format!("\x1b[{};{}R", self.cur_y + 1, self.cur_x + 1);
                        self.replies.extend_from_slice(s.as_bytes());
                    }
                    _ => {}
                }
            }
            'm' => self.sgr(params),
            'H' | 'f' => {
                let mut it = params.iter();
                let row = it.next().and_then(|p| p.first().copied()).unwrap_or(1).max(1);
                let col = it.next().and_then(|p| p.first().copied()).unwrap_or(1).max(1);
                self.cur_y = (row - 1).min(self.rows.saturating_sub(1));
                self.cur_x = (col - 1).min(self.cols.saturating_sub(1));
                self.dirty = true;
            }
            'A' => {
                let n = first_param(params, 1);
                self.cur_y = self.cur_y.saturating_sub(n);
                self.dirty = true;
            }
            'B' => {
                let n = first_param(params, 1);
                self.cur_y = (self.cur_y + n).min(self.rows.saturating_sub(1));
                self.dirty = true;
            }
            'C' => {
                let n = first_param(params, 1);
                self.cur_x = (self.cur_x + n).min(self.cols.saturating_sub(1));
                self.dirty = true;
            }
            'D' => {
                let n = first_param(params, 1);
                self.cur_x = self.cur_x.saturating_sub(n);
                self.dirty = true;
            }
            'G' => {
                let n = first_param(params, 1);
                self.cur_x = (n - 1).min(self.cols.saturating_sub(1));
                self.dirty = true;
            }
            'd' => {
                let n = first_param(params, 1);
                self.cur_y = (n - 1).min(self.rows.saturating_sub(1));
                self.dirty = true;
            }
            'J' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.erase_in_display(mode);
            }
            'K' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.erase_in_line(mode);
            }
            _ => {}
        }
    }
}
