mod cell;
mod charset;
mod edit;
mod parser;
mod screen;
mod selection;

use std::collections::VecDeque;

use crate::color::Color;

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
            charset_g0: Charset::Ascii,
            charset_g1: Charset::Ascii,
            active_charset: 0,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        // History rows have a fixed cols width — drop them on a width
        // change rather than reflowing. Height changes leave history alone.
        if cols != self.cols {
            self.history.clear();
            self.view_offset = 0;
        }
        // Selection coordinates are valid against the old grid; the safest
        // thing on any resize is to drop it so we don't highlight cells
        // that no longer correspond to the captured content.
        self.selection = None;
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
