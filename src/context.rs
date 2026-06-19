//! Context-window token accounting (Phase 2, Step 2).
//!
//! Compaction, dynamic injection, and prefix-cache discipline follow in later
//! steps. See `doc/context-management.md` and `doc/phase2-plan.md` Step 2
//! (decision A).
//!
//! ## Why a ledger and not a single counter
//!
//! The authoritative size of the prefix sent to the model is the provider's
//! `usage.input_tokens` on `RequestCompleted` — but it only arrives *after* the
//! request, some OpenAI-compatible endpoints omit it entirely, and the
//! conversation keeps growing (assistant reply, tool results, injected
//! reminders) before the next request measures it again. So the running count is
//! split in two:
//!
//! - `measured` — the last real `input_tokens`: an exact count of the prefix as
//!   of the most recent request.
//! - `pending_bytes` — raw bytes of everything appended *since* that request,
//!   converted to tokens with a provider-neutral `bytes / 4` heuristic.
//!
//! `running() = measured + pending_bytes / 4`. Each request that returns usage
//! recalibrates `measured` and resets the tail to zero, so the heuristic only
//! ever covers the un-measured tail and self-corrects every round. A provider
//! that returns no usage never recalibrates: `measured` stays `0` and the whole
//! context is estimated by the heuristic — degraded but always available.

use crate::llm::Message;

/// Default fraction of the context window we aim to stay under before
/// compacting (`doc/context-management.md` §4.2). A profile's
/// `[context].compaction_threshold` overrides it.
pub const DEFAULT_COMPACTION_THRESHOLD: f32 = 0.8;

/// Heuristic bytes-per-token ratio for the local estimator. Crude but
/// provider-neutral and zero-dependency; only ever applied to the un-measured
/// tail (see module docs).
const BYTES_PER_TOKEN: usize = 4;

/// Running estimate of how many input tokens the conversation view occupies.
///
/// Combines the provider's authoritative count with a local heuristic for the
/// un-measured tail. See the module docs for the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextLedger {
    /// Last authoritative `input_tokens` (exact prefix size at that request).
    measured: u32,
    /// Bytes appended since the last calibration, estimated via [`BYTES_PER_TOKEN`].
    pending_bytes: usize,
}

impl ContextLedger {
    /// A ledger seeded from an initial context (e.g. the system message). No
    /// authoritative count exists yet, so the whole seed lands in the heuristic
    /// tail until the first request calibrates it.
    #[must_use]
    pub fn seeded(context: &[Message]) -> Self {
        let mut ledger = Self::default();
        for message in context {
            ledger.record_message(message);
        }
        ledger
    }

    /// Account for a message just appended to the context view.
    pub fn record_message(&mut self, message: &Message) {
        self.pending_bytes = self.pending_bytes.saturating_add(message_bytes(message));
    }

    /// Fold in the provider's authoritative `input_tokens` for the request that
    /// was just sent. A non-zero value is exact for the prefix at send time, so
    /// it replaces `measured` and clears the heuristic tail (nothing has been
    /// appended yet — the reply and tool results come after). A zero value means
    /// the provider returned no usage: keep the prior `measured` and let the tail
    /// keep growing (decision A).
    pub const fn calibrate(&mut self, input_tokens: u32) {
        if input_tokens > 0 {
            self.measured = input_tokens;
            self.pending_bytes = 0;
        }
    }

    /// Current best estimate of the prefix size in tokens.
    #[must_use]
    pub fn running(&self) -> u32 {
        self.measured.saturating_add(bytes_to_tokens(self.pending_bytes))
    }
}

/// The token budget a turn must stay under, leaving room for the model's reply.
///
/// `threshold × context_window − max_output_tokens` (`doc/context-management.md`
/// §4.2). Returns `None` when the context window is unknown (`0`), so callers
/// skip threshold logic rather than treat everything as over-limit.
#[must_use]
pub fn effective_limit(
    context_window: u32,
    threshold: f32,
    max_output_tokens: Option<u32>,
) -> Option<u32> {
    if context_window == 0 {
        return None;
    }
    let budget = clamp_to_u32(f64::from(context_window) * f64::from(threshold));
    Some(budget.saturating_sub(max_output_tokens.unwrap_or(0)))
}

