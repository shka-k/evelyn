use std::time::Instant;

use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{Ime, KeyEvent};

use crate::config::config;
use crate::input::encode_key;

use super::{App, IME_CANDIDATE_WIDTH_CELLS};

impl App {
    pub(super) fn on_keyboard_input(&mut self, event: KeyEvent, is_synthetic: bool) {
        if is_synthetic {
            return;
        }
        // While IME is composing, suppress key→PTY translation; the IME
        // delivers the result via Ime::Commit.
        if !self.preedit.is_empty() {
            return;
        }
        if let Some(bytes) = encode_key(&event, &self.modifiers, self.term.app_cursor_keys) {
            // Snap the scrollback view back to the live bottom on user
            // input — matches every other terminal: typing pulls you
            // out of history.
            if self.term.view_offset != 0 {
                self.term.reset_view();
                self.request_redraw();
            }
            // Typing past a selection makes the highlight stale (the user
            // is editing the next prompt, not interacting with the picked
            // range). Clear it so the redraw reflects the new state.
            if self.term.selection.is_some() {
                self.term.clear_selection();
                self.request_redraw();
            }
            self.poke_cursor_blink();
            if let Some(p) = &self.pty {
                p.write(&bytes);
            }
        }
    }

    pub(super) fn on_ime(&mut self, ime: Ime) {
        match ime {
            Ime::Enabled | Ime::Disabled => {
                self.preedit.clear();
                self.preedit_cursor = 0;
            }
            Ime::Preedit(text, cursor) => {
                // winit hands the caret as a byte range `(start, end)`.
                // Use `end` as a single insertion point — IMEs that report a
                // collapsed selection set start == end, and for non-empty
                // selections placing the cursor at the trailing edge is the
                // conventional spot. None → end of preedit.
                let caret = cursor.map(|(_, e)| e).unwrap_or(text.len());
                self.preedit_cursor = caret.min(text.len());
                self.preedit = text;
                self.update_ime_cursor_area();
            }
            Ime::Commit(text) => {
                self.preedit.clear();
                self.preedit_cursor = 0;
                if !text.is_empty()
                    && let Some(p) = &self.pty
                {
                    p.write(text.as_bytes());
                }
            }
        }
        self.request_redraw();
    }

    pub(super) fn update_ime_cursor_area(&self) {
        let (Some(w), Some(r)) = (self.window.as_ref(), self.renderer.as_ref()) else {
            return;
        };
        let scale = w.scale_factor();
        let pad = config().window.padding as f64;
        let x = self.term.cur_x as f64 * r.cell_width as f64 / scale + pad;
        let y = (self.term.cur_y as f64 * r.line_height as f64 + r.line_height as f64) / scale + pad;
        let cell_w = r.cell_width as f64 / scale;
        let cell_h = r.line_height as f64 / scale;
        w.set_ime_cursor_area(
            LogicalPosition::new(x, y),
            LogicalSize::new(cell_w * IME_CANDIDATE_WIDTH_CELLS, cell_h),
        );
    }

    /// Snap the blink to "on" and reset the half-period timer. Called on
    /// any input/output activity so the cursor stays solid while typing
    /// and resumes blinking only after a full quiet interval — same UX
    /// xterm and friends use.
    pub(super) fn poke_cursor_blink(&mut self) {
        self.cursor_blink_on = true;
        self.cursor_blink_last_toggle = Instant::now();
    }
}
