use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Deserializer};

use crate::color::Rgb;
use crate::themes::BUILTIN_THEMES;

/// User-tweakable settings. Returned as an `Arc` so callers can hold it
/// across a hot-reload boundary without the file watcher pulling the rug
/// out. Search order: `$EVELYN_CONFIG` → `$XDG_CONFIG_HOME/evelyn/config.toml`
/// → `~/.config/evelyn/config.toml`. Missing file falls back silently;
/// parse errors are logged and the previous value is kept.
pub fn config() -> Arc<Config> {
    config_slot().read().unwrap().clone()
}

/// Effective theme — resolved from `config().theme` against either a
/// built-in or a file under `~/.config/evelyn/themes/`. Same Arc snapshot
/// pattern as [`config`] so render-time reads are stable.
pub fn theme() -> Arc<ThemeConfig> {
    theme_slot().read().unwrap().clone()
}

/// Re-read both files and atomically swap. Returns a snapshot of the
/// previous and new values so callers can decide what to invalidate
/// (e.g. rebuild the post-processor only when the shader effect changed).
pub fn reload() -> Reload {
    let prev_cfg = config();
    let next_cfg = Arc::new(load_or_default());
    *config_slot().write().unwrap() = next_cfg.clone();
    let next_theme = Arc::new(resolve_theme_with(&next_cfg));
    *theme_slot().write().unwrap() = next_theme;
    Reload {
        prev_cfg,
        cfg: next_cfg,
    }
}

/// Snapshot returned by [`reload`]. The renderer doesn't currently diff
/// anything off of this, but the prev/next config pair lets the caller
/// decide whether the theme path changed and the watcher needs respawning.
pub struct Reload {
    pub prev_cfg: Arc<Config>,
    pub cfg: Arc<Config>,
}

fn config_slot() -> &'static RwLock<Arc<Config>> {
    static SLOT: OnceLock<RwLock<Arc<Config>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Arc::new(load_or_default())))
}

fn theme_slot() -> &'static RwLock<Arc<ThemeConfig>> {
    static SLOT: OnceLock<RwLock<Arc<ThemeConfig>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Arc::new(resolve_theme_with(&config()))))
}

/// Path of the live config file, if one is reachable. Hot-reload watchers
/// use this to know what to subscribe to.
pub fn config_file_path() -> Option<PathBuf> {
    config_path()
}

