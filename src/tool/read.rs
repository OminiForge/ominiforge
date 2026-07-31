//! The `read` built-in tool: read a UTF-8 file or list a directory within the
//! workspace.
//!
//! A bare path reads the whole file. Optional `start` (default 1) / `end`
//! (default: last line) scope the read to an inclusive 1-based line range.
//!
//! Output is a `[path]` header and every line prefixed `N:`. Line numbers are
//! *absolute* even for a range, for orientation — they are not an anchor
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
}

/// A `path` argument plus the flat line window (`end: None` = to EOF).
#[derive(Debug, PartialEq, Eq)]
struct ParsedArg {
    /// The path to read.
    path: String,
    start: Option<usize>,
    end: Option<usize>,
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
                          workspace root. A bare file path numbers every line (`N:text`) \
                          under a `[path]` header. Line numbers are for orientation only \
                          — `edit` locates the exact lines you quote, not a line number, \
                          so don't cite them there. A bare path reads the whole file; \
                          `start` (default 1) / `end` (default: last line) scope the \
                          read to an inclusive 1-based range whose numbers stay \
                          absolute. A directory path lists its entries."
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
                    // A ranged read shows ONLY the slice — with absolute line
                    // numbers and the requested (pre-clamp) range, so the
                    // gutter matches the model-facing text and the view doesn't
                    // store the whole file when the call asked for a window.
                    let view = match (parsed.start, parsed.end) {
                        (None, None) => serde_json::json!({
                            "kind": "code",
                            "path": parsed.path,
                            "content": content,
                        }),
                        (start, end) => {
                            // The view carries the WHOLE file plus the
                            // RESOLVED window (start defaults to 1, end to
                            // the last line): the front-end highlights the
                            // full document (a slice breaks multi-line
                            // constructs) and shows the slice, numbered by
                            // the window's absolute lines.
                            let n = content.lines().count();
                            // render() bounds-checked this same window.
                            let (lo, hi) = clamp_range(start.unwrap_or(1), end.unwrap_or(n), n)
                                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
                            serde_json::json!({
                                "kind": "code",
                                "path": parsed.path,
                                "content": content,
                                "numbered": true,
                                "start": lo,
                                "end": hi,
                            })
                        }
                    }
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

/// Render file content per the optional start/end window.
///
/// - start/end set: `[path]` header then absolute `N:text` lines for the slice.
/// - neither set: `[path]` header then every line numbered.
fn render(parsed: &ParsedArg, content: &str) -> Result<String, String> {
    match (parsed.start, parsed.end) {
        (None, None) => Ok(numbered(&parsed.path, content)),
        (start, end) => {
            let lines: Vec<&str> = content.lines().collect();
            let n = lines.len();
            let (lo, hi) = clamp_range(start.unwrap_or(1), end.unwrap_or(n), n)?;
            // lo/hi are 1-based inclusive; slice is 0-based half-open.
            let slice = &lines[lo - 1..hi];
            let mut parts = vec![format!("[{}]", parsed.path)];
            parts.extend(
                slice
                    .iter()
                    .enumerate()
                    .map(|(i, l)| format!("{}:{l}", lo + i)),
            );
            Ok(parts.join("\n"))
        }
    }
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

/// Render the original whole-file form: `[path]` header then 1-based
/// `N:text` lines. An empty file yields just the header.
fn numbered(path: &str, content: &str) -> String {
    let mut parts = vec![format!("[{path}]")];
    parts.extend(
        content
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{}:{l}", i + 1)),
    );
    parts.join("\n")
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
        assert_eq!(text(&out), "[a.txt]\n1:hello\n2:world");
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
        assert_eq!(text(&out), "[a.txt]\n2:l2\n3:l3\n4:l4");
    }

    /// `end` past EOF clamps to the last line; this is a friendly "to the end".
    #[tokio::test]
    async fn range_end_past_eof_clamps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(range_input("a.txt", 1, 99)).await.unwrap();
        assert_eq!(text(&out), "[a.txt]\n1:a\n2:b");
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
        assert_eq!(text(&out), "[a.txt]\n2:l2\n3:l3");
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
        assert_eq!(text(&out), "[a.txt]\n1:l1\n2:l2");
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

    /// A whole-file read keeps the legacy view shape: raw content, numbered
    /// 1..N by the front-end, no `numbered`/`start`/`end` fields.
    #[tokio::test]
    async fn whole_file_view_is_unnumbered_full_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt")).await.unwrap();
        let view = view(&out);
        assert_eq!(view["content"], "l1\nl2\n");
        assert!(view.get("numbered").is_none());
        assert!(view.get("start").is_none());
        assert!(view.get("end").is_none());
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
