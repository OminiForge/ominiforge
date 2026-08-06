//! [`LspService`] / [`LspRouter`]: the process-level owner of language-server
//! instances, shared across every session that works in the same root
//! (`doc/lsp.md` §5.2).
//!
//! ## Why a service, not a per-session manager
//!
//! Before this, each session's `Assembled` held its own [`crate::lsp::LspManager`],
//! so N sessions editing the same workspace spawned N copies of the same
//! language server — N duplicate indexes, N× the memory. The sharing unit is
//! the server's **`root_uri`** (the workspace/worktree root), not the session:
//! a language server indexes one root and every session under that root can
//! share the single client. This mirrors [`crate::sandbox::manager::SandboxManager`]
//! — a process-level service the registry owns, handed to each session's
//! assembly so the tools reach shared state.
//!
//! ## Two traits, two callers
//!
//! - [`LspService`] — the **registry-facing** surface: `status`,
//!   `reclaim_inactive`, `close_idle_documents`. The gateway registry and its
//!   background sweeper hold `Arc<dyn LspService>`.
//! - [`LspRouter`] — the **manager-facing** surface: `shared_server`,
//!   `get_or_spawn`, `note_answered`, `note_died`. The session-level
//!   [`crate::lsp::LspManager`] holds `Arc<dyn LspRouter>` and routes each
//!   file op's diagnostics through it.
//!
//! [`ProcessLspService`] implements both; the split is by caller, not by
//! implementation.
//!
//! ## Concurrency (the whole point of the locking)
//!
//! Many sessions' tools may hit the same server at once:
//! - **Same key, one spawn** — `get_or_spawn` holds the per-server
//!   `client` mutex across `connect().await`, so concurrent first-touches
//!   spawn exactly one server.
//! - **Per-uri serialization** — `sync_document` takes the document's
//!   `doc_locks` entry before `didOpen`/`didChange`, so one file's versions
//!   stay monotonic while different files proceed in parallel.
//! - **Crash respawn shares the spawn lock** — a mid-session death drops the
//!   cached client; the next `diagnostics` re-enters `get_or_spawn`, which
//!   serializes the respawn against every other session that also noticed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use super::client::LspClient;
use super::config::LspServerConfig;
use super::{ServerState, ServerStatus};

/// How long to wait after a failed start before trying to spawn a server
/// again. A misconfigured or crashing language server would otherwise stall
/// every single file op by `init_timeout_ms`; this bounds the damage to one
/// attempt per window.
const START_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

/// Grace before a root with no active session has its servers reclaimed.
///
/// The grace (`doc/lsp.md` §5.2) exists so a user who tabs away and back does
/// not pay a full re-index; a server that IS in use is never reclaimed on a
/// timer (that was the rejected idle-timeout design — rust-analyzer's index
/// takes minutes, far past any idle bound).
pub const DEFAULT_RECLAIM_GRACE: Duration = Duration::from_secs(30 * 60);

/// Idle period after which an untouched open document is `didClose`d.
///
/// Frees the server's per-document text/syntax-tree memory (`doc/lsp.md`
/// §5.2). Distinct from reclaim: the server (and its workspace index) stays
/// alive; only the cached open-copy of an idle file is released, re-opened
/// lazily on the next touch. Never kills the server.
pub const DEFAULT_DOC_IDLE_CLOSE: Duration = Duration::from_secs(15 * 60);

/// The sharing key: one server instance per `(root, server-name)`. The root
/// is the workspace/worktree root — the `root_uri` the server indexes — so
/// two worktrees of one workspace never share a server, while two sessions
/// under the same root always do.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ServerKey {
    root: PathBuf,
    name: String,
}

/// Where one server is in its lifecycle (`doc/lsp.md` §5.1). Surfaced to the
/// UI as [`ServerState`]; the `starting` transient is what the input-area
/// "indexing…" indicator reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    /// Spawned and past the `initialize` handshake, but not yet known to be
    /// ready (no successful non-empty diagnostics yet, and no server-specific
    /// ready signal — heuristic (a1), `doc/lsp.md` §5.2).
    Starting,
    /// Answering diagnostics.
    Running,
    /// Last start/sync failed; in the retry cooldown.
    Failed,
}

impl Lifecycle {
    const fn as_state(self) -> ServerState {
        match self {
            Self::Starting => ServerState::Starting,
            Self::Running => ServerState::Running,
            Self::Failed => ServerState::Failed,
        }
    }
}

