//! Catppuccin Mocha palette mapped to semantic UI roles.
//!
//! The TUI never names raw colors; it asks for a *role* (answer text, thinking,
//! a tool name, an error, …) and gets the matching Mocha color. Keeping the
//! mapping in one place means a future theme switch (Latte, Frappé, …) is a
//! single edit here, and the rest of the UI stays palette-agnostic.
//!
//! Colors come from the `catppuccin` crate's palette data. We read each color's
//! `rgb` channels and build a truecolor [`Color::Rgb`] — the project targets
//! modern terminals (no 8/16-color fallback), so 24-bit color is assumed.

use catppuccin::PALETTE;
use ratatui::style::{Color, Modifier, Style};

/// Convert a Catppuccin palette color into a ratatui truecolor.
///
/// (The crate's own `ratatui` feature targets a different `ratatui-core`
/// version than ratatui 0.29 bundles, so we map the rgb channels by hand.)
const fn rgb(c: catppuccin::Color) -> Color {
    Color::Rgb(c.rgb.r, c.rgb.g, c.rgb.b)
}

/// The active flavor's colors. Mocha is the darkest dark flavor; switching the
/// whole theme is a one-line change here.
macro_rules! mocha {
    ($name:ident) => {
        rgb(PALETTE.mocha.colors.$name)
    };
}

/// Default body / assistant answer text.
pub const fn text() -> Color {
    mocha!(text)
}

/// The user's own messages (their prompt echoed into the conversation).
pub const fn user() -> Color {
    mocha!(mauve)
}

/// Reasoning / thinking text, shown dim while it streams.
pub const fn thinking() -> Color {
    mocha!(overlay0)
}

/// Markdown headings inside an answer. Distinct from [`user`] (mauve) so a
/// heading the assistant wrote is not mistaken for an echoed user prompt.
pub const fn heading() -> Color {
    mocha!(lavender)
}

/// Inline code and fenced code blocks. A own role (not [`thinking`]) so code is
/// not styled the same as reasoning text.
pub const fn code() -> Color {
    mocha!(peach)
}

/// The assistant's answer gutter marker, mirroring the user's `›` prefix so the
/// left edge reads as "who is speaking".
pub const fn assistant() -> Color {
    mocha!(green)
}

/// A tool name and its card border.
pub const fn tool() -> Color {
    mocha!(blue)
}

/// A successful tool result marker (✓).
pub const fn ok() -> Color {
    mocha!(green)
}

/// Tool result body text (dimmer than the answer, indented under the call).
pub const fn tool_result() -> Color {
    mocha!(subtext0)
}

/// Warnings and neutral system notices (e.g. a compaction notice).
pub const fn warn() -> Color {
    mocha!(yellow)
}

/// Errors (failed turn, failed tool).
pub const fn error() -> Color {
    mocha!(red)
}

/// Low-emphasis chrome: borders, the footer, separators.
pub const fn dim() -> Color {
    mocha!(surface2)
}

/// A foreground style for `color`.
pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

/// A dim foreground style for `color` (used for thinking / chrome).
pub fn fg_dim(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::DIM)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each role resolves to a concrete truecolor (no accidental shared/Reset
    /// color), and the roles that must differ visually actually differ — a
    /// regression guard so "thinking looks like answer text" can't creep back.
    #[test]
    fn roles_are_distinct_truecolors() {
        for c in [text(), user(), thinking(), tool(), ok(), error(), warn()] {
            assert!(matches!(c, Color::Rgb(..)), "expected truecolor, got {c:?}");
        }
        assert_ne!(text(), thinking(), "answer vs thinking must differ");
        assert_ne!(text(), user(), "answer vs user must differ");
        assert_ne!(tool(), error(), "tool vs error must differ");
        // A markdown heading must not reuse the user color, or an assistant
        // heading reads as an echoed user prompt (problem 3).
        assert_ne!(heading(), user(), "heading vs user must differ");
        // Code must not reuse the thinking color, or code blocks look like
        // reasoning text.
        assert_ne!(code(), thinking(), "code vs thinking must differ");
    }
}
