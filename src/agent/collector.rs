//! Stream collection: consume a provider's [`StreamEvent`] stream, persist each
//! as a core `ModelEvent`, and assemble the assistant message.
//!
//! The collector handles the streaming middle of one model round. The caller
//! brackets it with `RequestStarted` / `RequestCompleted` because it owns the
//! request timing; the collector returns the `stop_reason` and `usage` it saw
//! so the caller can fill in `RequestCompleted`.

use std::collections::HashMap;

use futures_util::StreamExt;

use super::error::AgentError;
use crate::core::payload::{ContentBlockType, ModelEvent, StopReason, Usage};
use crate::core::{EventId, EventPayload, EventSource, TurnId};
use crate::llm::{EventStream, Message, StreamEvent, ToolCall};
use crate::session::SessionWriter;

/// What one model round produced.
pub struct RoundOutcome {
    /// The assembled assistant message (text and/or tool calls).
    pub message: Message,
    /// Why the model stopped.
    pub stop_reason: StopReason,
    /// Token accounting reported by the provider.
    pub usage: Usage,
    /// Maps each tool-call id to the `ContentBlockStart` event that announced
    /// it, so tool execution can record `tool_call_event_id`.
    pub tool_call_event_ids: HashMap<String, EventId>,
}

/// Accumulates one tool call's id, name, and streamed argument JSON.
struct ToolAccum {
    index: u32,
    id: String,
    name: String,
    arguments: String,
}

/// Consume `stream`, writing a `ModelEvent` per item and returning the
/// assembled round outcome.
pub async fn collect_round(
    writer: &mut SessionWriter,
    mut stream: EventStream,
    source: &EventSource,
    request_id: &str,
    turn_id: &TurnId,
) -> Result<RoundOutcome, AgentError> {
    let mut state = Collector::new(writer, source, request_id, turn_id);
    while let Some(event) = stream.next().await {
        state.accept(event?)?;
    }
    Ok(state.finish())
}

/// Per-round mutable accumulation, factored out so the event loop stays small.
struct Collector<'a> {
    writer: &'a mut SessionWriter,
    source: &'a EventSource,
    request_id: &'a str,
    turn_id: &'a TurnId,
    text: String,
    tools: Vec<ToolAccum>,
    tool_call_event_ids: HashMap<String, EventId>,
    stop_reason: StopReason,
    usage: Usage,
}

impl<'a> Collector<'a> {
    fn new(
        writer: &'a mut SessionWriter,
        source: &'a EventSource,
        request_id: &'a str,
        turn_id: &'a TurnId,
    ) -> Self {
        Self {
            writer,
            source,
            request_id,
            turn_id,
            text: String::new(),
            tools: Vec::new(),
            tool_call_event_ids: HashMap::new(),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn accept(&mut self, event: StreamEvent) -> Result<(), AgentError> {
        match event {
            StreamEvent::BlockStart { index, block_type } => self.on_block_start(index, block_type),
            StreamEvent::TextDelta { index, text } => {
                self.text.push_str(&text);
                self.write(ModelEvent::TextDelta {
                    request_id: self.request_id.to_owned(),
                    index,
                    text,
                })
            }
            StreamEvent::ReasoningDelta { index, text } => {
                // Reasoning is persisted for audit but not fed back to the
                // model, so it is not accumulated into the message content.
                self.write(ModelEvent::ReasoningDelta {
                    request_id: self.request_id.to_owned(),
                    index,
                    text,
                })
            }
            StreamEvent::ToolCallDelta { index, json_delta } => {
                if let Some(accum) = self.tools.iter_mut().find(|t| t.index == index) {
                    accum.arguments.push_str(&json_delta);
                }
                self.write(ModelEvent::ToolCallDelta {
                    request_id: self.request_id.to_owned(),
                    index,
                    json_delta,
                })
            }
            StreamEvent::BlockStop { index } => self.write(ModelEvent::ContentBlockStop {
                request_id: self.request_id.to_owned(),
                index,
            }),
            StreamEvent::Completed { stop_reason, usage } => {
                self.stop_reason = stop_reason;
                self.usage = usage;
                Ok(())
            }
        }
    }

    fn on_block_start(
        &mut self,
        index: u32,
        block_type: ContentBlockType,
    ) -> Result<(), AgentError> {
        if let ContentBlockType::ToolCall { id, name } = &block_type {
            self.tools.push(ToolAccum {
                index,
                id: id.clone(),
                name: name.clone(),
                arguments: String::new(),
            });
            let call_id = id.clone();
            let seq = self.write_returning(ModelEvent::ContentBlockStart {
                request_id: self.request_id.to_owned(),
                index,
                block_type: block_type.clone(),
            })?;
            let event_id = EventId {
                session_id: self.writer.session_id().clone(),
                seq,
            };
            self.tool_call_event_ids.insert(call_id, event_id);
            Ok(())
        } else {
            self.write(ModelEvent::ContentBlockStart {
                request_id: self.request_id.to_owned(),
                index,
                block_type,
            })
        }
    }

    fn finish(self) -> RoundOutcome {
        let message = Message::Assistant {
            content: (!self.text.is_empty()).then_some(self.text),
            tool_calls: self
                .tools
                .into_iter()
                .map(|t| ToolCall {
                    id: t.id,
                    name: t.name,
                    arguments: t.arguments,
                })
                .collect(),
        };
        RoundOutcome {
            message,
            stop_reason: self.stop_reason,
            usage: self.usage,
            tool_call_event_ids: self.tool_call_event_ids,
        }
    }

    fn write(&mut self, event: ModelEvent) -> Result<(), AgentError> {
        self.write_returning(event).map(|_| ())
    }

    fn write_returning(&mut self, event: ModelEvent) -> Result<u64, AgentError> {
        Ok(self.writer.append(
            self.source.clone(),
            EventPayload::Model(event),
            None,
            Some(self.turn_id.clone()),
        )?)
    }
}