/// Path of the theme file currently in use, when the active theme resolves
/// to an on-disk file (not a built-in). Returns `None` for built-ins or
/// when no theme is set, so the watcher only subscribes to real paths.
pub fn theme_file_path() -> Option<PathBuf> {
    let cfg = config();
    let name = cfg.theme.as_deref()?;
    if lookup_builtin_theme(name).is_some() {
        return None;
    }
    let dir = themes_dir()?;
    Some(dir.join(format!("{name}.toml")))
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub shader: ShaderConfig,
    pub cursor: CursorConfig,

    /// Theme file name (without `.toml`) under `~/.config/evelyn/themes/`.
    /// The file must use the Alacritty `[colors.*]` schema, so you can
    /// drop in or symlink files from `~/.config/alacritty/themes/themes/`.
    /// `None` falls back to the built-in defaults.
    pub theme: Option<String>,

    /// Override the shell to spawn. `None` uses `$SHELL`, then `/bin/bash`.
    pub shell: Option<String>,
}

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
        Self {
            shape: CursorShape::Block,
            blink: false,
            blink_interval_ms: 530,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShaderConfig {
    /// Master switch. When `false` the post-processing pass is skipped
    /// entirely regardless of `effect`.
    pub enabled: bool,
    /// Built-in name (`"newpixie-crt"`, `"none"`) or a filename under
    /// `~/.config/evelyn/shaders/` (with or without `.wgsl`). Built-ins
    /// resolve in zero IO; user files are read at startup.
    pub effect: String,
}

impl ShaderConfig {
    pub fn effect_name(&self) -> &str {
        if self.enabled {
            self.effect.as_str()
        } else {
            "none"
        }
    }
}

impl Default for ShaderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            effect: "newpixie-crt".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Inset between the window edge and the cell grid, in logical points.
    /// Applied symmetrically on all four sides.
    pub padding: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { padding: 8.0 }
    }
}

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
        Self {
            family: None,
            size_pt: 14.0,
            line_height_factor: 1.3,
            ligatures: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    /// Terminal background.
    #[serde(deserialize_with = "de_rgb")]
    pub background: Rgb,
    /// Default text foreground when no SGR color is active.
    #[serde(deserialize_with = "de_rgb")]
    pub foreground: Rgb,
    /// Cursor block color.
    #[serde(deserialize_with = "de_rgb")]
    pub cursor: Rgb,
    /// Foreground color of the character under the (block) cursor.
    #[serde(deserialize_with = "de_rgb")]
    pub cursor_text: Rgb,
    /// 16-color ANSI palette. SGR 30-37 / 90-97 / 40-47 / 100-107 and the
    /// first 16 entries of SGR 38;5/48;5 read from here.
    pub ansi: AnsiPalette,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        crate::themes::DEFAULT.clone()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnsiPalette {
    #[serde(deserialize_with = "de_rgb")]
    pub black: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub red: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub green: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub yellow: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub blue: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub magenta: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub cyan: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub white: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_black: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_red: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_green: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_yellow: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_blue: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_magenta: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_cyan: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    pub bright_white: Rgb,
}

impl Default for AnsiPalette {
    fn default() -> Self {
        Self {
            black: Rgb(0x00, 0x00, 0x00),
            red: Rgb(0xcd, 0x31, 0x31),
            green: Rgb(0x0d, 0xbc, 0x79),
            yellow: Rgb(0xe5, 0xe5, 0x10),
            blue: Rgb(0x24, 0x72, 0xc8),
            magenta: Rgb(0xbc, 0x3f, 0xbc),
            cyan: Rgb(0x11, 0xa8, 0xcd),
            white: Rgb(0xe5, 0xe5, 0xe5),
            bright_black: Rgb(0x66, 0x66, 0x66),
            bright_red: Rgb(0xf1, 0x4c, 0x4c),
            bright_green: Rgb(0x23, 0xd1, 0x8b),
            bright_yellow: Rgb(0xf5, 0xf5, 0x43),
            bright_blue: Rgb(0x3b, 0x8e, 0xea),
            bright_magenta: Rgb(0xd6, 0x70, 0xd6),
            bright_cyan: Rgb(0x29, 0xb8, 0xdb),
            bright_white: Rgb(0xff, 0xff, 0xff),
        }
    }
}

fn de_rgb<'de, D: Deserializer<'de>>(d: D) -> Result<Rgb, D::Error> {
    let s = String::deserialize(d)?;
    parse_hex_rgb(&s).ok_or_else(|| {
        serde::de::Error::custom(format!("expected color like \"#rrggbb\", got {s:?}"))
    })
}

fn parse_hex_rgb(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

impl Config {
    /// Resolve which shell binary to spawn.
    pub fn resolved_shell(&self) -> String {
        if let Some(s) = self.shell.as_deref() {
            return s.to_string();
        }
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
    }
}

fn resolve_theme_with(cfg: &Config) -> ThemeConfig {
    let Some(name) = cfg.theme.as_deref() else {
        return ThemeConfig::default();
    };
    // Built-ins first — no file IO and no surprise fallbacks if the user's
    // theme dir doesn't exist yet.
    if let Some(theme) = lookup_builtin_theme(name) {
        eprintln!("[evelyn] loaded theme: {name} (built-in)");
        return theme;
    }
    let path = match themes_dir() {
        Some(d) => d.join(format!("{name}.toml")),
        None => {
            eprintln!("[evelyn] $HOME unset; cannot resolve theme {name:?}");
            return ThemeConfig::default();
        }
    };
    match load_alacritty_theme(&path) {
        Ok(t) => {
            eprintln!("[evelyn] loaded theme: {} ({})", name, path.display());
            t
        }
        Err(e) => {
            eprintln!("[evelyn] theme {name:?} load failed: {e}");
            ThemeConfig::default()
        }
    }
}

fn lookup_builtin_theme(name: &str) -> Option<ThemeConfig> {
    BUILTIN_THEMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| t.clone())
}

fn themes_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("evelyn/themes"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/evelyn/themes"))
}

fn load_alacritty_theme(path: &PathBuf) -> Result<ThemeConfig, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let file: AlacrittyThemeFile =
        toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(file.into_theme())
}

#[derive(Deserialize)]
struct AlacrittyThemeFile {
    colors: AlacrittyColors,
}

