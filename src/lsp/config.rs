//! `lsp.toml` configuration: which language servers to launch, and which file
//! extensions each one handles.

use std::collections::HashMap;

use serde::Deserialize;

/// The parsed contents of `.omini/config/lsp.toml` (`doc/lsp.md` §3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LspConfig {
    /// Each `[[servers]]` table.
    #[serde(default)]
    pub servers: Vec<LspServerConfig>,
}

/// One configured language server. Stdio only, spawned on the host (mirrors
/// [`crate::mcp::McpServerConfig`] — the same trust model applies).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LspServerConfig {
    /// Unique name; namespaces the server in logs.
    pub name: String,

    /// The executable to spawn.
    pub command: String,

    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,

    /// Extra environment variables for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// File extensions this server handles, without the leading dot (e.g.
    /// `"rs"`). A file is routed to the first server whose list contains its
    /// extension.
    pub extensions: Vec<String>,

    /// Milliseconds a `read`/`edit`/`write` op waits for fresh diagnostics
    /// after syncing the doc before giving up and returning without them. Caps
    /// the LSP overhead on every file op — see the performance model in
    /// `doc/lsp.md` §4.
    #[serde(default = "default_diag_timeout_ms")]
    pub diag_timeout_ms: u64,

    /// Milliseconds the *first* touch of this language waits for the
    /// `initialize` handshake before proceeding without diagnostics. Full
    /// workspace indexing continues in the background regardless of this
    /// timeout — it only bounds how long that first op stalls.
    #[serde(default = "default_init_timeout_ms")]
    pub init_timeout_ms: u64,
}

const fn default_diag_timeout_ms() -> u64 {
    400
}

const fn default_init_timeout_ms() -> u64 {
    2_000
}

impl LspConfig {
    /// Load and merge `config/lsp.toml` from each root (highest priority
    /// first; a server name defined in a higher root shadows a lower one). A
    /// missing file contributes nothing; absent everywhere yields an empty
    /// config. Mirrors [`crate::mcp::McpConfig::load`].
    ///
    /// # Errors
    /// Returns the offending path and parse error if a present file is
    /// malformed.
    pub fn load(roots: &[std::path::PathBuf]) -> Result<Self, ConfigError> {
        let mut merged: Vec<LspServerConfig> = Vec::new();
        for root in roots {
            let path = root.join("config").join("lsp.toml");
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

/// Why loading `lsp.toml` failed.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A representative `lsp.toml` parses into a stdio server with its
    /// extension routing and default timeouts intact.
    #[test]
    fn parses_representative_config() {
        let toml_src = r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
extensions = ["rs"]
"#;
        let config: LspConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(config.servers.len(), 1);
        let ra = &config.servers[0];
        assert_eq!(ra.name, "rust-analyzer");
        assert_eq!(ra.command, "rust-analyzer");
        assert_eq!(ra.extensions, ["rs"]);
        assert_eq!(ra.diag_timeout_ms, 400);
        assert_eq!(ra.init_timeout_ms, 2_000);
    }

    /// Explicit timeouts override the defaults.
    #[test]
    fn explicit_timeouts_override_defaults() {
        let toml_src = r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
extensions = ["rs"]
diag_timeout_ms = 800
init_timeout_ms = 5000
"#;
        let config: LspConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(config.servers[0].diag_timeout_ms, 800);
        assert_eq!(config.servers[0].init_timeout_ms, 5_000);
    }

    /// A higher-priority root shadows a same-named server in a lower root
    /// (same precedence as `McpConfig::load`).
    #[test]
    fn higher_root_shadows_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let high = dir.path().join("high");
        let low = dir.path().join("low");
        for (root, cmd) in [(&high, "high-cmd"), (&low, "low-cmd")] {
            let cfg = root.join("config");
            std::fs::create_dir_all(&cfg).unwrap();
            std::fs::write(
                cfg.join("lsp.toml"),
                format!(
                    "[[servers]]\nname = \"shared\"\ncommand = \"{cmd}\"\nextensions = [\"rs\"]\n"
                ),
            )
            .unwrap();
        }
        let config = LspConfig::load(&[high, low]).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].command, "high-cmd");
    }

    /// No `lsp.toml` anywhere → empty config, not an error.
    #[test]
    fn missing_everywhere_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = LspConfig::load(&[dir.path().to_path_buf()]).unwrap();
        assert!(config.servers.is_empty());
    }
}
