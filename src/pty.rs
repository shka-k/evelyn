use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use crate::config::config;

const TERM_ENV: &str = "xterm-256color";

pub struct Pty {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Pty {
    pub fn spawn<F, G>(cols: u16, rows: u16, mut on_data: F, on_exit: G) -> Result<Self>
    where
        F: FnMut(Vec<u8>) + Send + 'static,
        G: FnOnce() + Send + 'static,
    {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(config().resolved_shell());
        if let Ok(home) = std::env::var("HOME") {
            cmd.cwd(home);
        }
        cmd.env("TERM", TERM_ENV);
        // Apps like helix/crossterm gate 24-bit color on COLORTERM rather
        // than terminfo. Without this they fall back to the 16/256-color
        // palette and themes render with the wrong colors.
        cmd.env("COLORTERM", "truecolor");
        // GUI apps launched via Finder/Dock/launchd inherit an empty LANG
        // because launchd doesn't source shell rc files. Node-based TUIs
        // like gtop / blessed-contrib check this and downgrade Unicode
        // output to ASCII (braille sparklines render as literal `?`).
        // Seed a full UTF-8 locale only when nothing was inherited so we
        // don't override an explicit user preference. Use full
        // `en_US.UTF-8` form for both — the bare `UTF-8` value macOS
        // sometimes injects isn't recognized by glibc-style parsers.
        let have_locale = std::env::var_os("LANG").is_some()
            || std::env::var_os("LC_ALL").is_some()
            || std::env::var_os("LC_CTYPE").is_some();
        if !have_locale {
            cmd.env("LANG", "en_US.UTF-8");
            cmd.env("LC_CTYPE", "en_US.UTF-8");
        }

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => on_data(buf[..n].to_vec()),
                    Err(_) => break,
                }
            }
            on_exit();
        });

        let writer = pair.master.take_writer()?;

        Ok(Self {
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            _child: child,
        })
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}
