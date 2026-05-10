use std::sync::Arc;

use anyhow::Result;
use vte::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{Ime, KeyEvent, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Icon, Window, WindowAttributes, WindowId};

use crate::config::CONFIG;
use crate::input::encode_key;
use crate::pty::Pty;
use crate::render::Renderer;
use crate::term::Term;

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
        let pad = CONFIG.window.padding as f64;
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

    fn on_keyboard_input(&self, event: KeyEvent, is_synthetic: bool) {
        if is_synthetic {
            return;
        }
        // While IME is composing, suppress key→PTY translation; the IME
        // delivers the result via Ime::Commit.
        if !self.preedit.is_empty() {
            return;
        }
        if let Some(bytes) = encode_key(&event, &self.modifiers) {
            if let Some(p) = &self.pty {
                p.write(&bytes);
            }
        }
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
        if let Some(r) = self.renderer.as_mut() {
            if let Err(e) = r.render(&self.term, &self.preedit) {
                eprintln!("render error: {e}");
            }
            self.term.dirty = false;
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
        let pty = match Pty::spawn(cols, rows, move |bytes| {
            let _ = proxy.send_event(UserEvent::PtyData(bytes));
        }) {
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
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(bytes) => self.on_pty_data(bytes),
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
            } => self.on_keyboard_input(event, is_synthetic),
            WindowEvent::Ime(ime) => self.on_ime(ime),
            WindowEvent::RedrawRequested => self.on_redraw(),
            _ => {}
        }
    }
}

/// Decode the bundled PNG into RGBA and hand it to winit. Returns `None`
/// on failure — the window just runs without a custom icon then.
fn decode_window_icon() -> Option<Icon> {
    let decoder = png::Decoder::new(WINDOW_ICON_PNG);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        _ => return None,
    };
    Icon::from_rgba(rgba, w, h).ok()
}
