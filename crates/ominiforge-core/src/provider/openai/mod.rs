//! OpenAI-compatible Chat Completions provider.
//!
//! Holds connection config and implements [`Provider`] by sending a streaming
//! POST request and mapping the SSE byte stream through the pure [`wire`]
//! decoder.
//! "OpenAI-compatible" covers any endpoint speaking the Chat Completions API
//! (DeepSeek, local servers, Xiaomi MiMo via an OpenAI-shaped gateway, ...).

mod wire;

use futures_util::{StreamExt, stream};

use crate::llm::{EventStream, LlmError, ModelRequest, Provider, StreamEvent};
use wire::{ChatChunk, ChatRequest, ChunkAssembler, SseDecoder, SsePayload};

/// A provider backed by an OpenAI-compatible Chat Completions endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    /// Source identifier reported on emitted events (e.g. `"openai"`).
    name: String,
}

impl OpenAiProvider {
    /// Build a provider. `base_url` is the API root (e.g.
    /// `https://api.openai.com/v1`); the trailing `/chat/completions` is added
    /// per request.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            name: name.into(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn stream(&self, request: ModelRequest) -> Result<EventStream, LlmError> {
        let body = ChatRequest::from_request(request);
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 => LlmError::Auth(body),
                code => LlmError::Status { status: code, body },
            });
        }

        Ok(decode_response(response.bytes_stream()))
    }
}

/// State threaded through the decoding [`stream::unfold`].
struct DecodeState<S> {
    bytes: S,
    decoder: SseDecoder,
    assembler: ChunkAssembler,
    /// Decoded events not yet yielded (one byte chunk can produce several).
    pending: std::collections::VecDeque<Result<StreamEvent, LlmError>>,
    /// Set once `[DONE]` or end-of-stream has flushed the assembler.
    done: bool,
}

/// Turn a stream of HTTP byte chunks into a stream of [`StreamEvent`]s.
///
/// Generic over the byte stream so it can be exercised without a real socket.
fn decode_response<S>(bytes: S) -> EventStream
where
    S: stream::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + Unpin + 'static,
{
    let state = DecodeState {
        bytes,
        decoder: SseDecoder::default(),
        assembler: ChunkAssembler::default(),
        pending: std::collections::VecDeque::new(),
        done: false,
    };

    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Some((event, state));
            }
            if state.done {
                return None;
            }

            match state.bytes.next().await {
                Some(Ok(chunk)) => feed_chunk(&mut state, &chunk),
                Some(Err(e)) => {
                    state.done = true;
                    state
                        .pending
                        .push_back(Err(LlmError::Transport(e.to_string())));
                }
                None => flush(&mut state),
            }
        }
    })
    .boxed()
}

/// Feed one HTTP byte chunk: decode SSE payloads, assemble events, enqueue.
fn feed_chunk<S>(state: &mut DecodeState<S>, chunk: &[u8]) {
    for payload in state.decoder.feed(chunk) {
        match payload {
            SsePayload::Data(json) => match serde_json::from_str::<ChatChunk>(&json) {
                Ok(parsed) => {
                    for event in state.assembler.accept(parsed) {
                        state.pending.push_back(Ok(event));
                    }
                }
                Err(e) => state
                    .pending
                    .push_back(Err(LlmError::Decode(e.to_string()))),
            },
            SsePayload::Done => {
                flush(state);
                return;
            }
        }
    }
}

/// Close the assembler and enqueue its terminal events exactly once.
fn flush<S>(state: &mut DecodeState<S>) {
    if state.done {
        return;
    }
    state.done = true;
    for event in state.assembler.finish() {
        state.pending.push_back(Ok(event));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::payload::{ContentBlockType, StopReason};

    /// Drive the full byte-stream → event-stream path with synthetic SSE bytes
    /// (no network), exercising the same `unfold` glue the HTTP path uses.
    #[tokio::test]
    async fn decodes_synthetic_sse_byte_stream() {
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        );
        // Split mid-payload to prove buffering across chunk boundaries.
        let (a, b) = raw.split_at(20);
        let chunks = vec![
            Ok(bytes::Bytes::from(a.to_owned())),
            Ok(bytes::Bytes::from(b.to_owned())),
        ];
        let byte_stream = stream::iter(chunks);

        let events: Vec<_> = decode_response(byte_stream)
            .map(Result::unwrap)
            .collect()
            .await;

        assert_eq!(
            events.first(),
            Some(&StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::Text,
            })
        );
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Completed {
                stop_reason: StopReason::EndTurn,
                ..
            })
        ));
    }
}
