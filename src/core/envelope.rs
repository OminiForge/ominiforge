//! The event envelope shared by every persisted event.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{EventId, SessionId, TurnId};
use super::payload::EventPayload;

/// The unified event envelope. Every event carries these fields plus a
/// domain-specific [`EventPayload`]. See `doc/event-schema.md` §2.
///
/// `session_id` is held in memory but omitted on disk (it is the session
/// directory name); see `doc/session-storage.md` §3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreEvent {
    /// Protocol version, e.g. `"ominiforge.event.v1"`.
    pub schema_version: String,

    /// Monotonic sequence number within the session; guarantees strict order.
    pub seq: u64,

    /// The session this event belongs to.
    pub session_id: SessionId,

    /// UTC timestamp.
    pub timestamp: DateTime<Utc>,

    /// Where the event originated.
    pub source: EventSource,

    /// The upstream event that caused this one, if any (causal link).
    #[serde(default)]
    pub parent_event_id: Option<EventId>,

    /// The turn this event belongs to, once a turn has started.
    #[serde(default)]
    pub turn_id: Option<TurnId>,

    /// The domain payload.
    pub payload: EventPayload,
}

/// Where an event came from. `kind` enables fast routing/filtering; `id`
/// names the concrete instance (e.g. `"shell"`, `"mcp://github-server"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSource {
    pub kind: SourceKind,
    pub id: String,
}

/// Coarse classification of an event source.
///
/// No `Plugin` variant: the WASM plugin model was dropped; external extensions
/// are MCP servers, classified as [`SourceKind::External`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    /// An LLM provider.
    Model,
    /// A tool execution.
    Tool,
    /// The Ominiforge runtime itself.
    Runtime,
    /// A user action.
    User,
    /// System-level actors such as the scheduler or evolution worker.
    System,
    /// External actors: MCP servers, remote A2A agents.
    External,
}
