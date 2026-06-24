//! Domain event payloads. The envelope ([`super::CoreEvent`]) is shared; the
//! payload varies by domain. See `doc/event-schema.md` §3–§10.
//!
//! `MonitorEvent` is intentionally absent: monitoring is a derived view built
//! from this stream, not a payload kind (see `doc/monitor.md`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ids::{ArtifactId, EventId, SessionId, TurnId};

/// Domain-tagged payload of a [`super::CoreEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum EventPayload {
    Turn(TurnEvent),
    Model(ModelEvent),
    Tool(ToolEvent),
    Session(SessionEvent),
    Artifact(ArtifactEvent),
    Injection(InjectionEvent),
    Hook(HookEvent),
    Error(ErrorEvent),
}

/// Turn lifecycle.
///
/// A turn is one iteration of the agent loop and has an explicit state machine:
/// `pending → running → completed | failed | interrupted`. A failed or
/// interrupted turn records its break point so it can resume without the user
/// re-entering input. See `doc/event-schema.md` §4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum TurnEvent {
    Started {
        turn_id: TurnId,
        /// The user input that opened the turn, when one did. `None` for turns
        /// started by a non-user trigger (scheduler, autonomous continuation).
        /// Replay reconstructs the opening user message from this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
    },
    Completed {
        turn_id: TurnId,
    },
    Failed {
        turn_id: TurnId,
        failed_at_event_id: EventId,
        retryable: bool,
        /// Why the turn did not finish cleanly. Optional for backward
        /// compatibility with logs written before the field existed; new
        /// failures always set it so replay/monitoring can explain the stop
        /// without guessing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<TurnFailureReason>,
    },
    Interrupted {
        turn_id: TurnId,
        interrupted_at_event_id: EventId,
    },
    Resumed {
        turn_id: TurnId,
        resume_from_event_id: EventId,
    },
}

/// Why a turn ended without a clean `Completed`.
///
/// These are *graceful* stops the loop records and hands back to the caller
/// (the turn's side effects still stand) — not transport/persistence errors.
/// A hard error surfaces as a `Result::Err` (never a [`TurnFailureReason`]) but
/// still leaves a trace: the loop records a `TurnEvent::Failed` with
/// `reason: None` paired with an [`ErrorEvent::Raised`] carrying the detail, so
/// `reason.is_some()` distinguishes a graceful stop from a hard abort. See
/// `doc/event-schema.md` §4 and `doc/plan.md` §6–§7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum TurnFailureReason {
    /// The absolute max-rounds safety net tripped: the tool loop ran this many
    /// model rounds without the model giving a final answer.
    MaxRoundsExceeded { max_rounds: u32 },
    /// The completion gate gave up: the model kept stopping with this many
    /// non-terminal plan steps after repeated nudges (`doc/plan.md` §6).
    PlanStalled { incomplete_steps: u32 },
    /// A `turn:start` before hook blocked the turn before any model round ran
    /// (`doc/hook-protocol.md` §7). `by` names the blocking hook.
    BlockedByHook { by: String, reason: String },
}

/// Model interaction.
///
/// Streaming arrives as per-token deltas on the provider boundary
/// (`crate::llm::StreamEvent`); persisted history consolidates each content
/// block into one [`ModelEvent::ContentBlock`] rather than one event per token,
/// so the log stays compact and replayable.
///
/// A model emitting a tool call is a `ModelEvent`; the tool actually running is
/// a [`ToolEvent`] — the two are kept separate. See `doc/event-schema.md` §5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ModelEvent {
    RequestStarted {
        request_id: String,
        provider: String,
        model: String,
        temperature: f32,
        max_tokens: Option<u32>,
        tool_schemas_count: u32,
        input_tokens_estimate: u32,
    },
    /// One fully-assembled content block (text, reasoning, or a tool call).
    ///
    /// The streaming deltas that built it are live-transport only and are not
    /// persisted one-per-token. A `ToolCall` block's `ContentBlock` event is the
    /// one a [`ToolEvent::Started`] points back at via `tool_call_event_id`.
    ContentBlock {
        request_id: String,
        index: u32,
        content: BlockContent,
    },
    RequestCompleted {
        request_id: String,
        stop_reason: StopReason,
        usage: Usage,
        duration_ms: u64,
        time_to_first_token_ms: Option<u64>,
        provider_request_id: Option<String>,
    },
    RequestFailed {
        request_id: String,
        duration_ms: u64,
        error: ErrorDetail,
    },
}

