//! Bundled default values for each `Config` section. Section `Default`
//! impls just hand back these constants — keep the actual numbers here so
//! the out-of-box experience can be reviewed in one file.

use super::super::cursor::{CursorConfig, CursorShape};
use super::super::font::FontConfig;
use super::super::window::WindowConfig;

pub const CURSOR: CursorConfig = CursorConfig {
    shape: CursorShape::Block,
    blink: false,
    blink_interval_ms: 530,
};

pub const FONT: FontConfig = FontConfig {
    family: None,
    size_pt: 14.0,
    line_height_factor: 1.3,
    ligatures: true,
};

pub const WINDOW: WindowConfig = WindowConfig { padding: 8.0 };

// `ShaderConfig::effect` is a `String` so the whole struct can't be const.
// Section `Default` impl builds the struct from these two literals.
pub const SHADER_ENABLED: bool = true;
pub const SHADER_EFFECT: &str = "newpixie-crt";
