use std::time::{Duration, Instant};

use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta};

use crate::term::{MouseProto, SelectionMode};

use super::App;

#[derive(Clone, Copy)]
pub(super) struct ClickRecord {
    pub line: usize,
    pub col: u16,
    pub at: Instant,
    /// 1 = single (char), 2 = double (word), 3 = triple (line). Caps at 3.
    pub count: u8,
}

/// Same-cell repeat window for promoting click count. Matches macOS Finder /
/// most terminals — long enough that a deliberate double-click registers,
/// short enough that two unrelated clicks at the same spot don't merge.
const MULTI_CLICK_TIMEOUT: Duration = Duration::from_millis(500);

impl App {
    /// Resolve the current cursor pixel position to a global (line, col).
    /// Returns None when the cursor isn't tracked or the renderer isn't up
    /// — callers in those states should bail rather than guess a position.
    pub(super) fn cursor_to_global(&self, pos: PhysicalPosition<f64>) -> Option<(usize, u16)> {
        let r = self.renderer.as_ref()?;
        let (col1, row1) = r.pixel_to_cell(pos.x, pos.y);
        // pixel_to_cell uses xterm's 1-based convention — convert to the
        // 0-based screen coords the term grid expects.
        let col = col1.saturating_sub(1);
        let row = row1.saturating_sub(1);
        let line = self.term.screen_to_global_line(row);
        Some((line, col))
    }

    pub(super) fn on_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let Some(pos) = self.cursor_pos else { return };
        let shift = self.modifiers.state().shift_key();
        let tracking = self.term.mouse_proto != MouseProto::Off;

