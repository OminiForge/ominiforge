//! Rebuild a [`SessionRuntime`] from a session's persisted event stream.
//!
//! Resuming a session means reconstructing exactly what the model last saw: the
//! conversation view (`Vec<Message>`) and the working plan. Both are derived
//! from `events.jsonl` — the source of truth — so a session picked up in a new
//! process continues as if it never stopped (`doc/context-management.md` §6,
//! `doc/plan.md` §10.3).
//!
//! This module is pure: it takes already-read events and returns state, with no
//! I/O. The caller (the chat loop) reads the events, seeds the system message
//! from the profile — the system prompt is *not* in the event log
//! (`doc/session-storage.md` §2) — and hands both here.
//!
//! ## How events map back to messages
//!
//! The mapping is the inverse of how the agent loop writes them, so a rebuilt
//! view is byte-identical to the live one:
//! - `TurnEvent::Started { input }` → a `User` message.
//! - Consecutive `ModelEvent::ContentBlock`s of one request → one `Assistant`
//!   message (text joined, tool calls collected; reasoning is recorded but never
//!   fed back, matching [`super::collector`]).
//! - `ToolEvent::Completed`/`Failed` → a `Tool` message. The model addresses a
//!   tool result by the *call* id (a string like `call_9`), but the event only
//!   stores the `EventId` of the `ContentBlock` that emitted the call; we map
//!   `seq → call_id` on the way through and look it up.
//! - `InjectionEvent::ContextInjected` → a `User` message (runtime reminders are
//!   pushed as user turns by [`super::TurnState::inject_runtime`]).
//!
//! The plan is rebuilt separately by replaying each `plan` tool call's op, with
//! the same error tolerance as live dispatch (a bad op was never applied, so
//! replay skips it too).

use std::collections::HashMap;

use crate::core::CoreEvent;
use crate::core::SourceKind;
use crate::core::payload::{BlockContent, EventPayload, ModelEvent, ToolEvent, TurnEvent};
use crate::llm::{Message, ToolCall};

use super::plan::{PLAN_TOOL_NAME, PlanOp, apply_plan_op};
use super::{PlanStep, SessionRuntime, render_output};

/// Rebuild a [`SessionRuntime`] from `events`, seeded with `system` (the system
/// message(s) the caller derived from the profile).
///
/// `events` is the full stream as returned by `SessionStore::read_events`. The
/// returned runtime is ready to drive the next turn.
#[must_use]
pub fn rebuild_runtime(events: &[CoreEvent], system: Vec<Message>) -> SessionRuntime {
    SessionRuntime {
        context: rebuild_context(events, system),
        plan: rebuild_plan(events),
    }
}

/// Rebuild the conversation view: `system` followed by the messages replayed
/// from `events`.
fn rebuild_context(events: &[CoreEvent], system: Vec<Message>) -> Vec<Message> {
    let mut builder = ContextRebuilder::new(system);
    for event in events {
        builder.accept(event);
    }
    builder.finish()
}

/// An assistant message under construction, accumulating the content blocks of a
/// single model request before being flushed into the view.
#[derive(Default)]
struct PendingAssistant {
    /// The request these blocks belong to; a block from a different request
    /// flushes this one first.
    request_id: Option<String>,
    text: String,
    tool_calls: Vec<ToolCall>,
}

/// Folds an event stream back into a `Vec<Message>`, mirroring how the agent
/// loop appended them.
struct ContextRebuilder {
    ctx: Vec<Message>,
    pending: Option<PendingAssistant>,
    /// `ContentBlock` event seq → the tool *call* id it carried, so a later
    /// `ToolEvent` can recover the call id the model uses to address the result.
    call_ids: HashMap<u64, String>,
}

impl ContextRebuilder {
    fn new(system: Vec<Message>) -> Self {
        Self {
            ctx: system,
            pending: None,
            call_ids: HashMap::new(),
        }
    }

