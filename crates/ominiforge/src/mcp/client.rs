//! The stdio MCP client: owns a server subprocess, frames JSON-RPC over its
//! stdin/stdout, and adapts each MCP tool to the core [`Tool`] trait.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use super::config::McpServerConfig;
use super::protocol::{
    self, ContentBlock, Notification, Request, Response, ToolCallResult, ToolDef, ToolsListResult,
};
use crate::core::payload::{Content, ToolOutput, ToolSource};
use crate::process_env::apply_env_overlay;
use crate::tool::{Tool, ToolDescriptor, ToolError, ToolInput, ToolResult};

/// A connected MCP server: the subprocess plus its framed stdio. The subprocess
/// is killed when this is dropped (`Child` with `kill_on_drop`).
///
/// All I/O is serialized behind one mutex: stdio is a single ordered stream, so
/// only one request/response exchange can be in flight at a time.
#[derive(Debug)]
pub struct McpClient {
    io: Mutex<Io>,
    next_id: AtomicU64,
    // Held so the subprocess lives as long as the client and is killed on drop.
    _child: Child,
}

/// The framed read/write halves of the server's stdio.
#[derive(Debug)]
struct Io {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Why an MCP operation failed.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("mcp server `{0}` is not a stdio server (only stdio is supported)")]
    NotStdio(String),
    #[error("failed to spawn mcp server: {0}")]
    Spawn(String),
    #[error("mcp server closed the connection")]
    Closed,
    #[error("mcp io error: {0}")]
    Io(String),
    #[error("mcp protocol error: {0}")]
    Protocol(String),
    #[error("mcp server returned error {code}: {message}")]
    Rpc { code: i64, message: String },
}

impl McpClient {
    /// Spawn `server`, run the `initialize` handshake, and list its tools.
    /// Returns the live client and its advertised tools.
    ///
    /// # Errors
    /// [`McpError`] if the server is not stdio, fails to spawn, or the handshake
    /// / `tools/list` exchange fails.
    pub async fn connect(
        server: &McpServerConfig,
        env_overlay: &BTreeMap<String, Option<String>>,
    ) -> Result<(Self, Vec<ToolDef>), McpError> {
        let command = server
            .command
            .as_deref()
            .ok_or_else(|| McpError::NotStdio(server.name.clone()))?;

        let mut command = tokio::process::Command::new(command);
        command.args(&server.args);
        apply_env_overlay(&mut command, env_overlay);
        let mut child = command
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| McpError::Spawn(e.to_string()))?;

        let stdin = child.stdin.take().ok_or(McpError::Closed)?;
        let stdout = child.stdout.take().ok_or(McpError::Closed)?;
        let client = Self {
            io: Mutex::new(Io {
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: AtomicU64::new(1),
            _child: child,
        };

        client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// The `initialize` handshake: send capabilities, then the
    /// `notifications/initialized` notice MCP requires before normal traffic.
    async fn initialize(&self) -> Result<(), McpError> {
        let params = serde_json::json!({
            "protocolVersion": protocol::PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "ominiforge", "version": env!("CARGO_PKG_VERSION") },
        });
        let _ = self.request("initialize", Some(params)).await?;
        let mut io = self.io.lock().await;
        Self::write_line(
            &mut io.stdin,
            &Notification::new("notifications/initialized", None),
        )
        .await
    }

    /// `tools/list`: the server's advertised tools.
    async fn list_tools(&self) -> Result<Vec<ToolDef>, McpError> {
        let result = self.request("tools/list", None).await?;
        let parsed: ToolsListResult = serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("bad tools/list result: {e}")))?;
        Ok(parsed.tools)
    }

    /// `tools/call`: invoke `name` with `arguments`, returning the result
    /// envelope (content + `isError`).
    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolCallResult, McpError> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.request("tools/call", Some(params)).await?;
        serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("bad tools/call result: {e}")))
    }

    /// Send one JSON-RPC request and read lines until the matching response.
    /// Lines that are notifications or carry a different id are skipped (the
    /// server may interleave them). Returns the `result` value or maps an
    /// `error` object to [`McpError::Rpc`].
    ///
    /// The io lock is intentionally held across the whole write-then-read
    /// exchange: stdio is one ordered stream, so a second request must not
    /// interleave bytes with this one's response.
    #[allow(clippy::significant_drop_tightening)]
    async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut io = self.io.lock().await;
        Self::write_line(&mut io.stdin, &Request::new(id, method, params)).await?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = io
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| McpError::Io(e.to_string()))?;
            if n == 0 {
                return Err(McpError::Closed);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let response: Response = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                // Not a well-formed response line (a log line, a notification);
                // skip it and keep reading for our id.
                Err(_) => continue,
            };
            if response.id != Some(id) {
                continue;
            }
            if let Some(err) = response.error {
                return Err(McpError::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            return Ok(response.result.unwrap_or(serde_json::Value::Null));
        }
    }

    /// Serialize `value` as one JSON line (newline-delimited framing) and flush.
    async fn write_line<T: serde::Serialize + Sync>(
        stdin: &mut ChildStdin,
        value: &T,
    ) -> Result<(), McpError> {
        let mut bytes = serde_json::to_vec(value).map_err(|e| McpError::Protocol(e.to_string()))?;
        bytes.push(b'\n');
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| McpError::Io(e.to_string()))?;
        stdin.flush().await.map_err(|e| McpError::Io(e.to_string()))
    }
}

