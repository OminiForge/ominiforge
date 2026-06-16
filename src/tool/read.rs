//! The `read` built-in tool: read a UTF-8 file within the workspace.

use std::path::PathBuf;

use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult, resolve_in_workspace};
use crate::core::payload::{Content, ToolOutput};

/// Reads a text file relative to the session workspace.
#[derive(Debug, Clone)]
pub struct ReadTool {
    workspace: PathBuf,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
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
            description: "Read a UTF-8 text file, relative to the workspace root.".to_owned(),
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
            Ok(content) => Ok(ToolOutput {
                content: vec![Content::Text(content)],
                is_error: false,
                error_code: None,
            }),
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

    #[tokio::test]
    async fn reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());

        let out = tool.invoke(input("a.txt")).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, vec![Content::Text("hello".to_owned())]);
    }

    #[tokio::test]
    async fn missing_file_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());

        let out = tool.invoke(input("nope.txt")).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("read_failed"));
    }

    #[tokio::test]
    async fn escaping_path_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        assert!(matches!(
            tool.invoke(input("../escape")).await,
            Err(ToolError::InvalidInput(_))
        ));
    }
}
