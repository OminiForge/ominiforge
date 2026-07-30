//! Stream collection: consume a provider's [`StreamEvent`] stream, forward each
//! delta to a live [`StreamSink`], accumulate per-block content, and persist one
//! consolidated `ModelEvent::ContentBlock` per block.
//!
//! Streaming is two concerns kept apart. *Live transport* — the sink — sees
//! every token as it arrives, so a front-end can render progress. *Persistence*
//! — the event log — records the assembled block once it is whole, so history
//! stays compact and replayable instead of one line per token.
//!
//! The collector handles the streaming middle of one model round. The caller
//! brackets it with `RequestStarted` / `RequestCompleted` because it owns the
//! request timing; the collector returns the `stop_reason` and `usage` it saw
//! so the caller can fill in `RequestCompleted`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;

use super::error::AgentError;
use super::sink::{BlockKind, StreamSink};
use crate::core::payload::{BlockContent, ContentBlockType, ModelEvent, StopReason, Usage};
use crate::core::{EventId, EventPayload, EventSource, TurnId};
use crate::llm::{EventStream, Message, StreamEvent, ToolCall};
use crate::session::SessionWriter;
use crate::tool::{StreamPresenter, ToolRegistry};

/// Minimum interval between stage-2 [`StreamPresenter`] frames for one block
/// (`doc/tool-streaming.md` §2): model tokens arrive in bursts, so snapshots
/// are throttled to keep the front-end from re-rendering per token. The final
/// frame before a block's `BlockStop` always flushes regardless of interval.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(120);

/// Minimum growth of the accumulated args before a new frame is emitted even
/// if [`PROGRESS_MIN_INTERVAL`] has elapsed — a sub-line trickle isn't worth a
/// re-render. The final flush ignores this too.
const PROGRESS_MIN_GROWTH: usize = 64;

/// What one model round produced.
pub struct RoundOutcome {
    /// The assembled assistant message (text and/or tool calls).
    pub message: Message,
    /// Why the model stopped.
    pub stop_reason: StopReason,
    /// Token accounting reported by the provider.
    pub usage: Usage,
    /// Maps each tool-call id to the `ContentBlock` event that recorded it, so
    /// tool execution can record `tool_call_event_id`.
    pub tool_call_event_ids: HashMap<String, EventId>,
    /// Maps each tool-call id to its stream block index, so a tool that
    /// streams RESULTS mid-execution (`shell` output, `doc/tool-streaming.md`
    /// §5) can address live progress frames to the right streaming card.
    pub tool_call_block_index: HashMap<String, u32>,
}

/// One in-progress content block, accumulating its streamed pieces in order.
enum Block {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        /// Stage-2 presenter, if this tool streams a live view (None for most
        /// tools — `doc/tool-streaming.md`). Drives `on_tool_call_progress`.
        presenter: Option<Box<dyn StreamPresenter>>,
        /// Throttle state: when the last frame went out and how long the args
        /// were then (growth is measured from this).
        last_emit: Option<Instant>,
        last_emit_len: usize,
    },
}

/// Consume `stream`, forwarding deltas to `sink`, writing one consolidated
/// `ContentBlock` event per block, and returning the assembled round outcome.
pub async fn collect_round(
    writer: &mut SessionWriter,
    sink: &mut dyn StreamSink,
    mut stream: EventStream,
    source: &EventSource,
    request_id: &str,
    turn_id: &TurnId,
    tools: Option<&ToolRegistry>,
) -> Result<RoundOutcome, AgentError> {
    let mut state = Collector::new(writer, source, request_id, turn_id, tools);
    while let Some(event) = stream.next().await {
        state.accept(event?, sink).await;
    }
    state.finish()
}

/// Per-round mutable accumulation, factored out so the event loop stays small.
struct Collector<'a> {
    writer: &'a mut SessionWriter,
    source: &'a EventSource,
    request_id: &'a str,
    turn_id: &'a TurnId,
    /// Tool registry for stage-2 presenter lookup; None in contexts with no
    /// live rendering (tests, headless) — presenters simply never attach.
    tools: Option<&'a ToolRegistry>,
    /// Blocks in open order; `blocks[i]` is the block at stream index `i`.
    blocks: Vec<Block>,
    stop_reason: StopReason,
    usage: Usage,
}

