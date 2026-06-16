//! The agent loop: drives a turn from user input to final answer.
//!
//! A turn ([`Agent::run_turn`]) opens with `TurnEvent::Started`, then runs one
//! or more model rounds. Each round streams a model response (persisted as
//! `ModelEvent`s by [`collector`]); if the model asked for tools, each is
//! dispatched (persisted as `ToolEvent`s) and its result fed back as a `Tool`
//! message before the next round. The loop ends when the model stops without
//! requesting tools, emitting `TurnEvent::Completed`.
//!
//! The caller owns the conversation [`Message`] vector (the context view) and
//! the [`SessionWriter`]; `run_turn` appends to both. Context compaction and
//! prefix-cache management arrive with the `context` module (Phase 2).

mod collector;
mod error;

pub use error::AgentError;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::payload::{
    Content, ErrorDetail, ErrorSeverity, ModelEvent, StopReason, ToolEvent, ToolOutput, ToolSource,
    TurnEvent,
};
use crate::core::{EventId, EventPayload, EventSource, SourceKind, TurnId};
use crate::llm::{Message, ModelRequest, Provider, ToolCall, ToolSchema};
use crate::session::SessionWriter;
use crate::tool::{ToolError, ToolInput, ToolRegistry};

/// Knobs for a turn that do not change between rounds.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Model id sent to the provider (e.g. `gpt-4o`).
    pub model: String,
    /// Sampling temperature.
    pub temperature: f32,
    /// Output token cap, if any.
    pub max_tokens: Option<u32>,
    /// Per-tool-invocation time budget.
    pub tool_timeout: Duration,
    /// Maximum model rounds in one turn before bailing on a tool-call loop.
    pub max_rounds: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            temperature: 0.0,
            max_tokens: None,
            tool_timeout: Duration::from_secs(120),
            max_rounds: 16,
        }
    }
}

/// What a completed turn produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// The final assistant text (the answer shown to the user).
    pub answer: String,
    /// Why the final round stopped.
    pub stop_reason: StopReason,
    /// How many model rounds the turn took.
    pub rounds: u32,
}

/// Couples a model provider with a tool registry and per-turn config.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    config: AgentConfig,
}