        // xterm convention: when the foreground app has mouse tracking on,
        // forward press/release to the PTY so multiplexers (zellij/tmux) and
        // mouse-aware TUIs can do their own pane-local selection. Shift
        // overrides to fall back to our native cross-screen selection.
        if tracking && !shift {
            let button_code: u32 = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                _ => return,
            };
            let Some(r) = self.renderer.as_ref() else { return };
            let (col1, row1) = r.pixel_to_cell(pos.x, pos.y);
            match state {
                ElementState::Pressed => {
                    self.send_mouse_report(button_code, true, col1, row1);
                    if button == MouseButton::Left {
                        self.mouse_forwarding = true;
                        self.last_reported_cell = Some((col1, row1));
                    }
                }
                ElementState::Released => {
                    self.send_mouse_report(button_code, false, col1, row1);
                    if button == MouseButton::Left {
                        self.mouse_forwarding = false;
                        self.last_reported_cell = None;
                    }
                }
            }
            return;
        }

        // Native selection path — left button only.
        if button != MouseButton::Left {
            return;
        }
        let Some((line, col)) = self.cursor_to_global(pos) else { return };

        match state {
            ElementState::Pressed => {
                let now = Instant::now();
                let count = match self.last_click {
                    Some(prev)
                        if prev.line == line
                            && prev.col == col
                            && now.duration_since(prev.at) <= MULTI_CLICK_TIMEOUT =>
                    {
                        // Cap at 3 so a 4th click cycles back to single.
                        if prev.count >= 3 { 1 } else { prev.count + 1 }
                    }
                    _ => 1,
                };
                self.last_click = Some(ClickRecord { line, col, at: now, count });
                let mode = match count {
                    2 => SelectionMode::Word,
                    3 => SelectionMode::Line,
                    _ => SelectionMode::Char,
                };
                self.term.start_selection(line, col, mode);
                self.mouse_left_held = true;
                self.request_redraw();
            }
            ElementState::Released => {
                self.mouse_left_held = false;
                // A drag that produced any text → clipboard. A bare click
                // (anchor == head) is a deselect signal: clear so the
                // highlight goes away.
                if let Some(sel) = self.term.selection
                    && sel.anchor_line == sel.head_line
                    && sel.anchor_col == sel.head_col
                    && sel.mode == SelectionMode::Char
                {
                    self.term.clear_selection();
                    self.request_redraw();
                    return;
                }
                if let Some(text) = self.term.extract_selection_text() {
                    self.copy_to_clipboard(&text);
                }
            }
        }
    }

    pub(super) fn on_mouse_drag(&mut self, position: PhysicalPosition<f64>) {
        let shift = self.modifiers.state().shift_key();
        let tracking = self.term.mouse_proto != MouseProto::Off;

        // Forward motion to the PTY when the app is tracking. Button-event
        // mode (1002) reports drags while a button is held; Any-event (1003)
        // reports motion regardless. CursorMoved fires per pixel, so dedup
        // at cell granularity to avoid spamming the app.
        if tracking && !shift {
            let Some(r) = self.renderer.as_ref() else { return };
            let (col1, row1) = r.pixel_to_cell(position.x, position.y);
            if self.last_reported_cell == Some((col1, row1)) {
                return;
            }
            match self.term.mouse_proto {
                MouseProto::Button if self.mouse_forwarding => {
                    // Drag while left button held: button 0 + motion flag (32).
                    self.send_mouse_report(32, true, col1, row1);
                    self.last_reported_cell = Some((col1, row1));
                }
                MouseProto::Any => {
                    // Any-event tracking: report motion always. Button code
                    // 0 with motion if left is held, 3 (none) + motion otherwise.
                    let base = if self.mouse_forwarding { 0 } else { 3 };
                    self.send_mouse_report(base + 32, true, col1, row1);
                    self.last_reported_cell = Some((col1, row1));
                }
                _ => {}
            }
            return;
        }

        if !self.mouse_left_held {
            return;
        }
        let Some((line, col)) = self.cursor_to_global(position) else { return };
        self.term.update_selection(line, col);
        if self.term.dirty {
            self.request_redraw();
        }
    }

    pub(super) fn on_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        // Convert the wheel delta into integer line steps. winit hands us
        // either logical lines (LineDelta) or pixels (PixelDelta, common
        // on macOS trackpads); for pixels we divide by line height.
        let lines: f32 = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => {
                let lh = self
                    .renderer
                    .as_ref()
                    .map(|r| r.line_height as f64)
                    .unwrap_or(16.0);
                if lh > 0.0 {
                    (p.y / lh) as f32
                } else {
                    0.0
                }
            }
        };
        // Accumulate sub-line trackpad scrolls so a slow drag still moves.
        self.scroll_accum += lines;
        let step = self.scroll_accum.trunc() as i32;
        if step == 0 {
            return;
        }
        self.scroll_accum -= step as f32;

        // Priority order for what to do with the wheel:
        //   1. App requested mouse tracking (DECSET 1000/1002/1003) →
        //      send wheel reports. zellij/tmux/helix-with-mouse all hit
        //      this path; without it the wheel falls through to our
        //      scrollback while the app expects to handle scrolling.
        //   2. Alt screen or DECCKM (app cursor keys) → synthesize arrow
        //      keys (xterm "alternateScroll" stopgap for apps that don't
        //      enable mouse tracking but do want to consume the wheel).
        //   3. Otherwise → walk our own scrollback.
        if self.term.mouse_proto != MouseProto::Off {
            self.send_wheel_report(step);
        } else if self.term.is_alt_screen() || self.term.app_cursor_keys {
            let (byte, count) = if step > 0 {
                (b'A', step as usize)
            } else {
                (b'B', (-step) as usize)
            };
            let intro: u8 = if self.term.app_cursor_keys { b'O' } else { b'[' };
            let mut bytes = Vec::with_capacity(count * 3);
            for _ in 0..count {
                bytes.extend_from_slice(&[0x1b, intro, byte]);
            }
            if let Some(p) = &self.pty {
                p.write(&bytes);
            }
        } else {
            // Main screen → walk the scrollback view. Wheel up (+y) means
            // older content, which is +offset in our model.
            self.term.scroll_view(step);
            if self.term.dirty {
                self.request_redraw();
            }
        }
    }

    /// Encode a mouse press/release/motion event as an xterm mouse report
    /// (SGR if the app requested DECSET 1006, X10 otherwise) and write it
    /// to the PTY. `button` is the base code — caller adds the motion flag
    /// (+32) for drags and picks 0/1/2 for L/M/R or 64/65 for wheel. The
    /// shift/alt/ctrl modifier bits are OR'd in here so callers don't have
    /// to repeat the logic. `col` and `row` are 1-based xterm coords.
    fn send_mouse_report(&self, button: u32, pressed: bool, col: u16, row: u16) {
        let Some(pty) = &self.pty else { return };
        let mods = self.modifiers.state();
        let mut b = button;
        if mods.shift_key() {
            b |= 4;
        }
        if mods.alt_key() {
            b |= 8;
        }
        if mods.control_key() {
            b |= 16;
        }
        let mut bytes = Vec::new();
        if self.term.mouse_sgr {
            use std::io::Write;
            let term = if pressed { 'M' } else { 'm' };
            let _ = write!(&mut bytes, "\x1b[<{};{};{}{}", b, col, row, term);
        } else {
            // X10 releases don't carry button identity — collapse to 3.
            let raw = if pressed { b } else { (b & !0x03) | 3 };
            let cb = (32 + raw).min(255) as u8;
            let cx = (32 + col as u32).min(255) as u8;
            let cy = (32 + row as u32).min(255) as u8;
            bytes.extend_from_slice(&[0x1b, b'[', b'M', cb, cx, cy]);
        }
        pty.write(&bytes);
    }

    /// Encode `step` wheel ticks as xterm mouse reports and write them to
    /// the PTY. One report per tick (apps treat each as a discrete event;
    /// merging would lose granularity for slow wheels). Up == button 64,
    /// down == 65, both encoded as press-only (wheel has no release).
    /// Uses SGR (1006) when the app requested it, X10 otherwise — clamped
    /// to col/row 223 in X10 since the legacy form can't address beyond.
    fn send_wheel_report(&self, step: i32) {
        let Some(pty) = &self.pty else { return };
        let (col, row) = self
            .cursor_pos
            .and_then(|p| self.renderer.as_ref().map(|r| r.pixel_to_cell(p.x, p.y)))
            .unwrap_or((1, 1));
        let button: u32 = if step > 0 { 64 } else { 65 };
        let count = step.unsigned_abs() as usize;
        let mut bytes = Vec::new();
        for _ in 0..count {
            if self.term.mouse_sgr {
                use std::io::Write;
                let _ = write!(&mut bytes, "\x1b[<{};{};{}M", button, col, row);
            } else {
                let cb = (32 + button).min(255) as u8;
                let cx = (32 + col as u32).min(255) as u8;
                let cy = (32 + row as u32).min(255) as u8;
                bytes.extend_from_slice(&[0x1b, b'[', b'M', cb, cx, cy]);
            }
        }
        pty.write(&bytes);
    }
}
