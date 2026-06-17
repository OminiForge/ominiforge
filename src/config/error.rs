//! Errors for the configuration layer.

use std::path::PathBuf;

/// Result alias for configuration operations.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// Something went wrong loading or resolving configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required config file is absent.
    #[error("config file not found: {0}")]
    NotFound(PathBuf),

    /// A config file could not be read.
    #[error("config io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A config file could not be parsed as TOML.
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// The env var named by a provider's `api_key_env` is unset.
    #[error("environment variable {env} (api_key_env for provider `{provider}`) is not set")]
    MissingApiKey { provider: String, env: String },

    /// A model reference did not resolve to any configured provider/model.
    #[error("unknown model reference `{0}`: no matching provider/model in providers.toml")]
    UnknownModel(String),

    /// A profile referenced a provider name that is not configured.
    #[error("unknown provider `{0}`: not defined in providers.toml")]
    UnknownProvider(String),

    /// The provider's `type` has no built-in adapter yet.
    #[error("provider type `{0}` is not supported yet (only `openai-chat` is wired in Phase 1)")]
    UnsupportedProviderType(String),

    /// A profile's `extends` chain is longer than the allowed depth.
    #[error("profile inheritance chain for `{0}` exceeds the maximum depth of {1}")]
    InheritanceTooDeep(String, usize),

    /// A profile's `extends` chain contains a cycle.
    #[error("profile inheritance cycle detected at `{0}`")]
    InheritanceCycle(String),

    /// No usable model could be determined (profile has no default, no override).
    #[error("no model specified: profile `{0}` has no model.default and no --model override")]
    NoModel(String),
}
