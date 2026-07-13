//! The `shell` built-in tool: run a command through a [`Sandbox`].
//!
//! The tool itself is backend-agnostic: it hands the command line to
//! [`Sandbox::exec`] and renders the result. Which sandbox it runs in (host
//! passthrough today, `BoxLite` microVM when enabled) is decided at
//! construction (`doc/sandbox.md` §7, Step 3). Resource limits and network
//! policy travel with the sandbox's `SandboxConfig`, not this tool.

use std::sync::Arc;

use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult};
use crate::core::payload::{Content, ToolOutput};
use crate::sandbox::{ExecOutput, Sandbox, SandboxError};

/// Runs a shell command inside a [`Sandbox`].
#[derive(Clone)]
pub struct ShellTool {
    sandbox: Arc<dyn Sandbox>,
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
}

impl ShellTool {
    /// Create a `shell` tool that executes commands in `sandbox`.
    #[must_use]
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "shell".to_owned(),
            description: "Run a shell command in the workspace directory and capture its \
                          combined stdout and stderr."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command line to execute via `sh -c`."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        let args: ShellArgs = serde_json::from_value(input.input)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        let output = self
            .sandbox
            .exec(&args.command, input.timeout)
            .await
            .map_err(|e| match e {
                SandboxError::Timeout(d) => ToolError::Timeout(d),
                other => ToolError::Execution(other.to_string()),
            })?;

        Ok(render_output(&output))
    }
}

/// Combine a finished command's streams into a tool output, flagging non-zero
/// exits as business errors.
fn render_output(output: &ExecOutput) -> ToolOutput {
    let mut text = output.stdout.clone();
    if !output.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&output.stderr);
    }

    let success = output.success();
    let error_code = (!success).then(|| {
        output
            .exit_code
            .map_or_else(|| "signal".to_owned(), |c| format!("exit_{c}"))
    });

    ToolOutput {
        content: vec![Content::Text(text)],
        is_error: !success,
        error_code,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::sandbox::passthrough::PassthroughSandbox;

    fn shell(workspace: PathBuf, env: BTreeMap<String, Option<String>>) -> ShellTool {
        ShellTool::new(Arc::new(PassthroughSandbox::new(workspace, env)))
    }

    fn input(command: &str, timeout: Duration) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: serde_json::json!({ "command": command }),
            timeout,
        }
    }

    #[tokio::test]
    async fn captures_stdout_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shell(dir.path().to_path_buf(), BTreeMap::new());

        let out = tool
            .invoke(input("echo hello", Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, vec![Content::Text("hello\n".to_owned())]);
    }

    #[tokio::test]
    async fn nonzero_exit_is_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shell(dir.path().to_path_buf(), BTreeMap::new());

        let out = tool
            .invoke(input("exit 3", Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("exit_3"));
    }

    #[tokio::test]
    async fn runs_in_workspace_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let tool = shell(dir.path().to_path_buf(), BTreeMap::new());

        let out = tool
            .invoke(input("ls", Duration::from_secs(5)))
            .await
            .unwrap();
        let Content::Text(text) = &out.content[0] else {
            panic!("expected text");
        };
        assert!(text.contains("marker.txt"));
    }

    #[tokio::test]
    async fn env_overlay_is_visible_to_shell() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shell(
            dir.path().to_path_buf(),
            BTreeMap::from([("OMINI_SHELL_TEST".to_owned(), Some("active".to_owned()))]),
        );

        let out = tool
            .invoke(input(
                "printf %s \"$OMINI_SHELL_TEST\"",
                Duration::from_secs(5),
            ))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, vec![Content::Text("active".to_owned())]);
    }

    #[tokio::test]
    async fn timeout_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = shell(dir.path().to_path_buf(), BTreeMap::new());

        let result = tool
            .invoke(input("sleep 5", Duration::from_millis(50)))
            .await;
        assert!(matches!(result, Err(ToolError::Timeout(_))));
    }
}