impl<'a> Collector<'a> {
    fn new(
        writer: &'a mut SessionWriter,
        source: &'a EventSource,
        request_id: &'a str,
        turn_id: &'a TurnId,
        tools: Option<&'a ToolRegistry>,
    ) -> Self {
        Self {
            writer,
            source,
            request_id,
            turn_id,
            tools,
            blocks: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    /// Forward one streamed event to the live sink and fold it into the
    /// in-progress block accumulation. Nothing is persisted here — consolidated
    /// `ContentBlock` events are written once, in [`finish`](Self::finish).
    async fn accept(&mut self, event: StreamEvent, sink: &mut dyn StreamSink) {
        match event {
            StreamEvent::BlockStart { index, block_type } => {
                sink.on_block_start(index, block_kind(&block_type));
                self.blocks.push(self.new_block(block_type));
            }
            StreamEvent::TextDelta { index, text } => {
                sink.on_text(index, &text);
                if let Some(Block::Text { text: buf }) = self.block_mut(index) {
                    buf.push_str(&text);
                }
            }
            StreamEvent::ReasoningDelta { index, text } => {
                sink.on_reasoning(index, &text);
                if let Some(Block::Reasoning { text: buf }) = self.block_mut(index) {
                    buf.push_str(&text);
                }
            }
            StreamEvent::ToolCallDelta { index, json_delta } => {
                sink.on_tool_call_delta(index, &json_delta);
                if let Some(Block::ToolCall { arguments, .. }) = self.block_mut(index) {
                    arguments.push_str(&json_delta);
                }
                self.render_progress(index, sink, false).await;
            }
            StreamEvent::BlockStop { index } => {
                // Flush the final stage-2 frame before the block closes, so the
                // card shows the complete args view even if the throttle would
                // have held it back.
                self.render_progress(index, sink, true).await;
                sink.on_block_stop(index);
            }
            StreamEvent::Completed { stop_reason, usage } => {
                self.stop_reason = stop_reason;
                self.usage = usage;
            }
        }
    }

    /// Emit a stage-2 view snapshot for the tool call at `index`, if it has a
    /// presenter and the throttle allows (`force` bypasses the throttle — used
    /// for the final flush at `BlockStop`). No-op for non-tool blocks and for
    /// tools without a presenter (the common case).
    async fn render_progress(&mut self, index: u32, sink: &mut dyn StreamSink, force: bool) {
        let Some(Block::ToolCall {
            arguments,
            presenter,
            last_emit,
            last_emit_len,
            ..
        }) = self.block_mut(index)
        else {
            return;
        };
        let Some(presenter) = presenter.as_mut() else {
            return;
        };
        if !force {
            let grown = arguments.len().saturating_sub(*last_emit_len);
            let waited = last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_MIN_INTERVAL);
            if !(waited && grown >= PROGRESS_MIN_GROWTH) {
                return;
            }
        }
        if let Some(view) = presenter.render(arguments).await {
            sink.on_tool_call_progress(index, &view);
            *last_emit = Some(Instant::now());
            *last_emit_len = arguments.len();
        }
    }

    fn block_mut(&mut self, index: u32) -> Option<&mut Block> {
        self.blocks.get_mut(index as usize)
    }

    /// Open an empty block matching the streamed block type. A tool call also
    /// attaches its stage-2 presenter if the registry provides one.
    fn new_block(&self, block_type: ContentBlockType) -> Block {
        match block_type {
            ContentBlockType::Text => Block::Text {
                text: String::new(),
            },
            ContentBlockType::Reasoning => Block::Reasoning {
                text: String::new(),
            },
            ContentBlockType::ToolCall { id, name } => {
                let presenter = self.tools.and_then(|t| t.stream_presenter(&name));
                Block::ToolCall {
                    id,
                    name,
                    arguments: String::new(),
                    presenter,
                    last_emit: None,
                    last_emit_len: 0,
                }
            }
        }
    }

    /// Persist one consolidated `ContentBlock` per accumulated block, then
    /// assemble the assistant message and outcome.
    fn finish(self) -> Result<RoundOutcome, AgentError> {
        let Self {
            writer,
            source,
            request_id,
            turn_id,
            blocks,
            stop_reason,
            usage,
            ..
        } = self;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_call_event_ids = HashMap::new();
        let mut tool_call_block_index = HashMap::new();

        for (index, block) in blocks.into_iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let content = match block {
                // Reasoning is persisted for audit but not fed back to the
                // model, so it is recorded but not accumulated into the message.
                Block::Reasoning { text } => BlockContent::Reasoning { text },
                Block::Text { text: t } => {
                    text.push_str(&t);
                    BlockContent::Text { text: t }
                }
                Block::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    tool_calls.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    // Fill the one-line summary so the front-end header renders
                    // it verbatim instead of guessing from the JSON. An empty
                    // summary becomes `None` so the front-end falls back to a
                    // truncated args dump rather than showing a blank header.
                    let summary = crate::tool::summarize_by_name(
                        &name,
                        &serde_json::from_str(&arguments).unwrap_or_default(),
                    );
                    BlockContent::ToolCall {
                        id: id.clone(),
                        name,
                        arguments,
                        summary: if summary.is_empty() {
                            None
                        } else {
                            Some(summary)
                        },
                    }
                }
            };

            // Drop empty text/reasoning blocks: providers sometimes open a text
            // block then emit nothing (switching straight to reasoning or a tool
            // call). Such a block carries no information for replay, so skip it
            // rather than litter the log. Tool calls are always recorded.
            if is_empty_prose(&content) {
                continue;
            }

            let is_tool_call = matches!(content, BlockContent::ToolCall { .. });
            let call_id = if let BlockContent::ToolCall { id, .. } = &content {
                Some(id.clone())
            } else {
                None
            };

            let seq = writer.append(
                source.clone(),
                EventPayload::Model(ModelEvent::ContentBlock {
                    request_id: request_id.to_owned(),
                    index,
                    content,
                }),
                None,
                Some(turn_id.clone()),
            )?;

            if is_tool_call && let Some(id) = call_id {
                tool_call_block_index.insert(id.clone(), index);
                tool_call_event_ids.insert(
                    id,
                    EventId {
                        session_id: writer.session_id().clone(),
                        seq,
                    },
                );
            }
        }

