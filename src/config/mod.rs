//! User-tweakable settings. `Config` lives behind an Arc-swapped RwLock so
//! callers can hold a snapshot across a hot-reload boundary. Modules under
//! this folder hold one config section each; the live state and reload
//! plumbing stays here.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use serde::Deserialize;

mod builtins;
mod cursor;
mod font;
mod shader;
mod theme;
mod window;

pub use cursor::{CursorConfig, CursorShape};
pub use font::FontConfig;
pub use shader::{ShaderConfig, resolve_shader_source};
pub use theme::ThemeConfig;
pub use window::WindowConfig;

use theme::{lookup_builtin_theme, resolve_theme_with, themes_dir};

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

    /// Command launched by Cmd+E to view the buffer dump. Whitespace-
    /// tokenized; the temp-file path is appended as the final argument
    /// (e.g. `"code -r -w"` → `code -r -w /tmp/evelyn-buffer-….txt`).
    /// `None` falls back to `$VISUAL`, then `$EDITOR`, then `open -t`
    /// (macOS default text-editor handler).
    pub editor: Option<String>,

    /// When true, the editor command is written to the focused window's
    /// PTY as if typed at the shell prompt, so TUI editors (vi/nvim/hx/…)
    /// run inside the current terminal instead of attaching to whichever
    /// TTY originally spawned Evelyn. Leave false for GUI editors
    /// (`code -r -w`, `cursor -r -w`, `open -t`, …).
    pub editor_in_pty: bool,
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
    if let Ok(p) = std::env::var("EVELYN_CONFIG")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("evelyn/config.toml"));
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
