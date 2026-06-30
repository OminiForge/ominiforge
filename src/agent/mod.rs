//! The agent loop: drives a turn from user input to final answer.
//!
//! A turn ([`Agent::run_turn`]) opens with `TurnEvent::Started`, then runs one
//! or more model rounds. Each round streams a model response (persisted as
//! `ModelEvent`s by [`collector`]); if the model asked for tools, each is
//! dispatched (persisted as `ToolEvent`s) and its result fed back as a `Tool`
//! message before the next round. The loop ends when the model stops without
//! requesting tools **and** the working plan (if any) has no non-terminal
//! steps left — the completion gate (`doc/plan.md` §6).
//!
//! State has three homes by lifetime (`doc/plan.md` §3):
//! - turn-invariant deps (provider, tools, config) live on [`Agent`];
//! - session-scoped state (the conversation view and the working plan) lives in
//!   [`SessionRuntime`], owned by the caller so it survives across turns;
//! - turn-scoped state (round counter, gate/stuck counters, output
//!   accumulation) lives in [`TurnState`], built when a turn starts and dropped
//!   when it ends.
//!
//! `run_turn` borrows a [`SessionRuntime`] and a [`SessionWriter`] and appends
//! to both. Context compaction and prefix-cache management arrive with the
//! `context` module (Phase 2).

mod collector;
mod error;
mod plan;
mod resume;
mod sink;

pub use error::AgentError;
pub use plan::{PlanStep, StepStatus};
pub use resume::rebuild_runtime;
pub use sink::{BlockKind, NullSink, StreamSink};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::context::{
    ContextLedger, DEFAULT_COMPACTION_THRESHOLD, effective_limit, estimate_tokens,
};
use crate::core::payload::{
    Content, ErrorDetail, ErrorEvent, ErrorSeverity, HookEvent, InjectionEvent, InjectionSource,
    ModelEvent, StopReason, ToolEvent, ToolOutput, ToolSource, TurnEvent, TurnFailureReason, Usage,
};
use crate::core::{EventId, EventPayload, EventSource, SourceKind, TurnId};
use crate::hook::{BeforeEffect, HookExecution, HookPoint, HookRegistry};
use crate::llm::{LlmError, Message, ModelRequest, Provider, StreamEvent, ToolCall, ToolSchema};
use crate::session::SessionWriter;
use crate::tool::{ToolError, ToolInput, ToolRegistry};

use futures_util::StreamExt;

use plan::{PLAN_TOOL_NAME, PlanError, PlanOp, apply_plan_op};

/// How many completion-gate nudges a turn tolerates before giving up: the model
/// stopped without finishing the plan this many times running (`doc/plan.md` §6).
const MAX_GATE: u8 = 2;

/// How many consecutive rounds a step may stay `in_progress` before the loop
/// injects a one-shot stuck warning (`doc/plan.md` §7).
const STUCK_THRESHOLD: u32 = 5;

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
    /// Absolute safety net on model rounds in one turn. This is *not* the
    /// primary loop control — the completion gate and stuck detection
    /// (`doc/plan.md` §6–§7) catch a misbehaving turn far earlier and more
    /// cheaply. `max_rounds` only backstops a runaway that slips past both, so
    /// it is set generously: a routine multi-step task (read many files, run a
    /// few commands, write output) legitimately needs dozens of rounds.
    pub max_rounds: u32,
    /// The model's context window in tokens, for the usage estimate's effective
    /// limit. `0` means "unknown" (threshold tracking is skipped).
    pub context_window: u32,
    /// Fraction of the context window to stay under before compaction is due
    /// (`doc/context-management.md` §4.2). Step 2 only warns at this threshold;
    /// compaction itself lands in Step 3.
    pub compaction_threshold: f32,
    /// Canonical workspace root, used to discover project guidance files
    /// (`AGENTS.md`/`CLAUDE.md`) for the paths tools touch (`doc/agents-md.md`).
    /// Empty disables nested-guidance discovery.
    pub workspace: PathBuf,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            temperature: 0.0,
            max_tokens: None,
            tool_timeout: Duration::from_secs(120),
            max_rounds: 100,
            context_window: 0,
            compaction_threshold: DEFAULT_COMPACTION_THRESHOLD,
            workspace: PathBuf::new(),
        }
    }
}

/// Session-scoped runtime state that survives across turns.
///
/// Owned by the interactive loop / CLI and borrowed by each [`TurnState`].
/// Rebuilt from `events.jsonl` when resuming a session (replay the plan ops and
/// the conversation view; see `doc/plan.md` §10.3 — Phase 2). In the Phase 1
/// single-turn CLI it is built fresh per `run` and discarded, the degenerate
/// case of the same interface.
#[derive(Debug, Clone, Default)]
pub struct SessionRuntime {
    /// Conversation view sent to the model; appended each turn.
    pub context: Vec<Message>,
    /// Working plan; survives across turns until every step reaches a terminal
    /// state or the model replaces it via `init` (`doc/plan.md` §10).
    pub plan: Vec<PlanStep>,
    /// Running input-token estimate for the context view, calibrated each round
    /// from the provider's authoritative usage (`doc/phase2-plan.md` Step 2).
    pub ledger: ContextLedger,
    /// Workspace-relative paths of nested project-guidance files
    /// (`AGENTS.md`/`CLAUDE.md`) already injected this session, so each is loaded
    /// at most once however many times its subtree is touched
    /// (`doc/agents-md.md`). The root file lives in the system prompt and is
    /// never tracked here. Rebuilt on resume from the injection log.
    pub loaded_guidance: HashSet<String>,
}

impl SessionRuntime {
    /// A runtime seeded with an initial context (typically the system message)
    /// and an empty plan. The ledger is primed from the seed so the first turn's
    /// pre-request estimate already accounts for it.
    #[must_use]
    pub fn new(context: Vec<Message>) -> Self {
        let ledger = ContextLedger::seeded(&context);
        Self {
            context,
            plan: Vec::new(),
            ledger,
            loaded_guidance: HashSet::new(),
        }
    }

    /// Append a message to the context view and account for its tokens. Every
    /// addition to `context` must go through here so the ledger stays in step.
    fn push_message(&mut self, message: Message) {
        self.ledger.record_message(&message);
        self.context.push(message);
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
    /// Token usage summed over every model round in the turn.
    pub usage: Usage,
    /// `None` if the turn finished cleanly (`TurnEvent::Completed`); otherwise
    /// why it was cut short. The work done so far still stands — the caller
    /// decides whether to surface it, retry, or prompt the user. Mirrors the
    /// `reason` on the persisted `TurnEvent::Failed`.
    pub incomplete: Option<TurnFailureReason>,
    /// Running input-token estimate for the context view at turn end, calibrated
    /// from the provider's usage where available (`doc/phase2-plan.md` Step 2).
    pub context_tokens: u32,
    /// The token budget the context should stay under (`threshold × window −
    /// max_output`), or `None` when the context window is unknown. `context_tokens`
    /// exceeding this is the compaction trigger (Step 3); Step 2 only warns.
    pub context_limit: Option<u32>,
}

/// Couples a model provider with a tool registry and per-turn config.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    config: AgentConfig,
    /// Optional dedicated provider + model id for compaction summaries
    /// (`doc/phase2-plan.md` decision B). `None` reuses the main provider/model.
    compaction: Option<(Arc<dyn Provider>, String)>,
    /// Hooks fired at fixed pipeline points (`doc/hook-protocol.md`). Empty by
    /// default — a no-op until the caller attaches a registry.
    hooks: HookRegistry,
}

