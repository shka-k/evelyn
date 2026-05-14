mod cell;
mod charset;
mod edit;
mod parser;
mod screen;
mod selection;

use std::collections::VecDeque;

use crate::color::Color;
use crate::config::CursorShape;

pub use cell::Cell;
pub use selection::{Selection, SelectionMode};

use charset::Charset;
use screen::{SavedCursor, SavedScreen};

/// Max scrollback rows kept in history. Each row is `cols * sizeof(Cell)`,
/// so 5000 * ~256 cols * ~24B ≈ 30MB worst case — fine.
const HISTORY_CAP: usize = 5000;

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
    /// Per-screen-row dirty flags, indexed by screen row (0..rows). Set by
    /// every cell-content mutation; consumed by the renderer's per-row
    /// caches so unchanged rows skip rebuild work. Cursor / overlay
    /// changes leave this alone — those are rendered fresh each frame
    /// from `cur_x`/`cur_y` regardless. Always exactly `rows` long; kept
    /// in sync by `resize`.
    pub dirty_rows: Vec<bool>,
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
    /// Clipboard payload the running app asked us to set via OSC 52
    /// (`\e]52;<sel>;<base64>\e\\`). Drained by the application after each
    /// parser advance and pushed to the system clipboard. zellij/tmux/vim
    /// all use this for their own copy actions when running under a
    /// terminal that supports it — without honoring it, a copy inside
    /// zellij silently no-ops.
    pub pending_clipboard: Option<String>,
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
    /// Total scrollback rows that have rolled off the front of `history` since
    /// startup. Combined with positions in `history` and the screen, every
    /// line in the buffer has a stable global index `history_dropped + i` —
    /// rolling history doesn't shift the indices of younger lines, so a
    /// selection captured before a burst of output still highlights the same
    /// content. Monotonic; never reset.
history_dropped: usize,
    /// Active mouse / keyboard selection, if any. Anchored in global line
    /// coordinates so it survives history rolling and scrollback navigation.
    pub selection: Option<Selection>,
    /// DECSCUSR (`CSI Ps SP q`) override. `None` means fall back to the
    /// user's config; `Some((shape, blink))` is set by the foreground app
    /// (helix/vim/fish all swap shape on mode change). Param 0 clears it.
    pub cursor_style: Option<(CursorShape, bool)>,
    /// Designated character sets for G0 / G1. tmux/vim/less switch G0 to
    /// DEC Special Graphics with `ESC ( 0` to draw box borders, then back
    /// with `ESC ( B`. Without honoring this the border characters render
    /// as raw `qqq…` / `xxx…` ASCII.
charset_g0: Charset,
charset_g1: Charset,
    /// Which of G0/G1 is currently mapped to GL — flipped by SI (0x0F → G0)
    /// and SO (0x0E → G1). Most apps stay on G0 and toggle the *designation*
    /// instead, but ncurses drives borders via SO/SI on terminals where
    /// that's cheaper.
