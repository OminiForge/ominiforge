//! [`LocalProtocol`]: the `ClientProtocol` implementation that links
//! `ominiforge-core` directly as a library (`doc/network.md` §3).
//!
//! Zero network, zero serialization, compile-time type safety: every call is a
//! direct invocation on the same [`SessionRegistry`] the HTTP/SSE gateway uses,
//! so the local GPUI client and the remote gateway are two front-ends over one
//! core. The session actor / event-bus / status-hub machinery is shared, not
//! duplicated — local mode just skips the axum layer.
//!
//! Event subscription mirrors the SSE endpoint's ordering guarantee exactly:
//! subscribe the live broadcast *first*, then replay committed events from the
//! durable log, emit the `ReplayEnd` boundary, then yield the live tail. A
//! subscriber that races the replay sees each committed event exactly once (the
//! UI dedups by `seq`, as the web client does).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use ominiforge::agent::{ApprovalDecision, ApprovalScope};
use ominiforge::config::{ModelSummary, ProfileSummary, ProvidersFile};
use ominiforge::context::{bytes_to_tokens, message_bytes};
use ominiforge::core::SessionId;
use ominiforge::gateway::view::{SessionView, fold_view};
use ominiforge::gateway::{
    Command, GatewayConfig, GatewayEvent, SessionDefaults, SessionRegistry, SessionStatus,
};
use ominiforge::monitor::{self, SessionSummary};
use ominiforge::session::SessionMeta;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::{ClientProtocol, ConnectionState, DirEntry, EventStream, FilePreview};

/// The local, in-process `ClientProtocol`: wraps a [`SessionRegistry`].
///
/// Cheap to clone (the registry is `Arc`-backed); the GPUI app constructs one
/// at startup and hands clones to every panel.
#[derive(Clone)]
pub struct LocalProtocol {
    registry: SessionRegistry,
}

impl LocalProtocol {
    /// Build a local protocol over the given session defaults and gateway
    /// config — the same inputs `ominiforge serve` uses, minus the listener.
    ///
    /// # Errors
    /// Propagates [`SessionRegistry::new`] failures (e.g. a sandbox backend
    /// that cannot start on this host).
    pub fn new(defaults: SessionDefaults, config: &GatewayConfig) -> Result<Self> {
        Ok(Self {
            registry: SessionRegistry::new(defaults, config)?,
        })
    }

    /// Like [`new`](Self::new) but with an injected model provider, so tests and
    /// local synthetic runs drive full sessions over a scripted provider with no
    /// `providers.toml` and no network.
    ///
    /// # Errors
    /// Propagates [`SessionRegistry::new_with_provider`] failures.
    pub fn new_with_provider(
        defaults: SessionDefaults,
        config: &GatewayConfig,
        provider: Arc<dyn ominiforge::llm::Provider>,
        resolved: ominiforge::config::ResolvedModel,
    ) -> Result<Self> {
        Ok(Self {
            registry: SessionRegistry::new_with_provider(defaults, config, provider, resolved)?,
        })
    }

    /// Wrap an existing registry (when the caller already owns one, e.g. a
    /// process that also serves the gateway).
    #[must_use]
    pub const fn from_registry(registry: SessionRegistry) -> Self {
        Self { registry }
    }

    /// The underlying registry, for callers that need a handle the trait does
    /// not yet surface.
    #[must_use]
    pub const fn registry(&self) -> &SessionRegistry {
        &self.registry
    }

    /// Estimated token footprint of a session's inherited context snapshot
    /// (mirrors the gateway's `snapshot_tokens`, kept private there).
    fn snapshot_tokens(snapshot: &[ominiforge::llm::Message]) -> u32 {
        let bytes: usize = snapshot.iter().map(message_bytes).sum();
        bytes_to_tokens(bytes)
    }
}

#[async_trait::async_trait]
impl ClientProtocol for LocalProtocol {
    // ---- Session management ----

