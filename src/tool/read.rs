//! The `read` built-in tool: read a UTF-8 file or list a directory within the
//! workspace.
//!
//! A bare path reads the whole file. Append a `:` selector to scope the read:
//! `:50-200` is an inclusive 1-based line range, `:50+150` is the count form
//! (200 lines from line 50), and `:raw` returns verbatim bytes with no header or
//! line numbers. A range and `:raw` combine (`file:1-40:raw`).
//!
//! Output for a non-raw read is anchored for [`edit`](super::EditTool): a
//! `[path#TAG]` header fingerprints the whole file (the snapshot the later patch
//! is verified against) and every line is prefixed `N:`. Line numbers are
//! *absolute* even for a range, so an `edit` patch built off a sliced read still
//! cites the right lines. The tag is recorded in the shared [`SnapshotStore`]
//! keyed by the resolved path.
//!
//! A path that resolves to a directory lists its entries (sub-directories
//! suffixed `/`), sorted; directories carry no tag and cannot be edited.

use std::path::PathBuf;

use serde::Deserialize;

use super::snapshot::{SnapshotStore, tag_of};
use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Reads a text file (or lists a directory) relative to the session workspace.
#[derive(Debug, Clone)]
pub struct ReadTool {
    workspace: PathBuf,
    snapshots: SnapshotStore,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

/// A `path` argument split into its file path and optional selector.
#[derive(Debug, PartialEq, Eq)]
struct ParsedArg {
    /// The path with any trailing selector removed.
    path: String,
    /// Inclusive 1-based line range, when a `:N-M` / `:N+C` selector was given.
    range: Option<(usize, usize)>,
    /// `:raw` — emit verbatim bytes, no header, no line numbers.
    raw: bool,
}

impl ReadTool {
    /// Create a `read` tool rooted at `workspace`, recording snapshots into the
    /// shared `snapshots` store that `edit` verifies against.
    #[must_use]
    pub const fn new(workspace: PathBuf, snapshots: SnapshotStore) -> Self {
        Self {
            workspace,
            snapshots,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read".to_owned(),
            description: "Read a UTF-8 text file or list a directory, relative to the \
                          workspace root. A bare file path numbers every line (`N:text`) \
                          under a `[path#TAG]` header; cite that TAG and those numbers \
                          when calling `edit`. Append a selector: `:50-200` (line range), \
                          `:50+150` (200 lines from line 50), `:raw` (verbatim, no header \
                          or numbers). A range and `:raw` combine. Line numbers stay \
                          absolute for a range. A directory path lists its entries."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root, with an \
                                        optional `:N-M`, `:N+C`, and/or `:raw` selector."
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
        let parsed = parse_arg(&args.path);
        let path = resolve_in_workspace(&self.workspace, &parsed.path)?;

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => return Ok(read_failed(&parsed.path, &e.to_string())),
        };

        if meta.is_dir() {
            if parsed.range.is_some() || parsed.raw {
                return Ok(business_error(
                    "invalid_selector",
                    &format!("{} is a directory; selectors apply to files only", parsed.path),
                ));
            }
            return Ok(self.list_dir(&parsed.path, &path).await);
        }

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let tag = tag_of(content.as_bytes());
                self.snapshots.record(&path, tag.clone());
                match render(&parsed, &tag, &content) {
                    Ok(text) => Ok(ToolOutput {
                        content: vec![Content::Text(text)],
                        is_error: false,
                        error_code: None,
                    }),
                    Err(msg) => Ok(business_error("bad_range", &format!("{}: {msg}", parsed.path))),
                }
            }
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
        parts.extend(names);
        ToolOutput {
            content: vec![Content::Text(parts.join("\n"))],
            is_error: false,
            error_code: None,
        }
    }
}

/// Split a `path` argument into its path and optional `:` selectors.
///
/// `:raw` is stripped first (it always trails), then a `:N-M` / `:N+C` range.
/// A suffix that does not parse as a selector is left as part of the path, so a
/// path that legitimately contains `:` is not mangled.
fn parse_arg(arg: &str) -> ParsedArg {
    let mut s = arg;
    let mut raw = false;
    if let Some(rest) = s.strip_suffix(":raw") {
        raw = true;
        s = rest;
    }

    let mut range = None;
    if let Some((rest, sel)) = s.rsplit_once(':')
        && let Some(r) = parse_range_selector(sel)
    {
        range = Some(r);
        s = rest;
    }

    ParsedArg {
        path: s.to_owned(),
        range,
        raw,
    }
}

/// Parse a range selector into an inclusive 1-based `(start, end)`. `None` if the
/// text is not a range selector.
///
/// `N-M` is an explicit range; `N+C` is the count form (`C` lines from `N`).
fn parse_range_selector(sel: &str) -> Option<(usize, usize)> {
    if let Some((a, b)) = sel.split_once('-') {
        let start = a.parse::<usize>().ok()?;
        let end = b.parse::<usize>().ok()?;
        return Some((start, end));
    }
    if let Some((a, c)) = sel.split_once('+') {
        let start = a.parse::<usize>().ok()?;
        let count = c.parse::<usize>().ok()?;
        // C lines starting at N: lines N..=N+C-1. A zero count yields an empty,
        // inverted range that render() rejects as bad_range.
        return Some((start, start + count.saturating_sub(1)));
    }
    None
}

