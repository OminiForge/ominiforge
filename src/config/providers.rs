//! The `providers.toml` data model: connection info, model metadata, pricing.
//!
//! Mirrors `doc/profile.md` §2. A provider owns connection details (endpoint,
//! protocol, the *name* of the env var holding its API key) and a list of
//! models it serves. Profiles reference a model as `provider_name/model_id`.
//!
//! Secrets never live here: `api_key_env` names an environment variable, and
//! the key is read from the process environment at build time (architecture
//! §15).

use serde::{Deserialize, Serialize};

/// The parsed contents of a `providers.toml` file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProvidersFile {
    /// Each `[[providers]]` table.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

/// One configured provider and the models it serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Unique identifier; profiles reference it in `provider_name/model_id`.
    pub name: String,

    /// Wire protocol. See [`ProviderType`].
    #[serde(rename = "type")]
    pub provider_type: ProviderType,

    /// API endpoint root (e.g. `https://api.openai.com/v1`).
    pub base_url: String,

    /// The name of the environment variable holding this provider's API key.
    /// The key itself is never stored in config.
    pub api_key_env: String,

    /// Models this provider serves.
    #[serde(default, rename = "models")]
    pub models: Vec<ModelConfig>,
}

impl ProviderConfig {
    /// Find a model by its `id`.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// The wire protocol a provider speaks.
///
/// Only `openai-chat` has a wired adapter in Phase 1; the others parse so full
/// config files load, but selecting them surfaces
/// [`ConfigError::UnsupportedProviderType`](super::ConfigError).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderType {
    /// OpenAI Chat Completions API — compatible with many third parties.
    OpenaiChat,
    /// OpenAI legacy Completions API.
    OpenaiCompletion,
    /// Anthropic Messages API.
    Anthropic,
    /// A custom adapter (requires implementing the provider trait).
    Custom,
}

impl ProviderType {
    /// The string form as it appears in `providers.toml`, for error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai-chat",
            Self::OpenaiCompletion => "openai-completion",
            Self::Anthropic => "anthropic",
            Self::Custom => "custom",
        }
    }
}

/// A model offered by a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// The model identifier sent to the API (e.g. `gpt-4o`).
    pub id: String,

    /// Maximum context window in tokens.
    pub context_window: u32,

    /// Maximum output tokens.
    pub max_output_tokens: u32,

    /// Recommended default sampling temperature. Defaults to 0.0.
    #[serde(default)]
    pub default_temperature: f32,

    /// Cost metadata for the monitor's estimation (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
}

/// Per-million-token pricing, used by the monitor to estimate cost. Stored in
/// config (not in the event stream) so history can be recomputed with current
/// prices. See `doc/monitor.md` §6.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Pricing {
    #[serde(default)]
    pub input_per_million: f64,
    #[serde(default)]
    pub output_per_million: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_million: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_million: Option<f64>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    /// The full example from `doc/profile.md` §2 must parse, including inline
    /// pricing tables and the array-of-tables `[[providers.models]]` shape.
    #[test]
    fn parses_doc_example() {
        let toml_src = r#"
[[providers]]
name = "openai-main"
type = "openai-chat"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.0
pricing = { input_per_million = 2.50, output_per_million = 10.00, cache_read_per_million = 1.25 }

[[providers.models]]
id = "gpt-4o-mini"
context_window = 128000
max_output_tokens = 16384
pricing = { input_per_million = 0.15, output_per_million = 0.60 }

[[providers]]
name = "anthropic"
type = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[[providers.models]]
id = "claude-sonnet-4-6"
context_window = 200000
max_output_tokens = 16000
"#;
        let parsed: ProvidersFile = toml::from_str(toml_src).unwrap();
        assert_eq!(parsed.providers.len(), 2);

        let openai = &parsed.providers[0];
        assert_eq!(openai.name, "openai-main");
        assert_eq!(openai.provider_type, ProviderType::OpenaiChat);
        assert_eq!(openai.models.len(), 2);

        let gpt4o = openai.model("gpt-4o").unwrap();
        assert_eq!(gpt4o.context_window, 128_000);
        let pricing = gpt4o.pricing.unwrap();
        assert_eq!(pricing.input_per_million, 2.50);
        assert_eq!(pricing.cache_read_per_million, Some(1.25));

        // gpt-4o-mini omits default_temperature and cache pricing → defaults.
        let mini = openai.model("gpt-4o-mini").unwrap();
        assert_eq!(mini.default_temperature, 0.0);
        assert_eq!(mini.pricing.unwrap().cache_read_per_million, None);

        assert_eq!(parsed.providers[1].provider_type, ProviderType::Anthropic);
    }

    #[test]
    fn provider_type_round_trips_kebab_case() {
        #[derive(Deserialize)]
        struct Wrap {
            t: ProviderType,
        }

        let value: ProviderType = toml::from_str("t = \"openai-chat\"")
            .map(|w: Wrap| w.t)
            .unwrap();
        assert_eq!(value, ProviderType::OpenaiChat);
        assert_eq!(ProviderType::OpenaiChat.as_str(), "openai-chat");
    }
}