// Ready-signal extension point (a3): today `starting → running` is driven by
// the (a1) heuristic — the first successful diagnostics answer
// (`note_answered`). Some servers publish a precise "index done" signal
// (rust-analyzer's `experimental/serverStatus`, `$/progress` work-done); a
// per-server hook can be added here to flip `Starting → Running` on that
// signal instead, without waiting for the first query. It is deliberately
// NOT wired for every server — only where the signal exists and pays off
// (`doc/lsp.md` §5.2).

/// One shared language-server instance plus the metadata its lifecycle needs.
/// Lives in [`ProcessLspService`]'s map, `Arc`-shared by every session under
/// its root.
pub struct SharedServer {
    config: LspServerConfig,
    /// `None` until the first touch spawns the server; holds the live client
    /// (or stays `None` after a failed start). The mutex is the single-spawn
    /// and crash-respawn lock (`get_or_spawn` holds it across `connect`).
    client: Mutex<Option<Arc<LspClient>>>,
    /// When the last start attempt failed. Guards a broken server from
    /// re-paying `init_timeout` on *every* file op: within
    /// [`START_RETRY_COOLDOWN`] of a failure the retry is skipped outright.
    last_failed_at: Mutex<Option<Instant>>,
    /// The server's lifecycle, for the UI's `starting`/`running`/`failed`.
    lifecycle: Mutex<Lifecycle>,
    /// Per-document write locks: `didOpen`/`didChange` to one uri serialize
    /// here (keeping that file's version monotonic) while other files go
    /// parallel (`doc/lsp.md` §5.2).
    doc_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The last time any session touched this server (a `diagnostics` call).
    /// Drives the no-active-session grace reclaim and is unrelated to idle
    /// document closing (which is per-uri).
    last_touched: Mutex<Instant>,
}

impl SharedServer {
    fn new(config: LspServerConfig) -> Self {
        Self {
            config,
            client: Mutex::new(None),
            last_failed_at: Mutex::new(None),
            lifecycle: Mutex::new(Lifecycle::Starting),
            doc_locks: Mutex::new(HashMap::new()),
            last_touched: Mutex::new(Instant::now()),
        }
    }

