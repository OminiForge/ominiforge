//! OpenAI Chat Completions wire format — request encoding and streaming
//! response decoding.
//!
//! This module is pure (no I/O): it converts a [`ModelRequest`] into the
//! request body, splits an SSE byte stream into payloads, and assembles
//! streaming deltas into provider-neutral [`StreamEvent`]s. Keeping it free of
//! HTTP makes the decode logic — the fiddly part — unit-testable. The network
//! glue lives in the parent module.

use serde::{Deserialize, Serialize};

use crate::core::payload::{ContentBlockType, StopReason, Usage};
use crate::llm::{Message, ModelRequest, StreamEvent};

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

/// The Chat Completions request body.
#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool>,
    pub temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub stream_options: StreamOptions,
}

/// Ask the API to emit a final usage chunk in streaming mode.
#[derive(Debug, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: ChatFunctionCall,
}

#[derive(Debug, Serialize)]
pub struct ChatFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: ChatFunctionDef,
}

#[derive(Debug, Serialize)]
pub struct ChatFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ChatRequest {
    /// Build a streaming request body from a neutral [`ModelRequest`].
    #[must_use]
    pub fn from_request(req: ModelRequest) -> Self {
        let messages = req.messages.into_iter().map(ChatMessage::from).collect();
        let tools = req
            .tools
            .into_iter()
            .map(|t| ChatTool {
                kind: "function",
                function: ChatFunctionDef {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                },
            })
            .collect();
        Self {
            model: req.model,
            messages,
            tools,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        }
    }
}

