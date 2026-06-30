//! Configuration layer: providers, profiles, and their resolution.
//!
//! This module turns on-disk config (`doc/profile.md` §8) into the concrete
//! settings the agent loop needs. It is data-only — it depends on `core` and
//! `llm` but builds no provider; the CLI maps a [`ResolvedModel`] to a concrete
//! [`crate::llm::Provider`].
//!
//! Layout discovered (architecture §15, project overrides user):
//!
//! ```text
//! <root>/                       # project ./.omini  then  ~/.omini
//!   config/providers.toml       # provider + model definitions
//!   profiles/<name>.toml        # agent profiles
//! ```
//!
//! Secrets are never read from files: a provider names an env var in
//! `api_key_env`, and the key is read from the process environment here.

mod error;
mod profile;
mod providers;

pub use error::{ConfigError, Result};
pub use profile::{DEFAULT_SYSTEM_PROMPT, Profile, ProfileMeta, PromptSection, ToolsSection};
pub use providers::{ModelConfig, Pricing, ProviderConfig, ProviderType, ProvidersFile};

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const CONFIG_SUBDIR: &str = "config";
const PROFILES_SUBDIR: &str = "profiles";
const PROVIDERS_FILE: &str = "providers.toml";
const PRICING_FILE: &str = "pricing.toml";
const OMINI_DIR: &str = ".omini";
const MAX_INHERITANCE_DEPTH: usize = 5;

/// A fully-resolved model selection: everything needed to construct a provider
/// and configure a turn, with profile/CLI overrides already applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    /// Provider name (e.g. `openai-main`).
    pub provider_name: String,
    /// Wire protocol.
    pub provider_type: ProviderType,
    /// API endpoint root.
    pub base_url: String,
    /// The API key, read from the env var named by `api_key_env`.
    pub api_key: String,
    /// Model id sent to the API (e.g. `gpt-4o`).
    pub model_id: String,
    /// Effective temperature (CLI > profile > model default).
    pub temperature: f32,
    /// Effective output-token cap (profile override > model default).
    pub max_output_tokens: u32,
    /// The model's context window (for later compaction logic).
    pub context_window: u32,
    /// Pricing, if configured (for the monitor).
    pub pricing: Option<Pricing>,
}

/// A profile's listable identity: its name and human-readable description.
///
/// Surfaced to a front-end choosing a profile for a new session (`doc/profile.md`
/// §3.1). Deliberately shallow — enumerating profiles must not resolve the
/// `extends` chain (a broken parent must not hide a usable child).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ProfileSummary {
    /// Profile name (the `<name>.toml` the session binds to).
    pub name: String,
    /// Human-readable description from `[profile].description`, if set.
    pub description: Option<String>,
}

/// One selectable model offered by a provider, for a per-session override.
///
/// The override is sent back as `provider/model_id` (the qualified identity),
/// since two providers may serve the same `model_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ModelSummary {
    /// Provider name (e.g. `openai-main`).
    pub provider: String,
    /// Model id sent to the API (e.g. `gpt-4o`).
    pub model_id: String,
    /// Maximum context window in tokens (shown alongside the model).
    pub context_window: u32,
}

/// Loads and resolves configuration from one or more `.omini` roots.
///
/// Roots are searched in priority order: explicit `--config-dir`, then launch
/// cwd, then user home. Independent of any session workspace
/// ([`discover_with`](Self::discover_with)).
#[derive(Debug, Clone)]
pub struct ConfigStore {
    /// Config roots, highest priority first.
    roots: Vec<PathBuf>,
}

impl ConfigStore {
    /// Discover config roots in priority order, highest first:
    /// `--config-dir` (explicit) → launch cwd → user home. Each contributes its
    /// `.omini` subdir if present; absent ones are simply skipped, and duplicates
    /// (e.g. `--config-dir .` while launched there) collapse to one.
    ///
    /// Config discovery is deliberately **independent of the workspace**: a
    /// session can run in any workspace (the web client picks one per session),
    /// but config always comes from where `ominiforge` was launched (or an
    /// explicit `--config-dir`), never from the session's workspace.
    #[must_use]
    pub fn discover_with(config_dir: Option<&Path>, launch_cwd: &Path) -> Self {
        let mut roots = Vec::new();
        let mut push = |dir: PathBuf| {
            let root = dir.join(OMINI_DIR);
            if !roots.contains(&root) {
                roots.push(root);
            }
        };
        if let Some(explicit) = config_dir {
            push(explicit.to_path_buf());
        }
        push(launch_cwd.to_path_buf());
        if let Some(home) = home_dir() {
            push(home);
        }
        Self::from_roots(roots)
    }

