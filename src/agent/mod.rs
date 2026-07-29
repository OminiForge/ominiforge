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

mod approval;
mod collector;
mod error;
mod plan;
mod resume;
mod sink;

pub use approval::{
    ApprovalDecision, ApprovalGate, ApprovalOutcome, ApprovalRequest, ApprovalResolution,
    ApprovalScope, NullGate,
};
pub use error::AgentError;
pub use plan::{LeafOp, PlanOp, PlanStep, StepStatus};
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
    ModelEvent, PermissionEvent, PermissionOutcome, StopReason, ToolEvent, ToolOutput, ToolSource,
    TurnEvent, TurnFailureReason, Usage,
};
use crate::core::{EventId, EventPayload, EventSource, SourceKind, TurnId};
use crate::hook::{BeforeEffect, HookExecution, HookPoint, HookRegistry};
use crate::llm::{LlmError, Message, ModelRequest, Provider, StreamEvent, ToolCall, ToolSchema};
use crate::permission::{Decision, PermissionPolicy};
use crate::session::SessionWriter;
use crate::tool::{ToolError, ToolInput, ToolRegistry};

use futures_util::{FutureExt, StreamExt};

use plan::{PLAN_TOOL_NAME, PlanError, apply_plan_op};

/// How many completion-gate nudges a turn tolerates before giving up: the model
/// stopped without finishing the plan this many times running (`doc/plan.md` §6).
const MAX_GATE: u8 = 2;

/// How many consecutive rounds a step may stay `in_progress` before the loop
/// injects a one-shot stuck warning (`doc/plan.md` §7).
const STUCK_THRESHOLD: u32 = 5;

