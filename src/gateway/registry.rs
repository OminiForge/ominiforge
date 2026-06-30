//! [`SessionRegistry`]: maps a session id to its live [`SessionActor`], spawning
//! one on demand.
//!
//! A session is live in exactly one actor. Looking one up that is cold spawns a
//! fresh actor: build a per-session agent (isolated provider + MCP subprocesses,
//! the user's per-session-isolation choice), open the session for appending
//! (taking the event-log lock), and rebuild its runtime from the log. If the
//! lock is already held — by the CLI/TUI, or a still-running actor we don't know
//! about — `open` fails and the lookup surfaces it as a conflict (the server
//! maps it to HTTP 409).
//!
//! Creating a *new* session (or a fork) assembles an agent, mints the session,
//! and spawns the actor around it. Eviction is implicit: an idle actor shuts
//! itself down (`actor.rs`), its `ActorHandle` goes dead, and the next lookup
//! prunes the dead entry and respawns — so the registry never grows unbounded
//! with stale handles.
//!
//! Limitation: [`get_or_spawn`] re-assembles a respawned (cold/idle-evicted)
//! session's agent from the **gateway defaults**, not from the session's stored
//! `profile_id`/`workspace`. So a per-session override passed to
//! [`create_with`](SessionRegistry::create_with) is honored only for that
//! session's first warm lifetime; after eviction + reopen the live agent reverts
//! to defaults (while `session.toml` and the RUNTIME panel still show the
//! override). Fixing this means persisting the override set and re-deriving from
//! meta on respawn — deferred.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::agent::SessionRuntime;
use crate::app::{self, Assembled};
use crate::config::{ConfigStore, ModelSummary, ProfileSummary};
use crate::core::SessionId;
use crate::llm::Message;
use crate::session::{SessionMeta, SessionStore};

use super::actor::{ActorHandle, SessionActor};
use super::config::GatewayConfig;

/// Default model/profile selection a new session is assembled with.
///
/// Plus the workspace it operates in and the config store. Held by the registry
/// so every spawned session uses the same base configuration (the gateway is
/// single-user). The config store is discovered once at startup from
/// `--config-dir` / launch cwd / home — **not** from the workspace — so a
/// per-session workspace override never changes which config is read.
#[derive(Debug, Clone)]
pub struct SessionDefaults {
    /// Config store (provider/profile roots), discovered at startup.
    pub config: ConfigStore,
    /// Workspace root for assembled sessions.
    pub workspace: PathBuf,
    /// Profile name (looked up under `.omini/profiles`).
    pub profile: String,
    /// Whether to skip `.env` autoloading (already loaded at server startup).
    pub no_dotenv: bool,
}

/// The config-layer model identity for a session: the provider and model.
///
/// This is what the gateway resolves for the session (`doc/frontend.md`,
/// RUNTIME panel) — the *configured* selection, stable for the session's
/// lifetime — not whatever a given model request happened to use
/// (subagents/forks may differ; that divergence is a runtime-validation
/// concern, not this display source).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct RuntimeInfo {
    /// Provider name (e.g. `openai-main`).
    pub provider: String,
    /// Model id sent to the API (e.g. `gpt-4o`).
    pub model: String,
    /// Environment tags detected from marker files in the session's workspace
    /// (e.g. `["nix", "cargo"]`). Empty when the session has no workspace or no
    /// marker matched — the RUNTIME panel only shows the row when non-empty
    /// ("detected, therefore shown"; `doc/frontend.md`, B2).
    pub env: Vec<String>,
}

/// Detect environment tags from marker files at the workspace root.
///
/// Returns the tags in a stable order (nix, cargo, node, python) so the RUNTIME
/// panel never reorders between requests. Empty when `workspace` is `None`
/// (restricted session) or no marker is present.
///
/// Only the workspace root is probed — one `Path::exists` per marker, no
/// directory walk — because this runs on a display GET and must stay cheap.
fn detect_env(workspace: Option<&Path>) -> Vec<String> {
    // (tag, marker files that imply it). First matching marker wins per tag; a
    // workspace can carry several tags (a nix flake wrapping a cargo project).
    const MARKERS: &[(&str, &[&str])] = &[
        ("nix", &["flake.nix", ".envrc"]),
        ("cargo", &["Cargo.toml"]),
        ("node", &["package.json"]),
        ("python", &["pyproject.toml"]),
    ];
    let Some(root) = workspace else {
        return Vec::new();
    };
    MARKERS
        .iter()
        .filter(|(_, files)| files.iter().any(|f| root.join(f).exists()))
        .map(|(tag, _)| (*tag).to_owned())
        .collect()
}

