use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use vte::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{
    ElementState, Ime, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::Key;
use winit::window::{Icon, Window, WindowAttributes, WindowId};

use crate::config::{self, config};
use crate::input::encode_key;
use crate::pty::Pty;
use crate::render::Renderer;
use crate::term::{MouseProto, SelectionMode, Term};

const WINDOW_TITLE: &str = "evelyn";
/// PNG bytes for the window icon — same source the macOS .app bundle's
/// .icns is generated from. Decoded on startup so launching via
/// `cargo run` (no bundle) still picks up the right Dock icon.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/icons/evelyn.png");
const INITIAL_WINDOW_SIZE_LOGICAL: (f64, f64) = (960.0, 600.0);
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const IME_CANDIDATE_WIDTH_CELLS: f64 = 10.0;

#[derive(Debug, Clone)]
pub enum UserEvent {
    PtyData(Vec<u8>),
    PtyExit,
    /// `~/.config/evelyn/config.toml` (or its theme file) changed on disk.
    /// Coalesced via the timestamp so editor "atomic save" patterns that
    /// fire several events in a row only reload once.
    ConfigReload,
}

pub fn run() -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    term: Term,
    parser: Parser,
    pty: Option<Pty>,
    modifiers: Modifiers,
    preedit: String,
    /// Sub-line wheel delta accumulator — trackpads send fractional lines
    /// per event, and dropping the fraction would freeze slow scrolls.
    scroll_accum: f32,
    /// Last seen cursor position in physical window pixels. Updated from
    /// CursorMoved; used to attach a (col, row) to wheel events when the
    /// app has mouse tracking on. None until the cursor enters the window.
    cursor_pos: Option<PhysicalPosition<f64>>,
    /// Held to keep the file watcher alive — drop = unwatch. We don't
    /// interact with it after spawn; the callback owns the proxy clone.
    _config_watcher: Option<RecommendedWatcher>,
    /// Timestamp of the last reload, used to debounce bursty save events
    /// (e.g. editors that write to a temp file and rename).
    last_reload: Option<Instant>,
    /// Current phase of the configured cursor blink. Always `true` when
    /// blink is disabled in config — we just stop toggling it.
    cursor_blink_on: bool,
    /// When the blink phase was last flipped. `about_to_wait` schedules
    /// the next flip relative to this so the half-period is stable
    /// regardless of how often other events fire.
    cursor_blink_last_toggle: Instant,
    /// Left mouse button is currently held — drives drag-to-extend on
    /// `CursorMoved`. Released by either MouseInput::Released or window
    /// focus loss. (CursorLeft alone doesn't release: a drag past the
    /// edge keeps the button down.)
    mouse_left_held: bool,
    /// A button press was forwarded to the PTY rather than starting a
    /// native selection — set when the foreground app has mouse tracking
    /// on and Shift isn't held at press time. Cleared on Released so
    /// drag/motion forwarding tracks the same path the press took.
    mouse_forwarding: bool,
    /// Last cell forwarded to the PTY as a motion report. Used to dedup
    /// CursorMoved events, which fire per-pixel — without this we'd spam
    /// the app with redundant reports for the same cell.
    last_reported_cell: Option<(u16, u16)>,
    /// Last left-click for double / triple-click detection. Same global
    /// cell within the timeout escalates Char → Word → Line; anywhere
    /// else, or after the timeout, resets to a fresh single click.
    last_click: Option<ClickRecord>,
    /// Window focus state. While `false` we skip rendering and don't arm
    /// blink wakeups so a backgrounded window stops drawing the CPU/GPU.
    /// PTY parsing keeps running so the grid is up to date when focus
    /// returns; a single redraw on re-focus catches up the screen.
    focused: bool,
}

#[derive(Clone, Copy)]
struct ClickRecord {
    line: usize,
    col: u16,
    at: Instant,
    /// 1 = single (char), 2 = double (word), 3 = triple (line). Caps at 3.
    count: u8,
}

