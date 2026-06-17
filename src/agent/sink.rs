//! Live streaming sink: the agent loop's window onto a turn as it unfolds.
//!
//! The collector ([`super::collector`]) persists consolidated `ModelEvent`s for
//! replay, but a front-end also wants the model's output *as it streams* — token
//! by token — so the user sees progress instead of waiting for the whole turn.
//! That live view is a presentation concern, kept out of the persisted history:
//! the collector forwards each streamed delta to a [`StreamSink`] in real time,
//! and the front-end decides how to render it.
//!
//! All methods default to no-ops so a sink only implements the channels it
//! cares about; [`NullSink`] opts into nothing (the headless default).

/// Receives a turn's streamed output as it arrives, for live rendering.
///
/// Methods are called from the collector in stream order on one task, so an
/// implementation need not be `Sync` and can keep simple `&mut self` state (e.g.
/// which channel is currently open). Rendering should stay cheap and
/// non-blocking; this is on the hot path of the model stream.
///
/// `Send` is required so the agent future stays `Send` and can run on a worker
/// thread (gateway / scheduler), not just the current task.
pub trait StreamSink: Send {
    /// A new content block opened. `index` is its position in the response;
    /// for a tool call, `tool` carries the call name.
    fn on_block_start(&mut self, _index: u32, _block: BlockKind<'_>) {}

    /// Incremental assistant text (the answer shown to the user).
    fn on_text(&mut self, _index: u32, _text: &str) {}

    /// Incremental reasoning/thinking text.
    fn on_reasoning(&mut self, _index: u32, _text: &str) {}

    /// Incremental tool-call argument JSON.
    fn on_tool_call_delta(&mut self, _index: u32, _json_delta: &str) {}

    /// A content block closed.
    fn on_block_stop(&mut self, _index: u32) {}

    /// The turn finished (no more blocks will arrive). A place to flush a
    /// trailing newline, close styling, etc.
    fn on_turn_end(&mut self) {}
}

/// What kind of block just opened, passed to [`StreamSink::on_block_start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind<'a> {
    Text,
    Reasoning,
    /// A tool call; carries the tool name (the call id is an internal detail
    /// the sink does not need).
    ToolCall {
        name: &'a str,
    },
}

/// A [`StreamSink`] that discards everything — the default for headless runs
/// (tests, scheduler, gateway batch) where nobody is watching live output.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl StreamSink for NullSink {}