impl Agent {
    /// Build an agent.
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
            compaction: None,
            hooks: HookRegistry::new(),
        }
    }

    /// Use a dedicated provider + model for compaction summaries instead of the
    /// session's current model (`doc/phase2-plan.md` decision B).
    #[must_use]
    pub fn with_compaction_model(mut self, provider: Arc<dyn Provider>, model: String) -> Self {
        self.compaction = Some((provider, model));
        self
    }

    /// Attach a hook registry. Hooks fire at `turn:start`, `turn:end`,
    /// `tool:invoke:before`, and `tool:invoke:after` (`doc/hook-protocol.md`).
    #[must_use]
    pub fn with_hooks(mut self, hooks: HookRegistry) -> Self {
        self.hooks = hooks;
        self
    }

    /// Run one turn: append `input` to the runtime context, drive model rounds
    /// and tool calls to completion, and persist every event through `writer`.
    ///
    /// `runtime` is mutated in place — the user message, the assistant message,
    /// any tool results, and any plan changes are applied, leaving it ready for
    /// the next turn.
    ///
    /// This is the headless form: streamed output is persisted but not observed
    /// live. Use [`run_turn_with_sink`](Self::run_turn_with_sink) to render the
    /// model's output as it streams.
    ///
    /// # Errors
    /// [`AgentError::Model`] on provider failure or [`AgentError::Session`] on a
    /// persistence failure. Running out of round budget or stalling on the plan
    /// is *not* an error: it returns `Ok` with [`TurnOutcome::incomplete`] set.
    pub async fn run_turn(
        &self,
        writer: &mut SessionWriter,
        runtime: &mut SessionRuntime,
        input: String,
    ) -> Result<TurnOutcome, AgentError> {
        self.run_turn_with_sink(writer, runtime, input, &mut NullSink)
            .await
    }

    /// Like [`run_turn`](Self::run_turn), but forwards every streamed delta to
    /// `sink` in real time so a front-end can render the turn as it unfolds.
    /// `sink.on_turn_end()` is called once the turn settles (on success).
    ///
    /// # Errors
    /// Same as [`run_turn`](Self::run_turn).
    pub async fn run_turn_with_sink(
        &self,
        writer: &mut SessionWriter,
        runtime: &mut SessionRuntime,
        input: String,
        sink: &mut dyn StreamSink,
    ) -> Result<TurnOutcome, AgentError> {
        let turn_id = TurnId(ulid::Ulid::new().to_string());
        let mut turn = TurnState {
            agent: self,
            runtime,
            writer,
            sink,
            turn_id,
            round: 0,
            answer: String::new(),
            stop_reason: StopReason::EndTurn,
            accumulated_usage: Usage::default(),
            gate_count: 0,
            step_stuck_rounds: HashMap::new(),
        };
        turn.run(input).await
    }

    fn tool_schemas(&self) -> Vec<ToolSchema> {
        // Leaf-tool descriptors plus the `plan` control-tool descriptor, all
        // sorted by name so the schema block stays byte-stable for the prefix
        // cache (`doc/context-management.md` §3, `doc/plan.md` §5).
        let mut schemas: Vec<ToolSchema> = self
            .tools
            .descriptors()
            .into_iter()
            .map(|d| ToolSchema {
                name: d.name,
                description: d.description,
                parameters: d.input_schema,
            })
            .collect();
        schemas.push(plan::descriptor());
        schemas.sort_by(|a, b| a.name.cmp(&b.name));
        schemas
    }

    fn model_source(&self) -> EventSource {
        EventSource {
            kind: SourceKind::Model,
            id: format!("{}/{}", self.provider.name(), self.config.model),
        }
    }

    /// Generate a compaction summary of the current context. Calls the model with
    /// a summarization prompt, collects the response, and returns a new snapshot:
    /// system messages + summary + optionally the last `keep_last` user turns.
    ///
    /// Returns `None` if there's nothing to summarize (context is too short).
    ///
    /// # Errors
    /// [`AgentError::Model`] on provider failure.
    pub async fn compact(
        &self,
        runtime: &SessionRuntime,
        keep_last: Option<usize>,
    ) -> Result<Option<Vec<Message>>, AgentError> {
        let (system, to_summarize, tail) = split_for_compaction(&runtime.context, keep_last);

        if to_summarize.is_empty() {
            return Ok(None);
        }

        let mut messages = system.clone();
        messages.extend(to_summarize.iter().cloned());
        messages.push(Message::User {
            content: "<instruction>Summarize the above conversation concisely, preserving \
                      key facts, decisions, and context needed to continue the conversation. \
                      Keep it under 500 tokens.</instruction>"
                .to_owned(),
        });

        // Use the dedicated compaction provider/model if configured, else the
        // session's current one (`doc/phase2-plan.md` decision B).
        let (provider, model) = self.compaction.as_ref().map_or_else(
            || (&self.provider, self.config.model.clone()),
            |(p, m)| (p, m.clone()),
        );

        let request = ModelRequest {
            model,
            messages,
            tools: Vec::new(),
            temperature: 0.3,
            max_tokens: Some(1000),
        };

        let mut stream = provider.stream(request).await?;
        let mut summary = String::new();

        while let Some(event) = stream.next().await {
            if let StreamEvent::TextDelta { text, .. } = event? {
                summary.push_str(&text);
            }
        }

        let mut snapshot = system;
        snapshot.push(Message::User {
            content: format!("<conversation_summary>\n{summary}\n</conversation_summary>"),
        });
        snapshot.extend(tail.iter().cloned());

        Ok(Some(snapshot))
    }
}

impl AgentConfig {
    /// The token budget the context should stay under, or `None` if the context
    /// window is unknown. Delegates to [`effective_limit`].
    fn context_limit(&self) -> Option<u32> {
        effective_limit(
            self.context_window,
            self.compaction_threshold,
            self.max_tokens,
        )
    }
}

/// All mutable state threaded through one turn of the agent loop.
///
/// Constructed when a turn starts, dropped when it ends. Owns the turn-scoped
/// counters and output accumulation, borrows the session-scoped [`SessionRuntime`]
/// (context + plan) plus the shared resources the turn drives. Turn-invariant
/// deps stay on [`Agent`]; round-ephemeral values stay local to the round
/// (`doc/plan.md` §3).
struct TurnState<'a> {
    // turn-invariant deps (provider, tools, config)
    agent: &'a Agent,
    // session-scoped state, borrowed for the turn (context + plan live here)
    runtime: &'a mut SessionRuntime,
    // shared resources, borrowed for the turn's duration
    writer: &'a mut SessionWriter,
    sink: &'a mut dyn StreamSink,

    // turn identity
    turn_id: TurnId,

    // turn output accumulation — consumed by TurnOutcome on exit
    round: u32,
    answer: String,
    stop_reason: StopReason,
    accumulated_usage: Usage,

    // turn-scoped plan control counters, reset every turn
    gate_count: u8,
    step_stuck_rounds: HashMap<String, u32>,
}

/// What the completion gate decided when the model stopped calling tools.
enum Gate {
    /// Every step is terminal (or there is no plan) — exit cleanly.
    Done,
    /// Non-terminal steps remain; a reminder was injected — run another round.
    Continue,
    /// The model kept stopping with work outstanding — give up (retryable).
    GiveUp,
}

