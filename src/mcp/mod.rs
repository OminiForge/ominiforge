//! MCP client: server lifecycle, JSON-RPC over stdio, and the adapter that
//! exposes MCP tools through the core [`Tool`] trait.
//!
//! MCP is the one external extension mechanism (`doc/tool-protocol.md` §1, §5):
//! an MCP server is an ordinary subprocess speaking JSON-RPC 2.0 over
//! stdin/stdout. We spawn it, run the `initialize` handshake, list its tools,
//! and wrap each one as an [`McpTool`] that the agent loop dispatches exactly
//! like a built-in — only [`Tool::source`] differs, so monitoring can attribute
//! latency/failures to the right server.
//!
//! Only the stdio transport is implemented; SSE is deferred (`doc/tool-protocol.md`
//! §5.1). The client serializes requests behind a mutex: MCP over stdio is a
//! single ordered byte stream, so concurrent tool calls on one server must take
//! turns.
//!
//! [`Tool`]: crate::tool::Tool

mod client;
mod config;
mod protocol;

pub use client::{McpClient, McpError};
pub use config::{McpConfig, McpServerConfig};

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::tool::ToolRegistry;

/// Connect every server in `config`, list its tools, and register them.
///
/// A server that fails to spawn, handshake, or list tools is logged and
/// skipped — one broken server must not abort the agent
/// (`doc/tool-protocol.md` §12). Returns the live clients so the caller keeps
/// them alive for the session (dropping a client kills its subprocess).
///
/// # Errors
/// Never returns `Err`: per-server failures are reported via `on_warn` and
/// skipped. The signature stays `Result` for forward compatibility.
pub async fn connect_all(
    config: &McpConfig,
    env_overlay: &BTreeMap<String, Option<String>>,
    registry: &mut ToolRegistry,
    on_warn: impl Fn(&str),
) -> Vec<Arc<McpClient>> {
    let mut clients = Vec::new();
    for server in &config.servers {
        match McpClient::connect(server, env_overlay).await {
            Ok((client, tools)) => {
                let client = Arc::new(client);
                let count = tools.len();
                for tool in tools {
                    registry.register(Arc::new(client::McpTool::new(
                        Arc::clone(&client),
                        server.name.clone(),
                        tool,
                    )));
                }
                on_warn(&format!(
                    "mcp: connected `{}` ({count} tool(s))",
                    server.name
                ));
                clients.push(client);
            }
            Err(e) => on_warn(&format!("mcp: skipping `{}` — {e}", server.name)),
        }
    }
    clients
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::payload::ToolSource;

    /// A minimal stdio MCP server advertising one `ping` tool, used to verify the
    /// public `connect_all` entry point registers MCP tools end-to-end.
    const MOCK: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    m = json.loads(line); mid = m.get("id"); method = m.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05"}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"ping","inputSchema":{}}]}})
"#;

    /// `connect_all` spawns the configured server and registers its tools in the
    /// shared registry, attributed to the MCP source — the exact path `prepare`
    /// drives at startup.
    #[tokio::test]
    async fn connect_all_registers_mcp_tools() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mock.py");
        std::fs::write(&script, MOCK).unwrap();
        let config = McpConfig {
            servers: vec![McpServerConfig {
                name: "mock".to_owned(),
                command: Some("python3".to_owned()),
                args: vec![script.to_string_lossy().into_owned()],
                env: std::collections::HashMap::new(),
                url: None,
            }],
        };

        let mut registry = ToolRegistry::new();
        let clients = connect_all(&config, &BTreeMap::new(), &mut registry, |_| {}).await;

        assert_eq!(clients.len(), 1);
        assert_eq!(
            registry.source_of("ping"),
            ToolSource::Mcp {
                server_name: "mock".to_owned()
            }
        );
    }

    /// A server that cannot spawn is reported and skipped, never fatal: the
    /// registry stays empty and no client is returned.
    #[tokio::test]
    async fn broken_server_is_skipped() {
        let config = McpConfig {
            servers: vec![McpServerConfig {
                name: "nope".to_owned(),
                command: Some("definitely-not-a-real-binary-xyz".to_owned()),
                args: vec![],
                env: std::collections::HashMap::new(),
                url: None,
            }],
        };
        let mut registry = ToolRegistry::new();
        let warned = std::cell::Cell::new(false);
        let clients = connect_all(&config, &BTreeMap::new(), &mut registry, |_| {
            warned.set(true);
        })
        .await;

        assert!(clients.is_empty());
        assert!(registry.is_empty());
        assert!(warned.get());
    }
}
