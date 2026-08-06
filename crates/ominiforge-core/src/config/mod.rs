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
pub use profile::{
    DEFAULT_SYSTEM_PROMPT, NetworkSection, Profile, ProfileMeta, PromptSection, ToolsSection,
    WebFetchSection,
};
pub use providers::{ModelConfig, ProviderConfig, ProviderType, ProvidersFile, Thinking};

use crate::secrets::SecretStore;
use serde::Serialize;
use std::path::{Path, PathBuf};

const CONFIG_SUBDIR: &str = "config";
const PROFILES_SUBDIR: &str = "profiles";
const PROVIDERS_FILE: &str = "providers.toml";
const OMINI_DIR: &str = ".omini";

/// The embedded built-in provider catalog (`catalog.toml`, this directory).
const CATALOG_TOML: &str = include_str!("catalog.toml");
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
    /// Reasoning-effort tiers the model accepts (raw provider strings).
    pub think_efforts: Vec<String>,
    /// The profile's default effort tier, resolved to a valid tier for this
    /// model; `None` when the profile/model names no tier.
    pub think_effort: Option<String>,
}

/// A profile's listable identity: its name and human-readable description.
///
/// Surfaced to a front-end choosing a profile for a new session (`doc/profile.md`
/// §3.1). Deliberately shallow — enumerating profiles must not resolve the
/// `extends` chain (a broken parent must not hide a usable child).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
pub struct ModelSummary {
    /// Provider name (e.g. `openai-main`).
    pub provider: String,
    /// Model id sent to the API (e.g. `gpt-4o`).
    pub model_id: String,
    /// Maximum context window in tokens (shown alongside the model).
    pub context_window: u32,
    /// Whether / how the model reasons (drives the effort picker's visibility).
    pub thinking: crate::config::Thinking,
    /// Selectable reasoning-effort tiers (raw provider strings; empty = none).
    pub think_efforts: Vec<String>,
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

    /// The provider-secret store rooted at the highest-priority config root.
    ///
    /// Keys entered via the settings UI live here (`<root>/secrets.db`), taking
    /// precedence over a provider's `api_key_env` at [`resolve`](Self::resolve)
    /// time. Returns `None` only when no config root is configured (an empty
    /// store), which never happens through [`discover`](Self::discover).
    #[must_use]
    pub fn secret_store(&self) -> Option<SecretStore> {
        self.roots.first().map(|root| SecretStore::at_root(root))
    }

