//! The `shell` built-in tool: run a command in the workspace.
//!
//! Phase 1 has no sandbox (`doc/sandbox.md` §3): the command runs via the
//! system shell with the workspace as its working directory, bounded only by a
//! timeout. Container isolation and resource limits are Phase 2.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use serde::Deserialize;

use super::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult};
use crate::core::payload::{Content, ToolOutput};
use crate::process_env::apply_env_overlay;

/// Runs a shell command with the workspace as the working directory.
#[derive(Debug, Clone)]
pub struct ShellTool {
    workspace: PathBuf,
    env_overlay: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
}

impl ShellTool {
    /// Create a `shell` tool rooted at `workspace`.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(workspace: PathBuf, env_overlay: BTreeMap<String, Option<String>>) -> Self {
        Self {
            workspace,
            env_overlay,
        }
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

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.workspace);
        apply_env_overlay(&mut command, &self.env_overlay);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = command
            .spawn()
            .map_err(|e| ToolError::Execution(format!("failed to spawn shell: {e}")))?;

        let output = match tokio::time::timeout(input.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(ToolError::Execution(e.to_string())),
            Err(_) => return Err(ToolError::Timeout(input.timeout)),
        };

        Ok(render_output(&output))
    }
}

/// Combine a finished process's streams into a tool output, flagging non-zero
/// exits as business errors.
fn render_output(output: &std::process::Output) -> ToolOutput {
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }

    let success = output.status.success();
    let error_code = (!success).then(|| {
        output
            .status
            .code()
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
    use std::time::Duration;

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
        let tool = ShellTool::new(dir.path().to_path_buf(), BTreeMap::new());

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
        let tool = ShellTool::new(dir.path().to_path_buf(), BTreeMap::new());

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
        let tool = ShellTool::new(dir.path().to_path_buf(), BTreeMap::new());

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
        let tool = ShellTool::new(
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
        let tool = ShellTool::new(dir.path().to_path_buf(), BTreeMap::new());

        let result = tool
            .invoke(input("sleep 5", Duration::from_millis(50)))
            .await;
        assert!(matches!(result, Err(ToolError::Timeout(_))));
    }
}
