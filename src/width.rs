use unicode_width::UnicodeWidthChar;

/// Whether `c` should occupy two terminal cells. Delegates to the Unicode
/// East Asian Width table via `unicode-width`. Ambiguous-width characters
/// (e.g. `⏺`, `①`, box-drawing) stay narrow here; flip to `width_cjk()` if
/// we ever expose a CJK-ambiguous toggle in config.
pub fn is_wide(c: char) -> bool {
    UnicodeWidthChar::width(c) == Some(2)
}
