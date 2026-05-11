use serde::Deserialize;

use super::builtins::defaults;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Font family name (matches a Typographic Family or Family Name entry
    /// in any installed font). `None` falls back to the bundled font.
    pub family: Option<String>,

    /// Font size in logical points. Multiplied by the window scale factor at
    /// render time.
    pub size_pt: f32,

    /// Line height as a multiple of the font size.
    pub line_height_factor: f32,

    /// Whether to enable OpenType programming ligatures (`liga`, `clig`,
    /// `calt`, `dlig`). Set to `false` for fonts where you want to see the
    /// raw characters (e.g. `==`, `->`, `>=`) without composition.
    pub ligatures: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        defaults::FONT
    }
}