impl From<Message> for ChatMessage {
    fn from(msg: Message) -> Self {
        match msg {
            Message::System { content } => Self {
                role: "system",
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message::User { content } => Self {
                role: "user",
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
            Message::Assistant {
                content,
                tool_calls,
            } => Self {
                role: "assistant",
                content,
                tool_calls: tool_calls
                    .into_iter()
                    .map(|c| ChatToolCall {
                        id: c.id,
                        kind: "function",
                        function: ChatFunctionCall {
                            name: c.name,
                            arguments: c.arguments,
                        },
                    })
                    .collect(),
                tool_call_id: None,
            },
            Message::Tool {
                tool_call_id,
                content,
            } => Self {
                role: "tool",
                content: Some(content),
                tool_calls: Vec::new(),
                tool_call_id: Some(tool_call_id),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// SSE decoding
// ---------------------------------------------------------------------------

/// Splits a Server-Sent-Events byte stream into `data:` payloads.
///
/// Feed it raw byte chunks as they arrive; it buffers across chunk boundaries
/// (which may split mid-line or mid-UTF-8) and returns complete payload strings.
/// The terminal `data: [DONE]` sentinel is reported as
/// [`SsePayload::Done`]; JSON payloads come back as [`SsePayload::Data`].
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

/// A payload extracted from the SSE stream.
#[derive(Debug, PartialEq, Eq)]
pub enum SsePayload {
    /// A `data:` line carrying a JSON chunk.
    Data(String),
    /// The `data: [DONE]` end-of-stream sentinel.
    Done,
}

impl SseDecoder {
    /// Append bytes and extract every complete payload now available.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SsePayload> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();

        // SSE events are newline-delimited; OpenAI sends one `data:` line per
        // event followed by a blank line. We process complete lines only.
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();

            let Some(payload) = line.strip_prefix("data:") else {
                continue; // blank lines, comments, or `event:` lines
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                out.push(SsePayload::Done);
            } else if !payload.is_empty() {
                out.push(SsePayload::Data(payload.to_owned()));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Response chunk DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<ChunkUsage>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkChoice {
    pub delta: ChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// DeepSeek-style separate reasoning channel.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ChunkToolCall>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkToolCall {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkFunction>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

// ---------------------------------------------------------------------------
// Chunk assembly
// ---------------------------------------------------------------------------

/// Assembles OpenAI streaming deltas into provider-neutral [`StreamEvent`]s.
///
/// OpenAI streams flat deltas; the neutral protocol is block-structured (each
/// content region opens and closes). The assembler opens a block lazily on the
/// first delta for text, reasoning, or each tool call, assigning block indices
/// in open order, and closes them all on [`finish`](Self::finish). The final
/// `finish_reason` and `usage` arrive in late chunks, so `Completed` is emitted
/// only at `finish`.
#[derive(Debug, Default)]
pub struct ChunkAssembler {
    next_index: u32,
    text_index: Option<u32>,
    reasoning_index: Option<u32>,
    /// (`openai_tool_index`, `block_index`) in open order.
    tool_blocks: Vec<(u32, u32)>,
    stop_reason: Option<StopReason>,
    usage: Usage,
    finished: bool,
}

impl ChunkAssembler {
    /// Process one decoded chunk, emitting any newly-complete stream events.
    pub fn accept(&mut self, chunk: ChatChunk) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        for choice in chunk.choices {
            if let Some(text) = choice.delta.reasoning_content {
                self.push_reasoning(&text, &mut out);
            }
            if let Some(text) = choice.delta.content {
                self.push_text(&text, &mut out);
            }
            for call in choice.delta.tool_calls {
                self.push_tool_call(call, &mut out);
            }
            if let Some(reason) = choice.finish_reason {
                self.stop_reason = Some(map_stop_reason(&reason));
            }
        }
        if let Some(usage) = chunk.usage {
            self.usage = Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_tokens: usage.prompt_tokens_details.map_or(0, |d| d.cached_tokens),
                cache_write_tokens: 0,
            };
        }
        out
    }

    /// Close every open block and emit the terminal `Completed`. Idempotent.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        let mut out = Vec::new();
        // Blocks were assigned indices 0..next_index in open order; close in
        // the same order.
        for index in 0..self.next_index {
            out.push(StreamEvent::BlockStop { index });
        }
        out.push(StreamEvent::Completed {
            stop_reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
        });
        out
    }

    fn push_text(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        let index = Self::ensure_block(
            &mut self.text_index,
            &mut self.next_index,
            ContentBlockType::Text,
            out,
        );
        out.push(StreamEvent::TextDelta {
            index,
            text: text.to_owned(),
        });
    }

    fn push_reasoning(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        let index = Self::ensure_block(
            &mut self.reasoning_index,
            &mut self.next_index,
            ContentBlockType::Reasoning,
            out,
        );
        out.push(StreamEvent::ReasoningDelta {
            index,
            text: text.to_owned(),
        });
    }

    fn push_tool_call(&mut self, call: ChunkToolCall, out: &mut Vec<StreamEvent>) {
        let existing = self
            .tool_blocks
            .iter()
            .find_map(|&(oai, blk)| (oai == call.index).then_some(blk));

        let Some(block_index) = existing else {
            // First delta for this tool call: it carries id + name to open with.
            let id = call.id.unwrap_or_default();
            let name = call
                .function
                .as_ref()
                .and_then(|f| f.name.clone())
                .unwrap_or_default();
            let index = self.open_block(ContentBlockType::ToolCall { id, name }, out);
            self.tool_blocks.push((call.index, index));
            // Arguments may also be present in this same opening chunk.
            if let Some(args) = call.function.and_then(|f| f.arguments)
                && !args.is_empty()
            {
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    json_delta: args,
                });
            }
            return;
        };

        if let Some(args) = call.function.and_then(|f| f.arguments)
            && !args.is_empty()
        {
            out.push(StreamEvent::ToolCallDelta {
                index: block_index,
                json_delta: args,
            });
        }
    }

    /// Return the block index in `slot`, opening a new block (and recording its
    /// index) the first time. Shared by the single-block text and reasoning
    /// channels.
    fn ensure_block(
        slot: &mut Option<u32>,
        next_index: &mut u32,
        block_type: ContentBlockType,
        out: &mut Vec<StreamEvent>,
    ) -> u32 {
        if let Some(i) = *slot {
            return i;
        }
        let index = *next_index;
        *next_index += 1;
        out.push(StreamEvent::BlockStart { index, block_type });
        *slot = Some(index);
        index
    }

    fn open_block(&mut self, block_type: ContentBlockType, out: &mut Vec<StreamEvent>) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        out.push(StreamEvent::BlockStart { index, block_type });
        index
    }
}

/// Map an OpenAI `finish_reason` to the neutral [`StopReason`].
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        // "stop", "content_filter", and anything unrecognized end the turn.
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::llm::{ToolCall, ToolSchema};

    fn chunk(json: &str) -> ChatChunk {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn request_encodes_messages_tools_and_stream_options() {
        let req = ModelRequest {
            model: "gpt-4o".to_owned(),
            messages: vec![
                Message::System {
                    content: "be helpful".to_owned(),
                },
                Message::User {
                    content: "hi".to_owned(),
                },
                Message::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_owned(),
                        name: "shell".to_owned(),
                        arguments: "{}".to_owned(),
                    }],
                },
                Message::Tool {
                    tool_call_id: "call_1".to_owned(),
                    content: "ok".to_owned(),
                },
            ],
            tools: vec![ToolSchema {
                name: "shell".to_owned(),
                description: "run a command".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            temperature: 0.0,
            max_tokens: Some(256),
        };

        let body = ChatRequest::from_request(req);
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["stream"], true);
        assert_eq!(value["stream_options"]["include_usage"], true);
        assert_eq!(value["messages"][2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(value["messages"][3]["role"], "tool");
        assert_eq!(value["tools"][0]["function"]["name"], "shell");
    }

    #[test]
    fn sse_decoder_buffers_across_split_chunks() {
        let mut dec = SseDecoder::default();
        assert!(dec.feed(b"data: {\"a\":").is_empty());
        let out = dec.feed(b"1}\n\n");
        assert_eq!(out, vec![SsePayload::Data("{\"a\":1}".to_owned())]);
        assert_eq!(dec.feed(b"data: [DONE]\n\n"), vec![SsePayload::Done]);
    }

    #[test]
    fn assembles_text_stream_into_block_and_completed() {
        let mut asm = ChunkAssembler::default();
        let mut events = Vec::new();
        events.extend(asm.accept(chunk(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#)));
        events.extend(asm.accept(chunk(r#"{"choices":[{"delta":{"content":"lo"}}]}"#)));
        events.extend(asm.accept(chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        )));
        events.extend(asm.accept(chunk(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
        )));
        events.extend(asm.finish());

        assert_eq!(
            events,
            vec![
                StreamEvent::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "Hel".to_owned()
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "lo".to_owned()
                },
                StreamEvent::BlockStop { index: 0 },
                StreamEvent::Completed {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 2,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                },
            ]
        );
    }

    #[test]
    fn assembles_tool_call_across_chunks() {
        let mut asm = ChunkAssembler::default();
        let mut events = Vec::new();
        events.extend(asm.accept(chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"shell","arguments":"{\"cmd"}}]}}]}"#,
        )));
        events.extend(asm.accept(chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":\"ls\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        )));
        events.extend(asm.finish());

        assert_eq!(
            events,
            vec![
                StreamEvent::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::ToolCall {
                        id: "call_9".to_owned(),
                        name: "shell".to_owned(),
                    },
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    json_delta: "{\"cmd".to_owned()
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    json_delta: "\":\"ls\"}".to_owned()
                },
                StreamEvent::BlockStop { index: 0 },
                StreamEvent::Completed {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                },
            ]
        );
    }

    #[test]
    fn cached_tokens_map_to_cache_read() {
        let mut asm = ChunkAssembler::default();
        asm.accept(chunk(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":80}}}"#,
        ));
        let events = asm.finish();
        let StreamEvent::Completed { usage, .. } = events.last().unwrap() else {
            panic!("expected Completed last");
        };
        assert_eq!(usage.cache_read_tokens, 80);
        assert_eq!(usage.input_tokens, 100);
    }

    #[test]
    fn finish_is_idempotent() {
        let mut asm = ChunkAssembler::default();
        assert!(!asm.finish().is_empty());
        assert!(asm.finish().is_empty());
    }
}
