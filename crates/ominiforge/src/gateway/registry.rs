//! [`SessionRegistry`]: maps a session id to its live [`SessionActor`], spawning
//! one on demand.
//!
//! A session is live in exactly one actor. Looking one up that is cold spawns a
//! fresh actor: build a per-session agent (isolated provider + MCP subprocesses,
//! the user's per-session-isolation choice), open the session for appending
//! (taking the event-log lock), and rebuild its runtime from the log. If the
//! lock is already held — by a still-running actor we don't know about —
//! `open` fails and the lookup surfaces it as a conflict (the server
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
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::agent::{ApprovalDecision, ApprovalScope, SessionRuntime};
use crate::app::{self, Assembled};
use crate::config::{ConfigStore, ModelSummary, ProfileSummary};
use crate::core::SessionId;
use crate::llm::Message;
use crate::session::{SessionMeta, SessionStore};

use super::actor::{ActorHandle, Command, SessionActor};
use super::approval::ScopedDecision;
use super::config::GatewayConfig;
use super::status::{ActivityStatus, StatusHub};

/// Bridges the actor's per-turn model overrides to the config store: resolves
/// a `provider/model_id` reference (against the session's profile, for its
/// overrides) into a buildable [`crate::config::ResolvedModel`].
struct TurnModelResolver {
    config: ConfigStore,
}

impl super::actor::ModelResolver for TurnModelResolver {
    fn resolve_turn_model(
        &self,
        model_ref: &str,
        profile_name: &str,
    ) -> Result<crate::config::ResolvedModel> {
        let store = &self.config;
        let providers = store
            .load_providers()
            .context("failed to load providers.toml")?;
        let profile = store
            .load_profile(profile_name)
            .with_context(|| format!("failed to load profile `{profile_name}`"))?;
        store
            .resolve(&providers, &profile, Some(model_ref), None)
            .map_err(anyhow::Error::from)
    }
}

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
/// This is what the gateway resolves for the session (`doc/design/monitor.md`,
/// RUNTIME panel) — the *configured* selection, stable for the session's
/// lifetime — not whatever a given model request happened to use
/// (subagents/forks may differ; that divergence is a runtime-validation
/// concern, not this display source).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeInfo {
    /// Provider name (e.g. `openai-main`).
    pub provider: String,
    /// Model id sent to the API (e.g. `gpt-4o`).
    pub model: String,
    /// The model's full context window in tokens (`0` when unknown). The
    /// context gauge's denominator when only the persisted occupancy estimate
    /// is available (page reload, idle session).
    pub context_window: u32,
    /// Effective compaction threshold (profile override or the default
    /// fraction). Drawn as the gauge's tick.
    pub compaction_threshold: f32,
    /// Environment labels detected from the activated session environment (e.g.
    /// `["dev shell: impure (nix-shell-env)"]` or `["venv: .venv"]`). Empty
    /// when no activation signal is present — the RUNTIME panel only shows the
    /// row when non-empty ("detected, therefore shown"; `doc/design/monitor.md`, B2).
    pub env: Vec<String>,
    /// Reasoning-effort tiers the session's model declares (raw provider
    /// strings). Drives the per-turn effort picker; empty = the model offers
    /// no selectable tiers.
    pub think_efforts: Vec<String>,
    /// The session's default effort tier (from the profile, resolved against
    /// the model's tiers). `None` = provider default.
    pub think_effort: Option<String>,
    /// Live status of the language servers activated under this session's
    /// root (`doc/lsp.md` §5.2). Servers are shared per `root_uri`, so every
    /// session in the same workspace/worktree sees the same list. Empty when
    /// no server is configured or none has been touched yet (servers spawn
    /// lazily on the first file op of their language).
    pub lsp: Vec<crate::lsp::ServerStatus>,
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

