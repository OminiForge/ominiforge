//! The conversation model: a list of typed [`Block`]s and how they render.
//!
//! The old design flattened everything into `Vec<String>` and `join`ed it into
//! one paragraph at draw time, which threw away *what each line was* — so
//! thinking, answers, and tool calls all rendered as the same gray text, and
//! the scroll offset (computed from logical line counts) didn't match the
//! wrapped visual lines ratatui actually drew.
//!
//! Here every block keeps its kind. Rendering turns each block into styled
//! [`Line`]s (Catppuccin-colored per role, tool calls drawn as omp-style
//! cards), and thinking blocks fold to a one-line summary once they finish
//! streaming. The caller counts the rendered lines for an accurate scroll.

use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme;

/// How a finished tool call turned out, for the card's status glyph.
pub enum ToolStatus {
    /// Still running (no result yet).
    Running,
    /// Completed; `error` distinguishes ok from a tool-reported error.
    Done { error: bool, summary: String },
    /// Failed to execute at all (transport/timeout), with a message.
    Failed(String),
}

/// One renderable unit of the conversation.
pub enum Block {
    /// The user's message.
    User(String),
    /// Assistant answer text. Rendered as plain text while it streams
    /// (`done = false`) and re-rendered as markdown once the block completes
    /// (`done = true`) — partial markdown mid-stream would flicker.
    Answer { text: String, done: bool },
    /// Reasoning. Folds to a one-line summary once `done` is set.
    Thinking {
        text: String,
        /// When the block opened, to report how long the model thought.
        started: Instant,
        /// Set when streaming ends; carries the elapsed time and collapses it.
        elapsed: Option<Duration>,
    },
    /// A tool call rendered as a card: name, streamed arguments, and status.
    Tool {
        name: String,
        args: String,
        status: ToolStatus,
    },
    /// A neutral system notice (e.g. an auto-compaction message).
    Note(String),
    /// An error line.
    Error(String),
    /// The per-turn footer (rounds / tokens / context usage).
    Summary(String),
    /// A separator between a resumed history and the live continuation.
    Separator(String),
}

impl Block {
    /// Render this block to styled lines for the conversation pane.
    pub fn render(&self) -> Vec<Line<'static>> {
        match self {
            Self::User(text) => prefixed(
                text,
                "› ",
                theme::fg(theme::user()).add_modifier(Modifier::BOLD),
            ),
            Self::Answer { text, done } => render_answer(text, *done),
            Self::Thinking { text, elapsed, .. } => render_thinking(text, *elapsed),
            Self::Tool { name, args, status } => render_tool(name, args, status),
            Self::Note(msg) => vec![Line::styled(format!("◈ {msg}"), theme::fg(theme::warn()))],
            Self::Error(msg) => vec![Line::styled(
                format!("✗ {msg}"),
                theme::fg(theme::error()).add_modifier(Modifier::BOLD),
            )],
            Self::Summary(msg) => vec![Line::styled(msg.clone(), theme::fg_dim(theme::dim()))],
            Self::Separator(msg) => vec![Line::styled(
                format!("── {msg} ──"),
                theme::fg_dim(theme::dim()),
            )],
        }
    }
}

/// Render an answer block.
///
/// Markdown is rendered *as it streams*, not only when the block completes:
/// markdown is block-structured, so any text before the last blank line that is
/// not inside an open code fence will never change again and can be rendered as
/// markdown without flicker. Only the still-growing tail (the current paragraph
/// or an unterminated fence) stays plain until it settles. The whole block is
/// prefixed with an assistant gutter marker so the left edge reads as "who is
/// speaking", mirroring the user's `›`.
fn render_answer(text: &str, done: bool) -> Vec<Line<'static>> {
    let plain_style = theme::fg(theme::text());
    let body = if done {
        render_markdown(text)
    } else {
        let commit = stable_boundary(text);
        let mut lines = render_markdown(&text[..commit]);
        lines.extend(wrapped(&text[commit..], plain_style));
        lines
    };
    with_gutter(body, "⏺ ", theme::fg(theme::assistant()))
}

