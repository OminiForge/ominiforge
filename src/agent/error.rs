//! Errors raised while driving an agent turn.

use crate::llm::LlmError;
use crate::session::SessionError;

/// A hard failure that aborts a turn: the model provider or event persistence
/// broke.
///
/// A turn that merely ran out of round budget or stalled on its plan is *not*
/// an error — it returns a [`TurnOutcome`] flagged incomplete with a
/// [`TurnFailureReason`], so its side effects and partial output are preserved
/// (see [`TurnOutcome::incomplete`]).
///
/// A hard failure still leaves a trace before it propagates: the loop makes a
/// best-effort write of an `ErrorEvent::Raised` plus a `TurnEvent::Failed`
/// (`reason: None`) so every turn termination is visible to replay/monitor. If
/// the persistence layer itself is what broke, that closing write may also fail
/// — it is then silently abandoned and the original error propagates unmasked.
///
/// [`TurnOutcome`]: super::TurnOutcome
/// [`TurnOutcome::incomplete`]: super::TurnOutcome::incomplete
/// [`TurnFailureReason`]: crate::core::payload::TurnFailureReason
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The model provider failed.
    #[error("model error: {0}")]
    Model(#[from] LlmError),

    /// Persisting an event failed.
    #[error("session error: {0}")]
    Session(#[from] SessionError),
}
