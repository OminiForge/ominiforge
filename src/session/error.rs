//! Errors for the session storage layer.

use std::path::PathBuf;

use crate::core::SessionId;

/// Result alias for session storage operations.
pub type Result<T> = std::result::Result<T, SessionError>;

/// Something went wrong reading or writing session data.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The session's `events.jsonl` is already locked by another writer.
    ///
    /// A session allows only one writer at a time; the lock is an advisory
    /// `flock` held for the writer's lifetime. See `doc/session-storage.md` §8.
    #[error("session is in use by another process: {path}")]
    Locked { path: PathBuf },

    /// The requested session directory does not exist.
    #[error("session not found: {0}")]
    NotFound(SessionId),

    /// An underlying filesystem error.
    #[error("session io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A persisted event line could not be (de)serialized.
    #[error("event (de)serialization failed: {0}")]
    Event(#[from] serde_json::Error),

    /// The `session.toml` metadata could not be parsed.
    #[error("session metadata parse failed: {0}")]
    MetaParse(#[from] toml::de::Error),

    /// The `session.toml` metadata could not be serialized.
    #[error("session metadata serialize failed: {0}")]
    MetaSerialize(#[from] toml::ser::Error),
}