/// The byte offset up to which `text` is "settled" markdown: the position just
/// after the most recent blank line that sits *outside* a code fence. Text
/// before this can be rendered as markdown without ever changing; text after is
/// the in-progress block and stays plain until a blank line commits it.
fn stable_boundary(text: &str) -> usize {
    let mut offset = 0;
    let mut in_fence = false;
    let mut commit = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
        } else if trimmed.is_empty() && !in_fence {
            commit = offset + line.len();
        }
        offset += line.len();
    }
    commit
}

/// Prefix a rendered block with a gutter `marker` on the first line and an equal
/// indent on the rest, so multi-line content aligns under the marker.
fn with_gutter(lines: Vec<Line<'static>>, marker: &str, marker_style: Style) -> Vec<Line<'static>> {
    let indent = " ".repeat(marker.chars().count());
    if lines.is_empty() {
        return vec![Line::from(Span::styled(marker.to_owned(), marker_style))];
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let lead = if i == 0 {
                Span::styled(marker.to_owned(), marker_style)
            } else {
                Span::styled(indent.clone(), marker_style)
            };
            let mut spans = vec![lead];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Render markdown text to styled lines.
///
/// We implement only the common cases (headings, code fences, bullet lists,
/// inline code, bold, italic) rather than pulling in a crate whose types
/// don't match ratatui 0.29's `Style`.
fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let plain_style = theme::fg(theme::text());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let code_style = theme::fg(theme::code());

    for raw in text.split('\n') {
        if raw.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::styled(raw.to_owned(), code_style));
            continue;
        }
        if in_code_block {
            lines.push(Line::styled(raw.to_owned(), code_style));
            continue;
        }
        // Headings
        let (level, rest) = heading_level(raw);
        if level > 0 {
            lines.push(Line::styled(
                rest.to_owned(),
                theme::fg(theme::heading()).add_modifier(Modifier::BOLD),
            ));
            continue;
        }
        // Bullet lists
        if let Some(item) = raw.strip_prefix("- ").or_else(|| raw.strip_prefix("* ")) {
            let mut spans = vec![Span::styled("• ".to_owned(), theme::fg(theme::dim()))];
            spans.extend(inline_spans(item, plain_style));
            lines.push(Line::from(spans));
            continue;
        }
        // Ordered list (simple: "1. ")
        if raw.len() > 3
            && raw.as_bytes()[0].is_ascii_digit()
            && raw.as_bytes()[1] == b'.'
            && raw.as_bytes()[2] == b' '
        {
            let mut spans = vec![Span::styled(raw[..3].to_owned(), theme::fg(theme::dim()))];
            spans.extend(inline_spans(&raw[3..], plain_style));
            lines.push(Line::from(spans));
            continue;
        }
        lines.push(Line::from(inline_spans(raw, plain_style)));
    }
    lines
}

/// Returns (heading depth, content after `#` prefix). Depth 0 = not a heading.
fn heading_level(s: &str) -> (usize, &str) {
    let trimmed = s.trim_start_matches('#');
    let depth = s.len() - trimmed.len();
    if depth == 0 || !trimmed.starts_with(' ') {
        return (0, s);
    }
    (depth, trimmed.trim_start())
}

/// Parse a line into styled spans, handling `**bold**`, `*italic*`, `` `code` ``.
fn inline_spans(s: &str, base: Style) -> Vec<Span<'static>> {
    let bold = base.add_modifier(Modifier::BOLD);
    let italic = base.add_modifier(Modifier::ITALIC);
    let code = theme::fg(theme::code());

    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), base));
                }
                let inner = take_until(&mut chars, "**");
                spans.push(Span::styled(inner, bold));
            }
            '*' => {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), base));
                }
                let inner = take_until_char(&mut chars, '*');
                spans.push(Span::styled(inner, italic));
            }
            '`' => {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), base));
                }
                let inner = take_until_char(&mut chars, '`');
                spans.push(Span::styled(inner, code));
            }
            _ => buf.push(c),
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, base));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn take_until(chars: &mut std::iter::Peekable<std::str::Chars>, end: &str) -> String {
    let end1 = end.chars().next().unwrap_or_default();
    let end2 = end.chars().nth(1).unwrap_or_default();
    let mut buf = String::new();
    while let Some(&c) = chars.peek() {
        chars.next();
        if c == end1 && chars.peek() == Some(&end2) {
            chars.next();
            break;
        }
        buf.push(c);
    }
    buf
}

