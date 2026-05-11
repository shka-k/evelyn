# Evelyn

A GPU-accelerated terminal emulator for macOS, written in Rust. Wgpu for
rendering, a CRT post-processing shader by default, hot-reloadable config,
and an Alacritty-compatible theme schema.

## Features

- **GPU rendering** via `wgpu` (Metal on macOS) with `glyphon` for text shaping.
- **CRT shader** post-pass (`newpixie-crt`) toggleable in config; drop your
  own `.wgsl` into `~/.config/evelyn/shaders/` to use it.
- **Themes** — built-ins (`default`, `tokyo-night`, `dracula`, `nord`,
  `gruvbox-dark`, `catppuccin-mocha`) plus any Alacritty `[colors.*]` TOML
  file under `~/.config/evelyn/themes/`. You can symlink your existing
  `~/.config/alacritty/themes/` directory in directly.
- **Hot reload** — edits to `config.toml` or the active theme file are
  picked up live (FSEvents watcher).
- **Ligatures** via OpenType `liga` / `clig` / `calt` / `dlig` (configurable).
- **Mouse selection** with drag-to-select and auto-copy / Cmd+C.
- **Bracketed paste** on Cmd+V (paste-end markers in the payload are stripped).
- **Configurable cursor** — block / underline / bar / hollow, optional blink.
- **xterm-compatible key encoding** including DECCKM, modifier codes for
  cursor / nav / F-keys, and Cmd folded into Alt as a meta modifier.

## Build

Requires Rust (stable). The repo pins toolchain via `mise.toml`.

```sh
cargo run --release          # run directly
make app                     # build build/Evelyn.app
make app ARCH=both           # universal (x86_64 + aarch64) binary
make install                 # copy to /Applications
make dmg                     # build a draggable .dmg
```

`make app` codesigns ad-hoc so Gatekeeper lets the bundle launch from
`/Applications` on the same machine.

## Configuration

Search order:

1. `$EVELYN_CONFIG`
2. `$XDG_CONFIG_HOME/evelyn/config.toml`
3. `~/.config/evelyn/config.toml`

A missing file falls back to defaults silently; parse errors are logged
and the previous value is kept.

```toml
# ~/.config/evelyn/config.toml
theme = "tokyo-night"        # built-in name or file under themes/
shell = "/opt/homebrew/bin/fish"   # default: $SHELL, then /bin/bash

[font]
family = "Geist Mono"        # falls back to bundled Geist Mono Nerd Font
size_pt = 14.0
line_height_factor = 1.3
ligatures = true

[window]
padding = 8.0                # logical points, all four sides

[shader]
enabled = true
effect = "newpixie-crt"      # built-in or filename under shaders/

[cursor]
shape = "block"              # "block" | "underline" | "bar" | "hollow"
blink = false
blink_interval_ms = 530
```

### Themes

Drop an Alacritty-format theme file at
`~/.config/evelyn/themes/<name>.toml` and reference it as
`theme = "<name>"`. Built-in names take precedence over files.

```toml
# ~/.config/evelyn/themes/my-theme.toml
[colors.primary]
background = "#1a1b26"
foreground = "#c0caf5"

[colors.cursor]
cursor = "#c0caf5"
text   = "#1a1b26"

[colors.normal]
black = "#15161e"
red   = "#f7768e"
# … green / yellow / blue / magenta / cyan / white

[colors.bright]
# … same shape as [colors.normal]
```

## Key bindings

| Key       | Action                                       |
| --------- | -------------------------------------------- |
| `Cmd+C`   | Copy selection (when a selection is active)  |
| `Cmd+V`   | Paste (bracketed when the app supports it)   |
| `Cmd+N`   | Open a new window (spawns a new process)     |
| `Cmd+R`   | Force-reload config + theme                  |
| `Cmd+W`   | Quit                                         |

Drag with the left mouse button to select; the selection is also copied
to the system clipboard on release.

## Layout

```
src/
  app.rs          # winit event loop, window + clipboard glue
  config.rs       # config + theme loading, hot reload, Alacritty schema
  input.rs        # winit KeyEvent → PTY byte encoding (xterm-compatible)
  pty.rs          # portable-pty wrapper
  render/         # wgpu pipelines, glyph atlas, CRT post pass
  term/           # vte parser → cell grid + state
  themes.rs       # built-in palettes
```

## License

Unspecified — add a `LICENSE` file before distributing.
