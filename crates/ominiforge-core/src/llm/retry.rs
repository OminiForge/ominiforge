//! Retry wrapper for model requests: transparently re-sends a request when the
//! provider fails with a transient error, before the error reaches the agent
//! loop and aborts the turn.
//!
//! The classic cases are network hiccups (connection reset, TLS timeout) and a
//! busy inference engine — e.g. Kimi answering
//! `429 {"type":"engine_overloaded_error"}` — both of which used to surface as
//! a hard `model error: provider returned status 429: …` and kill the turn
//! even though simply asking again a moment later would have succeeded.
//!
//! Retries are only safe *before any content has streamed*: a chunk-level
//! failure mid-stream (`Err` items inside the returned [`EventStream`]) cannot
//! be replayed without duplicating or corrupting the assistant message, so
//! those still propagate and end the round. What is wrapped is
//! [`Provider::stream`] itself — connection setup and the initial HTTP
//! handshake — which is exactly where transient transport errors and
//! `429`/`5xx` statuses arrive.

use std::sync::Arc;
use std::time::Duration;

use crate::llm::{EventStream, LlmError, ModelRequest, Provider};

/// Retry policy for [`RetryingProvider`].
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// How many times to re-send the request after the first failure.
    pub max_retries: u32,
    /// Delay before the first retry; doubles each subsequent attempt, capped
    /// at [`Self::max_delay`].
    pub initial_delay: Duration,
    /// Upper bound on the exponential backoff.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    /// Three retries at 1s → 2s → 4s: covers a momentarily overloaded engine
    /// without stalling the turn for long.
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryConfig {
    /// The delay before attempt number `attempt` (1-based: the first retry).
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        self.initial_delay
            .saturating_mul(1u32 << attempt.saturating_sub(1).min(10))
            .min(self.max_delay)
    }
}

/// Whether an [`LlmError`] is worth retrying.
///
/// Transient transport faults and rate-limit / server-overload statuses (429,
/// 5xx — including Kimi's `engine_overloaded_error`) may succeed on re-send;
/// auth failures, bad requests (4xx), and decode faults are deterministic —
/// asking again changes nothing.
#[must_use]
pub fn is_retryable(err: &LlmError) -> bool {
    match err {
        LlmError::Transport(_) => true,
        LlmError::Status { status, .. } => *status == 429 || (500..600).contains(status),
        LlmError::Decode(_) | LlmError::Auth(_) => false,
    }
}

/// Called before each retry: the 1-based attempt number, the error being
/// retried, and the backoff delay about to elapse.
pub type OnRetry = Arc<dyn Fn(u32, &LlmError, Duration) + Send + Sync>;

/// A [`Provider`] decorator that retries `stream()` on transient failures.
///
/// Reports each retry through the `on_retry` callback so a front-end can tell
/// the user the request is being re-sent rather than sitting silent.
pub struct RetryingProvider {
    inner: Arc<dyn Provider>,
    config: RetryConfig,
    on_retry: OnRetry,
}

impl RetryingProvider {
    /// Wrap `inner` with the retry policy. `on_retry` receives the 1-based
    /// attempt number, the error being retried, and the backoff delay.
    #[must_use]
    pub fn new(inner: Arc<dyn Provider>, config: RetryConfig, on_retry: OnRetry) -> Self {
        Self {
            inner,
            config,
            on_retry,
        }
    }

    /// Wrap `inner` with the default policy and no reporting (the caller
    /// observes retries only through the eventual outcome).
    #[must_use]
    pub fn with_defaults(inner: Arc<dyn Provider>) -> Self {
        Self::new(inner, RetryConfig::default(), Arc::new(|_, _, _| {}))
    }
}

