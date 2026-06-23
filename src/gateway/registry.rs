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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tokio::sync::Mutex;

use crate::agent::SessionRuntime;
use crate::app::{self, Assembled};
use crate::core::SessionId;
use crate::llm::Message;
use crate::session::{SessionMeta, SessionStore};

use super::actor::{ActorHandle, SessionActor};
use super::config::GatewayConfig;

/// Default model/profile selection a new session is assembled with.
///
/// Plus the workspace it operates in. Held by the registry so every spawned
/// session uses the same base configuration (the gateway is single-user).
#[derive(Debug, Clone)]
pub struct SessionDefaults {
    /// Workspace root for assembled sessions.
    pub workspace: PathBuf,
    /// Profile name (looked up under `.omini/profiles`).
    pub profile: String,
    /// Whether to skip `.env` autoloading (already loaded at server startup).
    pub no_dotenv: bool,
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

    /// Create a brand-new session, spawn its actor, and return `(id, handle)`.
    ///
    /// # Errors
    /// Agent assembly or session-creation failure.
    pub async fn create(&self) -> Result<(SessionId, ActorHandle)> {
        let assembled = self.assemble().await?;
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
        self.inner.actors.lock().await.insert(id.clone(), handle.clone());
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
            return Err(anyhow!("parent session `{}` has no event at or before seq {at_seq}", parent.0));
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
        self.inner.actors.lock().await.insert(id.clone(), handle.clone());
        Ok((id, handle))
    }

    /// Assemble a fresh, isolated agent for one session (its own provider + MCP
    /// subprocesses). Diagnostics go to stderr (the server's log).
    async fn assemble(&self) -> Result<Assembled> {
        let d = &self.inner.defaults;
        app::assemble(
            d.workspace.clone(),
            &d.profile,
            None,
            None,
            d.no_dotenv,
            &|msg| eprintln!("gateway: {msg}"),
        )
        .await
    }

    /// The system-prompt seed for a session built from `assembled`.
    fn system_seed(assembled: &Assembled) -> Vec<Message> {
        vec![Message::System {
            content: assembled.system_prompt.clone(),
        }]
    }
}
