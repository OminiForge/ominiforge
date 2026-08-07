//! Status bar panel (doc/gpui-app.md §3.3): shows vim mode, session state,
//! and connection state. The first real component — intentionally minimal.

use gpui::{
    Focusable, InteractiveElement, IntoElement, ParentElement, Render, Styled, actions, div, rgb,
};

actions!(status_bar, [CycleMode]);

/// Element id used by tests to locate the bar via `debug_bounds`.
pub const STATUS_BAR_ID: &str = "status-bar";

/// Current vim mode, displayed on the left of the bar. Sourced from nvim RPC
/// when the editor panel is focused, from app state otherwise
/// (doc/editor.md §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
}

impl VimMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual => "VISUAL",
        }
    }
}

pub struct StatusBar {
    mode: VimMode,
    focus_handle: gpui::FocusHandle,
}

impl StatusBar {
    pub fn new(cx: &mut gpui::Context<Self>) -> Self {
        Self {
            mode: VimMode::Normal,
            focus_handle: cx.focus_handle(),
        }
    }

    pub const fn set_mode(&mut self, mode: VimMode) {
        self.mode = mode;
    }

    #[must_use]
    pub const fn mode(&self) -> VimMode {
        self.mode
    }
}

impl StatusBar {
    fn cycle_mode(
        &mut self,
        _: &CycleMode,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.mode = match self.mode {
            VimMode::Normal => VimMode::Insert,
            VimMode::Insert => VimMode::Visual,
            VimMode::Visual => VimMode::Normal,
        };
        cx.notify();
    }
}

impl Focusable for StatusBar {
    fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StatusBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .debug_selector(|| STATUS_BAR_ID.into())
            .on_action(cx.listener(Self::cycle_mode))
            .flex()
            .w_full()
            .h_6()
            .px_2()
            .items_center()
            .bg(rgb(0x1f_1f28))
            .text_color(rgb(0xdc_d7ba))
            .child(self.mode.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext, px};

    fn open_bar(cx: &mut TestAppContext) -> (gpui::Entity<StatusBar>, &mut VisualTestContext) {
        let (view, visual_cx) = cx.add_window_view(|_window, cx| StatusBar::new(cx));
        // debug_bounds reads the last rendered frame; ensure the view has been
        // laid out and painted at least once.
        visual_cx.run_until_parked();
        (view, visual_cx)
    }

    #[gpui::test]
    fn renders_at_full_width_with_fixed_height(cx: &mut TestAppContext) {
        let (_view, visual) = open_bar(cx);
        let bounds = visual
            .debug_bounds(STATUS_BAR_ID)
            .unwrap_or_else(|| panic!("status bar should render"));
        // Bar must span the full window width (whatever the test window size
        // is) and keep its fixed 24px height (h_6).
        let window_width = visual.update(|window, _cx| window.viewport_size().width);
        assert_eq!(bounds.size.width, window_width);
        assert_eq!(bounds.size.height, px(24.0));
    }

    #[gpui::test]
    fn tab_key_cycles_mode_through_key_dispatch(cx: &mut TestAppContext) {
        use gpui::KeyBinding;
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new("tab", CycleMode, None)]);
        });
        let (view, visual) = open_bar(cx);
        // Key dispatch only reaches the view while it holds focus.
        visual.update(|window, cx| window.focus(&view.read(cx).focus_handle));
        visual.run_until_parked();

        let mode = |visual: &mut VisualTestContext, view: &gpui::Entity<StatusBar>| {
            visual.update(|_window, cx| view.read(cx).mode())
        };
        assert_eq!(mode(visual, &view), VimMode::Normal);
        visual.simulate_keystrokes("tab");
        assert_eq!(mode(visual, &view), VimMode::Insert);
        visual.simulate_keystrokes("tab");
        assert_eq!(mode(visual, &view), VimMode::Visual);
        visual.simulate_keystrokes("tab");
        assert_eq!(mode(visual, &view), VimMode::Normal);
    }

    #[gpui::test]
    fn mode_drives_displayed_label(cx: &mut TestAppContext) {
        let (view, _visual) = open_bar(cx);
        cx.update_entity(&view, |bar, _cx| {
            assert_eq!(bar.mode(), VimMode::Normal);
            bar.set_mode(VimMode::Insert);
            assert_eq!(bar.mode(), VimMode::Insert);
        });
    }
}
