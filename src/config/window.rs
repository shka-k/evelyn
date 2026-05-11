use serde::Deserialize;

use super::builtins::defaults;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Inset between the window edge and the cell grid, in logical points.
    /// Applied symmetrically on all four sides.
    pub padding: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        defaults::WINDOW
    }
}
