//! The `read` built-in tool: read a UTF-8 file or list a directory within the
//! workspace.
//!
//! A bare path reads from the top of the file, capped at `MAX_BARE_LINES`
//! lines. Optional `start` (default 1) / `end` (default: last line) scope the
//! read to an inclusive 1-based line range.
//!
//! Output is a `[path] (N lines)` header — the total count lets the caller
//! scope a follow-up range without a separate `wc -l` — and every line
//! prefixed `N:`, capped at `MAX_LINE_CHARS` characters unless `verbatim:
//! true` is passed (a clipped line cannot be quoted in `edit`, so the cap
//! must be liftable; `verbatim` never widens the line window). Line numbers
//! are *absolute* even for a range, for orientation — they are not an anchor
//! [`edit`](super::EditTool) needs: `edit` locates the exact text you quote,
//! not a line number, so there is no snapshot/tag to go stale.
//!
//! A path that resolves to a directory lists its entries (sub-directories
//! suffixed `/`), sorted.

use std::path::PathBuf;

use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Reads a text file (or lists a directory) relative to the session workspace.
///
/// No LSP assist here, by design: diagnostics answer "did MY change break
/// something" and ride the write tools (`edit`/`write`); a read is a
/// positioning/inspection op whose caller did not change the file.
#[derive(Clone)]
pub struct ReadTool {
    workspace: PathBuf,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    /// First line to read, 1-based inclusive; default 1.
    start: Option<usize>,
    /// Last line to read, 1-based inclusive; default: the file's last line.
    end: Option<usize>,
    /// Return each line in full, disabling the per-line character cap.
    verbatim: Option<bool>,
}

/// A `path` argument plus the flat line window (`end: None` = to EOF).
#[derive(Debug, PartialEq, Eq)]
struct ParsedArg {
    /// The path to read.
    path: String,
    start: Option<usize>,
    end: Option<usize>,
    /// Disable the per-line character cap (default false).
    verbatim: bool,
}

impl ReadTool {
    /// Create a `read` tool rooted at `workspace`.
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read".to_owned(),
            description: "Read a UTF-8 text file or list a directory, relative to the \
                          workspace root. The `[path] (N lines)` header reports the \
                          file's total line count, so you can scope a follow-up range \
                          without guessing; each line is numbered `N:text` and capped \
                          at 500 characters — pass `verbatim: true` to read clipped \
                          lines in full (required before quoting them in `edit`; the \
                          line-window caps still apply). Line numbers are for \
                          orientation only — `edit` locates the exact lines you quote, \
                          not a line number, so don't cite them there. A bare path \
                          reads from the top, capped at 500 lines — for larger files \
                          use `start` (default 1) / `end` (default: last line) to read \
                          an inclusive 1-based range whose numbers stay absolute. \
                          A directory path lists its entries."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root."
                    },
                    "start": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 1,
                        "description": "First line to read, 1-based and inclusive \
                                         (default 1 — the whole file when `end` is \
                                         also omitted)."
                    },
                    "end": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Last line to read, 1-based and inclusive \
                                        (default: the file's last line; values past \
                                        EOF clamp to it)."
                    },
                    "verbatim": {
                        "type": "boolean",
                        "default": false,
                        "description": "Return each line in full, disabling the \
                                        per-line 500-character cap — quote these \
                                        lines verbatim in `edit`. The line-window \
                                        caps still apply; use start/end to move \
                                        the window."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: ReadArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let parsed = ParsedArg {
            path: args.path,
            start: args.start,
            end: args.end,
            verbatim: args.verbatim.unwrap_or(false),
        };
        let path = resolve_in_workspace(&self.workspace, &parsed.path)?;

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => return Ok(read_failed(&parsed.path, &e.to_string())),
        };

        if meta.is_dir() {
            if parsed.start.is_some() || parsed.end.is_some() {
                return Ok(business_error(
                    "invalid_range",
                    &format!(
                        "{} is a directory; start/end apply to files only",
                        parsed.path
                    ),
                ));
            }
            return Ok(self.list_dir(&parsed.path, &path).await);
        }

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => match render(&parsed, &content) {
                Ok(text) => {
                    // UI view: structured code content for the front-end to
                    // highlight (tree-sitter). The model-facing `Text` keeps the
                    // `[path]` + `N:line` anchor format the model needs for edit.
                    // The view always carries the WHOLE file plus the window the
                    // model actually saw — a bare read's text is capped at
                    // MAX_BARE_LINES, and the front-end highlights the full
                    // document (a slice breaks multi-line constructs) while
                    // showing just the window, numbered by its absolute lines.
                    let n = content.lines().count();
                    let (lo, hi) = match (parsed.start, parsed.end) {
                        (None, None) => (1, n.min(MAX_BARE_LINES)),
                        (start, end) => {
                            // render() bounds-checked this same window.
                            clamp_range(start.unwrap_or(1), end.unwrap_or(n), n)
                                .map_err(ToolError::InvalidInput)?
                        }
                    };
                    let view = serde_json::json!({
                        "kind": "code",
                        "path": parsed.path,
                        "content": content,
                        "numbered": true,
                        "start": lo,
                        "end": hi,
                    })
                    .to_string();
                    let mut output = ToolOutput {
                        content: vec![Content::Text(text)],
                        is_error: false,
                        error_code: None,
                    };
                    output.content.push(Content::TextView {
                        text: view,
                        audience: crate::core::payload::AUDIENCE_UI.to_owned(),
                    });
                    Ok(output)
                }
                Err(msg) => Ok(business_error(
                    "bad_range",
                    &format!("{}: {msg}", parsed.path),
                )),
            },
            // A missing/unreadable file is a business error the model can react
            // to, not a protocol fault.
            Err(e) => Ok(read_failed(&parsed.path, &e.to_string())),
        }
    }
}

