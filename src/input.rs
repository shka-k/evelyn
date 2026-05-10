use winit::event::{KeyEvent, Modifiers};
use winit::keyboard::{Key, NamedKey};

/// Translate a keyboard event into bytes to be written to the PTY.
/// `app_cursor_keys` reflects DECCKM — when set, bare cursor keys go out
/// as SS3 (`ESC O X`) instead of CSI (`ESC [ X`), which is what vi /
/// vim / helix / less actually look up in their key tables.
pub fn encode_key(event: &KeyEvent, mods: &Modifiers, app_cursor_keys: bool) -> Option<Vec<u8>> {
    if !event.state.is_pressed() {
        return None;
    }
    let m = mods.state();
    let shift = m.shift_key();
    let ctrl = m.control_key();
    // Cmd folds into alt on macOS so Cmd+Arrow keeps working as a "meta"
    // modifier — most TUIs read it as a word/line jump.
    let alt = m.alt_key() || m.super_key();

    if let Key::Named(named) = &event.logical_key {
        return encode_named(named, shift, alt, ctrl, app_cursor_keys);
    }

    // Character keys: prefer event.text (which already accounts for shift / dead keys).
    let text = event.text.as_deref().unwrap_or("");
    if text.is_empty() {
        if let Key::Character(s) = &event.logical_key {
            return encode_chars(s, ctrl, alt);
        }
        return None;
    }
    encode_chars(text, ctrl, alt)
}

/// xterm-style modifier code: 1 + shift + 2*alt + 4*ctrl. Used inside
/// `CSI 1; <m> X` / `CSI N; <m> ~` for cursor / nav / function keys.
fn modifier_code(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1u8 + (shift as u8) + 2 * (alt as u8) + 4 * (ctrl as u8)
}

fn encode_named(
    named: &NamedKey,
    shift: bool,
    alt: bool,
    ctrl: bool,
    app_cursor_keys: bool,
) -> Option<Vec<u8>> {
    let m_code = modifier_code(shift, alt, ctrl);
    let modded = m_code > 1;

    // Cursor / Home / End: bare goes SS3 (`ESC O X`) when DECCKM is set,
    // otherwise CSI (`ESC [ X`). Modified form is always CSI with the
    // xterm modifier code — DECCKM only affects the bare sequence.
    let csi_letter = |letter: u8| -> Vec<u8> {
        if modded {
            format!("\x1b[1;{}{}", m_code, letter as char).into_bytes()
        } else if app_cursor_keys {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    };
    // PageUp/Down, Insert, Delete: `\x1b[N~` bare, `\x1b[N;<m>~` modded.
    let csi_tilde = |n: u8| -> Vec<u8> {
        if modded {
            format!("\x1b[{};{}~", n, m_code).into_bytes()
        } else {
            format!("\x1b[{}~", n).into_bytes()
        }
    };
    // F1-F4: SS3 form (`\x1bOP`…) bare, `\x1b[1;<m>P`… modded — matches
    // xterm and what helix / fish / zsh actually look up in their tables.
    let csi_fkey = |letter: u8| -> Vec<u8> {
        if modded {
            format!("\x1b[1;{}{}", m_code, letter as char).into_bytes()
        } else {
            vec![0x1b, b'O', letter]
        }
    };

    let bytes = match named {
        NamedKey::ArrowUp => csi_letter(b'A'),
        NamedKey::ArrowDown => csi_letter(b'B'),
        NamedKey::ArrowRight => csi_letter(b'C'),
        NamedKey::ArrowLeft => csi_letter(b'D'),
        NamedKey::Home => csi_letter(b'H'),
        NamedKey::End => csi_letter(b'F'),
        NamedKey::PageUp => csi_tilde(5),
        NamedKey::PageDown => csi_tilde(6),
        NamedKey::Insert => csi_tilde(2),
        NamedKey::Delete => csi_tilde(3),
        NamedKey::F1 => csi_fkey(b'P'),
        NamedKey::F2 => csi_fkey(b'Q'),
        NamedKey::F3 => csi_fkey(b'R'),
        NamedKey::F4 => csi_fkey(b'S'),
        // Keys that don't carry an xterm modifier code: fall back to
        // the plain byte sequence with an ESC prefix for alt/meta.
        NamedKey::Enter => return Some(esc_prefix(b"\r", alt)),
        NamedKey::Backspace => return Some(esc_prefix(b"\x7f", alt)),
        NamedKey::Tab => return Some(esc_prefix(b"\t", alt)),
        NamedKey::Escape => return Some(esc_prefix(b"\x1b", alt)),
        NamedKey::Space => return Some(esc_prefix(b" ", alt)),
        _ => return None,
    };
    Some(bytes)
}

fn esc_prefix(bytes: &[u8], alt: bool) -> Vec<u8> {
    if !alt {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(0x1b);
    out.extend_from_slice(bytes);
    out
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
