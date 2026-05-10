use crate::config::RESOLVED_THEME;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub fn default_fg() -> Rgb {
    RESOLVED_THEME.foreground
}

pub fn default_bg() -> Rgb {
    RESOLVED_THEME.background
}

pub fn cursor_color() -> Rgb {
    RESOLVED_THEME.cursor
}

pub fn cursor_text_color() -> Rgb {
    RESOLVED_THEME.cursor_text
}

/// SGR 30-37 / 90-97 / 40-47 / 100-107.
pub fn ansi_basic(n: u8, bright: bool) -> Rgb {
    let p = &RESOLVED_THEME.ansi;
    let table = if bright {
        [
            p.bright_black,
            p.bright_red,
            p.bright_green,
            p.bright_yellow,
            p.bright_blue,
            p.bright_magenta,
            p.bright_cyan,
            p.bright_white,
        ]
    } else {
        [
            p.black, p.red, p.green, p.yellow, p.blue, p.magenta, p.cyan, p.white,
        ]
    };
    table[(n & 7) as usize]
}

/// SGR 38;5;n / 48;5;n. Colors 0-15 share the basic palette; 16-231 are a
/// 6×6×6 cube; 232-255 are 24 grays.
pub fn ansi_256(n: u8) -> Rgb {
    if n < 16 {
        ansi_basic(n & 7, n >= 8)
    } else if n < 232 {
        let i = n - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        Rgb(scale(r), scale(g), scale(b))
    } else {
        let v = 8 + (n - 232) * 10;
        Rgb(v, v, v)
    }
}
