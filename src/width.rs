use unicode_width::UnicodeWidthChar;

/// How many terminal cells `c` should occupy: 2 for East Asian Wide /
/// Fullwidth, 0 for combining marks / variation selectors / ZWJ / other
/// default-ignorable codepoints, 1 otherwise. Ambiguous-width characters
/// (e.g. `⏺`, `①`, box-drawing) stay narrow here; flip to `width_cjk()`
/// if we ever expose a CJK-ambiguous toggle in config. Control bytes
/// (`unicode-width` returns `None`) report as 1 so a stray print still
/// makes progress, though vte normally catches them via `execute`.
pub fn cell_width(c: char) -> u8 {
    match UnicodeWidthChar::width(c) {
        Some(0) => 0,
        Some(2) => 2,
        _ => 1,
    }
}