#[async_trait::async_trait]
impl Provider for RetryingProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn stream(&self, request: ModelRequest) -> Result<EventStream, LlmError> {
        let mut attempt = 0u32;
        loop {
            match self.inner.stream(request.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(err) if attempt < self.config.max_retries && is_retryable(&err) => {
                    attempt += 1;
                    let delay = self.config.delay_for(attempt);
                    (self.on_retry)(attempt, &err, delay);
                    tokio::time::sleep(delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::payload::{ContentBlockType, StopReason, Usage};
    use crate::llm::StreamEvent;
    use futures_util::{StreamExt, stream};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider that fails once per queued error, then streams one text
    /// batch — counting calls so tests can assert how many requests went out.
    struct FlakyProvider {
        calls: Arc<AtomicUsize>,
        failures: Mutex<std::collections::VecDeque<LlmError>>,
    }

    impl FlakyProvider {
        fn failing_with(errors: Vec<LlmError>) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                failures: Mutex::new(errors.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        #[allow(clippy::unnecessary_literal_bound)] // trait dictates `-> &str`
        fn name(&self) -> &str {
            "flaky"
        }

        async fn stream(&self, _request: ModelRequest) -> Result<EventStream, LlmError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let failure = self.failures.lock().unwrap().pop_front();
            if let Some(err) = failure {
                return Err(err);
            }
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamEvent::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                }),
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: "recovered".to_owned(),
                }),
                Ok(StreamEvent::Completed {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            ])))
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: "mock".to_owned(),
            messages: vec![],
            tools: vec![],
            temperature: 0.0,
            max_tokens: None,
            think_effort: None,
        }
    }

    fn config(max_retries: u32) -> RetryConfig {
        RetryConfig {
            max_retries,
            // Zero-delay backoff keeps the retry tests instantaneous.
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    fn wrap(provider: Arc<FlakyProvider>, max_retries: u32) -> RetryingProvider {
        RetryingProvider::new(provider, config(max_retries), Arc::new(|_, _, _| {}))
    }

    /// Kimi's overload response — `429 engine_overloaded_error` — must not
    /// kill the request: the wrapper re-sends until the engine answers.
    #[tokio::test]
    async fn engine_overload_429_is_retried_until_success() {
        let flaky = Arc::new(FlakyProvider::failing_with(vec![
            LlmError::Status {
                status: 429,
                body: r#"{"error":{"message":"The engine is currently overloaded, please try again later","type":"engine_overloaded_error"}}"#.to_owned(),
            },
        ]));
        let calls = Arc::clone(&flaky.calls);
        let provider = wrap(flaky, 3);

        let mut stream = provider.stream(request()).await.unwrap();
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::TextDelta { text: t, .. } = event.unwrap() {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "recovered");
        assert_eq!(calls.load(Ordering::Relaxed), 2, "one failure, one retry");
    }

    /// A dropped connection (transport error) is transient: retried, then the
    /// recovered stream is returned.
    #[tokio::test]
    async fn transport_error_is_retried() {
        let flaky = Arc::new(FlakyProvider::failing_with(vec![
            LlmError::Transport("connection reset by peer".to_owned()),
            LlmError::Transport("dns lookup failed".to_owned()),
        ]));
        let calls = Arc::clone(&flaky.calls);
        let provider = wrap(flaky, 3);

        let mut stream = provider.stream(request()).await.unwrap();
        while stream.next().await.is_some() {}
        assert_eq!(
            calls.load(Ordering::Relaxed),
            3,
            "two failures, two retries"
        );
    }

    /// The retry budget is finite: an engine that stays overloaded eventually
    /// surfaces the last error instead of looping forever.
    #[tokio::test]
    async fn gives_up_after_max_retries_and_returns_last_error() {
        let flaky = Arc::new(FlakyProvider::failing_with(vec![
            LlmError::Status {
                status: 503,
                body: "a".to_owned(),
            },
            LlmError::Status {
                status: 503,
                body: "b".to_owned(),
            },
            LlmError::Status {
                status: 503,
                body: "last".to_owned(),
            },
        ]));
        let calls = Arc::clone(&flaky.calls);
        let provider = wrap(flaky, 2);

        let result = provider.stream(request()).await;
        let Err(err) = result else {
            panic!("the final attempt's error should propagate")
        };
        assert!(
            matches!(err, LlmError::Status { status: 503, ref body } if body == "last"),
            "the final attempt's error propagates: {err}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 3, "initial + 2 retries");
    }

    /// Non-retryable failures pass through on the first attempt: retrying an
    /// auth rejection or a bad request only wastes rate-limit budget.
    #[tokio::test]
    async fn auth_and_client_errors_are_not_retried() {
        for err in [
            LlmError::Auth("bad key".to_owned()),
            LlmError::Status {
                status: 400,
                body: "malformed".to_owned(),
            },
        ] {
            let flaky = Arc::new(FlakyProvider::failing_with(vec![err]));
            let calls = Arc::clone(&flaky.calls);
            let provider = wrap(flaky, 3);
            let result = provider.stream(request()).await;
            assert!(result.is_err(), "non-retryable error must propagate");
            assert_eq!(calls.load(Ordering::Relaxed), 1, "no retry");
        }
    }

    /// The `on_retry` callback observes every retry with its attempt number,
    /// so a front-end can tell the user the request is being re-sent.
    #[tokio::test]
    async fn retries_are_reported_to_the_callback() {
        let flaky = Arc::new(FlakyProvider::failing_with(vec![
            LlmError::Transport("reset".to_owned()),
            LlmError::Status {
                status: 429,
                body: "overloaded".to_owned(),
            },
        ]));
        let reported = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&reported);
        let provider = RetryingProvider::new(
            flaky,
            config(3),
            Arc::new(move |attempt, err, _delay| {
                sink.lock().unwrap().push((attempt, err.to_string()));
            }),
        );

        let mut stream = provider.stream(request()).await.unwrap();
        while stream.next().await.is_some() {}
        let reported = reported.lock().unwrap().clone();
        assert_eq!(reported.len(), 2);
        assert_eq!(reported[0].0, 1);
        assert_eq!(reported[1].0, 2);
        assert!(reported[1].1.contains("429"));
    }

    /// Backoff doubles per attempt and is capped: 1s, 2s, 4s, …, max 30s.
    #[test]
    fn backoff_doubles_and_is_capped() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.delay_for(1), Duration::from_secs(1));
        assert_eq!(cfg.delay_for(2), Duration::from_secs(2));
        assert_eq!(cfg.delay_for(3), Duration::from_secs(4));
        assert_eq!(cfg.delay_for(10), Duration::from_secs(30));
    }

    /// `is_retryable` mirrors `error_detail`'s classification so the retry
    /// wrapper and the terminal-trace detail agree on what is transient.
    #[test]
    fn retryable_classification() {
        assert!(is_retryable(&LlmError::Transport("x".to_owned())));
        assert!(is_retryable(&LlmError::Status {
            status: 429,
            body: String::new()
        }));
        assert!(is_retryable(&LlmError::Status {
            status: 500,
            body: String::new()
        }));
        assert!(is_retryable(&LlmError::Status {
            status: 503,
            body: String::new()
        }));
        assert!(!is_retryable(&LlmError::Status {
            status: 400,
            body: String::new()
        }));
        assert!(!is_retryable(&LlmError::Status {
            status: 404,
            body: String::new()
        }));
        assert!(!is_retryable(&LlmError::Auth("x".to_owned())));
        assert!(!is_retryable(&LlmError::Decode("x".to_owned())));
    }
}