/// The kind of content block a model is streaming.
///
/// Used on the streaming boundary ([`crate::llm::StreamEvent::BlockStart`]): a
/// `ToolCall` block carries the call `id` and `name` at its start so the agent
/// loop has what it needs to dispatch the tool; the JSON arguments arrive as
/// subsequent deltas. Persisted history uses [`BlockContent`] instead, which
/// carries the fully-assembled block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ContentBlockType {
    Text,
    Reasoning,
    ToolCall { id: String, name: String },
}

/// A fully-assembled content block, persisted as one [`ModelEvent::ContentBlock`].
///
/// This is the consolidated counterpart to the streaming
/// [`ContentBlockType`] + deltas: instead of one event per token, the collector
/// accumulates a block and records it once, here, with its complete content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum BlockContent {
    /// Assistant free-text, concatenated from all text deltas.
    Text { text: String },
    /// Reasoning/thinking text, concatenated from all reasoning deltas.
    Reasoning { text: String },
    /// A tool call with its arguments fully assembled into a JSON string.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
}

/// Token accounting reported by the provider.
///
/// Cost is *not* stored: the monitor derives it from `usage` plus a
/// configurable pricing table, so history can be recomputed with current
/// prices. See `doc/monitor.md` §3, §6.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

/// Tool execution. `Started` points back at the model tool-call event that
/// triggered it; `Completed`/`Failed` carry timing and output metadata. See
/// `doc/event-schema.md` §6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ToolEvent {
    Started {
        tool_call_event_id: EventId,
        tool_name: String,
        source: ToolSource,
        input: serde_json::Value,
        working_dir: Option<PathBuf>,
    },
    Completed {
        tool_call_event_id: EventId,
        result: ToolOutput,
        duration_ms: u64,
        output_bytes: usize,
        artifacts_created: Vec<ArtifactId>,
    },
    Failed {
        tool_call_event_id: EventId,
        duration_ms: u64,
        error: ErrorDetail,
    },
}

/// Where a tool came from. The agent loop treats both uniformly; this only
/// drives source-aware monitoring (which MCP server is slow/failing/etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ToolSource {
    Builtin,
    Mcp { server_name: String },
}

/// The recorded outcome of a tool invocation.
///
/// A business-level failure is a successful invocation with `is_error = true`;
/// protocol errors surface as a [`ToolEvent::Failed`] instead. The tool layer's
/// `ToolResult` alias is `Result<ToolOutput, ToolError>`. See
/// `doc/tool-protocol.md` §7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ToolOutput {
    pub content: Vec<Content>,
    pub is_error: bool,
    #[serde(default)]
    pub error_code: Option<String>,
}

/// A unit of tool output. Payloads over 64KB are spilled to the artifact store
/// by the runtime and referenced via [`Content::ArtifactRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum Content {
    Text(String),
    Image {
        media_type: String,
        data: Vec<u8>,
    },
    ArtifactRef {
        artifact_id: ArtifactId,
        media_type: String,
    },
}

/// Session lifecycle. `Created` is always the first event and snapshots the
/// initial config so replay is self-contained. See `doc/event-schema.md` §7
/// and `doc/session-storage.md` §3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum SessionEvent {
    Created {
        profile_id: Option<String>,
        tools: Vec<String>,
        workspace: Option<PathBuf>,
    },
    Forked {
        parent_session_id: SessionId,
        fork_at_seq: u64,
    },
    Paused,
    Resumed,
    Ended {
        reason: SessionEndReason,
    },
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum SessionEndReason {
    Completed,
    Cancelled,
    Error,
}

/// Artifact lifecycle. The event records only a reference; content lives in the
/// artifact store. See `doc/event-schema.md` §8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ArtifactEvent {
    Created {
        artifact_id: ArtifactId,
        /// e.g. `"file"`, `"image"`, `"code_block"`.
        kind: String,
        media_type: String,
        /// Artifact-store path or URI.
        uri: String,
        size: u64,
        #[serde(default)]
        sha256: Option<String>,
        #[serde(default)]
        source_event_id: Option<EventId>,
    },
}

/// Dynamic context injection, recorded so replay can reconstruct exactly what
/// the model saw each turn. See `doc/event-schema.md` §9 and
/// `doc/context-management.md` §5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum InjectionEvent {
    ContextInjected {
        source: InjectionSource,
        content: String,
        token_count: u32,
    },
}

/// What produced an injection. Ordering is kept stable (Memory → Rag → Acp →
/// Hook → Runtime) to avoid needless prefix-cache churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum InjectionSource {
    Memory,
    #[serde(rename = "RAG")]
    Rag,
    #[serde(rename = "ACP")]
    Acp,
    Hook,
    /// The agent loop itself injected the text — e.g. a completion-gate or
    /// stuck-step reminder pushed into the context to keep a turn on track.
    /// See `doc/plan.md` §8.
    Runtime,
}

