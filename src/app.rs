use std::sync::Arc;

use anyhow::Result;
use vte::Parser;
use winit::application::ApplicationHandler;
use winit::event::{Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::input::encode_key;
use crate::pty::Pty;
use crate::render::Renderer;
use crate::term::Term;

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
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            window: None,
            renderer: None,
            term: Term::new(80, 24),
            parser: Parser::new(),
            pty: None,
            modifiers: Modifiers::default(),
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = WindowAttributes::default()
            .with_title("evelyn")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
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
        window.focus_window();
        window.request_redraw();

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.pty = Some(pty);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(bytes) => {
                self.parser.advance(&mut self.term, &bytes);
                if !self.term.replies.is_empty() {
                    let reply = std::mem::take(&mut self.term.replies);
                    if let Some(p) = &self.pty {
                        p.write(&reply);
                    }
                }
                if self.term.dirty {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
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
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                    let (cols, rows) = r.grid_size();
                    if cols != self.term.cols || rows != self.term.rows {
                        self.term.resize(cols, rows);
                        if let Some(p) = &self.pty {
                            p.resize(cols, rows);
                        }
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(r) = self.renderer.as_mut() {
                    r.set_scale(scale_factor as f32);
                    let (cols, rows) = r.grid_size();
                    if cols != self.term.cols || rows != self.term.rows {
                        self.term.resize(cols, rows);
                        if let Some(p) = &self.pty {
                            p.resize(cols, rows);
                        }
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m;
            }
            WindowEvent::KeyboardInput {
                event, is_synthetic, ..
            } => {
                if is_synthetic {
                    return;
                }
                if let Some(bytes) = encode_key(&event, &self.modifiers) {
                    if let Some(p) = &self.pty {
                        p.write(&bytes);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    if let Err(e) = r.render(&self.term) {
                        eprintln!("render error: {e}");
                    }
                    self.term.dirty = false;
                }
            }
            _ => {}
        }
    }
}