/// Estimate the token count of an arbitrary string (the public heuristic, used
/// for injection bookkeeping). Tokens, not bytes.
#[must_use]
pub fn estimate_tokens(text: &str) -> u32 {
    bytes_to_tokens(text.len())
}

/// Byte footprint of a message for the heuristic tail. Counts every part the
/// wire format will carry: text content plus, for an assistant turn, each tool
/// call's name and argument JSON.
pub(crate) fn message_bytes(message: &Message) -> usize {
    match message {
        Message::System { content }
        | Message::User { content }
        | Message::Tool { content, .. } => content.len(),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let text = content.as_ref().map_or(0, String::len);
            let calls: usize = tool_calls
                .iter()
                .map(|c| c.name.len() + c.arguments.len())
                .sum();
            text + calls
        }
    }
}

/// Convert a byte count to an estimated token count (saturating).
fn bytes_to_tokens(bytes: usize) -> u32 {
    u32::try_from(bytes / BYTES_PER_TOKEN).unwrap_or(u32::MAX)
}

/// Clamp a non-negative float to `u32`. The callers feed `window × fraction`,
/// always finite and `>= 0`; the bounds check makes the cast lossless-or-clamped
/// rather than wrapping.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn clamp_to_u32(value: f64) -> u32 {
    if value <= 0.0 {
        0
    } else if value >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        value.round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;

    /// A non-zero `input_tokens` becomes the exact prefix size and clears the
    /// heuristic tail; content appended after is estimated on top of it. This is
    /// the core "trust the provider, estimate only the tail" behaviour.
    #[test]
    fn calibration_replaces_heuristic_then_estimates_the_tail() {
        let mut ledger = ContextLedger::default();
        // 40 bytes of pre-request context → heuristic 10 tokens, no real count yet.
        ledger.record_message(&Message::User {
            content: "a".repeat(40),
        });
        assert_eq!(ledger.running(), 10);

        // Provider reports the prefix was really 1000 tokens: tail resets to 0.
        ledger.calibrate(1000);
        assert_eq!(ledger.running(), 1000);

        // A 20-byte reply appended after → 1000 + 5 estimated tail tokens.
        ledger.record_message(&Message::Assistant {
            content: Some("b".repeat(20)),
            tool_calls: vec![],
        });
        assert_eq!(ledger.running(), 1005);
    }

    /// A provider that returns no usage (`input_tokens == 0`) never recalibrates;
    /// the running count stays a pure heuristic over everything appended.
    #[test]
    fn no_usage_keeps_pure_heuristic() {
        let mut ledger = ContextLedger::seeded(&[Message::System {
            content: "x".repeat(64),
        }]);
        assert_eq!(ledger.running(), 16);
        ledger.calibrate(0); // no-op: provider sent no usage
        ledger.record_message(&Message::User {
            content: "y".repeat(64),
        });
        assert_eq!(ledger.running(), 32);
    }

    /// Assistant tool-call name + argument bytes count toward the estimate, not
    /// just free text — they are sent on the wire and occupy the window.
    #[test]
    fn assistant_tool_call_bytes_are_counted() {
        let bytes = message_bytes(&Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_owned(),
                name: "read".to_owned(),                // 4
                arguments: r#"{"path":"a"}"#.to_owned(), // 12
            }],
        });
        assert_eq!(bytes, 16);
    }

    /// `threshold × window − max_output`, with an unknown window opting out.
    #[test]
    fn effective_limit_math_and_unknown_window() {
        assert_eq!(effective_limit(10_000, 0.8, Some(2000)), Some(6000));
        assert_eq!(effective_limit(10_000, 0.5, None), Some(5000));
        // Reservation larger than the budget floors at zero, never wraps.
        assert_eq!(effective_limit(1000, 0.5, Some(9999)), Some(0));
        // Unknown window → no limit (callers skip threshold logic).
        assert_eq!(effective_limit(0, 0.8, Some(2000)), None);
    }
}