impl TurnState<'_> {
    /// Drive the turn to completion. Built by
    /// [`run_turn_with_sink`](Agent::run_turn_with_sink); the only entry point.
    ///
    /// Records `TurnEvent::Started`, then drives the round loop. A graceful stop
    /// (clean finish, max-rounds, plan stall) returns `Ok` from [`drive`]. A
    /// hard error (`AgentError::Model`/`Session`) bubbles out of `drive`; before
    /// propagating it we record a terminal trace (`ErrorEvent` + `Failed`) so no
    /// turn ends without a closing event (`doc/event-schema.md` §4).
    ///
    /// [`drive`]: Self::drive
    async fn run(&mut self, input: String) -> Result<TurnOutcome, AgentError> {
        self.writer.append(
            runtime_source(),
            EventPayload::Turn(TurnEvent::Started {
                turn_id: self.turn_id.clone(),
                input: Some(input.clone()),
            }),
            None,
            Some(self.turn_id.clone()),
        )?;

        // `turn:start` before hooks may block the turn before any model round
        // runs (`doc/hook-protocol.md` §3, §7). A block is a graceful stop: the
        // turn records `Failed { BlockedByHook }` and returns, no model call.
        if let BeforeEffect::Block { reason, by } = self
            .fire_before(
                HookPoint::TurnStart,
                serde_json::json!({ "input": input }),
                None,
            )
            .await?
        {
            self.runtime.push_message(Message::User { content: input });
            let outcome = self.fail(TurnFailureReason::BlockedByHook { by, reason }, false)?;
            self.fire_turn_end().await?;
            return Ok(outcome);
        }

        self.runtime.push_message(Message::User { content: input });

        match self.drive().await {
            Ok(outcome) => {
                // `turn:end` after hooks observe a settled turn (clean finish or
                // graceful stop). A hard error skips this — the turn did not
                // settle (`doc/hook-protocol.md` §3).
                self.fire_turn_end().await?;
                Ok(outcome)
            }
            Err(err) => {
                self.record_hard_failure(&err);
                Err(err)
            }
        }
    }

    /// Fire the `turn:end` after chain (observe only).
    async fn fire_turn_end(&mut self) -> Result<(), AgentError> {
        self.fire_after(
            HookPoint::TurnEnd,
            serde_json::json!({ "answer": self.answer }),
            None,
        )
        .await
    }

    /// The round loop. Returns `Ok` for every *graceful* outcome (clean finish,
    /// max-rounds safety net, plan stall); a hard provider/persistence fault
    /// short-circuits as `Err` and is given a terminal trace by [`run`](Self::run).
    async fn drive(&mut self) -> Result<TurnOutcome, AgentError> {
        while self.round < self.agent.config.max_rounds {
            let outcome = self.run_model_round().await?;
            let answer = assistant_text(&outcome.message);
            let tool_calls = assistant_tool_calls(&outcome.message);
            self.round += 1;
            self.stop_reason = outcome.stop_reason;
            self.accumulated_usage = add_usage(self.accumulated_usage, outcome.usage);
            if !answer.is_empty() {
                self.answer = answer;
            }
            self.runtime.push_message(outcome.message.clone());

            if tool_calls.is_empty() {
                match self.completion_gate()? {
                    Gate::Done => return self.finish(),
                    Gate::Continue => continue,
                    Gate::GiveUp => {
                        let incomplete = self.incomplete_step_count();
                        return self.fail(
                            TurnFailureReason::PlanStalled {
                                incomplete_steps: incomplete,
                            },
                            true,
                        );
                    }
                }
            }

            // A round counts as progress if at least one *leaf* tool call
            // succeeded with a non-error result. Plan ops and failed/errored
            // tools do not count, so a step that is genuinely working clears its
            // stuck counter while one that only spins keeps climbing toward the
            // threshold (`doc/plan.md` §7).
            let mut progressed = false;
            let mut touched: Vec<String> = Vec::new();
            for call in tool_calls {
                let event_id = outcome.tool_call_event_ids.get(&call.id).cloned();
                if let Some(path) = touched_path(&call) {
                    touched.push(path);
                }
                let (result, made_progress) = self.dispatch(&call, event_id).await?;
                progressed |= made_progress;
                self.runtime.push_message(result);
            }
            // Load any nested project-guidance file the touched paths sit under,
            // once per session, *after* the round's tool results are in place so
            // the assistant→tool message pairing the provider expects is intact
            // (`doc/agents-md.md`).
            self.load_project_guidance(&touched)?;
            self.check_stuck(progressed)?;
        }

        // The tool loop ran out of round budget. This is the absolute safety
        // net, not a crash: record why, then hand back the partial outcome so
        // the caller keeps whatever work already landed (`doc/plan.md` §7).
        self.fail(
            TurnFailureReason::MaxRoundsExceeded {
                max_rounds: self.agent.config.max_rounds,
            },
            false,
        )
    }

    /// Emit `TurnEvent::Completed`, flush the sink, and assemble the outcome.
    fn finish(&mut self) -> Result<TurnOutcome, AgentError> {
        self.writer.append(
            runtime_source(),
            EventPayload::Turn(TurnEvent::Completed {
                turn_id: self.turn_id.clone(),
            }),
            None,
            Some(self.turn_id.clone()),
        )?;
        self.sink.on_turn_end();
        Ok(self.outcome(None))
    }

    /// Record a `TurnEvent::Failed` carrying `reason`, flush the sink, and
    /// return the *partial* outcome flagged incomplete. A turn running out of
    /// budget or stalling is a graceful stop — its side effects stand — so the
    /// caller gets a `TurnOutcome`, never an `Err` (`doc/event-schema.md` §4).
    fn fail(
        &mut self,
        reason: TurnFailureReason,
        retryable: bool,
    ) -> Result<TurnOutcome, AgentError> {
        let last = EventId {
            session_id: self.writer.session_id().clone(),
            seq: self.writer.next_seq().saturating_sub(1),
        };
        self.writer.append(
            runtime_source(),
            EventPayload::Turn(TurnEvent::Failed {
                turn_id: self.turn_id.clone(),
                failed_at_event_id: last,
                retryable,
                reason: Some(reason.clone()),
            }),
            None,
            Some(self.turn_id.clone()),
        )?;
        self.sink.on_turn_end();
        Ok(self.outcome(Some(reason)))
    }

    /// Best-effort terminal trace for a hard error before it propagates: write
    /// an `ErrorEvent::Raised` carrying the detail, then a `TurnEvent::Failed`
    /// (`reason: None`) pointing at it. Every write is fire-and-forget — if the
    /// persistence layer is itself the fault, the closing writes will also fail,
    /// and we silently abandon them rather than mask the original error or loop
    /// (`doc/event-schema.md` §4). Does not touch the sink: the caller surfaces
    /// the `Err`, so there is no settled turn to signal.
    fn record_hard_failure(&mut self, err: &AgentError) {
        let detail = error_detail(err);
        let session_id = self.writer.session_id().clone();
        let error_seq = self.writer.append(
            runtime_source(),
            EventPayload::Error(ErrorEvent::Raised(detail.clone())),
            None,
            Some(self.turn_id.clone()),
        );
        // Point `failed_at` at the ErrorEvent we just wrote if it landed,
        // otherwise at the last event that did.
        let failed_at = EventId {
            session_id,
            seq: match error_seq {
                Ok(seq) => seq,
                Err(_) => self.writer.next_seq().saturating_sub(1),
            },
        };
        let _ = self.writer.append(
            runtime_source(),
            EventPayload::Turn(TurnEvent::Failed {
                turn_id: self.turn_id.clone(),
                failed_at_event_id: failed_at,
                retryable: detail.retryable,
                reason: None,
            }),
            None,
            Some(self.turn_id.clone()),
        );
    }

    /// Assemble the outcome from accumulated turn state.
    fn outcome(&mut self, incomplete: Option<TurnFailureReason>) -> TurnOutcome {
        TurnOutcome {
            answer: std::mem::take(&mut self.answer),
            stop_reason: self.stop_reason,
            rounds: self.round,
            usage: self.accumulated_usage,
            incomplete,
            context_tokens: self.runtime.ledger.running(),
            context_limit: self.agent.config.context_limit(),
        }
    }

    /// Count the plan steps still in a non-terminal state (for `PlanStalled`).
    fn incomplete_step_count(&self) -> u32 {
        let n = self
            .runtime
            .plan
            .iter()
            .filter(|s| !s.status.is_terminal())
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Decide whether the turn may exit now that the model stopped requesting
    /// tools. With no plan, or all steps terminal, the turn is done. Otherwise
    /// nudge the model (up to [`MAX_GATE`] times) to finish or mark the
    /// remaining steps (`doc/plan.md` §6).
    fn completion_gate(&mut self) -> Result<Gate, AgentError> {
        let incomplete = plan::render_incomplete(&self.runtime.plan);
        if incomplete.is_empty() {
            return Ok(Gate::Done);
        }
        if self.gate_count >= MAX_GATE {
            return Ok(Gate::GiveUp);
        }
        self.inject_runtime(format!(
            "<reminder>The following plan steps are not in a terminal state. \
             Continue working on them, or mark them cancelled/blocked with a \
             reason, then give your final answer:\n{incomplete}</reminder>"
        ))?;
        self.gate_count += 1;
        Ok(Gate::Continue)
    }

    /// At the end of a tool-bearing round, advance the stuck counters. If the
    /// round made progress (`progressed`) every in-progress step's counter is
    /// cleared — work happened, nothing is wedged. Otherwise each in-progress
    /// step's counter is bumped, and a step that spins past [`STUCK_THRESHOLD`]
    /// unproductive rounds gets a one-shot warning. Because progress resets the
    /// count, a step that stalls, recovers, then stalls again is warned each
    /// time it crosses the threshold afresh. Steps that left `in_progress` drop
    /// out of the map entirely (`doc/plan.md` §7).
    fn check_stuck(&mut self, progressed: bool) -> Result<(), AgentError> {
        let in_progress: Vec<(String, String)> = self
            .runtime
            .plan
            .iter()
            .filter(|s| s.status == StepStatus::InProgress)
            .map(|s| (s.id.clone(), s.content.clone()))
            .collect();
        let live: std::collections::HashSet<&String> =
            in_progress.iter().map(|(id, _)| id).collect();
        self.step_stuck_rounds.retain(|id, _| live.contains(id));

        if progressed {
            // Real work landed this round — no in-progress step is wedged.
            for (id, _) in &in_progress {
                self.step_stuck_rounds.insert(id.clone(), 0);
            }
            return Ok(());
        }

        let mut warnings = Vec::new();
        for (id, content) in in_progress {
            let count = self.step_stuck_rounds.entry(id).or_insert(0);
            *count += 1;
            if *count == STUCK_THRESHOLD {
                warnings.push(content);
            }
        }
        for content in warnings {
            self.inject_runtime(format!(
                "<reminder>Step \"{content}\" has been in progress for \
                 {STUCK_THRESHOLD} rounds without progress. Consider cancelling \
                 it or restructuring the plan.</reminder>"
            ))?;
        }
        Ok(())
    }

    /// Push a runtime reminder into the context (kept permanently, for prefix
    /// cache) and mirror it as an `InjectionEvent` (`doc/plan.md` §8).
    fn inject_runtime(&mut self, content: String) -> Result<(), AgentError> {
        let token_count = estimate_tokens(&content);
        self.writer.append(
            runtime_source(),
            EventPayload::Injection(InjectionEvent::ContextInjected {
                source: InjectionSource::Runtime,
                content: content.clone(),
                token_count,
            }),
            None,
            Some(self.turn_id.clone()),
        )?;
        self.runtime.push_message(Message::User { content });
        Ok(())
    }

    /// For each path a filesystem tool touched this round, find the nearest
    /// nested project-guidance file and inject it once per session. The dedup
    /// set is checked and updated synchronously, so several tool calls in one
    /// round that share a guidance directory load it a single time
    /// (`doc/agents-md.md`).
    fn load_project_guidance(&mut self, touched: &[String]) -> Result<(), AgentError> {
        let workspace = &self.agent.config.workspace;
        if workspace.as_os_str().is_empty() {
            return Ok(());
        }
        for path in touched {
            let Some(g) = crate::agents_md::discover_nearest(workspace, path) else {
                continue;
            };
            if !self.runtime.loaded_guidance.insert(g.label.clone()) {
                continue;
            }
            let content = crate::agents_md::wrap(&g.label, &g.body);
            let token_count = estimate_tokens(&content);
            self.writer.append(
                runtime_source(),
                EventPayload::Injection(InjectionEvent::ContextInjected {
                    source: InjectionSource::ProjectGuidance,
                    content: content.clone(),
                    token_count,
                }),
                None,
                Some(self.turn_id.clone()),
            )?;
            self.runtime.push_message(Message::User { content });
        }
        Ok(())
    }
    /// carry no parent and are attributed to a `hook`-named source so monitoring
    /// can route on them (`doc/hook-protocol.md` §11).
    fn record_hook_executions(&mut self, execs: &[HookExecution]) -> Result<(), AgentError> {
        for exec in execs {
            self.writer.append(
                EventSource {
                    kind: SourceKind::Runtime,
                    id: format!("hook:{}", exec.hook_name),
                },
                EventPayload::Hook(HookEvent::Executed {
                    hook_name: exec.hook_name.clone(),
                    hook_point: exec.hook_point.as_str().to_owned(),
                    outcome: exec.outcome.clone(),
                    duration_ms: exec.duration_ms,
                }),
                None,
                Some(self.turn_id.clone()),
            )?;
        }
        Ok(())
    }

    /// Run the before chain at `point`, persist its executions, and return the
    /// effect (proceed with possibly-modified payload, or block).
    async fn fire_before(
        &mut self,
        point: HookPoint,
        payload: serde_json::Value,
        tool_name: Option<String>,
    ) -> Result<BeforeEffect, AgentError> {
        if self.agent.hooks.is_empty() {
            return Ok(BeforeEffect::Proceed(payload));
        }
        let outcome = self.agent.hooks.run_before(point, payload, tool_name).await;
        self.record_hook_executions(&outcome.executions)?;
        Ok(outcome.effect)
    }

    /// Run the after chain at `point` and persist its executions. After hooks
    /// cannot affect the pipeline.
    async fn fire_after(
        &mut self,
        point: HookPoint,
        payload: serde_json::Value,
        tool_name: Option<String>,
    ) -> Result<(), AgentError> {
        if self.agent.hooks.is_empty() {
            return Ok(());
        }
        let execs = self.agent.hooks.run_after(point, payload, tool_name).await;
        self.record_hook_executions(&execs)
    }

    // __APPEND_MARKER__

    /// Run one model round: send the current context, persist the streamed
    /// response (forwarding deltas to the sink), and return the assembled
    /// assistant message.
    async fn run_model_round(&mut self) -> Result<collector::RoundOutcome, AgentError> {
        let request_id = ulid::Ulid::new().to_string();
        let tools = self.agent.tool_schemas();
        let source = self.agent.model_source();
        let config = &self.agent.config;

        let request = ModelRequest {
            model: config.model.clone(),
            messages: self.runtime.context.clone(),
            tools: tools.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
        };

        // Best pre-request estimate of the prefix we're about to send: the
        // ledger's running count, authoritative for everything measured so far
        // plus a heuristic tail (`doc/phase2-plan.md` Step 2).
        let input_tokens_estimate = self.runtime.ledger.running();

        self.writer.append(
            source.clone(),
            EventPayload::Model(ModelEvent::RequestStarted {
                request_id: request_id.clone(),
                provider: self.agent.provider.name().to_owned(),
                model: config.model.clone(),
                temperature: config.temperature,
                max_tokens: config.max_tokens,
                tool_schemas_count: u32::try_from(tools.len()).unwrap_or(u32::MAX),
                input_tokens_estimate,
            }),
            None,
            Some(self.turn_id.clone()),
        )?;

        let started = Instant::now();
        let stream = self.agent.provider.stream(request).await?;
        // Split-borrow disjoint fields: the `'static` stream holds no borrow of
        // `self`, so writer + sink can be borrowed mutably for collection.
        let outcome = collector::collect_round(
            self.writer,
            self.sink,
            stream,
            &source,
            &request_id,
            &self.turn_id,
        )
        .await?;

        // Calibrate the ledger against the provider's authoritative input-token
        // count *before* the reply / tool results are appended: `usage.input_tokens`
        // measures exactly the prefix we just sent. A provider that returns no
        // usage (`0`) leaves the ledger on its heuristic (decision A).
        self.runtime.ledger.calibrate(outcome.usage.input_tokens);

        self.writer.append(
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
            Some(self.turn_id.clone()),
        )?;

        Ok(outcome)
    }

    /// Route one tool call: the `plan` control tool is intercepted and applied
    /// to the runtime plan; every other name is a leaf tool dispatched to the
    /// registry. Both shapes emit the same `ToolEvent` bracket so replay and
    /// monitoring need no special case (`doc/plan.md` §5).
    ///
    /// The returned `bool` is whether this call counts as *progress* for stuck
    /// detection: `true` only for a leaf tool that returned a non-error result.
    /// Plan ops and failed/errored tools are `false` (`doc/plan.md` §7).
    async fn dispatch(
        &mut self,
        call: &ToolCall,
        tool_call_event_id: Option<EventId>,
    ) -> Result<(Message, bool), AgentError> {
        if call.name == PLAN_TOOL_NAME {
            self.dispatch_plan(call, tool_call_event_id)
                .map(|m| (m, false))
        } else {
            self.dispatch_tool(call, tool_call_event_id).await
        }
    }

    /// The model's tool-call event id, or a self reference if it was not
    /// captured (should not happen).
    fn parent_event_id(&self, captured: Option<EventId>) -> EventId {
        captured.unwrap_or_else(|| EventId {
            session_id: self.writer.session_id().clone(),
            seq: self.writer.next_seq(),
        })
    }

    /// Apply a `plan` op to the runtime plan and return the rendered plan as the
    /// tool result. Schema or id errors come back as an `is_error` result the
    /// model corrects next round — never a protocol failure.
    fn dispatch_plan(
        &mut self,
        call: &ToolCall,
        tool_call_event_id: Option<EventId>,
    ) -> Result<Message, AgentError> {
        let parent = self.parent_event_id(tool_call_event_id);
        let source = EventSource {
            kind: SourceKind::Tool,
            id: PLAN_TOOL_NAME.to_owned(),
        };
        let raw: serde_json::Value = if call.arguments.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null)
        };

        let started = Instant::now();
        self.writer.append(
            source.clone(),
            EventPayload::Tool(ToolEvent::Started {
                tool_call_event_id: parent.clone(),
                tool_name: PLAN_TOOL_NAME.to_owned(),
                source: ToolSource::Builtin,
                input: raw.clone(),
                working_dir: None,
            }),
            Some(parent.clone()),
            Some(self.turn_id.clone()),
        )?;

        // Decode then apply; either step can fail benignly.
        let result: Result<String, String> = serde_json::from_value::<PlanOp>(raw)
            .map_err(|e| format!("invalid plan op: {e}"))
            .and_then(|op| {
                apply_plan_op(&mut self.runtime.plan, op).map_err(|e: PlanError| e.to_string())
            })
            .map(|()| plan::render(&self.runtime.plan));

        let output = match &result {
            Ok(rendered) => ToolOutput {
                content: vec![Content::Text(rendered.clone())],
                is_error: false,
                error_code: None,
            },
            Err(message) => ToolOutput {
                content: vec![Content::Text(message.clone())],
                is_error: true,
                error_code: Some("invalid_plan_op".to_owned()),
            },
        };
        let text = render_output(&output);
        let bytes = output_bytes(&output);
        self.writer.append(
            source,
            EventPayload::Tool(ToolEvent::Completed {
                tool_call_event_id: parent.clone(),
                result: output,
                duration_ms: duration_ms(started.elapsed()),
                output_bytes: bytes,
                artifacts_created: Vec::new(),
            }),
            Some(parent),
            Some(self.turn_id.clone()),
        )?;
        Ok(Message::Tool {
            tool_call_id: call.id.clone(),
            content: text,
        })
    }

    /// Execute one leaf tool call, persisting `ToolEvent`s and returning the
    /// `Tool` message to feed back to the model, paired with whether it made
    /// progress (a non-error result; see [`dispatch`](Self::dispatch)).
    #[allow(clippy::too_many_lines)] // before/after hook brackets around one dispatch
    async fn dispatch_tool(
        &mut self,
        call: &ToolCall,
        tool_call_event_id: Option<EventId>,
    ) -> Result<(Message, bool), AgentError> {
        let parent = self.parent_event_id(tool_call_event_id);
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
                    return self
                        .fail_tool(
                            &source,
                            &parent,
                            call,
                            0,
                            "invalid_arguments",
                            &format!("tool arguments were not valid JSON: {e}"),
                        )
                        .map(|m| (m, false));
                }
            }
        };

        self.writer.append(
            source.clone(),
            EventPayload::Tool(ToolEvent::Started {
                tool_call_event_id: parent.clone(),
                tool_name: call.name.clone(),
                source: self.agent.tools.source_of(&call.name),
                input: args.clone(),
                working_dir: None,
            }),
            Some(parent.clone()),
            Some(self.turn_id.clone()),
        )?;

        // `tool:invoke:before` hooks may rewrite the input or block the call
        // (`doc/hook-protocol.md` §7). A block becomes the point-specific failure
        // event: a `ToolEvent::Failed` with code `blocked_by_hook`, which the
        // model sees as a tool result and can react to (§8).
        let args = match self
            .fire_before(HookPoint::ToolInvokeBefore, args, Some(call.name.clone()))
            .await?
        {
            BeforeEffect::Proceed(payload) => payload,
            BeforeEffect::Block { reason, by } => {
                let msg = self.fail_tool(
                    &source,
                    &parent,
                    call,
                    0,
                    "blocked_by_hook",
                    &format!("Blocked by hook [{by}]: {reason}"),
                )?;
                self.fire_after(
                    HookPoint::ToolInvokeAfter,
                    serde_json::json!({ "tool_name": call.name, "blocked": true }),
                    Some(call.name.clone()),
                )
                .await?;
                return Ok((msg, false));
            }
        };

        let Some(tool) = self.agent.tools.get(&call.name) else {
            let msg = self.fail_tool(
                &source,
                &parent,
                call,
                0,
                "unknown_tool",
                &format!("no such tool: {}", call.name),
            )?;
            return Ok((msg, false));
        };

        let started = Instant::now();
        let input = ToolInput {
            call_id: call.id.clone(),
            input: args,
            timeout: self.agent.config.tool_timeout,
        };
        let elapsed = |start: Instant| duration_ms(start.elapsed());

        let (message, made_progress) = match tool.invoke(input).await {
            Ok(output) => {
                // A successful invocation that reports a business-level error
                // (`is_error`) is not progress — the step is still spinning.
                let made_progress = !output.is_error;
                let text = render_output(&output);
                let output_bytes = output_bytes(&output);
                self.writer.append(
                    source,
                    EventPayload::Tool(ToolEvent::Completed {
                        tool_call_event_id: parent.clone(),
                        result: output,
                        duration_ms: elapsed(started),
                        output_bytes,
                        artifacts_created: Vec::new(),
                    }),
                    Some(parent),
                    Some(self.turn_id.clone()),
                )?;
                (
                    Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: text,
                    },
                    made_progress,
                )
            }
            Err(err) => {
                let (code, message) = tool_error_parts(&err);
                let msg =
                    self.fail_tool(&source, &parent, call, elapsed(started), code, &message)?;
                (msg, false)
            }
        };

        // `tool:invoke:after` hooks observe the settled call (`doc/hook-protocol.md`
        // §3). They cannot change the result already fed back to the model.
        self.fire_after(
            HookPoint::ToolInvokeAfter,
            serde_json::json!({ "tool_name": call.name }),
            Some(call.name.clone()),
        )
        .await?;
        Ok((message, made_progress))
    }

    /// Persist a `ToolEvent::Failed` and return the error as a `Tool` message so
    /// the model can react.
    fn fail_tool(
        &mut self,
        source: &EventSource,
        parent: &EventId,
        call: &ToolCall,
        duration_ms: u64,
        code: &str,
        message: &str,
    ) -> Result<Message, AgentError> {
        self.writer.append(
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
            Some(self.turn_id.clone()),
        )?;
        Ok(Message::Tool {
            tool_call_id: call.id.clone(),
            content: format!("[{code}] {message}"),
        })
    }
}

