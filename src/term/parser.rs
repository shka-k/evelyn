use vte::{Params, Perform};

use crate::color::{cursor_color, default_bg, default_fg, Color, Rgb};

use super::{Charset, Term};

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.carriage_return(),
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            0x08 => self.backspace(),
            b'\t' => self.tab(),
            0x07 => {} // bell
            // SI / SO — toggle the GL slot between G0 and G1. ncurses uses
            // these on terminfo entries that drive borders via SO instead
            // of designating G0 (`smacs`/`rmacs`).
            0x0F => self.shift_in(),
            0x0E => self.shift_out(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // Charset designation: `ESC ( <c>` for G0, `ESC ) <c>` for G1, etc.
        // Only `B` (ASCII) and `0` (DEC Special Graphics) appear in the
        // wild for tmux/vim/htop borders; treat anything else as ASCII.
        if intermediates.len() == 1 {
            let slot = match intermediates[0] {
                b'(' => Some(0u8),
                b')' => Some(1u8),
                b'*' => Some(2u8), // designated but never made active
                b'+' => Some(3u8),
                _ => None,
            };
            if let Some(slot) = slot {
                let cs = match byte {
                    b'0' => Charset::DecSpecialGraphics,
                    _ => Charset::Ascii,
                };
                self.designate_charset(slot, cs);
                return;
            }
        }
        match byte {
            b'7' => self.save_cursor(),    // DECSC
            b'8' => self.restore_cursor(), // DECRC
            b'M' => self.reverse_index(),  // RI: scroll within region if at top
            b'D' => self.line_feed(),      // IND: like LF
            b'E' => {                      // NEL: CR + LF
                self.carriage_return();
                self.line_feed();
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let term_seq: &[u8] = if bell_terminated { b"\x07" } else { b"\x1b\\" };
        let Some(num_bytes) = params.first() else { return };
        let Ok(num_str) = std::str::from_utf8(num_bytes) else { return };
        let Ok(base) = num_str.parse::<u32>() else { return };
        // OSC 52 — clipboard set/query. Format is `OSC 52 ; <sel> ; <data> ST`
        // where <sel> is any combo of c/p/s/q/0-7 (we treat them all as the
        // single macOS pasteboard) and <data> is base64-encoded text, or `?`
        // to read. Queries are intentionally ignored — replying would let
        // any process inside the terminal exfiltrate clipboard contents,
        // and zellij/tmux only need the write half for their copy actions.
        if base == 52 {
            if let Some(&payload) = params.get(2)
                && payload != b"?"
                && let Some(decoded) = base64_decode(payload)
                && let Ok(text) = std::str::from_utf8(&decoded)
                && !text.is_empty()
            {
                self.pending_clipboard = Some(text.to_string());
            }
            return;
        }
        if !(10..=12).contains(&base) {
            return;
        }
        for (i, arg) in params[1..].iter().enumerate() {
            let target = base + i as u32;
            if *arg != b"?" || target > 12 {
                break;
            }
            let c = match target {
                10 => default_fg(),
                11 => default_bg(),
                12 => cursor_color(),
                _ => break,
            };
            let resp = format!(
                "\x1b]{target};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}",
                c.0, c.0, c.1, c.1, c.2, c.2
            );
            self.replies.extend_from_slice(resp.as_bytes());
            self.replies.extend_from_slice(term_seq);
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // DECSET / DECRST — `\e[?Nh / l`. Cover the modes we actually act on.
        if intermediates == b"?" && (action == 'h' || action == 'l') {
            let on = action == 'h';
            for n in params.iter().flatten().copied() {
                match n {
                    1 => self.app_cursor_keys = on,
                    7 => self.auto_wrap = on,
                    25 => self.cursor_visible = on,
                    // Mouse tracking modes. Setting any one implies the
                    // previous; resetting any clears tracking entirely
                    // (xterm semantics — apps reset whichever mode they
                    // set, and shouldn't end up with a stale level).
                    1000 => self.mouse_proto = if on { super::MouseProto::Press } else { super::MouseProto::Off },
                    1002 => self.mouse_proto = if on { super::MouseProto::Button } else { super::MouseProto::Off },
                    1003 => self.mouse_proto = if on { super::MouseProto::Any } else { super::MouseProto::Off },
                    1006 => self.mouse_sgr = on,
                    2004 => self.bracketed_paste = on,
                    1047 | 1049 => {
                        if on {
                            self.enter_alt_screen();
                        } else {
                            self.exit_alt_screen();
                        }
                    }
                    _ => {}
                }
            }
            self.dirty = true;
            return;
        }
        match action {
            'c' => self.dispatch_da(intermediates),
            'n' => self.dispatch_dsr(first_param(params, 0)),
            'm' => sgr(self, params),
            'H' | 'f' => {
                let mut it = params.iter();
                let row = it.next().and_then(|p| p.first().copied()).unwrap_or(1).max(1);
                let col = it.next().and_then(|p| p.first().copied()).unwrap_or(1).max(1);
                self.cur_y = (row - 1).min(self.rows.saturating_sub(1));
                self.cur_x = (col - 1).min(self.cols.saturating_sub(1));
                self.pending_wrap = false;
                self.dirty = true;
            }
            'A' => {
                let n = first_param(params, 1);
                self.cur_y = self.cur_y.saturating_sub(n);
                self.pending_wrap = false;
                self.dirty = true;
            }
            'B' => {
                let n = first_param(params, 1);
                self.cur_y = (self.cur_y + n).min(self.rows.saturating_sub(1));
                self.pending_wrap = false;
                self.dirty = true;
            }
            'C' => {
                let n = first_param(params, 1);
                self.cur_x = (self.cur_x + n).min(self.cols.saturating_sub(1));
                self.pending_wrap = false;
                self.dirty = true;
            }
            'D' => {
                let n = first_param(params, 1);
                self.cur_x = self.cur_x.saturating_sub(n);
                self.pending_wrap = false;
                self.dirty = true;
            }
            'G' => {
                let n = first_param(params, 1);
                self.cur_x = (n - 1).min(self.cols.saturating_sub(1));
                self.pending_wrap = false;
                self.dirty = true;
            }
            'd' => {
                let n = first_param(params, 1);
                self.cur_y = (n - 1).min(self.rows.saturating_sub(1));
                self.pending_wrap = false;
                self.dirty = true;
            }
            'J' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.erase_in_display(mode);
            }
            'K' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.erase_in_line(mode);
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'r' => {
                // DECSTBM: top;bottom in 1-based row indices. Both omitted
                // means reset region to the full screen.
                let mut it = params.iter();
                let top = it.next().and_then(|p| p.first().copied()).unwrap_or(1).max(1);
                let bot = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .filter(|&v| v != 0)
                    .unwrap_or(self.rows);
                self.set_scroll_region(top.saturating_sub(1), bot.saturating_sub(1));
                self.dirty = true;
            }
            'L' => self.insert_lines(first_param(params, 1)),
            'M' => self.delete_lines(first_param(params, 1)),
            'S' => {
                self.scroll_up_in_region(first_param(params, 1));
                self.dirty = true;
            }
            'T' => {
                self.scroll_down_in_region(first_param(params, 1));
                self.dirty = true;
            }
            '@' => self.insert_chars(first_param(params, 1)),
            'P' => self.delete_chars(first_param(params, 1)),
            'X' => self.erase_chars(first_param(params, 1)),
            't' => {
                // Window manipulation queries. Reply to the size queries
                // so apps that gate on them (zellij occasionally does
                // for pixel-accurate layout) don't spin waiting.
                let mode = first_param(params, 0);
                match mode {
                    14 => {
                        // Report cell size in pixels — we don't know the
                        // host pixel size from the post pass, so return a
                        // conservative default. Apps mostly just need a
                        // non-empty reply to unblock.
                        self.replies.extend_from_slice(b"\x1b[4;600;800t");
                    }
                    18 => {
                        // Report screen size in characters.
                        let s = format!("\x1b[8;{};{}t", self.rows, self.cols);
                        self.replies.extend_from_slice(s.as_bytes());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Term {
    /// Reply to a Device Attributes request so apps like fish don't time
    /// out. Bumped from VT102 to VT220 (`?62;…`) advertising 132-column,
    /// printer, ANSI text locator, and selectable charset extensions —
    /// zellij/tmux gate part of their init on this and the older VT102
    /// reply could leave them spinning long enough to drop the first
    /// inner-shell prompt.
    fn dispatch_da(&mut self, intermediates: &[u8]) {
        if intermediates.is_empty() {
            self.replies.extend_from_slice(b"\x1b[?62;1;2;6;9c");
        } else if intermediates == b">" {
            // Secondary DA — pose as xterm patch level 0.
            self.replies.extend_from_slice(b"\x1b[>0;0;0c");
        }
    }

    /// Device Status Report (`CSI 5 n` = OK, `CSI 6 n` = cursor position).
    fn dispatch_dsr(&mut self, mode: u16) {
        match mode {
            5 => self.replies.extend_from_slice(b"\x1b[0n"),
            6 => {
                let s = format!("\x1b[{};{}R", self.cur_y + 1, self.cur_x + 1);
                self.replies.extend_from_slice(s.as_bytes());
            }
            _ => {}
        }
    }
}

/// Decode standard base64 (RFC 4648 alphabet, padding optional). Returns
/// `None` on any non-alphabet byte; whitespace and `=` padding are ignored.
/// Inlined to avoid pulling a base64 crate just for OSC 52 — payloads here
/// are short (a copied selection) and the standard alphabet is the only
/// one specified by the OSC 52 spec.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for &b in input {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v: u32 = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    Some(out)
}

fn first_param(params: &Params, default: u16) -> u16 {
    params
        .iter()
        .next()
        .and_then(|p| p.first().copied())
        .map(|v| if v == 0 { default } else { v })
        .unwrap_or(default)
}

fn sgr(term: &mut Term, params: &Params) {
    let flat: Vec<u16> = params.iter().flatten().copied().collect();
    if flat.is_empty() {
        term.reset_attrs();
        return;
    }
    let mut i = 0;
    while i < flat.len() {
        let p = flat[i];
        match p {
            0 => term.reset_attrs(),
            1 => term.bold = true,
            7 => term.reverse = true,
            22 => term.bold = false,
            27 => term.reverse = false,
            30..=37 => term.fg = Color::Indexed((p - 30) as u8),
            90..=97 => term.fg = Color::Indexed(8 + (p - 90) as u8),
            40..=47 => term.bg = Color::Indexed((p - 40) as u8),
            100..=107 => term.bg = Color::Indexed(8 + (p - 100) as u8),
            39 => term.fg = Color::Default,
            49 => term.bg = Color::Default,
            38 | 48 => {
                // 38;5;n  or 38;2;r;g;b
                if let Some(&kind) = flat.get(i + 1) {
                    if kind == 5 {
                        if let Some(&n) = flat.get(i + 2) {
                            let c = Color::Indexed(n as u8);
                            if p == 38 { term.fg = c; } else { term.bg = c; }
                            i += 2;
                        }
                    } else if kind == 2
                        && let (Some(&r), Some(&g), Some(&b)) =
                            (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4))
                    {
                        let c = Color::Rgb(Rgb(r as u8, g as u8, b as u8));
                        if p == 38 { term.fg = c; } else { term.bg = c; }
                        i += 4;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
}
