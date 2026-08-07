//! Ominiforge UI — GPUI component library (theme, components, panels).
//!
//! See `doc/gpui-app.md`.

#![allow(missing_docs)]
// gpui's actions! macro derives PartialEq without Eq; can't fix upstream.
#![allow(clippy::derive_partial_eq_without_eq)]

pub mod status_bar;
