//! `lsp.toml` configuration: which language servers to launch, and which file
//! extensions each one handles.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The parsed contents of `.omini/config/lsp.toml` (`doc/lsp.md` §3).
///
/// `Serialize` exists for the settings UI's write path (`gateway::langconfig`),
/// which rewrites the layer file the user edited; the runtime only ever
/// deserializes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspConfig {
    /// Each `[[servers]]` table.
    #[serde(default)]
    pub servers: Vec<LspServerConfig>,
}

/// One configured language server. Stdio only, spawned on the host (mirrors
/// [`crate::mcp::McpServerConfig`] — the same trust model applies).
///
/// `Serialize` exists for the settings UI's write path (`gateway::langconfig`);
/// every field is written explicitly (the file the user reads back is the
/// full record they sent, with no skipped defaults to second-guess).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
    /// `"rs"`). A file is routed to every enabled server whose list contains
    /// its extension (a language may have several: `pyright` + `ruff`).
    pub extensions: Vec<String>,

    /// Whether this server is used. A higher-precedence layer sets `false`
    /// under a built-in's `name` to disable that default (tombstone semantics,
    /// `doc/lsp.md` §3). Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

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

pub const fn default_diag_timeout_ms() -> u64 {
    400
}

pub const fn default_enabled() -> bool {
    true
}

pub const fn default_init_timeout_ms() -> u64 {
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
        // Roots are highest-priority first: the FIRST same-named server wins
        // (mirrors `McpConfig::load`), so a higher root shadows a lower one.
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
        // The built-in registry is the lowest-precedence layer. Overlay the
        // user's servers on it: a same-named user server REPLACES the built-in
        // (including an `enabled = false` tombstone that disables it); a new
        // name is appended.
        let mut result: Vec<LspServerConfig> = super::registry::builtin_servers();
        for server in merged {
            if let Some(existing) = result.iter_mut().find(|s| s.name == server.name) {
                *existing = server;
            } else {
                result.push(server);
            }
        }
        // Tombstones applied: drop disabled entries.
        result.retain(|s| s.enabled);
        Ok(Self { servers: result })
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
    /// (same precedence as `McpConfig::load`). The custom server rides on top
    /// of the built-in registry, which is always present as the base layer.
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
        let shared = config.servers.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.command, "high-cmd");
    }

    /// No `lsp.toml` anywhere → the built-in registry alone (out-of-the-box
    /// defaults), not an empty config.
    #[test]
    fn missing_everywhere_yields_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let config = LspConfig::load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(
            config.servers.len(),
            super::super::registry::builtin_servers().len()
        );
        assert!(config.servers.iter().all(|s| s.enabled));
    }

    /// A higher layer disables a built-in by name via `enabled = false`
    /// (tombstone): the entry vanishes from the merged result.
    #[test]
    fn enabled_false_disables_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("lsp.toml"),
            "[[servers]]\nname = \"rust-analyzer\"\ncommand = \"rust-analyzer\"\nextensions = [\"rs\"]\nenabled = false\n",
        )
        .unwrap();
        let config = LspConfig::load(&[dir.path().to_path_buf()]).unwrap();
        assert!(
            !config.servers.iter().any(|s| s.name == "rust-analyzer"),
            "disabled builtin should be dropped"
        );
        // Other builtins survive.
        assert!(config.servers.iter().any(|s| s.name == "pyright"));
    }

    /// A higher layer overrides a built-in's fields (here: the command) while
    /// keeping it a single merged entry.
    #[test]
    fn higher_layer_overrides_builtin_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("lsp.toml"),
            "[[servers]]\nname = \"rust-analyzer\"\ncommand = \"ra-wrapper\"\nextensions = [\"rs\"]\n",
        )
        .unwrap();
        let config = LspConfig::load(&[dir.path().to_path_buf()]).unwrap();
        let ra = config
            .servers
            .iter()
            .find(|s| s.name == "rust-analyzer")
            .unwrap();
        assert_eq!(ra.command, "ra-wrapper");
    }
}