/// Same-cell repeat window for promoting click count. Matches macOS Finder /
/// most terminals — long enough that a deliberate double-click registers,
/// short enough that two unrelated clicks at the same spot don't merge.
const MULTI_CLICK_TIMEOUT: Duration = Duration::from_millis(500);

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            window: None,
            renderer: None,
            term: Term::new(INITIAL_COLS, INITIAL_ROWS),
            parser: Parser::new(),
            pty: None,
            modifiers: Modifiers::default(),
            preedit: String::new(),
            scroll_accum: 0.0,
            cursor_pos: None,
            _config_watcher: None,
            last_reload: None,
            cursor_blink_on: true,
            cursor_blink_last_toggle: Instant::now(),
            mouse_left_held: false,
            mouse_forwarding: false,
            last_reported_cell: None,
            last_click: None,
            focused: true,
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Re-read the renderer's grid size and propagate to Term + PTY when it
    /// changed. Called from both Resized and ScaleFactorChanged.
    fn sync_grid(&mut self) {
        let Some(r) = self.renderer.as_ref() else { return };
        let (cols, rows) = r.grid_size();
        if cols != self.term.cols || rows != self.term.rows {
            self.term.resize(cols, rows);
            if let Some(p) = &self.pty {
                p.resize(cols, rows);
            }
        }
    }

    fn update_ime_cursor_area(&self) {
        let (Some(w), Some(r)) = (self.window.as_ref(), self.renderer.as_ref()) else {
            return;
        };
        let scale = w.scale_factor();
        let pad = config().window.padding as f64;
        let x = self.term.cur_x as f64 * r.cell_width as f64 / scale + pad;
        let y = (self.term.cur_y as f64 * r.line_height as f64 + r.line_height as f64) / scale + pad;
        let cell_w = r.cell_width as f64 / scale;
        let cell_h = r.line_height as f64 / scale;
        w.set_ime_cursor_area(
            LogicalPosition::new(x, y),
            LogicalSize::new(cell_w * IME_CANDIDATE_WIDTH_CELLS, cell_h),
        );
    }

    fn on_resized(&mut self, w: u32, h: u32) {
        if let Some(r) = self.renderer.as_mut() {
            r.resize(w, h);
            self.sync_grid();
            self.request_redraw();
        }
    }

    fn on_scale_factor_changed(&mut self, scale: f64) {
        if let Some(r) = self.renderer.as_mut() {
            r.set_scale(scale as f32);
            self.sync_grid();
        }
        self.request_redraw();
    }

    fn on_keyboard_input(&mut self, event: KeyEvent, is_synthetic: bool) {
        if is_synthetic {
            return;
        }
        // While IME is composing, suppress key→PTY translation; the IME
        // delivers the result via Ime::Commit.
        if !self.preedit.is_empty() {
            return;
        }
        if let Some(bytes) = encode_key(&event, &self.modifiers, self.term.app_cursor_keys) {
            // Snap the scrollback view back to the live bottom on user
            // input — matches every other terminal: typing pulls you
            // out of history.
            if self.term.view_offset != 0 {
                self.term.reset_view();
                self.request_redraw();
            }
            // Typing past a selection makes the highlight stale (the user
            // is editing the next prompt, not interacting with the picked
            // range). Clear it so the redraw reflects the new state.
            if self.term.selection.is_some() {
                self.term.clear_selection();
                self.request_redraw();
            }
            self.poke_cursor_blink();
            if let Some(p) = &self.pty {
                p.write(&bytes);
            }
        }
    }

    /// Resolve the current cursor pixel position to a global (line, col).
    /// Returns None when the cursor isn't tracked or the renderer isn't up
    /// — callers in those states should bail rather than guess a position.
    fn cursor_to_global(&self, pos: PhysicalPosition<f64>) -> Option<(usize, u16)> {
        let r = self.renderer.as_ref()?;
        let (col1, row1) = r.pixel_to_cell(pos.x, pos.y);
        // pixel_to_cell uses xterm's 1-based convention — convert to the
        // 0-based screen coords the term grid expects.
        let col = col1.saturating_sub(1);
        let row = row1.saturating_sub(1);
        let line = self.term.screen_to_global_line(row);
        Some((line, col))
    }

    fn on_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let Some(pos) = self.cursor_pos else { return };
        let shift = self.modifiers.state().shift_key();
        let tracking = self.term.mouse_proto != MouseProto::Off;

        // xterm convention: when the foreground app has mouse tracking on,
        // forward press/release to the PTY so multiplexers (zellij/tmux) and
        // mouse-aware TUIs can do their own pane-local selection. Shift
        // overrides to fall back to our native cross-screen selection.
        if tracking && !shift {
            let button_code: u32 = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                _ => return,
            };
            let Some(r) = self.renderer.as_ref() else { return };
            let (col1, row1) = r.pixel_to_cell(pos.x, pos.y);
            match state {
                ElementState::Pressed => {
                    self.send_mouse_report(button_code, true, col1, row1);
                    if button == MouseButton::Left {
                        self.mouse_forwarding = true;
                        self.last_reported_cell = Some((col1, row1));
                    }
                }
                ElementState::Released => {
                    self.send_mouse_report(button_code, false, col1, row1);
                    if button == MouseButton::Left {
                        self.mouse_forwarding = false;
                        self.last_reported_cell = None;
                    }
                }
            }
            return;
        }

        // Native selection path — left button only.
        if button != MouseButton::Left {
            return;
        }
        let Some((line, col)) = self.cursor_to_global(pos) else { return };

        match state {
            ElementState::Pressed => {
                let now = Instant::now();
                let count = match self.last_click {
                    Some(prev)
                        if prev.line == line
                            && prev.col == col
                            && now.duration_since(prev.at) <= MULTI_CLICK_TIMEOUT =>
                    {
                        // Cap at 3 so a 4th click cycles back to single.
                        if prev.count >= 3 { 1 } else { prev.count + 1 }
                    }
                    _ => 1,
                };
                self.last_click = Some(ClickRecord { line, col, at: now, count });
                let mode = match count {
                    2 => SelectionMode::Word,
                    3 => SelectionMode::Line,
                    _ => SelectionMode::Char,
                };
                self.term.start_selection(line, col, mode);
                self.mouse_left_held = true;
                self.request_redraw();
            }
            ElementState::Released => {
                self.mouse_left_held = false;
                // A drag that produced any text → clipboard. A bare click
                // (anchor == head) is a deselect signal: clear so the
                // highlight goes away.
                if let Some(sel) = self.term.selection {
                    if sel.anchor_line == sel.head_line
                        && sel.anchor_col == sel.head_col
                        && sel.mode == SelectionMode::Char
                    {
                        self.term.clear_selection();
                        self.request_redraw();
                        return;
                    }
                }
                if let Some(text) = self.term.extract_selection_text() {
                    self.copy_to_clipboard(&text);
                }
            }
        }
    }

    fn on_mouse_drag(&mut self, position: PhysicalPosition<f64>) {
        let shift = self.modifiers.state().shift_key();
        let tracking = self.term.mouse_proto != MouseProto::Off;

        // Forward motion to the PTY when the app is tracking. Button-event
        // mode (1002) reports drags while a button is held; Any-event (1003)
        // reports motion regardless. CursorMoved fires per pixel, so dedup
        // at cell granularity to avoid spamming the app.
        if tracking && !shift {
            let Some(r) = self.renderer.as_ref() else { return };
            let (col1, row1) = r.pixel_to_cell(position.x, position.y);
            if self.last_reported_cell == Some((col1, row1)) {
                return;
            }
            match self.term.mouse_proto {
                MouseProto::Button if self.mouse_forwarding => {
                    // Drag while left button held: button 0 + motion flag (32).
                    self.send_mouse_report(32, true, col1, row1);
                    self.last_reported_cell = Some((col1, row1));
                }
                MouseProto::Any => {
                    // Any-event tracking: report motion always. Button code
                    // 0 with motion if left is held, 3 (none) + motion otherwise.
                    let base = if self.mouse_forwarding { 0 } else { 3 };
                    self.send_mouse_report(base + 32, true, col1, row1);
                    self.last_reported_cell = Some((col1, row1));
                }
                _ => {}
            }
            return;
        }

        if !self.mouse_left_held {
            return;
        }
        let Some((line, col)) = self.cursor_to_global(position) else { return };
        self.term.update_selection(line, col);
        if self.term.dirty {
            self.request_redraw();
        }
    }

    /// Encode a mouse press/release/motion event as an xterm mouse report
    /// (SGR if the app requested DECSET 1006, X10 otherwise) and write it
    /// to the PTY. `button` is the base code — caller adds the motion flag
    /// (+32) for drags and picks 0/1/2 for L/M/R or 64/65 for wheel. The
    /// shift/alt/ctrl modifier bits are OR'd in here so callers don't have
    /// to repeat the logic. `col` and `row` are 1-based xterm coords.
    fn send_mouse_report(&self, button: u32, pressed: bool, col: u16, row: u16) {
        let Some(pty) = &self.pty else { return };
        let mods = self.modifiers.state();
        let mut b = button;
        if mods.shift_key() {
            b |= 4;
        }
        if mods.alt_key() {
            b |= 8;
        }
        if mods.control_key() {
            b |= 16;
        }
        let mut bytes = Vec::new();
        if self.term.mouse_sgr {
            use std::io::Write;
            let term = if pressed { 'M' } else { 'm' };
            let _ = write!(&mut bytes, "\x1b[<{};{};{}{}", b, col, row, term);
        } else {
            // X10 releases don't carry button identity — collapse to 3.
            let raw = if pressed { b } else { (b & !0x03) | 3 };
            let cb = (32 + raw).min(255) as u8;
            let cx = (32 + col as u32).min(255) as u8;
            let cy = (32 + row as u32).min(255) as u8;
            bytes.extend_from_slice(&[0x1b, b'[', b'M', cb, cx, cy]);
        }
        pty.write(&bytes);
    }

    /// Write `text` to the system clipboard. Errors are logged and dropped
    /// — copy is a fire-and-forget UX action and we don't want a clipboard
    /// hiccup to crash the terminal mid-session.
    fn copy_to_clipboard(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        match arboard::Clipboard::new() {
            Ok(mut c) => {
                if let Err(e) = c.set_text(text.to_string()) {
                    eprintln!("[evelyn] clipboard write failed: {e}");
                }
            }
            Err(e) => eprintln!("[evelyn] clipboard open failed: {e}"),
        }
    }

    fn on_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        // Convert the wheel delta into integer line steps. winit hands us
        // either logical lines (LineDelta) or pixels (PixelDelta, common
        // on macOS trackpads); for pixels we divide by line height.
        let lines: f32 = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => {
                let lh = self
                    .renderer
                    .as_ref()
                    .map(|r| r.line_height as f64)
                    .unwrap_or(16.0);
                if lh > 0.0 {
                    (p.y / lh) as f32
                } else {
                    0.0
                }
            }
        };
        // Accumulate sub-line trackpad scrolls so a slow drag still moves.
        self.scroll_accum += lines;
        let step = self.scroll_accum.trunc() as i32;
        if step == 0 {
            return;
        }
        self.scroll_accum -= step as f32;

        // Priority order for what to do with the wheel:
        //   1. App requested mouse tracking (DECSET 1000/1002/1003) →
        //      send wheel reports. zellij/tmux/helix-with-mouse all hit
        //      this path; without it the wheel falls through to our
        //      scrollback while the app expects to handle scrolling.
        //   2. Alt screen or DECCKM (app cursor keys) → synthesize arrow
        //      keys (xterm "alternateScroll" stopgap for apps that don't
        //      enable mouse tracking but do want to consume the wheel).
        //   3. Otherwise → walk our own scrollback.
        if self.term.mouse_proto != MouseProto::Off {
            self.send_wheel_report(step);
        } else if self.term.is_alt_screen() || self.term.app_cursor_keys {
            let (byte, count) = if step > 0 {
                (b'A', step as usize)
            } else {
                (b'B', (-step) as usize)
            };
            let intro: u8 = if self.term.app_cursor_keys { b'O' } else { b'[' };
            let mut bytes = Vec::with_capacity(count * 3);
            for _ in 0..count {
                bytes.extend_from_slice(&[0x1b, intro, byte]);
            }
            if let Some(p) = &self.pty {
                p.write(&bytes);
            }
        } else {
            // Main screen → walk the scrollback view. Wheel up (+y) means
            // older content, which is +offset in our model.
            self.term.scroll_view(step);
            if self.term.dirty {
                self.request_redraw();
            }
        }
    }

    /// Encode `step` wheel ticks as xterm mouse reports and write them to
    /// the PTY. One report per tick (apps treat each as a discrete event;
    /// merging would lose granularity for slow wheels). Up == button 64,
    /// down == 65, both encoded as press-only (wheel has no release).
    /// Uses SGR (1006) when the app requested it, X10 otherwise — clamped
    /// to col/row 223 in X10 since the legacy form can't address beyond.
    fn send_wheel_report(&self, step: i32) {
        let Some(pty) = &self.pty else { return };
        let (col, row) = self
            .cursor_pos
            .and_then(|p| self.renderer.as_ref().map(|r| r.pixel_to_cell(p.x, p.y)))
            .unwrap_or((1, 1));
        let button: u32 = if step > 0 { 64 } else { 65 };
        let count = step.unsigned_abs() as usize;
        let mut bytes = Vec::new();
        for _ in 0..count {
            if self.term.mouse_sgr {
                use std::io::Write;
                let _ = write!(&mut bytes, "\x1b[<{};{};{}M", button, col, row);
            } else {
                let cb = (32 + button).min(255) as u8;
                let cx = (32 + col as u32).min(255) as u8;
                let cy = (32 + row as u32).min(255) as u8;
                bytes.extend_from_slice(&[0x1b, b'[', b'M', cb, cx, cy]);
            }
        }
        pty.write(&bytes);
    }

    fn on_ime(&mut self, ime: Ime) {
        match ime {
            Ime::Enabled | Ime::Disabled => {
                self.preedit.clear();
            }
            Ime::Preedit(text, _cursor) => {
                self.preedit = text;
                self.update_ime_cursor_area();
            }
            Ime::Commit(text) => {
                self.preedit.clear();
                if !text.is_empty() {
                    if let Some(p) = &self.pty {
                        p.write(text.as_bytes());
                    }
                }
            }
        }
        self.request_redraw();
    }

    fn on_redraw(&mut self) {
        // Backgrounded windows skip the draw entirely — the OS keeps showing
        // the last presented frame, and we'll repaint on re-focus. `dirty`
        // is intentionally left set so the catch-up redraw still happens.
        if !self.focused {
            return;
        }
        if let Some(r) = self.renderer.as_mut() {
            // When blink is disabled in config, blink_on is held at true so
            // the cursor stays visible regardless of phase state.
            let blink_on = !config().cursor.blink || self.cursor_blink_on;
            if let Err(e) = r.render(&self.term, &self.preedit, blink_on) {
                eprintln!("render error: {e}");
            }
            self.term.dirty = false;
        }
    }

    /// Snap the blink to "on" and reset the half-period timer. Called on
    /// any input/output activity so the cursor stays solid while typing
    /// and resumes blinking only after a full quiet interval — same UX
    /// xterm and friends use.
    fn poke_cursor_blink(&mut self) {
        self.cursor_blink_on = true;
        self.cursor_blink_last_toggle = Instant::now();
    }

    /// Re-read config + theme files and propagate the change to the
    /// renderer + grid. Coalesces bursty filesystem events so editors
    /// that write atomically (rename-into-place) don't trigger 3-4
    /// reloads in a row.
    fn on_config_reload(&mut self) {
        const DEBOUNCE: Duration = Duration::from_millis(50);
        if let Some(prev) = self.last_reload
            && prev.elapsed() < DEBOUNCE
        {
            return;
        }
        self.last_reload = Some(Instant::now());

        let snap = config::reload();
        // If the active theme file changed (built-in → file, or vice
        // versa, or a different filename), refresh the watcher subscription
        // so the new path is the one we get notified about.
        if snap.cfg.theme != snap.prev_cfg.theme {
            self.respawn_config_watcher();
        }
        eprintln!("[evelyn] config reloaded");
        if let Some(r) = self.renderer.as_mut() {
            let cell_changed = r.reload_from_config();
            if cell_changed {
                self.sync_grid();
            }
        }
        self.request_redraw();
    }

    /// Spawn a `notify` watcher subscribed to the config + theme paths.
    /// macOS FSEvents fires on any change in the parent dir, but we filter
    /// down to just our two files so unrelated edits in `~/.config/evelyn/`
    /// don't cause needless reloads.
    fn respawn_config_watcher(&mut self) {
        let cfg_path = config::config_file_path();
        let theme_path = config::theme_file_path();
        let watch_paths: Vec<PathBuf> = cfg_path.iter().chain(theme_path.iter()).cloned().collect();
        if watch_paths.is_empty() {
            self._config_watcher = None;
            return;
        }
        let proxy = self.proxy.clone();
        let watch_set = watch_paths.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // FSEvents on macOS coalesces by path-prefix; be strict that one
            // of the watched files is actually in the event's path list.
            if !event.paths.iter().any(|p| watch_set.iter().any(|w| p == w)) {
                return;
            }
            let _ = proxy.send_event(UserEvent::ConfigReload);
        });
        let mut watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[evelyn] config watcher init failed: {e}");
                self._config_watcher = None;
                return;
            }
        };
        // Watch the parent dir non-recursively. Watching the file directly
        // misses atomic-rename saves (the inode swaps under us), and FSEvents
        // is per-directory anyway.
        let mut watched_any = false;
        for p in watch_paths.iter().filter_map(|p| p.parent()) {
            if let Err(e) = watcher.watch(p, RecursiveMode::NonRecursive) {
                eprintln!("[evelyn] watcher.watch({}) failed: {e}", p.display());
            } else {
                watched_any = true;
            }
        }
        if watched_any {
            self._config_watcher = Some(watcher);
        } else {
            self._config_watcher = None;
        }
    }

    /// Read the system clipboard and feed it to the PTY as if typed.
    /// Newlines are normalized to CR (what Enter produces), and any
    /// embedded paste-end marker (`\e[201~`) is stripped so a hostile
    /// clipboard payload can't break out of bracketed paste and inject
    /// commands. When the foreground app has DECSET 2004 on, the bytes
    /// are wrapped in `\e[200~ … \e[201~` so it knows this is a paste.
    fn paste_from_clipboard(&mut self) {
        if self.pty.is_none() {
            return;
        }
        let mut clip = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[evelyn] clipboard open failed: {e}");
                return;
            }
        };
        let text = match clip.get_text() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[evelyn] clipboard read failed: {e}");
                return;
            }
        };
        if text.is_empty() {
            return;
        }
        let cleaned: String = text
            .replace("\x1b[201~", "")
            .replace("\r\n", "\r")
            .replace('\n', "\r");
        let mut bytes = Vec::with_capacity(cleaned.len() + 12);
        if self.term.bracketed_paste {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(cleaned.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            bytes.extend_from_slice(cleaned.as_bytes());
        }
        if self.term.view_offset != 0 {
            self.term.reset_view();
            self.request_redraw();
        }
        self.poke_cursor_blink();
        if let Some(p) = &self.pty {
            p.write(&bytes);
        }
    }

    /// Track window focus. While unfocused we skip draws and blink wakeups;
    /// on re-focus we reset the blink phase and request a redraw so any PTY
    /// activity that arrived while backgrounded gets painted in one shot.
    /// Losing focus also releases a held left button so a drag interrupted
    /// by an OS-level focus switch doesn't leave us stuck in drag state.
    fn on_focus_change(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.poke_cursor_blink();
            self.request_redraw();
        } else {
            self.mouse_left_held = false;
        }
    }

    fn on_pty_data(&mut self, bytes: Vec<u8>) {
        self.parser.advance(&mut self.term, &bytes);
        if !self.term.replies.is_empty() {
            let reply = std::mem::take(&mut self.term.replies);
            if let Some(p) = &self.pty {
                p.write(&reply);
            }
        }
        if self.term.dirty {
            self.request_redraw();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = WindowAttributes::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(
                INITIAL_WINDOW_SIZE_LOGICAL.0,
                INITIAL_WINDOW_SIZE_LOGICAL.1,
            ));
        if let Some(icon) = decode_window_icon() {
            attrs = attrs.with_window_icon(Some(icon));
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("create_window failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let renderer = match Renderer::new(window.clone()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Renderer init failed: {e}");
                event_loop.exit();
                return;
            }
        };

        let (cols, rows) = renderer.grid_size();
        self.term = Term::new(cols, rows);

        let proxy = self.proxy.clone();
        let exit_proxy = self.proxy.clone();
        let pty = match Pty::spawn(
            cols,
            rows,
            move |bytes| {
                let _ = proxy.send_event(UserEvent::PtyData(bytes));
            },
            move || {
                let _ = exit_proxy.send_event(UserEvent::PtyExit);
            },
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("PTY spawn failed: {e}");
                event_loop.exit();
                return;
            }
        };

        eprintln!("[evelyn] grid={cols}x{rows}");
        window.set_ime_allowed(true);
        window.focus_window();
        window.request_redraw();

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.pty = Some(pty);
        self.respawn_config_watcher();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(bytes) => self.on_pty_data(bytes),
            UserEvent::PtyExit => event_loop.exit(),
            UserEvent::ConfigReload => self.on_config_reload(),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.on_resized(size.width, size.height),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.on_scale_factor_changed(scale_factor);
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m;
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if !is_synthetic
                    && event.state.is_pressed()
                    && self.modifiers.state().super_key()
                    && let Key::Character(s) = &event.logical_key
                {
                    if s.eq_ignore_ascii_case("w") {
                        event_loop.exit();
                        return;
                    }
                    if s.eq_ignore_ascii_case("r") {
                        self.on_config_reload();
                        return;
                    }
                    if s.eq_ignore_ascii_case("v") {
                        self.paste_from_clipboard();
                        return;
                    }
                    if s.eq_ignore_ascii_case("c") {
                        // Cmd+C with a selection copies. Without a
                        // selection we swallow the keypress rather than
                        // forwarding it as ESC+c, which is what the alt
                        // fold of super_key would otherwise produce —
                        // there is no useful "Cmd+C → PTY" mapping.
                        if let Some(text) = self.term.extract_selection_text() {
                            self.copy_to_clipboard(&text);
                        }
                        return;
                    }
                }
                self.on_keyboard_input(event, is_synthetic);
            }
            WindowEvent::MouseWheel { delta, .. } => self.on_mouse_wheel(delta),
            WindowEvent::MouseInput { state, button, .. } => self.on_mouse_input(state, button),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some(position);
                self.on_mouse_drag(position);
            }
            WindowEvent::CursorLeft { .. } => self.cursor_pos = None,
            WindowEvent::Focused(focused) => self.on_focus_change(focused),
            WindowEvent::Ime(ime) => self.on_ime(ime),
            WindowEvent::RedrawRequested => self.on_redraw(),
            _ => {}
        }
    }

    /// Schedule the next event-loop wakeup for cursor blink. With blink
    /// disabled we drop straight back to `Wait` so an idle terminal stays
    /// fully idle. With blink enabled we toggle the phase whenever the
    /// half-period has elapsed and arm a `WaitUntil` for the next flip,
    /// so the wakeup rate matches the configured rate exactly.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let cfg = config();
        // Backgrounded → no blink wakeups. Hold the phase at "on" so the
        // cursor doesn't appear in some random half-state when focus
        // returns; the re-focus path resets the toggle clock.
        if !self.focused {
            self.cursor_blink_on = true;
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        if !cfg.cursor.blink {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        // 50ms floor — runaway-config guard so a tiny value can't pin a
        // CPU on busy-wakeup. Above that we trust the user.
        let interval = Duration::from_millis(cfg.cursor.blink_interval_ms.max(50));
        let now = Instant::now();
        let next = self.cursor_blink_last_toggle + interval;
        if now >= next {
            self.cursor_blink_on = !self.cursor_blink_on;
            self.cursor_blink_last_toggle = now;
            self.request_redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + interval));
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(next));
        }
    }
}

/// Decode the bundled PNG into RGBA and hand it to winit. Returns `None`
/// on failure — the window just runs without a custom icon then.
fn decode_window_icon() -> Option<Icon> {
    // png 0.18+ wants `BufRead + Seek`; wrap the embedded slice in Cursor.
    let decoder = png::Decoder::new(std::io::Cursor::new(WINDOW_ICON_PNG));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let used = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => used.to_vec(),
        png::ColorType::Rgb => used
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        _ => return None,
    };
    Icon::from_rgba(rgba, w, h).ok()
}
