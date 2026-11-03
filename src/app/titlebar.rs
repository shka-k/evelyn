//! Keep the macOS title bar opaque while the window itself is
//! non-opaque (for `window.opacity < 1.0`).
//!
//! A transparent winit window has a clear `NSWindow.backgroundColor`.
//! Older macOS drew the title bar with its own material, but newer
//! releases draw it over the window background, so it turns see-through
//! along with the content. We put an opaque `NSBox` filled with the
//! system window background color behind the title bar's own subviews
//! (traffic lights, title), leaving the content area translucent.

use winit::window::Window;

#[cfg(target_os = "macos")]
pub(super) fn install_opaque_titlebar(window: &Window) {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSBox, NSBoxType, NSColor, NSTitlePosition, NSView,
        NSWindowButton, NSWindowOrderingMode,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit guarantees `ns_view` is a live NSView for the lifetime
    // of the window, and we're on the main thread.
    let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
    let Some(ns_window) = view.window() else {
        return;
    };
    // The close button lives directly inside the private NSTitlebarView.
    let Some(close) = ns_window.standardWindowButton(NSWindowButton::CloseButton) else {
        return;
    };
    let Some(titlebar) = (unsafe { close.superview() }) else {
        return;
    };

    let bg: Retained<NSBox> = NSBox::initWithFrame(NSBox::alloc(mtm), titlebar.bounds());
    bg.setBoxType(NSBoxType::Custom);
    bg.setTitlePosition(NSTitlePosition::NoTitle);
    bg.setBorderWidth(0.0);
    bg.setFillColor(&NSColor::windowBackgroundColor());
    bg.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    titlebar.addSubview_positioned_relativeTo(&bg, NSWindowOrderingMode::Below, None);
}

#[cfg(not(target_os = "macos"))]
pub(super) fn install_opaque_titlebar(_window: &Window) {}
