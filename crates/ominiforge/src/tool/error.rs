//! Errors a tool invocation can raise at the protocol level.
//!
//! Business-level failures (a command exiting non-zero, a missing file) are
//! *not* errors here: they come back as `Ok(ToolOutput { is_error: true, .. })`
//! so the model sees them and can react. `ToolError` is reserved for protocol
//! faults — malformed input, timeouts, a crashed MCP server. See
//! `doc/tool-protocol.md` §7.

use std::time::Duration;

/// A protocol-level tool failure.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The input did not match the tool's schema (missing/ill-typed field).
    #[error("invalid tool input: {0}")]
    InvalidInput(String),

    /// The tool exceeded its time budget.
    #[error("tool timed out after {0:?}")]
    Timeout(Duration),

    /// A backing process (e.g. an MCP server) died.
    #[error("tool backend crashed: {0}")]
    ServerCrashed(String),

    /// The tool could not run for an environmental reason (e.g. no workspace,
    /// failed to spawn a process).
    #[error("tool execution error: {0}")]
    Execution(String),
}