active_charset: u8,
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
            dirty_rows: vec![true; rows as usize],
            cursor_visible: true,
            auto_wrap: true,
            app_cursor_keys: false,
            mouse_proto: MouseProto::Off,
            mouse_sgr: false,
            bracketed_paste: false,
            pending_wrap: false,
            replies: Vec::new(),
            pending_clipboard: None,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            saved_cursor: None,
            saved: None,
            history: VecDeque::new(),
            view_offset: 0,
            history_dropped: 0,
            selection: None,
            cursor_style: None,
            charset_g0: Charset::Ascii,
            charset_g1: Charset::Ascii,
            active_charset: 0,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let old_cols = self.cols as usize;
        let old_rows = self.rows as usize;
        let new_cols = cols as usize;
        let new_rows = rows as usize;

        // Reflow scrollback to the new width — truncate or pad with blanks.
        // Keeps history alive across width changes; a hard clear here was
        // the most visible part of the "resize wipes my scrollback" bug.
        if new_cols != old_cols {
            for row in self.history.iter_mut() {
                row.resize(new_cols, Cell::default());
            }
        }

        // Bottom-align the live grid into the new buffer so the prompt stays
        // anchored at the bottom edge. When shrinking rows, the displaced top
        // rows of the main screen become scrollback; alt-screen scroll-off
        // is dropped (history belongs to the shell underneath).
        let mut new_cells = vec![Cell::default(); new_cols * new_rows];
        let mut cursor_shift: i32 = 0;
        if old_cols > 0 && old_rows > 0 {
            let copy_rows = old_rows.min(new_rows);
            let src_row_start = old_rows - copy_rows;
            let dst_row_start = new_rows - copy_rows;
            let copy_cols = old_cols.min(new_cols);
            for i in 0..copy_rows {
                let src = (src_row_start + i) * old_cols;
                let dst = (dst_row_start + i) * new_cols;
                new_cells[dst..dst + copy_cols]
                    .copy_from_slice(&self.cells[src..src + copy_cols]);
            }
            if self.saved.is_none() && src_row_start > 0 {
                for i in 0..src_row_start {
                    let src = i * old_cols;
                    let mut row: Vec<Cell> = self.cells[src..src + old_cols].to_vec();
                    row.resize(new_cols, Cell::default());
                    if self.history.len() == HISTORY_CAP {
                        self.history.pop_front();
                        self.history_dropped += 1;
                    }
                    self.history.push_back(row);
                }
            }
            cursor_shift = dst_row_start as i32 - src_row_start as i32;
        }
        self.cells = new_cells;

        // Selection is anchored in global line coords (history-rolling-safe),
        // but the column space shifts on width changes. Keep across pure
        // height changes; drop on width changes.
        if new_cols != old_cols {
            self.selection = None;
        }
        self.cols = cols;
        self.rows = rows;
        self.dirty_rows = vec![true; new_rows];
        self.cur_x = self.cur_x.min(cols.saturating_sub(1));
        let last_row = rows.saturating_sub(1) as i32;
        self.cur_y = (self.cur_y as i32 + cursor_shift).clamp(0, last_row) as u16;
        if self.view_offset > self.history.len() {
            self.view_offset = self.history.len();
        }
        // Reset scroll region to cover the new size — the app will reissue
        // CSI r if it cares (zellij does this on its SIGWINCH handler).
        self.scroll_top = 0;
        self.scroll_bot = rows.saturating_sub(1);
        self.saved_cursor = None;
        self.pending_wrap = false;
        self.dirty = true;
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

    /// Global line index of the topmost visible row. Lines in `history` run
    /// from `history_dropped` to `history_dropped + history.len() - 1`; live
    /// screen rows continue from there. Subtracting `view_offset` walks the
    /// scrollback view back through history.
    pub fn screen_top_line(&self) -> usize {
        let bottom = self.history_dropped + self.history.len();
        bottom.saturating_sub(self.view_offset)
    }

    /// Global line index for a screen-relative row, accounting for the
    /// active scrollback view. Used when the App turns a mouse position
    /// into a selection anchor.
    pub fn screen_to_global_line(&self, screen_y: u16) -> usize {
        self.screen_top_line() + screen_y as usize
    }

    /// Look up a cell by its global (line, col). `None` if the line has
    /// rolled off the front of history or the column is out of range.
    pub fn cell_at_global(&self, line: usize, col: u16) -> Option<Cell> {
        if line < self.history_dropped {
            return None;
        }
        let local = line - self.history_dropped;
        let cols = self.cols as usize;
        let c = col as usize;
        if c >= cols {
            return None;
        }
        if local < self.history.len() {
            return self.history[local].get(c).copied();
        }
        let screen_y = local - self.history.len();
        if screen_y >= self.rows as usize {
            return None;
        }
        self.cells.get(screen_y * cols + c).copied()
    }

fn idx(&self, x: u16, y: u16) -> usize {
        (y as usize) * (self.cols as usize) + (x as usize)
    }

    /// Mark one row as needing a bg/quad rebuild. Also flips the coarse
    /// `dirty` bit so the app knows to redraw at all. Out-of-bounds rows
    /// (can happen during a mid-resize parse on transient state) are
    /// ignored.
    pub(super) fn mark_row(&mut self, y: u16) {
        if let Some(slot) = self.dirty_rows.get_mut(y as usize) {
            *slot = true;
        }
        self.dirty = true;
    }

    /// Mark a contiguous range of rows `[top, bot]` inclusive. No-op if
    /// the range is empty (top > bot) or entirely past the grid.
    pub(super) fn mark_rows(&mut self, top: u16, bot: u16) {
        let len = self.dirty_rows.len();
        if len == 0 || top > bot {
            return;
        }
        let start = (top as usize).min(len);
        let end = (bot as usize).min(len - 1);
        if start > end {
            return;
        }
        for slot in &mut self.dirty_rows[start..=end] {
            *slot = true;
        }
        self.dirty = true;
    }

    /// Mark every visible row dirty. Used by alt-screen swaps, history
    /// scrolling (view_offset change), and any other operation whose
    /// effect on the rendered grid spans the full screen.
    pub(super) fn mark_all_rows(&mut self) {
        for slot in &mut self.dirty_rows {
            *slot = true;
        }
        self.dirty = true;
    }

    /// Called by the renderer-driver after a successful render to mark
    /// the current frame as consumed. Clears both the coarse `dirty`
    /// flag and the per-row bits.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
        for slot in &mut self.dirty_rows {
            *slot = false;
        }
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
}