impl ReadTool {
    /// List a directory's entries, sorted, sub-directories suffixed `/`.
    async fn list_dir(&self, rel: &str, abs: &std::path::Path) -> ToolOutput {
        let mut entries = match tokio::fs::read_dir(abs).await {
            Ok(rd) => rd,
            Err(e) => return read_failed(rel, &e.to_string()),
        };
        let mut names: Vec<String> = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
                    names.push(if is_dir { format!("{name}/") } else { name });
                }
                Ok(None) => break,
                Err(e) => return read_failed(rel, &e.to_string()),
            }
        }
        names.sort();
        let mut parts = vec![format!("[{}/]", rel.trim_end_matches('/'))];
        parts.extend(names.clone());
        let view = serde_json::json!({
            "kind": "listing",
            "path": rel,
            "entries": names,
        })
        .to_string();
        ToolOutput {
            content: vec![
                Content::Text(parts.join("\n")),
                Content::TextView {
                    text: view,
                    audience: crate::core::payload::AUDIENCE_UI.to_owned(),
                },
            ],
            is_error: false,
            error_code: None,
        }
    }
}

/// Lines shown by a bare (unranged) read before truncation kicks in — a bare
/// read must never dump an unbounded file into context.
const MAX_BARE_LINES: usize = 500;

/// Characters shown per line before the line is truncated — one minified line
/// must not blow the token budget.
const MAX_LINE_CHARS: usize = 500;

/// Render file content per the optional start/end window. The header always
/// carries the file's total line count so the caller can scope a follow-up
/// ranged read without a separate `wc -l` round-trip.
///
/// - start/end set: `[path] (N lines)` header then absolute `N:text` lines
///   for the slice — no notice: the caller chose that window and the header
///   already tells them the total.
/// - neither set: the same header then every line numbered, capped at
///   [`MAX_BARE_LINES`] with a truncation notice telling the caller exactly
///   which range to ask for next.
///
/// Every line is capped at [`MAX_LINE_CHARS`] unless `verbatim` is set — a
/// clipped line cannot be quoted in `edit`, so `verbatim` exists to fetch it
/// in full. It does NOT widen the line window.
fn render(parsed: &ParsedArg, content: &str) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    let header = format!("[{}] ({n} lines)", parsed.path);
    let (lo, hi) = match (parsed.start, parsed.end) {
        (None, None) => (1, n.min(MAX_BARE_LINES)),
        (start, end) => clamp_range(start.unwrap_or(1), end.unwrap_or(n), n)?,
    };
    // lo/hi are 1-based inclusive; slice is 0-based half-open.
    let slice = &lines[lo - 1..hi];
    let mut parts = vec![header];
    parts.extend(slice.iter().enumerate().map(|(i, l)| {
        if parsed.verbatim {
            format!("{}:{l}", lo + i)
        } else {
            format!("{}:{}", lo + i, clip_line(l))
        }
    }));
    // The notice exists only for a TOOL-imposed cap (bare read); a caller who
    // asked for a window that ends before EOF did so on purpose.
    if parsed.start.is_none() && parsed.end.is_none() && hi < n {
        parts.push(format!(
            "... (showing {lo}-{hi} of {n}; use start/end to read more)"
        ));
    }
    Ok(parts.join("\n"))
}

