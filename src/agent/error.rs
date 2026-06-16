//! Errors raised while driving an agent turn.

use crate::llm::LlmError;
use crate::session::SessionError;

/// Something went wrong executing a turn.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The model provider failed.
    #[error("model error: {0}")]
    Model(#[from] LlmError),

    /// Persisting an event failed.
    #[error("session error: {0}")]
    Session(#[from] SessionError),

    /// The turn made too many model round-trips without finishing, indicating
    /// a tool-call loop. The cap is [`AgentConfig::max_rounds`].
    ///
    /// [`AgentConfig::max_rounds`]: super::AgentConfig::max_rounds
    #[error("turn exceeded max model rounds ({0})")]
    MaxRounds(u32),
}
