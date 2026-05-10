use std::path::PathBuf;
use std::sync::LazyLock;

use serde::Deserialize;

/// User-tweakable settings. Loaded once on first access.
///
/// Search order: `$EVELYN_CONFIG` → `$XDG_CONFIG_HOME/evelyn/config.toml`
/// → `~/.config/evelyn/config.toml`. Missing file falls back silently;
/// parse errors are logged and replaced with defaults.
pub static CONFIG: LazyLock<Config> = LazyLock::new(load_or_default);

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,

    /// Override the shell to spawn. `None` uses `$SHELL`, then `/bin/bash`.
    pub shell: Option<String>,
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

impl Config {
    /// Resolve which shell binary to spawn.
    pub fn resolved_shell(&self) -> String {
        if let Some(s) = self.shell.as_deref() {
            return s.to_string();
        }
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
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
