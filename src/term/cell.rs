use crate::color::{Color, Rgb};

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    /// `'\0'` marks the right half of a wide character (continuation).
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    /// `\\e[7m` reverse video. Renderers swap fg/bg on output — this is
    /// how TUIs like yazi paint selection highlights without setting an
    /// explicit bg color.
    pub reverse: bool,
    /// Set on the LEFT half of a wide character. Its right neighbour is a
    /// continuation cell with `ch == '\0'`.
    pub wide: bool,
}

impl Cell {
    /// Effective foreground / background after reverse-video is applied,
    /// resolved against the live theme. The renderer should use these for
    /// actual drawing, never `fg`/`bg` directly, otherwise reverse cells
    /// lose their highlight and `Color::Default` skips the theme.
    pub fn fg_eff(&self) -> Rgb {
        if self.reverse {
            self.bg.resolve_bg()
        } else {
            self.fg.resolve_fg()
        }
    }
    pub fn bg_eff(&self) -> Rgb {
        if self.reverse {
            self.fg.resolve_fg()
        } else {
            self.bg.resolve_bg()
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            reverse: false,
            wide: false,
        }
    }
}
