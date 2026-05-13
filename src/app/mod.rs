mod clipboard;
mod config_watch;
mod editor;
mod keyboard;
mod mouse;
mod multiplexer;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::RecommendedWatcher;
use vte::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::Key;
use winit::window::{Icon, Window, WindowAttributes, WindowId};

use crate::config::config;
use crate::pty::Pty;
use crate::render::Renderer;
use crate::term::Term;

use mouse::ClickRecord;

const WINDOW_TITLE: &str = "evelyn";
/// PNG bytes for the window icon — same source the macOS .app bundle's
/// .icns is generated from. Decoded on startup so launching via
/// `cargo run` (no bundle) still picks up the right Dock icon.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../assets/icons/evelyn.png");
const INITIAL_WINDOW_SIZE_LOGICAL: (f64, f64) = (960.0, 600.0);
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
pub(super) const IME_CANDIDATE_WIDTH_CELLS: f64 = 10.0;
/// Burst-coalescing window for PTY output. The reader thread returns from
/// `read()` per kernel chunk, which for a verbose command (build logs,
/// `cat`, `yes`) can mean thousands of wakeups per second. Holding bytes
/// for this long and parsing+rendering once per window pins the maximum
/// render rate around 1/window regardless of input volume. 8ms is below
/// one 120Hz frame, so user-visible latency stays under the screen's own
/// refresh quantum — invisible in practice, but the GPU stops being woken
/// for every keystroke echo.
const PTY_COALESCE_WINDOW: Duration = Duration::from_millis(8);

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

