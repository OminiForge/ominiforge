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

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::agent::SessionRuntime;
use crate::app::{self, Assembled};
use crate::config::{ConfigStore, ModelSummary, ProfileSummary};
use crate::core::SessionId;
use crate::llm::Message;
use crate::session::{SessionMeta, SessionStore};

use super::actor::{ActorHandle, Command, SessionActor};
use super::config::GatewayConfig;
use super::status::{ActivityStatus, StatusHub};

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
    /// Whether to skip workspace env activation/autoloading.
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
    /// Environment labels detected from the activated session environment (e.g.
    /// `["dev shell: impure (nix-shell-env)"]` or `["venv: .venv"]`). Empty
    /// when no activation signal is present — the RUNTIME panel only shows the
    /// row when non-empty ("detected, therefore shown"; `doc/frontend.md`, B2).
    pub env: Vec<String>,
}

/// Detect environment labels from activated environment variables.
///
/// This intentionally does not inspect project files: `Cargo.toml` or
/// `pyproject.toml` describe language/project type, not whether the process is
/// inside the corresponding development environment.
fn current_env_overlay() -> std::collections::BTreeMap<String, Option<String>> {
    std::env::vars()
        .map(|(key, value)| (key, Some(value)))
        .collect()
}

fn apply_overlay(
    env: &mut std::collections::BTreeMap<String, Option<String>>,
    overlay: std::collections::BTreeMap<String, Option<String>>,
) {
    for (key, value) in overlay {
        env.insert(key, value);
    }
}

fn detect_env(env: &std::collections::BTreeMap<String, Option<String>>) -> Vec<String> {
    let mut labels = Vec::new();

    if let Some(mode) = env_value(env, "IN_NIX_SHELL") {
        let mut label = format!("dev shell: {mode}");
        if let Some(name) = env_value(env, "NIX_SHELL_NAME").or_else(|| env_value(env, "name")) {
            label.push_str(" (");
            label.push_str(name);
            label.push(')');
        }
        labels.push(label);
    }
    if let Some(path) = env_value(env, "VIRTUAL_ENV") {
        labels.push(format!("venv: {}", basename(path)));
    }
    if let Some(name) =
        env_value(env, "CONDA_DEFAULT_ENV").or_else(|| env_value(env, "CONDA_PREFIX").map(basename))
    {
        labels.push(format!("conda: {name}"));
    }
    if labels.is_empty()
        && let Some(path) = env_value(env, "DIRENV_FILE")
    {
        labels.push(format!("direnv: {}", basename(path)));
    }

    labels
}

fn env_value<'a>(
    env: &'a std::collections::BTreeMap<String, Option<String>>,
    key: &str,
) -> Option<&'a str> {
    env.get(key)
        .and_then(|value| value.as_deref())
        .filter(|value| !value.is_empty())
}

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
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
    /// The persisted `hash → path` workspace map, guarded by a std mutex (all its
    /// operations are synchronous filesystem reads/writes — no await is held).
    workspaces: std::sync::Mutex<super::workspace::WorkspaceRegistry>,
    /// Process-wide session activity status, fanned out to the session list. Every
    /// spawned actor gets a clone so it can publish its running/idle transitions.
    status_hub: StatusHub,
    /// Per-session sandboxes (`doc/sandbox.md` §3.2): owns each session's
    /// execution environment, decoupled from the (ephemeral) actor that drives
    /// it. Backend is chosen once here as a deployment property.
    sandbox_manager: crate::sandbox::manager::SandboxManager,
    /// Fallback sandbox network policy for sessions whose profile does not set
    /// one (`doc/sandbox.md` §6.2). Resolved once from `gateway.toml` at boot so
    /// a malformed default fails loud here, not per session.
    default_network: crate::sandbox::NetworkPolicy,
    /// Per-workspace sandbox config overrides (`doc/workspace-config.md`), keyed
    /// by workspace path hash, read from the gateway's trusted `.omini/workspaces/`
    /// — the top tier of the network resolution chain.
    workspace_config: super::workspace_config::WorkspaceConfigStore,
    /// Resolves `[[mounts]]` anchors (`doc/sandbox.md` §3.7) to host directories
    /// under the gateway's trusted `.omini` tree.
    mount_anchors: MountAnchors,
}

