//! Ominiforge App — GPUI desktop application entry point.
//!
//! Phase 3.2: minimal window that renders the status bar and proves keyboard
//! input routing works. See doc/gpui-app.md.

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size,
};
use ominiforge_ui::status_bar::{CycleMode, StatusBar};

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("tab", CycleMode, None)]);

        let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(StatusBar::new),
        )
        .unwrap_or_else(|e| panic!("failed to open window: {e}"));

        cx.activate(true);
    });
}