    async fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        self.registry.list_metas()
    }

    async fn list_archived_sessions(&self) -> Result<Vec<SessionMeta>> {
        self.registry.list_archived_metas()
    }

    async fn create_session(&self) -> Result<SessionId> {
        let (id, _handle) = self.registry.create().await?;
        Ok(id)
    }

    async fn get_session(&self, id: &SessionId) -> Result<SessionMeta> {
        self.registry.meta(id)
    }

    async fn fork_session(&self, parent: &SessionId, at_seq: u64) -> Result<SessionId> {
        let (id, _handle) = self.registry.fork(parent, at_seq).await?;
        Ok(id)
    }

    async fn archive_session(&self, id: &SessionId) -> Result<()> {
        self.registry.archive(id).await
    }

    async fn delete_session(&self, id: &SessionId) -> Result<()> {
        self.registry.delete(id)
    }

    // ---- Messaging ----

    async fn send_message(
        &self,
        id: &SessionId,
        text: String,
        model: Option<String>,
        think_effort: Option<String>,
    ) -> Result<()> {
        let handle = self.registry.get_or_spawn(id).await?;
        handle
            .send(Command::Send {
                text,
                model,
                think_effort,
            })
            .await
            .map_err(|_| anyhow!("session actor is unavailable"))
    }

    async fn cancel_turn(&self, id: &SessionId) -> Result<()> {
        let handle = self.registry.get_or_spawn(id).await?;
        handle
            .send(Command::Cancel)
            .await
            .map_err(|_| anyhow!("session actor is unavailable"))
    }

    async fn compact(&self, id: &SessionId, keep_last: Option<usize>) -> Result<()> {
        let handle = self.registry.get_or_spawn(id).await?;
        handle
            .send(Command::Compact { keep_last })
            .await
            .map_err(|_| anyhow!("session actor is unavailable"))
    }

    async fn approve(
        &self,
        id: &SessionId,
        call_id: String,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    ) -> Result<()> {
        let handle = self.registry.get_or_spawn(id).await?;
        handle
            .send(Command::Approve {
                call_id,
                decision,
                scope,
            })
            .await
            .map_err(|_| anyhow!("session actor is unavailable"))
    }

    // ---- File browsing (read-only) ----

    async fn workspace_root(&self) -> Result<PathBuf> {
        Ok(self.registry.workspace_root().to_path_buf())
    }

    async fn list_dir(&self, rel: &str) -> Result<Vec<DirEntry>> {
        let abs = resolve_in_workspace(self.registry.workspace_root(), rel)?;
        let read =
            std::fs::read_dir(&abs).with_context(|| format!("failed to list directory `{rel}`"))?;
        let mut entries = Vec::new();
        for entry in read {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            entries.push(DirEntry { name, is_dir });
        }
        // Directories first, then files, each by name; the UI's fold relies on
        // this order to interleave children into the visible rows.
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(entries)
    }

    async fn read_file(&self, rel: &str) -> Result<FilePreview> {
        // Preview payload cap: large enough to inspect most source files, small
        // enough to keep the panel snappy. Anything past it is cut + flagged.
        const PREVIEW_CAP: usize = 64 * 1024;
        let abs = resolve_in_workspace(self.registry.workspace_root(), rel)?;
        let bytes = std::fs::read(&abs).with_context(|| format!("failed to read file `{rel}`"))?;
        let truncated = bytes.len() > PREVIEW_CAP;
        let head = if truncated {
            &bytes[..PREVIEW_CAP]
        } else {
            &bytes
        };
        let content = String::from_utf8(head.to_vec())
            .map_err(|_| anyhow!("`{rel}` is not UTF-8 text (binary preview unsupported)"))?;
        Ok(FilePreview {
            path: rel.to_owned(),
            content,
            truncated,
        })
    }

    // ---- Event subscription ----

    async fn subscribe_session(&self, id: &SessionId) -> Result<EventStream<GatewayEvent>> {
        // An archived session has no actor — serve its committed history
        // replay-only (mirrors the SSE endpoint's archived branch).
        if self.registry.store().is_archived(id) {
            let replay = replay_events(&self.registry, id);
            let stream = tokio_stream::iter(replay)
                .chain(tokio_stream::iter([GatewayEvent::ReplayEnd]))
                .chain(tokio_stream::pending());
            return Ok(Box::pin(stream));
        }

        let handle = self.registry.get_or_spawn(id).await?;

        // Subscribe BEFORE reading the log so an event committed in the gap
        // lands in the broadcast buffer; the replay read also includes it, and
        // the UI dedups committed events by seq (same as the SSE endpoint).
        // A `Lagged` error (subscriber fell behind) is dropped, not fatal —
        // the client resyncs from the log on reconnect, so the stream survives.
        let live = BroadcastStream::new(handle.subscribe()).filter_map(Result::ok);
        let replay = replay_events(&self.registry, id);
        let stream = tokio_stream::iter(replay)
            .chain(tokio_stream::iter([GatewayEvent::ReplayEnd]))
            .chain(live);
        Ok(Box::pin(stream))
    }

    async fn subscribe_status(&self) -> Result<EventStream<SessionStatus>> {
        let hub = self.registry.status_hub();
        // Subscribe before snapshot so no transition slips through the gap;
        // statuses are applied idempotently (last-write-wins by session id).
        let live = BroadcastStream::new(hub.subscribe()).filter_map(Result::ok);
        let stream = tokio_stream::iter(hub.snapshot()).chain(live);
        Ok(Box::pin(stream))
    }

    // ---- Monitoring ----

    async fn session_view(&self, id: &SessionId) -> Result<SessionView> {
        let events = self
            .registry
            .store()
            .read_events(id)
            .with_context(|| format!("failed to read session `{}`", id.0))?;
        Ok(fold_view(&events))
    }

    async fn session_summary(&self, id: &SessionId) -> Result<SessionSummary> {
        let events = self
            .registry
            .store()
            .read_events(id)
            .with_context(|| format!("failed to read session `{}`", id.0))?;
        let mut summary = monitor::summarize(&events);
        if let Ok(snapshot) = self.registry.store().read_snapshot(id) {
            summary.context_tokens = summary
                .context_tokens
                .saturating_add(Self::snapshot_tokens(&snapshot));
        }
        Ok(summary)
    }

    // ---- Config ----

    async fn list_profiles(&self) -> Result<Vec<ProfileSummary>> {
        Ok(self.registry.list_profiles())
    }

    async fn list_models(&self) -> Result<Vec<ModelSummary>> {
        self.registry.list_models()
    }

    async fn load_providers(&self) -> Result<ProvidersFile> {
        self.registry.load_providers()
    }

    async fn save_providers(&self, providers: &ProvidersFile) -> Result<()> {
        self.registry.save_providers(providers)
    }

    // ---- Connection state ----

    fn connection_state(&self) -> ConnectionState {
        ConnectionState::Local
    }
}

/// Read the committed events for a session as `GatewayEvent::Event` frames
/// (the replay portion of a subscription). Empty if the session is unreadable
/// — the live tail still attaches, mirroring the SSE endpoint.
fn replay_events(registry: &SessionRegistry, id: &SessionId) -> Vec<GatewayEvent> {
    registry
        .store()
        .read_events(id)
        .unwrap_or_default()
        .into_iter()
        .map(|event| GatewayEvent::Event {
            event: Box::new(event),
        })
        .collect()
}

// Resolve a workspace-relative path against the root, refusing any escape
// (lexical, no filesystem touch — works for paths that do not exist yet).
// Mirrors the file tools' guard rail: `..` that climbs above the root, and
// absolute paths, are rejected before any read.
fn resolve_in_workspace(workspace: &Path, rel: &str) -> Result<PathBuf> {
    if Path::new(rel).is_absolute() {
        bail!("absolute path not allowed: `{rel}`");
    }
    let joined = workspace.join(rel);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path escapes workspace: `{rel}`");
                }
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    if !normalized.starts_with(workspace) {
        bail!("path escapes workspace: `{rel}`");
    }
    Ok(normalized)
}
