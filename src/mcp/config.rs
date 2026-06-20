//! `mcp.toml` configuration: the MCP servers to launch and how.

use std::collections::HashMap;

use serde::Deserialize;

/// The parsed contents of `.omini/config/mcp.toml` (`doc/tool-protocol.md` §5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct McpConfig {
    /// Each `[[servers]]` table.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One configured MCP server.
///
/// Only the stdio transport is wired: `command` plus `args`/`env` launch a
/// subprocess whose stdin/stdout carry JSON-RPC. A `url` (SSE transport) parses
/// but is not yet supported.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpServerConfig {
    /// Unique name; namespaces the server in monitoring and logs.
    pub name: String,

    /// The executable to spawn (stdio transport). Absent for a `url` server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,

    /// Extra environment variables for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Remote SSE endpoint (parsed; unsupported — see module docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl McpConfig {
    /// Load and merge `config/mcp.toml` from each root (highest priority first;
    /// a server name defined in a higher root shadows a lower one). A missing
    /// file contributes nothing; absent everywhere yields an empty config.
    ///
    /// # Errors
    /// Returns the offending path and parse error if a present file is malformed.
    pub fn load(roots: &[std::path::PathBuf]) -> Result<Self, ConfigError> {
        let mut merged: Vec<McpServerConfig> = Vec::new();
        for root in roots {
            let path = root.join("config").join("mcp.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(ConfigError::Io { path, source }),
            };
            let file: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
            for server in file.servers {
                if !merged.iter().any(|s| s.name == server.name) {
                    merged.push(server);
                }
            }
        }
        Ok(Self { servers: merged })
    }
}

/// Why loading `mcp.toml` failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },
}

impl McpServerConfig {
    /// Whether this server uses the (only supported) stdio transport.
    #[must_use]
    pub const fn is_stdio(&self) -> bool {
        self.command.is_some()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The doc example (`doc/tool-protocol.md` §5.1) parses into stdio + url
    /// servers, and `is_stdio` distinguishes them.
    #[test]
    fn parses_doc_example() {
        let toml_src = r#"
[[servers]]
name = "filesystem-extra"
command = "npx"
args = ["-y", "@anthropic/mcp-filesystem"]
env = { ROOT = "/home/user/projects" }

[[servers]]
name = "remote-rag"
url = "https://my-rag.example.com/sse"
"#;
        let config: McpConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(config.servers.len(), 2);

        let fs = &config.servers[0];
        assert_eq!(fs.name, "filesystem-extra");
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, ["-y", "@anthropic/mcp-filesystem"]);
        assert_eq!(
            fs.env.get("ROOT").map(String::as_str),
            Some("/home/user/projects")
        );
        assert!(fs.is_stdio());

        let rag = &config.servers[1];
        assert_eq!(rag.url.as_deref(), Some("https://my-rag.example.com/sse"));
        assert!(!rag.is_stdio());
    }

    /// A higher-priority root shadows a same-named server in a lower root.
    #[test]
    fn higher_root_shadows_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let high = dir.path().join("high");
        let low = dir.path().join("low");
        for (root, cmd) in [(&high, "high-cmd"), (&low, "low-cmd")] {
            let cfg = root.join("config");
            std::fs::create_dir_all(&cfg).unwrap();
            std::fs::write(
                cfg.join("mcp.toml"),
                format!("[[servers]]\nname = \"shared\"\ncommand = \"{cmd}\"\n"),
            )
            .unwrap();
        }
        let config = McpConfig::load(&[high, low]).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].command.as_deref(), Some("high-cmd"));
    }

    /// No `mcp.toml` anywhere → empty config, not an error.
    #[test]
    fn missing_everywhere_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = McpConfig::load(&[dir.path().to_path_buf()]).unwrap();
        assert!(config.servers.is_empty());
    }
}