pub(super) struct App {
proxy: EventLoopProxy<UserEvent>,
window: Option<Arc<Window>>,
renderer: Option<Renderer>,
term: Term,
parser: Parser,
pty: Option<Pty>,
modifiers: Modifiers,
preedit: String,
    /// Caret byte offset into `preedit` reported by the IME. Falls back to
    /// the end of the string when the platform doesn't supply one, so the
    /// rendered cursor still sits past the last composed char.
preedit_cursor: usize,
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
    /// Window focus state. While `false` we don't arm blink wakeups (the
    /// cursor holds at "on" and the renderer paints it as a hollow
    /// outline so it's still locatable). Rendering itself keeps running
    /// — a TUI scrolling logs in the background should still update.
focused: bool,
    /// Bytes received from the PTY reader thread but not yet handed to
    /// the parser. Drained by `flush_pty` once `PTY_COALESCE_WINDOW`
    /// elapses since `pty_first_arrival`, so a burst of small reads
    /// becomes one parse + one redraw.
pty_pending: Vec<u8>,
    /// When the first byte in the current pending batch arrived. `None`
    /// while the buffer is empty; set on the read that transitions the
    /// buffer from empty → non-empty so the deadline is measured from
    /// the oldest queued byte, not the newest.
pty_first_arrival: Option<Instant>,
}

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
            preedit_cursor: 0,
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
            pty_pending: Vec::new(),
            pty_first_arrival: None,
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

    fn on_redraw(&mut self) {
        if let Some(r) = self.renderer.as_mut() {
            // When blink is disabled, blink_on is held at true so the cursor
            // stays visible regardless of phase state. While unfocused we
            // also pin blink_on (see `about_to_wait`) so the cursor is
            // always visible — just rendered hollow instead of solid by the
            // renderer. DECSCUSR from the foreground app wins over config.
            let blink_enabled = self
                .term
                .cursor_style
                .map(|(_, b)| b)
                .unwrap_or(config().cursor.blink);
            let blink_on = !blink_enabled || self.cursor_blink_on;
            if let Err(e) = r.render(
                &self.term,
                &self.preedit,
                self.preedit_cursor,
                blink_on,
                self.focused,
            ) {
                eprintln!("render error: {e}");
            }
            self.term.dirty = false;
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
        if self.pty_pending.is_empty() {
            self.pty_first_arrival = Some(Instant::now());
            self.pty_pending = bytes;
        } else {
            self.pty_pending.extend_from_slice(&bytes);
        }
    }

    /// Hand the accumulated PTY bytes to the parser, then drive any side
    /// effects the parser surfaced (terminal replies back to the app,
    /// OSC-52 clipboard copies, dirty bit → redraw request). Safe to call
    /// with an empty buffer; the wakeup path invokes it unconditionally
    /// once the coalesce window elapses.
    fn flush_pty(&mut self) {
        let bytes = std::mem::take(&mut self.pty_pending);
        self.pty_first_arrival = None;
        if bytes.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, &bytes);
        if !self.term.replies.is_empty() {
            let reply = std::mem::take(&mut self.term.replies);
            if let Some(p) = &self.pty {
                p.write(&reply);
            }
        }
        if let Some(text) = self.term.pending_clipboard.take() {
            self.copy_to_clipboard(&text);
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
            UserEvent::PtyExit => {
                // Drain any final bytes the shell wrote before exiting so
                // farewell messages ("logout"/"exit") still make it onto
                // the screen for the brief moment before the window closes.
                self.flush_pty();
                event_loop.exit();
            }
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
                    if s.eq_ignore_ascii_case("n") {
                        spawn_new_window();
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
                    if s.eq_ignore_ascii_case("e") {
                        // Cmd+E: dump the visible+scrollback buffer to a
                        // temp file and open it in $VISUAL/$EDITOR (or the
                        // macOS default text editor). Useful when the CRT
                        // shader displaces glyphs and makes drag-select
                        // misalign with cell content.
                        self.open_buffer_in_editor();
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

    /// Schedule the next event-loop wakeup, taking the earliest of the
    /// active deadlines: PTY-coalesce flush and cursor blink. With both
    /// idle we fall back to `ControlFlow::Wait` so a quiescent terminal
    /// stays fully asleep. PTY-coalesce defers parse + redraw until a
    /// burst of small reads has settled, so heavy output (build logs,
    /// `yes`, etc.) renders at ~1/PTY_COALESCE_WINDOW instead of once
    /// per kernel chunk.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();

        // Coalesce window elapsed → drain the accumulated bytes. `flush_pty`
        // requests a redraw if the parse marked the term dirty.
        if let Some(first) = self.pty_first_arrival
            && now.saturating_duration_since(first) >= PTY_COALESCE_WINDOW
        {
            self.flush_pty();
        }
        let pty_deadline = self.pty_first_arrival.map(|t| t + PTY_COALESCE_WINDOW);

        // DECSCUSR override (set by the foreground app via `CSI Ps SP q`)
        // wins over the configured blink flag — a steady-bar param has to
        // suppress wakeups even if the user enabled blink, and vice versa.
        // Backgrounded windows also skip blink wakeups (cursor renders as
        // a hollow outline regardless of phase).
        let cfg = config();
        let blink_enabled = self.focused
            && self
                .term
                .cursor_style
                .map(|(_, b)| b)
                .unwrap_or(cfg.cursor.blink);
        let blink_deadline = if blink_enabled {
            // 50ms floor — runaway-config guard so a tiny value can't pin a
            // CPU on busy-wakeup. Above that we trust the user.
            let interval = Duration::from_millis(cfg.cursor.blink_interval_ms.max(50));
            let next = self.cursor_blink_last_toggle + interval;
            if now >= next {
                self.cursor_blink_on = !self.cursor_blink_on;
                self.cursor_blink_last_toggle = now;
                self.request_redraw();
                Some(now + interval)
            } else {
                Some(next)
            }
        } else {
            // Hold the phase at "on" so the cursor doesn't appear in some
            // random half-state when blink re-enables or focus returns; the
            // re-focus path also resets the toggle clock.
            self.cursor_blink_on = true;
            None
        };

        match (pty_deadline, blink_deadline) {
            (Some(p), Some(b)) => event_loop.set_control_flow(ControlFlow::WaitUntil(p.min(b))),
            (Some(d), None) | (None, Some(d)) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(d))
            }
            (None, None) => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Spawn a detached copy of ourselves for Cmd+N. We re-exec the current
/// binary (so a `cargo run` instance opens another `cargo run`-built
/// binary, and the bundled .app opens the same .app), then drop the
/// child handle: the new process runs independently and we don't want
/// its lifetime tied to ours. Errors are logged and swallowed — failing
/// to open a window shouldn't crash the existing session.
fn spawn_new_window() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[evelyn] current_exe() failed: {e}");
            return;
        }
    };
    match std::process::Command::new(&exe).spawn() {
        Ok(_child) => {}
        Err(e) => eprintln!("[evelyn] spawn new window failed: {e}"),
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
