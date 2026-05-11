use serde::Deserialize;

use super::builtins::defaults;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    /// Full-cell solid block (default). Cell character is drawn inverted
    /// against the block.
    Block,
    /// Thin horizontal stripe along the cell's bottom edge — does not
    /// invert the underlying character.
    Underline,
    /// Thin vertical bar at the cell's left edge (I-beam) — does not
    /// invert the underlying character.
    Bar,
    /// Hollow rectangle around the cell (4 thin edges). Same look the
    /// renderer uses for an unfocused Block — pick this when you want
    /// that style as the *default* shape regardless of focus.
    Hollow,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorConfig {
    pub shape: CursorShape,
    /// When true, the cursor toggles visibility every `blink_interval_ms`.
    /// Driving this requires an event-loop wakeup, so it stays off by
    /// default to keep the renderer fully event-driven when idle.
    pub blink: bool,
    /// Half-period of the blink cycle in ms. 530 matches xterm's default.
    /// Values <50 are clamped at use to keep the wakeup rate sane.
    pub blink_interval_ms: u64,
}

impl Default for CursorConfig {
    fn default() -> Self {
        defaults::CURSOR
    }
}