/// Adapts one MCP tool to the core [`Tool`] trait. Holds a shared handle to its
/// server's [`McpClient`] (so the subprocess stays alive while any of its tools
/// are registered) and the tool's advertised definition.
pub struct McpTool {
    client: Arc<McpClient>,
    server_name: String,
    def: ToolDef,
}

impl McpTool {
    /// Wrap `def` (from `tools/list`) as a dispatchable tool backed by `client`.
    #[must_use]
    pub const fn new(client: Arc<McpClient>, server_name: String, def: ToolDef) -> Self {
        Self {
            client,
            server_name,
            def,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.def.name.clone(),
            description: self.def.description.clone(),
            input_schema: self.def.input_schema.clone(),
        }
    }

    fn source(&self) -> ToolSource {
        ToolSource::Mcp {
            server_name: self.server_name.clone(),
        }
    }

    async fn invoke(&self, input: ToolInput) -> ToolResult {
        // Bound the round-trip by the per-call budget; a hung server is a
        // protocol timeout, not a business error.
        let call = self.client.call_tool(&self.def.name, input.input);
        let result = match tokio::time::timeout(input.timeout, call).await {
            Ok(Ok(result)) => result,
            Ok(Err(McpError::Closed)) => {
                return Err(ToolError::ServerCrashed(format!(
                    "mcp server `{}` closed the connection",
                    self.server_name
                )));
            }
            Ok(Err(e)) => return Err(ToolError::Execution(e.to_string())),
            Err(_) => return Err(ToolError::Timeout(input.timeout)),
        };

        Ok(to_tool_output(result))
    }
}

/// Map an MCP `tools/call` result to a [`ToolOutput`]. Text blocks pass through;
/// any non-text block becomes a placeholder so it is visible but not dropped.
/// `isError` becomes the business-level error flag (`doc/tool-protocol.md` §7.1).
fn to_tool_output(result: ToolCallResult) -> ToolOutput {
    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text { text } => Content::Text(text),
            ContentBlock::Other => Content::Text("[non-text mcp content]".to_owned()),
        })
        .collect();
    ToolOutput {
        content,
        is_error: result.is_error,
        error_code: result.is_error.then(|| "mcp_tool_error".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use super::*;

    /// A mock stdio MCP server in Python: answers `initialize`, advertises one
    /// `echo` tool, and on `tools/call` echoes its `text` argument. Returns
    /// `isError: true` when asked for tool name `boom`. This exercises the full
    /// JSON-RPC framing without depending on an external server binary.
    const MOCK_SERVER: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2024-11-05"}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [
            {"name": "echo", "description": "echo back", "inputSchema": {"type": "object"}}
        ]}})
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name")
        args = params.get("arguments", {})
        if name == "boom":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "kaboom"}], "isError": True}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": args.get("text", "")}]}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}})
"#;

    /// Write the mock server to a temp file and build a stdio server config that
    /// launches it via `python3`.
    fn mock_server(dir: &std::path::Path) -> McpServerConfig {
        let script = dir.join("mock_mcp.py");
        std::fs::write(&script, MOCK_SERVER).unwrap();
        McpServerConfig {
            name: "mock".to_owned(),
            command: Some("python3".to_owned()),
            args: vec![script.to_string_lossy().into_owned()],
            env: std::collections::HashMap::new(),
            url: None,
        }
    }

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput {
            call_id: "c1".to_owned(),
            input: value,
            timeout: Duration::from_secs(5),
        }
    }

    /// connect → handshake → tools/list surfaces the advertised tool, adapted
    /// through the core `Tool` trait with `ToolSource::Mcp`.
    #[tokio::test]
    async fn connect_lists_tools_with_mcp_source() {
        let dir = tempfile::tempdir().unwrap();
        let (client, tools) = McpClient::connect(&mock_server(dir.path()), &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let tool = McpTool::new(Arc::new(client), "mock".to_owned(), tools[0].clone());
        assert_eq!(tool.descriptor().name, "echo");
        assert_eq!(
            tool.source(),
            ToolSource::Mcp {
                server_name: "mock".to_owned()
            }
        );
    }

    /// A real `tools/call` round-trip returns the echoed text as tool output.
    #[tokio::test]
    async fn invoke_round_trips_through_stdio() {
        let dir = tempfile::tempdir().unwrap();
        let (client, tools) = McpClient::connect(&mock_server(dir.path()), &BTreeMap::new())
            .await
            .unwrap();
        let tool = McpTool::new(Arc::new(client), "mock".to_owned(), tools[0].clone());

        let out = tool
            .invoke(input(serde_json::json!({ "text": "hello mcp" })))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, vec![Content::Text("hello mcp".to_owned())]);
    }

    /// MCP `isError: true` maps to a business-level error output (not a protocol
    /// `Err`), carrying the `mcp_tool_error` code.
    #[tokio::test]
    async fn is_error_maps_to_business_error() {
        let dir = tempfile::tempdir().unwrap();
        let server = mock_server(dir.path());
        let (client, _tools) = McpClient::connect(&server, &BTreeMap::new()).await.unwrap();
        // The mock returns isError for tool name `boom`; advertise it directly.
        let boom = ToolDef {
            name: "boom".to_owned(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
        };
        let tool = McpTool::new(Arc::new(client), "mock".to_owned(), boom);

        let out = tool.invoke(input(serde_json::json!({}))).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code.as_deref(), Some("mcp_tool_error"));
    }
}