    /// The write lock for `uri`, creating it on first use. Held across
    /// `didOpen`/`didChange` so one document's versions stay monotonic under
    /// concurrent sessions.
    pub(crate) async fn doc_lock(&self, uri: &str) -> Arc<Mutex<()>> {
        self.doc_locks
            .lock()
            .await
            .entry(uri.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// The registry-facing surface of the process-level language-server service.
/// The gateway registry and its background sweeper hold `Arc<dyn LspService>`.
#[async_trait::async_trait]
pub trait LspService: Send + Sync {
    /// A snapshot of `root`'s **activated** servers for the UI
    /// (`doc/lsp.md` §5.1).
    async fn status(&self, root: &Path) -> Vec<ServerStatus>;

    /// Reclaim the servers of every root that is **inactive** and idle past
    /// the grace period. Returns the number reclaimed.
    async fn reclaim_inactive(&self, active_roots: &std::collections::HashSet<PathBuf>) -> usize;

    /// `didClose` every open document idle past the configured period.
    /// Returns the total closed.
    async fn close_idle_documents(&self) -> usize;
}

/// The manager-facing surface: routes a session's diagnostics requests to
/// the right shared server. The session-level [`crate::lsp::LspManager`]
/// holds `Arc<dyn LspRouter>`.
///
/// Inherits [`LspService`] so the manager can also read server status for
/// the UI without needing a second trait object.
#[async_trait::async_trait]
pub trait LspRouter: LspService {
    /// The shared server for `(root, config)`, registering it on first use.
    /// Cheap: only the map lookup happens here; spawning is lazy (`get_or_spawn`).
    async fn shared_server(&self, root: &Path, config: &LspServerConfig) -> Arc<SharedServer>;

    /// The server's live client, spawning it on first use. Holds the
    /// per-server `client` mutex across `connect().await` so concurrent
    /// first-touches spawn exactly one server, and a crash respawn serializes
    /// against every session that noticed the death. Returns `None` (never an
    /// error) when the server can't start — diagnostics are best-effort
    /// (`doc/lsp.md` §4).
    async fn get_or_spawn(
        &self,
        server: &Arc<SharedServer>,
        root: &Path,
        env_overlay: &std::collections::BTreeMap<String, Option<String>>,
    ) -> Option<Arc<LspClient>>;

    /// Note that `server` answered diagnostics successfully — the (a1) ready
    /// signal that moves it from `starting` to `running`.
    async fn note_answered(&self, server: &Arc<SharedServer>);

    /// Drop `server`'s cached client after a mid-session death (a sync
    /// failure): the next `get_or_spawn` respawns through the same lock.
    async fn note_died(&self, server: &Arc<SharedServer>);
}

/// The concrete process-level owner of shared language servers
/// (`doc/lsp.md` §5.2). The registry owns one; each session's
/// [`crate::lsp::LspManager`] is a thin per-session view that routes to it.
pub struct ProcessLspService {
    servers: Mutex<HashMap<ServerKey, Arc<SharedServer>>>,
    /// Grace period before a root with no active session is reclaimed.
    reclaim_grace: Duration,
    /// Idle period after which an untouched open document is `didClose`d.
    doc_idle_close: Duration,
}

impl Default for ProcessLspService {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessLspService {
    /// Build a service with the default reclaim grace and doc-idle-close
    /// periods.
    #[must_use]
    pub fn new() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
            reclaim_grace: DEFAULT_RECLAIM_GRACE,
            doc_idle_close: DEFAULT_DOC_IDLE_CLOSE,
        }
    }

    /// Override the lifecycle periods (config / tests).
    #[must_use]
    pub const fn with_periods(mut self, reclaim_grace: Duration, doc_idle_close: Duration) -> Self {
        self.reclaim_grace = reclaim_grace;
        self.doc_idle_close = doc_idle_close;
        self
    }

    /// Total servers currently held (any root, any state) — a test/monitor
    /// hook for the reclaim path.
    #[cfg(test)]
    pub(crate) async fn server_count(&self) -> usize {
        self.servers.lock().await.len()
    }
}

#[async_trait::async_trait]
impl LspService for ProcessLspService {
    /// A snapshot of `root`'s **activated** servers for the UI
    /// (`doc/lsp.md` §5.1): those that spawned (starting/running) or tried
    /// and failed (failed/cooldown). Servers never touched — including
    /// built-in defaults for languages this project doesn't use — are
    /// omitted, so a rust+ts root never lists clangd/gopls.
    async fn status(&self, root: &Path) -> Vec<ServerStatus> {
        // Snapshot the root's servers (Arc clones) and release the map lock
        // before awaiting each server's inner mutexes, so a status read never
        // holds the shared map across an await.
        let servers: Vec<Arc<SharedServer>> = self
            .servers
            .lock()
            .await
            .iter()
            .filter(|(k, _)| k.root == root)
            .map(|(_, s)| Arc::clone(s))
            .collect();
        let mut out = Vec::new();
        for server in servers {
            // Every entry in the map was registered by a touch, so it IS an
            // "activated" server by construction. A live client reports its
            // lifecycle (starting until the first answer, then running); a
            // dropped/failed one reports `failed` (cooldown); one that's
            // registered but not yet live and never failed is mid-spawn on
            // another session — still `starting`.
            let state = if server.client.lock().await.is_some() {
                server.lifecycle.lock().await.as_state()
            } else if server.last_failed_at.lock().await.is_some() {
                ServerState::Failed
            } else {
                ServerState::Starting
            };
            out.push(ServerStatus {
                name: server.config.name.clone(),
                extensions: server.config.extensions.clone(),
                state,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Reclaim the servers of every root that is **inactive** (no active
    /// session, per the caller's reckoning) and has been untouched for longer
    /// than the grace period. Dropping a server kills its subprocess
    /// (`kill_on_drop`); the next touch respawns it through `get_or_spawn`.
    ///
    /// The grace period is what makes this safe (`doc/lsp.md` §5.2): a root
    /// that merely went quiet for a few minutes keeps its index. The caller
    /// (the registry's sweeper) supplies the active-root set; this method only
    /// applies the grace + drop. Returns the number of servers reclaimed.
    async fn reclaim_inactive(&self, active_roots: &std::collections::HashSet<PathBuf>) -> usize {
        // Collect the keys to reclaim first (reading `last_touched` is async,
        // so it can't happen inside `retain`'s sync closure).
        let mut servers = self.servers.lock().await;
        let mut stale = Vec::new();
        for (key, server) in servers.iter() {
            // Active root, or touched within the grace period → keep.
            if active_roots.contains(&key.root) {
                continue;
            }
            if server.last_touched.lock().await.elapsed() < self.reclaim_grace {
                continue;
            }
            stale.push(key.clone());
        }
        let reclaimed = stale.len();
        for key in stale {
            if let Some(server) = servers.remove(&key) {
                tracing::info!(
                    server = %server.config.name,
                    root = %key.root.display(),
                    "lsp: reclaiming idle-root server (no active session, grace elapsed)"
                );
            }
        }
        reclaimed
    }

    /// `didClose` every open document that has been idle past the configured
    /// period, across every live server (`doc/lsp.md` §5.2). Runs on the
    /// sweeper's cadence; distinct from [`reclaim_inactive`](Self::reclaim_inactive),
    /// which drops whole servers for gone-idle roots. Returns the total closed.
    async fn close_idle_documents(&self) -> usize {
        // Snapshot the servers (Arc clones) up front so the shared map lock is
        // never held across the per-server client-lock awaits.
        let servers: Vec<Arc<SharedServer>> =
            self.servers.lock().await.values().map(Arc::clone).collect();
        let mut clients = Vec::new();
        for server in servers {
            if let Some(client) = server.client.lock().await.as_ref() {
                clients.push(Arc::clone(client));
            }
        }
        let mut closed = 0usize;
        for client in clients {
            closed += client.close_idle_docs(self.doc_idle_close).await;
        }
        closed
    }
}

#[async_trait::async_trait]
impl LspRouter for ProcessLspService {
    async fn shared_server(&self, root: &Path, config: &LspServerConfig) -> Arc<SharedServer> {
        let key = ServerKey {
            root: root.to_path_buf(),
            name: config.name.clone(),
        };
        let mut servers = self.servers.lock().await;
        servers
            .entry(key)
            .or_insert_with(|| Arc::new(SharedServer::new(config.clone())))
            .clone()
    }

    // The guard is deliberately held across `connect().await`: that is what
    // serializes concurrent spawns so exactly one wins.
    #[allow(clippy::significant_drop_tightening)]
    async fn get_or_spawn(
        &self,
        server: &Arc<SharedServer>,
        root: &Path,
        env_overlay: &std::collections::BTreeMap<String, Option<String>>,
    ) -> Option<Arc<LspClient>> {
        *server.last_touched.lock().await = Instant::now();
        let mut guard = server.client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Some(Arc::clone(client));
        }
        // Recently failed? Skip the retry (and its init_timeout stall) until
        // the cooldown elapses.
        {
            let last = server.last_failed_at.lock().await;
            if last.is_some_and(|t| t.elapsed() < START_RETRY_COOLDOWN) {
                return None;
            }
        }
        *server.lifecycle.lock().await = Lifecycle::Starting;
        let init_timeout = Duration::from_millis(server.config.init_timeout_ms);
        match LspClient::connect(&server.config, root, env_overlay, init_timeout).await {
            Ok(client) => {
                let client = Arc::new(client);
                *guard = Some(Arc::clone(&client));
                // Spawned + handshook but not yet known ready → `starting`
                // until the first successful diagnostics (heuristic a1).
                *server.lifecycle.lock().await = Lifecycle::Starting;
                Some(client)
            }
            Err(e) => {
                *server.last_failed_at.lock().await = Some(Instant::now());
                *server.lifecycle.lock().await = Lifecycle::Failed;
                // A misconfigured server must not fail 100% silently (§12).
                tracing::warn!(
                    server = %server.config.name,
                    cooldown_secs = START_RETRY_COOLDOWN.as_secs(),
                    "lsp: server failed to start ({e}); diagnostics disabled"
                );
                None
            }
        }
    }

    async fn note_answered(&self, server: &Arc<SharedServer>) {
        *server.last_touched.lock().await = Instant::now();
        let mut lifecycle = server.lifecycle.lock().await;
        if *lifecycle == Lifecycle::Starting {
            *lifecycle = Lifecycle::Running;
        }
    }

    async fn note_died(&self, server: &Arc<SharedServer>) {
        let mut guard = server.client.lock().await;
        if guard.take().is_some() {
            *server.last_failed_at.lock().await = Some(Instant::now());
            *server.lifecycle.lock().await = Lifecycle::Failed;
            tracing::warn!(
                server = %server.config.name,
                cooldown_secs = START_RETRY_COOLDOWN.as_secs(),
                "lsp: server stopped responding; client dropped, will respawn on next touch"
            );
        }
    }
}