/// Hook execution at a pipeline point.
///
/// Recorded so replay and monitoring can see every hook that ran, what it
/// decided, and how long it took: the protocol requires all hook executions to
/// be logged (`doc/hook-protocol.md` §1, §11).
///
/// A hook that *blocks* additionally produces the point-specific failure event
/// (e.g. a `tool:invoke:before` block emits a paired [`ToolEvent::Failed`] with
/// code `blocked_by_hook`, `doc/hook-protocol.md` §8); this event records the
/// hook's own execution regardless of outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum HookEvent {
    Executed {
        /// The hook's configured name.
        hook_name: String,
        /// The pipeline point it fired at, e.g. `"tool:invoke:before"`.
        hook_point: String,
        outcome: HookOutcome,
        duration_ms: u64,
    },
}

/// What a hook decided when it ran.
///
/// Before hooks can `Pass`/`Modified`/`Blocked`; after hooks only `Observed`.
/// `Failed` covers a hook that errored, timed out, or returned invalid output —
/// the pipeline's response to it is governed by the hook's failure mode
/// (`doc/hook-protocol.md` §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum HookOutcome {
    /// Before hook let the pipeline continue unchanged.
    Pass,
    /// Before hook rewrote the payload.
    Modified,
    /// Before hook stopped the pipeline.
    Blocked { reason: String },
    /// After hook observed the point (no control).
    Observed,
    /// The hook itself failed (non-zero exit, timeout, bad output).
    Failed { error: String },
}

/// A structured error surfaced as its own event. See `doc/event-schema.md` §10.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ErrorEvent {
    Raised(ErrorDetail),
}

/// Structured error detail, reused across model/tool/error events.
///
/// Carries more than a string so consumers can route on `code`, `severity`,
/// and `retryable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    pub severity: ErrorSeverity,
    pub retryable: bool,
    #[serde(default)]
    pub source_event_id: Option<EventId>,
    #[serde(default)]
    pub provider_raw: Option<serde_json::Value>,
}

/// Severity of an [`ErrorDetail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum ErrorSeverity {
    Fatal,
    Error,
    Warning,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::{CoreEvent, EventSource, SCHEMA_VERSION, SourceKind};
    use chrono::{TimeZone, Utc};

    /// The first event of every session is `SessionEvent::Created`. This mirrors
    /// the events.jsonl line shown in `doc/session-storage.md` §3 and pins the
    /// externally-tagged JSON wire shape (`{"Session":{"Created":{...}}}`).
    #[test]
    fn session_created_event_round_trips() {
        let event = CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq: 0,
            session_id: SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned()),
            timestamp: Utc.with_ymd_and_hms(2026, 6, 11, 10, 0, 0).unwrap(),
            source: EventSource {
                kind: SourceKind::Runtime,
                id: "ominiforge".to_owned(),
            },
            parent_event_id: None,
            turn_id: None,
            payload: EventPayload::Session(SessionEvent::Created {
                profile_id: Some("coding-agent".to_owned()),
                tools: vec!["shell".to_owned(), "read_file".to_owned()],
                workspace: None,
            }),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""Session":{"Created""#));
        let decoded: CoreEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
    }

    /// `Usage` is stored; cost is derived elsewhere. Guard the field names that
    /// the monitor's pricing math depends on.
    #[test]
    fn usage_serializes_token_fields() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 80,
            cache_write_tokens: 0,
        };
        let value = serde_json::to_value(usage).unwrap();
        assert_eq!(value["input_tokens"], 100);
        assert_eq!(value["cache_read_tokens"], 80);
        assert!(value.get("cost").is_none());
    }

    /// A blocking hook execution round-trips and pins the externally-tagged wire
    /// shape (`{"Hook":{"Executed":{...,"outcome":{"Blocked":{...}}}}}`) so
    /// replay and monitoring can route on it.
    #[test]
    fn hook_blocked_event_round_trips() {
        let event = CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq: 7,
            session_id: SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned()),
            timestamp: Utc.with_ymd_and_hms(2026, 6, 23, 10, 0, 0).unwrap(),
            source: EventSource {
                kind: SourceKind::Runtime,
                id: "ominiforge".to_owned(),
            },
            parent_event_id: None,
            turn_id: None,
            payload: EventPayload::Hook(HookEvent::Executed {
                hook_name: "security-guard".to_owned(),
                hook_point: "tool:invoke:before".to_owned(),
                outcome: HookOutcome::Blocked {
                    reason: "Dangerous command pattern detected: rm -rf".to_owned(),
                },
                duration_ms: 12,
            }),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""Hook":{"Executed""#));
        assert!(json.contains(r#""outcome":{"Blocked""#));
        let decoded: CoreEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
    }
}
