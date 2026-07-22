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

/// Build the write result: a single header line, no diff body. The model
/// already has the full new content in this call's own `content` argument, so
/// echoing it back as a diff would be redundant; the visual diff is the
/// frontend's job (it reconstructs it from the args plus its file cache). See
/// `doc/tool-protocol.md` §11.
///
/// - New file (`old` is `None`) → `wrote PATH (new, N lines)`.
/// - Overwrite (`old` is `Some`) → `wrote PATH (~, +A -B)` with the added/
///   removed line counts, or `wrote PATH (no change)` for identical content.
fn write_summary(path: &str, old: Option<&str>, new: &str) -> String {
    let Some(old) = old else {
        let n = new.lines().count();
        return format!("wrote {path} (new, {n} lines)");
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
    format!("wrote {path} (~, +{added} -{removed})")
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

    /// A brand-new file reports line count in a one-line header, no body — the
    /// model already has the full content in its own `content` arg.
    #[test]
    fn summary_new_file_is_header_only() {
        let out = write_summary("src/new.rs", None, "a\nb\n");
        assert_eq!(out, "wrote src/new.rs (new, 2 lines)");
    }

    /// An overwrite reports +added/-removed counts in a one-line header, no
    /// diff body (the frontend rebuilds the visual diff from args + its cache).
    #[test]
    fn summary_overwrite_is_header_only_with_counts() {
        let out = write_summary("f.txt", Some("a\nb\nc\n"), "a\nB\nc\n");
        assert_eq!(out, "wrote f.txt (~, +1 -1)");
    }

    /// Rewriting identical content is a no-op the reader should see as such, not
    /// an empty diff that looks like a failure.
    #[test]
    fn summary_no_change_is_flagged() {
        let out = write_summary("f.txt", Some("same\n"), "same\n");
        assert_eq!(out, "wrote f.txt (no change)");
    }
}
