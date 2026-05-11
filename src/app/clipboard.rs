use super::App;

impl App {
    /// Write `text` to the system clipboard. Errors are logged and dropped
    /// — copy is a fire-and-forget UX action and we don't want a clipboard
    /// hiccup to crash the terminal mid-session.
    pub(super) fn copy_to_clipboard(&self, text: &str) {
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

    /// Read the system clipboard and feed it to the PTY as if typed.
    /// Newlines are normalized to CR (what Enter produces), and any
    /// embedded paste-end marker (`\e[201~`) is stripped so a hostile
    /// clipboard payload can't break out of bracketed paste and inject
    /// commands. When the foreground app has DECSET 2004 on, the bytes
    /// are wrapped in `\e[200~ … \e[201~` so it knows this is a paste.
    pub(super) fn paste_from_clipboard(&mut self) {
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
}