/// Owns the live actors and the defaults used to spawn new ones.
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    defaults: SessionDefaults,
    idle_timeout: std::time::Duration,
    /// Session id → live actor handle. Guarded by an async mutex because spawning
    /// (which assembles an agent and connects MCP) is async and must not race two
    /// callers into two actors for the same session.
    actors: Mutex<HashMap<SessionId, ActorHandle>>,
}

impl SessionRegistry {
    /// Build a registry over `defaults`, with actors evicted after the config's
    /// idle timeout.
    #[must_use]
    pub fn new(defaults: SessionDefaults, config: &GatewayConfig) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                defaults,
                idle_timeout: config.idle_timeout(),
                actors: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The session store rooted at the configured workspace.
    #[must_use]
    pub fn store(&self) -> SessionStore {
        SessionStore::new(self.inner.defaults.workspace.join(app::SESSIONS_SUBDIR))
    }

    /// List all session ids, newest first.
    ///
    /// # Errors
    /// Filesystem errors reading the store root.
    pub fn list(&self) -> Result<Vec<SessionId>> {
        self.store().list().context("failed to list sessions")
    }

    /// Read a session's metadata.
    ///
    /// # Errors
    /// [`anyhow::Error`] if the session does not exist or its metadata is
    /// unreadable.
    pub fn meta(&self, id: &SessionId) -> Result<SessionMeta> {
        self.store()
            .read_meta(id)
            .with_context(|| format!("failed to read session `{}`", id.0))
    }

    /// Resolve the config-layer provider/model for `profile_id` (the gateway's
    /// default profile when `None`), plus the environment tags detected at
    /// `workspace`. This is the *configured* selection the RUNTIME panel
    /// displays — read straight from config (providers + profile + resolve),
    /// deliberately **not** through [`app::assemble`], which would also spawn
    /// this profile's MCP subprocesses. Resolving is two small TOML reads; a
    /// display GET must not pay the assembly cost.
    ///
    /// `workspace` is the *session's* workspace (`SessionMeta.workspace`), not
    /// the gateway default — env tags must reflect the directory this session
    /// actually runs in. `None` (restricted session) yields no env tags.
    ///
    /// # Errors
    /// [`anyhow::Error`] if config is unreadable or the profile/model cannot be
    /// resolved (no model named, unknown provider, missing api key).
    pub fn runtime_info(
        &self,
        profile_id: Option<&str>,
        workspace: Option<&Path>,
    ) -> Result<RuntimeInfo> {
        let profile_name = profile_id.unwrap_or(&self.inner.defaults.profile);

        let store = &self.inner.defaults.config;
        let providers = store
            .load_providers()
            .context("failed to load providers.toml")?;
        let profile = store
            .load_profile(profile_name)
            .with_context(|| format!("failed to load profile `{profile_name}`"))?;
        let resolved = store
            .resolve(&providers, &profile, None, None)
            .context("failed to resolve model selection")?;

        Ok(RuntimeInfo {
            provider: resolved.provider_name,
            model: resolved.model_id,
            env: detect_env(workspace),
        })
    }

    /// Get the live actor for `id`, spawning one if the session is cold. The
    /// session must already exist on disk.
    ///
    /// # Errors
    /// - session not found
    /// - the session is locked by another writer (CLI/TUI) — surfaced so the
    ///   server can return 409
    /// - agent assembly failure (bad config)
    // The actors-map guard is intentionally held across the assemble/open awaits:
    // it serializes cold-spawn so two concurrent lookups cannot both build an
    // actor (and both try to take the event-log lock) for the same session.
    // Releasing it early to satisfy `significant_drop_tightening` would reopen
    // exactly that race.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn get_or_spawn(&self, id: &SessionId) -> Result<ActorHandle> {
        let mut actors = self.inner.actors.lock().await;

        // Live and still alive? Reuse it.
        if let Some(handle) = actors.get(id) {
            if handle.is_alive() {
                return Ok(handle.clone());
            }
            // Dead (idle-evicted): drop the stale entry and respawn below.
            actors.remove(id);
        }