/// Why loading a per-workspace config failed.
///
/// Lets the HTTP layer pick the right status: an unknown workspace id is the
/// caller's fault (404); a present-but-malformed file is a server-side problem
/// (500) that must not be mis-reported as "workspace does not exist".
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceConfigError {
    /// No recorded path resolves for this workspace id → 404.
    #[error("unknown workspace id `{0}`")]
    UnknownWorkspace(String),
    /// The config file exists but could not be read/parsed → 500.
    #[error("failed to load workspace config: {0}")]
    Load(#[source] anyhow::Error),
}

/// How often the LSP sweeper runs (`doc/lsp.md` §5.2). Short relative to the
/// 30-min grace so a gone-idle root is reclaimed promptly after the grace
/// elapses, without the sweep itself being a measurable cost.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The set of workspace roots that currently have at least one live actor
/// (`doc/lsp.md` §5.2): the "is this root still in use" signal the LSP
/// sweeper reclaims against. Dead handles (idle-evicted but not yet pruned)
/// don't count — a root whose sessions all went idle is eligible for reclaim
/// after the grace period. Free-standing so the sweeper task can call it on
/// the shared `RegistryInner` without a `SessionRegistry` handle.
async fn active_roots(inner: &RegistryInner) -> std::collections::HashSet<PathBuf> {
    let store = SessionStore::new(inner.defaults.workspace.join(app::SESSIONS_SUBDIR));
    // Snapshot the live session ids and release the actors lock before the
    // (synchronous) meta reads, so the map isn't held across them.
    let live: Vec<SessionId> = {
        let actors = inner.actors.lock().await;
        actors
            .iter()
            .filter(|(_, handle)| handle.is_alive())
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut roots = std::collections::HashSet::new();
    for id in live {
        if let Some(ws) = store.read_meta(&id).ok().and_then(|m| m.workspace) {
            roots.insert(ws);
        }
    }
    roots
}

struct RegistryInner {
    defaults: SessionDefaults,
    idle_timeout: std::time::Duration,
    /// Where each session's model provider comes from (`doc/design/runtime-architecture.md`):
    /// `Configured` in production, `Injected` for tests / local synthetic runs.
    provider_source: crate::app::ProviderSource,
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
    /// Per-session sandboxes (`doc/design/runtime-architecture.md` §3.2): owns each session's
    /// execution environment, decoupled from the (ephemeral) actor that drives
    /// it. Backend is chosen once here as a deployment property.
    sandbox_manager: crate::sandbox::manager::SandboxManager,
    /// Process-level owner of shared language servers (`doc/lsp.md` §5.2):
    /// one server per `(root_uri, language)`, shared by every session under
    /// the root. Handed to each assembly so the tools reach shared clients.
    lsp_service: Arc<dyn crate::lsp::LspRouter>,
    /// Fallback sandbox network policy for sessions whose profile does not set
    /// one (`doc/design/runtime-architecture.md` §6.2). Resolved once from `gateway.toml` at boot so
    /// a malformed default fails loud here, not per session.
    default_network: crate::sandbox::NetworkPolicy,
    /// Gateway-wide baseline tool-call gate (`doc/permission.md` §3), the bottom
    /// tier of the three-tier resolution. Seeded from `gateway.toml` at boot; its
    /// `deny` rules are a floor every session inherits.
    ///
    /// Behind a `RwLock` because the settings UI can change it at runtime
    /// (`PUT /gateway/permission`): a new session must pick up the updated policy
    /// without a gateway restart, so the value is read fresh at each `assemble`
    /// rather than captured once. A security control that silently required a
    /// restart to take effect would be a fail-silent trap (Karpathy §12).
    default_permission: RwLock<crate::permission::PermissionPolicy>,
    /// Per-workspace sandbox config overrides (`doc/design/runtime-architecture.md`), keyed
    /// by workspace path hash, read from the gateway's trusted `.omini/workspaces/`
    /// — the top tier of the network resolution chain.
    workspace_config: super::workspace_config::WorkspaceConfigStore,
    /// Resolves `[[mounts]]` anchors (`doc/design/runtime-architecture.md` §3.7) to host directories
    /// under the gateway's trusted `.omini` tree.
    mount_anchors: MountAnchors,
    /// Serializes every profile/gateway config read-modify-write
    /// (`save_profile`, `set_gateway_permission`, scoped-approval persistence):
    /// two concurrent writers must not lose each other's rules (lost update).
    /// Async because the scoped-rule callback locks it on a spawned task; no
    /// file I/O happens while holding it on an executor thread — that runs on
    /// `spawn_blocking`.
    config_write_lock: Mutex<()>,
}

/// Resolves a workspace's named mount anchors (`doc/design/runtime-architecture.md` §3.7) into
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
    /// Fails loud (`doc/design/runtime-architecture.md` §3.7, Karpathy §12) on: an unknown anchor
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
    /// idle timeout. Non-fatal diagnostics (e.g. an `auto` sandbox fallback)
    /// are logged via `tracing`.
    ///
    /// # Errors
    /// Fails if the configured sandbox backend is `boxlite` but boxlite cannot
    /// start on this host (`doc/design/runtime-architecture.md` §3.2) — an explicit isolation
    /// request must not silently degrade.
    pub fn new(defaults: SessionDefaults, config: &GatewayConfig) -> Result<Self> {
        Self::with_provider_source(defaults, config, app::ProviderSource::Configured)
    }

    /// Like [`new`](Self::new) but with an injected model provider, so tests and
    /// local synthetic runs drive sessions without `providers.toml` or a network.
    /// The same provider instance backs every session this registry spawns.
    ///
    /// # Errors
    /// Same as [`new`](Self::new) — sandbox backend initialization and config
    /// failures.
    pub fn new_with_provider(
        defaults: SessionDefaults,
        config: &GatewayConfig,
        provider: Arc<dyn crate::llm::Provider>,
        resolved: crate::config::ResolvedModel,
    ) -> Result<Self> {
        Self::with_provider_source(
            defaults,
            config,
            app::ProviderSource::Injected { provider, resolved },
        )
    }

    /// Shared constructor: everything but the provider source is identical.
    fn with_provider_source(
        defaults: SessionDefaults,
        config: &GatewayConfig,
        provider_source: app::ProviderSource,
    ) -> Result<Self> {
        // The workspace map + per-workspace config dir live beside the session
        // store, under `.omini` (the gateway's trusted config root, not the
        // agent-writable project dir — `doc/design/runtime-architecture.md`).
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
            crate::sandbox::manager::SandboxManager::from_choice(config.sandbox_backend)
                .context("failed to initialize sandbox backend")?;
        let default_network = config
            .default_network_policy()
            .map_err(|e| anyhow::anyhow!("invalid gateway.toml default_network: {e}"))?;
        let default_permission = RwLock::new(config.default_permission.clone());
        Ok(Self {
            inner: Arc::new(RegistryInner {
                defaults,
                idle_timeout: config.idle_timeout(),
                provider_source,
                actors: Mutex::new(HashMap::new()),
                workspaces,
                status_hub: StatusHub::new(),
                sandbox_manager,
                lsp_service: Arc::new(crate::lsp::ProcessLspService::new().with_periods(
                    config.lsp_reclaim_grace(),
                    crate::lsp::DEFAULT_DOC_IDLE_CLOSE,
                )),
                default_network,
                default_permission,
                workspace_config,
                mount_anchors,
                config_write_lock: Mutex::new(()),
            }),
        })
    }

    /// The process-wide session activity status hub — the session list's live
    /// read source (the gateway-wide `/status/events` SSE subscribes to it).
    #[must_use]
    pub fn status_hub(&self) -> StatusHub {
        self.inner.status_hub.clone()
    }

    /// Spawn the background LSP sweeper (`doc/lsp.md` §5.2): every
    /// [`SWEEP_INTERVAL`] it reclaims the shared language servers of roots
    /// that have no active session and have been idle past the grace period.
    /// The sweep is cheap (a map scan) and never touches an in-use server —
    /// the grace period, not server idleness, gates reclaim.
    ///
    /// The task runs for the process lifetime, capturing only the shared
    /// `RegistryInner` (so it outlives any one `SessionRegistry` handle).
    pub fn start_lsp_sweeper(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            // The first tick fires immediately; skip it so a just-booted
            // gateway doesn't sweep servers still within their grace.
            tick.tick().await;
            loop {
                tick.tick().await;
                let roots = active_roots(&inner).await;
                let reclaimed = inner.lsp_service.reclaim_inactive(&roots).await;
                if reclaimed > 0 {
                    tracing::debug!(reclaimed, "lsp: sweeper reclaimed idle-root servers");
                }
                // Independent of root reclaim: close open docs that have sat
                // idle, freeing per-document memory on LIVE servers while
                // keeping their workspace indexes warm (`doc/lsp.md` §5.2).
                let closed = inner.lsp_service.close_idle_documents().await;
                if closed > 0 {
                    tracing::debug!(closed, "lsp: sweeper closed idle open documents");
                }
            }
        });
    }

    /// The workspace root the gateway assembles sessions in. The GPUI file
    /// tree scopes its read-only browsing to this root.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.inner.defaults.workspace
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
                tracing::warn!("failed to persist workspace map seed: {e:#}");
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
        let id = ws.record(path)?;
        let canonical = ws.path_for(&id);
        drop(ws);
        // Prepare the workspace's direnv environment in the background
        // (`doc/design/runtime-architecture.md`): the first session here then finds a warm snapshot
        // and never pays the (possibly minutes-long) evaluation cost.
        if !self.inner.defaults.no_dotenv
            && let Some(canonical) = canonical
            && let Some(root) = self.inner.defaults.config.roots().first()
        {
            crate::env::spawn_refresh(
                canonical,
                crate::env::WorkspaceEnvCache::anchored_at(root),
                crate::env::EnvActivation::default(),
            );
        }
        Ok(id)
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
    /// (`doc/design/runtime-architecture.md` GC). Read-only — surfaces orphans for an
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

    /// Delete one per-workspace config by id (`doc/design/runtime-architecture.md` GC).
    /// Idempotent: a missing config is `Ok`. This is the only path that removes a
    /// config file — GC is always explicit.
    ///
    /// # Errors
    /// An io error removing the file.
    pub fn delete_workspace_config(&self, id: &super::workspace::WorkspaceId) -> Result<()> {
        self.inner
            .workspace_config
            .delete(id)
            .with_context(|| format!("failed to delete workspace config `{}`", id.0))?;
        // The workspace's env snapshot shares the config's lifecycle
        // (`doc/design/runtime-architecture.md` §4). Best-effort: an unresolvable path or a missing
        // file is not a deletion failure.
        if let Some(path) = self.resolve_or_seed_workspace_id(id)
            && let Some(root) = self.inner.defaults.config.roots().first()
        {
            crate::env::WorkspaceEnvCache::anchored_at(root).remove(&path);
        }
        Ok(())
    }

    /// The gateway-wide baseline permission policy (bottom tier of the three-tier
    /// resolution). A clone of the live value, for the settings UI to display.
    ///
    /// # Errors
    /// A poisoned lock — surfaced rather than papered over so the caller fails
    /// loud instead of showing a stale/empty policy.
    pub fn gateway_permission(&self) -> Result<crate::permission::PermissionPolicy> {
        self.inner
            .default_permission
            .read()
            .map_err(|_| anyhow!("gateway permission lock poisoned"))
            .map(|p| p.clone())
    }

    /// The LSP settings view over the config root chain: every registry entry
    /// (tombstoned ones kept, greyed) plus user-defined servers, each labelled
    /// with its source layer and a `PATH` install probe (`gateway::langconfig`).
    /// Backs `GET /config/lsp`.
    ///
    /// # Errors
    /// A present-but-malformed `lsp.toml` in any root (fail loud).
    pub fn lsp_config_view(&self) -> Result<super::langconfig::LspConfigView> {
        super::langconfig::lsp_config_view(&self.inner.defaults.config)
    }

    /// Persist an edited LSP list to the primary root's `lsp.toml` and verify
    /// the reload reflects it. Serialized with every other config write
    /// (`config_write_lock`). Backs `PUT /config/lsp`.
    ///
    /// # Errors
    /// A `command` edit on a not-installed entry, an unknown server name, no
    /// config root, serialize/io failure, or the post-write reload not
    /// reflecting the request.
    pub async fn save_lsp_config(&self, edit: &super::langconfig::LspConfigEdit) -> Result<()> {
        let _guard = self.inner.config_write_lock.lock().await;
        super::langconfig::save_lsp_config(&self.inner.defaults.config, edit)
    }

    /// The format settings view over the config root chain (mode + the
    /// registry-driven formatter list). Backs `GET /config/format`.
    ///
    /// # Errors
    /// A present-but-malformed `format.toml` in any root (fail loud).
    pub fn format_config_view(&self) -> Result<super::langconfig::FormatConfigView> {
        super::langconfig::format_config_view(&self.inner.defaults.config)
    }

    /// Persist an edited format list (+ mode) to the primary root's
    /// `format.toml` and verify the reload reflects it. Serialized with every
    /// other config write (`config_write_lock`). Backs `PUT /config/format`.
    ///
    /// # Errors
    /// Same conditions as [`save_lsp_config`](Self::save_lsp_config).
    pub async fn save_format_config(
        &self,
        edit: &super::langconfig::FormatConfigEdit,
    ) -> Result<()> {
        let _guard = self.inner.config_write_lock.lock().await;
        super::langconfig::save_format_config(&self.inner.defaults.config, edit)
    }

    /// The workspace's env-overlay snapshot (its direnv-exported PATH etc.),
    /// when one has been prepared. Read-only and non-blocking — this never
    /// triggers a direnv evaluation. `None` when the workspace id is unknown
    /// or no snapshot exists yet (the install probe then falls back to the
    /// gateway PATH, matching how a session with no prepared env would run).
    fn workspace_env_snapshot(
        &self,
        id: &super::workspace::WorkspaceId,
    ) -> Option<(PathBuf, std::collections::BTreeMap<String, Option<String>>)> {
        let path = self.resolve_or_seed_workspace_id(id)?;
        let root = self.inner.defaults.config.roots().first()?;
        let cache = crate::env::WorkspaceEnvCache::anchored_at(root);
        cache.snapshot(&path).map(|env| (path, env))
    }

    /// The LSP settings view scoped to workspace `id` (its `.omini` over the
    /// gateway chain, installs probed against its env-overlay PATH). Backs
    /// `GET /workspaces/{id}/config/lsp`.
    ///
    /// # Errors
    /// An unknown workspace id, or a malformed `lsp.toml` in the chain.
    pub fn lsp_config_view_for(
        &self,
        id: &super::workspace::WorkspaceId,
    ) -> Result<super::langconfig::LspConfigView> {
        let (path, env) = self
            .workspace_env_snapshot(id)
            .map(|(p, e)| (p, Some(e)))
            .or_else(|| {
                // No snapshot: still need the path to read the workspace's
                // `.omini` layer. Resolve it directly (404 if unknown).
                self.resolve_or_seed_workspace_id(id).map(|p| (p, None))
            })
            .ok_or_else(|| anyhow!("unknown workspace id `{}`", id.0))?;
        super::langconfig::lsp_config_view_for(&self.inner.defaults.config, &path, env.as_ref())
    }

    /// Persist an edited LSP list to workspace `id`'s `.omini/config/lsp.toml`.
    /// Backs `PUT /workspaces/{id}/config/lsp`.
    ///
    /// # Errors
    /// An unknown workspace id; see also
    /// [`save_lsp_config`](Self::save_lsp_config).
    pub async fn save_lsp_config_for(
        &self,
        id: &super::workspace::WorkspaceId,
        edit: &super::langconfig::LspConfigEdit,
    ) -> Result<()> {
        let (path, env) = self
            .workspace_env_snapshot(id)
            .map(|(p, e)| (p, Some(e)))
            .or_else(|| self.resolve_or_seed_workspace_id(id).map(|p| (p, None)))
            .ok_or_else(|| anyhow!("unknown workspace id `{}`", id.0))?;
        let _guard = self.inner.config_write_lock.lock().await;
        super::langconfig::save_lsp_config_for(
            &self.inner.defaults.config,
            &path,
            env.as_ref(),
            edit,
        )
    }

    /// The format settings view scoped to workspace `id`. Backs
    /// `GET /workspaces/{id}/config/format`.
    ///
    /// # Errors
    /// An unknown workspace id, or a malformed `format.toml` in the chain.
    pub fn format_config_view_for(
        &self,
        id: &super::workspace::WorkspaceId,
    ) -> Result<super::langconfig::FormatConfigView> {
        let (path, env) = self
            .workspace_env_snapshot(id)
            .map(|(p, e)| (p, Some(e)))
            .or_else(|| self.resolve_or_seed_workspace_id(id).map(|p| (p, None)))
            .ok_or_else(|| anyhow!("unknown workspace id `{}`", id.0))?;
        super::langconfig::format_config_view_for(&self.inner.defaults.config, &path, env.as_ref())
    }

    /// Persist an edited format list (+ mode) to workspace `id`'s
    /// `.omini/config/format.toml`. Backs `PUT /workspaces/{id}/config/format`.
    ///
    /// # Errors
    /// An unknown workspace id; see also
    /// [`save_format_config`](Self::save_format_config).
    pub async fn save_format_config_for(
        &self,
        id: &super::workspace::WorkspaceId,
        edit: &super::langconfig::FormatConfigEdit,
    ) -> Result<()> {
        let (path, env) = self
            .workspace_env_snapshot(id)
            .map(|(p, e)| (p, Some(e)))
            .or_else(|| self.resolve_or_seed_workspace_id(id).map(|p| (p, None)))
            .ok_or_else(|| anyhow!("unknown workspace id `{}`", id.0))?;
        let _guard = self.inner.config_write_lock.lock().await;
        super::langconfig::save_format_config_for(
            &self.inner.defaults.config,
            &path,
            env.as_ref(),
            edit,
        )
    }

    /// The profile a rule pinned from session `sid` persists into: the session's
    /// stamped profile, or the gateway default when the session tracks the
    /// default (the same fallback [`runtime_info`](Self::runtime_info) uses).
    /// `None` when the session meta is unreadable — the caller logs and skips
    /// persistence rather than writing the rule into a guessed profile.
    fn effective_profile_name(&self, sid: &SessionId) -> Option<String> {
        let meta = self.store().read_meta(sid).ok()?;
        Some(
            meta.profile_id
                .unwrap_or_else(|| self.inner.defaults.profile.clone()),
        )
    }

    /// The callback injected into every session's approval gate: persist a
    /// `profile`/`gateway`-scoped approval decision as a rule in the matching
    /// config layer (`doc/permission.md` §5). The gate has already pinned the
    /// rule into the session's live policy; this is the durable half. The work
    /// runs on a detached task — serialized with every other profile/gateway
    /// config write by `config_write_lock`, the blocking file I/O off the
    /// executor via `spawn_blocking`. A persistence failure is logged and
    /// swallowed — an approval must not fail because a config write did.
    fn scoped_rule_callback(&self, sid: SessionId) -> Arc<dyn Fn(ScopedDecision) + Send + Sync> {
        let registry = self.clone();
        Arc::new(move |scoped: ScopedDecision| {
            let registry = registry.clone();
            let sid = sid.clone();
            tokio::spawn(async move {
                // Serialize the read-modify-write against every other config
                // write (`save_profile` / `set_gateway_permission`): two
                // concurrent writers must not lose each other's rules.
                let _guard = registry.inner.config_write_lock.lock().await;
                let task = {
                    let registry = registry.clone();
                    let sid = sid.clone();
                    tokio::task::spawn_blocking(move || registry.persist_scoped_rule(&sid, &scoped))
                };
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(
                        session = %sid.0,
                        "failed to persist approval rule: {e:#}"
                    ),
                    Err(e) => {
                        tracing::warn!(session = %sid.0, "persistence task failed: {e}");
                    }
                }
            });
        })
    }

    /// Persist one scoped decision's compiled rule into its config layer —
    /// profile TOML for `profile`, `gateway.toml` for `gateway` (approve →
    /// `allow`, reject → `deny`). A rule already present is a no-op (no
    /// rewrite), so repeated pins of the same call stay idempotent. Runs on
    /// `spawn_blocking` with `config_write_lock` held (see
    /// [`scoped_rule_callback`](Self::scoped_rule_callback)).
    fn persist_scoped_rule(&self, sid: &SessionId, scoped: &ScopedDecision) -> Result<()> {
        match scoped.scope {
            ApprovalScope::Profile => {
                let name = self
                    .effective_profile_name(sid)
                    .context("session meta unreadable; refusing to guess a profile")?;
                let mut profile = self.load_profile_raw(&name)?;
                let list = match scoped.decision {
                    ApprovalDecision::Approve => &mut profile.permission.allow,
                    ApprovalDecision::Reject => &mut profile.permission.deny,
                };
                if !list.contains(&scoped.rule) {
                    list.push(scoped.rule.clone());
                    self.save_profile_locked(&name, &profile)?;
                }
                Ok(())
            }
            ApprovalScope::Gateway => {
                let mut policy = self.gateway_permission()?;
                let list = match scoped.decision {
                    ApprovalDecision::Approve => &mut policy.allow,
                    ApprovalDecision::Reject => &mut policy.deny,
                };
                if !list.contains(&scoped.rule) {
                    list.push(scoped.rule.clone());
                    self.set_gateway_permission_locked(policy)?;
                }
                Ok(())
            }
            // The gate only invokes the callback for the two durable scopes.
            ApprovalScope::Once | ApprovalScope::Session => Ok(()),
        }
    }

    /// Replace the gateway-wide baseline permission policy: update the in-memory
    /// value (so new sessions see it immediately) **and** persist it to
    /// `gateway.toml` (so it survives a restart). Both, atomically from the
    /// caller's view — the file write happens first; only on success is the live
    /// value swapped, so a failed write leaves the running gateway unchanged.
    /// Serialized with every other profile/gateway config write
    /// (`config_write_lock`).
    ///
    /// The persisted file preserves every other gateway field: the current config
    /// is re-loaded from disk, only `default_permission` is replaced, then the
    /// whole record is written back.
    ///
    /// # Errors
    /// No writable config root, a malformed existing `gateway.toml`, a
    /// serialize/io failure, or a poisoned lock.
    pub async fn set_gateway_permission(
        &self,
        policy: crate::permission::PermissionPolicy,
    ) -> Result<()> {
        let _guard = self.inner.config_write_lock.lock().await;
        self.set_gateway_permission_locked(policy)
    }

    /// The lock-free body of [`set_gateway_permission`](Self::set_gateway_permission),
    /// for callers already holding `config_write_lock` (scoped-rule persistence).
    fn set_gateway_permission_locked(
        &self,
        policy: crate::permission::PermissionPolicy,
    ) -> Result<()> {
        let roots = self.inner.defaults.config.roots();
        // Load across ALL roots so we preserve the *effective* config (bind,
        // api_key_env, default_network …) — `GatewayConfig::load` returns the
        // first root that actually has a `gateway.toml`. Loading from only
        // `roots.first()` would miss a file living in a lower root and write a
        // shadow file (defaults + new permission) that silently masks it next boot.
        let mut config =
            GatewayConfig::load(roots).context("failed to load gateway.toml before update")?;
        config.default_permission = policy.clone();
        // Write back to the root that already holds `gateway.toml`; if none does
        // yet (first-ever write), fall back to the highest-priority root.
        let root = roots
            .iter()
            .find(|r| r.join("config").join("gateway.toml").is_file())
            .or_else(|| roots.first())
            .cloned()
            .context("no config root to persist gateway.toml")?;
        config
            .save(&root)
            .context("failed to persist gateway.toml")?;
        // Persist succeeded — now swap the live value new sessions read.
        *self
            .inner
            .default_permission
            .write()
            .map_err(|_| anyhow!("gateway permission lock poisoned"))? = policy;
        Ok(())
    }

    /// The per-workspace config for `id` (network + mounts + permission), or the
    /// default (all-absent) config when none is stored. Backs the workspace-config
    /// editor.
    ///
    /// # Errors
    /// An unknown workspace id (no recorded path to resolve), or a
    /// present-but-malformed config file (fail-loud).
    pub fn load_workspace_config(
        &self,
        id: &super::workspace::WorkspaceId,
    ) -> Result<super::workspace_config::WorkspaceConfig, WorkspaceConfigError> {
        let path = self
            .resolve_or_seed_workspace_id(id)
            .ok_or_else(|| WorkspaceConfigError::UnknownWorkspace(id.0.clone()))?;
        Ok(self
            .inner
            .workspace_config
            .load(&path)
            .map_err(|e| WorkspaceConfigError::Load(anyhow!(e)))?
            .unwrap_or_default())
    }

    /// Write the per-workspace config for `id` (settings UI full-state save).
    ///
    /// # Errors
    /// An unknown workspace id, or a serialize/io failure persisting the file.
    pub fn save_workspace_config(
        &self,
        id: &super::workspace::WorkspaceId,
        config: &super::workspace_config::WorkspaceConfig,
    ) -> Result<()> {
        let path = self
            .resolve_or_seed_workspace_id(id)
            .with_context(|| format!("unknown workspace id `{}`", id.0))?;
        self.inner
            .workspace_config
            .save(&path, config)
            .with_context(|| format!("failed to write workspace config `{}`", id.0))
    }

    /// The permission-config tool catalog for a workspace: the static built-in
    /// catalog plus this workspace's MCP tools, enumerated best-effort
    /// (`doc/permission.md` §3.2).
    ///
    /// MCP enumeration spawns each configured server, runs the handshake, and
    /// reads `tools/list` — so it is fallible and can be slow. Every per-server
    /// failure is swallowed (the server is skipped) and the built-ins are always
    /// returned, so the config UI degrades to "built-ins only" rather than
    /// erroring. The spawned clients are dropped immediately; we need only the
    /// tool list, not a live session. MCP tools carry no field metadata (their
    /// schemas are arbitrary), so they render as generic whole-input cards.
    ///
    /// # Errors
    /// An unknown workspace id (no recorded path). MCP problems are non-fatal.
    pub async fn list_workspace_tools(
        &self,
        id: &super::workspace::WorkspaceId,
    ) -> Result<Vec<crate::tool::ToolInfo>> {
        // Resolve the id to validate it (a 404 for an unknown workspace, not an
        // empty list). The MCP config itself comes from the gateway config roots,
        // matching how `assemble` loads it.
        self.resolve_or_seed_workspace_id(id)
            .with_context(|| format!("unknown workspace id `{}`", id.0))?;
        let mut catalog = crate::tool::builtin_catalog();

        // Best-effort MCP enumeration. An empty env overlay keeps this cheap (no
        // direnv spawn); a server that needs workspace env and fails is simply
        // skipped — the built-ins still return.
        let roots = self.inner.defaults.config.roots();
        if let Ok(mcp_config) = crate::mcp::McpConfig::load(roots) {
            let empty_env = std::collections::BTreeMap::new();
            for server in &mcp_config.servers {
                match crate::mcp::McpClient::connect(server, &empty_env).await {
                    Ok((_client, tools)) => {
                        for def in tools {
                            catalog.push(crate::tool::ToolInfo {
                                name: def.name,
                                label: None,
                                description: Some(if def.description.is_empty() {
                                    format!("MCP · {}", server.name)
                                } else {
                                    def.description
                                }),
                                fields: Vec::new(),
                            });
                        }
                        // Client dropped here: kills the subprocess (we only
                        // wanted the tool list, `kill_on_drop`).
                    }
                    Err(e) => {
                        tracing::warn!(
                            server = %server.name,
                            "skipping MCP server in tool listing: {e}"
                        );
                    }
                }
            }
        }
        Ok(catalog)
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
    /// files for later inspection (`doc/design/runtime-architecture.md` §9). This is the
    /// **release trigger** for the sandbox lifecycle (`doc/design/runtime-architecture.md` §9 Q5) —
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

    /// Permanently delete `id`'s files (`doc/design/runtime-architecture.md` §9).
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
        model: Option<&str>,
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
            .resolve(&providers, &profile, model, None)
            .context("failed to resolve model selection")?;

        let mut env = current_env_overlay();
        if !self.inner.defaults.no_dotenv
            && let Some(workspace) = workspace
        {
            let env_cache = self
                .inner
                .defaults
                .config
                .roots()
                .first()
                .map(|root| crate::env::WorkspaceEnvCache::anchored_at(root));
            apply_overlay(
                &mut env,
                crate::env::session_env(
                    workspace,
                    env_cache.as_ref(),
                    &crate::env::EnvActivation::default(),
                )
                .await,
            );
        }

        Ok(RuntimeInfo {
            provider: resolved.provider_name,
            model: resolved.model_id,
            context_window: resolved.context_window,
            compaction_threshold: profile
                .context
                .compaction_threshold
                .unwrap_or(crate::context::DEFAULT_COMPACTION_THRESHOLD),
            env: detect_env(&env),
            think_efforts: resolved.think_efforts,
            think_effort: resolved.think_effort,
            // Config-layer resolution has no live session; the gateway handler
            // fills this from the actor's LSP manager (`session_runtime`).
            lsp: Vec::new(),
        })
    }

    /// Live LSP server status for `root`, read from the shared `LspService`
    /// (`doc/lsp.md` §5.2). Every session under the same root sees the same
    /// server list, because the servers themselves are shared. Empty when the
    /// root has no activated server — servers spawn lazily, so there is
    /// nothing to report until the first file op wakes one.
    pub async fn lsp_status(&self, root: &Path) -> Vec<crate::lsp::ServerStatus> {
        self.inner.lsp_service.status(root).await
    }

    /// Get the live actor for `id`, spawning one if the session is cold. The
    /// session must already exist on disk.
    ///
    /// # Errors
    /// - session not found
    /// - the session is locked by another writer — surfaced so the
    ///   server can return 409
    /// - agent assembly failure (bad config)
    // The actors-map guard is intentionally held across the assemble/open awaits:
    // it serializes cold-spawn so two concurrent lookups cannot both build an
    // actor (and both try to take the event-log lock) for the same session.
    // Releasing it early to satisfy `significant_drop_tightening` would reopen
    // exactly that race.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn get_or_spawn(&self, id: &SessionId) -> Result<ActorHandle> {
        // Archived sessions are retired for good (`doc/design/runtime-architecture.md` §9):
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
        let mut writer = self
            .store()
            .open(id)
            .with_context(|| format!("session `{}` is unavailable (locked or missing)", id.0))?;
        // A gateway killed mid-turn leaves the log ending on an open turn with
        // possibly dangling tool calls. Reconcile before resuming — without the
        // terminator the view fold renders the turn (and its calls) `running`
        // forever, and a client that trusts `turn_running` queues sends waiting
        // on a settle that can never come (the turn died with the process), so
        // the session is unrecoverable from the UI. Headless here (no bus yet):
        // the writer carries no live subscribers until the actor attaches its
        // own bus, and SSE replays these events from the log on subscribe.
        super::actor::reconcile_open_turn(
            &mut writer,
            id,
            super::actor::ReconcileCause::Interrupted,
        );
        let system = Self::system_seed(&assembled);
        let runtime = crate::agent::rebuild_runtime(writer.events(), system.clone());

        // Register the (freshly assembled) sandbox so `fork` can reach a resumed
        // session by id. With passthrough this fresh host sandbox is equivalent
        // to the original; re-attaching a *stateful* backend's environment from
        // the persisted descriptor lands when boxlite is wired (`doc/design/runtime-architecture.md`
        // §3.5, Step 4 boxlite / Step 5).
        self.inner
            .sandbox_manager
            .register(id, Arc::clone(&assembled.sandbox))
            .await;

        let handle = SessionActor::spawn(
            assembled.agent,
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
            Some(self.scoped_rule_callback(id.clone())),
            assembled.resolved.clone(),
            assembled.profile_name.clone(),
            Some(self.model_resolver()),
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
        // (`doc/design/runtime-architecture.md` §3.7); the same id is then persisted below.
        let id = self.store().mint_id();
        let assemble_started = std::time::Instant::now();
        let assembled = self
            .assemble_with(&id, profile, model, workspace, None)
            .await?;
        tracing::info!(
            session = %id.0,
            elapsed_ms = u64::try_from(assemble_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "session assembled"
        );
        // Stamp the model only when the caller actually chose one (a per-session
        // override); a `None` stays `None` so the session tracks the profile
        // default rather than freezing today's default into `session.toml`. When
        // set, persist the *resolved* `provider/model_id` (authoritative, and
        // unambiguous across providers that share a bare id).
        let model_stamp = model.map(|_| {
            format!(
                "{}/{}",
                assembled.resolved.provider_name, assembled.resolved.model_id
            )
        });
        let writer = self
            .store()
            .create_new_with_id(
                id.clone(),
                Some(assembled.profile_name.clone()),
                model_stamp,
                Some(assembled.workspace.clone()),
                assembled.tool_names.clone(),
            )
            .context("failed to create session")?;
        debug_assert_eq!(writer.session_id(), &id);
        // Bind the session to its sandbox: persist the descriptor for restart
        // re-attach, and register the live handle so `fork` can reach it by id
        // (`doc/design/runtime-architecture.md` §3.2).
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
            assembled.agent,
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
            Some(self.scoped_rule_callback(id.clone())),
            assembled.resolved.clone(),
            assembled.profile_name.clone(),
            Some(self.model_resolver()),
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
    /// conversation rebuilt up to `at_seq` (`doc/design/runtime-architecture.md` §6.1).
    ///
    /// # Errors
    /// Parent not found/unreadable, or agent assembly / fork-creation failure.
    pub async fn fork(&self, parent: &SessionId, at_seq: u64) -> Result<(SessionId, ActorHandle)> {
        // Fork the parent's sandbox up front so it can be injected into the child
        // agent (`doc/design/runtime-architecture.md` §4.2). A snapshot-capable backend yields an
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
        // A fork inherits the parent's per-session model: read the parent meta up
        // front so the child both *runs* on that model (fed to `assemble_with`)
        // and *records* it (stamped via `create_fork` below). `None` (parent had
        // no override) keeps the profile default. Without this the fork would
        // silently drop to the profile default on both axes.
        let meta = self.meta(parent)?;
        let assembled = self
            .assemble_with(&id, None, meta.model.as_deref(), None, injected)
            .await?;
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

        let writer = self
            .store()
            .create_fork(
                id.clone(),
                parent.clone(),
                at_seq,
                meta.profile_id,
                meta.model,
                meta.workspace,
                assembled.tool_names.clone(),
                &snapshot,
            )
            .context("failed to create fork")?;
        debug_assert_eq!(writer.session_id(), &id);

        // Register and persist the child's sandbox — the injected CoW fork on a
        // snapshot-capable backend, or the freshly assembled fallback sandbox on
        // passthrough. Uniform either way (`doc/design/runtime-architecture.md` §3.2, §4.2).
        self.inner
            .sandbox_manager
            .register(&id, Arc::clone(&assembled.sandbox))
            .await;
        self.store()
            .bind_sandbox(&id, assembled.sandbox_descriptor.clone())
            .context("failed to persist fork sandbox descriptor")?;
        let runtime = SessionRuntime::new(snapshot);

        let handle = SessionActor::spawn(
            assembled.agent,
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
            Some(self.scoped_rule_callback(id.clone())),
            assembled.resolved.clone(),
            assembled.profile_name.clone(),
            Some(self.model_resolver()),
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
        // mount resolution (`doc/design/runtime-architecture.md` §3.7).
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

        // Same per-session stamp rule as `create_with`: record the chosen model
        // (resolved, qualified) only when one was passed, else `None`.
        let model_stamp = model.map(|_| {
            format!(
                "{}/{}",
                assembled.resolved.provider_name, assembled.resolved.model_id
            )
        });
        let writer = self
            .store()
            .create_reconfiguration(
                id.clone(),
                parent.clone(),
                Some(assembled.profile_name.clone()),
                model_stamp,
                Some(assembled.workspace.clone()),
                assembled.tool_names.clone(),
                &snapshot,
            )
            .context("failed to create reconfiguration session")?;
        debug_assert_eq!(writer.session_id(), &id);
        let runtime = SessionRuntime::new(snapshot);

        let handle = SessionActor::spawn(
            assembled.agent,
            self.store(),
            system,
            (writer, runtime),
            self.inner.idle_timeout,
            assembled.mcp_clients,
            self.inner.status_hub.clone(),
            Some(self.scoped_rule_callback(id.clone())),
            assembled.resolved.clone(),
            assembled.profile_name.clone(),
            Some(self.model_resolver()),
        );
        self.inner
            .actors
            .lock()
            .await
            .insert(id.clone(), handle.clone());
        Ok((id, handle))
    }

    /// Assemble a fresh, isolated agent for one session (its own provider + MCP
    /// subprocesses), on the gateway defaults. Diagnostics go through `tracing`.
    async fn assemble(&self, session_id: &SessionId) -> Result<Assembled> {
        // Respawn on the session's *stamped* model (`session.toml`), not the
        // gateway default: a session created with a per-session model override
        // must come back on that model after idle eviction, not silently drop to
        // the profile default (previously a known limitation). `None` (no
        // override stamped) still means "follow the profile default".
        let model = self.meta(session_id).ok().and_then(|m| m.model);
        self.assemble_with(session_id, None, model.as_deref(), None, None)
            .await
    }

    /// Like [`assemble`](Self::assemble) but with per-session overrides: `profile`
    /// and `workspace` fall back to the gateway defaults when `None`, and `model`
    /// (a `provider/model_id` or bare `model_id`) overrides the profile's default
    /// model when set. Used by [`create_with`](Self::create_with) so a Web client
    /// can choose profile/model/workspace for a *new* session without changing
    /// config. Diagnostics go through `tracing`.
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
        tracing::debug!(session = %session_id.0, workspace = %workspace.display(), "assembling agent");
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
        // Gateway baseline gate (bottom tier), read fresh so a runtime
        // `PUT /gateway/permission` applies to new sessions without a restart.
        // Cloned into a local BEFORE the await below: an `RwLockReadGuard` is
        // `!Send` and must not be held across `app::assemble().await`. A poisoned
        // lock fails the session start rather than silently dropping the floor.
        let default_permission = self
            .inner
            .default_permission
            .read()
            .map_err(|_| anyhow!("gateway permission lock poisoned"))?
            .clone();
        app::assemble(
            &d.config,
            workspace,
            profile,
            model,
            None,
            self.inner.provider_source.clone(),
            d.no_dotenv,
            self.inner.sandbox_manager.backend(),
            injected_sandbox,
            self.inner.default_network.clone(),
            workspace_network,
            default_permission,
            workspace_config.permission.clone(),
            mounts,
            Arc::clone(&self.inner.lsp_service),
        )
        .await
    }

    /// List the profiles available for a new session (`doc/profile.md` §3.1),
    /// resolved from the gateway's config roots. Infallible: an unreadable or
    /// malformed profile file is skipped with a warning to the server log.
    #[must_use]
    pub fn list_profiles(&self) -> Vec<ProfileSummary> {
        self.inner.defaults.config.list_profiles()
    }

    /// The per-turn model resolver handed to actors: shares the gateway's
    /// config store so a cross-provider model pick resolves against the same
    /// providers/credentials session creation uses.
    fn model_resolver(&self) -> Arc<dyn super::actor::ModelResolver> {
        Arc::new(TurnModelResolver {
            config: self.inner.defaults.config.clone(),
        })
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

    /// Overwrite profile `name`'s file with `profile`, serialized with every
    /// other profile/gateway config write (`config_write_lock`).
    ///
    /// # Errors
    /// [`anyhow::Error`] on serialize/io failure.
    pub async fn save_profile(&self, name: &str, profile: &crate::config::Profile) -> Result<()> {
        let _guard = self.inner.config_write_lock.lock().await;
        self.save_profile_locked(name, profile)
    }

    /// The lock-free body of [`save_profile`](Self::save_profile), for callers
    /// already holding `config_write_lock` (scoped-rule persistence).
    fn save_profile_locked(&self, name: &str, profile: &crate::config::Profile) -> Result<()> {
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

    /// Probe a provider's connectivity + credentials with a minimal request,
    /// using the caller's unsaved profile edits and/or unsaved key when given
    /// (so the settings UI can test before persisting). Returns the model id
    /// that answered on success.
    ///
    /// The base provider definition comes from the merged catalog; `edit`, when
    /// present, overlays connection fields (a draft custom provider the UI is
    /// editing), and `key` overrides the stored/env key. A minimal
    /// `builtin_default` profile carries no model, so the probe model is the
    /// provider's first catalog entry.
    ///
    /// # Errors
    /// [`anyhow::Error`] naming the failing stage: unknown provider, no model,
    /// no credentials, no adapter, transport, or a provider-side rejection.
    #[allow(clippy::items_after_statements)]
    pub async fn test_provider(
        &self,
        name: &str,
        edit: Option<crate::config::ProviderConfig>,
        key: Option<String>,
    ) -> Result<String> {
        let store = &self.inner.defaults.config;
        let catalog = store.load_providers()?;
        let mut provider = catalog
            .providers
            .into_iter()
            .find(|p| p.name == name)
            .with_context(|| format!("unknown provider `{name}`"))?;
        if let Some(e) = edit {
            provider.provider_type = e.provider_type;
            provider.base_url = e.base_url;
            provider.api_key_env = e.api_key_env;
            if !e.models.is_empty() {
                provider.models = e.models;
            }
        }
        let model_id = provider
            .models
            .first()
            .map(|m| m.id.clone())
            .with_context(|| format!("provider `{name}` has no models"))?;

        // Key precedence mirrors resolve(): unsaved test key → secret store →
        // the provider's api_key_env. Read only here, never exported.
        let api_key = match key {
            Some(k) => k,
            None => match store.secret_store().and_then(|s| s.get(name).transpose()) {
                Some(result) => result?,
                None => std::env::var(&provider.api_key_env).with_context(|| {
                    format!(
                        "no API key for `{name}`: none stored and {} is not set",
                        provider.api_key_env
                    )
                })?,
            },
        };

        let resolved = crate::config::ResolvedModel {
            provider_name: provider.name.clone(),
            provider_type: provider.provider_type,
            base_url: provider.base_url.clone(),
            api_key,
            model_id: model_id.clone(),
            temperature: provider.models[0].default_temperature,
            max_output_tokens: provider.models[0].max_output_tokens,
            context_window: provider.models[0].context_window,
            think_efforts: provider.models[0].think_efforts.clone(),
            think_effort: None,
        };
        let handle = crate::provider::build(&resolved)
            .with_context(|| format!("provider `{name}` has no adapter"))?;

        use futures_util::StreamExt;
        let request = crate::llm::ModelRequest {
            model: model_id.clone(),
            messages: vec![Message::User {
                content: "ping".to_owned(),
            }],
            tools: Vec::new(),
            temperature: resolved.temperature,
            max_tokens: Some(1),
            think_effort: None,
        };
        let mut stream = handle
            .stream(request)
            .await
            .map_err(|e| anyhow!(friendly_probe_error(&e)))?;
        // Drain until the first terminal event; per-chunk errors surface here.
        while let Some(item) = stream.next().await {
            match item {
                Ok(crate::llm::StreamEvent::Completed { .. }) => break,
                Ok(_) => {}
                Err(e) => return Err(anyhow!(friendly_probe_error(&e))),
            }
        }
        Ok(model_id)
    }

    /// The system-prompt seed for a session built from `assembled`.
    fn system_seed(assembled: &Assembled) -> Vec<Message> {
        vec![Message::System {
            content: assembled.system_prompt.clone(),
        }]
    }
}

/// Turn an [`crate::llm::LlmError`] into a short, human-readable probe failure
/// for the settings UI's Test action. The raw `Display` of an `LlmError`
/// embeds the provider's JSON error body (`provider auth error:
/// {"error":{...}}`), which is noise next to a key input; map each variant to a
/// plain phrase. The original body is dropped on purpose — the gateway log
/// still carries it if an operator needs it.
fn friendly_probe_error(e: &crate::llm::LlmError) -> String {
    use crate::llm::LlmError;
    match e {
        LlmError::Auth(_) => "API key invalid or expired".to_owned(),
        LlmError::Status { status, .. } => {
            format!("provider rejected the request (HTTP {status})")
        }
        LlmError::Transport(_) => "cannot reach the provider (network/endpoint)".to_owned(),
        LlmError::Decode(_) => "provider returned an unreadable response".to_owned(),
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
