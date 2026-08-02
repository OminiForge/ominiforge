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
use crate::tool::terminal::Terminal;
use std::time::{Duration, Instant};

/// The live-output callback `Sandbox::exec_streaming` invokes per chunk.
type OutputCallback = Box<dyn for<'a> FnMut(&'a [u8]) + Send>;

/// Minimum interval between live output frames (`doc/tool-streaming.md` §5):
/// output can arrive in bursts, so snapshots are throttled to keep the
/// front-end from re-rendering per chunk. Matches the args-streaming cadence.
const OUTPUT_MIN_INTERVAL: Duration = Duration::from_millis(120);

/// Drives stage-2 output streaming for one `shell` call: a terminal model fed
/// the raw bytes, throttled, rendered to a self-contained "current screen"
/// `terminal` view passed to the agent's progress sink. The terminal model is
/// what makes panel-style commands (progress bars, spinners, full-screen
/// redraws) render as in-place refresh rather than accumulating control
/// sequences (`terminal.rs`).
struct OutputStream {
    command: String,
    terminal: Terminal,
    last_emit: Option<Instant>,
    render: Box<dyn FnMut(String) + Send>,
}

impl OutputStream {
    fn new(command: &str, render: Box<dyn FnMut(String) + Send>) -> Self {
        Self {
            command: command.to_owned(),
            terminal: Terminal::new(),
            last_emit: None,
            render,
        }
    }

    /// The `on_output` callback for `Sandbox::exec_streaming`. The terminal
    /// model and throttle state move into the closure; the exit-code is
    /// unknown mid-stream (`null`), filled in the settled stage-3 view.
    fn into_callback(self) -> OutputCallback {
        let mut this = self;
        Box::new(move |bytes: &[u8]| {
            this.terminal.feed(bytes);
            let due = this
                .last_emit
                .is_none_or(|t| t.elapsed() >= OUTPUT_MIN_INTERVAL);
            if due {
                this.last_emit = Some(Instant::now());
                (this.render)(terminal_view(&this.command, &this.terminal.screen(), None));
            }
        })
    }
}

/// Render a `terminal` view envelope (same shape as `render_output`'s settled
/// view, so the front-end uses one render path). `exit_code` is `None`
/// mid-stream (unknown until the process exits).
fn terminal_view(command: &str, output: &str, exit_code: Option<i32>) -> String {
    serde_json::json!({
        "kind": "terminal",
        "command": command,
        "output": output,
        "exit_code": exit_code,
    })
    .to_string()
}

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

        let map_err = |e: SandboxError| match e {
            SandboxError::Timeout(d) => ToolError::Timeout(d),
            other => ToolError::Execution(other.to_string()),
        };

        // Stage-2 output streaming (`doc/tool-streaming.md` §5): with a
        // progress sink, feed the raw bytes to a terminal model and emit a
        // self-contained "current screen" `terminal` view per throttled frame —
        // ordinary commands grow, panel-style commands refresh in place. The
        // settled stage-3 view is unaffected either way.
        let output = match input.progress {
            Some(render) => {
                let on_output = OutputStream::new(&args.command, render).into_callback();
                self.sandbox
                    .exec_streaming(&args.command, input.timeout, on_output)
                    .await
                    .map_err(map_err)?
            }
            None => self
                .sandbox
                .exec(&args.command, input.timeout)
                .await
                .map_err(map_err)?,
        };

        Ok(render_output(&args.command, &output))
    }
}

/// Combine a finished command's streams into a tool output, flagging non-zero
/// exits as business errors. The UI view is a structured terminal envelope so
/// the front-end renders command + output + exit code without parsing text.
fn render_output(command: &str, output: &ExecOutput) -> ToolOutput {
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

    let view = terminal_view(command, &text, output.exit_code);

    ToolOutput {
        content: vec![
            Content::Text(text),
            Content::TextView {
                text: view,
                audience: crate::core::payload::AUDIENCE_UI.to_owned(),
            },
        ],
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
            progress: None,
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
        // The model-facing text is the raw output; the UI view is a structured
        // terminal envelope.
        assert_eq!(out.content[0], Content::Text("hello\n".to_owned()));
        let view_json: serde_json::Value = match &out.content[1] {
            Content::TextView { text, .. } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected TextView"),
        };
        assert_eq!(view_json["kind"], "terminal");
        assert_eq!(view_json["command"], "echo hello");
        assert_eq!(view_json["output"], "hello\n");
        assert_eq!(view_json["exit_code"], 0);
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
        assert_eq!(out.content[0], Content::Text("active".to_owned()));
        // The UI view is a structured terminal envelope.
        let view_json: serde_json::Value = match &out.content[1] {
            Content::TextView { text, .. } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected TextView"),
        };
        assert_eq!(view_json["kind"], "terminal");
        assert_eq!(view_json["output"], "active");
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
