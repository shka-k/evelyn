/// East Asian Wide / Fullwidth characters that should occupy two terminal cells.
/// Hand-rolled subset of `unicode-width` covering the ranges we actually meet
/// in CJK terminal output.
pub fn is_wide(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
        0x1100..=0x115F |  // Hangul Jamo
        0x2329..=0x232A |  // angle brackets
        0x2E80..=0x303E |  // CJK Radicals / Symbols
        0x3041..=0x33FF |  // Hiragana, Katakana, CJK Symbols
        0x3400..=0x4DBF |  // CJK Unified Ideographs Extension A
        0x4E00..=0x9FFF |  // CJK Unified Ideographs
        0xA000..=0xA4CF |  // Yi
        0xAC00..=0xD7A3 |  // Hangul Syllables
        0xF900..=0xFAFF |  // CJK Compatibility Ideographs
        0xFE30..=0xFE4F |  // CJK Compatibility Forms
        0xFF00..=0xFF60 |  // Fullwidth Forms
        0xFFE0..=0xFFE6
    )
}