#[derive(Deserialize)]
struct AlacrittyColors {
    primary: AlacrittyPrimary,
    #[serde(default)]
    cursor: Option<AlacrittyCursor>,
    #[serde(default)]
    normal: Option<AlacrittyNormalBright>,
    #[serde(default)]
    bright: Option<AlacrittyNormalBright>,
}

#[derive(Deserialize)]
struct AlacrittyPrimary {
    #[serde(deserialize_with = "de_rgb")]
    background: Rgb,
    #[serde(deserialize_with = "de_rgb")]
    foreground: Rgb,
}

#[derive(Deserialize)]
struct AlacrittyCursor {
    #[serde(deserialize_with = "de_rgb_opt", default)]
    cursor: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    text: Option<Rgb>,
}

#[derive(Deserialize)]
struct AlacrittyNormalBright {
    #[serde(deserialize_with = "de_rgb_opt", default)]
    black: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    red: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    green: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    yellow: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    blue: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    magenta: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    cyan: Option<Rgb>,
    #[serde(deserialize_with = "de_rgb_opt", default)]
    white: Option<Rgb>,
}

impl AlacrittyThemeFile {
    fn into_theme(self) -> ThemeConfig {
        let defaults = ThemeConfig::default();
        let cursor = self.colors.cursor.as_ref();
        let normal = self.colors.normal.as_ref();
        let bright = self.colors.bright.as_ref();
        ThemeConfig {
            background: self.colors.primary.background,
            foreground: self.colors.primary.foreground,
            cursor: cursor.and_then(|c| c.cursor).unwrap_or(defaults.cursor),
            // Some themes omit cursor.text — fall back to background so the
            // inverted cursor character stays visible.
            cursor_text: cursor
                .and_then(|c| c.text)
                .unwrap_or(self.colors.primary.background),
            ansi: AnsiPalette {
                black: normal.and_then(|n| n.black).unwrap_or(defaults.ansi.black),
                red: normal.and_then(|n| n.red).unwrap_or(defaults.ansi.red),
                green: normal.and_then(|n| n.green).unwrap_or(defaults.ansi.green),
                yellow: normal
                    .and_then(|n| n.yellow)
                    .unwrap_or(defaults.ansi.yellow),
                blue: normal.and_then(|n| n.blue).unwrap_or(defaults.ansi.blue),
                magenta: normal
                    .and_then(|n| n.magenta)
                    .unwrap_or(defaults.ansi.magenta),
                cyan: normal.and_then(|n| n.cyan).unwrap_or(defaults.ansi.cyan),
                white: normal.and_then(|n| n.white).unwrap_or(defaults.ansi.white),
                bright_black: bright
                    .and_then(|b| b.black)
                    .unwrap_or(defaults.ansi.bright_black),
                bright_red: bright
                    .and_then(|b| b.red)
                    .unwrap_or(defaults.ansi.bright_red),
                bright_green: bright
                    .and_then(|b| b.green)
                    .unwrap_or(defaults.ansi.bright_green),
                bright_yellow: bright
                    .and_then(|b| b.yellow)
                    .unwrap_or(defaults.ansi.bright_yellow),
                bright_blue: bright
                    .and_then(|b| b.blue)
                    .unwrap_or(defaults.ansi.bright_blue),
                bright_magenta: bright
                    .and_then(|b| b.magenta)
                    .unwrap_or(defaults.ansi.bright_magenta),
                bright_cyan: bright
                    .and_then(|b| b.cyan)
                    .unwrap_or(defaults.ansi.bright_cyan),
                bright_white: bright
                    .and_then(|b| b.white)
                    .unwrap_or(defaults.ansi.bright_white),
            },
        }
    }
}

fn de_rgb_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Rgb>, D::Error> {
    let s = Option::<String>::deserialize(d)?;
    match s {
        None => Ok(None),
        Some(s) => parse_hex_rgb(&s).map(Some).ok_or_else(|| {
            serde::de::Error::custom(format!("expected color like \"#rrggbb\", got {s:?}"))
        }),
    }
}

fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EVELYN_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("evelyn/config.toml"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/evelyn/config.toml"))
}

fn load_or_default() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str::<Config>(&text) {
        Ok(c) => {
            eprintln!("[evelyn] loaded config: {}", path.display());
            c
        }
        Err(e) => {
            eprintln!(
                "[evelyn] config parse error in {}: {e}\n[evelyn] falling back to defaults",
                path.display()
            );
            Config::default()
        }
    }
}