/// Render file content per the parsed selector.
///
/// - `raw` whole file: verbatim content, untouched.
/// - `raw` + range: the selected lines joined verbatim, no header/numbers.
/// - range: `[path#TAG]` header then absolute `N:text` lines for the slice.
/// - none: `[path#TAG]` header then every line numbered (the original form).
fn render(parsed: &ParsedArg, tag: &str, content: &str) -> Result<String, String> {
    match (parsed.range, parsed.raw) {
        (None, true) => Ok(content.to_owned()),
        (None, false) => Ok(numbered(&parsed.path, tag, content)),
        (Some((start, end)), raw) => {
            let lines: Vec<&str> = content.lines().collect();
            let (lo, hi) = clamp_range(start, end, lines.len())?;
            // lo/hi are 1-based inclusive; slice is 0-based half-open.
            let slice = &lines[lo - 1..hi];
            if raw {
                Ok(slice.join("\n"))
            } else {
                let mut parts = vec![format!("[{}#{tag}]", parsed.path)];
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

/// Render the original whole-file form: `[path#TAG]` header then 1-based
/// `N:text` lines. An empty file yields just the header.
fn numbered(path: &str, tag: &str, content: &str) -> String {
    let mut parts = vec![format!("[{path}#{tag}]")];
    parts.extend(content.lines().enumerate().map(|(i, l)| format!("{}:{l}", i + 1)));
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
        }
    }

    fn tool(workspace: PathBuf) -> ReadTool {
        ReadTool::new(workspace, SnapshotStore::new())
    }

    fn text(out: &ToolOutput) -> String {
        match &out.content[0] {
            Content::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reads_existing_file_with_header_and_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt")).await.unwrap();
        assert!(!out.is_error);
        let tag = tag_of(b"hello\nworld");
        assert_eq!(text(&out), format!("[a.txt#{tag}]\n1:hello\n2:world"));
    }

    /// A successful read records the file's tag in the shared store under the
    /// resolved path — this is the anchor `edit` later verifies against.
    #[tokio::test]
    async fn read_records_snapshot_tag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        let store = SnapshotStore::new();
        let t = ReadTool::new(dir.path().to_path_buf(), store.clone());

        t.invoke(input("a.txt")).await.unwrap();
        let resolved = dir.path().join("a.txt");
        assert_eq!(store.get(&resolved), Some(tag_of(b"hi")));
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

    // --- selector parsing ---------------------------------------------------

    #[test]
    fn parse_plain_path() {
        assert_eq!(
            parse_arg("src/a.rs"),
            ParsedArg { path: "src/a.rs".to_owned(), range: None, raw: false }
        );
    }

    #[test]
    fn parse_range_dash_and_count() {
        assert_eq!(parse_arg("a:50-200").range, Some((50, 200)));
        // count form: 150 lines from 50 -> 50..=199
        assert_eq!(parse_arg("a:50+150").range, Some((50, 199)));
    }

    #[test]
    fn parse_raw_and_combo() {
        assert_eq!(
            parse_arg("a:raw"),
            ParsedArg { path: "a".to_owned(), range: None, raw: true }
        );
        assert_eq!(
            parse_arg("src/p.ts:1-40:raw"),
            ParsedArg { path: "src/p.ts".to_owned(), range: Some((1, 40)), raw: true }
        );
    }

    /// A non-selector suffix (e.g. a scheme-like `:foo`) stays part of the path.
    #[test]
    fn parse_non_selector_suffix_is_kept() {
        assert_eq!(parse_arg("a:foo").path, "a:foo");
        assert!(parse_arg("a:foo").range.is_none());
    }

    // --- selector behavior --------------------------------------------------

    /// A range keeps ABSOLUTE line numbers so an `edit` built off the slice still
    /// cites the correct lines.
    #[tokio::test]
    async fn range_keeps_absolute_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt:2-4")).await.unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        let tag = tag_of(b"l1\nl2\nl3\nl4\nl5\n");
        assert_eq!(text(&out), format!("[a.txt#{tag}]\n2:l2\n3:l3\n4:l4"));
    }

    /// The header tag on a sliced read is the WHOLE-file tag, so it matches what
    /// `edit` verifies against.
    #[tokio::test]
    async fn range_header_tag_is_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let body = "a\nb\nc\nd\n";
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let store = SnapshotStore::new();
        let t = ReadTool::new(dir.path().to_path_buf(), store.clone());

        t.invoke(input("a.txt:2-3")).await.unwrap();
        assert_eq!(store.get(&dir.path().join("a.txt")), Some(tag_of(body.as_bytes())));
    }

    #[tokio::test]
    async fn count_form_selects_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        // 2 lines from line 3 -> lines 3,4
        let out = t.invoke(input("a.txt:3+2")).await.unwrap();
        let tag = tag_of(b"1\n2\n3\n4\n5\n");
        assert_eq!(text(&out), format!("[a.txt#{tag}]\n3:3\n4:4"));
    }

    #[tokio::test]
    async fn raw_whole_file_is_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\ny\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt:raw")).await.unwrap();
        assert_eq!(text(&out), "x\ny\n");
    }

    #[tokio::test]
    async fn raw_range_is_slice_without_anchors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\nd\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt:2-3:raw")).await.unwrap();
        assert_eq!(text(&out), "b\nc");
    }

    /// `end` past EOF clamps to the last line; this is a friendly "to the end".
    #[tokio::test]
    async fn range_end_past_eof_clamps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt:1-99")).await.unwrap();
        let tag = tag_of(b"a\nb\n");
        assert_eq!(text(&out), format!("[a.txt#{tag}]\n1:a\n2:b"));
    }

    /// `start` past EOF fails loud rather than returning an empty slice.
    #[tokio::test]
    async fn range_start_past_eof_is_bad_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt:5-9")).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("bad_range"));
    }

    #[tokio::test]
    async fn inverted_range_is_bad_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt:3-1")).await.unwrap();
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
    async fn selector_on_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("sub:1-10")).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("invalid_selector"));
    }
}