impl Agent {
    /// Build an agent.
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
        }
    }

    /// Run one turn: append `input` to `context`, drive model rounds and tool
    /// calls to completion, and persist every event through `writer`.
    ///
    /// `context` is mutated in place — the assistant message and any tool
    /// results are appended, leaving it ready for the next turn.
    ///
    /// # Errors
    /// [`AgentError::Model`] on provider failure, [`AgentError::Session`] on a
    /// persistence failure, or [`AgentError::MaxRounds`] if the tool loop never
    /// settles.
    pub async fn run_turn(
        &self,
        writer: &mut SessionWriter,
        context: &mut Vec<Message>,
        input: String,
    ) -> Result<TurnOutcome, AgentError> {
        let turn_id = TurnId(ulid::Ulid::new().to_string());
        writer.append(
            runtime_source(),
            EventPayload::Turn(TurnEvent::Started {
                turn_id: turn_id.clone(),
                input: Some(input.clone()),
            }),
            None,
            Some(turn_id.clone()),
        )?;
        context.push(Message::User { content: input });

        for round in 0..self.config.max_rounds {
            let outcome = self.run_model_round(writer, context, &turn_id).await?;
            let answer = assistant_text(&outcome.message);
            let tool_calls = assistant_tool_calls(&outcome.message);
            context.push(outcome.message.clone());

            if tool_calls.is_empty() {
                writer.append(
                    runtime_source(),
                    EventPayload::Turn(TurnEvent::Completed {
                        turn_id: turn_id.clone(),
                    }),
                    None,
                    Some(turn_id.clone()),
                )?;
                return Ok(TurnOutcome {
                    answer,
                    stop_reason: outcome.stop_reason,
                    rounds: round + 1,
                });
            }

            for call in tool_calls {
                let event_id = outcome.tool_call_event_ids.get(&call.id).cloned();
                let result = self
                    .dispatch_tool(writer, &turn_id, &call, event_id)
                    .await?;
                context.push(result);
            }
        }

        // Tool loop never settled. Record where it gave up for audit, then error.
        let last = EventId {
            session_id: writer.session_id().clone(),
            seq: writer.next_seq().saturating_sub(1),
        };
        writer.append(
            runtime_source(),
            EventPayload::Turn(TurnEvent::Failed {
                turn_id,
                failed_at_event_id: last,
                retryable: false,
            }),
            None,
            None,
        )?;
        Err(AgentError::MaxRounds(self.config.max_rounds))
    }

    // __APPEND_MARKER__

    /// Run one model round: send the current context, persist the streamed
    /// response, and return the assembled assistant message.
    async fn run_model_round(
        &self,
        writer: &mut SessionWriter,
        context: &[Message],
        turn_id: &TurnId,
    ) -> Result<collector::RoundOutcome, AgentError> {
        let request_id = ulid::Ulid::new().to_string();
        let tools = self.tool_schemas();
        let source = self.model_source();

        let request = ModelRequest {
            model: self.config.model.clone(),
            messages: context.to_vec(),
            tools: tools.clone(),
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
        };

        writer.append(
            source.clone(),
            EventPayload::Model(ModelEvent::RequestStarted {
                request_id: request_id.clone(),
                provider: self.provider.name().to_owned(),
                model: self.config.model.clone(),
                temperature: self.config.temperature,
                max_tokens: self.config.max_tokens,
                tool_schemas_count: u32::try_from(tools.len()).unwrap_or(u32::MAX),
                input_tokens_estimate: 0,
            }),
            None,
            Some(turn_id.clone()),
        )?;

        let started = Instant::now();
        let stream = self.provider.stream(request).await?;
        let outcome =
            collector::collect_round(writer, stream, &source, &request_id, turn_id).await?;

        writer.append(
            source,
            EventPayload::Model(ModelEvent::RequestCompleted {
                request_id,
                stop_reason: outcome.stop_reason,
                usage: outcome.usage,
                duration_ms: duration_ms(started.elapsed()),
                time_to_first_token_ms: None,
                provider_request_id: None,
            }),
            None,
            Some(turn_id.clone()),
        )?;

        Ok(outcome)
    }

    /// Execute one tool call, persisting `ToolEvent`s and returning the `Tool`
    /// message to feed back to the model.
    async fn dispatch_tool(
        &self,
        writer: &mut SessionWriter,
        turn_id: &TurnId,
        call: &ToolCall,
        tool_call_event_id: Option<EventId>,
    ) -> Result<Message, AgentError> {
        // The model's tool-call event is the parent; fall back to a self
        // reference if it was not captured (should not happen).
        let parent = tool_call_event_id.unwrap_or_else(|| EventId {
            session_id: writer.session_id().clone(),
            seq: writer.next_seq(),
        });
        let source = EventSource {
            kind: SourceKind::Tool,
            id: call.name.clone(),
        };

        let args: serde_json::Value = if call.arguments.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str(&call.arguments) {
                Ok(value) => value,
                Err(e) => {
                    return Self::fail_tool(
                        writer,
                        turn_id,
                        &source,
                        &parent,
                        call,
                        0,
                        "invalid_arguments",
                        &format!("tool arguments were not valid JSON: {e}"),
                    );
                }
            }
        };

        writer.append(
            source.clone(),
            EventPayload::Tool(ToolEvent::Started {
                tool_call_event_id: parent.clone(),
                tool_name: call.name.clone(),
                source: ToolSource::Builtin,
                input: args.clone(),
                working_dir: None,
            }),
            Some(parent.clone()),
            Some(turn_id.clone()),
        )?;

        let Some(tool) = self.tools.get(&call.name) else {
            return Self::fail_tool(
                writer,
                turn_id,
                &source,
                &parent,
                call,
                0,
                "unknown_tool",
                &format!("no such tool: {}", call.name),
            );
        };

        let started = Instant::now();
        let input = ToolInput {
            call_id: call.id.clone(),
            input: args,
            timeout: self.config.tool_timeout,
        };
        let elapsed = |start: Instant| duration_ms(start.elapsed());

        match tool.invoke(input).await {
            Ok(output) => {
                let text = render_output(&output);
                let output_bytes = output_bytes(&output);
                writer.append(
                    source,
                    EventPayload::Tool(ToolEvent::Completed {
                        tool_call_event_id: parent.clone(),
                        result: output,
                        duration_ms: elapsed(started),
                        output_bytes,
                        artifacts_created: Vec::new(),
                    }),
                    Some(parent),
                    Some(turn_id.clone()),
                )?;
                Ok(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: text,
                })
            }
            Err(err) => {
                let (code, message) = tool_error_parts(&err);
                Self::fail_tool(
                    writer,
                    turn_id,
                    &source,
                    &parent,
                    call,
                    elapsed(started),
                    code,
                    &message,
                )
            }
        }
    }

    /// Persist a `ToolEvent::Failed` and return the error as a `Tool` message so
    /// the model can react.
    #[allow(clippy::too_many_arguments)]
    fn fail_tool(
        writer: &mut SessionWriter,
        turn_id: &TurnId,
        source: &EventSource,
        parent: &EventId,
        call: &ToolCall,
        duration_ms: u64,
        code: &str,
        message: &str,
    ) -> Result<Message, AgentError> {
        writer.append(
            source.clone(),
            EventPayload::Tool(ToolEvent::Failed {
                tool_call_event_id: parent.clone(),
                duration_ms,
                error: ErrorDetail {
                    code: code.to_owned(),
                    message: message.to_owned(),
                    severity: ErrorSeverity::Error,
                    retryable: false,
                    source_event_id: Some(parent.clone()),
                    provider_raw: None,
                },
            }),
            Some(parent.clone()),
            Some(turn_id.clone()),
        )?;
        Ok(Message::Tool {
            tool_call_id: call.id.clone(),
            content: format!("[{code}] {message}"),
        })
    }

    fn tool_schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .descriptors()
            .into_iter()
            .map(|d| ToolSchema {
                name: d.name,
                description: d.description,
                parameters: d.input_schema,
            })
            .collect()
    }

    fn model_source(&self) -> EventSource {
        EventSource {
            kind: SourceKind::Model,
            id: format!("{}/{}", self.provider.name(), self.config.model),
        }
    }
}

