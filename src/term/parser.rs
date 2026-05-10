use vte::{Params, Perform};

use crate::color::{ansi_256, ansi_basic, cursor_color, default_bg, default_fg, Rgb};

use super::Term;

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
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let term_seq: &[u8] = if bell_terminated { b"\x07" } else { b"\x1b\\" };
        let Some(num_bytes) = params.first() else { return };
        let Ok(num_str) = std::str::from_utf8(num_bytes) else { return };
        let Ok(base) = num_str.parse::<u32>() else { return };
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
                    25 => self.cursor_visible = on,
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
                self.dirty = true;
            }
            'A' => {
                let n = first_param(params, 1);
                self.cur_y = self.cur_y.saturating_sub(n);
                self.dirty = true;
            }
            'B' => {
                let n = first_param(params, 1);
                self.cur_y = (self.cur_y + n).min(self.rows.saturating_sub(1));
                self.dirty = true;
            }
            'C' => {
                let n = first_param(params, 1);
                self.cur_x = (self.cur_x + n).min(self.cols.saturating_sub(1));
                self.dirty = true;
            }
            'D' => {
                let n = first_param(params, 1);
                self.cur_x = self.cur_x.saturating_sub(n);
                self.dirty = true;
            }
            'G' => {
                let n = first_param(params, 1);
                self.cur_x = (n - 1).min(self.cols.saturating_sub(1));
                self.dirty = true;
            }
            'd' => {
                let n = first_param(params, 1);
                self.cur_y = (n - 1).min(self.rows.saturating_sub(1));
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
            _ => {}
        }
    }
}

impl Term {
    /// Reply to a Device Attributes request so apps like fish don't time out.
    fn dispatch_da(&mut self, intermediates: &[u8]) {
        if intermediates.is_empty() {
            // Primary DA — VT102 with Advanced Video Option.
            self.replies.extend_from_slice(b"\x1b[?6c");
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
            22 => term.bold = false,
            30..=37 => term.fg = ansi_basic((p - 30) as u8, false),
            90..=97 => term.fg = ansi_basic((p - 90) as u8, true),
            40..=47 => term.bg = ansi_basic((p - 40) as u8, false),
            100..=107 => term.bg = ansi_basic((p - 100) as u8, true),
            39 => term.fg = default_fg(),
            49 => term.bg = default_bg(),
            38 | 48 => {
                // 38;5;n  or 38;2;r;g;b
                if let Some(&kind) = flat.get(i + 1) {
                    if kind == 5 {
                        if let Some(&n) = flat.get(i + 2) {
                            let c = ansi_256(n as u8);
                            if p == 38 { term.fg = c; } else { term.bg = c; }
                            i += 2;
                        }
                    } else if kind == 2 {
                        if let (Some(&r), Some(&g), Some(&b)) =
                            (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4))
                        {
                            let c = Rgb(r as u8, g as u8, b as u8);
                            if p == 38 { term.fg = c; } else { term.bg = c; }
                            i += 4;
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
}
