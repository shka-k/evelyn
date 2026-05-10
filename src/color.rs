use crate::config::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// SGR color slot stored unresolved in cells. Keeping `Default` and
/// `Indexed` symbolic — instead of baking them through the theme at parse
/// time — is what makes hot-reload visually update zellij/vim and any
/// other already-painted content: the renderer re-resolves through the
/// live theme on every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// SGR 39/49 or post-reset state — picks up the live theme's fg/bg.
    Default,
    /// SGR 30-37/40-47 (n in 0..=7), 90-97/100-107 (n in 8..=15), and the
    /// extended 256-color palette (n in 16..=255). Resolved via [`ansi_256`].
    Indexed(u8),
    /// SGR 38;2;r;g;b / 48;2;r;g;b — already absolute, never re-themed.
    Rgb(Rgb),
}

impl Color {
    pub fn resolve_fg(self) -> Rgb {
        match self {
            Color::Default => default_fg(),
            Color::Indexed(n) => ansi_256(n),
            Color::Rgb(c) => c,
        }
    }

    pub fn resolve_bg(self) -> Rgb {
        match self {
            Color::Default => default_bg(),
            Color::Indexed(n) => ansi_256(n),
            Color::Rgb(c) => c,
        }
    }
}

pub fn default_fg() -> Rgb {
    theme().foreground
}

pub fn default_bg() -> Rgb {
    theme().background
}

pub fn cursor_color() -> Rgb {
    theme().cursor
}

pub fn cursor_text_color() -> Rgb {
    theme().cursor_text
}

/// SGR 30-37 / 90-97 / 40-47 / 100-107.
pub fn ansi_basic(n: u8, bright: bool) -> Rgb {
    let t = theme();
    let p = &t.ansi;
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
