//! The `read` built-in tool: read a UTF-8 file within the workspace.
//!
//! Output is anchored for [`edit`](super::EditTool): a `[path#TAG]` header
//! fingerprints the file (the snapshot the later patch is verified against) and
//! every line is prefixed `N:` so a patch can cite exact line numbers. The tag
//! is recorded in the shared [`SnapshotStore`] keyed by the resolved path.

use std::path::PathBuf;

use serde::Deserialize;

use super::snapshot::{SnapshotStore, tag_of};
use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Reads a text file relative to the session workspace.
#[derive(Debug, Clone)]
pub struct ReadTool {
    workspace: PathBuf,
    snapshots: SnapshotStore,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
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
            description: "Read a UTF-8 text file, relative to the workspace root. \
                          Output starts with a `[path#TAG]` header and numbers every \
                          line (`N:text`); cite that TAG and those line numbers when \
                          calling `edit`."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root."
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
        let path = resolve_in_workspace(&self.workspace, &args.path)?;

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let tag = tag_of(content.as_bytes());
                self.snapshots.record(&path, tag.clone());
                Ok(ToolOutput {
                    content: vec![Content::Text(render(&args.path, &tag, &content))],
                    is_error: false,
                    error_code: None,
                })
            }
            // A missing/unreadable file is a business error the model can react
            // to, not a protocol fault.
            Err(e) => Ok(ToolOutput {
                content: vec![Content::Text(format!("failed to read {}: {e}", args.path))],
                is_error: true,
                error_code: Some("read_failed".to_owned()),
            }),
        }
    }
}

/// Render read output: a `[path#TAG]` header line followed by 1-based
/// `N:text` lines. An empty file yields just the header.
fn render(path: &str, tag: &str, content: &str) -> String {
    let mut parts = vec![format!("[{path}#{tag}]")];
    parts.extend(content.lines().enumerate().map(|(i, l)| format!("{}:{l}", i + 1)));
    parts.join("\n")
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

    #[tokio::test]
    async fn reads_existing_file_with_header_and_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld").unwrap();
        let t = tool(dir.path().to_path_buf());

        let out = t.invoke(input("a.txt")).await.unwrap();
        assert!(!out.is_error);
        let tag = tag_of(b"hello\nworld");
        assert_eq!(
            out.content,
            vec![Content::Text(format!("[a.txt#{tag}]\n1:hello\n2:world"))]
        );
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
}
