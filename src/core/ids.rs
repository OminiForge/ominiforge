//! Identifier types for sessions, turns, events, and artifacts.
//!
//! `SessionId` is a ULID in practice (see `doc/session-storage.md`), but the
//! core type stays a transparent string newtype — generation and validation
//! belong to the `session` module, and keeping `core` free of an id-generation
//! dependency preserves its position at the bottom of the dependency graph.

use serde::{Deserialize, Serialize};

/// Identifies a session. ULID format in practice (time-sortable + random).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Identifies a turn — one iteration of the agent loop — within a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub String);

/// Identifies a stored artifact (tool output, intermediate product, ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactId(pub String);

/// Globally unique event identifier: `session_id` + monotonic `seq`.
///
/// Within a session only `seq` is needed; cross-session references use the full
/// pair. Persisted events omit `session_id` per line (it is the directory
/// name), so `EventId` here is the in-memory/cross-session form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId {
    pub session_id: SessionId,
    pub seq: u64,
}