/// Runtime-sourced events (turn lifecycle).
fn runtime_source() -> EventSource {
    EventSource {
        kind: SourceKind::Runtime,
        id: "ominiforge".to_owned(),
    }
}

/// Split the context view into three parts for compaction: leading system
/// message(s), the middle to summarize, and a tail of `keep_last` user turns to
/// preserve verbatim (`doc/context-management.md` §4.4).
///
/// "System" is the leading run of `System` messages (the stable prefix). The
/// tail begins at the `keep_last`-th-from-last `User` message in the remainder,
/// so that many recent turns survive uncompressed; with `keep_last = None` (or
/// `0`) the whole remainder is summarized. If there are fewer than `keep_last`
/// user turns, nothing is summarized (the tail swallows everything).
fn split_for_compaction(
    context: &[Message],
    keep_last: Option<usize>,
) -> (Vec<Message>, Vec<Message>, Vec<Message>) {
    let system_end = context
        .iter()
        .position(|m| !matches!(m, Message::System { .. }))
        .unwrap_or(context.len());
    let (system, rest) = context.split_at(system_end);

    let keep = keep_last.unwrap_or(0);
    let tail_start = if keep == 0 {
        rest.len()
    } else {
        // Index of the keep-th-from-last User message in `rest`, or 0 if there
        // are fewer than `keep` user turns (keep everything).
        let user_positions: Vec<usize> = rest
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, Message::User { .. }))
            .map(|(i, _)| i)
            .collect();
        user_positions
            .len()
            .checked_sub(keep)
            .and_then(|idx| user_positions.get(idx).copied())
            .unwrap_or(0)
    };
    let (to_summarize, tail) = rest.split_at(tail_start);

    (system.to_vec(), to_summarize.to_vec(), tail.to_vec())
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