fn take_until_char(chars: &mut std::iter::Peekable<std::str::Chars>, end: char) -> String {
    let mut buf = String::new();
    for c in chars.by_ref() {
        if c == end {
            break;
        }
        buf.push(c);
    }
    buf
}

/// Render a thinking block: streaming = dim full text under a header; finished
/// = a single folded summary line showing how long the model thought.
fn render_thinking(text: &str, elapsed: Option<Duration>) -> Vec<Line<'static>> {
    let style = theme::fg_dim(theme::thinking());
    elapsed.map_or_else(
        || {
            let mut lines = vec![Line::styled("▾ thinking…", style)];
            lines.extend(wrapped(text, style));
            lines
        },
        |d| {
            vec![Line::styled(
                format!("▸ thought for {:.1}s", d.as_secs_f32()),
                style,
            )]
        },
    )
}

/// Render a tool call as an omp-style card: a header line with the tool name and
/// a status glyph, the streamed argument JSON dimmed beneath, and the result (or
/// failure) indented under that.
fn render_tool(name: &str, args: &str, status: &ToolStatus) -> Vec<Line<'static>> {
    let border = theme::fg(theme::tool());
    let (glyph, glyph_style) = match status {
        ToolStatus::Running => ("⋯", theme::fg_dim(theme::dim())),
        ToolStatus::Done { error: false, .. } => ("✓", theme::fg(theme::ok())),
        ToolStatus::Done { error: true, .. } => ("⚠", theme::fg(theme::warn())),
        ToolStatus::Failed(_) => ("✗", theme::fg(theme::error())),
    };

    let mut lines = vec![Line::from(vec![
        Span::styled("╭ ", border),
        Span::styled(
            format!("tool: {name} "),
            theme::fg(theme::tool()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(glyph.to_owned(), glyph_style),
    ])];

    if !args.trim().is_empty() {
        for line in wrapped(args, theme::fg_dim(theme::dim())) {
            lines.push(prefix_span("│ ", border, line));
        }
    }

    match status {
        ToolStatus::Done { summary, error } if !summary.is_empty() => {
            let style = if *error {
                theme::fg(theme::warn())
            } else {
                theme::fg(theme::tool_result())
            };
            lines.push(Line::from(vec![
                Span::styled("╰ ↳ ", border),
                Span::styled(summary.clone(), style),
            ]));
        }
        ToolStatus::Failed(msg) => {
            lines.push(Line::from(vec![
                Span::styled("╰ ✗ ", border),
                Span::styled(msg.clone(), theme::fg(theme::error())),
            ]));
        }
        _ => lines.push(Line::styled("╰─", border)),
    }
    lines
}

/// Split `text` on newlines into styled lines (ratatui handles soft-wrapping at
/// the viewport width; we only break on explicit newlines).
fn wrapped(text: &str, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|l| Line::styled(l.to_owned(), style))
        .collect()
}

/// Like [`wrapped`], but prefixes the first line with `prefix` and indents the
/// rest by the prefix's width, so multi-line user messages align.
fn prefixed(text: &str, prefix: &str, style: Style) -> Vec<Line<'static>> {
    let indent = " ".repeat(prefix.chars().count());
    text.split('\n')
        .enumerate()
        .map(|(i, l)| {
            let lead = if i == 0 { prefix } else { &indent };
            Line::styled(format!("{lead}{l}"), style)
        })
        .collect()
}

