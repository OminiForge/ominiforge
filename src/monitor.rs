//! Monitor: derives traces, token/cost usage, cache-hit rate, and per-tool
//! metrics from the event stream without touching the core execution path.
//!
//! The monitor is a pure fold over [`CoreEvent`]s (`doc/monitor.md`): the same
//! [`Monitor::observe`] drives both consumption paths — an offline
//! `inspect <session>` that replays `events.jsonl`, and an online subscriber
//! draining an [`EventBus`](crate::session::EventBus). Cost is derived from
//! `usage` + a pricing table at read time, never written back to the log, so
//! history can be recomputed with current prices (`doc/monitor.md` §6).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::Pricing;
use crate::core::CoreEvent;
use crate::core::payload::{ErrorEvent, EventPayload, ModelEvent, ToolEvent, TurnEvent, Usage};

/// Maps a model id to its pricing, for cost derivation.
///
/// Built from `pricing.toml` (or the inline pricing in `providers.toml`); an
/// unlisted model contributes zero cost rather than erroring
/// (`doc/monitor.md` §6.2).
pub type PricingTable = HashMap<String, Pricing>;

/// Aggregated, derived view of one session, produced by folding its events.
///
/// All counts are saturating; `cost_usd` is `None` when no priced model ran
/// (so the UI can say "unpriced" rather than print a misleading `$0.00`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct SessionSummary {
    pub total_turns: u32,
    pub total_model_requests: u32,
    pub total_tool_calls: u32,
    pub total_tool_failures: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    /// `cache_read / input`, in `[0, 1]`. `0.0` when no input tokens were seen.
    pub cache_hit_rate: f64,
    /// Derived USD cost, or `None` if no model with pricing ran.
    pub cost_usd: Option<f64>,
    /// The first turn's user input, if any — a human-readable title for the
    /// session list (`doc/frontend.md`). `None` for sessions with no user turn
    /// (e.g. an empty draft that was never sent). Not truncated server-side; the
    /// UI clips it for display.
    pub first_user_input: Option<String>,
    /// The timestamp of the *last* turn whose user input was non-empty — the
    /// session's "last activity" for list ordering (`doc/frontend.md`). Unlike
    /// [`first_user_input`](Self::first_user_input) (first-write-wins as a stable
    /// title), this is last-write-wins so the list surfaces recently-touched
    /// sessions. Deliberately keyed on the *user* message, not any event, so a
    /// long-running agent turn does not churn the ordering. `None` for a session
    /// with no real user turn (the UI falls back to `created_at`).
    pub last_user_message_at: Option<DateTime<Utc>>,
    /// `tool_name → call count` (includes failures).
    pub tools_used: HashMap<String, u64>,
    /// One entry per error code, with how many times it occurred.
    pub errors: HashMap<String, u64>,
}

/// Folds an event stream into a [`SessionSummary`]. Drive it with
/// [`observe`](Self::observe) per event, then read [`summary`](Self::summary).
///
/// A `RequestStarted` records its `request_id → model` so the matching
/// `RequestCompleted` (which carries `usage` but not the model) can be priced.
#[derive(Debug, Default)]
pub struct Monitor {
    pricing: PricingTable,
    /// `request_id` → model id, to price a completion against its model.
    request_models: HashMap<String, String>,
    summary: SessionSummary,
    /// Accumulated cost; folded into `summary.cost_usd` lazily so an all-unpriced
    /// run reports `None`.
    cost_acc: f64,
    saw_priced_model: bool,
}

impl Monitor {
    /// A monitor that prices completions against `pricing`.
    #[must_use]
    pub fn new(pricing: PricingTable) -> Self {
        Self {
            pricing,
            ..Self::default()
        }
    }