        // Cold: assemble an isolated agent and open the session (takes the lock).
        let assembled = self.assemble().await?;
        let events = self
            .store()
            .read_events(id)
            .with_context(|| format!("failed to read session `{}`", id.0))?;
        let writer = self
            .store()
            .open(id)
            .with_context(|| format!("session `{}` is unavailable (locked or missing)", id.0))?;
        let system = Self::system_seed(&assembled);
        let runtime = crate::agent::rebuild_runtime(&events, system.clone());

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
        );
        actors.insert(id.clone(), handle.clone());
        Ok(handle)
    }

    /// Create a brand-new session on the gateway defaults, spawn its actor, and
    /// return `(id, handle)`.
    ///
    /// # Errors
    /// Agent assembly or session-creation failure.
    pub async fn create(&self) -> Result<(SessionId, ActorHandle)> {
        self.create_with(None, None, None).await
    }

    /// Create a brand-new session with optional per-session overrides — `profile`,
    /// `model` (a `provider/model_id` or bare `model_id`), and `workspace` — each
    /// falling back to the gateway default when `None`. The overrides apply to
    /// this session only; they are not written back to config (`doc/profile.md`
    /// §5). The session is stamped with the resolved profile + workspace via
    /// `create_new`, so its `session.toml` records exactly what it ran on.
    ///
    /// Note: only the session's first warm lifetime honors a `model` override —
    /// after idle eviction, [`get_or_spawn`](Self::get_or_spawn) respawns on the
    /// gateway defaults (a pre-existing limitation; see the module docs).
    ///
    /// # Errors
    /// - a `workspace` that does not exist (canonicalization fails)
    /// - a `profile`/`model` that does not resolve
    /// - session-creation failure
    pub async fn create_with(
        &self,
        profile: Option<&str>,
        model: Option<&str>,
        workspace: Option<PathBuf>,
    ) -> Result<(SessionId, ActorHandle)> {
        let assembled = self.assemble_with(profile, model, workspace).await?;
        let writer = self
            .store()
            .create_new(
                Some(assembled.profile_name.clone()),
                Some(assembled.workspace.clone()),
                assembled.tool_names.clone(),
            )
            .context("failed to create session")?;
        let id = writer.session_id().clone();
        let system = Self::system_seed(&assembled);
        let runtime = SessionRuntime::new(system.clone());

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
        );
        self.inner
            .actors
            .lock()
            .await
            .insert(id.clone(), handle.clone());
        Ok((id, handle))
    }

    /// Fork `parent` at `at_seq` into a new self-contained session, spawn its
    /// actor, and return `(new_id, handle)`. The fork's context is the parent's
    /// conversation rebuilt up to `at_seq` (`doc/architecture.md` §6.1).
    ///
    /// # Errors
    /// Parent not found/unreadable, or agent assembly / fork-creation failure.
    pub async fn fork(&self, parent: &SessionId, at_seq: u64) -> Result<(SessionId, ActorHandle)> {
        let assembled = self.assemble().await?;
        let system = Self::system_seed(&assembled);

        // Rebuild the parent's context up to (and including) `at_seq` as the
        // fork's snapshot. Truncating by seq keeps only events at or before the
        // branch point.
        let all = self
            .store()
            .read_events(parent)
            .with_context(|| format!("failed to read parent session `{}`", parent.0))?;
        let upto: Vec<_> = all.into_iter().filter(|e| e.seq <= at_seq).collect();
        if upto.is_empty() {
            return Err(anyhow!(
                "parent session `{}` has no event at or before seq {at_seq}",
                parent.0
            ));
        }
        let parent_runtime = crate::agent::rebuild_runtime(&upto, system.clone());
        let snapshot = parent_runtime.context;

        let meta = self.meta(parent)?;
        let writer = self
            .store()
            .create_fork(
                parent.clone(),
                at_seq,
                meta.profile_id,
                meta.workspace,
                assembled.tool_names.clone(),
                &snapshot,
            )
            .context("failed to create fork")?;
        let id = writer.session_id().clone();
        let runtime = SessionRuntime::new(snapshot);

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
        );
        self.inner
            .actors
            .lock()
            .await
            .insert(id.clone(), handle.clone());
        Ok((id, handle))
    }

    /// Reconfigure `parent` into a new session under a different `profile` and/or
    /// `model`, seeded with the parent's *full* conversation (`doc/profile.md`
    /// §5). The session's config is immutable, so a config change is a new
    /// session (`origin.kind = reconfiguration`), not an in-place edit. The
    /// workspace is inherited from the parent (it is a session property, not a
    /// reconfigurable one).
    ///
    /// Mirrors [`fork`](Self::fork) but keeps the whole history (no `at_seq`
    /// truncation) and rebuilds context under the *new* assembled system prompt,
    /// so a profile change swaps the system prompt while the conversation carries
    /// over.
    ///
    /// # Errors
    /// - parent not found/unreadable
    /// - a `profile`/`model` that does not resolve
    /// - agent assembly / session-creation failure
    pub async fn reconfigure(
        &self,
        parent: &SessionId,
        profile: Option<&str>,
        model: Option<&str>,
    ) -> Result<(SessionId, ActorHandle)> {
        let meta = self.meta(parent)?;
        // The reconfigured session runs in the parent's workspace (immutable);
        // only profile/model change.
        let assembled = self
            .assemble_with(profile, model, meta.workspace.clone())
            .await?;
        let system = Self::system_seed(&assembled);

        // Rebuild the parent's full conversation under the new system seed: the
        // new profile's system prompt replaces the old one, the conversation
        // (user/assistant/tool messages) carries over.
        let all = self
            .store()
            .read_events(parent)
            .with_context(|| format!("failed to read parent session `{}`", parent.0))?;
        let parent_runtime = crate::agent::rebuild_runtime(&all, system.clone());
        let snapshot = parent_runtime.context;

        let writer = self
            .store()
            .create_reconfiguration(
                parent.clone(),
                Some(assembled.profile_name.clone()),
                Some(assembled.workspace.clone()),
                assembled.tool_names.clone(),
                &snapshot,
            )
            .context("failed to create reconfiguration session")?;
        let id = writer.session_id().clone();
        let runtime = SessionRuntime::new(snapshot);

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
        );
        self.inner
            .actors
            .lock()
            .await
            .insert(id.clone(), handle.clone());
        Ok((id, handle))
    }

    /// Assemble a fresh, isolated agent for one session (its own provider + MCP
    /// subprocesses), on the gateway defaults. Diagnostics go to stderr (the
    /// server's log).
    async fn assemble(&self) -> Result<Assembled> {
        self.assemble_with(None, None, None).await
    }

    /// Like [`assemble`](Self::assemble) but with per-session overrides: `profile`
    /// and `workspace` fall back to the gateway defaults when `None`, and `model`
    /// (a `provider/model_id` or bare `model_id`) overrides the profile's default
    /// model when set. Used by [`create_with`](Self::create_with) so a Web client
    /// can choose profile/model/workspace for a *new* session without changing
    /// config. Diagnostics go to stderr (the server's log).
    async fn assemble_with(
        &self,
        profile: Option<&str>,
        model: Option<&str>,
        workspace: Option<PathBuf>,
    ) -> Result<Assembled> {
        let d = &self.inner.defaults;
        let workspace = workspace.unwrap_or_else(|| d.workspace.clone());
        let profile = profile.unwrap_or(&d.profile);
        app::assemble(
            &d.config,
            workspace,
            profile,
            model,
            None,
            d.no_dotenv,
            &|msg| eprintln!("gateway: {msg}"),
        )
        .await
    }

    /// List the profiles available for a new session (`doc/profile.md` §3.1),
    /// resolved from the gateway's config roots. Infallible: an unreadable or
    /// malformed profile file is skipped with a warning to the server log.
    #[must_use]
    pub fn list_profiles(&self) -> Vec<ProfileSummary> {
        self.inner
            .defaults
            .config
            .list_profiles(&|msg| eprintln!("gateway: {msg}"))
    }

    /// List the models available for a per-session override, flattened from the
    /// configured providers.
    ///
    /// # Errors
    /// [`anyhow::Error`] if `providers.toml` is unreadable or malformed.
    pub fn list_models(&self) -> Result<Vec<ModelSummary>> {
        self.inner
            .defaults
            .config
            .list_models()
            .context("failed to load providers.toml")
    }

    /// The system-prompt seed for a session built from `assembled`.
    fn system_seed(assembled: &Assembled) -> Vec<Message> {
        vec![Message::System {
            content: assembled.system_prompt.clone(),
        }]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::detect_env;

    /// No workspace (restricted session) yields no env tags, so the RUNTIME
    /// panel omits the ENV row rather than showing an empty one.
    #[test]
    fn detect_env_none_workspace_is_empty() {
        assert!(detect_env(None).is_empty());
    }

    /// An empty workspace (no marker files) yields no tags — detection is
    /// presence-based, never a default guess.
    #[test]
    fn detect_env_no_markers_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_env(Some(dir.path())).is_empty());
    }

    /// A nix flake wrapping a cargo project carries both tags, in the fixed
    /// order (nix before cargo) regardless of which file was created first —
    /// the panel must not reorder between requests.
    #[test]
    fn detect_env_reports_multiple_tags_in_fixed_order() {
        let dir = tempfile::tempdir().unwrap();
        // Create cargo's marker first to prove ordering is by MARKERS, not fs.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.path().join("flake.nix"), "{}").unwrap();
        assert_eq!(detect_env(Some(dir.path())), vec!["nix", "cargo"]);
    }

    /// `.envrc` alone implies nix (direnv-managed), matching the plan's marker
    /// table.
    #[test]
    fn detect_env_envrc_implies_nix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".envrc"), "use flake").unwrap();
        assert_eq!(detect_env(Some(dir.path())), vec!["nix"]);
    }
}
