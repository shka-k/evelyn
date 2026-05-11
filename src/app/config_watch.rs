use std::path::PathBuf;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::config;

use super::{App, UserEvent};

impl App {
    /// Re-read config + theme files and propagate the change to the
    /// renderer + grid. Coalesces bursty filesystem events so editors
    /// that write atomically (rename-into-place) don't trigger 3-4
    /// reloads in a row.
    pub(super) fn on_config_reload(&mut self) {
        const DEBOUNCE: Duration = Duration::from_millis(50);
        if let Some(prev) = self.last_reload
            && prev.elapsed() < DEBOUNCE
        {
            return;
        }
        self.last_reload = Some(Instant::now());

        let snap = config::reload();
        // If the active theme file changed (built-in → file, or vice
        // versa, or a different filename), refresh the watcher subscription
        // so the new path is the one we get notified about.
        if snap.cfg.theme != snap.prev_cfg.theme {
            self.respawn_config_watcher();
        }
        eprintln!("[evelyn] config reloaded");
        if let Some(r) = self.renderer.as_mut() {
            let cell_changed = r.reload_from_config();
            if cell_changed {
                self.sync_grid();
            }
        }
        self.request_redraw();
    }

    /// Spawn a `notify` watcher subscribed to the config + theme paths.
    /// macOS FSEvents fires on any change in the parent dir, but we filter
    /// down to just our two files so unrelated edits in `~/.config/evelyn/`
    /// don't cause needless reloads.
    pub(super) fn respawn_config_watcher(&mut self) {
        let cfg_path = config::config_file_path();
        let theme_path = config::theme_file_path();
        let watch_paths: Vec<PathBuf> = cfg_path.iter().chain(theme_path.iter()).cloned().collect();
        if watch_paths.is_empty() {
            self._config_watcher = None;
            return;
        }
        let proxy = self.proxy.clone();
        let watch_set = watch_paths.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // FSEvents on macOS coalesces by path-prefix; be strict that one
            // of the watched files is actually in the event's path list.
            if !event.paths.iter().any(|p| watch_set.iter().any(|w| p == w)) {
                return;
            }
            let _ = proxy.send_event(UserEvent::ConfigReload);
        });
        let mut watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[evelyn] config watcher init failed: {e}");
                self._config_watcher = None;
                return;
            }
        };
        // Watch the parent dir non-recursively. Watching the file directly
        // misses atomic-rename saves (the inode swaps under us), and FSEvents
        // is per-directory anyway.
        let mut watched_any = false;
        for p in watch_paths.iter().filter_map(|p| p.parent()) {
            if let Err(e) = watcher.watch(p, RecursiveMode::NonRecursive) {
                eprintln!("[evelyn] watcher.watch({}) failed: {e}", p.display());
            } else {
                watched_any = true;
            }
        }
        if watched_any {
            self._config_watcher = Some(watcher);
        } else {
            self._config_watcher = None;
        }
    }
}