    /// Fold one event into the running aggregates.
    pub fn observe(&mut self, event: &CoreEvent) {
        match &event.payload {
            EventPayload::Turn(TurnEvent::Started { input, .. }) => {
                self.summary.total_turns = self.summary.total_turns.saturating_add(1);
                // Keep the first non-empty input as the session's title. Later
                // turns don't overwrite it — the opening message is the most
                // recognizable label.
                if let Some(text) = input
                    && !text.trim().is_empty()
                {
                    if self.summary.first_user_input.is_none() {
                        self.summary.first_user_input = Some(text.clone());
                    }
                    // Track the LAST real user message's time for list ordering
                    // (last-write-wins), so a busy session floats to the top.
                    self.summary.last_user_message_at = Some(event.timestamp);
                }
            }
            EventPayload::Model(ModelEvent::RequestStarted {
                request_id, model, ..
            }) => {
                self.request_models
                    .insert(request_id.clone(), model.clone());
            }
            EventPayload::Model(ModelEvent::RequestCompleted {
                request_id, usage, ..
            }) => self.observe_completion(request_id, usage),
            EventPayload::Tool(ToolEvent::Started { tool_name, .. }) => {
                self.summary.total_tool_calls = self.summary.total_tool_calls.saturating_add(1);
                *self
                    .summary
                    .tools_used
                    .entry(tool_name.clone())
                    .or_insert(0) += 1;
            }
            EventPayload::Tool(ToolEvent::Failed { error, .. }) => {
                self.summary.total_tool_failures =
                    self.summary.total_tool_failures.saturating_add(1);
                *self.summary.errors.entry(error.code.clone()).or_insert(0) += 1;
            }
            EventPayload::Error(ErrorEvent::Raised(detail)) => {
                *self.summary.errors.entry(detail.code.clone()).or_insert(0) += 1;
            }
            _ => {}
        }
    }

    /// Account for a completed model request: tally tokens and derive cost.
    fn observe_completion(&mut self, request_id: &str, usage: &Usage) {
        self.summary.total_model_requests = self.summary.total_model_requests.saturating_add(1);
        self.summary.total_input_tokens = self
            .summary
            .total_input_tokens
            .saturating_add(u64::from(usage.input_tokens));
        self.summary.total_output_tokens = self
            .summary
            .total_output_tokens
            .saturating_add(u64::from(usage.output_tokens));
        self.summary.total_cache_read_tokens = self
            .summary
            .total_cache_read_tokens
            .saturating_add(u64::from(usage.cache_read_tokens));

        // Price against the model that started this request, if known + listed.
        if let Some(model) = self.request_models.get(request_id)
            && let Some(pricing) = self.pricing.get(model)
        {
            self.cost_acc += cost_of(usage, pricing);
            self.saw_priced_model = true;
        }
    }

    /// Finalize and return the summary. Computes the derived ratios/cost from the
    /// accumulated tallies.
    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        let mut summary = self.summary.clone();
        summary.cache_hit_rate = if summary.total_input_tokens == 0 {
            0.0
        } else {
            // u64→f64 is lossy only past 2^53 tokens, which no session reaches.
            #[allow(clippy::cast_precision_loss)]
            {
                summary.total_cache_read_tokens as f64 / summary.total_input_tokens as f64
            }
        };
        summary.cost_usd = self.saw_priced_model.then_some(self.cost_acc);
        summary
    }
}

/// Derive the USD cost of one request from its `usage` and a model's `pricing`.
/// Cache read/write rates default to the input rate / zero when unset.
fn cost_of(usage: &Usage, pricing: &Pricing) -> f64 {
    let per = |tokens: u32, rate: f64| f64::from(tokens) * rate / 1_000_000.0;
    let cache_read_rate = pricing
        .cache_read_per_million
        .unwrap_or(pricing.input_per_million);
    let cache_write_rate = pricing.cache_write_per_million.unwrap_or(0.0);
    per(usage.input_tokens, pricing.input_per_million)
        + per(usage.output_tokens, pricing.output_per_million)
        + per(usage.cache_read_tokens, cache_read_rate)
        + per(usage.cache_write_tokens, cache_write_rate)
}

