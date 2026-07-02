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

    /// A config value could not be serialized to TOML for writing.
    #[error("failed to serialize {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },

    /// A write was requested but the store has no config root to write into.
    #[error("no config root available to write configuration")]
    NoRoot,

    /// Neither the secret store nor the env var named by `api_key_env` yields a
    /// key for the provider.
    #[error(
        "no API key for provider `{provider}`: not in the secret store and \
         environment variable {env} (api_key_env) is not set"
    )]
    MissingApiKey { provider: String, env: String },

    /// The secret store (`SQLite`) could not be opened or queried.
    #[error(transparent)]
    Secret(#[from] crate::secrets::SecretError),

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