    /// Discover config roots from `cwd` (launch directory) then user home, with
    /// no explicit `--config-dir`. Thin shim over
    /// [`discover_with`](Self::discover_with); kept for call sites and tests that
    /// have only a launch directory.
    #[must_use]
    pub fn discover(cwd: &Path) -> Self {
        Self::discover_with(None, cwd)
    }

    /// Build a store over explicit roots (highest priority first). Mainly for
    /// tests; [`discover`](Self::discover) is the normal entry point.
    #[must_use]
    pub const fn from_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// The config roots, highest priority first.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Load and merge every root's `config/providers.toml`. A provider defined
    /// in a higher-priority root shadows a same-named one in a lower root.
    ///
    /// # Errors
    /// [`ConfigError::Parse`] / [`ConfigError::Io`] on a malformed or unreadable
    /// file. A missing file is not an error (that root simply contributes none).
    pub fn load_providers(&self) -> Result<ProvidersFile> {
        let mut merged: Vec<ProviderConfig> = Vec::new();
        for root in &self.roots {
            let path = root.join(CONFIG_SUBDIR).join(PROVIDERS_FILE);
            let Some(text) = read_optional(&path)? else {
                continue;
            };
            let file: ProvidersFile =
                toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.clone(),
                    source,
                })?;
            for provider in file.providers {
                if !merged.iter().any(|p| p.name == provider.name) {
                    merged.push(provider);
                }
            }
        }
        Ok(ProvidersFile { providers: merged })
    }

    /// Build the model→pricing table the monitor uses to derive cost
    /// (`doc/monitor.md` §6.2). Pricing comes from two sources, merged with
    /// `pricing.toml` winning: the inline `pricing` on each model in
    /// `providers.toml` (a sensible default shipped with the model), overridden
    /// by an explicit `.omini/config/pricing.toml` (so a user can update prices
    /// without touching provider definitions). A missing `pricing.toml` is fine.
    ///
    /// # Errors
    /// [`ConfigError::Parse`] / [`ConfigError::Io`] on a malformed or unreadable
    /// `pricing.toml`.
    pub fn load_pricing(&self, providers: &ProvidersFile) -> Result<HashMap<String, Pricing>> {
        let mut table: HashMap<String, Pricing> = HashMap::new();

        // Base layer: inline pricing from providers.toml.
        for provider in &providers.providers {
            for model in &provider.models {
                if let Some(pricing) = model.pricing {
                    table.entry(model.id.clone()).or_insert(pricing);
                }
            }
        }

        // Override layer: pricing.toml (highest-priority root wins per id).
        for root in &self.roots {
            let path = root.join(CONFIG_SUBDIR).join(PRICING_FILE);
            let Some(text) = read_optional(&path)? else {
                continue;
            };
            let file: PricingFile = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
            for (id, pricing) in file.models {
                table.insert(id, pricing);
            }
        }

        Ok(table)
    }

    // __APPEND_MARKER__

    /// Load a profile by name, resolving its `extends` chain and reading any
    /// `system_file`. Returns the [`Profile::builtin_default`] if `name` is
    /// `"default"` and no `default.toml` exists anywhere.
    ///
    /// # Errors
    /// [`ConfigError::NotFound`] if a named (non-default) profile is missing,
    /// parse/io errors, or [`ConfigError::InheritanceTooDeep`] /
    /// [`ConfigError::InheritanceCycle`] on a bad `extends` chain.
    pub fn load_profile(&self, name: &str) -> Result<Profile> {
        self.load_profile_inner(name, &mut Vec::new())
    }

    fn load_profile_inner(&self, name: &str, seen: &mut Vec<String>) -> Result<Profile> {
        if seen.iter().any(|s| s == name) {
            return Err(ConfigError::InheritanceCycle(name.to_owned()));
        }
        if seen.len() >= MAX_INHERITANCE_DEPTH {
            return Err(ConfigError::InheritanceTooDeep(
                name.to_owned(),
                MAX_INHERITANCE_DEPTH,
            ));
        }
        seen.push(name.to_owned());

        let Some((mut profile, dir)) = self.find_profile(name)? else {
            // A missing "default" profile falls back to the hardcoded one; any
            // other missing name is an error.
            if name == "default" && seen.len() == 1 {
                return Ok(Profile::builtin_default());
            }
            return Err(ConfigError::NotFound(self.profile_path(name)));
        };

        // Resolve system_file against the profile's own directory before any
        // overlay, so each level reads its own prompt file.
        resolve_system_file(&mut profile, &dir)?;

        match profile.profile.extends.clone() {
            Some(parent_name) => {
                let parent = self.load_profile_inner(&parent_name, seen)?;
                Ok(profile.overlay_onto(parent))
            }
            None => Ok(profile),
        }
    }

    /// Find a profile file across roots (highest priority first), returning the
    /// parsed profile and the directory it was loaded from.
    fn find_profile(&self, name: &str) -> Result<Option<(Profile, PathBuf)>> {
        for root in &self.roots {
            let dir = root.join(PROFILES_SUBDIR);
            let path = dir.join(format!("{name}.toml"));
            let Some(text) = read_optional(&path)? else {
                continue;
            };
            let profile: Profile = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
            return Ok(Some((profile, dir)));
        }
        Ok(None)
    }

    /// The path the highest-priority root would use for profile `name` (for
    /// error messages).
    fn profile_path(&self, name: &str) -> PathBuf {
        let root = self.roots.first().cloned().unwrap_or_default();
        root.join(PROFILES_SUBDIR).join(format!("{name}.toml"))
    }

    /// List every profile across the config roots: each `<root>/profiles/*.toml`,
    /// deduped by name (a higher-priority root shadows a same-named profile in a
    /// lower one, mirroring [`load_providers`](Self::load_providers)).
    ///
    /// Deliberately infallible and shallow: it parses only each file's
    /// `[profile]` table (name + description) and does **not** resolve `extends`,
    /// so a profile with a broken parent still lists. A file that fails to parse
    /// or read is skipped with a warning via `on_warn` (same posture as a broken
    /// MCP server / hook — one bad profile must not blank the whole list).
    #[must_use]
    pub fn list_profiles(&self, on_warn: &(dyn Fn(&str) + Sync)) -> Vec<ProfileSummary> {
        /// Minimal view over a profile file: just its `[profile]` table. Parsing
        /// this instead of the full [`Profile`] keeps enumeration cheap and
        /// tolerant of sections this build does not yet act on.
        #[derive(serde::Deserialize)]
        struct ProfileHead {
            profile: ProfileMeta,
        }

        let mut summaries: Vec<ProfileSummary> = Vec::new();
        for root in &self.roots {
            let dir = root.join(PROFILES_SUBDIR);
            // A missing profiles/ dir is normal (that root contributes none).
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(e) => {
                        on_warn(&format!("skipping profile {}: {e}", path.display()));
                        continue;
                    }
                };
                match toml::from_str::<ProfileHead>(&text) {
                    Ok(head) => {
                        // Higher-priority root wins: skip a name already seen.
                        if !summaries.iter().any(|s| s.name == head.profile.name) {
                            summaries.push(ProfileSummary {
                                name: head.profile.name,
                                description: head.profile.description,
                            });
                        }
                    }
                    Err(e) => {
                        on_warn(&format!("skipping profile {}: {e}", path.display()));
                    }
                }
            }
        }
        summaries
    }

    /// Flatten every configured provider's models into a selectable list for a
    /// per-session model override. Order follows `providers.toml` (provider, then
    /// model order within it), stable across calls so a front-end never reorders.
    ///
    /// # Errors
    /// Propagates [`load_providers`](Self::load_providers) failures (malformed
    /// `providers.toml`).
    pub fn list_models(&self) -> Result<Vec<ModelSummary>> {
        let providers = self.load_providers()?;
        let models = providers
            .providers
            .iter()
            .flat_map(|p| {
                p.models.iter().map(move |m| ModelSummary {
                    provider: p.name.clone(),
                    model_id: m.id.clone(),
                    context_window: m.context_window,
                })
            })
            .collect();
        Ok(models)
    }

    // __APPEND_MARKER2__

    /// Resolve a model selection into a [`ResolvedModel`], applying overrides.
    ///
    /// Precedence: `model_override` (CLI `--model`) wins over
    /// `profile.model.default`. Temperature: `temperature_override` (CLI) wins
    /// over `profile.model.temperature`, then the model's `default_temperature`.
    /// Output cap: `profile.model.max_output_tokens` wins over the model's
    /// `max_output_tokens`.
    ///
    /// # Errors
    /// [`ConfigError::NoModel`] if neither override nor profile names a model;
    /// [`ConfigError::UnknownModel`] / [`ConfigError::UnknownProvider`] if the
    /// reference matches nothing; [`ConfigError::MissingApiKey`] if the
    /// provider's `api_key_env` is unset; [`ConfigError::UnsupportedProviderType`]
    /// for a provider whose type has no adapter yet.
    pub fn resolve(
        &self,
        providers: &ProvidersFile,
        profile: &Profile,
        model_override: Option<&str>,
        temperature_override: Option<f32>,
    ) -> Result<ResolvedModel> {
        let model_ref = model_override
            .or(profile.model.default.as_deref())
            .ok_or_else(|| ConfigError::NoModel(profile.profile.name.clone()))?;

        let (provider, model) = find_model(providers, model_ref)?;

        if provider.provider_type != ProviderType::OpenaiChat {
            return Err(ConfigError::UnsupportedProviderType(
                provider.provider_type.as_str().to_owned(),
            ));
        }

        let api_key =
            std::env::var(&provider.api_key_env).map_err(|_| ConfigError::MissingApiKey {
                provider: provider.name.clone(),
                env: provider.api_key_env.clone(),
            })?;

        let temperature = temperature_override
            .or(profile.model.temperature)
            .unwrap_or(model.default_temperature);
        let max_output_tokens = profile
            .model
            .max_output_tokens
            .unwrap_or(model.max_output_tokens);

        Ok(ResolvedModel {
            provider_name: provider.name.clone(),
            provider_type: provider.provider_type,
            base_url: provider.base_url.clone(),
            api_key,
            model_id: model.id.clone(),
            temperature,
            max_output_tokens,
            context_window: model.context_window,
            pricing: model.pricing,
        })
    }

    /// The profile's system prompt, falling back to [`DEFAULT_SYSTEM_PROMPT`]
    /// when none is set (`system_file` is already inlined by `load_profile`).
    #[must_use]
    pub fn system_prompt(profile: &Profile) -> String {
        profile
            .prompt
            .system
            .clone()
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned())
    }
}

