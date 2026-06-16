//! The model provider abstraction.
//!
//! A [`Provider`] turns a [`ModelRequest`] into a stream of [`StreamEvent`]s.
//! Events are provider-neutral: the agent loop wraps each one with envelope
//! and timing data to produce a core `ModelEvent` (see `doc/event-schema.md`
//! §5), so the loop never depends on any provider's JSON shape
//! (`doc/architecture.md` §9). Concrete adapters live in `crate::provider`.

mod message;

pub use message::{Message, ModelRequest, ToolCall, ToolSchema};

use futures_util::stream::BoxStream;

use crate::core::payload::{ContentBlockType, StopReason, Usage};

/// A model backend that can stream a completion.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Provider identifier for event sourcing, e.g. `"openai"`.
    fn name(&self) -> &str;

    /// Start a streaming completion.
    ///
    /// The returned stream yields decoded [`StreamEvent`]s until the model
    /// stops. Connection setup and the initial HTTP handshake happen before
    /// this resolves; per-chunk decode errors surface as `Err` items within
    /// the stream.
    ///
    /// # Errors
    /// Returns [`LlmError`] if the request cannot be initiated (network failure,
    /// non-success status, auth error).
    async fn stream(&self, request: ModelRequest) -> Result<EventStream, LlmError>;
}

/// A boxed stream of decoded model events.
pub type EventStream = BoxStream<'static, Result<StreamEvent, LlmError>>;

/// A provider-neutral streaming event.
///
/// Mirrors the streaming subset of the core `ModelEvent` minus the envelope
/// fields (`request_id`, timing, provider metadata) that the agent loop adds.
/// Block indices delimit nested content; `Completed` is terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A new content block opened. For a tool call, `block_type` carries the
    /// call id and name.
    BlockStart {
        index: u32,
        block_type: ContentBlockType,
    },
    /// Incremental assistant text.
    TextDelta { index: u32, text: String },
    /// Incremental reasoning/thinking text (e.g. DeepSeek `reasoning_content`).
    ReasoningDelta { index: u32, text: String },
    /// Incremental tool-call argument JSON.
    ToolCallDelta { index: u32, json_delta: String },
    /// A content block closed.
    BlockStop { index: u32 },
    /// The model finished. Terminal event of a successful stream.
    Completed {
        stop_reason: StopReason,
        usage: Usage,
    },
}

/// An error from a model provider.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// The HTTP request failed to send or the connection dropped.
    #[error("provider transport error: {0}")]
    Transport(String),

    /// The provider returned a non-success HTTP status.
    #[error("provider returned status {status}: {body}")]
    Status { status: u16, body: String },

    /// A streamed chunk could not be decoded.
    #[error("provider response decode error: {0}")]
    Decode(String),

    /// Authentication was missing or rejected.
    #[error("provider auth error: {0}")]
    Auth(String),
}
