use serde::Deserialize;

use super::builtins::defaults;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Inset between the window edge and the cell grid, in logical points.
    /// Applied symmetrically on all four sides.
    pub padding: f32,

    /// Window background opacity in the range `0.0..=1.0`. `1.0` is fully
    /// opaque (the default). Only takes effect when the post-process shader
    /// is disabled (`shader.enabled = false`) — when a shader is active it
    /// fills the surface and would overwrite any transparent regions.
    /// Only the cell backgrounds and the padding bezel become translucent;
    /// glyphs, the cursor, and the selection overlay stay fully opaque so
    /// text remains readable over the desktop behind the window.
    pub opacity: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        defaults::WINDOW
    }
}