    /// Load and merge every root's `config/providers.toml`, then append the
    /// built-in catalog ([`builtin_providers`]). A provider defined in a
    /// higher-priority root shadows a same-named one in a lower root; a
    /// same-named user entry likewise shadows a built-in one (it sorts earlier
    /// in the merged list, and [`find_model`] takes the first match), while
    /// [`save_providers`](Self::save_providers) refuses to *write* a built-in
    /// name so the settings UI cannot silently fork the catalog.
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
        for provider in &builtin_providers().providers {
            if !merged.iter().any(|p| p.name == provider.name) {
                merged.push(provider.clone());
            }
        }
        Ok(ProvidersFile { providers: merged })
    }

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
    /// or read is skipped with a `tracing` warning (same posture as a broken
    /// MCP server / hook — one bad profile must not blank the whole list).
    #[must_use]
    pub fn list_profiles(&self) -> Vec<ProfileSummary> {
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
                        tracing::warn!("skipping profile {}: {e}", path.display());
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
                        tracing::warn!("skipping profile {}: {e}", path.display());
                    }
                }
            }
        }
        summaries
    }

    /// Flatten every *usable* provider's models into a selectable list for a
    /// per-session model override. A provider is usable when credentials would
    /// resolve — a key in the secret store or its `api_key_env` set (the same
    /// rule [`resolve`](Self::resolve) applies) — so a provider the user has
    /// not configured never offers models that would only fail at resolve time.
    /// Order follows `providers.toml` (provider, then model order within it),
    /// stable across calls so a front-end never reorders.
    ///
    /// # Errors
    /// Propagates [`load_providers`](Self::load_providers) failures (malformed
    /// `providers.toml`).
    pub fn list_models(&self) -> Result<Vec<ModelSummary>> {
        let providers = self.load_providers()?;
        let models = providers
            .providers
            .iter()
            .filter(|p| self.provider_has_credentials(p))
            .flat_map(|p| {
                p.models.iter().map(move |m| ModelSummary {
                    provider: p.name.clone(),
                    model_id: m.id.clone(),
                    context_window: m.context_window,
                    thinking: m.thinking,
                    think_efforts: m.think_efforts.clone(),
                })
            })
            .collect();
        Ok(models)
    }

    /// Whether a provider would resolve credentials. Built-in catalog entries
    /// are configured through the settings UI's connect cards (a key in the
    /// secret store) — their `api_key_env` is documentation, so an env var
    /// happening to share the name must NOT make an unconfigured built-in look
    /// usable. User-defined providers keep the resolve precedence: secret
    /// store first, then their `api_key_env`.
    fn provider_has_credentials(&self, provider: &ProviderConfig) -> bool {
        if let Some(store) = self.secret_store()
            && let Ok(Some(_)) = store.get(&provider.name)
        {
            return true;
        }
        let is_builtin = builtin_providers()
            .providers
            .iter()
            .any(|b| b.name == provider.name);
        !is_builtin && std::env::var_os(&provider.api_key_env).is_some()
    }

    // __APPEND_MARKER2__

    /// Write the merged provider set back to `<primary root>/config/providers.toml`,
    /// replacing the file wholesale. The settings UI sends the full desired
    /// state, so this is a full overwrite (not a merge). Written atomically
    /// (temp file + rename) so a crash mid-write never leaves a truncated file.
    ///
    /// Built-in catalog entries are read-only: posting one back under its
    /// reserved name is rejected rather than silently persisted or dropped.
    ///
    /// # Errors
    /// [`ConfigError::BuiltinProviderConflict`] if any entry uses a built-in
    /// provider name; [`ConfigError::NoRoot`] if the store has no config root;
    /// serialize or io failure otherwise.
    pub fn save_providers(&self, providers: &ProvidersFile) -> Result<()> {
        for provider in &providers.providers {
            if builtin_providers()
                .providers
                .iter()
                .any(|b| b.name == provider.name)
            {
                return Err(ConfigError::BuiltinProviderConflict(provider.name.clone()));
            }
        }
        let root = self.primary_root()?;
        let path = root.join(CONFIG_SUBDIR).join(PROVIDERS_FILE);
        let text = toml::to_string_pretty(providers).map_err(|source| ConfigError::Serialize {
            path: path.clone(),
            source,
        })?;
        write_atomic(&path, &text)
    }

    /// Read a single profile file **without** resolving its `extends` chain — the
    /// raw authored content, so the settings UI edits exactly what is on disk
    /// (editing the resolved/overlaid form would flatten inheritance and inline
    /// the parent's fields). Returns the [`Profile::builtin_default`] when `name`
    /// is `"default"` and no file exists (mirroring [`load_profile`]).
    ///
    /// # Errors
    /// [`ConfigError::NotFound`] for a missing named profile; parse/io errors.
    pub fn load_profile_raw(&self, name: &str) -> Result<Profile> {
        match self.find_profile(name)? {
            Some((profile, _dir)) => Ok(profile),
            None if name == "default" => Ok(Profile::builtin_default()),
            None => Err(ConfigError::NotFound(self.profile_path(name))),
        }
    }

    /// Write a profile to `<primary root>/profiles/<name>.toml`, replacing the
    /// file wholesale. Written atomically (temp + rename).
    ///
    /// # Errors
    /// [`ConfigError::NoRoot`] if the store has no config root; serialize or io
    /// failure otherwise.
    pub fn save_profile(&self, name: &str, profile: &Profile) -> Result<()> {
        let root = self.primary_root()?;
        let path = root.join(PROFILES_SUBDIR).join(format!("{name}.toml"));
        let text = toml::to_string_pretty(profile).map_err(|source| ConfigError::Serialize {
            path: path.clone(),
            source,
        })?;
        write_atomic(&path, &text)
    }

    /// Delete `<primary root>/profiles/<name>.toml`. Returns `true` if a file was
    /// removed, `false` if it did not exist.
    ///
    /// # Errors
    /// [`ConfigError::NoRoot`] if the store has no config root; io failure other
    /// than not-found.
    pub fn delete_profile(&self, name: &str) -> Result<bool> {
        let root = self.primary_root()?;
        let path = root.join(PROFILES_SUBDIR).join(format!("{name}.toml"));
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(ConfigError::Io { path, source }),
        }
    }

    /// The highest-priority config root, where writes land.
    fn primary_root(&self) -> Result<&Path> {
        self.roots
            .first()
            .map(PathBuf::as_path)
            .ok_or(ConfigError::NoRoot)
    }

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

        // API key precedence: the secret store (settings UI) first, then the
        // provider's `api_key_env`. The stored key is read only here, at resolve
        // time, and never exported to a subprocess env overlay — so a shell/MCP
        // command the agent runs cannot read it via `env`.
        let api_key = match self
            .secret_store()
            .and_then(|s| s.get(&provider.name).transpose())
        {
            Some(result) => result?,
            None => {
                std::env::var(&provider.api_key_env).map_err(|_| ConfigError::MissingApiKey {
                    provider: provider.name.clone(),
                    env: provider.api_key_env.clone(),
                })?
            }
        };

        let temperature = temperature_override
            .or(profile.model.temperature)
            .unwrap_or(model.default_temperature);
        let max_output_tokens = profile
            .model
            .max_output_tokens
            .unwrap_or(model.max_output_tokens);

        // A profile effort tier that the resolved model does not declare is
        // dropped (a stale tier from another model must not leak into a
        // request that would reject it).
        let think_effort = profile
            .model
            .think_effort
            .as_ref()
            .filter(|tier| model.think_efforts.contains(tier))
            .cloned();

        Ok(ResolvedModel {
            provider_name: provider.name.clone(),
            provider_type: provider.provider_type,
            base_url: provider.base_url.clone(),
            api_key,
            model_id: model.id.clone(),
            temperature,
            max_output_tokens,
            context_window: model.context_window,
            think_efforts: model.think_efforts.clone(),
            think_effort,
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

/// Write `text` to `path` atomically: write a sibling temp file, then rename it
/// over `path` (an atomic replace on the same filesystem). Creates the parent
/// directory if absent. A crash mid-write leaves either the old file or the new
/// one, never a truncated one.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|source| ConfigError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
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

/// The user's home directory from `HOME`, if set.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The built-in provider catalog, parsed once from the embedded TOML.
///
/// The file is compile-time data we control, so a malformed catalog is a build
/// bug, not a runtime condition — this panics rather than threading a
/// `Result` through every read.
fn builtin_providers() -> &'static ProvidersFile {
    use std::sync::OnceLock;
    static CATALOG: OnceLock<ProvidersFile> = OnceLock::new();
    #[allow(clippy::expect_used)]
    CATALOG
        .get_or_init(|| toml::from_str(CATALOG_TOML).expect("embedded provider catalog must parse"))
}

/// The provider names reserved by the built-in catalog (for the settings UI
/// to render them as connect cards rather than editable forms).
#[must_use]
pub fn builtin_provider_names() -> Vec<String> {
    builtin_providers()
        .providers
        .iter()
        .map(|p| p.name.clone())
        .collect()
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
    fn list_models_skips_providers_without_credentials() {
        // `HOME` is always set (a usable custom provider); the second provider
        // names an env var that never is — its models must not be offered,
        // since picking one could only fail at resolve time. `kimi-code` is a
        // built-in: its `api_key_env` being set must NOT list it (built-ins
        // are configured via the secret store).
        let (_dir, store) = store_with(
            r#"
[[providers]]
name = "usable"
type = "openai-chat"
base_url = "https://a.test/v1"
api_key_env = "HOME"

[[providers.models]]
id = "m1"
context_window = 1000
max_output_tokens = 100

[[providers]]
name = "unconfigured"
type = "openai-chat"
base_url = "https://b.test/v1"
api_key_env = "OMINI_TEST_NEVER_SET_KEY_ENV"

[[providers.models]]
id = "m2"
context_window = 1000
max_output_tokens = 100
"#,
            &[],
        );
        let models = store.list_models().unwrap();
        assert!(
            models
                .iter()
                .any(|m| m.provider == "usable" && m.model_id == "m1"),
            "a credentialed provider's model must be listed, got {models:?}"
        );
        assert!(
            !models.iter().any(|m| m.provider == "unconfigured"),
            "a provider without credentials must not offer models, got {models:?}"
        );
        assert!(
            !models.iter().any(|m| m.provider == "kimi-code"),
            "a built-in without a stored key must not offer models (env is not its config), got {models:?}"
        );
    }

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

    /// A key stored in the secret store resolves even when the provider's
    /// `api_key_env` is unset — the store is the primary source (settings UI),
    /// the env var only a fallback.
    #[test]
    fn secret_store_supplies_api_key_without_env() {
        let providers_src = PROVIDERS.replace(
            "api_key_env = \"HOME\"",
            "api_key_env = \"OMINI_DEFINITELY_UNSET_VAR_XYZ\"",
        );
        let (_d, store) = store_with(&providers_src, &[]);
        store
            .secret_store()
            .unwrap()
            .set("openai-main", "sk-from-store")
            .unwrap();
        let providers = store.load_providers().unwrap();
        let profile = Profile::builtin_default();
        let r = store
            .resolve(&providers, &profile, Some("gpt-4o"), None)
            .unwrap();
        assert_eq!(r.api_key, "sk-from-store");
    }

    /// The secret store wins over `api_key_env` when both are present, so a key
    /// set in the UI overrides a stale environment variable.
    #[test]
    fn secret_store_takes_precedence_over_env() {
        // PROVIDERS uses api_key_env = "HOME", which is set in the test env.
        let (_d, store) = store_with(PROVIDERS, &[]);
        store
            .secret_store()
            .unwrap()
            .set("openai-main", "sk-store-wins")
            .unwrap();
        let providers = store.load_providers().unwrap();
        let profile = Profile::builtin_default();
        let r = store
            .resolve(&providers, &profile, Some("gpt-4o"), None)
            .unwrap();
        assert_eq!(
            r.api_key, "sk-store-wins",
            "stored key must override the env-var fallback"
        );
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
        let names: Vec<&str> = providers
            .providers
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        // The user-defined `shared` shadows across roots; built-ins follow.
        assert_eq!(names[0], "shared");
        assert_eq!(providers.providers[0].base_url, "https://project");
        assert!(names[1..].iter().all(|n| *n != "shared"));
        assert_eq!(names.len(), 1 + builtin_provider_names().len());
    }

    /// The embedded catalog parses and every entry is complete enough to be
    /// usable: a name, an endpoint, an env-var name, and at least one model.
    /// This guards the data file against half-finished edits.
    #[test]
    fn builtin_catalog_is_well_formed() {
        let catalog = builtin_providers();
        assert!(!catalog.providers.is_empty());
        let mut names = std::collections::HashSet::new();
        for p in &catalog.providers {
            assert!(!p.name.is_empty());
            assert!(p.base_url.starts_with("https://"), "{}", p.name);
            assert!(!p.api_key_env.is_empty(), "{}", p.name);
            assert!(!p.models.is_empty(), "{}", p.name);
            assert!(names.insert(p.name.as_str()), "duplicate {}", p.name);
        }
    }

    /// `load_providers` exposes built-ins after user entries, and a same-named
    /// user provider shadows the built-in (it sorts first, so `find_model`
    /// picks it).
    #[test]
    fn builtin_providers_are_appended_and_shadowable() {
        let builtin = &builtin_provider_names()[0];
        let (_d, store) = store_with(PROVIDERS, &[]);
        let providers = store.load_providers().unwrap();
        assert!(providers.providers.iter().any(|p| p.name == *builtin));
        assert_eq!(providers.providers[0].name, "openai-main");

        // A user entry reusing the built-in name shadows it.
        let shadow = PROVIDERS.replace("openai-main", builtin);
        let (_d2, store2) = store_with(&shadow, &[]);
        let providers2 = store2.load_providers().unwrap();
        let matches: Vec<_> = providers2
            .providers
            .iter()
            .filter(|p| p.name == *builtin)
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].base_url, "https://api.openai.com/v1");
    }

    /// `save_providers` refuses to persist a provider under a built-in name —
    /// the settings UI posts full state, and silently writing the catalog back
    /// would fork it.
    #[test]
    fn save_providers_rejects_builtin_names() {
        let (_d, store) = store_with(PROVIDERS, &[]);
        let builtin = builtin_providers().providers[0].clone();
        let err = store
            .save_providers(&ProvidersFile {
                providers: vec![builtin],
            })
            .unwrap_err();
        assert!(matches!(err, ConfigError::BuiltinProviderConflict(_)));
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
