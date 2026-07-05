//! The `write` built-in tool: write a UTF-8 file within the workspace.

use std::path::PathBuf;

use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Writes a text file relative to the session workspace, creating parent
/// directories as needed.
#[derive(Debug, Clone)]
pub struct WriteTool {
    workspace: PathBuf,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

impl WriteTool {
    /// Create a `write` tool rooted at `workspace`.
    #[must_use]
    pub const fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "write".to_owned(),
            description: "Write a UTF-8 text file, relative to the workspace root. \
                          Creates parent directories and overwrites existing files."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full file contents to write."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: WriteArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let path = resolve_in_workspace(&self.workspace, &args.path)?;

        // Snapshot the prior content (if any) BEFORE writing, so an overwrite can
        // diff old→new. A missing/unreadable file is treated as a new file.
        let old = tokio::fs::read_to_string(&path).await.ok();

        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(business_error(&args.path, &e));
        }
        match tokio::fs::write(&path, args.content.as_bytes()).await {
            Ok(()) => Ok(ToolOutput {
                content: vec![Content::Text(write_summary(
                    &args.path,
                    old.as_deref(),
                    &args.content,
                ))],
                is_error: false,
                error_code: None,
            }),
            Err(e) => Ok(business_error(&args.path, &e)),
        }
    }
}

/// Number of unchanged context lines shown on each side of a change hunk.
const DIFF_CONTEXT: usize = 3;

/// Build the write result: a header line plus a diff body that mirrors `edit`'s
/// unified-diff shape (so the frontend renders both with one parser).
///
/// - New file (`old` is `None`) → `wrote PATH (new, N lines)` then every content
///   line prefixed `+` (a whole-file addition; no `@@` header needed).
/// - Overwrite (`old` is `Some`) → `wrote PATH (~, +A -B)` then a unified diff
///   (`@@` hunks, ` `/`-`/`+` lines) computed with [`similar`]. Identical content
///   yields `wrote PATH (no change)` with no body.
fn write_summary(path: &str, old: Option<&str>, new: &str) -> String {
    let Some(old) = old else {
        // New file: whole-file addition.
        let lines: Vec<&str> = new.lines().collect();
        let mut out = format!("wrote {path} (new, {} lines)", lines.len());
        for line in lines {
            out.push('\n');
            out.push('+');
            out.push_str(line);
        }
        return out;
    };

    if old == new {
        return format!("wrote {path} (no change)");
    }

    let diff = similar::TextDiff::from_lines(old, new);
    let (mut added, mut removed) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }

    let body = diff
        .unified_diff()
        .context_radius(DIFF_CONTEXT)
        .missing_newline_hint(false)
        .to_string();
    let mut out = format!("wrote {path} (~, +{added} -{removed})\n");
    out.push_str(body.trim_end_matches('\n'));
    out
}

fn business_error(path: &str, e: &std::io::Error) -> ToolOutput {
    ToolOutput {
        content: vec![Content::Text(format!("failed to write {path}: {e}"))],
        is_error: true,
        error_code: Some("write_failed".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    fn input(path: &str, content: &str) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "path": path, "content": content }),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn writes_file_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());

        let out = tool.invoke(input("nested/dir/a.txt", "hi")).await.unwrap();
        assert!(!out.is_error);
        let written = std::fs::read_to_string(dir.path().join("nested/dir/a.txt")).unwrap();
        assert_eq!(written, "hi");
    }

    #[tokio::test]
    async fn escaping_path_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());
        assert!(matches!(
            tool.invoke(input("../escape", "x")).await,
            Err(ToolError::InvalidInput(_))
        ));
    }

    // --- write_summary -------------------------------------------------------

    /// A brand-new file is a whole-file addition: header notes `new` + line
    /// count, body is every line prefixed `+` (no `@@`, since there's no old
    /// side to anchor against). This is the shape the frontend colours green.
    #[test]
    fn summary_new_file_is_all_plus() {
        let out = write_summary("src/new.rs", None, "a\nb\n");
        assert_eq!(out, "wrote src/new.rs (new, 2 lines)\n+a\n+b");
    }

    /// An overwrite emits a unified diff: `~` header with +added/-removed counts,
    /// then `@@` hunks with `-`/`+`/context lines — exactly `edit`'s diff shape,
    /// so one frontend parser handles both.
    #[test]
    fn summary_overwrite_is_unified_diff() {
        let out = write_summary("f.txt", Some("a\nb\nc\n"), "a\nB\nc\n");
        let head = out.lines().next().unwrap();
        assert_eq!(head, "wrote f.txt (~, +1 -1)");
        assert!(out.contains("@@"), "expected hunk header: {out}");
        assert!(out.contains("-b"), "expected removed line: {out}");
        assert!(out.contains("+B"), "expected added line: {out}");
        // The `\ No newline` metadata line must be suppressed so the frontend
        // diff parser never sees a `\`-prefixed row.
        assert!(
            !out.contains('\\'),
            "no-newline hint must be suppressed: {out}"
        );
    }

    /// Rewriting identical content is a no-op the reader should see as such, not
    /// an empty diff that looks like a failure.
    #[test]
    fn summary_no_change_is_flagged() {
        let out = write_summary("f.txt", Some("same\n"), "same\n");
        assert_eq!(out, "wrote f.txt (no change)");
    }
}