        let message = Message::Assistant {
            content: (!text.is_empty()).then_some(text),
            tool_calls,
        };
        Ok(RoundOutcome {
            message,
            stop_reason,
            usage,
            tool_call_event_ids,
            tool_call_block_index,
        })
    }
}

/// Whether a block is empty prose (text or reasoning with no content). Empty
/// tool calls are never considered empty here — they are always meaningful.
const fn is_empty_prose(content: &BlockContent) -> bool {
    match content {
        BlockContent::Text { text } | BlockContent::Reasoning { text } => text.is_empty(),
        BlockContent::ToolCall { .. } => false,
    }
}

/// Map a streamed block type to the sink-facing [`BlockKind`].
fn block_kind(block_type: &ContentBlockType) -> BlockKind<'_> {
    match block_type {
        ContentBlockType::Text => BlockKind::Text,
        ContentBlockType::Reasoning => BlockKind::Reasoning,
        ContentBlockType::ToolCall { name, .. } => BlockKind::ToolCall { name },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::SourceKind;
    use crate::session::SessionStore;
    use futures_util::stream;

    /// Records the live calls a sink receives, so tests can assert that deltas
    /// are forwarded token-by-token (the streaming-transport half).
    #[derive(Default)]
    struct RecordingSink {
        text: String,
        reasoning: String,
        tool_args: String,
        block_starts: Vec<String>,
        /// Stage-2 view snapshots received, in order (`on_tool_call_progress`).
        progress: Vec<String>,
        ended: bool,
    }

    impl StreamSink for RecordingSink {
        fn on_block_start(&mut self, _index: u32, block: BlockKind<'_>) {
            self.block_starts.push(match block {
                BlockKind::Text => "text".to_owned(),
                BlockKind::Reasoning => "reasoning".to_owned(),
                BlockKind::ToolCall { name } => format!("tool:{name}"),
            });
        }
        fn on_text(&mut self, _index: u32, text: &str) {
            self.text.push_str(text);
        }
        fn on_reasoning(&mut self, _index: u32, text: &str) {
            self.reasoning.push_str(text);
        }
        fn on_tool_call_delta(&mut self, _index: u32, json_delta: &str) {
            self.tool_args.push_str(json_delta);
        }
        fn on_tool_call_progress(&mut self, _index: u32, view: &str) {
            self.progress.push(view.to_owned());
        }
        fn on_turn_end(&mut self) {
            self.ended = true;
        }
    }

    fn model_source() -> EventSource {
        EventSource {
            kind: SourceKind::Model,
            id: "test/model".to_owned(),
        }
    }

    fn ok_stream(events: Vec<StreamEvent>) -> EventStream {
        Box::pin(stream::iter(events.into_iter().map(Ok)))
    }

    /// A reasoning block then a text block, each streamed as several deltas,
    /// must persist as exactly two `ContentBlock` events (not one per delta),
    /// while the sink sees every delta live.
    #[tokio::test]
    async fn deltas_consolidate_into_one_block_each_and_forward_live() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let turn_id = TurnId("01TESTTURN".to_owned());

        let events = vec![
            // An empty text block the provider opened then abandoned: must not
            // be persisted.
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::Text,
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::BlockStart {
                index: 1,
                block_type: ContentBlockType::Reasoning,
            },
            StreamEvent::ReasoningDelta {
                index: 1,
                text: "let me ".to_owned(),
            },
            StreamEvent::ReasoningDelta {
                index: 1,
                text: "think".to_owned(),
            },
            StreamEvent::BlockStop { index: 1 },
            StreamEvent::BlockStart {
                index: 2,
                block_type: ContentBlockType::Text,
            },
            StreamEvent::TextDelta {
                index: 2,
                text: "Hel".to_owned(),
            },
            StreamEvent::TextDelta {
                index: 2,
                text: "lo".to_owned(),
            },
            StreamEvent::BlockStop { index: 2 },
            StreamEvent::Completed {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ];

        let mut sink = RecordingSink::default();
        let outcome = collect_round(
            &mut writer,
            &mut sink,
            ok_stream(events),
            &model_source(),
            "req_1",
            &turn_id,
            None,
        )
        .await
        .unwrap();
        drop(writer);

        // Message: text accumulated, reasoning excluded, no tool calls.
        assert_eq!(
            outcome.message,
            Message::Assistant {
                content: Some("Hello".to_owned()),
                tool_calls: vec![],
            }
        );

        // Live sink saw every delta and all block starts (including the empty
        // text block — live forwarding is unconditional), in order.
        assert_eq!(sink.reasoning, "let me think");
        assert_eq!(sink.text, "Hello");
        assert_eq!(sink.block_starts, vec!["text", "reasoning", "text"]);

        // Persistence: exactly two ContentBlock events — the empty text block is
        // dropped, each remaining block carries its fully-assembled content
        // (not one event per delta).
        let events = store.read_events(&sid).unwrap();
        let blocks: Vec<&BlockContent> = events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Model(ModelEvent::ContentBlock { content, .. }) => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(blocks.len(), 2, "one consolidated event per block");
        assert_eq!(
            blocks[0],
            &BlockContent::Reasoning {
                text: "let me think".to_owned()
            }
        );
        assert_eq!(
            blocks[1],
            &BlockContent::Text {
                text: "Hello".to_owned()
            }
        );
    }

    /// A tool call streamed as fragmented argument deltas consolidates into one
    /// `ContentBlock`, and its event id is returned for `tool_call_event_id`
    /// wiring.
    #[tokio::test]
    async fn tool_call_consolidates_and_reports_event_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let turn_id = TurnId("01TESTTURN".to_owned());

        let events = vec![
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::ToolCall {
                    id: "call_9".to_owned(),
                    name: "shell".to_owned(),
                },
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: "{\"cmd".to_owned(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: "\":\"ls\"}".to_owned(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Completed {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ];

        let mut sink = RecordingSink::default();
        let outcome = collect_round(
            &mut writer,
            &mut sink,
            ok_stream(events),
            &model_source(),
            "req_1",
            &turn_id,
            None,
        )
        .await
        .unwrap();
        drop(writer);

        // The assembled tool call carries the fully-joined arguments.
        let Message::Assistant { tool_calls, .. } = &outcome.message else {
            panic!("expected assistant message");
        };
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(sink.tool_args, "{\"cmd\":\"ls\"}");

        // The reported event id points at the persisted ContentBlock event.
        let event_id = outcome.tool_call_event_ids.get("call_9").unwrap();
        assert_eq!(event_id.session_id, sid);
        let events = store.read_events(&sid).unwrap();
        let target = events.iter().find(|e| e.seq == event_id.seq).unwrap();
        assert!(matches!(
            &target.payload,
            EventPayload::Model(ModelEvent::ContentBlock {
                content: BlockContent::ToolCall { id, .. },
                ..
            }) if id == "call_9"
        ));
    }

    /// End-to-end stage 2: a `write` call streaming its args drives
    /// `on_tool_call_progress` with render-ready view snapshots (the same
    /// envelope as the settled view), and the final frame flushes at
    /// `BlockStop` even though the throttle's growth threshold isn't met
    /// (`doc/tool-streaming.md`). A registered `shell` call in the same round
    /// emits NOTHING — most tools have no presenter.
    #[tokio::test]
    async fn write_call_streams_progress_snapshots_and_flushes_at_block_stop() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let turn_id = TurnId("01TESTTURN".to_owned());

        // Register the real `write` tool rooted at a temp workspace.
        let ws = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(crate::tool::WriteTool::new(
            ws.path().to_path_buf(),
        )));

        // One big args delta (over the growth threshold) then a tiny one (under
        // it). The tiny one only reaches the sink via the BlockStop flush.
        let head = format!("{{\"path\": \"n.txt\", \"content\": \"{}", "x".repeat(200));
        let events = vec![
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::ToolCall {
                    id: "c1".to_owned(),
                    name: "write".to_owned(),
                },
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: head,
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: "tail\"}".to_owned(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Completed {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ];

        let mut sink = RecordingSink::default();
        collect_round(
            &mut writer,
            &mut sink,
            ok_stream(events),
            &model_source(),
            "req_1",
            &turn_id,
            Some(&registry),
        )
        .await
        .unwrap();
        drop(writer);

        // At least the big frame and the forced final flush arrived; each is a
        // self-contained code-envelope snapshot for a NEW file.
        assert!(
            sink.progress.len() >= 2,
            "expected the throttled frame + the BlockStop flush, got {:?}",
            sink.progress
        );
        let last: serde_json::Value =
            serde_json::from_str(sink.progress.last().unwrap()).unwrap();
        assert_eq!(last["kind"], "code");
        assert_eq!(last["path"], "n.txt");
        assert!(last["content"].as_str().unwrap().ends_with("tail"));
    }

    /// End-to-end stage 2 for `edit`: streaming an edit's args drives
    /// `on_tool_call_progress` with a diff-envelope snapshot of the active
    /// entry, rendered against the real pre-edit file (`doc/tool-streaming.md`).
    #[tokio::test]
    async fn edit_call_streams_progress_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let turn_id = TurnId("01TESTTURN".to_owned());

        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(crate::tool::EditTool::new(
            ws.path().to_path_buf(),
        )));

        // Args long enough to clear the growth threshold; one delta suffices.
        let args = format!(
            "{{\"edits\": [{{\"path\":\"f.txt\",\"old\":[\"b\"],\"new\":[\"{}\"]}}]}}",
            "B".repeat(200)
        );
        let events = vec![
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::ToolCall {
                    id: "c1".to_owned(),
                    name: "edit".to_owned(),
                },
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: args,
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Completed {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ];

        let mut sink = RecordingSink::default();
        collect_round(
            &mut writer,
            &mut sink,
            ok_stream(events),
            &model_source(),
            "req_1",
            &turn_id,
            Some(&registry),
        )
        .await
        .unwrap();
        drop(writer);

        assert!(!sink.progress.is_empty(), "edit streams a diff snapshot");
        let last: serde_json::Value =
            serde_json::from_str(sink.progress.last().unwrap()).unwrap();
        assert_eq!(last["kind"], "diff");
        assert_eq!(last["files"][0]["path"], "f.txt");
        let patch = last["files"][0]["patch"].as_str().unwrap();
        assert!(patch.contains("-b"), "old shown as removed: {patch}");
    }
}