/// Cap one line at [`MAX_LINE_CHARS`], appending a notice that names the
/// escape hatch (`verbatim: true`) alongside the original length.
fn clip_line(line: &str) -> String {
    if line.len() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let cut = line
        .char_indices()
        .nth(MAX_LINE_CHARS)
        .map_or(line.len(), |(i, _)| i);
    format!(
        "{}... ({} chars total; pass verbatim: true for the full line)",
        &line[..cut],
        line.len()
    )
}

/// Bounds-check a 1-based inclusive range against a file of `n` lines. `end` is
/// clamped to `n` (reading "to the end" is friendly); `start` past EOF or an
/// inverted range is an error so a typo fails loud rather than returning empty.
fn clamp_range(start: usize, end: usize, n: usize) -> Result<(usize, usize), String> {
    if start == 0 {
        return Err("line numbers are 1-based; 0 is invalid".to_owned());
    }
    if start > end {
        return Err(format!("inverted range {start}-{end}"));
    }
    if start > n {
        return Err(format!("line {start} past end of file ({n} lines)"));
    }
    Ok((start, end.min(n)))
}

fn business_error(code: &str, message: &str) -> ToolOutput {
    ToolOutput {
        content: vec![Content::Text(message.to_owned())],
        is_error: true,
        error_code: Some(code.to_owned()),
    }
}

