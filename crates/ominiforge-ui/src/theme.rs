//! Theme tokens: the single source of truth for all visual **values**
//! (colors, type, spacing, radii) in the GPUI client.
//!
//! The *principles* these values serve — single scarce accent, surface
//! ladder, state color redundancy, anti-slop — live in `doc/gpui-design.md`
//! (rules 12/13: doc = intent, code = values). Panels reference these tokens
//! by **role**, never raw hex/px, so a retheme is a one-struct change.
//!
//! `Theme` is a process-wide gpui global: set once at app startup via
//! [`Theme::set_global`], read anywhere with `cx.global::<Theme>()`. Panels
//! never own a copy.
//!
//! 🔴 **Iron rule (doc/gpui-design.md §2)**: component code must **never contain literal
//! color values** (`rgb(...)` / `rgba(...)` / `hsla(...)` / `#...`). When a color is
//! needed, first add a **semantically-named field** here (describing its role), then
//! reference the field — never write a literal in a panel/component. This is enforced by
//! a CI grep check (`just design-lint`), not by convention. Name new tokens by role (not
//! appearance) and register their usage in the `doc/gpui-design.md` §2 cheat sheet.

use gpui::{App, Global, Hsla, rgba};

/// Semantic design tokens (dark theme is the default; `doc/gpui-design.md` §2).
///
/// Field names mirror the design doc's semantic token names 1:1. Every color
/// is a role; the hex below is the dark-theme value for that role.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // -- Surface ladder (doc/gpui-design.md §2.1) --
    /// Main background.
    pub canvas_base: Hsla,
    /// Sidebar / topbar / input region.
    pub canvas_raised: Hsla,
    /// Cards / input boxes.
    pub canvas_overlay: Hsla,
    /// Code blocks / topmost floating layer.
    pub canvas_float: Hsla,

    // -- Border ladder --
    /// Dividers, default card borders.
    pub border_subtle: Hsla,
    /// Interactive borders.
    pub border_default: Hsla,
    /// Hover / focus.
    pub border_strong: Hsla,

    // -- Text ladder --
    /// Body text.
    pub text_primary: Hsla,
    /// Secondary text.
    pub text_secondary: Hsla,
    /// Labels / timestamps.
    pub text_tertiary: Hsla,
    /// Placeholder / faintest.
    pub text_disabled: Hsla,

    // -- Accent (scarce: one primary action per screen) --
    /// The single primary-action accent.
    pub accent: Hsla,
    /// Accent pressed/hover.
    pub accent_hover: Hsla,
    /// Faint accent wash (selected backgrounds, user bubble tint).
    pub accent_dim: Hsla,
    /// Accent as text on dark (links, current state).
    pub accent_ink: Hsla,

    // -- State (done / running / error, color+shape redundancy) --
    /// Tool settled OK.
    pub state_done: Hsla,
    /// Tool running.
    pub state_running: Hsla,
    /// Tool failed / error text.
    pub state_error: Hsla,

    // -- Reasoning (deliberately de-emphasized cool tone) --
    /// Reasoning block text.
    pub reasoning_text: Hsla,

    // -- User bubble --
    /// User-turn text accent.
    pub user_text: Hsla,
}

impl Theme {
    /// The default dark theme (charcoal canvas ladder + a single acid-lime
    /// accent, per `doc/gpui-design.md`).
    #[must_use]
    pub fn dark() -> Self {
        fn hex(v: u32) -> Hsla {
            rgba(v).into()
        }
        Self {
            canvas_base: hex(0x0e0e_10ff),
            canvas_raised: hex(0x1414_16ff),
            canvas_overlay: hex(0x1a1a_1eff),
            canvas_float: hex(0x2222_28ff),

            border_subtle: hex(0xffff_ff0e),
            border_default: hex(0xffff_ff17),
            border_strong: hex(0xffff_ff29),

            text_primary: hex(0xf0f0_f2ff),
            text_secondary: hex(0x8a8a_9aff),
            text_tertiary: hex(0x5c5c_6eff),
            text_disabled: hex(0x3a3a_48ff),

            accent: hex(0xc6f1_35ff),
            accent_hover: hex(0xd4ff_3dff),
            accent_dim: hex(0xc6f1_350f),
            accent_ink: hex(0xc6f1_35ff),

            state_done: hex(0x3d9b_5cff),
            state_running: hex(0xe8a8_38ff),
            state_error: hex(0xe052_52ff),

            reasoning_text: hex(0x7878_b0ff),

            user_text: hex(0xc6f1_35ff),
        }
    }

    /// Install this theme as the process-wide global. Call once at startup.
    pub fn set_global(self, cx: &mut App) {
        cx.set_global(self);
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Global for Theme {}
