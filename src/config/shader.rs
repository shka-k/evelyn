use std::borrow::Cow;
use std::path::PathBuf;

use serde::Deserialize;

use super::builtins::{defaults, shaders::builtin_shader_source};

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
            enabled: defaults::SHADER_ENABLED,
            effect: defaults::SHADER_EFFECT.into(),
        }
    }
}

/// Resolve the WGSL source for the configured post-processing effect.
/// `"none"` (or `enabled = false`) → no post pass. Built-ins resolve at
/// compile time. Anything else is read from `~/.config/evelyn/shaders/`.
pub fn resolve_shader_source() -> Option<Cow<'static, str>> {
    let cfg = super::config();
    let name = cfg.shader.effect_name();
    if name == "none" {
        return None;
    }
    if let Some(src) = builtin_shader_source(name) {
        eprintln!("[evelyn] loaded shader: {name} (built-in)");
        return Some(Cow::Borrowed(src));
    }
    let path = match user_shader_path(name) {
        Some(p) => p,
        None => {
            eprintln!("[evelyn] $HOME unset; cannot resolve shader {name:?}");
            return None;
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(src) => {
            eprintln!("[evelyn] loaded shader: {name} ({})", path.display());
            Some(Cow::Owned(src))
        }
        Err(e) => {
            eprintln!("[evelyn] shader {name:?} load failed: {e}");
            None
        }
    }
}

fn user_shader_path(name: &str) -> Option<PathBuf> {
    let dir = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            PathBuf::from(xdg).join("evelyn/shaders")
        } else {
            PathBuf::from(std::env::var("HOME").ok()?).join(".config/evelyn/shaders")
        }
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".config/evelyn/shaders")
    };
    Some(if name.ends_with(".wgsl") {
        dir.join(name)
    } else {
        dir.join(format!("{name}.wgsl"))
    })
}