/// Resolve a `provider/model` or short `model` reference against the configured
/// providers. The short form matches the first provider serving that model id.
fn find_model<'a>(
    providers: &'a ProvidersFile,
    model_ref: &str,
) -> Result<(&'a ProviderConfig, &'a ModelConfig)> {
    if let Some((provider_name, model_id)) = model_ref.split_once('/') {
        let provider = providers
            .providers
            .iter()
            .find(|p| p.name == provider_name)
            .ok_or_else(|| ConfigError::UnknownProvider(provider_name.to_owned()))?;
        let model = provider
            .model(model_id)
            .ok_or_else(|| ConfigError::UnknownModel(model_ref.to_owned()))?;
        Ok((provider, model))
    } else {
        providers
            .providers
            .iter()
            .find_map(|p| p.model(model_ref).map(|m| (p, m)))
            .ok_or_else(|| ConfigError::UnknownModel(model_ref.to_owned()))
    }
}

/// Inline a profile's `system_file` into `prompt.system`, reading it relative
/// to the profile's directory. A `system_file` overrides an inline `system`
/// only if `system` is unset.
fn resolve_system_file(profile: &mut Profile, dir: &Path) -> Result<()> {
    if profile.prompt.system.is_some() {
        return Ok(());
    }
    let Some(rel) = profile.prompt.system_file.clone() else {
        return Ok(());
    };
    let path = dir.join(rel);
    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    profile.prompt.system = Some(text);
    Ok(())
}

