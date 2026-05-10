mod parser;

use std::collections::VecDeque;

use crate::color::{Color, Rgb};
use crate::width::is_wide;

/// Max scrollback rows kept in history. Each row is `cols * sizeof(Cell)`,
/// so 5000 * ~256 cols * ~24B ≈ 30MB worst case — fine.
const HISTORY_CAP: usize = 5000;

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    /// `'\0'` marks the right half of a wide character (continuation).
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    /// `\\e[7m` reverse video. Renderers swap fg/bg on output — this is
    /// how TUIs like yazi paint selection highlights without setting an
    /// explicit bg color.
    pub reverse: bool,
    /// Set on the LEFT half of a wide character. Its right neighbour is a
    /// continuation cell with `ch == '\0'`.
    pub wide: bool,
}

impl Cell {
    /// Effective foreground / background after reverse-video is applied,
    /// resolved against the live theme. The renderer should use these for
    /// actual drawing, never `fg`/`bg` directly, otherwise reverse cells
    /// lose their highlight and `Color::Default` skips the theme.
    pub fn fg_eff(&self) -> Rgb {
        if self.reverse {
            self.bg.resolve_bg()
        } else {
            self.fg.resolve_fg()
        }
    }
    pub fn bg_eff(&self) -> Rgb {
        if self.reverse {
            self.fg.resolve_fg()
        } else {
            self.bg.resolve_bg()
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            reverse: false,
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
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub reverse: bool,
    pub dirty: bool,
    /// `\\e[?25 h/l` — apps like helix or less hide the cursor while
    /// rendering. The renderer skips the block when this is false.
    pub cursor_visible: bool,
    /// DECAWM (`\\e[?7 h/l`). When false, the cursor stops at the right
    /// edge instead of wrapping; subsequent characters overwrite the last
    /// column. zellij and similar TUIs disable this while drawing borders.
    pub auto_wrap: bool,
    /// DECCKM (`\\e[?1 h/l`). When set, cursor / arrow keys must be sent
    /// as SS3 (`ESC O X`) instead of CSI (`ESC [ X`). vi/vim/helix/less
    /// all enable this on entry — without honoring it, arrow keys arrive
    /// as the wrong sequence and the app silently ignores them.
    pub app_cursor_keys: bool,
    /// xterm mouse tracking mode set by the running app via DECSET 1000 /
    /// 1002 / 1003. Anything other than `Off` means the app wants to
    /// receive mouse events itself — wheel events in particular get
    /// reported instead of driving our scrollback. Zellij turns this on
    /// (typically `Button` + SGR encoding); without it our scroll handler
    /// would walk scrollback while zellij is the foreground app.
    pub mouse_proto: MouseProto,
    /// DECSET 1006 — SGR-form mouse reporting. When on, reports use
    /// `\\e[<b;x;yM/m` (decimal, terminator distinguishes press/release).
    /// When off, the legacy X10 form `\\e[Mbxy` (one byte each, +32) is
    /// used and gets capped at column/row 223. Modern TUIs request 1006.
    pub mouse_sgr: bool,
    /// DECSET 2004 — bracketed paste. When set, pasted text must be
    /// wrapped in `\e[200~ … \e[201~` so the app can distinguish it
    /// from typed input (shells/editors use this to disable autoindent,
    /// suppress key bindings, etc. mid-paste).
    pub bracketed_paste: bool,
    /// VT100 "last column" / deferred wrap. Set after a print lands in the
    /// rightmost column with DECAWM on; the wrap is held until the next
    /// print, and any cursor motion (CR/LF/BS/CUP/…) cancels it. Without
    /// this, drawing a box-corner glyph at (rows-1, cols-1) would scroll
    /// the whole screen — zellij/vim/tmux all rely on the deferral.
    pending_wrap: bool,
    /// Bytes the terminal needs to send back to the host program (DA, DSR, …).
    /// Drained by the application after each parser advance.
    pub replies: Vec<u8>,
    /// DECSTBM scroll region — top/bottom row indices, inclusive, in [0, rows).
    /// Default covers the whole screen. Apps like zellij set per-pane regions
    /// so an LF at the pane bottom only scrolls that band, not the whole grid.
    scroll_top: u16,
    scroll_bot: u16,
    /// `\\e7` / CSI s. zellij saves cursor + attrs, draws its UI, then
    /// restores so the inner shell's next byte lands where it expects.
    saved_cursor: Option<SavedCursor>,
    /// Snapshot of the main screen kept while we're in alt screen
    /// (`\\e[?1049h`). On exit (`\\e[?1049l`) we restore it.
    saved: Option<SavedScreen>,
    /// Scrollback buffer — rows that have rolled off the top of the main
    /// screen. Each entry is exactly `cols` cells wide; `cols` mismatches
    /// (window resize) clear the buffer rather than reflowing.
    history: VecDeque<Vec<Cell>>,
    /// Lines the viewport is shifted up from the live bottom, in [0, history.len()].
    /// 0 means we're showing the live screen.
    pub view_offset: usize,
}

/// xterm mouse tracking levels. Each level subsumes the previous, but for
/// our purposes the only thing that matters is "off vs. anything" — the
/// wheel-report path doesn't depend on which level is active.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseProto {
    /// No mouse reporting requested.
    Off,
    /// DECSET 1000 — press / release only.
    Press,
    /// DECSET 1002 — press / release + drag while a button is held.
    Button,
    /// DECSET 1003 — every motion event.
    Any,
}

#[derive(Clone, Copy)]
struct SavedCursor {
    cur_x: u16,
    cur_y: u16,
    fg: Color,
    bg: Color,
    bold: bool,
    reverse: bool,
}

struct SavedScreen {
    cells: Vec<Cell>,
    cur_x: u16,
    cur_y: u16,
    fg: Color,
    bg: Color,
    bold: bool,
    reverse: bool,
}

impl Term {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); (cols as usize) * (rows as usize)],
            cur_x: 0,
            cur_y: 0,
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            reverse: false,
            dirty: true,
            cursor_visible: true,
            auto_wrap: true,
            app_cursor_keys: false,
            mouse_proto: MouseProto::Off,
            mouse_sgr: false,
            bracketed_paste: false,
            pending_wrap: false,
            replies: Vec::new(),
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            saved_cursor: None,
            saved: None,
            history: VecDeque::new(),
            view_offset: 0,
        }
    }

    pub fn is_alt_screen(&self) -> bool {
        self.saved.is_some()
    }

    /// Cell at screen position `(x, y)` accounting for the scrollback view.
    /// When `view_offset > 0` the top of the screen is sourced from history.
    pub fn cell_at(&self, x: u16, y: u16) -> &Cell {
        let cols = self.cols as usize;
        let x = x as usize;
        let y = y as usize;
        let h = self.history.len();
        let view_top = h.saturating_sub(self.view_offset);
        let global_y = view_top + y;
        if global_y < h {
            &self.history[global_y][x]
        } else {
            let local_y = global_y - h;
            &self.cells[local_y * cols + x]
        }
    }

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

    pub fn resize(&mut self, cols: u16, rows: u16) {
        // History rows have a fixed cols width — drop them on a width
        // change rather than reflowing. Height changes leave history alone.
        if cols != self.cols {
            self.history.clear();
            self.view_offset = 0;
        }
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![Cell::default(); (cols as usize) * (rows as usize)];
        self.cur_x = self.cur_x.min(cols.saturating_sub(1));
        self.cur_y = self.cur_y.min(rows.saturating_sub(1));
        // Reset scroll region to cover the new size — the app will reissue
        // CSI r if it cares (zellij does this on its SIGWINCH handler).
        self.scroll_top = 0;
        self.scroll_bot = rows.saturating_sub(1);
        self.saved_cursor = None;
        self.pending_wrap = false;
        self.dirty = true;
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        (y as usize) * (self.cols as usize) + (x as usize)
    }

    fn reset_attrs(&mut self) {
        self.fg = Color::Default;
        self.bg = Color::Default;
        self.bold = false;
        self.reverse = false;
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
        self.dirty = true;
    }

    fn line_feed(&mut self) {
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

    /// Scroll the DECSTBM region up by `n` lines: rows [top, bot] shift
    /// upward, the bottom `n` rows are blanked with current SGR bg. Used
    /// for LF at the region bottom and for CSI S.
    fn scroll_up_in_region(&mut self, n: u16) {
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
    fn scroll_down_in_region(&mut self, n: u16) {
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
        self.dirty = true;
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
        self.dirty = true;
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
            reverse: self.reverse,
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