    /// Fold one event into the view.
    fn accept(&mut self, event: &CoreEvent) {
        match &event.payload {
            EventPayload::Turn(TurnEvent::Started {
                input: Some(input), ..
            }) => {
                self.flush();
                self.ctx.push(Message::User {
                    content: input.clone(),
                });
            }
            EventPayload::Model(ModelEvent::ContentBlock {
                request_id,
                content,
                ..
            }) => self.accept_block(event.seq, request_id, content),
            EventPayload::Tool(ToolEvent::Completed {
                tool_call_event_id,
                result,
                ..
            }) => {
                self.flush();
                self.ctx.push(Message::Tool {
                    tool_call_id: self.call_id_for(tool_call_event_id.seq),
                    content: render_output(result),
                });
            }
            EventPayload::Tool(ToolEvent::Failed {
                tool_call_event_id,
                error,
                ..
            }) => {
                self.flush();
                self.ctx.push(Message::Tool {
                    tool_call_id: self.call_id_for(tool_call_event_id.seq),
                    content: format!("[{}] {}", error.code, error.message),
                });
            }
            EventPayload::Injection(crate::core::payload::InjectionEvent::ContextInjected {
                content,
                ..
            }) => {
                self.flush();
                self.ctx.push(Message::User {
                    content: content.clone(),
                });
            }
            _ => {}
        }
    }

    /// Fold one content block into the pending assistant message, starting a
    /// fresh one when the request id changes. Reasoning is skipped (it is logged
    /// for audit but never fed back to the model — see [`super::collector`]).
    fn accept_block(&mut self, seq: u64, request_id: &str, content: &BlockContent) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.request_id.as_deref() != Some(request_id))
        {
            self.flush();
        }
        let pending = self.pending.get_or_insert_with(|| PendingAssistant {
            request_id: Some(request_id.to_owned()),
            ..PendingAssistant::default()
        });
        match content {
            BlockContent::Text { text } => pending.text.push_str(text),
            BlockContent::ToolCall {
                id,
                name,
                arguments,
            } => {
                pending.tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                self.call_ids.insert(seq, id.clone());
            }
            BlockContent::Reasoning { .. } => {}
        }
    }

    /// The call id recorded for a `ContentBlock` seq, falling back to the seq as
    /// a string if it was never seen (should not happen for a well-formed log).
    fn call_id_for(&self, seq: u64) -> String {
        self.call_ids
            .get(&seq)
            .cloned()
            .unwrap_or_else(|| seq.to_string())
    }

    /// Push any pending assistant message into the view. A message with neither
    /// text nor tool calls is dropped (matching the collector's empty-block rule).
    fn flush(&mut self) {
        if let Some(pending) = self.pending.take() {
            let content = (!pending.text.is_empty()).then_some(pending.text);
            if content.is_some() || !pending.tool_calls.is_empty() {
                self.ctx.push(Message::Assistant {
                    content,
                    tool_calls: pending.tool_calls,
                });
            }
        }
    }

    /// Flush the trailing assistant message and return the assembled view.
    fn finish(mut self) -> Vec<Message> {
        self.flush();
        self.ctx
    }
}