/// Resolves a workspace's named mount anchors (`doc/sandbox.md` §3.7) into
/// concrete [`VolumeMount`]s. An anchor names a *sharing scope* rooted under the
/// gateway's trusted `.omini` tree — never the agent-writable project dir:
///
/// - `session`   → `<omini>/sessions/<session_id>/work/`   (per-session private)
/// - `workspace` → `<omini>/workspaces/<workspace_id>/shared/` (shared across a
///   workspace's sessions)
/// - `gateway`   → `<omini>/shared/`                        (global)
///
/// The user composes what goes in each — the anchor fixes only the scope, not a
/// purpose. Host directories are created on demand (all three roots are app-owned).
#[derive(Debug, Clone)]
struct MountAnchors {
    /// The gateway's `.omini` directory (holds `sessions/`, `workspaces/`, `shared/`).
    omini: PathBuf,
}

impl MountAnchors {
    /// Resolve every `[[mounts]]` entry to a [`VolumeMount`], creating the host
    /// directory for each. `session_id`/`workspace` key the `session`/`workspace`
    /// anchors.
    ///
    /// # Errors
    /// Fails loud (`doc/sandbox.md` §3.7, Karpathy §12) on: an unknown anchor
    /// name, a `path` that escapes its anchor root (`..` / absolute), a non-absolute
    /// `guest` mount point, or a host-directory creation failure — a misdeclared
    /// mount must break the session, not silently bind the wrong directory.
    fn resolve(
        &self,
        specs: &[super::workspace_config::MountSpec],
        session_id: &SessionId,
        workspace: &Path,
    ) -> Result<Vec<crate::sandbox::VolumeMount>> {
        specs
            .iter()
            .map(|spec| self.resolve_one(spec, session_id, workspace))
            .collect()
    }

    fn resolve_one(
        &self,
        spec: &super::workspace_config::MountSpec,
        session_id: &SessionId,
        workspace: &Path,
    ) -> Result<crate::sandbox::VolumeMount> {
        let root = match spec.anchor.as_str() {
            "session" => self.omini.join("sessions").join(&session_id.0).join("work"),
            "workspace" => {
                let ws_id = super::workspace::WorkspaceId::from_path(workspace);
                self.omini.join("workspaces").join(&ws_id.0).join("shared")
            }
            "gateway" => self.omini.join("shared"),
            other => {
                bail!(
                    "unknown mount anchor `{other}`; expected `session`, `workspace`, or `gateway`"
                )
            }
        };

        // Join the optional subpath, rejecting any escape from the anchor root.
        let host_path = match &spec.path {
            None => root,
            Some(rel) => {
                let candidate = Path::new(rel);
                if candidate.is_absolute()
                    || candidate
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    bail!("mount path `{rel}` must be a relative subpath without `..`");
                }
                root.join(candidate)
            }
        };

        if !Path::new(&spec.guest).is_absolute() {
            bail!("mount guest path `{}` must be absolute", spec.guest);
        }

        std::fs::create_dir_all(&host_path)
            .with_context(|| format!("failed to create mount dir {}", host_path.display()))?;

        Ok(crate::sandbox::VolumeMount {
            host_path,
            guest_path: PathBuf::from(&spec.guest),
            read_only: spec.ro,
        })
    }
}

impl SessionRegistry {
    /// Build a registry over `defaults`, with actors evicted after the config's
    /// idle timeout. Non-fatal diagnostics (e.g. an `auto` sandbox fallback) go
    /// to `on_warn`.
    ///
    /// # Errors
    /// Fails if the configured sandbox backend is `boxlite` but boxlite cannot
    /// start on this host (`doc/sandbox.md` §3.2) — an explicit isolation
    /// request must not silently degrade.
    pub fn new(
        defaults: SessionDefaults,
        config: &GatewayConfig,
        on_warn: &(dyn Fn(&str) + Sync),
    ) -> Result<Self> {
        // The workspace map + per-workspace config dir live beside the session
        // store, under `.omini` (the gateway's trusted config root, not the
        // agent-writable project dir — `doc/workspace-config.md`).
        let omini_dir = defaults
            .workspace
            .join(app::SESSIONS_SUBDIR)
            .parent()
            .map_or_else(|| defaults.workspace.clone(), Path::to_path_buf);
        let workspaces = std::sync::Mutex::new(super::workspace::WorkspaceRegistry::load(
            omini_dir.join("workspaces.json"),
        ));
        let workspace_config =
            super::workspace_config::WorkspaceConfigStore::new(omini_dir.join("workspaces"));
        let mount_anchors = MountAnchors { omini: omini_dir };
        let sandbox_manager =
            crate::sandbox::manager::SandboxManager::from_choice(config.sandbox_backend, on_warn)
                .context("failed to initialize sandbox backend")?;
        let default_network = config
            .default_network_policy()
            .map_err(|e| anyhow::anyhow!("invalid gateway.toml default_network: {e}"))?;
        Ok(Self {
            inner: Arc::new(RegistryInner {
                defaults,
                idle_timeout: config.idle_timeout(),
                actors: Mutex::new(HashMap::new()),
                workspaces,
                status_hub: StatusHub::new(),
                sandbox_manager,
                default_network,
                workspace_config,
                mount_anchors,
            }),
        })
    }