/// Prepend a styled `prefix` span to an existing line.
fn prefix_span(prefix: &str, prefix_style: Style, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];
    spans.extend(line.spans);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A streaming thinking block shows its text under a header; once finished
    /// it folds to a single "thought for Ns" summary line. This is the
    /// expand-while-streaming / collapse-when-done behavior.
    #[test]
    fn thinking_folds_when_finished() {
        let streaming = Block::Thinking {
            text: "step one\nstep two".to_owned(),
            started: Instant::now(),
            elapsed: None,
        };
        let lines = streaming.render();
        assert!(lines.len() >= 3, "streaming shows header + text lines");

        let finished = Block::Thinking {
            text: "step one\nstep two".to_owned(),
            started: Instant::now(),
            elapsed: Some(Duration::from_millis(1500)),
        };
        let folded = finished.render();
        assert_eq!(folded.len(), 1, "finished folds to one line");
        let txt: String = folded[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(txt.contains("thought for 1.5s"), "got {txt:?}");
    }

    /// A tool block renders a card header with the name and a status glyph, and
    /// a successful result shows its summary. The glyph differs by status so the
    /// user can tell ok / warning / failure apart at a glance.
    #[test]
    fn tool_card_shows_name_and_status() {
        let ok = Block::Tool {
            name: "shell".to_owned(),
            args: r#"{"command":"ls"}"#.to_owned(),
            status: ToolStatus::Done {
                error: false,
                summary: "3 files".to_owned(),
            },
        };
        let lines = ok.render();
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            header.contains("tool: shell"),
            "header missing name: {header:?}"
        );
        assert!(header.contains('✓'), "ok glyph missing: {header:?}");
        let last: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.contains("3 files"), "result summary missing: {last:?}");

        let failed = Block::Tool {
            name: "shell".to_owned(),
            args: String::new(),
            status: ToolStatus::Failed("timeout".to_owned()),
        };
        let header: String = failed.render()[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(header.contains('✗'), "failure glyph missing: {header:?}");
    }

    /// Answer text and user text render with different foreground colors, so the
    /// two are visually distinguishable (the core of problem 3). The assistant
    /// gutter marker uses the assistant color, distinct from the user prefix.
    #[test]
    fn answer_and_user_use_distinct_styles() {
        let answer = Block::Answer {
            text: "hi".to_owned(),
            done: true,
        }
        .render();
        let user = Block::User("hi".to_owned()).render();
        // The leading gutter span carries the role color.
        let answer_marker = answer[0].spans[0].style.fg;
        let user_marker = user[0].style.fg;
        assert_ne!(
            answer_marker, user_marker,
            "assistant gutter and user prefix must use different colors"
        );
        assert_eq!(answer_marker, Some(theme::assistant()));
    }

    /// A markdown heading inside an answer is colored with the heading role, not
    /// the user role — otherwise an assistant heading reads as an echoed prompt.
    #[test]
    fn heading_does_not_reuse_user_color() {
        let lines = render_markdown("# Title");
        assert_eq!(
            lines[0].style.fg,
            Some(theme::heading()),
            "heading must use the heading role"
        );
        assert_ne!(lines[0].style.fg, Some(theme::user()));
    }

    /// While streaming, text before the last blank line (outside a code fence)
    /// is committed as markdown, while the trailing in-progress paragraph stays
    /// plain — so finished blocks don't re-flow or flicker as more arrives.
    #[test]
    fn streaming_commits_settled_blocks() {
        // A committed heading paragraph, then an in-progress line.
        let text = "# Done\n\nstill **typ";
        let commit = stable_boundary(text);
        assert_eq!(&text[..commit], "# Done\n\n", "blank line commits the heading");

        // An unterminated code fence must NOT commit (it is still open).
        let open_fence = "intro\n\n```rust\nlet x = 1;";
        let commit = stable_boundary(open_fence);
        assert_eq!(
            &open_fence[..commit],
            "intro\n\n",
            "an open fence stays in the uncommitted tail"
        );
    }
}
