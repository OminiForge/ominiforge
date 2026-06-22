//! The AI status line shown below the input box: model, a context-usage gauge,
//! and the scroll-follow indicator.
//!
//! The gauge is computed against the model's *actual* context window (whatever
//! the resolved model reports — 200k, 400k, 1M, …), never a hardcoded value.
//! The compaction threshold is drawn as a tick on the bar so "how close am I to
//! a compaction" is visible at a glance, and the fill is color-graded so it
//! turns yellow then red as it approaches that line.

use ratatui::style::Color;
use ratatui::text::{Line, Span};

use super::theme;

/// Width of the gauge bar in cells.
const BAR_WIDTH: usize = 16;

/// The scroll-follow state, surfaced so the user knows whether they are seeing
/// the newest output.
#[derive(Clone, Copy)]
pub enum Follow {
    /// Pinned to the bottom — newest output is visible.
    AtBottom,
    /// Scrolled up while a turn streams — there is unseen new output below.
    NewOutput,
    /// Scrolled up and idle — just not at the bottom.
    ScrolledUp,
}

/// Render the AI status line: `model · used/window [gauge] pct` plus, when not
/// following, a right-aligned scroll hint.
#[must_use]
pub fn render(
    model: &str,
    used: u32,
    window: u32,
    threshold: f32,
    follow: Follow,
) -> Line<'static> {
    let mut spans = vec![Span::styled(format!(" {model}"), theme::fg(theme::tool()))];
    spans.push(Span::styled("  ·  ", theme::fg_dim(theme::dim())));

    if window == 0 {
        // Unknown window: just show the running estimate, no gauge.
        spans.push(Span::styled(
            format!("~{} tokens", compact(used)),
            theme::fg_dim(theme::dim()),
        ));
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pct = (f64::from(used) / f64::from(window) * 100.0).round() as u32;
        spans.push(Span::styled(
            format!("{}/{} ", compact(used), compact(window)),
            theme::fg_dim(theme::tool_result()),
        ));
        spans.extend(gauge(used, window, threshold));
        spans.push(Span::styled(
            format!(" {pct}%"),
            theme::fg(usage_color(used, window, threshold)),
        ));
    }

    if let Some(hint) = follow_hint(follow) {
        spans.push(Span::styled("        ", theme::fg_dim(theme::dim())));
        spans.push(hint);
    }

    Line::from(spans)
}

/// The scroll hint span, or `None` when following the bottom.
fn follow_hint(follow: Follow) -> Option<Span<'static>> {
    match follow {
        Follow::AtBottom => None,
        Follow::NewOutput => Some(Span::styled(
            "▼ new output · End to follow",
            theme::fg(theme::warn()),
        )),
        Follow::ScrolledUp => Some(Span::styled(
            "▼ scrolled up · End to bottom",
            theme::fg_dim(theme::dim()),
        )),
    }
}

/// Build the bracketed gauge `▕████░░╎░░▏`, with a `╎` tick at the compaction
/// threshold and the fill colored by how close usage is to that threshold.
fn gauge(used: u32, window: u32, threshold: f32) -> Vec<Span<'static>> {
    let frac = (f64::from(used) / f64::from(window)).clamp(0.0, 1.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = (frac * BAR_WIDTH as f64).round() as usize;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let tick = ((f64::from(threshold) * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    let color = usage_color(used, window, threshold);

    let mut bar = String::with_capacity(BAR_WIDTH + 2);
    bar.push('▕');
    for i in 0..BAR_WIDTH {
        if i == tick {
            bar.push('╎'); // the compaction-threshold marker
        } else if i < filled {
            bar.push('█');
        } else {
            bar.push('░');
        }
    }
    bar.push('▏');

    vec![Span::styled(bar, theme::fg(color))]
}

/// Color the usage by fraction of the *window*: green under 60%, yellow up to
/// the compaction threshold, red at or past it.
fn usage_color(used: u32, window: u32, threshold: f32) -> Color {
    let frac = f64::from(used) / f64::from(window);
    if frac >= f64::from(threshold) {
        theme::error()
    } else if frac >= 0.6 {
        theme::warn()
    } else {
        theme::ok()
    }
}

/// Compact a token count: `1234` → `1.2k`, `1_000_000` → `1.0M`.
fn compact(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", f64::from(n) / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", f64::from(n) / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Token counts compact to k/M so the gauge label stays short regardless of
    /// the window size.
    #[test]
    fn compact_formats_k_and_m() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(12_000), "12k");
        assert_eq!(compact(1_000_000), "1.0M");
    }

    /// Usage color escalates green → yellow → red as it crosses 60% and then the
    /// compaction threshold, so the user sees the approach to compaction.
    #[test]
    fn usage_color_escalates_with_fraction() {
        let window = 1000;
        let threshold = 0.8;
        assert_eq!(usage_color(100, window, threshold), theme::ok());
        assert_eq!(usage_color(700, window, threshold), theme::warn());
        assert_eq!(usage_color(850, window, threshold), theme::error());
    }

    /// The gauge places the threshold tick proportionally and the fill tracks
    /// usage. The denominator is the window, so the same usage on a bigger
    /// window fills less — verifying the bar is not hardcoded to any size.
    #[test]
    fn gauge_fill_scales_with_window() {
        let small: String = gauge(500, 1000, 0.8)
            .iter()
            .flat_map(|s| s.content.chars())
            .collect();
        let big: String = gauge(500, 10_000, 0.8)
            .iter()
            .flat_map(|s| s.content.chars())
            .collect();
        let count = |s: &str| s.chars().filter(|c| *c == '█').count();
        assert!(
            count(&small) > count(&big),
            "same usage fills more of a smaller window: {small} vs {big}"
        );
        // The threshold tick is present in both.
        assert!(small.contains('╎') && big.contains('╎'));
    }

    /// The follow hint is absent at the bottom and present (distinct messages)
    /// when scrolled up streaming vs idle — the at-bottom vs not distinction the
    /// user asked for.
    #[test]
    fn follow_hint_reflects_state() {
        assert!(follow_hint(Follow::AtBottom).is_none());
        let new = follow_hint(Follow::NewOutput).unwrap();
        let up = follow_hint(Follow::ScrolledUp).unwrap();
        assert!(new.content.contains("new output"));
        assert!(up.content.contains("scrolled up"));
    }
}