    /// The process-wide session activity status hub — the session list's live
    /// read source (the gateway-wide `/status/events` SSE subscribes to it).
    #[must_use]
    pub fn status_hub(&self) -> StatusHub {
        self.inner.status_hub.clone()
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

    /// Read every session's metadata, newest first. An individual session whose
    /// `session.toml` is unreadable is skipped (not fatal) so one corrupt session
    /// never blanks the workspace grouping.
    ///
    /// Side effect: seeds the workspace map from the metadata, so every workspace
    /// that has at least one session becomes addressable by its id (even if it
    /// predates `workspaces.json`). Best-effort — a seed-persist failure is
    /// swallowed (the map stays in memory for this run).
    ///
    /// # Errors
    /// Filesystem errors reading the store root (listing the ids).
    pub fn list_metas(&self) -> Result<Vec<SessionMeta>> {
        let store = self.store();
        let ids = store.list().context("failed to list sessions")?;
        let metas: Vec<SessionMeta> = ids
            .iter()
            .filter_map(|id| store.read_meta(id).ok())
            .collect();
        if let Ok(mut ws) = self.inner.workspaces.lock() {
            // Best-effort persist: a session-only workspace stays resolvable this
            // run even if the write fails, but surface the failure so a full disk
            // (which would silently lose the seeded entries on restart) is
            // diagnosable rather than invisible (fail loud).
            if let Err(e) = ws.seed_from_metas(&metas) {
                eprintln!("gateway: failed to persist workspace map seed: {e:#}");
            }
        }
        Ok(metas)
    }

    /// Read every **archived** session's metadata, newest first — the archived
    /// view's read source. Mirrors [`list_metas`](Self::list_metas) but sources
    /// ids from [`SessionStore::list_archived`], and deliberately does **not**
    /// seed the workspace map: archived sessions are retired and must not
    /// resurrect a workspace entry. A session whose `session.toml` is unreadable
    /// is skipped, not fatal.
    ///
    /// # Errors
    /// Filesystem errors reading the store root (listing the archived ids).
    pub fn list_archived_metas(&self) -> Result<Vec<SessionMeta>> {
        let store = self.store();
        let ids = store
            .list_archived()
            .context("failed to list archived sessions")?;
        Ok(ids
            .iter()
            .filter_map(|id| store.read_meta(id).ok())
            .collect())
    }

    /// Record a workspace by `path` and return its opaque id (the route the
    /// dashboard opens). The path is canonicalized and persisted in the workspace
    /// map so a later `create_in_workspace` can resolve it server-side — the path
    /// itself never travels to the client.
    ///
    /// # Errors
    /// A `path` that does not exist (canonicalization fails), a poisoned lock, or
    /// an io error persisting the map.
    pub fn record_workspace(&self, path: &Path) -> Result<super::workspace::WorkspaceId> {
        let mut ws = self
            .inner
            .workspaces
            .lock()
            .map_err(|_| anyhow!("workspace map lock poisoned"))?;
        ws.record(path)
    }

    /// Resolve a workspace id to its canonical path, seeding from session metadata
    /// only on a miss.
    ///
    /// The name carries the side effect deliberately: a **miss** triggers
    /// [`list_metas`](Self::list_metas) — a full store scan (every session's
    /// `.toml`) plus a possible `workspaces.json` write — to recover a workspace
    /// known only through its sessions (one created before it was recorded). A
    /// **hit** is a pure in-memory lookup with no IO, so the common path (a
    /// workspace already recorded via `POST /workspaces`) pays nothing.
    fn resolve_or_seed_workspace_id(&self, id: &super::workspace::WorkspaceId) -> Option<PathBuf> {
        // Fast path: already known → no IO.
        if let Ok(ws) = self.inner.workspaces.lock()
            && let Some(path) = ws.path_for(id)
        {
            return Some(path);
        }
        // Miss: seed from session metadata (scans the store), then retry once.
        let _ = self.list_metas();
        self.inner.workspaces.lock().ok()?.path_for(id)
    }

    /// List per-workspace configs whose workspace path no longer resolves
    /// (`doc/workspace-config.md` GC). Read-only — surfaces orphans for an
    /// explicit [`delete_workspace_config`](Self::delete_workspace_config); never
    /// deletes on its own. Each orphan carries the path it *was* for, when known.
    #[must_use]
    pub fn list_config_orphans(&self) -> Vec<(super::workspace::WorkspaceId, Option<PathBuf>)> {
        // Seed first so an orphan known only through old sessions still has a path
        // to show; a lock failure degrades to an empty (best-effort) list.
        let _ = self.list_metas();
        self.inner.workspaces.lock().map_or_else(
            |_| Vec::new(),
            |ws| self.inner.workspace_config.list_orphans(&ws),
        )
    }

    /// Delete one per-workspace config by id (`doc/workspace-config.md` GC).
    /// Idempotent: a missing config is `Ok`. This is the only path that removes a
    /// config file — GC is always explicit.
    ///
    /// # Errors
    /// An io error removing the file.
    pub fn delete_workspace_config(&self, id: &super::workspace::WorkspaceId) -> Result<()> {
        self.inner
            .workspace_config
            .delete(id)
            .with_context(|| format!("failed to delete workspace config `{}`", id.0))
    }

    /// Create a new session **in the workspace identified by `id`**, resolving the
    /// path from the workspace map (never from client input). `profile`/`model`
    /// are optional per-session overrides, exactly as [`create_with`]. The session
    /// is stamped with the resolved workspace, so its `meta.workspace` hashes back
    /// to `id`.
    ///
    /// # Errors
    /// - the id is unknown (no recorded path and no session to seed from) → the
    ///   server maps this to 404
    /// - a `profile`/`model` that does not resolve, or session-creation failure
    pub async fn create_in_workspace(
        &self,
        id: &super::workspace::WorkspaceId,
        profile: Option<&str>,
        model: Option<&str>,
    ) -> Result<(SessionId, ActorHandle)> {
        let path = self
            .resolve_or_seed_workspace_id(id)
            .ok_or_else(|| anyhow!("unknown workspace id `{}`", id.0))?;
        self.create_with(profile, model, Some(path)).await
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

    /// Archive `id`: retire it from the active session list while keeping its
    /// files for later inspection (`doc/session-storage.md` §9). This is the
    /// **release trigger** for the sandbox lifecycle (`doc/sandbox.md` §9 Q5) —
    /// the first path that actually ends a session:
    ///
    /// 1. refuse if a turn is running (surfaced as a 409 to the caller);
    /// 2. stop the live actor, freeing the event-log lock;
    /// 3. release the session's sandbox (boxlite reclaims the `CoW` disk if no
    ///    fork child still depends on it; passthrough is a no-op);
    /// 4. write the archive marker.
    ///
    /// A sandbox-release failure aborts the archive (fail loud) rather than
    /// leaking the environment; the actor simply respawns on next access.
    ///
    /// # Errors
    /// [`SessionError::NotFound`] (as a context) for an unknown session, a
    /// "locked" error if a turn is running, or a sandbox/filesystem failure.
    pub async fn archive(&self, id: &SessionId) -> Result<()> {
        // 404 before touching anything: don't stop an actor for a ghost.
        let _ = self.meta(id)?;

        // Don't retire a session out from under a running turn. "locked" in the
        // message maps to a 409 via the server's `conflict_or_not_found`.
        if self.inner.status_hub.status_of(id) == Some(ActivityStatus::Running) {
            return Err(anyhow!(
                "session `{}` is locked: a turn is running; cancel it before archiving",
                id.0
            ));
        }

        self.stop_actor(id).await;
        self.inner
            .sandbox_manager
            .release(id)
            .await
            .with_context(|| format!("failed to release sandbox for `{}`", id.0))?;
        self.store()
            .archive(id)
            .with_context(|| format!("failed to archive session `{}`", id.0))
    }

    /// Permanently delete `id`'s files (`doc/session-storage.md` §9).
    /// **Irreversible.** Requires the session to be **archived first** — that
    /// two-step is the confirmation gate; a non-archived session is refused
    /// (surfaced as a 409). Since archiving already stopped the actor and released
    /// the sandbox, this is a pure `rm -rf` of the session directory.
    ///
    /// # Errors
    /// [`SessionError::NotFound`] for an unknown session,
    /// [`SessionError::NotArchived`] if it was never archived, or a filesystem
    /// failure removing the directory.
    pub fn delete(&self, id: &SessionId) -> Result<()> {
        self.store()
            .delete(id)
            .with_context(|| format!("failed to delete session `{}`", id.0))
    }

    /// Stop `id`'s live actor if one is registered: drop it from the actor map
    /// and send `Shutdown` so its loop exits and releases the event-log lock. A
    /// no-op if no actor is live (already idle-evicted, or never spawned).
    async fn stop_actor(&self, id: &SessionId) {
        let handle = self.inner.actors.lock().await.remove(id);
        if let Some(handle) = handle {
            // Best-effort: a `send` failure just means the actor already exited.
            let _ = handle.send(Command::Shutdown).await;
        }
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
    pub async fn runtime_info(
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

        let mut env = current_env_overlay();
        if !self.inner.defaults.no_dotenv
            && let Some(workspace) = workspace
        {
            apply_overlay(
                &mut env,
                crate::app::activate_direnv(workspace, &|msg| {
                    eprintln!("gateway: {msg}");
                })
                .await,
            );
        }

        Ok(RuntimeInfo {
            provider: resolved.provider_name,
            model: resolved.model_id,
            env: detect_env(&env),
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
        // Archived sessions are retired for good (`doc/session-storage.md` §9):
        // refuse to bring one back to run. Every run/stream path routes through
        // here, so this single gate covers them all; read-only paths (`meta`,
        // `read_events`) bypass it, keeping the session inspectable. Checked
        // before the actor lock — a plain filesystem stat needs no lock, and
        // `archive` stops any live actor itself, so there is nothing to race.
        if self.store().is_archived(id) {
            return Err(anyhow!(
                "session `{}` is archived and cannot be run; it is retired permanently",
                id.0
            ));
        }

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
        let assembled = self.assemble(id).await?;
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

        // Register the (freshly assembled) sandbox so `fork` can reach a resumed
        // session by id. With passthrough this fresh host sandbox is equivalent
        // to the original; re-attaching a *stateful* backend's environment from
        // the persisted descriptor lands when boxlite is wired (`doc/sandbox.md`
        // §3.5, Step 4 boxlite / Step 5).
        self.inner
            .sandbox_manager
            .register(id, Arc::clone(&assembled.sandbox))
            .await;

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
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
        // Mint the id up front so the sandbox (assembled before the session row
        // exists) can resolve a `session`-anchored mount to `sessions/<id>/`
        // (`doc/sandbox.md` §3.7); the same id is then persisted below.
        let id = self.store().mint_id();
        let assembled = self
            .assemble_with(&id, profile, model, workspace, None)
            .await?;
        let writer = self
            .store()
            .create_new_with_id(
                id.clone(),
                Some(assembled.profile_name.clone()),
                Some(assembled.workspace.clone()),
                assembled.tool_names.clone(),
            )
            .context("failed to create session")?;
        debug_assert_eq!(writer.session_id(), &id);
        // Bind the session to its sandbox: persist the descriptor for restart
        // re-attach, and register the live handle so `fork` can reach it by id
        // (`doc/sandbox.md` §3.2).
        self.store()
            .bind_sandbox(&id, assembled.sandbox_descriptor.clone())
            .context("failed to persist sandbox descriptor")?;
        self.inner
            .sandbox_manager
            .register(&id, Arc::clone(&assembled.sandbox))
            .await;
        let system = Self::system_seed(&assembled);
        let runtime = SessionRuntime::new(system.clone());

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
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
        // Fork the parent's sandbox up front so it can be injected into the child
        // agent (`doc/sandbox.md` §4.2). A snapshot-capable backend yields an
        // isolated CoW child; passthrough cannot snapshot, so `fork_from` returns
        // `Unsupported` and the child falls back to a fresh sandbox on the
        // inherited workspace (assemble builds it when `injected` is `None`).
        let injected = match self.inner.sandbox_manager.fork_from(parent).await {
            Ok(pair) => Some(pair),
            Err(crate::sandbox::SandboxError::Unsupported(_)) => None,
            Err(e) => return Err(anyhow!("sandbox fork failed: {e}")),
        };
        // Mint the child's id up front for the same reason as `create_with`: a
        // `session`-anchored mount must resolve to the child's own directory.
        let id = self.store().mint_id();
        let assembled = self.assemble_with(&id, None, None, None, injected).await?;
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
                id.clone(),
                parent.clone(),
                at_seq,
                meta.profile_id,
                meta.workspace,
                assembled.tool_names.clone(),
                &snapshot,
            )
            .context("failed to create fork")?;
        debug_assert_eq!(writer.session_id(), &id);

        // Register and persist the child's sandbox — the injected CoW fork on a
        // snapshot-capable backend, or the freshly assembled fallback sandbox on
        // passthrough. Uniform either way (`doc/sandbox.md` §3.2, §4.2).
        self.inner
            .sandbox_manager
            .register(&id, Arc::clone(&assembled.sandbox))
            .await;
        self.store()
            .bind_sandbox(&id, assembled.sandbox_descriptor.clone())
            .context("failed to persist fork sandbox descriptor")?;
        let runtime = SessionRuntime::new(snapshot);

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
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
        // only profile/model change. Mint the id up front for `session`-anchored
        // mount resolution (`doc/sandbox.md` §3.7).
        let id = self.store().mint_id();
        let assembled = self
            .assemble_with(&id, profile, model, meta.workspace.clone(), None)
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
                id.clone(),
                parent.clone(),
                Some(assembled.profile_name.clone()),
                Some(assembled.workspace.clone()),
                assembled.tool_names.clone(),
                &snapshot,
            )
            .context("failed to create reconfiguration session")?;
        debug_assert_eq!(writer.session_id(), &id);
        let runtime = SessionRuntime::new(snapshot);

        let handle = SessionActor::spawn(
            Arc::new(assembled.agent),
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
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
    async fn assemble(&self, session_id: &SessionId) -> Result<Assembled> {
        self.assemble_with(session_id, None, None, None, None).await
    }

    /// Like [`assemble`](Self::assemble) but with per-session overrides: `profile`
    /// and `workspace` fall back to the gateway defaults when `None`, and `model`
    /// (a `provider/model_id` or bare `model_id`) overrides the profile's default
    /// model when set. Used by [`create_with`](Self::create_with) so a Web client
    /// can choose profile/model/workspace for a *new* session without changing
    /// config. Diagnostics go to stderr (the server's log).
    async fn assemble_with(
        &self,
        session_id: &SessionId,
        profile: Option<&str>,
        model: Option<&str>,
        workspace: Option<PathBuf>,
        injected_sandbox: Option<(
            Arc<dyn crate::sandbox::Sandbox>,
            crate::session::SandboxDescriptor,
        )>,
    ) -> Result<Assembled> {
        let d = &self.inner.defaults;
        let workspace = workspace.unwrap_or_else(|| d.workspace.clone());
        let profile = profile.unwrap_or(&d.profile);
        // Load the per-workspace config once: it carries both the network
        // override (top of the §6.2 chain) and the auxiliary mounts (§3.7). A
        // present-but-broken file fails the session start (fail-loud) rather than
        // silently dropping to a weaker default.
        let workspace_config = self
            .inner
            .workspace_config
            .load(&workspace)
            .with_context(|| {
                format!(
                    "failed to load workspace config for {}",
                    workspace.display()
                )
            })?
            .unwrap_or_default();
        // Network: a `[network].policy` wins outright; a section without a
        // `policy` key is not an override and falls through to profile/gateway.
        let workspace_network = workspace_config
            .network
            .as_ref()
            .and_then(|section| {
                section.policy.as_ref().map(|name| {
                    crate::sandbox::NetworkPolicy::from_policy_name(name, &section.allow)
                })
            })
            .transpose()
            .map_err(|e| anyhow!("invalid workspace network policy: {e}"))?;
        // Auxiliary mounts (§3.7): resolve each named anchor to a host dir under
        // the gateway's trusted tree, keyed by this session's id and workspace.
        let mounts = self
            .inner
            .mount_anchors
            .resolve(&workspace_config.mounts, session_id, &workspace)
            .context("failed to resolve workspace mounts")?;
        app::assemble(
            &d.config,
            workspace,
            profile,
            model,
            None,
            d.no_dotenv,
            self.inner.sandbox_manager.backend(),
            injected_sandbox,
            self.inner.default_network.clone(),
            workspace_network,
            mounts,
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

    /// The raw `providers.toml` contents (for the settings UI to edit).
    ///
    /// # Errors
    /// [`anyhow::Error`] if `providers.toml` is unreadable or malformed.
    pub fn load_providers(&self) -> Result<crate::config::ProvidersFile> {
        self.inner
            .defaults
            .config
            .load_providers()
            .context("failed to load providers.toml")
    }

    /// Overwrite `providers.toml` with `providers` (settings UI full-state save).
    ///
    /// # Errors
    /// [`anyhow::Error`] on serialize/io failure.
    pub fn save_providers(&self, providers: &crate::config::ProvidersFile) -> Result<()> {
        self.inner
            .defaults
            .config
            .save_providers(providers)
            .context("failed to write providers.toml")
    }

    /// The raw (unresolved) profile file `name`, for editing.
    ///
    /// # Errors
    /// [`anyhow::Error`] if the profile is missing or unparsable.
    pub fn load_profile_raw(&self, name: &str) -> Result<crate::config::Profile> {
        self.inner
            .defaults
            .config
            .load_profile_raw(name)
            .with_context(|| format!("failed to load profile `{name}`"))
    }

    /// Overwrite profile `name`'s file with `profile`.
    ///
    /// # Errors
    /// [`anyhow::Error`] on serialize/io failure.
    pub fn save_profile(&self, name: &str, profile: &crate::config::Profile) -> Result<()> {
        self.inner
            .defaults
            .config
            .save_profile(name, profile)
            .with_context(|| format!("failed to write profile `{name}`"))
    }

    /// Delete profile `name`'s file. Returns whether a file was removed.
    ///
    /// # Errors
    /// [`anyhow::Error`] on io failure.
    pub fn delete_profile(&self, name: &str) -> Result<bool> {
        self.inner
            .defaults
            .config
            .delete_profile(name)
            .with_context(|| format!("failed to delete profile `{name}`"))
    }

    /// The provider names that have an API key stored in the secret store.
    ///
    /// # Errors
    /// [`anyhow::Error`] if the secret store cannot be read.
    pub fn secret_names(&self) -> Result<Vec<String>> {
        self.inner.defaults.config.secret_store().map_or_else(
            || Ok(Vec::new()),
            |store| store.list_names().context("failed to read secret store"),
        )
    }

    /// Store an API key for `provider` in the secret store.
    ///
    /// # Errors
    /// [`anyhow::Error`] if the store has no config root or cannot be written.
    pub fn set_secret(&self, provider: &str, api_key: &str) -> Result<()> {
        let store = self
            .inner
            .defaults
            .config
            .secret_store()
            .context("no config root for the secret store")?;
        store
            .set(provider, api_key)
            .context("failed to write the secret store")
    }

    /// Delete `provider`'s stored API key. Returns whether a key existed.
    ///
    /// # Errors
    /// [`anyhow::Error`] if the store has no config root or cannot be written.
    pub fn delete_secret(&self, provider: &str) -> Result<bool> {
        let store = self
            .inner
            .defaults
            .config
            .secret_store()
            .context("no config root for the secret store")?;
        store
            .delete(provider)
            .context("failed to write the secret store")
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

    use super::{apply_overlay, detect_env};
    use std::collections::BTreeMap;

    /// No activation signals yields no env labels, so the RUNTIME panel omits
    /// the ENV row rather than guessing from project files.
    #[test]
    fn detect_env_without_activation_signal_is_empty() {
        assert!(detect_env(&BTreeMap::new()).is_empty());
    }

    /// Activated environment labels include the environment kind and useful
    /// names from the active environment, not language/project marker files.
    #[test]
    fn detect_env_reports_activation_signals_in_fixed_order() {
        let env = BTreeMap::from([
            ("CONDA_PREFIX".to_owned(), Some("/tmp/conda".to_owned())),
            ("IN_NIX_SHELL".to_owned(), Some("impure".to_owned())),
            ("name".to_owned(), Some("nix-shell-env".to_owned())),
            ("VIRTUAL_ENV".to_owned(), Some("/tmp/.venv".to_owned())),
        ]);

        assert_eq!(
            detect_env(&env),
            vec![
                "dev shell: impure (nix-shell-env)",
                "venv: .venv",
                "conda: conda"
            ]
        );
    }

    /// `NIX_PROFILES` alone is not a dev-shell signal on NixOS/Home Manager; it
    /// is too broad to report as an activated workspace environment.
    #[test]
    fn detect_env_ignores_nix_profiles_without_dev_shell() {
        let env = BTreeMap::from([("NIX_PROFILES".to_owned(), Some("/nix/profile".to_owned()))]);
        assert!(detect_env(&env).is_empty());
    }

    /// A plain direnv environment is shown as direnv only when no more specific
    /// activated environment can be inferred.
    #[test]
    fn detect_env_reports_plain_direnv() {
        let env = BTreeMap::from([("DIRENV_FILE".to_owned(), Some("/tmp/.envrc".to_owned()))]);
        assert_eq!(detect_env(&env), vec!["direnv: .envrc"]);
    }

    /// More specific activated environment labels are preferred over the generic
    /// direnv label, because direnv is only the activation mechanism.
    #[test]
    fn detect_env_prefers_specific_signal_over_direnv() {
        let env = BTreeMap::from([
            ("DIRENV_FILE".to_owned(), Some("/tmp/.envrc".to_owned())),
            ("IN_NIX_SHELL".to_owned(), Some("impure".to_owned())),
        ]);
        assert_eq!(detect_env(&env), vec!["dev shell: impure"]);
    }

    /// Conda's explicit environment name is clearer than the prefix basename
    /// when both are available.
    #[test]
    fn detect_env_prefers_conda_default_env_name() {
        let env = BTreeMap::from([
            ("CONDA_DEFAULT_ENV".to_owned(), Some("base".to_owned())),
            (
                "CONDA_PREFIX".to_owned(),
                Some("/tmp/conda-prefix".to_owned()),
            ),
        ]);
        assert_eq!(detect_env(&env), vec!["conda: base"]);
    }

    /// Runtime detection observes the effective environment after applying the
    /// workspace overlay on top of the server's already-active environment.
    #[test]
    fn apply_overlay_preserves_existing_activation_signals() {
        let mut env = BTreeMap::from([("IN_NIX_SHELL".to_owned(), Some("impure".to_owned()))]);
        apply_overlay(
            &mut env,
            BTreeMap::from([("DIRENV_FILE".to_owned(), Some("/tmp/.envrc".to_owned()))]),
        );
        assert_eq!(detect_env(&env), vec!["dev shell: impure"]);
    }

    mod mount_anchors {
        #![allow(clippy::unwrap_used)]

        use super::super::MountAnchors;
        use crate::core::SessionId;
        use crate::gateway::workspace_config::MountSpec;
        use std::path::PathBuf;
        fn anchors(omini: PathBuf) -> MountAnchors {
            MountAnchors { omini }
        }

        fn spec(anchor: &str, path: Option<&str>, guest: &str, ro: bool) -> MountSpec {
            MountSpec {
                anchor: anchor.to_owned(),
                path: path.map(ToOwned::to_owned),
                guest: guest.to_owned(),
                ro,
            }
        }

        /// Each anchor resolves under a distinct gateway-owned root and the host
        /// dir is created. Pins the sharing-scope → path mapping (§3.7).
        #[test]
        fn three_anchors_resolve_to_distinct_roots() {
            let tmp = tempfile::tempdir().unwrap();
            let a = anchors(tmp.path().to_path_buf());
            let sid = SessionId("SESS1".to_owned());
            let ws = tmp.path().join("proj");

            let specs = vec![
                spec("session", Some("cache"), "/s", false),
                spec("workspace", None, "/w", true),
                spec("gateway", Some("dl"), "/g", false),
            ];
            let mounts = a.resolve(&specs, &sid, &ws).unwrap();
            assert_eq!(mounts.len(), 3);

            // session → sessions/<id>/work/cache, RW.
            assert!(mounts[0].host_path.ends_with("sessions/SESS1/work/cache"));
            assert_eq!(mounts[0].guest_path, PathBuf::from("/s"));
            assert!(!mounts[0].read_only);
            // workspace → workspaces/<ws_id>/shared, RO, path absent = root itself.
            assert!(mounts[1].host_path.ends_with("shared"));
            assert!(
                mounts[1]
                    .host_path
                    .to_string_lossy()
                    .contains("workspaces/")
            );
            assert!(mounts[1].read_only);
            // gateway → shared/dl (global).
            assert!(mounts[2].host_path.ends_with("shared/dl"));

            // Host dirs were created (not just computed).
            assert!(mounts.iter().all(|m| m.host_path.is_dir()));
        }

        /// A `..` in the subpath escapes the anchor root → fail loud (§3.7).
        #[test]
        fn parent_dir_escape_is_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let a = anchors(tmp.path().to_path_buf());
            let sid = SessionId("S".to_owned());
            let specs = vec![spec("gateway", Some("../../etc"), "/x", false)];
            assert!(a.resolve(&specs, &sid, tmp.path()).is_err());
        }

        /// A non-absolute guest mount point → fail loud.
        #[test]
        fn relative_guest_is_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let a = anchors(tmp.path().to_path_buf());
            let sid = SessionId("S".to_owned());
            let specs = vec![spec("session", None, "relative/guest", false)];
            assert!(a.resolve(&specs, &sid, tmp.path()).is_err());
        }

        /// An unknown anchor name → fail loud, not a silent skip.
        #[test]
        fn unknown_anchor_is_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let a = anchors(tmp.path().to_path_buf());
            let sid = SessionId("S".to_owned());
            let specs = vec![spec("bogus", None, "/x", false)];
            assert!(a.resolve(&specs, &sid, tmp.path()).is_err());
        }
    }
}
