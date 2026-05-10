use winit::event::{KeyEvent, Modifiers};
use winit::keyboard::{Key, NamedKey};

/// Translate a keyboard event into bytes to be written to the PTY.
pub fn encode_key(event: &KeyEvent, mods: &Modifiers) -> Option<Vec<u8>> {
    if !event.state.is_pressed() {
        return None;
    }
    let m = mods.state();
    let ctrl = m.control_key();
    let alt = m.alt_key() || m.super_key(); // treat Cmd as Alt-ish on macOS for ESC-prefix Meta

    if let Key::Named(named) = &event.logical_key {
        let bytes: &[u8] = match named {
            NamedKey::Enter => b"\r",
            NamedKey::Backspace => b"\x7f",
            NamedKey::Tab => b"\t",
            NamedKey::Escape => b"\x1b",
            NamedKey::ArrowUp => b"\x1b[A",
            NamedKey::ArrowDown => b"\x1b[B",
            NamedKey::ArrowRight => b"\x1b[C",
            NamedKey::ArrowLeft => b"\x1b[D",
            NamedKey::Home => b"\x1b[H",
            NamedKey::End => b"\x1b[F",
            NamedKey::PageUp => b"\x1b[5~",
            NamedKey::PageDown => b"\x1b[6~",
            NamedKey::Delete => b"\x1b[3~",
            NamedKey::Insert => b"\x1b[2~",
            NamedKey::F1 => b"\x1bOP",
            NamedKey::F2 => b"\x1bOQ",
            NamedKey::F3 => b"\x1bOR",
            NamedKey::F4 => b"\x1bOS",
            NamedKey::Space => b" ",
            _ => return None,
        };
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        return Some(out);
    }

    // Character keys: prefer event.text (which already accounts for shift / dead keys).
    let text = event.text.as_deref().unwrap_or("");
    if text.is_empty() {
        // Fallback: pull a single character out of the logical key.
        if let Key::Character(s) = &event.logical_key {
            return encode_chars(s, ctrl, alt);
        }
        return None;
    }
    encode_chars(text, ctrl, alt)
}

fn encode_chars(text: &str, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    if ctrl {
        // Map Ctrl+letter to control byte. Only handle a single character cleanly.
        let mut chars = text.chars();
        if let Some(c) = chars.next() {
            let lower = c.to_ascii_lowercase();
            let byte = match lower {
                'a'..='z' => Some((lower as u8) - b'a' + 1),
                ' ' | '@' => Some(0x00),
                '[' => Some(0x1b),
                '\\' => Some(0x1c),
                ']' => Some(0x1d),
                '^' => Some(0x1e),
                '_' | '?' => Some(0x1f),
                _ => None,
            };
            if let Some(b) = byte {
                out.push(b);
                return Some(out);
            }
        }
    }
    out.extend_from_slice(text.as_bytes());
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
