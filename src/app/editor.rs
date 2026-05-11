use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::config;

use super::App;

impl App {
    /// Dump the current buffer (scrollback + screen) to a temp file and
    /// hand it to an editor. Triggered by Cmd+E.
    ///
    /// Two modes, switched by `config.editor_in_pty`:
    /// - **External** (default): spawn the command as a child process.
    ///   Good for GUI editors (`code -r -w`, `cursor -r -w`). TUI editors
    ///   like vi/nvim *do not work here* — they have no TTY in this path
    ///   and will attach to whichever terminal spawned Evelyn rather than
    ///   the focused window.
    /// - **PTY**: write `<cmd> <path>\r` into this window's PTY as if the
    ///   user typed it at the shell prompt. Required for TUI editors and
    ///   makes the editor land in the focused Evelyn window. Caveat: if
    ///   the shell isn't sitting at a prompt the line gets appended to
    ///   whatever's there, same as any other paste.
    ///
    /// Editor selection: `config.editor`, then `$VISUAL`, then `$EDITOR`,
    /// then `open -t` (external mode only — PTY mode requires an explicit
    /// command).
    pub(super) fn open_buffer_in_editor(&self) {
        let text = self.term.extract_buffer_text();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("evelyn-buffer-{ts}.txt"));
        if let Err(e) = std::fs::write(&path, &text) {
            eprintln!("[evelyn] write buffer dump failed: {e}");
            return;
        }

        let cfg = config();
        if cfg.editor_in_pty {
            self.run_editor_in_pty(&path);
            return;
        }

        let spawn_result = if let Some(cmd) = editor_command() {
            let mut parts = cmd.split_whitespace();
            let Some(prog) = parts.next() else {
                eprintln!("[evelyn] editor command was set but empty");
                return;
            };
            let args: Vec<&str> = parts.collect();
            Command::new(prog).args(&args).arg(&path).spawn()
        } else {
            Command::new("open").arg("-t").arg(&path).spawn()
        };
        if let Err(e) = spawn_result {
            eprintln!("[evelyn] spawn editor failed: {e}");
        }
    }

    fn run_editor_in_pty(&self, path: &std::path::Path) {
        let Some(pty) = self.pty.as_ref() else { return };
        let Some(cmd) = editor_command() else {
            eprintln!(
                "[evelyn] editor_in_pty=true but no editor command set \
                 (config.editor / $VISUAL / $EDITOR all empty)"
            );
            return;
        };
        // Single-quote the path so any future change to the temp name
        // can't get split by the shell. The path is ASCII (temp_dir +
        // unix-millis), so no embedded apostrophe to escape.
        let line = format!("{cmd} '{}'\r", path.display());
        pty.write(line.as_bytes());
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
