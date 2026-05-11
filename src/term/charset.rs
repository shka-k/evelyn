use super::Term;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Charset {
    Ascii,
    DecSpecialGraphics,
}

impl Term {
    pub(super) fn designate_charset(&mut self, slot: u8, cs: Charset) {
        match slot {
            0 => self.charset_g0 = cs,
            1 => self.charset_g1 = cs,
            _ => {}
        }
    }

    pub(super) fn shift_in(&mut self) {
        self.active_charset = 0;
    }

    pub(super) fn shift_out(&mut self) {
        self.active_charset = 1;
    }

    pub(super) fn active_cs(&self) -> Charset {
        if self.active_charset == 0 {
            self.charset_g0
        } else {
            self.charset_g1
        }
    }
}

/// DEC Special Graphics (charset `0`) → Unicode. tmux/vim/htop draw box
/// borders by switching G0 to this set and emitting plain ASCII letters,
/// which we'd otherwise render literally as `qqq…` for horizontal lines.
/// Characters outside the table pass through unchanged.
pub(super) fn dec_special_graphics(c: char) -> char {
    match c {
        '`' => '\u{25C6}', // ◆
        'a' => '\u{2592}', // ▒
        'b' => '\u{2409}', // ␉ HT symbol
        'c' => '\u{240C}', // ␌ FF symbol
        'd' => '\u{240D}', // ␍ CR symbol
        'e' => '\u{240A}', // ␊ LF symbol
        'f' => '\u{00B0}', // °
        'g' => '\u{00B1}', // ±
        'h' => '\u{2424}', // ␤ NL symbol
        'i' => '\u{240B}', // ␋ VT symbol
        'j' => '\u{2518}', // ┘
        'k' => '\u{2510}', // ┐
        'l' => '\u{250C}', // ┌
        'm' => '\u{2514}', // └
        'n' => '\u{253C}', // ┼
        'o' => '\u{23BA}', // ⎺
        'p' => '\u{23BB}', // ⎻
        'q' => '\u{2500}', // ─
        'r' => '\u{23BC}', // ⎼
        's' => '\u{23BD}', // ⎽
        't' => '\u{251C}', // ├
        'u' => '\u{2524}', // ┤
        'v' => '\u{2534}', // ┴
        'w' => '\u{252C}', // ┬
        'x' => '\u{2502}', // │
        'y' => '\u{2264}', // ≤
        'z' => '\u{2265}', // ≥
        '{' => '\u{03C0}', // π
        '|' => '\u{2260}', // ≠
        '}' => '\u{00A3}', // £
        '~' => '\u{00B7}', // ·
        _ => c,
    }
}