/// Read a file, returning `None` if it does not exist (a missing optional config
/// file is not an error) and an [`ConfigError::Io`] for any other failure.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// The on-disk shape of `.omini/config/pricing.toml`: a `[models."<id>"]` table
/// per model. Used only by [`ConfigStore::load_pricing`] (`doc/monitor.md` §6.2).
#[derive(Debug, Default, serde::Deserialize)]
struct PricingFile {
    #[serde(default)]
    models: HashMap<String, Pricing>,
}

/// The user's home directory from `HOME`, if set.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    /// Write `providers.toml` and a profile into a fresh root, returning a store
    /// scoped to that single root (so tests never touch a real `~/.omini`).
    fn store_with(providers: &str, profiles: &[(&str, &str)]) -> (tempfile::TempDir, ConfigStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".omini");
        std::fs::create_dir_all(root.join(CONFIG_SUBDIR)).unwrap();
        std::fs::create_dir_all(root.join(PROFILES_SUBDIR)).unwrap();
        std::fs::write(root.join(CONFIG_SUBDIR).join(PROVIDERS_FILE), providers).unwrap();
        for (name, body) in profiles {
            std::fs::write(
                root.join(PROFILES_SUBDIR).join(format!("{name}.toml")),
                body,
            )
            .unwrap();
        }
        let store = ConfigStore::from_roots(vec![root]);
        (dir, store)
    }

    // `HOME` is reliably set in the test environment; using it as the
    // `api_key_env` lets `resolve` succeed without the (now-unsafe) set_var.
    const PROVIDERS: &str = r#"