/// Runtime-sourced events (turn lifecycle).
fn runtime_source() -> EventSource {
    EventSource {
        kind: SourceKind::Runtime,
        id: "ominiforge".to_owned(),
    }
}

/// The assistant's free-text content, or empty if it only made tool calls.
fn assistant_text(message: &Message) -> String {
    match message {
        Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// The tool calls in an assistant message (empty for other message kinds).
fn assistant_tool_calls(message: &Message) -> Vec<ToolCall> {
    match message {
        Message::Assistant { tool_calls, .. } => tool_calls.clone(),
        _ => Vec::new(),
    }
}

/// Flatten tool output content into the text fed back to the model. Artifact
/// references become a placeholder until the artifact store lands (Phase 2).
fn render_output(output: &ToolOutput) -> String {
    use std::fmt::Write;

    let mut text = String::new();
    for content in &output.content {
        match content {
            Content::Text(t) => text.push_str(t),
            Content::Image { media_type, .. } => {
                let _ = write!(text, "[image {media_type}]");
            }
            Content::ArtifactRef {
                artifact_id,
                media_type,
            } => {
                let _ = write!(text, "[artifact {} {media_type}]", artifact_id.0);
            }
        }
    }
    text
}

/// Byte size of a tool output's text/image payloads, for monitoring.
fn output_bytes(output: &ToolOutput) -> usize {
    output
        .content
        .iter()
        .map(|c| match c {
            Content::Text(t) => t.len(),
            Content::Image { data, .. } => data.len(),
            Content::ArtifactRef { .. } => 0,
        })
        .sum()
}

/// Split a [`ToolError`] into an event error code and message.
fn tool_error_parts(err: &ToolError) -> (&'static str, String) {
    match err {
        ToolError::InvalidInput(m) => ("invalid_input", m.clone()),
        ToolError::Timeout(d) => ("timeout", format!("timed out after {d:?}")),
        ToolError::ServerCrashed(m) => ("server_crashed", m.clone()),
        ToolError::Execution(m) => ("execution_failed", m.clone()),
    }
}

/// Saturating millisecond conversion for event durations.
fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::core::payload::{ContentBlockType, EventPayload, SessionEvent, Usage};
    use crate::core::{CoreEvent, SourceKind};
    use crate::llm::{EventStream, LlmError, StreamEvent};
    use crate::session::SessionStore;
    use futures_util::stream;
    use std::sync::Mutex;

    /// A provider that replays scripted [`StreamEvent`] batches, one batch per
    /// `stream()` call, so we can drive a multi-round turn deterministically.
    struct ScriptedProvider {
        rounds: Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    }

    impl ScriptedProvider {
        fn new(rounds: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                rounds: Mutex::new(rounds.into_iter().collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        #[allow(clippy::unnecessary_literal_bound)] // trait dictates `-> &str`
        fn name(&self) -> &str {
            "scripted"
        }

        async fn stream(&self, _request: ModelRequest) -> Result<EventStream, LlmError> {
            let batch = self
                .rounds
                .lock()
                .unwrap()
                .pop_front()
                .expect("provider called more times than scripted");
            let items: Vec<Result<StreamEvent, LlmError>> = batch.into_iter().map(Ok).collect();
            Ok(Box::pin(stream::iter(items)))
        }
    }

    fn tool_call_round(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                },
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: args.to_owned(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Completed {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    }

    fn text_round(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::Text,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: text.to_owned(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Completed {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ]
    }

    #[tokio::test]
    async fn turn_runs_tool_call_then_final_answer() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("note.txt"), "secret answer").unwrap();

        // Round 1: model calls `read`. Round 2: model gives the final answer.
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_call_round("call_1", "read", r#"{"path":"note.txt"}"#),
            text_round("the file says: secret answer"),
        ]));

        let mut tools = ToolRegistry::new();
        crate::tool::register_builtin(&mut tools, workspace);

        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store
            .create_new(None, None, vec!["read".to_owned()])
            .unwrap();
        let sid = writer.session_id().clone();
        let mut context = vec![Message::System {
            content: "be helpful".to_owned(),
        }];

        let outcome = agent
            .run_turn(
                &mut writer,
                &mut context,
                "what does note.txt say?".to_owned(),
            )
            .await
            .unwrap();
        drop(writer);

        assert_eq!(outcome.rounds, 2);
        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert_eq!(outcome.answer, "the file says: secret answer");

        // The tool result was fed back into the context for round 2.
        assert!(matches!(context.last(), Some(Message::Assistant { .. })));
        assert!(context.iter().any(|m| matches!(
            m,
            Message::Tool { content, .. } if content.contains("secret answer")
        )));

        // The persisted event stream is replayable and well-formed.
        let events = store.read_events(&sid).unwrap();
        assert!(starts_with_created(&events));
        assert_eq!(turn_started_count(&events), 1);
        assert_eq!(turn_completed_count(&events), 1);
        assert!(has_tool_completed(&events));
        assert!(seqs_are_contiguous(&events));
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_and_turn_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_call_round("call_1", "nonexistent", "{}"),
            text_round("recovered"),
        ]));

        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let mut context = Vec::new();

        let outcome = agent
            .run_turn(&mut writer, &mut context, "do a thing".to_owned())
            .await
            .unwrap();

        assert_eq!(outcome.answer, "recovered");
        assert!(context.iter().any(|m| matches!(
            m,
            Message::Tool { content, .. } if content.contains("unknown_tool")
        )));
    }

    fn starts_with_created(events: &[CoreEvent]) -> bool {
        matches!(
            events.first().map(|e| &e.payload),
            Some(EventPayload::Session(SessionEvent::Created { .. }))
        )
    }

    fn turn_started_count(events: &[CoreEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::Turn(TurnEvent::Started { .. })))
            .count()
    }

    fn turn_completed_count(events: &[CoreEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::Turn(TurnEvent::Completed { .. })))
            .count()
    }

    fn has_tool_completed(events: &[CoreEvent]) -> bool {
        events.iter().any(|e| {
            matches!(e.payload, EventPayload::Tool(ToolEvent::Completed { .. }))
                && e.source.kind == SourceKind::Tool
        })
    }

    fn seqs_are_contiguous(events: &[CoreEvent]) -> bool {
        events.iter().enumerate().all(|(i, e)| e.seq == i as u64)
    }
}