/// Rebuild the working plan by replaying each `plan` tool call's op in order.
///
/// An op that fails to decode or apply is skipped — live dispatch never applied
/// it either (a bad op yields an `is_error` result, leaving the plan unchanged),
/// so the replayed plan matches the runtime plan at the time the session stopped.
fn rebuild_plan(events: &[CoreEvent]) -> Vec<PlanStep> {
    let mut plan = Vec::new();
    for event in events {
        if event.source.kind != SourceKind::Tool || event.source.id != PLAN_TOOL_NAME {
            continue;
        }
        if let EventPayload::Tool(ToolEvent::Started { input, .. }) = &event.payload
            && let Ok(op) = serde_json::from_value::<PlanOp>(input.clone())
        {
            let _ = apply_plan_op(&mut plan, op);
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::payload::{
        Content, ErrorDetail, ErrorSeverity, InjectionEvent, InjectionSource, ToolOutput,
        ToolSource, Usage,
    };
    use crate::core::{
        CoreEvent, EventId, EventSource, SCHEMA_VERSION, SessionId, SourceKind, TurnId,
    };
    use crate::llm::Message;

    fn sid() -> SessionId {
        SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned())
    }

    /// Build a `CoreEvent` with the given seq/source/payload. Timestamp and ids
    /// not relevant to reconstruction are filled with neutral values.
    fn ev(seq: u64, source: EventSource, payload: EventPayload) -> CoreEvent {
        CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            session_id: sid(),
            timestamp: chrono::Utc::now(),
            source,
            parent_event_id: None,
            turn_id: Some(TurnId("t".to_owned())),
            payload,
        }
    }

    fn runtime_src() -> EventSource {
        EventSource {
            kind: SourceKind::Runtime,
            id: "ominiforge".to_owned(),
        }
    }

    fn model_src() -> EventSource {
        EventSource {
            kind: SourceKind::Model,
            id: "test/m".to_owned(),
        }
    }

    fn tool_src(name: &str) -> EventSource {
        EventSource {
            kind: SourceKind::Tool,
            id: name.to_owned(),
        }
    }

    fn started(input: &str) -> EventPayload {
        EventPayload::Turn(TurnEvent::Started {
            turn_id: TurnId("t".to_owned()),
            input: Some(input.to_owned()),
        })
    }

    fn text_block(request_id: &str, text: &str) -> EventPayload {
        EventPayload::Model(ModelEvent::ContentBlock {
            request_id: request_id.to_owned(),
            index: 0,
            content: BlockContent::Text {
                text: text.to_owned(),
            },
        })
    }

    fn tool_call_block(request_id: &str, id: &str, name: &str, args: &str) -> EventPayload {
        EventPayload::Model(ModelEvent::ContentBlock {
            request_id: request_id.to_owned(),
            index: 0,
            content: BlockContent::ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: args.to_owned(),
            },
        })
    }

    fn tool_completed(call_event_seq: u64, text: &str) -> EventPayload {
        EventPayload::Tool(ToolEvent::Completed {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: call_event_seq,
            },
            result: ToolOutput {
                content: vec![Content::Text(text.to_owned())],
                is_error: false,
                error_code: None,
            },
            duration_ms: 1,
            output_bytes: text.len(),
            artifacts_created: vec![],
        })
    }

    fn plan_started(input: serde_json::Value) -> EventPayload {
        EventPayload::Tool(ToolEvent::Started {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: 0,
            },
            tool_name: PLAN_TOOL_NAME.to_owned(),
            source: ToolSource::Builtin,
            input,
            working_dir: None,
        })
    }

    /// A two-turn conversation rebuilds into system + user/assistant/user/assistant,
    /// in order, with the system seed preserved at the front. This is the core
    /// "resume remembers the conversation" guarantee.
    #[test]
    fn rebuilds_multi_turn_conversation_in_order() {
        let events = vec![
            ev(0, model_src(), text_block("r0", "")), // ignored: not a turn/tool
            ev(1, runtime_src(), started("remember the number 42")),
            ev(2, model_src(), text_block("r1", "Got it, 42.")),
            ev(3, runtime_src(), started("what number?")),
            ev(4, model_src(), text_block("r2", "42")),
        ];
        let system = vec![Message::System {
            content: "be helpful".to_owned(),
        }];

        let rt = rebuild_runtime(&events, system);

        assert_eq!(
            rt.context,
            vec![
                Message::System {
                    content: "be helpful".to_owned()
                },
                Message::User {
                    content: "remember the number 42".to_owned()
                },
                Message::Assistant {
                    content: Some("Got it, 42.".to_owned()),
                    tool_calls: vec![]
                },
                Message::User {
                    content: "what number?".to_owned()
                },
                Message::Assistant {
                    content: Some("42".to_owned()),
                    tool_calls: vec![]
                },
            ]
        );
        assert!(rt.plan.is_empty());
    }

    /// A tool-calling round rebuilds into an assistant message carrying the call
    /// followed by a `Tool` message whose `tool_call_id` is the *call* id (looked
    /// up from the `ContentBlock` seq), not the event seq — so the model can match
    /// the result to its request.
    #[test]
    fn rebuilds_tool_call_and_result_with_correct_call_id() {
        let events = vec![
            ev(0, runtime_src(), started("read the note")),
            // ContentBlock at seq 1 emits tool call "call_9".
            ev(
                1,
                model_src(),
                tool_call_block("r1", "call_9", "read", r#"{"path":"n"}"#),
            ),
            // Tool result points back at seq 1.
            ev(2, tool_src("read"), tool_completed(1, "file contents")),
            ev(3, model_src(), text_block("r2", "done")),
        ];

        let rt = rebuild_runtime(&events, vec![]);

        assert_eq!(
            rt.context,
            vec![
                Message::User {
                    content: "read the note".to_owned()
                },
                Message::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_9".to_owned(),
                        name: "read".to_owned(),
                        arguments: r#"{"path":"n"}"#.to_owned(),
                    }]
                },
                Message::Tool {
                    tool_call_id: "call_9".to_owned(),
                    content: "file contents".to_owned()
                },
                Message::Assistant {
                    content: Some("done".to_owned()),
                    tool_calls: vec![]
                },
            ]
        );
    }

    /// A failed tool event rebuilds into a `Tool` message formatted exactly as
    /// the live loop formats it (`[code] message`), so a resumed turn sees the
    /// same error text the model originally got.
    #[test]
    fn rebuilds_failed_tool_as_error_message() {
        let events = vec![
            ev(0, runtime_src(), started("do it")),
            ev(1, model_src(), tool_call_block("r1", "call_1", "bad", "{}")),
            ev(
                2,
                tool_src("bad"),
                EventPayload::Tool(ToolEvent::Failed {
                    tool_call_event_id: EventId {
                        session_id: sid(),
                        seq: 1,
                    },
                    duration_ms: 0,
                    error: ErrorDetail {
                        code: "unknown_tool".to_owned(),
                        message: "no such tool: bad".to_owned(),
                        severity: ErrorSeverity::Error,
                        retryable: false,
                        source_event_id: None,
                        provider_raw: None,
                    },
                }),
            ),
        ];

        let rt = rebuild_runtime(&events, vec![]);

        assert_eq!(
            rt.context.last().unwrap(),
            &Message::Tool {
                tool_call_id: "call_1".to_owned(),
                content: "[unknown_tool] no such tool: bad".to_owned(),
            }
        );
    }

    /// Reasoning blocks are reconstructed-but-excluded from the message (only
    /// recorded for audit), exactly as the collector does when persisting.
    #[test]
    fn reasoning_blocks_are_excluded_from_rebuilt_message() {
        let events = vec![
            ev(0, runtime_src(), started("hi")),
            ev(
                1,
                model_src(),
                EventPayload::Model(ModelEvent::ContentBlock {
                    request_id: "r1".to_owned(),
                    index: 0,
                    content: BlockContent::Reasoning {
                        text: "thinking hard".to_owned(),
                    },
                }),
            ),
            ev(2, model_src(), text_block("r1", "hello")),
        ];

        let rt = rebuild_runtime(&events, vec![]);

        // The reasoning text never appears; the assistant message is just "hello".
        assert_eq!(
            rt.context.last().unwrap(),
            &Message::Assistant {
                content: Some("hello".to_owned()),
                tool_calls: vec![]
            }
        );
    }

    /// An injected runtime reminder rebuilds into a `User` message, preserving
    /// what the model saw (the completion-gate / stuck reminders).
    #[test]
    fn rebuilds_injection_as_user_message() {
        let events = vec![
            ev(0, runtime_src(), started("go")),
            ev(1, model_src(), text_block("r1", "stopping early")),
            ev(
                2,
                runtime_src(),
                EventPayload::Injection(InjectionEvent::ContextInjected {
                    source: InjectionSource::Runtime,
                    content: "<reminder>finish the plan</reminder>".to_owned(),
                    token_count: 5,
                }),
            ),
        ];

        let rt = rebuild_runtime(&events, vec![]);

        assert_eq!(
            rt.context.last().unwrap(),
            &Message::User {
                content: "<reminder>finish the plan</reminder>".to_owned()
            }
        );
    }

    /// The plan is rebuilt by replaying plan ops; a malformed op is skipped just
    /// as live dispatch never applied it, so the final plan matches runtime state.
    #[test]
    fn rebuilds_plan_by_replaying_ops_and_skips_bad_ones() {
        let events = vec![
            ev(0, runtime_src(), started("two-step task")),
            ev(
                1,
                tool_src(PLAN_TOOL_NAME),
                plan_started(serde_json::json!({
                    "op": "init",
                    "steps": [{"content": "step one"}, {"content": "step two"}]
                })),
            ),
            ev(
                2,
                tool_src(PLAN_TOOL_NAME),
                plan_started(serde_json::json!({"op": "start", "id": "1"})),
            ),
            // Malformed op (cancel without reason) — must be skipped, not panic.
            ev(
                3,
                tool_src(PLAN_TOOL_NAME),
                plan_started(serde_json::json!({"op": "cancel", "id": "2"})),
            ),
            ev(
                4,
                tool_src(PLAN_TOOL_NAME),
                plan_started(serde_json::json!({"op": "complete", "id": "1"})),
            ),
        ];

        let rt = rebuild_runtime(&events, vec![]);

        assert_eq!(rt.plan.len(), 2);
        assert_eq!(rt.plan[0].id, "1");
        assert_eq!(rt.plan[0].status, super::super::StepStatus::Completed);
        // Step two stayed pending: the cancel op was malformed and skipped.
        assert_eq!(rt.plan[1].id, "2");
        assert_eq!(rt.plan[1].status, super::super::StepStatus::Pending);
    }

    /// Usage/seq fields on events don't matter to reconstruction; an empty
    /// stream yields just the system seed.
    #[test]
    fn empty_stream_yields_system_seed_only() {
        let _ = Usage::default();
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let rt = rebuild_runtime(&[], system.clone());
        assert_eq!(rt.context, system);
        assert!(rt.plan.is_empty());
    }
}