[[providers]]
name = "openai-main"
type = "openai-chat"
base_url = "https://api.openai.com/v1"
api_key_env = "HOME"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
default_temperature = 0.3
"#;

    #[test]
    fn resolves_full_model_ref_with_overrides() {
        let profile_body = r#"
[profile]
name = "coding"
[model]
default = "openai-main/gpt-4o"
"#;
        let (_d, store) = store_with(PROVIDERS, &[("coding", profile_body)]);
        let providers = store.load_providers().unwrap();
        let profile = store.load_profile("coding").unwrap();

        // No CLI override → temperature is the model default (0.3).
        let r = store.resolve(&providers, &profile, None, None).unwrap();
        assert_eq!(r.provider_name, "openai-main");
        assert_eq!(r.model_id, "gpt-4o");
        assert_eq!(r.temperature, 0.3);
        assert_eq!(r.max_output_tokens, 16384);
        assert!(!r.api_key.is_empty()); // came from $HOME

        // CLI temperature override wins.
        let r2 = store
            .resolve(&providers, &profile, None, Some(0.9))
            .unwrap();
        assert_eq!(r2.temperature, 0.9);

        // CLI model override (short ref) wins over profile default.
        let r3 = store
            .resolve(&providers, &profile, Some("gpt-4o"), None)
            .unwrap();
        assert_eq!(r3.model_id, "gpt-4o");
    }

    #[test]
    fn short_ref_and_unknown_refs() {
        let (_d, store) = store_with(PROVIDERS, &[]);
        let providers = store.load_providers().unwrap();
        let profile = Profile::builtin_default();

        assert!(matches!(
            find_model(&providers, "gpt-4o"),
            Ok((p, m)) if p.name == "openai-main" && m.id == "gpt-4o"
        ));
        assert!(matches!(
            store.resolve(&providers, &profile, Some("nope/x"), None),
            Err(ConfigError::UnknownProvider(_))
        ));
        assert!(matches!(
            store.resolve(&providers, &profile, Some("ghost"), None),
            Err(ConfigError::UnknownModel(_))
        ));
        // builtin default has no model and we pass no override.
        assert!(matches!(
            store.resolve(&providers, &profile, None, None),
            Err(ConfigError::NoModel(_))
        ));
    }

    #[test]
    fn missing_api_key_is_reported() {
        let providers_src = PROVIDERS.replace(
            "api_key_env = \"HOME\"",
            "api_key_env = \"OMINI_DEFINITELY_UNSET_VAR_XYZ\"",
        );
        let (_d, store) = store_with(&providers_src, &[]);
        let providers = store.load_providers().unwrap();
        let profile = Profile::builtin_default();
        match store.resolve(&providers, &profile, Some("gpt-4o"), None) {
            Err(ConfigError::MissingApiKey { env, .. }) => {
                assert_eq!(env, "OMINI_DEFINITELY_UNSET_VAR_XYZ");
            }
            other => panic!("expected MissingApiKey, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_provider_type_is_rejected() {
        let providers_src = PROVIDERS.replace("type = \"openai-chat\"", "type = \"anthropic\"");
        let (_d, store) = store_with(&providers_src, &[]);
        let providers = store.load_providers().unwrap();
        let profile = Profile::builtin_default();
        assert!(matches!(
            store.resolve(&providers, &profile, Some("gpt-4o"), None),
            Err(ConfigError::UnsupportedProviderType(_))
        ));
    }

    #[test]
    fn extends_chain_overlays_parent() {
        let base = r#"
[profile]
name = "base"
[prompt]
system = "base prompt"
[model]
default = "openai-main/gpt-4o"
[tools]
builtin = ["read", "write", "shell"]
"#;
        let coding = r#"
[profile]
name = "coding"
extends = "base"
[model]
temperature = 0.7
"#;
        let (_d, store) = store_with(PROVIDERS, &[("base", base), ("coding", coding)]);
        let profile = store.load_profile("coding").unwrap();
        assert_eq!(profile.prompt.system.as_deref(), Some("base prompt"));
        assert_eq!(profile.model.default.as_deref(), Some("openai-main/gpt-4o"));
        assert_eq!(profile.model.temperature, Some(0.7));
    }

    #[test]
    fn missing_default_profile_falls_back_to_builtin() {
        let (_d, store) = store_with(PROVIDERS, &[]);
        let profile = store.load_profile("default").unwrap();
        assert_eq!(profile.profile.name, "default");
        assert_eq!(
            profile.prompt.system.as_deref(),
            Some(DEFAULT_SYSTEM_PROMPT)
        );
    }

    #[test]
    fn missing_named_profile_is_not_found() {
        let (_d, store) = store_with(PROVIDERS, &[]);
        assert!(matches!(
            store.load_profile("ghost"),
            Err(ConfigError::NotFound(_))
        ));
    }

    #[test]
    fn inheritance_cycle_is_detected() {
        let a = "[profile]\nname = \"a\"\nextends = \"b\"\n";
        let b = "[profile]\nname = \"b\"\nextends = \"a\"\n";
        let (_d, store) = store_with(PROVIDERS, &[("a", a), ("b", b)]);
        assert!(matches!(
            store.load_profile("a"),
            Err(ConfigError::InheritanceCycle(_))
        ));
    }

    #[test]
    fn system_file_is_inlined_relative_to_profile_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".omini");
        let profiles = root.join(PROFILES_SUBDIR);
        std::fs::create_dir_all(root.join(CONFIG_SUBDIR)).unwrap();
        std::fs::create_dir_all(profiles.join("prompts")).unwrap();
        std::fs::write(root.join(CONFIG_SUBDIR).join(PROVIDERS_FILE), PROVIDERS).unwrap();
        std::fs::write(profiles.join("prompts/coding.md"), "from file").unwrap();
        std::fs::write(
            profiles.join("withfile.toml"),
            "[profile]\nname = \"withfile\"\n[prompt]\nsystem_file = \"prompts/coding.md\"\n",
        )
        .unwrap();

        let store = ConfigStore::from_roots(vec![root]);
        let profile = store.load_profile("withfile").unwrap();
        assert_eq!(profile.prompt.system.as_deref(), Some("from file"));
    }

    #[test]
    fn project_root_shadows_user_root_for_providers() {
        let project = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        for (base, name, url) in [
            (project.path(), "shared", "https://project"),
            (user.path(), "shared", "https://user"),
        ] {
            let cfg = base.join(".omini").join(CONFIG_SUBDIR);
            std::fs::create_dir_all(&cfg).unwrap();
            std::fs::write(
                cfg.join(PROVIDERS_FILE),
                format!(
                    "[[providers]]\nname = \"{name}\"\ntype = \"openai-chat\"\nbase_url = \"{url}\"\napi_key_env = \"HOME\"\n"
                ),
            )
            .unwrap();
        }
        let store = ConfigStore::from_roots(vec![
            project.path().join(".omini"),
            user.path().join(".omini"),
        ]);
        let providers = store.load_providers().unwrap();
        assert_eq!(providers.providers.len(), 1);
        assert_eq!(providers.providers[0].base_url, "https://project");
    }

    /// `load_pricing` seeds from inline `providers.toml` pricing, then lets an
    /// explicit `pricing.toml` override a model's price — the monitor's cost
    /// source (`doc/monitor.md` §6.2).
    #[test]
    fn load_pricing_merges_inline_then_override() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".omini").join(CONFIG_SUBDIR);
        std::fs::create_dir_all(&cfg).unwrap();
        // providers.toml: gpt-4o has inline pricing; gpt-4o-mini does not.
        std::fs::write(
            cfg.join(PROVIDERS_FILE),
            r#"
[[providers]]
name = "openai-main"
type = "openai-chat"
base_url = "https://api.openai.com/v1"
api_key_env = "HOME"

[[providers.models]]
id = "gpt-4o"
context_window = 128000
max_output_tokens = 16384
pricing = { input_per_million = 2.50, output_per_million = 10.00 }

[[providers.models]]
id = "gpt-4o-mini"
context_window = 128000
max_output_tokens = 16384
"#,
        )
        .unwrap();
        // pricing.toml overrides gpt-4o and adds gpt-4o-mini.
        std::fs::write(
            cfg.join(PRICING_FILE),
            r#"
[models."gpt-4o"]
input_per_million = 3.00
output_per_million = 12.00

[models."gpt-4o-mini"]
input_per_million = 0.15
output_per_million = 0.60
"#,
        )
        .unwrap();

        let store = ConfigStore::from_roots(vec![dir.path().join(".omini")]);
        let providers = store.load_providers().unwrap();
        let pricing = store.load_pricing(&providers).unwrap();

        // gpt-4o: pricing.toml wins over the inline 2.50.
        assert_eq!(pricing["gpt-4o"].input_per_million, 3.00);
        // gpt-4o-mini: only in pricing.toml.
        assert_eq!(pricing["gpt-4o-mini"].output_per_million, 0.60);
    }

    /// A missing `pricing.toml` is not an error: the table is just the inline
    /// pricing from `providers.toml`.
    #[test]
    fn load_pricing_without_pricing_toml_uses_inline_only() {
        let (_d, store) = store_with(PROVIDERS, &[]);
        let providers = store.load_providers().unwrap();
        // PROVIDERS has no inline pricing → empty table, not an error.
        let pricing = store.load_pricing(&providers).unwrap();
        assert!(pricing.is_empty());
    }

    /// `discover_with` orders roots `--config-dir` → launch cwd → home, each as a
    /// `.omini` subdir. This is the precedence the user specified; config is keyed
    /// off the launch location + explicit flag, never a session workspace.
    #[test]
    fn discover_with_orders_explicit_then_cwd_then_home() {
        let explicit = PathBuf::from("/etc/omini-conf");
        let cwd = PathBuf::from("/home/u/project");
        let store = ConfigStore::discover_with(Some(&explicit), &cwd);
        let roots = store.roots();

        // Explicit config dir wins, then launch cwd. (Home is appended last if
        // $HOME is set; we assert only the leading, deterministic prefix.)
        assert_eq!(roots[0], explicit.join(".omini"));
        assert_eq!(roots[1], cwd.join(".omini"));
        // The session workspace is irrelevant: no workspace path appears here.
        assert!(!roots.contains(&PathBuf::from("/some/session/workspace/.omini")));
    }

    /// With no `--config-dir`, launch cwd is the highest-priority root (then home).
    #[test]
    fn discover_with_no_explicit_starts_at_cwd() {
        let cwd = PathBuf::from("/home/u/project");
        let store = ConfigStore::discover_with(None, &cwd);
        assert_eq!(store.roots()[0], cwd.join(".omini"));
    }

    /// `--config-dir` equal to the launch cwd collapses to a single root (no
    /// duplicate), so the same `.omini` isn't scanned twice.
    #[test]
    fn discover_with_dedups_explicit_equal_to_cwd() {
        let cwd = PathBuf::from("/home/u/project");
        let store = ConfigStore::discover_with(Some(&cwd), &cwd);
        let count = store
            .roots()
            .iter()
            .filter(|r| **r == cwd.join(".omini"))
            .count();
        assert_eq!(count, 1, "explicit == cwd must not duplicate the root");
    }
}
