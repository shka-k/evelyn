use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::config;

use super::App;
use super::multiplexer;

impl App {
    /// Dump the current buffer (scrollback + screen) to a temp file and
    /// hand it to an editor. Triggered by Cmd+E.
    ///
    /// Editor selection: `config.editor`, then `$VISUAL`, then `$EDITOR`.
    /// If all are empty, the file is handed to `open -t` (macOS default
    /// text-editor handler) as an external child process — this fallback
    /// always uses external mode regardless of `editor_in_pty`.
    ///
    /// When an editor command resolves, `config.editor_in_pty` picks the
    /// delivery mechanism:
    /// - **PTY** (default): write `<cmd> <path>\r` into this window's PTY
    ///   as if the user typed it at the shell prompt. Required for TUI
    ///   editors (vi/nvim/hx) and lands them in the focused Evelyn window.
    ///   Caveat: if the shell isn't sitting at a prompt, the line gets
    ///   appended to whatever's there, same as any other paste.
    /// - **External**: spawn the command as a child process. Use for GUI
    ///   editors (`code -r -w`, `cursor -r -w`). TUI editors *do not work*
    ///   here — they have no TTY in this path and attach to whichever
    ///   terminal spawned Evelyn instead of the focused window.
    pub(super) fn open_buffer_in_editor(&self) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("evelyn-buffer-{ts}.txt"));

        // If a multiplexer is running inside our PTY, Evelyn only sees
        // its rendered chrome (status bar + single visible pane). Ask
        // the multiplexer for its own scrollback instead. Fall back to
        // dumping Evelyn's buffer when nothing is detected or the dump
        // command fails.
        let dumped = self
            .pty
            .as_ref()
            .and_then(|p| p.child_pid())
            .is_some_and(|pid| multiplexer::dump_active_buffer(pid, &path));

        if !dumped {
            let text = self.term.extract_buffer_text();
            if let Err(e) = std::fs::write(&path, &text) {
                eprintln!("[evelyn] write buffer dump failed: {e}");
                return;
            }
        }

        match editor_command() {
            Some(cmd) if config().editor_in_pty => self.run_editor_in_pty(&cmd, &path),
            Some(cmd) => spawn_external(&cmd, &path),
            None => spawn_open_t(&path),
        }
    }

    fn run_editor_in_pty(&self, cmd: &str, path: &Path) {
        let Some(pty) = self.pty.as_ref() else { return };
        // Single-quote the path so any future change to the temp name
        // can't get split by the shell. The path is ASCII (temp_dir +
        // unix-millis), so no embedded apostrophe to escape.
        let line = format!("{cmd} '{}'\r", path.display());
        pty.write(line.as_bytes());
    }
}

fn spawn_external(cmd: &str, path: &Path) {
    let mut parts = cmd.split_whitespace();
    let Some(prog) = parts.next() else {
        eprintln!("[evelyn] editor command was set but empty");
        return;
    };
    let args: Vec<&str> = parts.collect();
    if let Err(e) = Command::new(prog).args(&args).arg(path).spawn() {
        eprintln!("[evelyn] spawn editor failed: {e}");
    }
}

fn spawn_open_t(path: &Path) {
    if let Err(e) = Command::new("open").arg("-t").arg(path).spawn() {
        eprintln!("[evelyn] spawn `open -t` failed: {e}");
    }
}

fn editor_command() -> Option<String> {
    if let Some(cfg) = config().editor.as_deref()
        && !cfg.trim().is_empty()
    {
        return Some(cfg.to_string());
    }
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return Some(v);
        }
    }
    None
}
