//! Client protocol abstraction: the one trait every front-end (GPUI app, Web
//! transition front-end, future remote clients) uses to talk to Core
//! (`doc/network.md` §2).
//!
//! [`ClientProtocol`] defines the full client-facing operation surface —
//! session lifecycle, messaging, event subscription, monitoring, config, and
//! connection state. Implementations are pluggable: [`LocalProtocol`] links
//! `ominiforge-core` directly (zero network, zero serialization);
//! `WebSocketProtocol` (Phase 4) connects to a remote Gateway over the wire.
//!
//! The trait is deliberately a thin, transport-shaped contract: it carries the
//! same types the Gateway already exchanges (`SessionMeta`, `CoreEvent`,
//! `GatewayEvent`, `SessionSummary`, `SessionStatus`, …) so the GPUI client
//! never has to care whether those values arrived in-process or over a socket.

mod local;

pub use local::LocalProtocol;

use std::pin::Pin;

use anyhow::Result;
use futures_core::Stream;
use ominiforge::agent::{ApprovalDecision, ApprovalScope};
use ominiforge::config::{ModelSummary, ProfileSummary, ProvidersFile};
use ominiforge::core::SessionId;
use ominiforge::gateway::view::SessionView;
use ominiforge::gateway::{GatewayEvent, SessionStatus};
use ominiforge::monitor::SessionSummary;
use ominiforge::session::SessionMeta;

/// A boxed, sendable stream of protocol events — what a UI panel consumes.
pub type EventStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// How a client is currently attached to Core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Linked in-process (`LocalProtocol`) — always connected.
    Local,
    /// A remote transport is up.
    Connected,
    /// A remote transport is reconnecting / down.
    Disconnected,
}

/// The unified client ↔ Core interface (`doc/network.md` §2.1).
///
/// Every interactive front-end drives Core exclusively through this trait, so
/// the same UI code runs against an in-process core (`LocalProtocol`) or a
/// remote Gateway (`WebSocketProtocol`) without change.
#[async_trait::async_trait]
pub trait ClientProtocol: Send + Sync {
    // ---- Session management ----

    /// List all active (non-archived) sessions, newest first.
    async fn list_sessions(&self) -> Result<Vec<SessionMeta>>;

    /// List archived sessions, newest first.
    async fn list_archived_sessions(&self) -> Result<Vec<SessionMeta>>;

    /// Create a new session, returning its id.
    async fn create_session(&self) -> Result<SessionId>;

    /// Read one session's metadata.
    async fn get_session(&self, id: &SessionId) -> Result<SessionMeta>;

    /// Fork `parent` at `at_seq`, returning the new session id.
    async fn fork_session(&self, parent: &SessionId, at_seq: u64) -> Result<SessionId>;

    /// Archive a session (one-way retirement). The files stay for read-only
    /// inspection; the session can no longer be run (`doc/architecture.md` §9).
    async fn archive_session(&self, id: &SessionId) -> Result<()>;

    /// Permanently delete a session. Requires it to be archived first.
    async fn delete_session(&self, id: &SessionId) -> Result<()>;

    // ---- Messaging ----

    /// Enqueue a user message (a turn). Returns immediately; output streams
    /// over the session's event channel.
    async fn send_message(
        &self,
        id: &SessionId,
        text: String,
        model: Option<String>,
        think_effort: Option<String>,
    ) -> Result<()>;

    /// Abort the running turn, if any.
    async fn cancel_turn(&self, id: &SessionId) -> Result<()>;

    /// Summarize and switch to a compaction session; `keep_last` keeps the last
    /// N user turns verbatim.
    async fn compact(&self, id: &SessionId, keep_last: Option<usize>) -> Result<()>;

    /// Deliver a human decision for a suspended tool call (`doc/permission.md` §5).
    async fn approve(
        &self,
        id: &SessionId,
        call_id: String,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    ) -> Result<()>;

    // ---- Event subscription ----

    /// Subscribe to a session's event stream: committed-event replay (from the
    /// durable log), then the `ReplayEnd` boundary, then live committed events
    /// and deltas — the same ordering the SSE endpoint guarantees.
    async fn subscribe_session(&self, id: &SessionId) -> Result<EventStream<GatewayEvent>>;

    /// Subscribe to the gateway-wide session-activity stream (one status feed
    /// for every session): a snapshot of current statuses, then live deltas.
    async fn subscribe_status(&self) -> Result<EventStream<SessionStatus>>;

    // ---- Monitoring ----

    /// The folded, render-ready conversation view rebuilt from the event log.
    async fn session_view(&self, id: &SessionId) -> Result<SessionView>;

    /// Derived monitor metrics for one session.
    async fn session_summary(&self, id: &SessionId) -> Result<SessionSummary>;

    // ---- Config ----

    /// List available profiles (summary view).
    async fn list_profiles(&self) -> Result<Vec<ProfileSummary>>;

    /// List available models.
    async fn list_models(&self) -> Result<Vec<ModelSummary>>;

    /// Load the merged provider set.
    async fn load_providers(&self) -> Result<ProvidersFile>;

    /// Overwrite the provider set (full desired state).
    async fn save_providers(&self, providers: &ProvidersFile) -> Result<()>;

    // ---- Connection state ----

    /// The current connection state. `LocalProtocol` is always
    /// [`ConnectionState::Local`].
    fn connection_state(&self) -> ConnectionState;
}