/// Replay a full event stream into a [`SessionSummary`] offline (the
/// `inspect <session>` path). Equivalent to `observe`-ing each event then
/// reading `summary`.
#[must_use]
pub fn summarize(events: &[CoreEvent], pricing: PricingTable) -> SessionSummary {
    let mut monitor = Monitor::new(pricing);
    for event in events {
        monitor.observe(event);
    }
    monitor.summary()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::core::payload::{ErrorDetail, ErrorSeverity, StopReason, ToolOutput, ToolSource};
    use crate::core::{
        CoreEvent, EventId, EventSource, SCHEMA_VERSION, SessionId, SourceKind, TurnId,
    };
    use chrono::TimeZone;

    fn sid() -> SessionId {
        SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned())
    }

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

    fn request_started(request_id: &str, model: &str) -> EventPayload {
        EventPayload::Model(ModelEvent::RequestStarted {
            request_id: request_id.to_owned(),
            provider: "test".to_owned(),
            model: model.to_owned(),
            temperature: 0.0,
            max_tokens: None,
            tool_schemas_count: 0,
            input_tokens_estimate: 0,
        })
    }

    fn request_completed(request_id: &str, usage: Usage) -> EventPayload {
        EventPayload::Model(ModelEvent::RequestCompleted {
            request_id: request_id.to_owned(),
            stop_reason: StopReason::EndTurn,
            usage,
            duration_ms: 1,
            time_to_first_token_ms: None,
            provider_request_id: None,
        })
    }

    fn tool_started(name: &str) -> EventPayload {
        EventPayload::Tool(ToolEvent::Started {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: 0,
            },
            tool_name: name.to_owned(),
            source: ToolSource::Builtin,
            input: serde_json::Value::Null,
            working_dir: None,
        })
    }

    fn tool_completed() -> EventPayload {
        EventPayload::Tool(ToolEvent::Completed {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: 0,
            },
            result: ToolOutput {
                content: vec![],
                is_error: false,
                error_code: None,
            },
            duration_ms: 1,
            output_bytes: 0,
            artifacts_created: vec![],
        })
    }

    fn tool_failed(code: &str) -> EventPayload {
        EventPayload::Tool(ToolEvent::Failed {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: 0,
            },
            duration_ms: 1,
            error: ErrorDetail {
                code: code.to_owned(),
                message: "boom".to_owned(),
                severity: ErrorSeverity::Error,
                retryable: false,
                source_event_id: None,
                provider_raw: None,
            },
        })
    }

    fn pricing(input: f64, output: f64) -> PricingTable {
        let mut table = PricingTable::new();
        table.insert(
            "gpt-4o".to_owned(),
            Pricing {
                input_per_million: input,
                output_per_million: output,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
        );
        table
    }

    /// The first turn's user input becomes the session title and later turns
    /// don't overwrite it — the opening message is the recognizable label, so a
    /// long multi-turn session still lists under what it started as. An
    /// empty/whitespace opening input is skipped in favour of the next real one.
    #[test]
    fn first_user_input_captures_opening_turn_only() {
        let events = vec![
            ev(0, runtime_src(), started("fix the auth bug")),
            ev(1, model_src(), request_started("r1", "gpt-4o")),
            ev(2, runtime_src(), started("now add a test")),
        ];
        let summary = summarize(&events, PricingTable::new());
        assert_eq!(summary.total_turns, 2);
        assert_eq!(
            summary.first_user_input.as_deref(),
            Some("fix the auth bug")
        );
    }

    /// A session whose only turn carried no input (or empty input) has no title,
    /// so the UI falls back to workspace/id rather than printing a blank.
    #[test]
    fn first_user_input_is_none_without_real_input() {
        let blank = EventPayload::Turn(TurnEvent::Started {
            turn_id: TurnId("t".to_owned()),
            input: Some("   ".to_owned()),
        });
        let events = vec![ev(0, runtime_src(), blank)];
        let summary = summarize(&events, PricingTable::new());
        assert_eq!(summary.total_turns, 1);
        assert_eq!(summary.first_user_input, None);
    }

    /// `last_user_message_at` tracks the *latest* real user turn (last-write-wins),
    /// unlike `first_user_input` which pins the opening one. This is what lets the
    /// session list sort by recent activity: a follow-up message moves the session
    /// to the top. Built with explicit timestamps so the "last, not first" choice
    /// is asserted deterministically (the `ev` helper stamps `now`).
    #[test]
    fn last_user_message_at_tracks_the_latest_user_turn() {
        let t0 = Utc.with_ymd_and_hms(2026, 7, 15, 9, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 7, 15, 10, 30, 0).unwrap();
        let at = |seq: u64, ts: DateTime<Utc>, payload: EventPayload| CoreEvent {
            timestamp: ts,
            ..ev(seq, runtime_src(), payload)
        };
        let events = vec![
            at(0, t0, started("fix the auth bug")),
            at(1, t0, request_started("r1", "gpt-4o")),
            at(2, t2, started("now add a test")),
        ];
        let summary = summarize(&events, PricingTable::new());
        // Title stays the opening message; activity time is the latest user turn.
        assert_eq!(summary.first_user_input.as_deref(), Some("fix the auth bug"));
        assert_eq!(summary.last_user_message_at, Some(t2));
    }

    /// A blank follow-up turn does not advance `last_user_message_at`: activity
    /// ordering keys on *real* user messages only, so an empty send can't reorder
    /// the list. Mirrors the `first_user_input` empty-input skip.
    #[test]
    fn last_user_message_at_ignores_blank_input() {
        let t0 = Utc.with_ymd_and_hms(2026, 7, 15, 9, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 7, 15, 9, 5, 0).unwrap();
        let blank = EventPayload::Turn(TurnEvent::Started {
            turn_id: TurnId("t".to_owned()),
            input: Some("   ".to_owned()),
        });
        let at = |seq: u64, ts: DateTime<Utc>, payload: EventPayload| CoreEvent {
            timestamp: ts,
            ..ev(seq, runtime_src(), payload)
        };
        let events = vec![at(0, t0, started("real message")), at(1, t1, blank)];
        let summary = summarize(&events, PricingTable::new());
        assert_eq!(summary.last_user_message_at, Some(t0));
    }

    /// A representative two-turn stream aggregates into the expected counts, and
    /// cost is derived from usage × pricing for the model that ran. This pins the
    /// numbers the `inspect` view prints (`doc/monitor.md` §8).
    #[test]
    fn aggregates_turns_requests_tokens_and_cost() {
        let events = vec![
            ev(0, runtime_src(), started("hi")),
            ev(1, model_src(), request_started("r1", "gpt-4o")),
            ev(
                2,
                model_src(),
                request_completed(
                    "r1",
                    Usage {
                        input_tokens: 1000,
                        output_tokens: 200,
                        cache_read_tokens: 250,
                        cache_write_tokens: 0,
                    },
                ),
            ),
            ev(3, tool_src("read"), tool_started("read")),
            ev(4, tool_src("read"), tool_completed()),
            ev(5, runtime_src(), started("again")),
            ev(6, model_src(), request_started("r2", "gpt-4o")),
            ev(
                7,
                model_src(),
                request_completed(
                    "r2",
                    Usage {
                        input_tokens: 1000,
                        output_tokens: 100,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                ),
            ),
        ];

        // gpt-4o at $2.50/M input, $10/M output.
        let summary = summarize(&events, pricing(2.50, 10.00));

        assert_eq!(summary.total_turns, 2);
        assert_eq!(summary.total_model_requests, 2);
        assert_eq!(summary.total_tool_calls, 1);
        assert_eq!(summary.total_tool_failures, 0);
        assert_eq!(summary.total_input_tokens, 2000);
        assert_eq!(summary.total_output_tokens, 300);
        assert_eq!(summary.total_cache_read_tokens, 250);
        // cache_hit_rate = 250 / 2000.
        assert_eq!(summary.cache_hit_rate, 0.125);
        assert_eq!(*summary.tools_used.get("read").unwrap(), 1);

        // cost = 2000/M × 2.50 + 300/M × 10 + 250/M × 2.50 (cache read defaults
        // to input rate) = 0.005 + 0.003 + 0.000625 = 0.008625.
        let cost = summary.cost_usd.unwrap();
        assert!((cost - 0.008_625).abs() < 1e-9, "got {cost}");
    }

    /// A model with no pricing entry contributes zero cost and the summary
    /// reports `None` (not a misleading `$0.00`) — the unpriced fallback
    /// (`doc/monitor.md` §6.2).
    #[test]
    fn unpriced_model_yields_none_cost_not_zero() {
        let events = vec![
            ev(0, model_src(), request_started("r1", "local-llama")),
            ev(
                1,
                model_src(),
                request_completed(
                    "r1",
                    Usage {
                        input_tokens: 500,
                        output_tokens: 50,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                ),
            ),
        ];
        // Pricing table knows gpt-4o, not local-llama.
        let summary = summarize(&events, pricing(2.50, 10.00));
        assert_eq!(summary.total_input_tokens, 500);
        assert_eq!(summary.cost_usd, None);
    }

    /// Tool failures and raised errors both tally under their error code, and
    /// failures bump the failure counter.
    #[test]
    fn counts_tool_failures_and_errors_by_code() {
        let events = vec![
            ev(0, tool_src("shell"), tool_started("shell")),
            ev(1, tool_src("shell"), tool_failed("execution_failed")),
            ev(
                2,
                runtime_src(),
                EventPayload::Error(ErrorEvent::Raised(ErrorDetail {
                    code: "model_transport".to_owned(),
                    message: "dropped".to_owned(),
                    severity: ErrorSeverity::Error,
                    retryable: true,
                    source_event_id: None,
                    provider_raw: None,
                })),
            ),
        ];
        let summary = summarize(&events, PricingTable::new());
        assert_eq!(summary.total_tool_calls, 1);
        assert_eq!(summary.total_tool_failures, 1);
        assert_eq!(*summary.errors.get("execution_failed").unwrap(), 1);
        assert_eq!(*summary.errors.get("model_transport").unwrap(), 1);
    }

    /// An empty stream yields the zero summary with no cost and a 0.0 hit rate
    /// (no division by zero).
    #[test]
    fn empty_stream_is_zero_summary() {
        let summary = summarize(&[], PricingTable::new());
        assert_eq!(summary, SessionSummary::default());
        assert_eq!(summary.cache_hit_rate, 0.0);
        assert_eq!(summary.cost_usd, None);
    }
}