fn read_failed(path: &str, err: &str) -> ToolOutput {
    business_error("read_failed", &format!("failed to read {path}: {err}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    fn input(path: &str) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "path": path }),
            timeout: Duration::from_secs(5),
            progress: None,
        }
    }

    fn range_input(path: &str, start: usize, end: usize) -> ToolInput {
        window_input(path, Some(start), Some(end))
    }

    fn window_input(path: &str, start: Option<usize>, end: Option<usize>) -> ToolInput {
        let mut input = serde_json::json!({ "path": path });
        if let Some(s) = start {
            input["start"] = serde_json::json!(s);
        }
        if let Some(e) = end {
            input["end"] = serde_json::json!(e);
        }
        ToolInput {
            call_id: "c1".to_owned(),
            input,
            timeout: Duration::from_secs(5),
            progress: None,
        }
    }

    fn tool(workspace: PathBuf) -> ReadTool {
        ReadTool::new(workspace)
    }

    fn text(out: &ToolOutput) -> String {
        match &out.content[0] {
            Content::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn view(out: &ToolOutput) -> serde_json::Value {
        out.content
            .iter()
            .find_map(|c| match c {
                Content::TextView { text, .. } => Some(serde_json::from_str(text).unwrap()),
                _ => None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn reads_existing_file_with_header_and_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt")).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(text(&out), "[a.txt] (2 lines)\n1:hello\n2:world");
    }

    #[tokio::test]
    async fn missing_file_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("nope.txt")).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("read_failed"));
    }

    #[tokio::test]
    async fn escaping_path_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path().to_path_buf());
        assert!(matches!(
            t.invoke(input("../escape")).await,
            Err(ToolError::InvalidInput(_))
        ));
    }

    // --- range behavior -----------------------------------------------------

    /// A range keeps ABSOLUTE line numbers: `edit` anchors on quoted content,
    /// not line numbers — absolute numbers exist to locate the slice within the
    /// file for the model and any human reading the output.
    #[tokio::test]
    async fn range_keeps_absolute_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("a.txt", 2, 4)).await.unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(text(&out), "[a.txt] (5 lines)\n2:l2\n3:l3\n4:l4");
    }

    /// `end` past EOF clamps to the last line; this is a friendly "to the end".
    #[tokio::test]
    async fn range_end_past_eof_clamps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("a.txt", 1, 99)).await.unwrap();
        assert_eq!(text(&out), "[a.txt] (2 lines)\n1:a\n2:b");
    }

    /// `start` past EOF fails loud rather than returning an empty slice.
    #[tokio::test]
    async fn range_start_past_eof_is_bad_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("a.txt", 5, 9)).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_range"));
    }

    /// `start` alone defaults `end` to the file's last line — "read from N to
    /// the end" without needing to know the length.
    #[tokio::test]
    async fn start_alone_reads_to_eof() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(window_input("a.txt", Some(2), None))
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(text(&out), "[a.txt] (3 lines)\n2:l2\n3:l3");
    }

    /// `end` alone defaults `start` to 1 — "read the first N lines". The view
    /// reports the RESOLVED window, not just the args echoed.
    #[tokio::test]
    async fn end_alone_reads_from_line_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(window_input("a.txt", None, Some(2)))
            .await
            .unwrap();
        assert_eq!(text(&out), "[a.txt] (3 lines)\n1:l1\n2:l2");
        let view = view(&out);
        assert_eq!(view["numbered"], true);
        assert_eq!(view["start"], 1);
        assert_eq!(view["end"], 2);
    }

    // --- UI view (TextView) -------------------------------------------------

    /// A ranged read's UI view carries the WHOLE file plus the resolved
    /// window: the front-end highlights the full document (partial-file
    /// parsing breaks multi-line constructs) and shows the slice. The gutter
    /// numbers are the window's absolute lines.
    #[tokio::test]
    async fn ranged_read_view_is_full_content_with_window() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("a.txt", 2, 4)).await.unwrap();
        let view = view(&out);
        assert_eq!(view["kind"], "code");
        assert_eq!(view["content"], "l1\nl2\nl3\nl4\nl5\n");
        assert_eq!(view["numbered"], true);
        assert_eq!(view["start"], 2);
        assert_eq!(view["end"], 4);
    }

    /// `start`/`end` in the view are the RESOLVED window — the gutter the
    /// front-end renders must equal the absolute lines the model's text cites.
    #[tokio::test]
    async fn ranged_view_window_matches_model_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("a.txt", 2, 4)).await.unwrap();
        let view = view(&out);
        let (start, end) = (
            view["start"].as_u64().unwrap(),
            view["end"].as_u64().unwrap(),
        );
        let model_text = text(&out);
        let body = model_text.split_once('\n').unwrap().1;
        let nums: Vec<u64> = body
            .lines()
            .map(|l| l.split_once(':').unwrap().0.parse().unwrap())
            .collect();
        assert_eq!(nums, (start..=end).collect::<Vec<_>>());
    }

    /// A bare read's view carries the whole file plus the resolved window —
    /// the same shape as a ranged read — so a truncated bare read still
    /// highlights correctly and shows exactly what the model saw.
    #[tokio::test]
    async fn bare_read_view_is_full_content_with_window() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt")).await.unwrap();
        let view = view(&out);
        assert_eq!(view["content"], "l1\nl2\n");
        assert_eq!(view["numbered"], true);
        assert_eq!(view["start"], 1);
        assert_eq!(view["end"], 2);
    }

    #[tokio::test]
    async fn inverted_range_is_bad_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("a.txt", 3, 1)).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_range"));
    }

    // --- header line count, bare-read cap, long-line cap ---------------------

    /// A bare read of a file over `MAX_BARE_LINES` returns only the head of
    /// the file — the header plus the truncation notice tell the model the
    /// total and the exact next range to ask for, replacing the `wc -l` +
    /// `sed -n` two-call idiom.
    #[tokio::test]
    async fn bare_read_of_large_file_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=600u32).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "line{i}");
            s
        });
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("big.txt")).await.unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let text = text(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "[big.txt] (600 lines)");
        assert_eq!(lines[1], "1:line1");
        assert_eq!(lines[500], "500:line500");
        assert_eq!(
            lines[501],
            "... (showing 1-500 of 600; use start/end to read more)"
        );
        assert_eq!(lines.len(), 502);
    }

    /// A small file below the cap shows every line with no truncation notice.
    #[tokio::test]
    async fn bare_read_under_cap_shows_all_lines() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=3u32).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "line{i}");
            s
        });
        std::fs::write(dir.path().join("small.txt"), body).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("small.txt")).await.unwrap();
        assert_eq!(
            text(&out),
            "[small.txt] (3 lines)\n1:line1\n2:line2\n3:line3"
        );
    }

    /// One pathological line (minified code, generated JSON) must not blow
    /// the token budget: it is cut at the cap and annotated with its true
    /// length. Lines under the cap pass through unchanged.
    #[tokio::test]
    async fn overlong_line_is_clipped_with_length_notice() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(1200);
        std::fs::write(dir.path().join("long.txt"), format!("short\n{long}")).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("long.txt")).await.unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let text = text(&out);
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), "[long.txt] (2 lines)");
        assert_eq!(lines.next().unwrap(), "1:short");
        let clipped = lines.next().unwrap();
        assert!(clipped.starts_with(&format!("2:{}", "x".repeat(500))));
        assert!(clipped.ends_with("... (1200 chars total; pass verbatim: true for the full line)"));
        assert_eq!(lines.next(), None);
    }

    /// `verbatim: true` returns the line in full: a clipped line cannot be
    /// quoted in `edit` (the anchor would never match), so the cap must be
    /// liftable on demand. The word mirrors `edit`'s "quote verbatim"
    /// contract so the fetch-then-quote chain reads consistently.
    #[tokio::test]
    async fn verbatim_returns_clipped_line_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(1200);
        std::fs::write(dir.path().join("long.txt"), format!("short\n{long}")).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(ToolInput {
                call_id: "c1".to_owned(),
                input: serde_json::json!({ "path": "long.txt", "verbatim": true }),
                timeout: Duration::from_secs(5),
                progress: None,
            })
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            text(&out),
            format!("[long.txt] (2 lines)\n1:short\n2:{long}")
        );
    }

    /// `verbatim` composes with a range: the window selects the lines,
    /// verbatim widens the columns. Combined, they fetch exactly the slice
    /// to be edited, in quotable form, in one call.
    #[tokio::test]
    async fn verbatim_composes_with_range() {
        let dir = tempfile::tempdir().unwrap();
        let long = "y".repeat(800);
        std::fs::write(dir.path().join("mixed.txt"), format!("a\n{long}\nb\nc\n")).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(ToolInput {
                call_id: "c1".to_owned(),
                input: serde_json::json!({
                    "path": "mixed.txt",
                    "start": 2,
                    "end": 3,
                    "verbatim": true,
                }),
                timeout: Duration::from_secs(5),
                progress: None,
            })
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(text(&out), format!("[mixed.txt] (4 lines)\n2:{long}\n3:b"));
    }

    /// `verbatim` lifts the COLUMN cap only — the bare-read line-window cap
    /// still applies. A caller who reads `verbatim: true` expecting the whole
    /// raw file must still see the truncation notice pointing at start/end.
    #[tokio::test]
    async fn verbatim_does_not_widen_the_line_window() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=600u32).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "line{i}");
            s
        });
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t
            .invoke(ToolInput {
                call_id: "c1".to_owned(),
                input: serde_json::json!({ "path": "big.txt", "verbatim": true }),
                timeout: Duration::from_secs(5),
                progress: None,
            })
            .await
            .unwrap();
        let body_text = text(&out);
        let lines: Vec<&str> = body_text.lines().collect();
        assert_eq!(lines[0], "[big.txt] (600 lines)");
        assert_eq!(lines[500], "500:line500");
        assert_eq!(
            lines[501],
            "... (showing 1-500 of 600; use start/end to read more)"
        );
        assert_eq!(lines.len(), 502);
    }

    // --- directory listing --------------------------------------------------

    #[tokio::test]
    async fn lists_directory_entries_sorted_with_dir_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "x").unwrap();
        std::fs::write(dir.path().join("a.txt"), "y").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input(".")).await.unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(text(&out), "[./]\na.txt\nb.txt\nsub/");
    }

    #[tokio::test]
    async fn range_on_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("sub", 1, 10)).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("invalid_range"));
    }
}