/// Knobs for a turn that do not change between rounds.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Retry policy for transient model-request failures (network drops,
    /// 429/5xx engine overload). Only pre-stream handshake errors are retried;
    /// a chunk-level failure mid-stream still aborts the round.
    pub retry: crate::llm::RetryConfig,
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
            retry: crate::llm::RetryConfig::default(),
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
    /// The tool-call permission gate (`doc/permission.md`). Empty by default —
    /// every call is allowed until the caller attaches a policy. Behind an
    /// `Arc<RwLock>` so a front-end (the gateway's approval gate) can pin
    /// scoped approvals into the running session mid-turn
    /// ([`permission_handle`](Self::permission_handle)).
    permission: Arc<std::sync::RwLock<PermissionPolicy>>,
    /// Resolves an `Ask` decision into approve/reject. Defaults to the
    /// fail-closed [`NullGate`]; a front-end attaches its own via
    /// [`with_approval_gate`](Self::with_approval_gate).
    approval: std::sync::Arc<dyn ApprovalGate>,
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
            permission: Arc::new(std::sync::RwLock::new(PermissionPolicy::default())),
            approval: approval::default_gate(),
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

    /// Attach a permission policy. Tool calls are then classified allow/deny/ask
    /// in `dispatch_tool` before execution (`doc/permission.md`).
    #[must_use]
    pub fn with_permission(mut self, permission: PermissionPolicy) -> Self {
        self.permission = Arc::new(std::sync::RwLock::new(permission));
        self
    }

    /// A shared handle to the live permission policy, so a front-end (the
    /// gateway's approval gate) can pin scoped approvals into the running
    /// session (`doc/permission.md` §5). Writes through the handle take effect
    /// on the next `dispatch_tool` evaluation.
    #[must_use]
    pub fn permission_handle(&self) -> Arc<std::sync::RwLock<PermissionPolicy>> {
        Arc::clone(&self.permission)
    }

    /// Attach the approval gate that resolves `ask` decisions. Without this the
    /// agent uses the fail-closed [`NullGate`] (an `ask` becomes a rejection).
    #[must_use]
    pub fn with_approval_gate(mut self, gate: std::sync::Arc<dyn ApprovalGate>) -> Self {
        self.approval = gate;
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

/// A call that will not execute, carrying everything its failure write needs
/// (`TurnState::write_deferred_failure`). On the concurrent path the event is
/// written immediately after phase A — the call is already decided, there is
/// nothing to wait for; on the serial path it is written in place, so the
/// failure always commits in completion order like any other result event.
/// Only the messages fed to the model keep strict call order.
struct DeferredFailure {
    /// The tool-call event this failure pairs with.
    parent: EventId,
    /// The machine-readable failure code (`denied_by_policy`, …).
    code: &'static str,
    /// The human/model-facing message.
    reason: String,
    /// The `tool:invoke:after` payload to fire after the failure write, if any.
    after_payload: Option<serde_json::Value>,
}

/// How an answered `ask` settles (`TurnState::audit_ask`).
enum AskVerdict {
    /// Run the tool.
    Approved,
    /// Block the call with this model-facing code/reason.
    Blocked { code: &'static str, reason: String },
}

/// What one per-call chain (`TurnState::spawn_chain`) settled into. A chain
/// only *runs* — awaiting its own gate answer and executing; the gate's answer
/// is reported back over the verdict channel the moment it lands (audited on
/// the turn task immediately), and the result event is written on the turn
/// task as soon as the chain finishes, in completion order.
enum ChainResult {
    /// The call was blocked (gate rejection/auto-denial) or could not run
    /// (unknown tool): write this failure as the chain's result.
    Failed {
        code: &'static str,
        reason: String,
        after_payload: Option<serde_json::Value>,
    },
    /// The call executed: write this result (invoke outcome + wall time).
    Executed {
        result: (crate::tool::ToolResult, std::time::Duration),
    },
}

/// One call's state in the concurrent dispatcher: either a failure deferred
/// from `prepare_tool` (written immediately — already decided) or a running
/// per-call chain to join.
enum PhaseBOutcome {
    /// No chain — write this failure right away.
    Failed(DeferredFailure),
    /// A `plan` control call already handled in Phase A: no chain, no deferred
    /// failure — its message is taken from `plan_results` in the write-back.
    Plan,
    /// A running chain: the call awaits only its *own* gate answer (an `ask`)
    /// and executes the moment it is approved — an `allow` chain went straight
    /// to execution — never waiting on any other call's decision.
    Chained {
        parent: EventId,
        handle: tokio::task::JoinHandle<ChainResult>,
    },
}

/// Aborts every still-running chain when dropped (`drive` holds it across the
/// write-back loop): on a cancel or a hard turn error the turn task's future
/// is dropped, and this guard with it — killing each chain's `tool.invoke`
/// mid-flight rather than letting a detached task finish its side effects
/// while the log reads `cancelled` (`doc/permission.md` §5.2). On the normal
/// path the loop has drained every chain, so the aborts are no-ops.
struct ChainAbortGuard(Vec<tokio::task::AbortHandle>);

impl Drop for ChainAbortGuard {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// One leaf tool call after [`TurnState::prepare_tool`]: the dispatch front half
/// (parse, `Started`, before-hooks, permission) has run, so what remains is
/// execution, nothing (already settled), or a human decision.
enum PreparedCall {
    /// Permission allowed: execute with this post-hook input, under this parent
    /// event id.
    Run {
        parent: EventId,
        args: serde_json::Value,
    },
    /// Settled before execution (bad arguments, hook block, policy deny):
    /// decided and audited; the failure *event* is written immediately on the
    /// concurrent path (its feedback message still joins the model-facing
    /// results in call order).
    Settled(DeferredFailure),
    /// The policy asked a human (`Requested` already audited). `gate` is the
    /// spawned gate task when the round is dispatched concurrently, or `None`
    /// to request inline on the serial path.
    Ask {
        parent: EventId,
        args: serde_json::Value,
        gate: Option<tokio::task::JoinHandle<ApprovalOutcome>>,
    },
    /// A `plan` control call, intercepted before the chain machinery: it is a
    /// synchronous state op with no file/sandbox/permission concerns, so it
    /// never spawns a chain. The concurrent path must intercept it just like
    /// the serial `dispatch` does — otherwise it falls through to the leaf
    /// registry lookup and fails `unknown_tool` (plan is a control tool, never
    /// registered). The rendered plan message is produced in Phase A.
    Plan,
}

/// Map a gate answer to what the call may do. Pure — usable inside a spawned
/// chain, which has no turn-state access; the audit write happens later, on
/// the turn task (`TurnState::audit_answer`).
fn ask_verdict(resolution: ApprovalResolution, tool_name: &str) -> AskVerdict {
    match resolution {
        ApprovalResolution::Approved | ApprovalResolution::PinnedByRule { approved: true } => {
            AskVerdict::Approved
        }
        ApprovalResolution::RejectedByUser => AskVerdict::Blocked {
            code: "denied_by_user",
            reason: format!("Rejected by user: the call to `{tool_name}` was not approved."),
        },
        // A pinned deny rule blocks exactly like a policy deny in `prepare_tool`.
        ApprovalResolution::PinnedByRule { approved: false } => AskVerdict::Blocked {
            code: "denied_by_policy",
            reason: format!(
                "Denied by permission policy: tool `{tool_name}` is not permitted with this input."
            ),
        },
        ApprovalResolution::AutoDenied => AskVerdict::Blocked {
            code: "denied_no_approval",
            reason: format!(
                "Not approved: the call to `{tool_name}` was blocked — no approval was received."
            ),
        },
    }
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
    #[allow(clippy::too_many_lines)] // the two-phase dispatch keeps one linear narration
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
            if self.agent.approval.supports_concurrent_requests() {
                // Two-phase dispatch (`doc/permission.md` §5). Phase A prepares
                // every call in order — `Started`, before-hooks, permission —
                // never waiting on a human, spawning a gate task per `ask` so
                // all of the round's prompts are live at once. Phase B runs
                // every call on its own chain: it executes the moment its own
                // approval lands, and its result event commits as soon as it
                // finishes. Only the messages fed back to the model wait —
                // they assemble afterwards, strictly in call order.
                let mut prepared: Vec<PreparedCall> = Vec::with_capacity(tool_calls.len());
                // Plan results ride through Phase B untouched: a plan call is a
                // synchronous control op, intercepted BEFORE prepare/chain
                // (mirroring the serial path's `dispatch` intercept). Without
                // this the concurrent path skipped the intercept and landed in
                // `execute_tool`'s registry lookup — `unknown_tool`, so a plan
                // op never applied and its card rendered as a failed tool.
                let mut plan_results: Vec<Option<Message>> =
                    (0..tool_calls.len()).map(|_| None).collect();
                for (slot, call) in tool_calls.iter().enumerate() {
                    let event_id = outcome.tool_call_event_ids.get(&call.id).cloned();
                    touched.extend(touched_paths(call));
                    if call.name == PLAN_TOOL_NAME {
                        let message = self.dispatch_plan(call, event_id)?;
                        plan_results[slot] = Some(message);
                        prepared.push(PreparedCall::Plan);
                        continue;
                    }
                    let mut prep = self.prepare_tool(call, event_id).await?;
                    if let PreparedCall::Ask { args, gate, .. } = &mut prep {
                        let approval = Arc::clone(&self.agent.approval);
                        let request = ApprovalRequest {
                            tool_name: call.name.clone(),
                            input: args.clone(),
                            call_id: call.id.clone(),
                        };
                        *gate = Some(tokio::spawn(async move { approval.request(request).await }));
                    }
                    prepared.push(prep);
                }
                // Phase B: spawn one independent chain per call. Each chain
                // awaits only its *own* gate answer and executes the moment it
                // is approved — an approval of call #2 starts #2's execution
                // while call #1 is still undecided. `allow` chains skip the
                // gate. Ask chains report their answers over the verdict
                // channel the moment they land.
                let (verdict_tx, mut verdict_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(usize, ApprovalOutcome)>();
                // Tool-result messages accumulate per slot and are pushed after
                // the loop, so the model always sees them in `tool_call` order.
                let mut results: Vec<Option<Message>> =
                    (0..tool_calls.len()).map(|_| None).collect();
                let mut completions = futures_util::stream::FuturesUnordered::new();
                let mut abort_handles = Vec::new();
                for (slot, (call, prep)) in tool_calls.iter().zip(prepared).enumerate() {
                    let (outcome, abort) = self.spawn_chain(slot, call, prep, verdict_tx.clone());
                    if let Some(abort) = abort {
                        abort_handles.push(abort);
                    }
                    match outcome {
                        // Plan was executed in Phase A; its message joins the
                        // ordered results here (plan ops never count as progress).
                        PhaseBOutcome::Plan => {
                            results[slot] = plan_results[slot].take();
                        }
                        // Settled in phase A (bad args, hook block, policy deny):
                        // no chain to wait on — the failure writes immediately.
                        PhaseBOutcome::Failed(failure) => {
                            let (message, made_progress) =
                                self.write_deferred_failure(call, failure).await?;
                            progressed |= made_progress;
                            results[slot] = Some(message);
                        }
                        // Map the join so the completion carries its slot.
                        // Dropping the mapped future drops the `JoinHandle`,
                        // which detaches the chain (a `JoinSet` would abort its
                        // tasks on drop, changing the cancel semantics the
                        // gateway relies on).
                        PhaseBOutcome::Chained { parent, handle } => {
                            completions.push(handle.map(move |joined| (slot, parent, joined)));
                        }
                    }
                }
                // From here until the write-back loop ends, dropping the turn
                // future (cancel, hard error) recalls every still-running chain
                // — each `invoke` dies mid-flight instead of a detached task
                // finishing its side effects while the log reads `cancelled`
                // (`doc/permission.md` §5.2). On the normal path the loop
                // drains every chain and the guard's aborts are no-ops.
                let _chain_guard = ChainAbortGuard(abort_handles);
                // The verdict channel closes once every ask chain dropped its
                // sender (chains hold theirs to the end of their run).
                drop(verdict_tx);
                // Write-back driver: a verdict is audited the moment it arrives
                // (a human's approval is visible at once), and each chain's
                // result event commits the moment the chain finishes — the
                // front-end watches every call complete in real time, in
                // completion order. The select never waits on one call's chain
                // to write another call's events.
                loop {
                    tokio::select! {
                        // Verdicts first: a decision is audited as soon as it
                        // lands, never queued behind a completion.
                        biased;
                        Some((vslot, answer)) = verdict_rx.recv() => {
                            self.audit_answer(&tool_calls[vslot], answer)?;
                        }
                        Some((slot, parent, joined)) = completions.next() => {
                            let call = &tool_calls[slot];
                            let (message, made_progress) =
                                self.write_chain_result(call, parent, joined).await?;
                            progressed |= made_progress;
                            results[slot] = Some(message);
                        }
                        // The verdict channel is closed and drained, and every
                        // chain completed — nothing left to wait for. (A
                        // verdict always precedes its chain's completion, so a
                        // closed channel can never strand an unfinished chain.)
                        else => break,
                    }
                }
                // Belt and braces, in case the race reasoning above ever
                // changes: mop up any straggler rather than drop a decision.
                while let Ok((vslot, answer)) = verdict_rx.try_recv() {
                    self.audit_answer(&tool_calls[vslot], answer)?;
                }
                // The model sees tool results strictly in `tool_call` order,
                // however the executions finished.
                for message in results.into_iter().flatten() {
                    self.runtime.push_message(message);
                }
            } else {
                for call in tool_calls {
                    let event_id = outcome.tool_call_event_ids.get(&call.id).cloned();
                    touched.extend(touched_paths(&call));
                    let (result, made_progress) = self.dispatch(&call, event_id).await?;
                    progressed |= made_progress;
                    self.runtime.push_message(result);
                }
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

    /// Record a `PermissionEvent::Requested` for a gated tool call: the audit
    /// trail and the front-end's authoritative pending-approval source
    /// (`doc/permission.md` §6). Emitted from the runtime, before the gate decides.
    fn record_permission_requested(
        &mut self,
        call: &ToolCall,
        input: &serde_json::Value,
        preview: Option<String>,
    ) -> Result<(), AgentError> {
        self.writer.append(
            runtime_source(),
            EventPayload::Permission(PermissionEvent::Requested {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                input: input.clone(),
                preview,
            }),
            None,
            Some(self.turn_id.clone()),
        )?;
        Ok(())
    }

    /// Record how a gated call resolved (`doc/permission.md` §6). `scope` is the
    /// human-chosen reach of the decision when one was made (`None` for policy
    /// denies and fail-closed auto-denials).
    fn record_permission_decided(
        &mut self,
        call: &ToolCall,
        outcome: PermissionOutcome,
        decided_by: &str,
        scope: Option<ApprovalScope>,
    ) -> Result<(), AgentError> {
        self.writer.append(
            runtime_source(),
            EventPayload::Permission(PermissionEvent::Decided {
                call_id: call.id.clone(),
                outcome,
                decided_by: decided_by.to_owned(),
                scope,
            }),
            None,
            Some(self.turn_id.clone()),
        )?;
        Ok(())
    }

    // __APPEND_MARKER__

    /// Run one model round: send the current context, persist the streamed
    /// response (forwarding deltas to the sink), and return the assembled
    /// assistant message.
    ///
    /// A transient handshake failure (network drop, 429/5xx — e.g. Kimi's
    /// `engine_overloaded_error`) is retried with exponential backoff rather
    /// than aborting the turn: each attempt is its own persisted
    /// `RequestStarted` (a failed attempt closes with `RequestFailed`), and the
    /// sink is notified so the front-end can show "retrying…" instead of
    /// sitting silent. Chunk-level failures mid-stream are NOT retried —
    /// partial content has already streamed, so re-sending would duplicate the
    /// assistant message; they abort the round as before.
    async fn run_model_round(&mut self) -> Result<collector::RoundOutcome, AgentError> {
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

        let max_retries = self.agent.config.retry.max_retries;
        let mut attempt = 0u32;
        let (request_id, stream, started) = loop {
            let request_id = ulid::Ulid::new().to_string();
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

            let attempt_started = Instant::now();
            match self.agent.provider.stream(request.clone()).await {
                Ok(stream) => break (request_id, stream, attempt_started),
                Err(err) if attempt < max_retries && crate::llm::is_retryable(&err) => {
                    attempt += 1;
                    let delay = self.agent.config.retry.delay_for(attempt);
                    self.writer.append(
                        source.clone(),
                        EventPayload::Model(ModelEvent::RequestFailed {
                            request_id,
                            duration_ms: duration_ms(attempt_started.elapsed()),
                            error: retry_error_detail(&err),
                        }),
                        None,
                        Some(self.turn_id.clone()),
                    )?;
                    self.sink
                        .on_retry(attempt, max_retries, delay, &err.to_string());
                    tokio::time::sleep(delay).await;
                }
                Err(err) => return Err(err.into()),
            }
        };

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

        // Per-round context snapshot for live display: the freshly-calibrated
        // running estimate + the full window + compaction threshold. The gauge is
        // `tokens/window` (threshold is a tick, not the denominator — matches the
        // TUI status line). Emitted through the sink, not persisted.
        self.sink.on_context(
            self.runtime.ledger.running(),
            self.agent.config.context_window,
            self.agent.config.compaction_threshold,
        );

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
    /// progress (a non-error result; see [`dispatch`](Self::dispatch)). This is
    /// the serial path: prepare and settle inline, one call at a time.
    async fn dispatch_tool(
        &mut self,
        call: &ToolCall,
        tool_call_event_id: Option<EventId>,
    ) -> Result<(Message, bool), AgentError> {
        let prepared = self.prepare_tool(call, tool_call_event_id).await?;
        self.settle_prepared(call, prepared).await
    }

    /// The dispatch front half: parse the arguments, write `ToolEvent::Started`,
    /// run the `tool:invoke:before` chain, and classify the post-hook input
    /// through the permission policy (`doc/permission.md`). Every step is
    /// in-order and never waits on a human, so the concurrent dispatcher can
    /// run it back-to-back for a whole round before settling any call.
    #[allow(clippy::too_many_lines)] // before/after hook brackets around one dispatch
    async fn prepare_tool(
        &mut self,
        call: &ToolCall,
        tool_call_event_id: Option<EventId>,
    ) -> Result<PreparedCall, AgentError> {
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
                    return Ok(PreparedCall::Settled(DeferredFailure {
                        parent,
                        code: "invalid_arguments",
                        reason: format!("tool arguments were not valid JSON: {e}"),
                        after_payload: None,
                    }));
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
                return Ok(PreparedCall::Settled(DeferredFailure {
                    parent,
                    code: "blocked_by_hook",
                    reason: format!("Blocked by hook [{by}]: {reason}"),
                    after_payload: Some(
                        serde_json::json!({ "tool_name": call.name, "blocked": true, "reason": "hook" }),
                    ),
                }));
            }
        };

        // Permission gate (`doc/permission.md`): classify the post-hook input as
        // allow / deny / ask *before* the tool runs. Deny blocks outright; ask
        // suspends for the approval gate; a rejected ask blocks too. A blocked
        // call becomes a `ToolEvent::Failed` the model sees and can react to —
        // the same shape as a hook block (§8) — so it is fed back, not fatal.
        //
        // The policy is cloned out under a read lock — never held across an
        // `.await` — so a scoped approval pinning a rule mid-turn is picked up
        // on the next evaluation. A poisoned lock still holds an intact policy
        // (a writer panic cannot tear a `Vec<Rule>`), so recovering the guard
        // keeps the gate in force rather than silently skipping it.
        let policy = self
            .agent
            .permission
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !policy.is_empty() {
            match policy.evaluate(&call.name, &args) {
                Decision::Allow => {}
                Decision::Deny => {
                    // A policy deny is not a human-gated request — no `Requested`
                    // is emitted (it would render a spurious approval card, §6).
                    // Only the resolution is audited.
                    self.record_permission_decided(
                        call,
                        PermissionOutcome::AutoDenied,
                        "policy",
                        None,
                    )?;
                    return Ok(PreparedCall::Settled(DeferredFailure {
                        parent,
                        code: "denied_by_policy",
                        reason: format!(
                            "Denied by permission policy: tool `{}` is not permitted \
                             with this input.",
                            call.name
                        ),
                        after_payload: Some(
                            serde_json::json!({ "tool_name": call.name, "blocked": true, "reason": "permission" }),
                        ),
                    }));
                }
                Decision::Ask => {
                    // An `ask` genuinely requests a human decision — audit the
                    // request now, so every ask of a round is published before
                    // any of them settles; the gate itself is awaited in the
                    // settle half (`doc/permission.md` §6). For content tools
                    // (`edit`/`write`) attach the would-be diff so the human
                    // approves the actual change, not abstract args.
                    let preview = match self.agent.tools.get(&call.name) {
                        Some(tool) => tool.preview(&args).await,
                        None => None,
                    };
                    self.record_permission_requested(call, &args, preview)?;
                    return Ok(PreparedCall::Ask {
                        parent,
                        args,
                        gate: None,
                    });
                }
            }
        }
        Ok(PreparedCall::Run { parent, args })
    }

    /// The dispatch back half: resolve a prepared call to its `Tool` message.
    /// An `ask` awaits the gate task the concurrent dispatcher spawned (or
    /// requests inline on the serial path); an approved call then executes.
    async fn settle_prepared(
        &mut self,
        call: &ToolCall,
        prepared: PreparedCall,
    ) -> Result<(Message, bool), AgentError> {
        match prepared {
            PreparedCall::Settled(failure) => self.write_deferred_failure(call, failure).await,
            PreparedCall::Run { parent, args } => self.execute_tool(call, parent, args).await,
            // `Plan` is only produced by the concurrent dispatcher, which
            // intercepts plan calls before `prepare_tool`; the serial path
            // reaches `settle_prepared` via `dispatch`, whose own intercept
            // routes plan away first. A `Plan` here is unreachable.
            PreparedCall::Plan => unreachable!("plan calls never reach settle_prepared"),
            PreparedCall::Ask { parent, args, gate } => {
                let answer = match gate {
                    // A join error means the gate task panicked: nobody decided,
                    // so fail closed as an auto-denial, never a user rejection.
                    Some(handle) => handle.await.unwrap_or(ApprovalOutcome {
                        resolution: ApprovalResolution::AutoDenied,
                        scope: None,
                    }),
                    None => {
                        self.agent
                            .approval
                            .request(ApprovalRequest {
                                tool_name: call.name.clone(),
                                input: args.clone(),
                                call_id: call.id.clone(),
                            })
                            .await
                    }
                };
                self.settle_ask(call, parent, args, answer).await
            }
        }
    }

    /// Settle an `ask` once the gate answered: audit the decision
    /// (`doc/permission.md` §6), then block or execute.
    async fn settle_ask(
        &mut self,
        call: &ToolCall,
        parent: EventId,
        args: serde_json::Value,
        answer: ApprovalOutcome,
    ) -> Result<(Message, bool), AgentError> {
        match self.audit_ask(call, answer)? {
            AskVerdict::Approved => self.execute_tool(call, parent, args).await,
            AskVerdict::Blocked { code, reason } => {
                self.write_deferred_failure(
                    call,
                    DeferredFailure {
                        parent,
                        code,
                        reason,
                        after_payload: Some(
                            serde_json::json!({ "tool_name": call.name, "blocked": true, "reason": "permission" }),
                        ),
                    },
                )
                .await
            }
        }
    }

    /// Record the `PermissionEvent::Decided` for an answered `ask`, mapping the
    /// resolution to *who* decided (`doc/permission.md` §6): a human's answer
    /// audits as `"user"`, a fail-closed auto-denial as `"gate"` — never as a
    /// user rejection (`CLAUDE.md` §12).
    fn audit_answer(&mut self, call: &ToolCall, answer: ApprovalOutcome) -> Result<(), AgentError> {
        let (outcome, decided_by) = match answer.resolution {
            ApprovalResolution::Approved => (PermissionOutcome::Approved, "user"),
            ApprovalResolution::RejectedByUser => (PermissionOutcome::Rejected, "user"),
            // A pinned rule, not a fresh human answer, resolved the ask
            // (`doc/permission.md` §5.1): audited as `"policy"` — approved by
            // an `allow` pin, or auto-denied by a `deny` pin.
            ApprovalResolution::PinnedByRule { approved: true } => {
                (PermissionOutcome::Approved, "policy")
            }
            ApprovalResolution::PinnedByRule { approved: false } => {
                (PermissionOutcome::AutoDenied, "policy")
            }
            // Fail-closed: no human decided (no gate, dropped channel,
            // non-interactive terminal).
            ApprovalResolution::AutoDenied => (PermissionOutcome::AutoDenied, "gate"),
        };
        self.record_permission_decided(call, outcome, decided_by, answer.scope)
    }

    /// Audit an answered `ask` and classify it: approved to execute, or
    /// blocked with the model-facing code/reason (`ask_verdict`).
    fn audit_ask(
        &mut self,
        call: &ToolCall,
        answer: ApprovalOutcome,
    ) -> Result<AskVerdict, AgentError> {
        self.audit_answer(call, answer)?;
        Ok(ask_verdict(answer.resolution, &call.name))
    }

    /// Write a pre-execution failure (bad arguments, hook block, policy deny,
    /// gate rejection) and build its feedback message. Shared by the serial
    /// settle and the concurrent dispatcher (which writes these immediately —
    /// the call is already decided, there is nothing to wait for).
    async fn write_deferred_failure(
        &mut self,
        call: &ToolCall,
        failure: DeferredFailure,
    ) -> Result<(Message, bool), AgentError> {
        let source = EventSource {
            kind: SourceKind::Tool,
            id: call.name.clone(),
        };
        let message = self.fail_tool(
            &source,
            &failure.parent,
            call,
            0,
            failure.code,
            &failure.reason,
        )?;
        if let Some(payload) = failure.after_payload {
            self.fire_after(HookPoint::ToolInvokeAfter, payload, Some(call.name.clone()))
                .await?;
        }
        Ok((message, false))
    }

    /// Execute a prepared call inline (the serial path): look the tool up,
    /// invoke it, and write the result back through
    /// [`write_execution_result`](Self::write_execution_result).
    async fn execute_tool(
        &mut self,
        call: &ToolCall,
        parent: EventId,
        args: serde_json::Value,
    ) -> Result<(Message, bool), AgentError> {
        let Some(tool) = self.agent.tools.get(&call.name) else {
            let source = EventSource {
                kind: SourceKind::Tool,
                id: call.name.clone(),
            };
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

        let input = ToolInput {
            call_id: call.id.clone(),
            input: args,
            timeout: self.agent.config.tool_timeout,
        };
        let started = Instant::now();
        let result = tool.invoke(input).await;
        self.write_execution_result(call, parent, Ok((result, started.elapsed())))
            .await
    }

    /// Spawn one call's independent chain (the concurrent path): the chain
    /// awaits only its *own* gate answer and, on approval, executes
    /// immediately — never waiting on any other call's decision. An `allow`
    /// chain skips the gate entirely. An `ask` chain reports its answer over
    /// `verdict_tx` the moment it lands (audited on the turn task at once — a
    /// human's approval is immediately visible); its result event commits as
    /// soon as the chain finishes, in completion order (`write_chain_result`).
    /// A `Settled` call from `prepare_tool` needs no chain — its deferred
    /// failure writes immediately after phase A.
    ///
    /// The chain's [`tokio::task::AbortHandle`] comes back with it so `drive`
    /// can recall the chain on cancel (`ChainAbortGuard`).
    #[allow(clippy::too_many_lines)] // the ask arm is one straight-line closure
    fn spawn_chain(
        &self,
        slot: usize,
        call: &ToolCall,
        prep: PreparedCall,
        verdict_tx: tokio::sync::mpsc::UnboundedSender<(usize, ApprovalOutcome)>,
    ) -> (PhaseBOutcome, Option<tokio::task::AbortHandle>) {
        match prep {
            PreparedCall::Settled(failure) => (PhaseBOutcome::Failed(failure), None),
            // Already handled in Phase A (its message is in `plan_results`):
            // nothing to execute, no chain to join.
            PreparedCall::Plan => (PhaseBOutcome::Plan, None),
            PreparedCall::Run { parent, args } => {
                let tool = self.agent.tools.get(&call.name);
                let tool_name = call.name.clone();
                let input = ToolInput {
                    call_id: call.id.clone(),
                    input: args,
                    timeout: self.agent.config.tool_timeout,
                };
                let handle = tokio::spawn(async move {
                    match tool {
                        Some(tool) => {
                            let started = Instant::now();
                            let result = tool.invoke(input).await;
                            ChainResult::Executed {
                                result: (result, started.elapsed()),
                            }
                        }
                        None => ChainResult::Failed {
                            code: "unknown_tool",
                            reason: format!("no such tool: {tool_name}"),
                            after_payload: None,
                        },
                    }
                });
                let abort = handle.abort_handle();
                (PhaseBOutcome::Chained { parent, handle }, Some(abort))
            }
            PreparedCall::Ask { parent, args, gate } => {
                let tool = self.agent.tools.get(&call.name);
                let approval = Arc::clone(&self.agent.approval);
                let timeout = self.agent.config.tool_timeout;
                let tool_name = call.name.clone();
                let call_id = call.id.clone();
                let handle = tokio::spawn(async move {
                    let answer = match gate {
                        // A join error means the gate task panicked: nobody
                        // decided, so fail closed as an auto-denial.
                        Some(handle) => handle.await.unwrap_or(ApprovalOutcome {
                            resolution: ApprovalResolution::AutoDenied,
                            scope: None,
                        }),
                        None => {
                            approval
                                .request(ApprovalRequest {
                                    tool_name: tool_name.clone(),
                                    input: args.clone(),
                                    call_id: call_id.clone(),
                                })
                                .await
                        }
                    };
                    // Report the verdict before doing anything else: the turn
                    // task audits it the moment it lands, so a human's decision
                    // becomes visible immediately rather than at this call's
                    // ordered write-back slot. A dead receiver (the turn failed
                    // hard) is harmless — the chain's own work continues.
                    let _ = verdict_tx.send((slot, answer));
                    match ask_verdict(answer.resolution, &tool_name) {
                        AskVerdict::Approved => match tool {
                            Some(tool) => {
                                let started = Instant::now();
                                let result = tool
                                    .invoke(ToolInput {
                                        call_id,
                                        input: args,
                                        timeout,
                                    })
                                    .await;
                                ChainResult::Executed {
                                    result: (result, started.elapsed()),
                                }
                            }
                            None => ChainResult::Failed {
                                code: "unknown_tool",
                                reason: format!("no such tool: {tool_name}"),
                                after_payload: None,
                            },
                        },
                        AskVerdict::Blocked { code, reason } => ChainResult::Failed {
                            code,
                            reason,
                            after_payload: Some(
                                serde_json::json!({ "tool_name": tool_name, "blocked": true, "reason": "permission" }),
                            ),
                        },
                    }
                });
                let abort = handle.abort_handle();
                (PhaseBOutcome::Chained { parent, handle }, Some(abort))
            }
        }
    }

    /// Write a finished chain's settlement as its result event, the moment the
    /// chain completes: the execution result, or the failure. (The gate's
    /// answer was already audited when the chain reported it over the verdict
    /// channel.) A panicked chain becomes this call's `tool_panic` failure,
    /// scoped to the call.
    async fn write_chain_result(
        &mut self,
        call: &ToolCall,
        parent: EventId,
        outcome: Result<ChainResult, tokio::task::JoinError>,
    ) -> Result<(Message, bool), AgentError> {
        match outcome {
            Ok(ChainResult::Failed {
                code,
                reason,
                after_payload,
            }) => {
                self.write_deferred_failure(
                    call,
                    DeferredFailure {
                        parent,
                        code,
                        reason,
                        after_payload,
                    },
                )
                .await
            }
            Ok(ChainResult::Executed { result }) => {
                self.write_execution_result(call, parent, Ok(result)).await
            }
            Err(join_err) => {
                self.write_execution_result(call, parent, Err(join_err))
                    .await
            }
        }
    }

    /// Write a finished execution's result at the call's ordered slot:
    /// `Completed` on success, `Failed` on a tool error or a panicked task
    /// (scoped to this call — every other concurrent call is unaffected), then
    /// the `tool:invoke:after` chain. Runs on the turn task; the execution
    /// itself already ran, possibly concurrently (`spawn_execution`).
    async fn write_execution_result(
        &mut self,
        call: &ToolCall,
        parent: EventId,
        outcome: Result<(crate::tool::ToolResult, std::time::Duration), tokio::task::JoinError>,
    ) -> Result<(Message, bool), AgentError> {
        let source = EventSource {
            kind: SourceKind::Tool,
            id: call.name.clone(),
        };
        let (message, made_progress) = match outcome {
            Ok((Ok(output), elapsed)) => {
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
                        duration_ms: duration_ms(elapsed),
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
            Ok((Err(err), elapsed)) => {
                let (code, message) = tool_error_parts(&err);
                let msg =
                    self.fail_tool(&source, &parent, call, duration_ms(elapsed), code, &message)?;
                (msg, false)
            }
            Err(join_err) => {
                // The execution task panicked or was cancelled: scoped to this
                // call, the rest of the concurrent batch is unaffected.
                let msg = self.fail_tool(
                    &source,
                    &parent,
                    call,
                    0,
                    "tool_panic",
                    &format!("tool execution task failed: {join_err}"),
                )?;
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

/// Workspace paths a built-in filesystem tool call targets, for nested
/// project-guidance discovery. `read`/`write` target one `path`; `edit` targets
/// the `path` of every entry in its `edits` array. Other tools have no single
/// path and return none (`doc/agents-md.md`).
fn touched_paths(call: &ToolCall) -> Vec<String> {
    if !matches!(call.name.as_str(), "read" | "write" | "edit") {
        return Vec::new();
    }
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return Vec::new();
    };

    if call.name == "edit" {
        return edit_touched_paths(&args);
    }

    args.get("path")
        .and_then(serde_json::Value::as_str)
        .map(|path| vec![path.to_owned()])
        .unwrap_or_default()
}

fn edit_touched_paths(args: &serde_json::Value) -> Vec<String> {
    args.get("edits")
        .and_then(serde_json::Value::as_array)
        .map(|edits| {
            edits
                .iter()
                .filter_map(|e| e.get("path").and_then(serde_json::Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
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
/// `Content::TextView` is skipped: it is a UI-only rendering, never model input
/// (`doc/tool-view.md` §3).
fn render_output(output: &ToolOutput) -> String {
    use std::fmt::Write;

    let mut text = String::new();
    for content in &output.content {
        match content {
            Content::Text(t) => text.push_str(t),
            Content::TextView { .. } => {}
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
            Content::Text(t) | Content::TextView { text: t, .. } => t.len(),
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

/// Build the [`ErrorDetail`] recorded for a retried model request
/// (`ModelEvent::RequestFailed`). `retryable` is always true — this event is
/// only written when the loop is about to retry — and severity stays
/// `Warning`: the turn is still alive, unlike the terminal `ErrorEvent` a hard
/// failure records (`error_detail`).
fn retry_error_detail(err: &LlmError) -> ErrorDetail {
    let code = match err {
        LlmError::Transport(_) => "model_transport",
        LlmError::Status { .. } => "model_status",
        // `run_model_round` only calls this for retryable errors, so
        // decode/auth never reach here; the mapping exists defensively.
        LlmError::Decode(_) => "model_decode",
        LlmError::Auth(_) => "model_auth",
    };
    ErrorDetail {
        code: code.to_owned(),
        message: err.to_string(),
        severity: ErrorSeverity::Warning,
        retryable: true,
        source_event_id: None,
        provider_raw: None,
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

    /// A provider that fails its first `failures` calls with a retryable error,
    /// then streams the scripted batches — counting calls so tests can assert
    /// how many attempts went out.
    struct FlakyProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        failures: Mutex<u32>,
        error: LlmErrorKind,
        rounds: Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    }

    /// The [`LlmError`] a [`FlakyProvider`] fails with (`LlmError` is not
    /// `Clone`, so the shape is stored and rebuilt per failure).
    enum LlmErrorKind {
        Transport,
        Status429,
        Auth,
    }

    impl FlakyProvider {
        fn new(failures: u32, error: LlmErrorKind, rounds: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                failures: Mutex::new(failures),
                error,
                rounds: Mutex::new(rounds.into_iter().collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        #[allow(clippy::unnecessary_literal_bound)]
        fn name(&self) -> &str {
            "flaky"
        }

        async fn stream(&self, _request: ModelRequest) -> Result<EventStream, LlmError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let should_fail = {
                let mut failures = self.failures.lock().unwrap();
                if *failures > 0 {
                    *failures -= 1;
                    true
                } else {
                    false
                }
            };
            if should_fail {
                return Err(match self.error {
                    LlmErrorKind::Transport => {
                        LlmError::Transport("connection reset by peer".to_owned())
                    }
                    LlmErrorKind::Status429 => LlmError::Status {
                        status: 429,
                        body: r#"{"error":{"message":"The engine is currently overloaded, please try again later","type":"engine_overloaded_error"}}"#
                            .to_owned(),
                    },
                    LlmErrorKind::Auth => LlmError::Auth("invalid api key".to_owned()),
                });
            }
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

    /// A sink that records every `on_retry` notification, so tests can assert
    /// the front-end is told about each retry.
    #[derive(Default)]
    struct RetrySink {
        retries: Vec<(u32, u32, std::time::Duration, String)>,
    }

    impl StreamSink for RetrySink {
        fn on_retry(
            &mut self,
            attempt: u32,
            max_retries: u32,
            delay: std::time::Duration,
            error: &str,
        ) {
            self.retries
                .push((attempt, max_retries, delay, error.to_owned()));
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

    #[test]
    fn touched_paths_include_structured_edit_targets() {
        let multi = ToolCall {
            id: "c1".to_owned(),
            name: "edit".to_owned(),
            arguments: serde_json::json!({
                "edits": [
                    { "path": "a/one.txt", "old": ["x"], "new": ["y"] },
                    { "path": "b/two.txt", "old": ["x"], "new": ["y"] }
                ]
            })
            .to_string(),
        };
        assert_eq!(
            touched_paths(&multi),
            vec!["a/one.txt".to_owned(), "b/two.txt".to_owned()]
        );

        let single = ToolCall {
            id: "c2".to_owned(),
            name: "edit".to_owned(),
            arguments: serde_json::json!({
                "edits": [
                    { "path": "c/three.txt", "old": ["x"], "new": ["y"] }
                ]
            })
            .to_string(),
        };
        assert_eq!(touched_paths(&single), vec!["c/three.txt".to_owned()]);
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
            plan_round("c2", r#"{"ops":[{"op":"start","id":"1"}]}"#),
            plan_round("c3", r#"{"ops":[{"op":"complete","id":"1"}]}"#),
            plan_round("c4", r#"{"ops":[{"op":"complete","id":"2"}]}"#),
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
            plan_round("c2", r#"{"ops":[{"op":"complete","id":"1"}]}"#),
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
        .chain(
            (0..10).map(|i| plan_round(&format!("s{i}"), r#"{"ops":[{"op":"start","id":"1"}]}"#)),
        )
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
            plan_round("c2", r#"{"ops":[{"op":"cancel","id":"1"}]}"#),
            // recovers with a proper reason.
            plan_round(
                "c3",
                r#"{"ops":[{"op":"cancel","id":"1","reason":"no such tool"}]}"#,
            ),
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
                r#"{"ops":[{"op":"block","id":"1","reason":"set OPENAI_API_KEY"}]}"#,
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
            plan_round("c1", r#"{"ops":[{"op":"start","id":"1"}]}"#),
        ];
        for i in 0..STUCK_THRESHOLD {
            rounds.push(plan_round(
                &format!("s{i}"),
                r#"{"ops":[{"op":"start","id":"1"}]}"#,
            ));
        }
        rounds.push(plan_round(
            "done",
            r#"{"ops":[{"op":"complete","id":"1"}]}"#,
        ));
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
            plan_round("c1", r#"{"ops":[{"op":"start","id":"1"}]}"#),
        ];
        for i in 0..(STUCK_THRESHOLD + 2) {
            rounds.push(tool_call_round(
                &format!("r{i}"),
                "read",
                r#"{"path":"note.txt"}"#,
            ));
        }
        rounds.push(plan_round(
            "done",
            r#"{"ops":[{"op":"complete","id":"1"}]}"#,
        ));
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

    /// Transient handshake failures are retried with backoff: Kimi's 429
    /// `engine_overloaded_error` on the first two attempts must not abort the
    /// turn — the third attempt streams the answer, the sink was notified per
    /// retry, and the log holds a `RequestStarted`/`RequestFailed` pair per
    /// failed attempt so replay can explain the delay.
    #[tokio::test]
    async fn transient_model_error_retries_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FlakyProvider::new(
            2,
            LlmErrorKind::Status429,
            vec![text_round("recovered after retry")],
        ));
        let calls = Arc::clone(&provider.calls);
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                // Zero-delay backoff keeps the retry test instantaneous.
                retry: crate::llm::RetryConfig {
                    max_retries: 10,
                    initial_delay: Duration::ZERO,
                    max_delay: Duration::ZERO,
                },
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();
        let mut sink = RetrySink::default();

        let outcome = agent
            .run_turn_with_sink(&mut writer, &mut runtime, "hi".to_owned(), &mut sink)
            .await
            .unwrap();
        drop(writer);

        // The turn completed through the retries, not as an error.
        assert_eq!(outcome.answer, "recovered after retry");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert_eq!(outcome.rounds, 1);

        // The front-end was notified about each retry, with the 1-based
        // attempt number and the retry budget.
        assert_eq!(sink.retries.len(), 2);
        assert_eq!(sink.retries[0].0, 1);
        assert_eq!(sink.retries[1].0, 2);
        assert_eq!(sink.retries[0].1, 10, "carries the configured budget");
        assert!(sink.retries[0].3.contains("429"));

        // Each failed attempt is a persisted RequestStarted/RequestFailed pair
        // (retryable warning), so replay can explain the gap; the successful
        // attempt closes with RequestCompleted as usual.
        let events = store.read_events(&sid).unwrap();
        let failed_requests = events
            .iter()
            .filter(|e| {
                matches!(
                    &e.payload,
                    EventPayload::Model(ModelEvent::RequestFailed { error, .. })
                        if error.retryable && error.severity == ErrorSeverity::Warning
                )
            })
            .count();
        assert_eq!(failed_requests, 2, "one RequestFailed per retried attempt");
        let completed_requests = events
            .iter()
            .filter(|e| {
                matches!(
                    &e.payload,
                    EventPayload::Model(ModelEvent::RequestCompleted { .. })
                )
            })
            .count();
        assert_eq!(completed_requests, 1);
        assert!(seqs_are_contiguous(&events));
    }

    /// Non-retryable failures never enter the retry loop: an auth rejection
    /// propagates on the first attempt — re-sending it only burns rate-limit
    /// budget — and the sink is never notified.
    #[tokio::test]
    async fn non_retryable_model_error_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FlakyProvider::new(1, LlmErrorKind::Auth, vec![]));
        let calls = Arc::clone(&provider.calls);
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                retry: crate::llm::RetryConfig {
                    max_retries: 10,
                    initial_delay: Duration::ZERO,
                    max_delay: Duration::ZERO,
                },
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let mut runtime = SessionRuntime::default();
        let mut sink = RetrySink::default();

        let result = agent
            .run_turn_with_sink(&mut writer, &mut runtime, "hi".to_owned(), &mut sink)
            .await;

        assert!(matches!(result, Err(AgentError::Model(LlmError::Auth(_)))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(sink.retries.is_empty(), "no retry notification went out");
    }

    /// The retry budget is finite: a provider that stays overloaded eventually
    /// aborts the turn with the last error instead of looping forever, and the
    /// sink saw exactly `max_retries` notifications.
    #[tokio::test]
    async fn retries_exhausted_aborts_turn_with_last_error() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(FlakyProvider::new(
            u32::MAX,
            LlmErrorKind::Transport,
            vec![],
        ));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                retry: crate::llm::RetryConfig {
                    max_retries: 3,
                    initial_delay: Duration::ZERO,
                    max_delay: Duration::ZERO,
                },
                ..AgentConfig::default()
            },
        );

        let store = SessionStore::new(dir.path().join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let mut runtime = SessionRuntime::default();
        let mut sink = RetrySink::default();

        let result = agent
            .run_turn_with_sink(&mut writer, &mut runtime, "hi".to_owned(), &mut sink)
            .await;

        assert!(matches!(
            result,
            Err(AgentError::Model(LlmError::Transport(_)))
        ));
        // initial attempt + 3 retries = 4 calls.
        assert_eq!(sink.retries.len(), 3);
        assert_eq!(sink.retries[2].0, 3);
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

    // ── Permission gate (Step 3, `doc/permission.md`) ────────────────────────

    /// A fake approval gate returning a fixed decision, for driving the `ask`
    /// paths without a real front-end.
    struct FixedGate(ApprovalResolution);

    #[async_trait::async_trait]
    impl ApprovalGate for FixedGate {
        async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
            ApprovalOutcome {
                resolution: self.0,
                scope: None,
            }
        }
    }

    /// Build a single-round turn that calls `write`, run it under `policy`
    /// (optionally with `gate`), and report whether the file landed plus the
    /// events. Mirrors the hook tests' real-`write`-tool + filesystem-effect
    /// pattern so the assertion is on actual execution, not a mock.
    async fn run_write_under_policy(
        dir: &std::path::Path,
        policy: PermissionPolicy,
        gate: Option<std::sync::Arc<dyn ApprovalGate>>,
    ) -> (bool, Vec<CoreEvent>) {
        let workspace = dir.to_path_buf();
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_call_round("c1", "write", r#"{"path":"g.txt","content":"hi"}"#),
            text_round("done"),
        ]));
        let mut tools = ToolRegistry::new();
        crate::tool::register_builtin(&mut tools, workspace);
        let mut agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        )
        .with_permission(policy);
        if let Some(gate) = gate {
            agent = agent.with_approval_gate(gate);
        }

        let store = SessionStore::new(dir.join("sessions"));
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
        (dir.join("g.txt").exists(), events)
    }

    fn has_failed_code(events: &[CoreEvent], code: &str) -> bool {
        events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::Tool(ToolEvent::Failed { error, .. }) if error.code == code
            )
        })
    }

    fn has_permission_requested(events: &[CoreEvent]) -> bool {
        events.iter().any(|e| {
            matches!(
                &e.payload,
                EventPayload::Permission(PermissionEvent::Requested { .. })
            )
        })
    }

    fn has_decided_by(events: &[CoreEvent], who: &str) -> bool {
        events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::Permission(PermissionEvent::Decided { decided_by, .. }) if decided_by == who
        ))
    }

    fn has_permission_decided(events: &[CoreEvent], outcome: PermissionOutcome) -> bool {
        events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::Permission(PermissionEvent::Decided { outcome: o, .. }) if *o == outcome
        ))
    }

    fn deny_rule(tool: &str, contains: &[&str]) -> crate::permission::Rule {
        crate::permission::Rule::contains(tool, contains.iter().map(|s| (*s).to_owned()).collect())
    }

    /// Allow: a policy with no matching rule runs the tool — the file lands and
    /// no permission failure is recorded.
    #[tokio::test]
    async fn permission_allow_runs_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy {
            deny: vec![deny_rule("shell", &["rm"])], // unrelated to write
            allow: vec![],
            ask: vec![],
        };
        let (wrote, events) = run_write_under_policy(dir.path(), policy, None).await;
        assert!(wrote, "an allowed write reaches the filesystem");
        assert!(!has_failed_code(&events, "denied_by_policy"));
        assert!(!has_failed_code(&events, "denied_by_user"));
        // An allowed call never enters the gate, so it leaves no audit trail.
        assert!(!has_permission_requested(&events));
    }

    /// Deny: a matching deny rule blocks the tool before it runs — the file is
    /// never written and a `denied_by_policy` failure is fed back. If the gate
    /// leaked, the file would exist; asserting its absence proves code, not the
    /// model, stopped the call.
    #[tokio::test]
    async fn permission_deny_blocks_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy {
            deny: vec![deny_rule("write", &[])], // deny the write tool outright
            allow: vec![],
            ask: vec![],
        };
        let (wrote, events) = run_write_under_policy(dir.path(), policy, None).await;
        assert!(!wrote, "a denied write never touches the filesystem");
        assert!(has_failed_code(&events, "denied_by_policy"));
        // A policy deny is not a human-gated request: only the resolution is
        // audited (no `Requested`, which would render a spurious approval card —
        // M4). `decided_by` is "policy", not "user".
        assert!(!has_permission_requested(&events));
        assert!(has_permission_decided(
            &events,
            PermissionOutcome::AutoDenied
        ));
        assert!(has_decided_by(&events, "policy"));
    }

    /// Ask + approve: an approved call runs. The same policy that blocks under a
    /// rejecting gate must execute under an approving one — proving the gate's
    /// answer, not the policy alone, decides an `ask`.
    #[tokio::test]
    async fn permission_ask_approved_runs_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("write", &[])],
        };
        let gate = std::sync::Arc::new(FixedGate(ApprovalResolution::Approved));
        let (wrote, events) = run_write_under_policy(dir.path(), policy, Some(gate)).await;
        assert!(wrote, "an approved ask runs the tool");
        assert!(!has_failed_code(&events, "denied_by_user"));
        assert!(has_permission_requested(&events));
        assert!(has_permission_decided(&events, PermissionOutcome::Approved));
    }

    /// Ask + reject: a rejected call is blocked with `denied_by_user` and the
    /// file never lands.
    #[tokio::test]
    async fn permission_ask_rejected_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("write", &[])],
        };
        let gate = std::sync::Arc::new(FixedGate(ApprovalResolution::RejectedByUser));
        let (wrote, events) = run_write_under_policy(dir.path(), policy, Some(gate)).await;
        assert!(!wrote, "a rejected ask never runs the tool");
        assert!(has_failed_code(&events, "denied_by_user"));
        assert!(has_permission_decided(&events, PermissionOutcome::Rejected));
    }

    /// Fail-closed default: with an `ask` policy and no gate attached, the
    /// built-in `NullGate` rejects — an `ask` never becomes a silent allow just
    /// because a front-end forgot to wire a gate (`CLAUDE.md` §12).
    #[tokio::test]
    async fn permission_ask_without_gate_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("write", &[])],
        };
        let (wrote, events) = run_write_under_policy(dir.path(), policy, None).await;
        assert!(!wrote, "no gate => fail closed => tool blocked");
        // Audited honestly: no human decided, so it is an AutoDenied by the
        // "gate", never a fabricated "user" rejection (M2, `CLAUDE.md` §12). The
        // model-facing code says "no approval received", not "denied by user".
        assert!(has_permission_decided(
            &events,
            PermissionOutcome::AutoDenied
        ));
        assert!(has_failed_code(&events, "denied_no_approval"));
        assert!(!has_failed_code(&events, "denied_by_user"));
        assert!(
            !has_decided_by(&events, "user"),
            "a fail-closed denial must not be attributed to a user"
        );
    }

    // ── Parallel approvals (two-phase dispatch, `doc/permission.md` §5) ──────

    /// A concurrency-capable fake gate: resolves each request from a per-call
    /// map and records every request that arrived — driving the two-phase
    /// dispatch without a front-end.
    struct ConcurrentGate {
        resolutions: HashMap<String, ApprovalResolution>,
        requested: Mutex<Vec<String>>,
    }

    impl ConcurrentGate {
        fn approve_all() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                resolutions: HashMap::new(),
                requested: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl ApprovalGate for ConcurrentGate {
        async fn request(&self, req: ApprovalRequest) -> ApprovalOutcome {
            self.requested.lock().unwrap().push(req.call_id.clone());
            let resolution = self
                .resolutions
                .get(&req.call_id)
                .copied()
                .unwrap_or(ApprovalResolution::Approved);
            ApprovalOutcome {
                resolution,
                scope: None,
            }
        }

        fn supports_concurrent_requests(&self) -> bool {
            true
        }
    }

    /// One model round issuing two `write` calls (block index 0 and 1), so a
    /// round holds more than one call to dispatch.
    fn two_write_calls_round(id1: &str, path1: &str, id2: &str, path2: &str) -> Vec<StreamEvent> {
        let block = |index: u32, id: &str, path: &str| {
            vec![
                StreamEvent::BlockStart {
                    index,
                    block_type: ContentBlockType::ToolCall {
                        id: id.to_owned(),
                        name: "write".to_owned(),
                    },
                },
                StreamEvent::ToolCallDelta {
                    index,
                    json_delta: format!(r#"{{"path":"{path}","content":"hi"}}"#),
                },
                StreamEvent::BlockStop { index },
            ]
        };
        let mut events = block(0, id1, path1);
        events.extend(block(1, id2, path2));
        events.push(StreamEvent::Completed {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        events
    }

    /// Run a scripted turn under `policy` with `gate`, returning the committed
    /// events. Generalizes `run_write_under_policy` to custom round scripts —
    /// the parallel-approval tests drive two-call rounds through it.
    async fn run_rounds_under_policy(
        dir: &std::path::Path,
        rounds: Vec<Vec<StreamEvent>>,
        policy: PermissionPolicy,
        gate: std::sync::Arc<dyn ApprovalGate>,
    ) -> Vec<CoreEvent> {
        let workspace = dir.to_path_buf();
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
        )
        .with_permission(policy)
        .with_approval_gate(gate);

        let store = SessionStore::new(dir.join("sessions"));
        let mut writer = store
            .create_new(None, None, vec!["write".to_owned()])
            .unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();
        agent
            .run_turn(&mut writer, &mut runtime, "write files".to_owned())
            .await
            .unwrap();
        drop(writer);
        store.read_events(&sid).unwrap()
    }

    /// Two `ask` calls in one round under a concurrency-capable gate: BOTH
    /// requests are published (`Requested`) before either settles (`Decided`),
    /// and both approved calls execute in the model's original order. With the
    /// old serial loop the second call's request never even fired until the
    /// first was answered.
    #[tokio::test]
    async fn parallel_asks_publish_all_requests_before_settling() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("write", &[])],
        };
        let gate = ConcurrentGate::approve_all();
        let events = run_rounds_under_policy(
            dir.path(),
            vec![
                two_write_calls_round("c1", "a.txt", "c2", "b.txt"),
                text_round("done"),
            ],
            policy,
            gate.clone(),
        )
        .await;

        // Both asks were approved and executed — both files landed.
        assert!(dir.path().join("a.txt").exists(), "c1 executed");
        assert!(dir.path().join("b.txt").exists(), "c2 executed");
        assert_eq!(
            gate.requested.lock().unwrap().len(),
            2,
            "both asks went out"
        );

        // Every Requested precedes every Decided.
        let mut requested_seqs = Vec::new();
        let mut decided_seqs = Vec::new();
        for e in &events {
            match &e.payload {
                EventPayload::Permission(PermissionEvent::Requested { .. }) => {
                    requested_seqs.push(e.seq);
                }
                EventPayload::Permission(PermissionEvent::Decided { .. }) => {
                    decided_seqs.push(e.seq);
                }
                _ => {}
            }
        }
        assert_eq!(requested_seqs.len(), 2, "both asks must be published");
        assert_eq!(decided_seqs.len(), 2, "both asks settled");
        let last_requested = requested_seqs.iter().max().unwrap();
        let first_decided = decided_seqs.iter().min().unwrap();
        assert!(
            last_requested < first_decided,
            "every Requested must precede every Decided: {requested_seqs:?} vs {decided_seqs:?}"
        );

        // Both approved calls executed — the files asserted above landed. (The
        // two real `write` tools race to completion, so no commit order is
        // asserted here; the model still reads results in call order.)
        assert_eq!(rebuilt_tool_result_ids(&events), ["c1", "c2"]);
    }

    /// An `ask` on a content tool carries the would-be diff in `Requested.preview`
    /// (`doc/permission.md` §6): the human approves the actual change, not
    /// abstract args. The preview is computed at gate time and must NOT have
    /// written the file yet (the gate is still pending).
    #[tokio::test]
    async fn ask_on_write_carries_a_diff_preview() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-existing file so the write is an overwrite with a real diff.
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("write", &[])],
        };
        let gate = ConcurrentGate::approve_all();
        let events = run_rounds_under_policy(
            dir.path(),
            vec![
                vec![
                    StreamEvent::BlockStart {
                        index: 0,
                        block_type: ContentBlockType::ToolCall {
                            id: "c1".to_owned(),
                            name: "write".to_owned(),
                        },
                    },
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        json_delta: r#"{"path":"a.txt","content":"new\n"}"#.to_owned(),
                    },
                    StreamEvent::BlockStop { index: 0 },
                    StreamEvent::Completed {
                        stop_reason: StopReason::ToolUse,
                        usage: Usage::default(),
                    },
                ],
                text_round("done"),
            ],
            policy,
            gate,
        )
        .await;

        let preview = events.iter().find_map(|e| match &e.payload {
            EventPayload::Permission(PermissionEvent::Requested { preview, .. }) => preview.clone(),
            _ => None,
        });
        let preview = preview.expect("Requested carries a preview for write");
        // The preview is the same JSON envelope the executed `TextView` carries.
        let json: serde_json::Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(json["kind"], "diff");
        let patch = json["files"][0]["patch"].as_str().unwrap();
        assert!(
            json["files"][0]["path"] == "a.txt" && patch.contains("-old") && patch.contains("+new"),
            "preview is the old→new diff: {preview}"
        );
    }

    /// Older logs lack `Requested.preview` — the field is optional and defaults
    /// to `None`, so a pre-preview event still deserializes (`doc/event-schema.md`).
    #[test]
    fn requested_without_preview_deserializes() {
        let v = serde_json::json!({
            "Requested": { "call_id": "c1", "tool_name": "write", "input": {"path":"a.txt"} }
        });
        let ev: PermissionEvent = serde_json::from_value(v).unwrap();
        match ev {
            PermissionEvent::Requested { preview, .. } => assert_eq!(preview, None),
            PermissionEvent::Decided { .. } => panic!("expected Requested"),
        }
    }

    /// Mixed verdicts on parallel asks: the approved call runs, the rejected
    /// one is blocked with `denied_by_user`, and both decisions are audited.
    /// (Decided events land in arrival order — with an instantly-resolving fake
    /// gate that order is a race, so the assertions are per-call, not ordered.)
    #[tokio::test]
    async fn parallel_asks_settle_in_order_with_mixed_verdicts() {
        let dir = tempfile::tempdir().unwrap();
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("write", &[])],
        };
        let gate = std::sync::Arc::new(ConcurrentGate {
            resolutions: HashMap::from([("c2".to_owned(), ApprovalResolution::RejectedByUser)]),
            requested: Mutex::new(Vec::new()),
        });
        let events = run_rounds_under_policy(
            dir.path(),
            vec![
                two_write_calls_round("c1", "a.txt", "c2", "b.txt"),
                text_round("done"),
            ],
            policy,
            gate,
        )
        .await;

        assert!(dir.path().join("a.txt").exists(), "approved c1 executed");
        assert!(!dir.path().join("b.txt").exists(), "rejected c2 never ran");
        assert!(has_failed_code(&events, "denied_by_user"));

        // Each call's decision is audited with its own outcome, whichever
        // chain's verdict landed first.
        let outcome_of = |call_id: &str| {
            events.iter().find_map(|e| match &e.payload {
                EventPayload::Permission(PermissionEvent::Decided {
                    call_id: id,
                    outcome,
                    ..
                }) if id == call_id => Some(*outcome),
                _ => None,
            })
        };
        assert_eq!(outcome_of("c1"), Some(PermissionOutcome::Approved));
        assert_eq!(outcome_of("c2"), Some(PermissionOutcome::Rejected));

        // …while every call's result event is scoped to its own kind, and the
        // model reads both results in `tool_call` order.
        let mut kinds: HashMap<String, String> = result_events_in_order(&events)
            .into_iter()
            .map(|(id, _, kind)| (id, kind))
            .collect();
        assert_eq!(kinds.remove("c1").as_deref(), Some("completed"));
        assert_eq!(kinds.remove("c2").as_deref(), Some("denied_by_user"));
        assert_eq!(rebuilt_tool_result_ids(&events), ["c1", "c2"]);
    }

    // ── Concurrent execution (two-phase dispatch, `doc/permission.md` §5) ────

    /// A tool that sleeps `delay` before returning a text result, counting its
    /// invocations — drives the concurrent-execution timing assertions and the
    /// "rejected calls never execute" check.
    struct DelayedTool {
        name: String,
        delay: std::time::Duration,
        invocations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::tool::Tool for DelayedTool {
        fn descriptor(&self) -> crate::tool::ToolDescriptor {
            crate::tool::ToolDescriptor {
                name: self.name.clone(),
                description: "sleeps, then answers".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn invoke(&self, _input: ToolInput) -> crate::tool::ToolResult {
            self.invocations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(self.delay).await;
            Ok(ToolOutput {
                content: vec![Content::Text(format!("{} done", self.name))],
                is_error: false,
                error_code: None,
            })
        }
    }

    /// A tool that always fails with a protocol error — its `Failed` must stay
    /// scoped to its own call.
    struct ErrorTool;

    #[async_trait::async_trait]
    impl crate::tool::Tool for ErrorTool {
        fn descriptor(&self) -> crate::tool::ToolDescriptor {
            crate::tool::ToolDescriptor {
                name: "erratic".to_owned(),
                description: "always errors".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn invoke(&self, _input: ToolInput) -> crate::tool::ToolResult {
            Err(ToolError::Execution("it broke".to_owned()))
        }
    }

    /// A tool that panics mid-invoke — the join error must become this call's
    /// `Failed` (`tool_panic`), not kill the batch.
    struct PanicTool;

    #[async_trait::async_trait]
    impl crate::tool::Tool for PanicTool {
        fn descriptor(&self) -> crate::tool::ToolDescriptor {
            crate::tool::ToolDescriptor {
                name: "panics".to_owned(),
                description: "always panics".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn invoke(&self, _input: ToolInput) -> crate::tool::ToolResult {
            panic!("boom")
        }
    }

    /// One model round issuing one `{}`-argument call per `(id, tool)` pair, in
    /// order — the concurrent-execution tests drive multi-call rounds with
    /// heterogeneous tools.
    fn multi_call_round(calls: &[(&str, &str)]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for (index, (id, name)) in (0u32..).zip(calls.iter()) {
            events.extend([
                StreamEvent::BlockStart {
                    index,
                    block_type: ContentBlockType::ToolCall {
                        id: (*id).to_owned(),
                        name: (*name).to_owned(),
                    },
                },
                StreamEvent::ToolCallDelta {
                    index,
                    json_delta: "{}".to_owned(),
                },
                StreamEvent::BlockStop { index },
            ]);
        }
        events.push(StreamEvent::Completed {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        events
    }

    /// Run a scripted turn with an explicit tool registry (custom test tools),
    /// returning the committed events.
    async fn run_rounds_with_registry(
        dir: &std::path::Path,
        rounds: Vec<Vec<StreamEvent>>,
        tools: ToolRegistry,
        policy: PermissionPolicy,
        gate: std::sync::Arc<dyn ApprovalGate>,
    ) -> Vec<CoreEvent> {
        let provider = Arc::new(ScriptedProvider::new(rounds));
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        )
        .with_permission(policy)
        .with_approval_gate(gate);

        let store = SessionStore::new(dir.join("sessions"));
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let mut runtime = SessionRuntime::default();
        agent
            .run_turn(&mut writer, &mut runtime, "go".to_owned())
            .await
            .unwrap();
        drop(writer);
        store.read_events(&sid).unwrap()
    }

    /// Every call's result event in log order: `(call_id, seq, kind)` where
    /// kind is `"completed"` or the failure code — asserts the write-back order
    /// regardless of execution completion order.
    fn result_events_in_order(events: &[CoreEvent]) -> Vec<(String, u64, String)> {
        let mut call_ids: HashMap<u64, String> = HashMap::new();
        for e in events {
            if let EventPayload::Model(ModelEvent::ContentBlock {
                content: crate::core::payload::BlockContent::ToolCall { id, .. },
                ..
            }) = &e.payload
            {
                call_ids.insert(e.seq, id.clone());
            }
        }
        events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Tool(ToolEvent::Completed {
                    tool_call_event_id, ..
                }) => Some((
                    call_ids[&tool_call_event_id.seq].clone(),
                    e.seq,
                    "completed".to_owned(),
                )),
                EventPayload::Tool(ToolEvent::Failed {
                    tool_call_event_id,
                    error,
                    ..
                }) => Some((
                    call_ids[&tool_call_event_id.seq].clone(),
                    e.seq,
                    error.code.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Two 500ms tools in one round must overlap: total wall time stays well
    /// under the serial floor of 1000ms.
    #[tokio::test]
    async fn concurrent_execution_overlaps_slow_tools() {
        let dir = tempfile::tempdir().unwrap();
        let invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(DelayedTool {
            name: "slow".to_owned(),
            delay: std::time::Duration::from_millis(500),
            invocations: std::sync::Arc::clone(&invocations),
        }));

        let started = Instant::now();
        let events = run_rounds_with_registry(
            dir.path(),
            vec![
                multi_call_round(&[("c1", "slow"), ("c2", "slow")]),
                text_round("done"),
            ],
            tools,
            PermissionPolicy::default(),
            ConcurrentGate::approve_all(),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "two 500ms tools must overlap (serial would be ≥1000ms): took {elapsed:?}"
        );
        assert_eq!(invocations.load(std::sync::atomic::Ordering::Relaxed), 2);
        let results = result_events_in_order(&events);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, _, kind)| kind == "completed"));
    }

    /// Regression: a `plan` call in a CONCURRENT round must be intercepted and
    /// applied, not fall through to the leaf registry (`unknown_tool`). The
    /// concurrent dispatcher used to skip the serial path's `dispatch` plan
    /// intercept, so a plan op landed in `execute_tool`'s registry lookup and
    /// failed — the plan never advanced and its card rendered as a failed tool.
    #[tokio::test]
    async fn concurrent_dispatch_intercepts_plan_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::new();
        crate::tool::register_builtin(&mut tools, dir.path().to_path_buf());

        // One round mixing a plan op with a leaf write — enough to take the
        // concurrent two-phase path (the gate supports concurrent requests).
        let events = run_rounds_with_registry(
            dir.path(),
            vec![
                multi_tool_call_round(&[
                    (
                        "p1",
                        "plan",
                        r#"{"op":"init","steps":[{"content":"only step"}]}"#,
                    ),
                    ("w1", "write", r#"{"path":"f.txt","content":"hi"}"#),
                    ("p2", "plan", r#"{"ops":[{"op":"complete","id":"1"}]}"#),
                ]),
                text_round("done"),
            ],
            tools,
            PermissionPolicy::default(),
            ConcurrentGate::approve_all(),
        )
        .await;

        // The plan ops succeeded — no `unknown_tool` failure for `plan`.
        assert!(
            !has_failed_code(&events, "unknown_tool"),
            "plan must not hit unknown_tool on the concurrent path"
        );
        // Both plan ops produced a successful `Completed` (the rendered plan),
        // and the leaf write executed too.
        let plan_completions = events
            .iter()
            .filter(|e| {
                matches!(&e.payload, EventPayload::Tool(ToolEvent::Completed { .. }))
                    && e.source.kind == SourceKind::Tool
                    && e.source.id == "plan"
            })
            .count();
        assert_eq!(plan_completions, 2, "both plan ops applied");
        assert!(dir.path().join("f.txt").exists(), "leaf write executed");
        // The rebuilt runtime holds the plan in its terminal state.
        let runtime = crate::agent::rebuild_runtime(&events, vec![]);
        assert_eq!(runtime.plan.len(), 1);
        assert!(runtime.plan.iter().all(|s| s.status.is_terminal()));
    }

    /// The `tool_call_id`s of the `Tool` messages in the rebuilt context — the
    /// order the model reads results in, which must be the `tool_call` order
    /// however the result events committed.
    fn rebuilt_tool_result_ids(events: &[CoreEvent]) -> Vec<String> {
        let runtime = crate::agent::rebuild_runtime(events, vec![]);
        runtime
            .context
            .iter()
            .filter_map(|m| match m {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Result events commit in *completion* order: the later, faster call's
    /// `Completed` lands first — the front-end sees it the moment it finishes.
    /// The model's read order is unaffected: its tool-result messages still
    /// assemble in `tool_call` order.
    #[tokio::test]
    async fn results_commit_in_completion_order_not_call_order() {
        let dir = tempfile::tempdir().unwrap();
        let counter = || std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(DelayedTool {
            name: "slow".to_owned(),
            delay: std::time::Duration::from_millis(400),
            invocations: counter(),
        }));
        tools.register(Arc::new(DelayedTool {
            name: "fast".to_owned(),
            delay: std::time::Duration::ZERO,
            invocations: counter(),
        }));

        let events = run_rounds_with_registry(
            dir.path(),
            vec![
                multi_call_round(&[("c1", "slow"), ("c2", "fast")]),
                text_round("done"),
            ],
            tools,
            PermissionPolicy::default(),
            ConcurrentGate::approve_all(),
        )
        .await;

        let results = result_events_in_order(&events);
        assert_eq!(
            results,
            vec![
                ("c2".to_owned(), results[0].1, "completed".to_owned()),
                ("c1".to_owned(), results[1].1, "completed".to_owned()),
            ],
            "the faster c2 commits before the slower c1"
        );
        // …but the model still reads the results in `tool_call` order.
        assert_eq!(rebuilt_tool_result_ids(&events), ["c1", "c2"]);
    }

    /// ask + allow mix: the allowed call executes without waiting and its
    /// result commits first (completion order); both execute, and the model
    /// reads both results in `tool_call` order.
    #[tokio::test]
    async fn ask_and_allow_mix_executes_and_commits_in_completion_order() {
        let dir = tempfile::tempdir().unwrap();
        let counter = || std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(DelayedTool {
            name: "slow".to_owned(),
            delay: std::time::Duration::from_millis(100),
            invocations: counter(),
        }));
        tools.register(Arc::new(DelayedTool {
            name: "fast".to_owned(),
            delay: std::time::Duration::ZERO,
            invocations: counter(),
        }));
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("slow", &[])],
        };

        let events = run_rounds_with_registry(
            dir.path(),
            vec![
                multi_call_round(&[("c1", "slow"), ("c2", "fast")]),
                text_round("done"),
            ],
            tools,
            policy,
            ConcurrentGate::approve_all(),
        )
        .await;

        let results = result_events_in_order(&events);
        assert_eq!(
            results,
            vec![
                ("c2".to_owned(), results[0].1, "completed".to_owned()),
                ("c1".to_owned(), results[1].1, "completed".to_owned()),
            ],
            "the allowed (faster) call commits first — completion order"
        );
        assert!(has_permission_decided(&events, PermissionOutcome::Approved));
        // The model still reads both results in `tool_call` order.
        assert_eq!(rebuilt_tool_result_ids(&events), ["c1", "c2"]);
    }

    /// A rejected ask skips execution entirely (the tool is never invoked);
    /// its `Failed` commits in completion order like any result, while the
    /// model still reads it at its `tool_call` slot.
    #[tokio::test]
    async fn rejected_ask_skips_execution_but_keeps_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let slow_invocations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(DelayedTool {
            name: "slow".to_owned(),
            delay: std::time::Duration::ZERO,
            invocations: std::sync::Arc::clone(&slow_invocations),
        }));
        tools.register(Arc::new(DelayedTool {
            name: "fast".to_owned(),
            delay: std::time::Duration::ZERO,
            invocations: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        let policy = PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![deny_rule("slow", &[])],
        };
        let gate = std::sync::Arc::new(ConcurrentGate {
            resolutions: HashMap::from([("c1".to_owned(), ApprovalResolution::RejectedByUser)]),
            requested: Mutex::new(Vec::new()),
        });

        let events = run_rounds_with_registry(
            dir.path(),
            vec![
                multi_call_round(&[("c1", "slow"), ("c2", "fast")]),
                text_round("done"),
            ],
            tools,
            policy,
            gate,
        )
        .await;

        assert_eq!(
            slow_invocations.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a rejected call must not execute"
        );
        let mut kinds: HashMap<String, String> = result_events_in_order(&events)
            .into_iter()
            .map(|(id, _, kind)| (id, kind))
            .collect();
        assert_eq!(kinds.remove("c1").as_deref(), Some("denied_by_user"));
        assert_eq!(kinds.remove("c2").as_deref(), Some("completed"));
        // The LLM message slot is kept: the model reads the rejection at c1's
        // position, ahead of c2's result.
        assert_eq!(rebuilt_tool_result_ids(&events), ["c1", "c2"]);
    }

    /// An erroring tool and a panicking tool fail only their own calls: the
    /// batch completes, the healthy call commits its result, and every failure
    /// is scoped to its own call (the model reads them in `tool_call` order).
    #[tokio::test]
    async fn tool_error_and_panic_are_scoped_to_their_own_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(DelayedTool {
            name: "ok".to_owned(),
            delay: std::time::Duration::ZERO,
            invocations: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        tools.register(Arc::new(ErrorTool));
        tools.register(Arc::new(PanicTool));

        let events = run_rounds_with_registry(
            dir.path(),
            vec![
                multi_call_round(&[("c1", "ok"), ("c2", "erratic"), ("c3", "panics")]),
                text_round("done"),
            ],
            tools,
            PermissionPolicy::default(),
            ConcurrentGate::approve_all(),
        )
        .await;

        let mut kinds: HashMap<String, String> = result_events_in_order(&events)
            .into_iter()
            .map(|(id, _, kind)| (id, kind))
            .collect();
        assert_eq!(kinds.remove("c1").as_deref(), Some("completed"));
        assert_eq!(kinds.remove("c2").as_deref(), Some("execution_failed"));
        assert_eq!(kinds.remove("c3").as_deref(), Some("tool_panic"));
        assert!(kinds.is_empty(), "exactly one result per call");
        assert_eq!(rebuilt_tool_result_ids(&events), ["c1", "c2", "c3"]);
        // The turn survived the batch and completed.
        assert!(
            events
                .iter()
                .any(|e| matches!(e.payload, EventPayload::Turn(TurnEvent::Completed { .. })))
        );
    }
}