/// The workspace path a built-in filesystem tool call targets, for nested
/// project-guidance discovery. Only `read`/`write`/`edit` carry a `path`; other
/// tools (shell, MCP, the `plan` control tool) have no single path and return
/// `None` (`doc/agents-md.md`).
fn touched_path(call: &ToolCall) -> Option<String> {
    if !matches!(call.name.as_str(), "read" | "write" | "edit") {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()?
        .get("path")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// Accumulate per-round token usage into a turn total (saturating).
const fn add_usage(acc: Usage, round: Usage) -> Usage {
    Usage {
        input_tokens: acc.input_tokens.saturating_add(round.input_tokens),
        output_tokens: acc.output_tokens.saturating_add(round.output_tokens),
        cache_read_tokens: acc
            .cache_read_tokens
            .saturating_add(round.cache_read_tokens),
        cache_write_tokens: acc
            .cache_write_tokens
            .saturating_add(round.cache_write_tokens),
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

/// Build the [`ErrorDetail`] recorded for a hard turn failure. `code`,
/// `severity`, and `retryable` are derived from the error kind so a consumer can
/// route on them: transport hiccups and 429/5xx statuses are worth retrying;
/// auth, bad requests, decode faults, and any persistence error are not.
fn error_detail(err: &AgentError) -> ErrorDetail {
    let (code, severity, retryable) = match err {
        AgentError::Model(LlmError::Transport(_)) => {
            ("model_transport", ErrorSeverity::Error, true)
        }
        AgentError::Model(LlmError::Status { status, .. }) => {
            let retryable = *status == 429 || (500..600).contains(status);
            ("model_status", ErrorSeverity::Error, retryable)
        }
        AgentError::Model(LlmError::Decode(_)) => ("model_decode", ErrorSeverity::Error, false),
        AgentError::Model(LlmError::Auth(_)) => ("model_auth", ErrorSeverity::Fatal, false),
        AgentError::Session(_) => ("session", ErrorSeverity::Fatal, false),
    };
    ErrorDetail {
        code: code.to_owned(),
        message: err.to_string(),
        severity,
        retryable,
        source_event_id: None,
        provider_raw: None,
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

    /// A provider whose `stream()` always fails, to drive the hard-error path.
    struct FailingProvider;

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        #[allow(clippy::unnecessary_literal_bound)]
        fn name(&self) -> &str {
            "failing"
        }

        async fn stream(&self, _request: ModelRequest) -> Result<EventStream, LlmError> {
            Err(LlmError::Transport("connection refused".to_owned()))
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

    /// Like [`text_round`] but the `Completed` carries a provider `input_tokens`
    /// count, so the round calibrates the context ledger (the authoritative path).
    fn text_round_with_input_tokens(text: &str, input_tokens: u32) -> Vec<StreamEvent> {
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
                usage: Usage {
                    input_tokens,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            },
        ]
    }

    /// A round that calls the `plan` control tool with `args`.
    fn plan_round(call_id: &str, args: &str) -> Vec<StreamEvent> {
        tool_call_round(call_id, "plan", args)
    }

    /// An agent with no leaf tools (the `plan` control tool is always present).
    fn planning_agent(provider: Arc<ScriptedProvider>) -> Agent {
        Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        )
    }

    fn injection_count(events: &[CoreEvent]) -> usize {
        events
            .iter()
            .filter(|e| {
                matches!(
                    &e.payload,
                    EventPayload::Injection(InjectionEvent::ContextInjected {
                        source: InjectionSource::Runtime,
                        ..
                    })
                )
            })
            .count()
    }

    fn project_guidance_count(events: &[CoreEvent]) -> usize {
        events
            .iter()
            .filter(|e| {
                matches!(
                    &e.payload,
                    EventPayload::Injection(InjectionEvent::ContextInjected {
                        source: InjectionSource::ProjectGuidance,
                        ..
                    })
                )
            })
            .count()
    }

    /// A single model round that issues several tool calls (each its own block).
    fn multi_tool_call_round(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for (i, (id, name, args)) in calls.iter().enumerate() {
            let index = u32::try_from(i).unwrap();
            events.push(StreamEvent::BlockStart {
                index,
                block_type: ContentBlockType::ToolCall {
                    id: (*id).to_owned(),
                    name: (*name).to_owned(),
                },
            });
            events.push(StreamEvent::ToolCallDelta {
                index,
                json_delta: (*args).to_owned(),
            });
            events.push(StreamEvent::BlockStop { index });
        }
        events.push(StreamEvent::Completed {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        events
    }

    /// The `plan` control tool is dispatched like a leaf tool — same
    /// `ToolEvent` bracket — but applies to `runtime.plan`, and a turn does not
    /// finish until every step is terminal (the completion gate).
    #[tokio::test]
    async fn plan_drives_a_multi_round_turn_to_completion() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            plan_round(
                "c1",
                r#"{"op":"init","steps":[{"content":"step one"},{"content":"step two"}]}"#,
            ),
            plan_round("c2", r#"{"op":"start","id":"1"}"#),
            plan_round("c3", r#"{"op":"complete","id":"1"}"#),
            plan_round("c4", r#"{"op":"complete","id":"2"}"#),
            text_round("all done"),
        ]));
        let agent = planning_agent(provider);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "do two things".to_owned())
            .await
            .unwrap();
        drop(writer);

        assert_eq!(outcome.answer, "all done");
        assert_eq!(outcome.rounds, 5);
        // Plan reached an all-terminal state and persists in the runtime.
        assert_eq!(runtime.plan.len(), 2);
        assert!(runtime.plan.iter().all(|s| s.status.is_terminal()));

        // Plan ops are recorded as ordinary builtin ToolEvents (same bracket as
        // leaf tools), so replay/monitor need no special case.
        let events = store.read_events(&sid).unwrap();
        let plan_completions = events
            .iter()
            .filter(|e| {
                matches!(&e.payload, EventPayload::Tool(ToolEvent::Completed { .. }))
                    && e.source.kind == SourceKind::Tool
                    && e.source.id == "plan"
            })
            .count();
        assert_eq!(plan_completions, 4);
        // No gate nudge was needed — the model finished the plan on its own.
        assert_eq!(injection_count(&events), 0);
        assert!(seqs_are_contiguous(&events));
    }

    /// When the model stops with a non-terminal step, the completion gate
    /// injects a reminder and runs another round instead of exiting.
    #[tokio::test]
    async fn completion_gate_nudges_then_lets_turn_finish() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            plan_round("c1", r#"{"op":"init","steps":[{"content":"only step"}]}"#),
            // Model tries to stop with the step still pending — gate nudges.
            text_round("I think I'm done"),
            // After the nudge it finishes the step, then answers.
            plan_round("c2", r#"{"op":"complete","id":"1"}"#),
            text_round("actually done now"),
        ]));
        let agent = planning_agent(provider);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "one step".to_owned())
            .await
            .unwrap();
        drop(writer);

        assert_eq!(outcome.answer, "actually done now");
        assert_eq!(outcome.rounds, 4);
        let events = store.read_events(&sid).unwrap();
        // Exactly one runtime reminder was injected, and it persists in context.
        assert_eq!(injection_count(&events), 1);
        assert!(runtime.context.iter().any(|m| matches!(
            m,
            Message::User { content } if content.contains("not in a terminal state")
        )));
    }

    /// If the model keeps stopping with work outstanding, the gate gives up
    /// after `MAX_GATE` nudges. This is a graceful, *retryable* stop: the turn
    /// returns `Ok` flagged `PlanStalled`, and the event log records the reason.
    #[tokio::test]
    async fn completion_gate_gives_up_after_max_nudges() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            plan_round("c1", r#"{"op":"init","steps":[{"content":"never done"}]}"#),
            text_round("stopping 1"),
            text_round("stopping 2"),
            text_round("stopping 3"),
        ]));
        let agent = planning_agent(provider);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "loop".to_owned())
            .await
            .unwrap();
        drop(writer);

        // Not an error: a partial outcome flagged stalled (one step outstanding).
        assert_eq!(
            outcome.incomplete,
            Some(TurnFailureReason::PlanStalled {
                incomplete_steps: 1
            })
        );
        let events = store.read_events(&sid).unwrap();
        // MAX_GATE nudges injected, then a retryable Failed turn carrying the
        // structured reason so replay can explain the stop.
        assert_eq!(injection_count(&events), usize::from(MAX_GATE));
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::Turn(TurnEvent::Failed {
                retryable: true,
                reason: Some(TurnFailureReason::PlanStalled {
                    incomplete_steps: 1
                }),
                ..
            })
        )));
    }

    /// Running out of round budget is the absolute safety net, not a crash:
    /// the turn returns `Ok` flagged `MaxRoundsExceeded`, the work it did
    /// stands, and the event log records the reason (non-retryable).
    #[tokio::test]
    async fn max_rounds_returns_incomplete_outcome_not_error() {
        let dir = tempfile::tempdir().unwrap();
        // Every round calls a tool, so the loop never settles on its own and
        // must hit the cap. `start id=1` is idempotent and harmless.
        let rounds = std::iter::once(plan_round(
            "c0",
            r#"{"op":"init","steps":[{"content":"endless"}]}"#,
        ))
        .chain((0..10).map(|i| plan_round(&format!("s{i}"), r#"{"op":"start","id":"1"}"#)))
        .collect();
        let provider = Arc::new(ScriptedProvider::new(rounds));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                max_rounds: 4,
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "go forever".to_owned())
            .await
            .unwrap();
        drop(writer);

        assert_eq!(outcome.rounds, 4);
        assert_eq!(
            outcome.incomplete,
            Some(TurnFailureReason::MaxRoundsExceeded { max_rounds: 4 })
        );
        // The reason is in the log, not just the error string — replayable.
        let events = store.read_events(&sid).unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::Turn(TurnEvent::Failed {
                retryable: false,
                reason: Some(TurnFailureReason::MaxRoundsExceeded { max_rounds: 4 }),
                ..
            })
        )));
        // No clean Completed event was written.
        assert_eq!(turn_completed_count(&events), 0);
    }

    /// A `cancel`/`block` op missing its required `reason` fails to decode and
    /// comes back as an `is_error` tool result (not a protocol failure); the
    /// model recovers on the next round.
    #[tokio::test]
    async fn invalid_plan_op_returns_error_result_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            plan_round("c1", r#"{"op":"init","steps":[{"content":"a step"}]}"#),
            // cancel without a reason — rejected at decode.
            plan_round("c2", r#"{"op":"cancel","id":"1"}"#),
            // recovers with a proper reason.
            plan_round("c3", r#"{"op":"cancel","id":"1","reason":"no such tool"}"#),
            text_round("cancelled it"),
        ]));
        let agent = planning_agent(provider);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "cancel a step".to_owned())
            .await
            .unwrap();
        drop(writer);

        assert_eq!(outcome.answer, "cancelled it");
        assert_eq!(runtime.plan[0].status, StepStatus::Cancelled);

        // One plan ToolEvent::Completed carries an is_error result.
        let events = store.read_events(&sid).unwrap();
        let had_error_result = events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::Tool(ToolEvent::Completed { result, .. })
                    if result.is_error && e.source.id == "plan"
            )
        });
        assert!(had_error_result, "invalid plan op should yield is_error");
    }

    /// A `blocked` step is terminal: it does not trip the completion gate, so
    /// the turn ends cleanly and the blocked state (with reason) persists in the
    /// `SessionRuntime` for the next turn to pick up.
    #[tokio::test]
    async fn blocked_step_lets_turn_finish_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            plan_round("c1", r#"{"op":"init","steps":[{"content":"needs a key"}]}"#),
            plan_round(
                "c2",
                r#"{"op":"block","id":"1","reason":"set OPENAI_API_KEY"}"#,
            ),
            text_round("blocked on your input"),
        ]));
        let agent = planning_agent(provider);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "do the thing".to_owned())
            .await
            .unwrap();
        drop(writer);

        // Clean finish despite an unfinished-but-blocked step.
        assert_eq!(outcome.answer, "blocked on your input");
        assert_eq!(runtime.plan.len(), 1);
        assert_eq!(runtime.plan[0].status, StepStatus::Blocked);
        assert_eq!(
            runtime.plan[0].reason.as_deref(),
            Some("set OPENAI_API_KEY")
        );
    }

    /// A step stuck `in_progress` for `STUCK_THRESHOLD` tool-bearing rounds
    /// triggers a one-shot stuck reminder.
    #[tokio::test]
    async fn stuck_step_triggers_a_reminder() {
        let dir = tempfile::tempdir().unwrap();
        // init, then keep re-issuing a (no-op) tool-bearing round so the step
        // stays in_progress across rounds; check_stuck runs at the end of each
        // tool-bearing round, and `start id=1` is idempotent.
        let mut rounds = vec![
            plan_round("c0", r#"{"op":"init","steps":[{"content":"long step"}]}"#),
            plan_round("c1", r#"{"op":"start","id":"1"}"#),
        ];
        for i in 0..STUCK_THRESHOLD {
            rounds.push(plan_round(&format!("s{i}"), r#"{"op":"start","id":"1"}"#));
        }
        rounds.push(plan_round("done", r#"{"op":"complete","id":"1"}"#));
        rounds.push(text_round("finished"));
        let provider = Arc::new(ScriptedProvider::new(rounds));
        let agent = planning_agent(provider);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        agent
            .run_turn(&mut writer, &mut runtime, "slow task".to_owned())
            .await
            .unwrap();
        drop(writer);

        let events = store.read_events(&sid).unwrap();
        let stuck_warning = events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::Injection(InjectionEvent::ContextInjected { source: InjectionSource::Runtime, content, .. })
                if content.contains("without progress")
        ));
        assert!(stuck_warning, "a stuck-step reminder should be injected");
    }

    /// A step that stays `in_progress` across many rounds but makes real
    /// progress each round (a leaf tool succeeds) must NOT trip the stuck
    /// warning: progress clears the counter, so wall-clock rounds alone never
    /// reach the threshold. This is the false-positive the counter reset fixes.
    #[tokio::test]
    async fn productive_step_does_not_trigger_stuck_reminder() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("note.txt"), "content").unwrap();

        // init + start, then well past STUCK_THRESHOLD rounds that each succeed
        // at a real `read` while step 1 stays in_progress, then complete + answer.
        let mut rounds = vec![
            plan_round(
                "c0",
                r#"{"op":"init","steps":[{"content":"long but productive"}]}"#,
            ),
            plan_round("c1", r#"{"op":"start","id":"1"}"#),
        ];
        for i in 0..(STUCK_THRESHOLD + 2) {
            rounds.push(tool_call_round(
                &format!("r{i}"),
                "read",
                r#"{"path":"note.txt"}"#,
            ));
        }
        rounds.push(plan_round("done", r#"{"op":"complete","id":"1"}"#));
        rounds.push(text_round("finished"));
        let provider = Arc::new(ScriptedProvider::new(rounds));

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
        let mut runtime = SessionRuntime::default();

        agent
            .run_turn(&mut writer, &mut runtime, "productive task".to_owned())
            .await
            .unwrap();
        drop(writer);

        let events = store.read_events(&sid).unwrap();
        let stuck_warning = events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::Injection(InjectionEvent::ContextInjected { source: InjectionSource::Runtime, content, .. })
                if content.contains("without progress")
        ));
        assert!(
            !stuck_warning,
            "a step making progress every round must not be flagged stuck"
        );
    }

    /// A nested `AGENTS.md` is injected once per session: several tool calls in
    /// one round that touch its subtree load it a single time, a later round
    /// touching the same subtree does not reload it, and a different subtree
    /// loads its own. This is the dedup guarantee (`doc/agents-md.md`).
    #[tokio::test]
    async fn nested_project_guidance_injected_once_per_dir() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(workspace.join("a")).unwrap();
        std::fs::create_dir_all(workspace.join("b")).unwrap();
        std::fs::write(workspace.join("a/AGENTS.md"), "a-guidance").unwrap();
        std::fs::write(workspace.join("b/AGENTS.md"), "b-guidance").unwrap();
        std::fs::write(workspace.join("a/one.txt"), "1").unwrap();
        std::fs::write(workspace.join("a/two.txt"), "2").unwrap();
        std::fs::write(workspace.join("b/three.txt"), "3").unwrap();

        let provider = Arc::new(ScriptedProvider::new(vec![
            // Round 1: two reads under a/ in ONE round → guidance a loaded once.
            multi_tool_call_round(&[
                ("r1", "read", r#"{"path":"a/one.txt"}"#),
                ("r2", "read", r#"{"path":"a/two.txt"}"#),
            ]),
            // Round 2: another read under a/ → already loaded, no new injection.
            tool_call_round("r3", "read", r#"{"path":"a/one.txt"}"#),
            // Round 3: a read under b/ → loads guidance b.
            tool_call_round("r4", "read", r#"{"path":"b/three.txt"}"#),
            text_round("done"),
        ]));

        let mut tools = ToolRegistry::new();
        crate::tool::register_builtin(&mut tools, workspace.clone());
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                workspace: workspace.clone(),
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store
            .create_new(None, None, vec!["read".to_owned()])
            .unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        agent
            .run_turn(&mut writer, &mut runtime, "touch files".to_owned())
            .await
            .unwrap();
        drop(writer);

        let events = store.read_events(&sid).unwrap();
        // a loaded once (despite 3 touches across 2 rounds), b once → 2 total.
        assert_eq!(project_guidance_count(&events), 2);
        let sep = std::path::MAIN_SEPARATOR;
        assert!(
            runtime
                .loaded_guidance
                .contains(&format!("a{sep}AGENTS.md"))
        );
        assert!(
            runtime
                .loaded_guidance
                .contains(&format!("b{sep}AGENTS.md"))
        );
        // The guidance bodies reached the model's context, wrapped + attributed.
        assert!(runtime.context.iter().any(|m| matches!(
            m,
            Message::User { content }
                if content.contains("a-guidance") && content.contains("project-guidance")
        )));
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
        let mut runtime = SessionRuntime::new(vec![Message::System {
            content: "be helpful".to_owned(),
        }]);

        let outcome = agent
            .run_turn(
                &mut writer,
                &mut runtime,
                "what does note.txt say?".to_owned(),
            )
            .await
            .unwrap();
        drop(writer);

        assert_eq!(outcome.rounds, 2);
        assert_eq!(outcome.stop_reason, StopReason::EndTurn);
        assert_eq!(outcome.answer, "the file says: secret answer");

        // The tool result was fed back into the context for round 2.
        assert!(matches!(
            runtime.context.last(),
            Some(Message::Assistant { .. })
        ));
        assert!(runtime.context.iter().any(|m| matches!(
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
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "do a thing".to_owned())
            .await
            .unwrap();

        assert_eq!(outcome.answer, "recovered");
        assert!(runtime.context.iter().any(|m| matches!(
            m,
            Message::Tool { content, .. } if content.contains("unknown_tool")
        )));
    }

    /// A hard provider error aborts the turn as `Err`, but still leaves a
    /// terminal trace: an `ErrorEvent::Raised` carrying the detail, then a
    /// `TurnEvent::Failed { reason: None }` pointing at it. This is what lets
    /// replay/monitor see *every* turn termination, not just graceful ones.
    #[tokio::test]
    async fn hard_model_error_records_failed_event_then_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::new(
            Arc::new(FailingProvider),
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        let result = agent
            .run_turn(&mut writer, &mut runtime, "trigger a fault".to_owned())
            .await;
        drop(writer);

        // The hard error still surfaces to the caller as `Err`.
        assert!(matches!(result, Err(AgentError::Model(_))));

        let events = store.read_events(&sid).unwrap();
        // The error detail was recorded as its own event...
        let error_seq = events.iter().find_map(|e| match &e.payload {
            EventPayload::Error(ErrorEvent::Raised(detail)) => {
                assert_eq!(detail.code, "model_transport");
                assert!(detail.retryable, "transport faults are retryable");
                Some(e.seq)
            }
            _ => None,
        });
        let error_seq = error_seq.expect("a hard error should record an ErrorEvent::Raised");

        // ...and a Failed turn (no graceful reason) points back at it.
        let failed = events.iter().find_map(|e| match &e.payload {
            EventPayload::Turn(TurnEvent::Failed {
                failed_at_event_id,
                reason,
                retryable,
                ..
            }) => Some((failed_at_event_id.clone(), reason.clone(), *retryable)),
            _ => None,
        });
        let (failed_at, reason, retryable) =
            failed.expect("a hard error should record a TurnEvent::Failed");
        assert_eq!(reason, None, "hard errors carry no TurnFailureReason");
        assert!(retryable);
        assert_eq!(
            failed_at.seq, error_seq,
            "Failed must point at the ErrorEvent it paired with"
        );
        // No clean Completed was written.
        assert_eq!(turn_completed_count(&events), 0);
        assert!(seqs_are_contiguous(&events));
    }

    /// When the provider returns a real `input_tokens`, the ledger snaps to it
    /// (authoritative), and the turn outcome reports it against the configured
    /// limit. Heuristic estimation of the seed is overwritten by the real count.
    #[tokio::test]
    async fn usage_calibrates_context_ledger_and_outcome() {
        let dir = tempfile::tempdir().unwrap();
        // One round that reports the prefix was 5000 tokens.
        let provider = Arc::new(ScriptedProvider::new(vec![text_round_with_input_tokens(
            "hi", 5000,
        )]));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                context_window: 10_000,
                compaction_threshold: 0.8,
                max_tokens: Some(2000),
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let mut runtime = SessionRuntime::new(vec![Message::System {
            content: "be helpful".to_owned(),
        }]);

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "hello".to_owned())
            .await
            .unwrap();
        drop(writer);

        // Ledger snapped to the authoritative 5000 — the reply ("hi", 2 bytes)
        // adds a negligible heuristic tail (0 tokens), so the running count is 5000.
        assert_eq!(outcome.context_tokens, 5000);
        // effective_limit = 0.8 × 10_000 − 2000 = 6000.
        assert_eq!(outcome.context_limit, Some(6000));
        // 5000 < 6000 → under threshold.
        assert!(outcome.context_tokens < outcome.context_limit.unwrap());
        // And the runtime ledger persists the calibration for the next turn.
        assert_eq!(runtime.ledger.running(), 5000);
    }

    /// A provider that never returns usage (`input_tokens == 0`) leaves the
    /// ledger on the pure heuristic: the running count equals `bytes / 4` over
    /// the whole context, and no authoritative value ever lands. This is the
    /// OpenAI-compatible-endpoint fallback (decision A).
    #[tokio::test]
    async fn missing_usage_falls_back_to_heuristic_estimate() {
        let dir = tempfile::tempdir().unwrap();
        // `text_round` carries Usage::default() → input_tokens == 0 (no usage).
        let provider = Arc::new(ScriptedProvider::new(vec![text_round("ok")]));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                context_window: 10_000,
                ..AgentConfig::default()
            },
        );

        let system = "s".repeat(40); // 40 bytes
        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let mut runtime = SessionRuntime::new(vec![Message::System {
            content: system.clone(),
        }]);

        let user = "u".repeat(80); // 80 bytes
        let outcome = agent
            .run_turn(&mut writer, &mut runtime, user.clone())
            .await
            .unwrap();
        drop(writer);

        // Pure heuristic: system(40) + user(80) + assistant "ok"(2) = 122 bytes
        // → 122 / 4 = 30 tokens. No calibration happened (no usage).
        let expected = (system.len() + user.len() + "ok".len()) / 4;
        assert_eq!(outcome.context_tokens as usize, expected);
        assert_eq!(runtime.ledger.running() as usize, expected);
    }

    /// `split_for_compaction` separates the leading system run, the middle to
    /// summarize, and a verbatim tail. With `keep_last = None` everything after
    /// the system prefix is summarized.
    #[test]
    fn split_compaction_no_keep_summarizes_everything_after_system() {
        let ctx = vec![
            Message::System {
                content: "sys".to_owned(),
            },
            Message::User {
                content: "u1".to_owned(),
            },
            Message::Assistant {
                content: Some("a1".to_owned()),
                tool_calls: vec![],
            },
            Message::User {
                content: "u2".to_owned(),
            },
        ];
        let (system, mid, tail) = split_for_compaction(&ctx, None);
        assert_eq!(
            system,
            vec![Message::System {
                content: "sys".to_owned()
            }]
        );
        assert_eq!(mid.len(), 3);
        assert!(tail.is_empty());
    }

    /// With `keep_last = 1`, the last user turn (and anything after it) is kept
    /// verbatim while everything before it is summarized.
    #[test]
    fn split_compaction_keeps_last_user_turn_verbatim() {
        let ctx = vec![
            Message::System {
                content: "sys".to_owned(),
            },
            Message::User {
                content: "u1".to_owned(),
            },
            Message::Assistant {
                content: Some("a1".to_owned()),
                tool_calls: vec![],
            },
            Message::User {
                content: "u2".to_owned(),
            },
            Message::Assistant {
                content: Some("a2".to_owned()),
                tool_calls: vec![],
            },
        ];
        let (system, mid, tail) = split_for_compaction(&ctx, Some(1));
        assert_eq!(system.len(), 1);
        // u1, a1 get summarized.
        assert_eq!(
            mid,
            vec![
                Message::User {
                    content: "u1".to_owned()
                },
                Message::Assistant {
                    content: Some("a1".to_owned()),
                    tool_calls: vec![]
                },
            ]
        );
        // u2 onward kept verbatim.
        assert_eq!(
            tail,
            vec![
                Message::User {
                    content: "u2".to_owned()
                },
                Message::Assistant {
                    content: Some("a2".to_owned()),
                    tool_calls: vec![]
                },
            ]
        );
    }

    /// When `keep_last` exceeds the available user turns, nothing is summarized:
    /// the tail swallows the whole remainder (compaction is a no-op).
    #[test]
    fn split_compaction_keep_more_than_available_summarizes_nothing() {
        let ctx = vec![
            Message::System {
                content: "sys".to_owned(),
            },
            Message::User {
                content: "u1".to_owned(),
            },
        ];
        let (_system, mid, tail) = split_for_compaction(&ctx, Some(5));
        assert!(mid.is_empty());
        assert_eq!(tail.len(), 1);
    }

    /// `Agent::compact` calls the model, wraps its reply in a summary message,
    /// and returns a snapshot of system + summary (+ verbatim tail). The middle
    /// of the conversation is replaced by the summary — that's the compression.
    #[tokio::test]
    async fn compact_produces_snapshot_with_summary() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_round("CONDENSED SUMMARY")]));
        let agent = planning_agent(provider);

        let mut runtime = SessionRuntime::new(vec![Message::System {
            content: "be helpful".to_owned(),
        }]);
        runtime.context.push(Message::User {
            content: "old turn 1".to_owned(),
        });
        runtime.context.push(Message::Assistant {
            content: Some("old reply 1".to_owned()),
            tool_calls: vec![],
        });

        let snapshot = agent.compact(&runtime, None).await.unwrap().unwrap();

        // system preserved at front.
        assert_eq!(
            snapshot[0],
            Message::System {
                content: "be helpful".to_owned()
            }
        );
        // the model's reply is wrapped in a summary marker.
        assert!(matches!(
            &snapshot[1],
            Message::User { content } if content.contains("CONDENSED SUMMARY")
                && content.contains("conversation_summary")
        ));
        // the original turns are gone — replaced by the summary.
        assert_eq!(snapshot.len(), 2);
    }

    /// Compacting a context with nothing past the system prefix is a no-op
    /// (`None`): there is nothing to summarize.
    #[tokio::test]
    async fn compact_with_only_system_is_noop() {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let agent = planning_agent(provider);
        let runtime = SessionRuntime::new(vec![Message::System {
            content: "be helpful".to_owned(),
        }]);
        assert!(agent.compact(&runtime, None).await.unwrap().is_none());
    }

    /// With a dedicated compaction model set, the summary is produced by *that*
    /// provider, not the main one. The main provider here would panic if called
    /// (empty script), proving compaction routed to the dedicated provider.
    #[tokio::test]
    async fn compact_uses_dedicated_compaction_provider() {
        // Main provider is never expected to stream during compaction.
        let main = Arc::new(ScriptedProvider::new(vec![]));
        // Dedicated compaction provider yields a recognizable summary.
        let compaction = Arc::new(ScriptedProvider::new(vec![text_round("DEDICATED SUMMARY")]));
        let agent = planning_agent(main).with_compaction_model(compaction, "cheap".to_owned());

        let mut runtime = SessionRuntime::new(vec![Message::System {
            content: "be helpful".to_owned(),
        }]);
        runtime.context.push(Message::User {
            content: "old".to_owned(),
        });

        let snapshot = agent.compact(&runtime, None).await.unwrap().unwrap();
        assert!(matches!(
            &snapshot[1],
            Message::User { content } if content.contains("DEDICATED SUMMARY")
        ));
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

    // ── Hook wiring ───────────────────────────────────────────────────────

    use crate::core::payload::{HookEvent, HookOutcome};
    use crate::hook::{
        AfterHook, BeforeDecision, BeforeHook, HookPoint, HookRegistry, HookRequest,
    };

    /// A before hook that always returns the same decision, for asserting the
    /// agent's response to block/modify/pass.
    struct FixedBefore {
        name: &'static str,
        decision: BeforeDecision,
    }

    #[async_trait::async_trait]
    impl BeforeHook for FixedBefore {
        fn name(&self) -> &str {
            self.name
        }
        async fn intercept(&self, _req: &HookRequest) -> BeforeDecision {
            self.decision.clone()
        }
    }

    /// An after hook that always observes successfully.
    struct NoopAfter {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl AfterHook for NoopAfter {
        fn name(&self) -> &str {
            self.name
        }
        async fn observe(&self, _req: &HookRequest) -> Result<(), String> {
            Ok(())
        }
    }

    fn hook_events(events: &[CoreEvent]) -> Vec<(&str, &HookOutcome)> {
        events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Hook(HookEvent::Executed {
                    hook_point,
                    outcome,
                    ..
                }) => Some((hook_point.as_str(), outcome)),
                _ => None,
            })
            .collect()
    }

    /// A `turn:start` before hook that blocks stops the turn before any model
    /// round runs: no `ModelEvent`, a `Failed { BlockedByHook }`, and a logged
    /// `HookEvent` with a `Blocked` outcome (`doc/hook-protocol.md` §3, §7, §11).
    #[tokio::test]
    async fn turn_start_block_stops_turn_before_model_call() {
        let dir = tempfile::tempdir().unwrap();
        // Scripted with zero rounds: if the loop calls the provider, the test
        // panics ("called more times than scripted"), proving no model round ran.
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let mut hooks = HookRegistry::new();
        hooks.register_before(
            HookPoint::TurnStart,
            Arc::new(FixedBefore {
                name: "gate",
                decision: BeforeDecision::Block {
                    reason: "no".to_owned(),
                },
            }),
        );
        let agent = planning_agent(provider).with_hooks(hooks);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        let outcome = agent
            .run_turn(&mut writer, &mut runtime, "blocked input".to_owned())
            .await
            .unwrap();
        drop(writer);

        assert!(matches!(
            outcome.incomplete,
            Some(TurnFailureReason::BlockedByHook { .. })
        ));
        assert_eq!(outcome.rounds, 0, "no model round ran");

        let events = store.read_events(&sid).unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::Model(_))),
            "blocked turn made no model request"
        );
        let hooks = hook_events(&events);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].0, "turn:start");
        assert!(matches!(hooks[0].1, HookOutcome::Blocked { .. }));
        assert!(seqs_are_contiguous(&events));
    }

    /// A clean turn fires `turn:end` after hooks, recorded as an observed
    /// `HookEvent` (the `doc/todo.md` Phase 4 acceptance check).
    #[tokio::test]
    async fn turn_end_after_hook_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![text_round("done")]));
        let mut hooks = HookRegistry::new();
        hooks.register_after(HookPoint::TurnEnd, Arc::new(NoopAfter { name: "notify" }));
        let agent = planning_agent(provider).with_hooks(hooks);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        agent
            .run_turn(&mut writer, &mut runtime, "hi".to_owned())
            .await
            .unwrap();
        drop(writer);

        let events = store.read_events(&sid).unwrap();
        let hooks = hook_events(&events);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].0, "turn:end");
        assert_eq!(hooks[0].1, &HookOutcome::Observed);
        assert!(seqs_are_contiguous(&events));
    }

    /// A `tool:invoke:before` block turns into a `ToolEvent::Failed` with code
    /// `blocked_by_hook`, the tool never runs, and both the before block and the
    /// after observe are logged (`doc/hook-protocol.md` §8).
    #[tokio::test]
    async fn tool_invoke_before_block_becomes_tool_failure() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        // The model asks to write a file; the hook blocks it; then it gives up.
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_call_round("c1", "write", r#"{"path":"x.txt","content":"hi"}"#),
            text_round("ok, blocked"),
        ]));
        let mut tools = ToolRegistry::new();
        crate::tool::register_builtin(&mut tools, workspace);
        let mut hooks = HookRegistry::new();
        hooks.register_before(
            HookPoint::ToolInvokeBefore,
            Arc::new(FixedBefore {
                name: "deny-write",
                decision: BeforeDecision::Block {
                    reason: "writes disabled".to_owned(),
                },
            }),
        );
        hooks.register_after(
            HookPoint::ToolInvokeAfter,
            Arc::new(NoopAfter { name: "audit" }),
        );
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        )
        .with_hooks(hooks);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store
            .create_new(None, None, vec!["write".to_owned()])
            .unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();

        agent
            .run_turn(&mut writer, &mut runtime, "write a file".to_owned())
            .await
            .unwrap();
        drop(writer);

        let events = store.read_events(&sid).unwrap();
        // The tool call failed with the hook-block code; the file was never written.
        let blocked = events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::Tool(ToolEvent::Failed { error, .. }) if error.code == "blocked_by_hook"
        ));
        assert!(blocked, "blocked tool surfaces as blocked_by_hook failure");
        assert!(
            !dir.path().join("x.txt").exists(),
            "blocked write never touched the filesystem"
        );
        // Both the before block and the after observe were logged.
        let hooks = hook_events(&events);
        assert!(
            hooks.iter().any(
                |(p, o)| *p == "tool:invoke:before" && matches!(o, HookOutcome::Blocked { .. })
            )
        );
        assert!(
            hooks
                .iter()
                .any(|(p, o)| *p == "tool:invoke:after" && matches!(o, HookOutcome::Observed))
        );
        assert!(seqs_are_contiguous(&events));
    }

    /// A `tool:invoke:before` modify rewrites the tool input the tool actually
    /// receives (`doc/hook-protocol.md` §7).
    #[tokio::test]
    async fn tool_invoke_before_modify_rewrites_input() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_path_buf();
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_call_round("c1", "write", r#"{"path":"orig.txt","content":"a"}"#),
            text_round("written"),
        ]));
        let mut tools = ToolRegistry::new();
        crate::tool::register_builtin(&mut tools, workspace);
        let mut hooks = HookRegistry::new();
        hooks.register_before(
            HookPoint::ToolInvokeBefore,
            Arc::new(FixedBefore {
                name: "redirect",
                decision: BeforeDecision::Modify(serde_json::json!({
                    "path": "redirected.txt",
                    "content": "a"
                })),
            }),
        );
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        )
        .with_hooks(hooks);

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store
            .create_new(None, None, vec!["write".to_owned()])
            .unwrap();
        let mut runtime = SessionRuntime::default();

        agent
            .run_turn(&mut writer, &mut runtime, "write something".to_owned())
            .await
            .unwrap();
        drop(writer);

        assert!(
            dir.path().join("redirected.txt").exists(),
            "the hook's modified path is what the tool wrote"
        );
        assert!(
            !dir.path().join("orig.txt").exists(),
            "the original path was overridden by the hook"
        );
    }
}
