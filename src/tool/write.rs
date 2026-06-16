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

        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(business_error(&args.path, &e));
        }
        match tokio::fs::write(&path, args.content.as_bytes()).await {
            Ok(()) => Ok(ToolOutput {
                content: vec![Content::Text(format!(
                    "wrote {} bytes to {}",
                    args.content.len(),
                    args.path
                ))],
                is_error: false,
                error_code: None,
            }),
            Err(e) => Ok(business_error(&args.path, &e)),
        }
    }
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
}
