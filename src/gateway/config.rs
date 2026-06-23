//! Gateway configuration: `.omini/config/gateway.toml`.
//!
//! Mirrors the `mcp.toml` / `hooks.toml` loading pattern (multi-root merge,
//! highest-priority root wins). Holds only network/lifecycle settings; agent
//! identity stays in profiles (`doc/profile.md`), and the API key — like every
//! secret — is named by env var, never stored inline (`doc/architecture.md`
//! §15).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;

/// Default bind address: loopback only. Public exposure is the reverse proxy's
/// job (`doc/architecture.md` §18.1, TLS terminated upstream), so the gateway
/// itself never listens on a public interface by default.
const DEFAULT_BIND: &str = "127.0.0.1:7878";

/// Default idle timeout before a live session actor is evicted (releasing its
/// event-log lock so the CLI/TUI can reopen it). 30 minutes.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;

/// Parsed `gateway.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Socket address to bind (host:port). Loopback by default.
    pub bind: String,

    /// Name of the environment variable holding the bearer token. When set, all
    /// routes except `/healthz` require `Authorization: Bearer <token>`. When
    /// unset (or the env var is empty), the gateway runs **unauthenticated** —
    /// only acceptable behind loopback + a trusted proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,

    /// Idle seconds before an inactive session actor is shut down and evicted.
    pub idle_timeout_secs: u64,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.to_owned(),
            api_key_env: None,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
        }
    }
}

impl GatewayConfig {
    /// Load `config/gateway.toml` from the first root that has one (project
    /// `.omini` before `~/.omini`). Absent everywhere → defaults.
    ///
    /// Unlike `mcp.toml` (a list that merges across roots), gateway config is a
    /// single record, so the highest-priority file wins wholesale rather than
    /// merging field-by-field — simpler and matches "one gateway per host".
    ///
    /// # Errors
    /// [`ConfigError::Parse`] / [`ConfigError::Io`] if a present file is
    /// malformed or unreadable.
    pub fn load(roots: &[PathBuf]) -> Result<Self, ConfigError> {
        for root in roots {
            let path = root.join("config").join("gateway.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(ConfigError::Io { path, source }),
            };
            let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
            return Ok(config);
        }
        Ok(Self::default())
    }

    /// Resolve the bearer token from the configured env var. `None` means no
    /// auth is configured (open gateway); `Some("")` is treated as `None` so an
    /// empty env var does not silently configure an unguessable-but-empty token.
    #[must_use]
    pub fn resolve_api_key(&self) -> Option<String> {
        let var = self.api_key_env.as_ref()?;
        match std::env::var(var) {
            Ok(key) if !key.is_empty() => Some(key),
            _ => None,
        }
    }

    /// Idle timeout as a [`std::time::Duration`].
    #[must_use]
    pub const fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Absent config everywhere yields defaults (loopback, no auth, 30-min idle).
    #[test]
    fn absent_config_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = GatewayConfig::load(&[dir.path().to_owned()]).unwrap();
        assert_eq!(config.bind, DEFAULT_BIND);
        assert!(config.api_key_env.is_none());
        assert_eq!(config.idle_timeout_secs, DEFAULT_IDLE_TIMEOUT_SECS);
    }

    /// A present file overrides defaults; partial files keep defaults for the
    /// rest (`#[serde(default)]`).
    #[test]
    fn partial_file_keeps_defaults_for_unset_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("gateway.toml"),
            "bind = \"0.0.0.0:9000\"\napi_key_env = \"OMINI_GATEWAY_KEY\"\n",
        )
        .unwrap();

        let config = GatewayConfig::load(&[dir.path().to_owned()]).unwrap();
        assert_eq!(config.bind, "0.0.0.0:9000");
        assert_eq!(config.api_key_env.as_deref(), Some("OMINI_GATEWAY_KEY"));
        // Unset field falls back to default.
        assert_eq!(config.idle_timeout_secs, DEFAULT_IDLE_TIMEOUT_SECS);
    }

    /// The first root with a file wins; lower-priority roots are not consulted.
    #[test]
    fn highest_priority_root_wins() {
        let dir = tempfile::tempdir().unwrap();
        let high = dir.path().join("high");
        let low = dir.path().join("low");
        for (root, bind) in [(&high, "1.1.1.1:1"), (&low, "2.2.2.2:2")] {
            let c = root.join("config");
            std::fs::create_dir_all(&c).unwrap();
            std::fs::write(c.join("gateway.toml"), format!("bind = \"{bind}\"\n")).unwrap();
        }
        let config = GatewayConfig::load(&[high, low]).unwrap();
        assert_eq!(config.bind, "1.1.1.1:1");
    }

    /// No `api_key_env` configured → no key resolved (open gateway).
    #[test]
    fn no_api_key_env_resolves_to_none() {
        let config = GatewayConfig::default();
        assert!(config.resolve_api_key().is_none());
    }
}
