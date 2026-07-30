//! [`SessionActor`]: one tokio task that owns a single live session.
//!
//! The session store enforces a single writer per session via an OS file lock
//! held for the [`SessionWriter`]'s lifetime (`src/session`). A network gateway
//! has many clients fanning into one session, so they must serialize through one
//! owner — this actor. It owns the `(SessionWriter, SessionRuntime)` pair between
//! turns and processes commands from an mpsc inbox one at a time, so two turns
//! never interleave on one session.
//!
//! Two streams flow out over one [`broadcast`] channel ([`GatewayEvent`]):
//! - **committed events** — every persisted [`CoreEvent`], carrying a `seq` so a
//!   reconnecting SSE client can resume via `Last-Event-ID` (`doc/monitor.md`
//!   §9). Forwarded from the session [`EventBus`].
//! - **live deltas** — token-level streaming for responsive UX, ephemeral and
//!   never replayed (a reconnect rebuilds from committed events instead).
//!
//! A turn runs on a spawned task that *moves* the writer+runtime in and returns
//! them out, so a `Cancel` can `abort` the task; after the abort the writer is
//! dropped (releasing the lock) and the actor rebuilds the runtime from the
//! log, which is the source of truth.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::agent::{
    Agent, ApprovalDecision, ApprovalScope, BlockKind, SessionRuntime, StreamSink, TurnOutcome,
};
use crate::core::payload::{
    BlockContent, ErrorDetail, ErrorSeverity, ModelEvent, ToolEvent, TurnEvent,
};
use crate::core::{CoreEvent, EventId, EventPayload, EventSource, SessionId, SourceKind, TurnId};
use crate::llm::Message;
use crate::session::{EventBus, SessionStore, SessionWriter};

use super::approval::{GatewayApprovalGate, PendingApprovals, ScopedDecision};
use super::status::{ActivityStatus, SessionStatus, StatusHub};
use super::workspace::WorkspaceId;

/// Capacity of the per-session outbound broadcast. A subscriber that lags past
/// this many buffered items gets a `Lagged` error and should resync from the log
/// (committed events) — deltas it missed are simply gone (ephemeral).
const OUTBOUND_CAPACITY: usize = 1024;

/// Capacity of an actor's command inbox.
const INBOX_CAPACITY: usize = 64;

/// What a front-end sees on the wire for one session. Tagged JSON so a client
/// can switch on `type`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayEvent {
    /// A committed, persisted event. The flattened event's `seq` is its session
    /// sequence number — the SSE `Last-Event-ID` for resume.
    Event {
        #[serde(flatten)]
        event: Box<CoreEvent>,
    },
    /// A live token-level delta. Ephemeral: not persisted, not replayed on
    /// reconnect (the committed `Event` is the authoritative record).
    Delta(Delta),
    /// A turn settled. `incomplete` is `None` on a clean finish, else a short
    /// reason (round budget, todo stall, hook block).
    TurnSettled { incomplete: Option<String> },
    /// The session was compacted into a new one; the client should follow
    /// `new_session_id` for subsequent events.
    Compacted { new_session_id: String },
    /// A non-fatal note (compaction failure, etc.) for display.
    Notice { message: String },
    /// A per-round context-window occupancy snapshot for live display. Emitted
    /// after each model round calibrates the ledger. `tokens` is the running
    /// estimate; `window` the model's full context window (`0` when unknown);
    /// `threshold` the compaction fraction (a gauge tick — the gauge is
    /// `tokens/window`, not `tokens/effective_limit`). Ephemeral
    /// like [`Delta`]: not persisted, not replayed — a fresh snapshot arrives next round.
    ContextUpdated {
        tokens: u32,
        window: u32,
        threshold: f32,
    },
    /// A tool call classified `ask` by the permission policy is suspended,
    /// awaiting a human decision (`doc/permission.md` §5). Ephemeral like
    /// [`Delta`]: a client that connects later learns of it from the session's
    /// `AwaitingInput` status, not a replay. Answer with `Command::Approve`
    /// carrying the same `call_id`.
    ApprovalRequested {
        call_id: String,
        tool_name: String,
        input: serde_json::Value,
    },
    /// Marks the end of the committed-log replay on a fresh (or resumed)
    /// subscription: every event after this frame is live. The web client
    /// folds the replay burst off-screen and only presents the conversation
    /// once this lands — mirroring how the Zed client `await`s a thread's
    /// replay before handing it to the UI, so history never visibly scrolls
    /// past. Carries no payload; a client that doesn't care can ignore it.
    ReplayEnd,
}

/// A live streaming delta, mirroring [`StreamSink`] callbacks.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(tag = "delta", rename_all = "snake_case")]
pub enum Delta {
    /// A new content block opened.
    BlockStart {
        index: u32,
        /// `text` | `reasoning` | `tool_call`.
        kind: &'static str,
        /// Present for a `tool_call` block.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    /// Incremental assistant answer text.
    Text { index: u32, text: String },
    /// Incremental reasoning text.
    Reasoning { index: u32, text: String },
    /// Incremental tool-call argument JSON.
    ToolArgs { index: u32, json: String },
    /// The model request is being retried after a transient failure (network
    /// drop, 429/5xx engine overload). `attempt` is the 1-based retry number,
    /// `max_retries` the configured budget, `delay_ms` the backoff about to
    /// elapse. Ephemeral like every delta: not persisted, not replayed.
    Retrying {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
}

/// A command sent to a [`SessionActor`] over its inbox.
#[derive(Debug)]
pub enum Command {
    /// Run a turn with `text` as the user input. Enqueued if a turn is already
    /// running (turns never overlap on one session).
    Send { text: String },
    /// Abort the running turn (if any). The partial work already persisted
    /// stands; the runtime is rebuilt from the log.
    Cancel,
    /// Summarize and switch to a compaction session. `keep_last` keeps the last
    /// N user turns verbatim (`doc/context-management.md` §4).
    Compact { keep_last: Option<usize> },
    /// Deliver a human decision for a tool call the permission policy suspended
    /// (`doc/permission.md` §5). Routed to the parked waiter by `call_id`; an
    /// unknown id is ignored (already resolved, or the turn moved on). `scope`
    /// says how far the decision reaches (`once` / `session` / `profile` /
    /// `gateway`).
    Approve {
        call_id: String,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    },
    /// Stop the actor and release the session lock.
    Shutdown,
}

/// A cheap, clonable handle to a live actor: send commands, subscribe to events.
#[derive(Debug, Clone)]
pub struct ActorHandle {
    /// The session this actor currently owns. Changes on compaction (the actor
    /// follows the new session), so callers should treat it as advisory.
    inbox: mpsc::Sender<Command>,
    outbound: broadcast::Sender<GatewayEvent>,
}

impl ActorHandle {
    /// Send a command. Fails only if the actor has stopped (its inbox is
    /// closed) — the caller (registry) treats that as "respawn needed".
    ///
    /// # Errors
    /// Returns the command back if the actor is gone.
    pub async fn send(&self, cmd: Command) -> Result<(), mpsc::error::SendError<Command>> {
        self.inbox.send(cmd).await
    }

    /// Subscribe to this session's outbound event stream (committed events +
    /// live deltas). Each subscriber is an independent SSE/WS connection.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.outbound.subscribe()
    }

    /// Whether the actor is still alive (inbox open).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.inbox.is_closed()
    }
}

/// The owned, between-turns state of a session.
type Session = (SessionWriter, SessionRuntime);

/// What a finished turn task returns: the writer+runtime to resume with, plus
/// the outcome. An `Err` means the turn failed hard (provider/persistence) and
/// the session was consumed.
/// What a finished turn task returns: the writer+runtime to resume with, plus
/// the outcome. The writer rides along on the error path too (the run failed,
/// the log is intact) so `on_turn_done` never has to reopen the session — every
/// `store.open` is another fd on the same events.jsonl, and in-process flock
/// never excludes itself.
type TurnResult = (
    SessionWriter,
    SessionRuntime,
    Result<TurnOutcome, crate::agent::AgentError>,
);

/// One live session's driver. Spawned by the registry; runs until idle-evicted
/// or told to shut down.
pub struct SessionActor {
    agent: Arc<Agent>,
    /// Tool calls suspended awaiting a human decision, keyed by call id. The
    /// agent's approval gate parks waiters here; the command loop delivers
    /// decisions and clears it on cancel/shutdown (`doc/permission.md` §5).
    pending_approvals: PendingApprovals,
    store: SessionStore,
    /// System seed for rebuilding the runtime after a cancel/abort.
    system: Vec<Message>,
    session_id: SessionId,
    bus: EventBus,
    outbound: broadcast::Sender<GatewayEvent>,
    /// Process-wide activity-status publisher: the actor pushes `Running` when a
    /// turn starts and `Idle` when it settles, so the session list lights up
    /// without subscribing to this session's full event stream.
    status: StatusHub,
    /// The workspace this session belongs to, stamped on every status the actor
    /// publishes (so a workspace-scoped list can filter). Derived once from the
    /// session's meta at spawn — a session's workspace is immutable.
    workspace_id: WorkspaceId,
    /// The latest committed event `seq`, kept live by the event forwarder (which
    /// already observes every committed event) so `publish_status` reads it in
    /// O(1) — instead of re-reading and deserializing the whole event log at every
    /// turn boundary just to find the last seq. Shared with the forwarder task.
    latest_seq: Arc<AtomicU64>,
    inbox: mpsc::Receiver<Command>,
    idle_timeout: std::time::Duration,
    /// Commands received while a turn was running, replayed in order once it
    /// settles (turns never overlap, so `Send`/`Compact` mid-turn are deferred).
    deferred: VecDeque<Command>,
    /// Live MCP subprocess clients for this session. Held (never read) for the
    /// actor's lifetime: dropping a client kills its subprocess, and the agent's
    /// MCP tools dispatch through them. Per-session isolation means each actor
    /// owns its own set (user's choice; `doc/gateway.md`).
    _mcp_clients: Vec<Arc<crate::mcp::McpClient>>,
}

impl SessionActor {
    /// Spawn an actor that owns `session` (an already-open writer + its runtime),
    /// returning a handle to drive it. The writer should *not* yet have a bus
    /// attached — the actor attaches its own.
    ///
    /// `system` is the system-prompt seed used to rebuild the runtime if a turn
    /// is cancelled mid-flight. `mcp_clients` are held alive for the actor's
    /// lifetime (per-session MCP isolation). `status` is the process-wide status
    /// hub the actor publishes running/idle transitions to; the session's
    /// `workspace_id` (immutable, from its meta) is stamped on each. `on_scoped`
    /// persists `profile`/`gateway`-scoped approval decisions (injected by the
    /// registry, which owns the config roots).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        agent: Agent,
        store: SessionStore,
        system: Vec<Message>,
        session: Session,
        idle_timeout: std::time::Duration,
        mcp_clients: Vec<Arc<crate::mcp::McpClient>>,
        status: StatusHub,
        on_scoped: Option<Arc<dyn Fn(ScopedDecision) + Send + Sync>>,
    ) -> ActorHandle {
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let (outbound, _) = broadcast::channel(OUTBOUND_CAPACITY);

        let bus = EventBus::new();
        let session_id = session.0.session_id().clone();
        // The session's workspace is immutable, recorded in its meta (written
        // before the actor spawns). Derive its id once; an unreadable meta (should
        // not happen for a session we just opened) degrades to the `"none"` group.
        let workspace_id = store
            .read_meta(&session_id)
            .ok()
            .and_then(|m| m.workspace)
            .as_deref()
            .map_or_else(WorkspaceId::none, WorkspaceId::from_path);
        // Seed the cached tail seq from the writer's next-seq (O(1), in-memory):
        // the last committed seq is `next_seq - 1`, or 0 for an empty log. The
        // forwarder keeps it current from there as events commit.
        let latest_seq = Arc::new(AtomicU64::new(session.0.next_seq().saturating_sub(1)));
        // Attach the actor's bus to the writer so every appended event is
        // published; the forwarder below turns those into outbound `Event`s.
        let session = (session.0.with_bus(bus.clone()), session.1);

        // The approval gate shares the actor's pending table, outbound stream,
        // status hub, latest-seq cache, and the agent's live policy handle, so
        // an `ask` suspends the turn, a `Command::Approve` resumes it, and a
        // scoped decision pins a rule into the running session
        // (`doc/permission.md` §5).
        let pending_approvals: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let gate = GatewayApprovalGate::new(
            Arc::clone(&pending_approvals),
            outbound.clone(),
            status.clone(),
            session_id.clone(),
            workspace_id.clone(),
            Arc::clone(&latest_seq),
            agent.permission_handle(),
            on_scoped,
        );
        let agent = Arc::new(agent.with_approval_gate(Arc::new(gate)));

        let actor = Self {
            agent,
            pending_approvals,
            store,
            system,
            session_id,
            bus: bus.clone(),
            outbound: outbound.clone(),
            status,
            workspace_id,
            latest_seq: Arc::clone(&latest_seq),
            inbox: inbox_rx,
            idle_timeout,
            deferred: VecDeque::new(),
            _mcp_clients: mcp_clients,
        };

        // Forward committed events from the session bus onto the outbound stream,
        // tracking the latest committed seq for O(1) status publishing.
        spawn_event_forwarder(&bus, outbound.clone(), latest_seq);

        tokio::spawn(actor.run(session));

        ActorHandle {
            inbox: inbox_tx,
            outbound,
        }
    }

    /// The session's latest committed `seq` (the SSE resume cursor), read O(1)
    /// from the cache the event forwarder keeps current. `0` for an empty log — a
    /// benign floor for the front-end's unseen compare.
    fn tail_seq(&self) -> u64 {
        self.latest_seq.load(Ordering::Relaxed)
    }

    /// Publish the current session's activity `status` to the process-wide hub,
    /// stamped with its workspace and latest committed seq. Called at turn
    /// boundaries (not per token), so this is off the hot path.
    fn publish_status(&self, status: ActivityStatus) {
        self.status.publish(SessionStatus {
            session_id: self.session_id.clone(),
            workspace_id: self.workspace_id.clone(),
            status,
            latest_seq: self.tail_seq(),
        });
    }

    /// Route a human decision to the tool call parked awaiting it. Removing the
    /// entry and sending the decision wakes the suspended turn task. An unknown
    /// `call_id` (already resolved — by an earlier answer or a pinned rule — or
    /// the turn moved on) is a no-op. A dropped receiver (the turn abandoned the
    /// wait) means the `send` fails harmlessly.
    fn deliver_approval(&self, call_id: &str, decision: ApprovalDecision, scope: ApprovalScope) {
        // A poisoned lock (a panic while another thread held it) shouldn't crash
        // the actor; treat it as "no waiter", which fails the ask closed.
        let waiter = self
            .pending_approvals
            .lock()
            .ok()
            .and_then(|mut m| m.remove(call_id));
        if let Some(entry) = waiter {
            let _ = entry.sender.send(super::approval::PendingAnswer {
                decision,
                scope,
                pinned_by_rule: false,
            });
        }
    }

    /// Drop every parked approval waiter. Their receivers then error, so the
    /// gate returns `Reject` (fail-closed) — used when a turn is torn down
    /// (cancel/shutdown) with asks still outstanding (`doc/permission.md` §5).
    fn clear_pending(&self) {
        if let Ok(mut m) = self.pending_approvals.lock() {
            m.clear();
        }
    }

    /// The actor loop. Owns `session` between turns; transitions to a busy phase
    /// while a turn task runs, replaying any commands deferred during the turn.
    async fn run(mut self, mut session: Session) {
        loop {
            // Replay a deferred command before blocking on the inbox, so work
            // queued during a turn runs in order.
            let cmd = if let Some(cmd) = self.deferred.pop_front() {
                cmd
            } else {
                tokio::select! {
                    cmd = self.inbox.recv() => match cmd {
                        Some(cmd) => cmd,
                        // All handles dropped — nothing can reach us again.
                        None => return,
                    },
                    () = tokio::time::sleep(self.idle_timeout) => {
                        // Idle too long: drop the writer (releases the lock) and
                        // exit so the session can be reopened on demand.
                        return;
                    }
                }
            };

            match cmd {
                Command::Send { text } => {
                    let Some(next) = self.run_turn_phase(session, text).await else {
                        return; // Shutdown requested mid-turn.
                    };
                    session = next;
                }
                Command::Compact { keep_last } => {
                    session = self.compact(session, keep_last).await;
                }
                // No turn running while idle: nothing to cancel, and no ask is
                // parked, so a late `Approve` for an already-settled call is a
                // no-op too.
                Command::Cancel | Command::Approve { .. } => {}
                Command::Shutdown => return,
            }
        }
    }

    /// Run one turn on a spawned task, deferring `Send`/`Compact` and honoring
    /// `Cancel`/`Shutdown` while it runs.
    ///
    /// Returns `Some(session)` to resume with (possibly a fresh compaction
    /// session, or a rebuilt one after a cancel), or `None` if the actor was
    /// told to shut down (the loop should exit).
    async fn run_turn_phase(&mut self, session: Session, text: String) -> Option<Session> {
        let (writer, runtime) = session;
        let agent = Arc::clone(&self.agent);
        let outbound = self.outbound.clone();

        // A turn is starting: light the session up in the list. Published before
        // the turn task spawns so the transition races nothing.
        self.publish_status(ActivityStatus::Running);

        let mut handle: JoinHandle<TurnResult> = tokio::spawn(async move {
            let mut writer = writer;
            let mut runtime = runtime;
            let mut sink = BroadcastSink { tx: outbound };
            let result = agent
                .run_turn_with_sink(&mut writer, &mut runtime, text, &mut sink)
                .await;
            (writer, runtime, result)
        });

        loop {
            tokio::select! {
                res = &mut handle => {
                    return self.on_turn_done(res).await;
                }
                cmd = self.inbox.recv() => match cmd {
                    Some(Command::Cancel) => {
                        return self.cancel_turn(handle).await;
                    }
                    Some(Command::Shutdown) => {
                        self.clear_pending();
                        handle.abort();
                        let _ = handle.await;
                        // The aborted task dropped the writer (lock released).
                        // Nothing to return — the actor is stopping.
                        return None;
                    }
                    // A decision for a suspended `ask`: wake the parked waiter
                    // and keep driving the same turn (it resumes in place).
                    Some(Command::Approve { call_id, decision, scope }) => {
                        self.deliver_approval(&call_id, decision, scope);
                    }
                    // Turns never overlap: defer until this one settles.
                    Some(cmd @ (Command::Send { .. } | Command::Compact { .. })) => {
                        self.deferred.push_back(cmd);
                    }
                    None => {
                        // All handles dropped: no `Command::Approve` can ever
                        // arrive, so a turn parked on an `ask` would hang forever
                        // (leaking the actor task and the session lock). Drop the
                        // waiters first — their receivers error → the gate fails
                        // closed → the turn settles — then reap it (`doc/permission.md`
                        // §5, mirrors the Shutdown arm).
                        self.clear_pending();
                        return self.on_turn_done(handle.await).await;
                    }
                }
            }
        }
    }

    /// Handle a finished turn: emit `TurnSettled`, auto-compact if over the
    /// limit, and return the session to resume with. A hard error emits a notice
    /// and rebuilds from the log so the session stays usable. `None` means the
    /// session could not be reopened — the actor stops.
    async fn on_turn_done(
        &mut self,
        res: Result<TurnResult, tokio::task::JoinError>,
    ) -> Option<Session> {
        // The turn has settled (cleanly, hard-error, or panic) — the session is no
        // longer running. Publish before the match so every outcome path (incl.
        // auto-compact below) leaves the list showing `Idle` for this session.
        self.publish_status(ActivityStatus::Idle);
        let (writer, runtime, result) = match res {
            Ok(triple) => triple,
            Err(join_err) => {
                // The task panicked or was cancelled out from under us.
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("turn task ended unexpectedly: {join_err}"),
                });
                return self.reopen_after_abort();
            }
        };
        match result {
            Ok(outcome) => {
                let incomplete = outcome.incomplete.as_ref().map(|r| format!("{r:?}"));
                let _ = self.outbound.send(GatewayEvent::TurnSettled { incomplete });

                let over = outcome
                    .context_limit
                    .is_some_and(|l| outcome.context_tokens >= l);
                if over {
                    Some(self.compact((writer, runtime), None).await)
                } else {
                    Some((writer, runtime))
                }
            }
            Err(e) => {
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("turn failed: {e}"),
                });
                // The turn task already handed the writer back (its lock is
                // live): reuse it instead of opening another fd on the same log.
                Some(self.resume_with(writer))
            }
        }
    }

    /// Abort a running turn and rebuild the session from the log. The aborted
    /// task drops the writer (releasing the lock); awaiting the handle guarantees
    /// that drop has happened before we reopen. `None` means reopen failed and
    /// the actor stops.
    ///
    /// Before rebuilding, persist a `TurnEvent::Interrupted` for the open turn.
    /// The abort tears the writer down without recording why the turn stopped, so
    /// the log would otherwise end on a dangling `Turn::Started` — and a client
    /// replaying that history (no live `TurnSettled` on reconnect) can't tell the
    /// turn ended, leaving a turn-running UI (e.g. a stale Cancel button) stuck on.
    /// Writing the committed terminator makes the stop durable. Reopening to append
    /// is safe: the aborted task already released the lock.
    ///
    /// One `store.open` serves the backfill, the terminator, AND the resumed
    /// session: every open is another fd on the same events.jsonl (in-process
    /// flock never excludes itself), so a gateway that cancels often would
    /// otherwise leak descriptors toward EMFILE.
    async fn cancel_turn(&self, handle: JoinHandle<TurnResult>) -> Option<Session> {
        self.clear_pending();
        handle.abort();
        let _ = handle.await;
        let _ = self.outbound.send(GatewayEvent::Notice {
            message: "turn cancelled".to_owned(),
        });
        // The task is dead, so nothing more can commit: backfill any tool call
        // it left dangling (the abort can win the race against the fail-closed
        // gate writing `denied_no_approval`) *before* the terminator, so the
        // Interrupted event stays the log's tail. A failed open is best-effort:
        // skip the backfill and let `reopen_after_abort` surface the error.
        let Ok(writer) = self.store.open(&self.session_id) else {
            self.publish_status(ActivityStatus::Idle);
            return self.reopen_after_abort();
        };
        let mut writer = writer.with_bus(self.bus.clone());
        if let Some(last_seq) = self.record_cancelled_tool_calls(&mut writer) {
            self.record_interrupted(&mut writer, last_seq);
        }
        // The turn is over. Publish after the terminator so the status'
        // `latest_seq` includes it.
        self.publish_status(ActivityStatus::Idle);
        Some(self.resume_with(writer))
    }

    /// Backfill a `ToolEvent::Failed { code: "cancelled" }` for every tool call
    /// of the open turn that has no `Completed`/`Failed` pairing.
    ///
    /// The abort in [`cancel_turn`](Self::cancel_turn) can kill the turn task
    /// mid-dispatch — parked on an `ask`, or inside `tool.invoke` — before the
    /// fail-closed path writes its failure event. Without a result event the
    /// assistant's `tool_call` dangles in the log, and the next turn's provider
    /// request fails the pairing check (`tool_call_ids did not have response
    /// messages`). The task is already dead when this runs, so the scan races
    /// nothing; a call the task did settle (the gate won the race) is in the
    /// settled set and skipped. Best-effort like [`record_interrupted`]: a
    /// reopen/append failure leaves the pre-existing behavior, and
    /// `rebuild_runtime` synthesizes the missing result on read either way.
    ///
    /// Both writers share one `store.open` (one fd — see `cancel_turn`); the
    /// terminator's seq lands in `latest_seq` for the `Idle` status that
    /// follows.
    fn record_cancelled_tool_calls(&self, writer: &mut SessionWriter) -> Option<u64> {
        let events = self.store.read_events(&self.session_id).unwrap_or_default();
        let Some(turn_id) = open_turn_id(&events) else {
            return None; // no open turn — nothing can be dangling
        };
        // The universe is the open turn's tool-call content blocks (block seq →
        // tool name); a call is settled when any `Completed`/`Failed` points at
        // its block's seq. Restricting to the open turn keeps an older, cleanly
        // settled turn's calls untouched.
        let mut calls: Vec<(u64, String)> = Vec::new();
        let mut settled: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for event in &events {
            if event.turn_id.as_ref() != Some(&turn_id) {
                continue;
            }
            match &event.payload {
                EventPayload::Model(ModelEvent::ContentBlock {
                    content: BlockContent::ToolCall { name, .. },
                    ..
                }) => calls.push((event.seq, name.clone())),
                EventPayload::Tool(
                    ToolEvent::Completed {
                        tool_call_event_id, ..
                    }
                    | ToolEvent::Failed {
                        tool_call_event_id, ..
                    },
                ) => {
                    settled.insert(tool_call_event_id.seq);
                }
                _ => {}
            }
        }
        let dangling: Vec<(u64, String)> = calls
            .into_iter()
            .filter(|(seq, _)| !settled.contains(seq))
            .collect();
        let mut last_seq = None;
        for (seq, tool_name) in dangling {
            let call_event = EventId {
                session_id: self.session_id.clone(),
                seq,
            };
            last_seq = writer
                .append(
                    EventSource {
                        kind: SourceKind::Tool,
                        id: tool_name,
                    },
                    EventPayload::Tool(ToolEvent::Failed {
                        tool_call_event_id: call_event.clone(),
                        duration_ms: 0,
                        error: ErrorDetail {
                            code: "cancelled".to_owned(),
                            message: "turn cancelled by user".to_owned(),
                            severity: ErrorSeverity::Error,
                            retryable: false,
                            source_event_id: Some(call_event.clone()),
                            provider_raw: None,
                        },
                    }),
                    Some(call_event),
                    Some(turn_id.clone()),
                )
                .ok();
        }
        Some(last_seq.unwrap_or_else(|| writer.next_seq().saturating_sub(1)))
    }

    /// Append a committed `TurnEvent::Interrupted` for the turn left open by an
    /// abort. `last_seq` is the seq the terminator points at (the last cancelled
    /// tool call, or the log's tail when nothing dangled).
    ///
    /// This makes the stop durable: history replay carries a turn terminator, so
    /// a reconnecting client (which never re-sees the live `TurnSettled`) can tell
    /// the turn ended. Best-effort — if the open turn can't be found or the write
    /// fails, the session still reopens from the log; the worst case is the
    /// pre-existing dangling-turn behavior, not a crash. The append publishes on
    /// the bus, so live clients also receive the terminator (the forwarder turns
    /// it into an outbound committed `Event`).
    fn record_interrupted(&self, writer: &mut SessionWriter, last_seq: u64) {
        let events = self.store.read_events(&self.session_id).unwrap_or_default();
        let Some(turn_id) = open_turn_id(&events) else {
            return; // no turn open (already terminated) — nothing to record
        };
        let interrupted_at = EventId {
            session_id: self.session_id.clone(),
            seq: last_seq,
        };
        if let Ok(seq) = writer.append(
            EventSource {
                kind: SourceKind::Runtime,
                id: "ominiforge".to_owned(),
            },
            EventPayload::Turn(TurnEvent::Interrupted {
                turn_id: turn_id.clone(),
                interrupted_at_event_id: interrupted_at,
            }),
            None,
            Some(turn_id),
        ) {
            // Advance the cached tail synchronously, so the `Idle` status that
            // `cancel_turn` publishes right after carries this terminator's seq
            // without waiting on the async forwarder to catch up.
            self.latest_seq.fetch_max(seq, Ordering::Relaxed);
        }
    }

    /// Reopen the current session and rebuild its runtime from the event log,
    /// reattaching the bus. Used after an abort/error consumed the live pair.
    /// `None` if the session cannot be reopened (e.g. the lock is somehow still
    /// held) — the actor stops rather than panic.
    fn reopen_after_abort(&self) -> Option<Session> {
        match self.store.open(&self.session_id) {
            Ok(writer) => Some(self.resume_with(writer)),
            Err(e) => {
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("could not reopen session after abort: {e}"),
                });
                None
            }
        }
    }

    /// Attach the bus to `writer` and rebuild the runtime from the log — the
    /// session to resume with after a turn's live pair was consumed (abort,
    /// hard error) or is simply being reused.
    fn resume_with(&self, writer: SessionWriter) -> Session {
        let events = self.store.read_events(&self.session_id).unwrap_or_default();
        let runtime = crate::agent::rebuild_runtime(&events, self.system.clone());
        (writer.with_bus(self.bus.clone()), runtime)
    }

    /// Summarize and switch to a compaction session, following it as the actor's
    /// new session. On failure, keep the current session and emit a notice.
    async fn compact(&mut self, session: Session, keep_last: Option<usize>) -> Session {
        let (writer, runtime) = session;
        let snapshot = match self.agent.compact(&runtime, keep_last).await {
            Ok(Some(s)) => s,
            Ok(None) => return (writer, runtime), // nothing to compact
            Err(e) => {
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("compaction failed: {e}"),
                });
                return (writer, runtime);
            }
        };

        let old_sid = writer.session_id().clone();
        let meta = match self.store.read_meta(&old_sid) {
            Ok(m) => m,
            Err(e) => {
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("compaction failed (read meta): {e}"),
                });
                return (writer, runtime);
            }
        };
        match self.store.create_compaction(
            old_sid,
            meta.profile_id,
            meta.model,
            meta.workspace,
            Vec::new(),
            &snapshot,
        ) {
            Ok(new_writer) => {
                let new_writer = new_writer.with_bus(self.bus.clone());
                self.session_id = new_writer.session_id().clone();
                let _ = self.outbound.send(GatewayEvent::Compacted {
                    new_session_id: self.session_id.0.clone(),
                });
                (new_writer, SessionRuntime::new(snapshot))
            }
            Err(e) => {
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("compaction failed (create): {e}"),
                });
                (writer, runtime)
            }
        }
    }
}

/// Publish a terminal `Idle` when the actor task ends, on *every* exit path
/// (turn settled, idle-eviction, all-handles-dropped, `Shutdown`). `run` owns
/// `self`, so this drops exactly when the loop returns; it reads the *current*
/// `session_id` (correct after a compaction follow). `mark_idle` is a no-op when
/// the session is already `Idle`, so a clean settle-then-exit publishes at most
/// one `Idle` — this only fires the safety-net transition when an actor dies
/// mid-turn, so the list never shows a stuck spinner for a session with no actor.
impl Drop for SessionActor {
    fn drop(&mut self) {
        self.status.mark_idle(&self.session_id);
    }
}

/// Forward every committed [`CoreEvent`] from the session bus onto the outbound
/// [`GatewayEvent`] stream, tagged with its seq for SSE resume, and keep
/// `latest_seq` current (the tail seq the actor stamps on published statuses).
/// Runs until the bus has no more senders (the actor and its writer dropped).
fn spawn_event_forwarder(
    bus: &EventBus,
    outbound: broadcast::Sender<GatewayEvent>,
    latest_seq: Arc<AtomicU64>,
) {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Seqs are monotonic per session, so `max` is defensive against
                    // any out-of-order delivery — it never regresses the tail.
                    latest_seq.fetch_max(event.seq, Ordering::Relaxed);
                    let _ = outbound.send(GatewayEvent::Event {
                        event: Box::new(event),
                    });
                }
                // Lagged: skip the gap; the client resyncs committed events from
                // the log on reconnect.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// The turn id left open at the tail of the log, if any. Turns never overlap, so
/// the last turn-lifecycle event decides: a `Started`/`Resumed` with no following
/// terminator means that turn is still open; a `Completed`/`Failed`/`Interrupted`
/// means none is. Returns `None` when no turn is open (nothing to terminate).
fn open_turn_id(events: &[CoreEvent]) -> Option<TurnId> {
    events.iter().rev().find_map(|e| match &e.payload {
        EventPayload::Turn(
            TurnEvent::Started { turn_id, .. } | TurnEvent::Resumed { turn_id, .. },
        ) => Some(Some(turn_id.clone())),
        EventPayload::Turn(
            TurnEvent::Completed { .. } | TurnEvent::Failed { .. } | TurnEvent::Interrupted { .. },
        ) => Some(None),
        _ => None,
    })?
}

/// A [`StreamSink`] that forwards each live delta onto the session's outbound
/// broadcast as a [`GatewayEvent::Delta`].
struct BroadcastSink {
    tx: broadcast::Sender<GatewayEvent>,
}

impl StreamSink for BroadcastSink {
    fn on_block_start(&mut self, index: u32, block: BlockKind<'_>) {
        let delta = match block {
            BlockKind::Text => Delta::BlockStart {
                index,
                kind: "text",
                tool: None,
            },
            BlockKind::Reasoning => Delta::BlockStart {
                index,
                kind: "reasoning",
                tool: None,
            },
            BlockKind::ToolCall { name } => Delta::BlockStart {
                index,
                kind: "tool_call",
                tool: Some(name.to_owned()),
            },
        };
        let _ = self.tx.send(GatewayEvent::Delta(delta));
    }

    fn on_text(&mut self, index: u32, text: &str) {
        let _ = self.tx.send(GatewayEvent::Delta(Delta::Text {
            index,
            text: text.to_owned(),
        }));
    }

    fn on_reasoning(&mut self, index: u32, text: &str) {
        let _ = self.tx.send(GatewayEvent::Delta(Delta::Reasoning {
            index,
            text: text.to_owned(),
        }));
    }

    fn on_tool_call_delta(&mut self, index: u32, json_delta: &str) {
        let _ = self.tx.send(GatewayEvent::Delta(Delta::ToolArgs {
            index,
            json: json_delta.to_owned(),
        }));
    }

    fn on_context(&mut self, tokens: u32, window: u32, threshold: f32) {
        let _ = self.tx.send(GatewayEvent::ContextUpdated {
            tokens,
            window,
            threshold,
        });
    }

    fn on_retry(
        &mut self,
        attempt: u32,
        max_retries: u32,
        delay: std::time::Duration,
        error: &str,
    ) {
        let _ = self.tx.send(GatewayEvent::Delta(Delta::Retrying {
            attempt,
            max_retries,
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            error: error.to_owned(),
        }));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::agent::AgentConfig;
    use crate::core::payload::{ContentBlockType, StopReason, Usage};
    use crate::llm::{EventStream, LlmError, ModelRequest, Provider, StreamEvent};
    use crate::tool::ToolRegistry;
    use futures_util::StreamExt as _;
    use futures_util::stream;
    use std::sync::Mutex;

    /// A provider that replays one scripted batch of stream events per `stream()`
    /// call, so a turn runs deterministically without a network.
    struct ScriptedProvider {
        rounds: Mutex<VecDeque<Vec<StreamEvent>>>,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        #[allow(clippy::unnecessary_literal_bound)]
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

    /// One model round that answers with `text` and ends the turn cleanly.
    fn answer(text: &str) -> Vec<StreamEvent> {
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

    /// Build an actor over a fresh session in a temp store, scripted to produce
    /// `rounds` (one batch per turn). Returns the handle and the temp dir (kept
    /// alive so the store outlives the test).
    fn spawn_test_actor(rounds: Vec<Vec<StreamEvent>>) -> (ActorHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let provider = Arc::new(ScriptedProvider {
            rounds: Mutex::new(rounds.into_iter().collect()),
        });
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let writer = store.create_new(None, None, vec![]).unwrap();
        let runtime = SessionRuntime::new(system.clone());
        let handle = SessionActor::spawn(
            agent,
            store,
            system,
            (writer, runtime),
            std::time::Duration::from_secs(3600),
            Vec::new(),
            StatusHub::new(),
            None,
        );
        (handle, dir)
    }

    /// Like [`spawn_test_actor`] but with an explicit idle timeout and a shared
    /// [`StatusHub`], so a test can subscribe to the actor's status transitions
    /// and (with a short timeout) exercise idle-eviction.
    fn spawn_test_actor_with_status(
        rounds: Vec<Vec<StreamEvent>>,
        idle_timeout: std::time::Duration,
        status: StatusHub,
    ) -> (ActorHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let provider = Arc::new(ScriptedProvider {
            rounds: Mutex::new(rounds.into_iter().collect()),
        });
        let agent = Agent::new(
            provider,
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let writer = store.create_new(None, None, vec![]).unwrap();
        let runtime = SessionRuntime::new(system.clone());
        let handle = SessionActor::spawn(
            agent,
            store,
            system,
            (writer, runtime),
            idle_timeout,
            Vec::new(),
            status,
            None,
        );
        (handle, dir)
    }

    /// A turn publishes `Running` then `Idle` to the status hub, and the `Idle`
    /// carries the session's tail seq. This is the session list's whole signal:
    /// a spinner while the turn runs, resting after — asserted independently of
    /// the outbound event stream that drives the open-conversation view.
    #[tokio::test]
    async fn turn_publishes_running_then_idle_status() {
        let hub = StatusHub::new();
        let mut rx = hub.subscribe();
        let (handle, _dir) = spawn_test_actor_with_status(
            vec![answer("hello")],
            std::time::Duration::from_secs(3600),
            hub,
        );
        handle
            .send(Command::Send {
                text: "hi".to_owned(),
            })
            .await
            .unwrap();

        let mut saw_running = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(s)) if s.status == ActivityStatus::Running => saw_running = true,
                Ok(Ok(s)) if s.status == ActivityStatus::Idle => {
                    assert!(saw_running, "Running must precede Idle");
                    // A committed turn (Started..Completed) advanced the seq past 0.
                    assert!(s.latest_seq > 0, "Idle carries the committed tail seq");
                    return;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        panic!("did not observe Running→Idle status");
    }

    /// Dropping the actor (idle-eviction, all-handles-dropped, shutdown) publishes
    /// a terminal `Idle` via the Drop guard, so a session whose actor is gone never
    /// stays stuck showing `Running` in the list. Here a very short idle timeout
    /// evicts the actor after its turn; we assert the hub ends on `Idle`.
    #[tokio::test]
    async fn dropped_actor_publishes_terminal_idle() {
        let hub = StatusHub::new();
        let sid = {
            // Run one turn, then let the short idle timeout evict the actor.
            let (handle, _dir) = spawn_test_actor_with_status(
                vec![answer("bye")],
                std::time::Duration::from_millis(50),
                hub.clone(),
            );
            handle
                .send(Command::Send {
                    text: "hi".to_owned(),
                })
                .await
                .unwrap();
            // Give the turn time to settle and the idle timeout to fire (evicting
            // the actor → Drop → terminal Idle).
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            hub.snapshot()
                .into_iter()
                .next()
                .expect("the actor published at least one status")
                .session_id
        };

        let snap = hub.snapshot();
        let entry = snap
            .iter()
            .find(|s| s.session_id == sid)
            .expect("session status present after eviction");
        assert_eq!(
            entry.status,
            ActivityStatus::Idle,
            "an evicted actor must leave the session Idle, not stuck Running"
        );
    }

    /// A `Send` runs a turn: the outbound stream carries committed events and a
    /// terminal `TurnSettled`. This is the core actor contract a gateway client
    /// relies on.
    #[tokio::test]
    async fn send_runs_a_turn_and_emits_settled() {
        let (handle, _dir) = spawn_test_actor(vec![answer("hello")]);
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "hi".to_owned(),
            })
            .await
            .unwrap();

        // Collect until TurnSettled or timeout.
        let mut saw_event = false;
        let mut settled = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(GatewayEvent::Event { .. })) => saw_event = true,
                Ok(Ok(GatewayEvent::TurnSettled { incomplete })) => {
                    assert!(incomplete.is_none(), "turn should finish cleanly");
                    settled = true;
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(saw_event, "should have seen at least one committed event");
        assert!(settled, "should have seen TurnSettled");
    }

    /// Two `Send`s on one session run sequentially (turns never overlap): the
    /// actor processes the second only after the first settles, so we see two
    /// `TurnSettled` in order without interleaving.
    #[tokio::test]
    async fn two_sends_serialize() {
        let (handle, _dir) = spawn_test_actor(vec![answer("one"), answer("two")]);
        let mut rx = handle.subscribe();

        handle
            .send(Command::Send {
                text: "first".to_owned(),
            })
            .await
            .unwrap();
        handle
            .send(Command::Send {
                text: "second".to_owned(),
            })
            .await
            .unwrap();

        let mut settled = 0;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while settled < 2 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(GatewayEvent::TurnSettled { .. })) => settled += 1,
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert_eq!(settled, 2, "both queued turns should run and settle");
    }

    /// A provider whose stream never completes: it emits one text block then
    /// parks forever, so the turn task is still running when we cancel it. This
    /// is the precondition for the abort path (`cancel_turn`) — a finished turn
    /// would have nothing to abort.
    struct HangingProvider;

    #[async_trait::async_trait]
    impl Provider for HangingProvider {
        #[allow(clippy::unnecessary_literal_bound)]
        fn name(&self) -> &str {
            "hanging"
        }

        async fn stream(&self, _request: ModelRequest) -> Result<EventStream, LlmError> {
            let head = stream::iter(vec![
                Ok(StreamEvent::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                }),
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: "working".to_owned(),
                }),
            ]);
            // Never yields again: the turn parks here until aborted.
            let tail = stream::pending::<Result<StreamEvent, LlmError>>();
            Ok(Box::pin(head.chain(tail)))
        }
    }

    /// Cancelling a running turn must leave a durable terminator in the log.
    /// Without it, replaying the session (which never re-sends the live
    /// `TurnSettled`) ends on a dangling `Turn::Started`, so a client can't tell
    /// the turn stopped and a turn-running UI stays stuck on. We assert the log's
    /// last event is `Turn::Interrupted` for the turn that was open.
    #[tokio::test]
    async fn cancel_persists_interrupted_terminator() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let agent = Agent::new(
            Arc::new(HangingProvider),
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let runtime = SessionRuntime::new(system.clone());
        let handle = SessionActor::spawn(
            agent,
            store.clone(),
            system,
            (writer, runtime),
            std::time::Duration::from_secs(3600),
            Vec::new(),
            StatusHub::new(),
            None,
        );

        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "hi".to_owned(),
            })
            .await
            .unwrap();

        // Wait until the turn is actually running (a committed Turn::Started has
        // been forwarded) before cancelling, so there's a task to abort.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            assert!(tokio::time::Instant::now() < deadline, "turn never started");
            if let Ok(Ok(GatewayEvent::Event { event })) =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
                && matches!(event.payload, EventPayload::Turn(TurnEvent::Started { .. }))
            {
                break;
            }
        }

        handle.send(Command::Cancel).await.unwrap();

        // Poll the log until the committed Interrupted lands (the append happens
        // after the abort completes, slightly after Cancel is accepted).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut last = None;
        while tokio::time::Instant::now() < deadline {
            let events = store.read_events(&sid).unwrap_or_default();
            if let Some(e) = events.last()
                && matches!(e.payload, EventPayload::Turn(TurnEvent::Interrupted { .. }))
            {
                last = Some(e.payload.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        match last {
            Some(EventPayload::Turn(TurnEvent::Interrupted { .. })) => {}
            other => panic!("log should end with Turn::Interrupted, got {other:?}"),
        }

        // And the fold the frontend uses must read this log as a finished turn,
        // i.e. open_turn_id finds nothing open after the terminator.
        let events = store.read_events(&sid).unwrap();
        assert_eq!(open_turn_id(&events), None, "turn must read as closed");
    }

    /// Build a committed event carrying `payload` at `seq` (test helper for the
    /// open-turn scan; only seq + payload matter to `open_turn_id`).
    fn ev(seq: u64, payload: EventPayload) -> CoreEvent {
        CoreEvent {
            schema_version: "ominiforge.event.v1".to_owned(),
            seq,
            session_id: SessionId("s".to_owned()),
            timestamp: chrono::Utc::now(),
            source: EventSource {
                kind: SourceKind::Runtime,
                id: "ominiforge".to_owned(),
            },
            parent_event_id: None,
            turn_id: None,
            payload,
        }
    }

    fn started(seq: u64, id: &str) -> CoreEvent {
        ev(
            seq,
            EventPayload::Turn(TurnEvent::Started {
                turn_id: TurnId(id.to_owned()),
                input: Some("hi".to_owned()),
            }),
        )
    }

    fn completed(seq: u64, id: &str) -> CoreEvent {
        ev(
            seq,
            EventPayload::Turn(TurnEvent::Completed {
                turn_id: TurnId(id.to_owned()),
            }),
        )
    }

    /// The open-turn scan underpins cancel's durable terminator: cancel must
    /// know *which* turn to mark Interrupted, and must not double-terminate an
    /// already-finished turn.

    #[test]
    fn open_turn_id_finds_the_dangling_started() {
        // A Started with no following terminator — exactly the post-abort log.
        let events = vec![started(1, "t1")];
        assert_eq!(open_turn_id(&events), Some(TurnId("t1".to_owned())));
    }

    #[test]
    fn open_turn_id_none_when_last_turn_completed() {
        // A cleanly finished turn must not be re-terminated on a stray cancel.
        let events = vec![started(1, "t1"), completed(2, "t1")];
        assert_eq!(open_turn_id(&events), None);
    }

    #[test]
    fn open_turn_id_tracks_the_latest_turn() {
        // Turn 1 finished, turn 2 is open: the open one is what cancel terminates.
        let events = vec![started(1, "t1"), completed(2, "t1"), started(3, "t2")];
        assert_eq!(open_turn_id(&events), Some(TurnId("t2".to_owned())));
    }

    #[test]
    fn open_turn_id_none_on_empty_log() {
        assert_eq!(open_turn_id(&[]), None);
    }

    #[test]
    fn open_turn_id_ignores_non_turn_tail_events() {
        // A non-Turn event after Started (here an Error) doesn't close the turn —
        // it's still open, so cancel still has a turn to terminate.
        let err = EventPayload::Error(crate::core::payload::ErrorEvent::Raised(
            crate::core::payload::ErrorDetail {
                code: "x".to_owned(),
                message: "boom".to_owned(),
                severity: crate::core::payload::ErrorSeverity::Error,
                retryable: false,
                source_event_id: None,
                provider_raw: None,
            },
        ));
        let events = vec![started(1, "t1"), ev(2, err)];
        assert_eq!(open_turn_id(&events), Some(TurnId("t1".to_owned())));
    }

    // ── Approval closed loop (Step 5, `doc/permission.md` §5) ────────────────

    /// One model round that calls the `write` tool, then (next round) answers.
    fn write_call(call_id: &str, path: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::ToolCall {
                    id: call_id.to_owned(),
                    name: "write".to_owned(),
                },
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: format!(r#"{{"path":"{path}","content":"hi"}}"#),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Completed {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    }

    /// Spawn an actor whose agent has a real `write` tool rooted at `workspace`
    /// and an `ask`-on-write permission policy, so a `write` call suspends for
    /// approval. Returns the handle, the temp store dir, and the workspace dir.
    fn spawn_actor_ask_on_write(
        rounds: Vec<Vec<StreamEvent>>,
    ) -> (ActorHandle, tempfile::TempDir, tempfile::TempDir) {
        let store_dir = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(store_dir.path());
        let provider = Arc::new(ScriptedProvider {
            rounds: Mutex::new(rounds.into_iter().collect()),
        });
        let mut tools = ToolRegistry::new();
        crate::tool::register_builtin(&mut tools, ws_dir.path().to_path_buf());
        let policy = crate::permission::PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![crate::permission::Rule::contains("write", vec![])],
        };
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        )
        .with_permission(policy);
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let writer = store
            .create_new(None, None, vec!["write".to_owned()])
            .unwrap();
        let runtime = SessionRuntime::new(system.clone());
        let handle = SessionActor::spawn(
            agent,
            store,
            system,
            (writer, runtime),
            std::time::Duration::from_secs(3600),
            Vec::new(),
            StatusHub::new(),
            None,
        );
        (handle, store_dir, ws_dir)
    }

    /// Wait for the next `ApprovalRequested` on the stream, returning its
    /// `call_id`. Panics on timeout — a suspended ask must announce itself.
    async fn wait_for_approval_request(rx: &mut broadcast::Receiver<GatewayEvent>) -> String {
        wait_for_approval_request_for(rx, "write").await
    }

    /// Like [`wait_for_approval_request`], but expecting a specific tool.
    async fn wait_for_approval_request_for(
        rx: &mut broadcast::Receiver<GatewayEvent>,
        tool: &str,
    ) -> String {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(GatewayEvent::ApprovalRequested {
                    call_id, tool_name, ..
                })) => {
                    assert_eq!(tool_name, tool);
                    return call_id;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        panic!("never saw an ApprovalRequested event");
    }

    /// Wait for `TurnSettled`. Returns once seen (or panics on timeout).
    async fn wait_for_settled(rx: &mut broadcast::Receiver<GatewayEvent>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(GatewayEvent::TurnSettled { .. })) => return,
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        panic!("never saw TurnSettled");
    }

    /// Approve closes the loop: an `ask` suspends the turn (announced via
    /// `ApprovalRequested`), a `Command::Approve` resumes it, and the tool runs
    /// (the file lands). Proves the decision — not the model — released the call.
    #[tokio::test]
    async fn approve_resumes_suspended_turn_and_runs_tool() {
        let (handle, _store, ws) =
            spawn_actor_ask_on_write(vec![write_call("call-1", "ok.txt"), answer("done")]);
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "write a file".to_owned(),
            })
            .await
            .unwrap();

        let call_id = wait_for_approval_request(&mut rx).await;
        assert!(
            !ws.path().join("ok.txt").exists(),
            "the tool must not run before approval"
        );

        handle
            .send(Command::Approve {
                call_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();

        wait_for_settled(&mut rx).await;
        assert!(
            ws.path().join("ok.txt").exists(),
            "an approved call runs the tool"
        );
    }

    /// Reject closes the loop the other way: the suspended call is blocked with
    /// `denied_by_user` and the file never lands.
    #[tokio::test]
    async fn reject_blocks_suspended_turn() {
        let (handle, _store, ws) =
            spawn_actor_ask_on_write(vec![write_call("call-9", "no.txt"), answer("blocked")]);
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "write a file".to_owned(),
            })
            .await
            .unwrap();

        let call_id = wait_for_approval_request(&mut rx).await;
        handle
            .send(Command::Approve {
                call_id,
                decision: ApprovalDecision::Reject,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();

        // Drain until settled, checking a denied_by_user tool failure committed.
        let mut denied = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(GatewayEvent::Event { event })) => {
                    if let EventPayload::Tool(crate::core::payload::ToolEvent::Failed {
                        error, ..
                    }) = &event.payload
                        && error.code == "denied_by_user"
                    {
                        denied = true;
                    }
                }
                Ok(Ok(GatewayEvent::TurnSettled { .. }) | Err(_)) | Err(_) => break,
                Ok(Ok(_)) => {}
            }
        }
        assert!(denied, "a rejected call surfaces as denied_by_user");
        assert!(
            !ws.path().join("no.txt").exists(),
            "a rejected call never runs the tool"
        );
    }

    /// M1 regression: if every `ActorHandle` is dropped while a turn is parked on
    /// an `ask`, the actor must not hang. The `None` inbox arm clears pending
    /// waiters (fail-closed) so the suspended turn settles and the actor task
    /// exits — releasing the session lock. We detect the exit by the outbound
    /// broadcast closing (all senders live inside the actor task); a hang would
    /// instead time out.
    #[tokio::test]
    async fn dropping_handles_during_pending_ask_does_not_hang() {
        let (handle, _store, ws) =
            spawn_actor_ask_on_write(vec![write_call("call-x", "x.txt"), answer("done")]);
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "write a file".to_owned(),
            })
            .await
            .unwrap();

        // Wait until the turn is genuinely parked on the ask.
        let _call_id = wait_for_approval_request(&mut rx).await;

        // Drop the only handle without sending Approve/Cancel/Shutdown: this is
        // the `None` inbox path. Pre-fix, the parked turn would hang forever.
        drop(handle);

        // The actor should tear down: drain the outbound until it closes. Bounded
        // so a regression (hang) fails the test via timeout instead of blocking.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return true,
                }
            }
        })
        .await;
        assert_eq!(
            closed,
            Ok(true),
            "actor must exit, not hang, on dropped handles mid-ask"
        );
        // Fail-closed: the tool never ran.
        assert!(
            !ws.path().join("x.txt").exists(),
            "a turn torn down mid-ask must not have run the tool"
        );
    }

    // ── Cancel robustness + parallel approvals (`doc/permission.md` §5) ──────

    /// A tool whose `invoke` never completes, so the turn is inside the tool
    /// (not the approval gate) when the cancel lands.
    struct HangingTool;

    #[async_trait::async_trait]
    impl crate::tool::Tool for HangingTool {
        fn descriptor(&self) -> crate::tool::ToolDescriptor {
            crate::tool::ToolDescriptor {
                name: "hang".to_owned(),
                description: "never returns".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn invoke(&self, _input: crate::tool::ToolInput) -> crate::tool::ToolResult {
            std::future::pending().await
        }
    }

    /// One model round that calls the `hang` tool, so a cancel lands
    /// mid-execution rather than mid-approval.
    fn hang_call(call_id: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::BlockStart {
                index: 0,
                block_type: ContentBlockType::ToolCall {
                    id: call_id.to_owned(),
                    name: "hang".to_owned(),
                },
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                json_delta: "{}".to_owned(),
            },
            StreamEvent::BlockStop { index: 0 },
            StreamEvent::Completed {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            },
        ]
    }

    /// One model round issuing two `write` calls (block index 0 and 1), so the
    /// round holds two asks to dispatch at once.
    fn two_write_calls(call1: &str, path1: &str, call2: &str, path2: &str) -> Vec<StreamEvent> {
        let block = |index: u32, call_id: &str, path: &str| {
            vec![
                StreamEvent::BlockStart {
                    index,
                    block_type: ContentBlockType::ToolCall {
                        id: call_id.to_owned(),
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
        let mut events = block(0, call1, path1);
        events.extend(block(1, call2, path2));
        events.push(StreamEvent::Completed {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        events
    }

    /// How one tool-call content block settled in the log — the invariant
    /// behind the provider's call↔result pairing constraint.
    #[derive(Debug, PartialEq, Eq)]
    enum CallPairing {
        /// No `Completed`/`Failed` ever paired the call — the poison that 400s
        /// the next turn's provider request.
        Dangling,
        Completed,
        Failed(String),
    }

    /// Pair every tool-call content block in the log with its outcome.
    fn pair_log(events: &[CoreEvent]) -> Vec<(String, CallPairing)> {
        let mut calls: Vec<(u64, String)> = Vec::new();
        let mut completed: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut failed: HashMap<u64, String> = HashMap::new();
        for event in events {
            match &event.payload {
                EventPayload::Model(ModelEvent::ContentBlock {
                    content: BlockContent::ToolCall { id, .. },
                    ..
                }) => calls.push((event.seq, id.clone())),
                EventPayload::Tool(ToolEvent::Completed {
                    tool_call_event_id, ..
                }) => {
                    completed.insert(tool_call_event_id.seq);
                }
                EventPayload::Tool(ToolEvent::Failed {
                    tool_call_event_id,
                    error,
                    ..
                }) => {
                    failed.insert(tool_call_event_id.seq, error.code.clone());
                }
                _ => {}
            }
        }
        calls
            .into_iter()
            .map(|(seq, id)| {
                let pairing = failed.get(&seq).map_or_else(
                    || {
                        if completed.contains(&seq) {
                            CallPairing::Completed
                        } else {
                            CallPairing::Dangling
                        }
                    },
                    |code| CallPairing::Failed(code.clone()),
                );
                (id, pairing)
            })
            .collect()
    }

    /// Read the single session's committed log from a temp store dir.
    fn read_log(store_dir: &tempfile::TempDir) -> Vec<CoreEvent> {
        let store = SessionStore::new(store_dir.path());
        let ids = store.list().unwrap();
        assert_eq!(ids.len(), 1, "test stores hold exactly one session");
        store.read_events(&ids[0]).unwrap()
    }

    /// Wait for the committed `Turn::Interrupted` on the stream (it lands
    /// slightly after Cancel is accepted). Panics on timeout.
    async fn wait_for_interrupted(rx: &mut broadcast::Receiver<GatewayEvent>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(GatewayEvent::Event { event }))
                    if matches!(
                        event.payload,
                        EventPayload::Turn(TurnEvent::Interrupted { .. })
                    ) =>
                {
                    return;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        panic!("never saw Turn::Interrupted");
    }

    /// Wait until a `ToolEvent::Started` commits — the tool is mid-execution.
    async fn wait_for_tool_started(rx: &mut broadcast::Receiver<GatewayEvent>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(GatewayEvent::Event { event }))
                    if matches!(event.payload, EventPayload::Tool(ToolEvent::Started { .. })) =>
                {
                    return;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        panic!("never saw ToolEvent::Started");
    }

    /// Cancel during an `ask`: the abort kills the turn task before any failure
    /// is written, leaving the assistant's `tool_call` dangling — and the next
    /// provider request 400s on the unpaired call. `cancel_turn` must backfill
    /// a `cancelled` failure so every call in the log pairs with a result.
    #[tokio::test]
    async fn cancel_during_ask_backfills_cancelled_failure() {
        let (handle, store_dir, ws) =
            spawn_actor_ask_on_write(vec![write_call("call-1", "x.txt"), answer("done")]);
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "write a file".to_owned(),
            })
            .await
            .unwrap();

        let _call_id = wait_for_approval_request(&mut rx).await;
        handle.send(Command::Cancel).await.unwrap();
        wait_for_interrupted(&mut rx).await;

        let events = read_log(&store_dir);
        assert_eq!(
            pair_log(&events),
            vec![(
                "call-1".to_owned(),
                CallPairing::Failed("cancelled".to_owned())
            )],
        );
        // The terminator still tails the log (the backfill lands before it).
        assert!(matches!(
            events.last().map(|e| &e.payload),
            Some(EventPayload::Turn(TurnEvent::Interrupted { .. }))
        ));
        assert!(!ws.path().join("x.txt").exists(), "the tool never ran");
    }

    /// Cancel mid-execution: the hanging tool never produces a result event, so
    /// without the backfill its call would dangle in the log — the same
    /// provider-400 poison as a cancel mid-ask.
    #[tokio::test]
    async fn cancel_during_execution_backfills_cancelled_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let provider = Arc::new(ScriptedProvider {
            rounds: Mutex::new(
                vec![hang_call("call-1"), answer("done")]
                    .into_iter()
                    .collect(),
            ),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HangingTool));
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let writer = store
            .create_new(None, None, vec!["hang".to_owned()])
            .unwrap();
        let sid = writer.session_id().clone();
        let runtime = SessionRuntime::new(system.clone());
        let handle = SessionActor::spawn(
            agent,
            store.clone(),
            system,
            (writer, runtime),
            std::time::Duration::from_secs(3600),
            Vec::new(),
            StatusHub::new(),
            None,
        );

        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "run".to_owned(),
            })
            .await
            .unwrap();
        wait_for_tool_started(&mut rx).await;
        handle.send(Command::Cancel).await.unwrap();
        wait_for_interrupted(&mut rx).await;

        let events = store.read_events(&sid).unwrap();
        assert_eq!(
            pair_log(&events),
            vec![(
                "call-1".to_owned(),
                CallPairing::Failed("cancelled".to_owned())
            )],
        );
    }

    /// A cleanly settled turn's calls must stay untouched: a cancel in a LATER
    /// turn backfills only that turn's dangling calls.
    #[tokio::test]
    async fn cancel_backfills_only_the_open_turns_dangling_calls() {
        let (handle, store_dir, ws) = spawn_actor_ask_on_write(vec![
            write_call("call-1", "ok.txt"),
            answer("done"),
            write_call("call-2", "no.txt"),
            answer("unused"),
        ]);
        let mut rx = handle.subscribe();
        // Turn 1: approved to completion — call-1 gets a real `Completed`.
        handle
            .send(Command::Send {
                text: "write a file".to_owned(),
            })
            .await
            .unwrap();
        let call_id = wait_for_approval_request(&mut rx).await;
        handle
            .send(Command::Approve {
                call_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();
        wait_for_settled(&mut rx).await;
        assert!(ws.path().join("ok.txt").exists());

        // Turn 2: the ask suspends again; this time cancel.
        handle
            .send(Command::Send {
                text: "write another".to_owned(),
            })
            .await
            .unwrap();
        let _call_id = wait_for_approval_request(&mut rx).await;
        handle.send(Command::Cancel).await.unwrap();
        wait_for_interrupted(&mut rx).await;

        let events = read_log(&store_dir);
        assert_eq!(
            pair_log(&events),
            vec![
                ("call-1".to_owned(), CallPairing::Completed),
                (
                    "call-2".to_owned(),
                    CallPairing::Failed("cancelled".to_owned())
                ),
            ],
        );
        assert!(!ws.path().join("no.txt").exists());
    }

    /// A cancelled turn with no tool calls backfills nothing — the scan must
    /// not invent failures for a turn that never dispatched.
    #[tokio::test]
    async fn cancel_without_tool_calls_backfills_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let agent = Agent::new(
            Arc::new(HangingProvider),
            ToolRegistry::new(),
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        );
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        let runtime = SessionRuntime::new(system.clone());
        let handle = SessionActor::spawn(
            agent,
            store.clone(),
            system,
            (writer, runtime),
            std::time::Duration::from_secs(3600),
            Vec::new(),
            StatusHub::new(),
            None,
        );

        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "hi".to_owned(),
            })
            .await
            .unwrap();
        // Wait until the turn is actually running before cancelling.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            assert!(tokio::time::Instant::now() < deadline, "turn never started");
            if let Ok(Ok(GatewayEvent::Event { event })) =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
                && matches!(event.payload, EventPayload::Turn(TurnEvent::Started { .. }))
            {
                break;
            }
        }
        handle.send(Command::Cancel).await.unwrap();
        wait_for_interrupted(&mut rx).await;

        let events = store.read_events(&sid).unwrap();
        assert!(pair_log(&events).is_empty(), "no tool calls to pair");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.payload, EventPayload::Tool(_))),
            "no tool events may be written for a tool-less turn"
        );
    }

    /// Two asks in one round: BOTH `ApprovalRequested`s fire before any answer
    /// (two-phase dispatch — the old serial loop starved the second call behind
    /// the first ask), and a cancel with both in flight kills both waiters and
    /// pairs both calls.
    #[tokio::test]
    async fn two_asks_are_published_together_and_cancel_pairs_both() {
        let (handle, store_dir, ws) = spawn_actor_ask_on_write(vec![
            two_write_calls("call-1", "a.txt", "call-2", "b.txt"),
            answer("done"),
        ]);
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "write both".to_owned(),
            })
            .await
            .unwrap();

        let first = wait_for_approval_request(&mut rx).await;
        let second = wait_for_approval_request(&mut rx).await;
        assert_ne!(first, second, "both asks must publish before any answer");

        handle.send(Command::Cancel).await.unwrap();
        wait_for_interrupted(&mut rx).await;

        let events = read_log(&store_dir);
        assert_eq!(
            pair_log(&events),
            vec![
                (
                    "call-1".to_owned(),
                    CallPairing::Failed("cancelled".to_owned())
                ),
                (
                    "call-2".to_owned(),
                    CallPairing::Failed("cancelled".to_owned())
                ),
            ],
        );
        assert!(!ws.path().join("a.txt").exists());
        assert!(!ws.path().join("b.txt").exists());
    }

    // ── Independent per-call chains (`doc/permission.md` §5) ─────────────────

    /// A tool that records each invocation's call id at entry (`started`) and
    /// at exit (`finished`) — proves WHICH call is executing at any moment,
    /// and whether it has finished (independent-chain tests).
    struct MarkingTool {
        started: Arc<Mutex<Vec<String>>>,
        finished: Arc<Mutex<Vec<String>>>,
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl crate::tool::Tool for MarkingTool {
        fn descriptor(&self) -> crate::tool::ToolDescriptor {
            crate::tool::ToolDescriptor {
                name: "gated".to_owned(),
                description: "records execution markers".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn invoke(&self, input: crate::tool::ToolInput) -> crate::tool::ToolResult {
            self.started.lock().unwrap().push(input.call_id.clone());
            tokio::time::sleep(self.delay).await;
            self.finished.lock().unwrap().push(input.call_id);
            Ok(crate::core::payload::ToolOutput {
                content: vec![crate::core::payload::Content::Text("marked".to_owned())],
                is_error: false,
                error_code: None,
            })
        }
    }

    /// One model round issuing one `{}`-argument call per `(id, tool)` pair, in
    /// order.
    fn named_calls_round(calls: &[(&str, &str)]) -> Vec<StreamEvent> {
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

    /// Spawn an actor with a custom tool registry and an `ask`-on-`ask_tool`
    /// policy. Returns the handle and the temp store dir.
    fn spawn_actor_with_tools(
        rounds: Vec<Vec<StreamEvent>>,
        tools: ToolRegistry,
        ask_tool: &str,
    ) -> (ActorHandle, tempfile::TempDir) {
        let store_dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(store_dir.path());
        let provider = Arc::new(ScriptedProvider {
            rounds: Mutex::new(rounds.into_iter().collect()),
        });
        let policy = crate::permission::PermissionPolicy {
            deny: vec![],
            allow: vec![],
            ask: vec![crate::permission::Rule::contains(ask_tool, vec![])],
        };
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "mock".to_owned(),
                ..AgentConfig::default()
            },
        )
        .with_permission(policy);
        let system = vec![Message::System {
            content: "sys".to_owned(),
        }];
        let writer = store
            .create_new(None, None, vec![ask_tool.to_owned()])
            .unwrap();
        let runtime = SessionRuntime::new(system.clone());
        let handle = SessionActor::spawn(
            agent,
            store,
            system,
            (writer, runtime),
            std::time::Duration::from_secs(3600),
            Vec::new(),
            StatusHub::new(),
            None,
        );
        (handle, store_dir)
    }

    /// Poll `marks` until it contains `call_id` — a marker the tool pushes from
    /// inside `invoke`, so this returns the moment the execution starts.
    async fn wait_for_mark(marks: &Arc<Mutex<Vec<String>>>, call_id: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if marks.lock().unwrap().iter().any(|id| id == call_id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("never saw an execution mark for {call_id}");
    }

    /// Call ids in the order their result events (`Completed`/`Failed`)
    /// committed — proves the write-back stays in call order even when
    /// executions finish out of order.
    fn result_order(events: &[CoreEvent]) -> Vec<String> {
        let mut call_ids: HashMap<u64, String> = HashMap::new();
        for event in events {
            if let EventPayload::Model(ModelEvent::ContentBlock {
                content: BlockContent::ToolCall { id, .. },
                ..
            }) = &event.payload
            {
                call_ids.insert(event.seq, id.clone());
            }
        }
        events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::Tool(
                    ToolEvent::Completed {
                        tool_call_event_id, ..
                    }
                    | ToolEvent::Failed {
                        tool_call_event_id, ..
                    },
                ) => call_ids.get(&tool_call_event_id.seq).cloned(),
                _ => None,
            })
            .collect()
    }

    /// Poll the log until a `PermissionEvent::Decided` for `call_id` commits —
    /// the moment the human's decision becomes durable (and visible to a
    /// front-end folding the log).
    async fn wait_for_decided(store_dir: &tempfile::TempDir, call_id: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let events = read_log(store_dir);
            if events.iter().any(|e| {
                matches!(
                    &e.payload,
                    EventPayload::Permission(crate::core::payload::PermissionEvent::Decided {
                        call_id: id,
                        ..
                    }) if id == call_id
                )
            }) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("never saw a Decided for {call_id}");
    }

    /// Independent chains: approving call #2 first starts #2's execution while
    /// call #1 is still undecided — and #2's `Decided` commits at once (the
    /// approval is immediately visible), not queued behind #1's chain. Result
    /// events commit in *completion* order (#2 before #1), while the model
    /// still reads the tool results in `tool_call` order (#1 before #2).
    #[tokio::test]
    async fn second_approval_executes_before_the_first_is_decided() {
        let started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let finished: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MarkingTool {
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
            delay: std::time::Duration::from_millis(300),
        }));
        let (handle, store_dir) = spawn_actor_with_tools(
            vec![
                named_calls_round(&[("call-1", "gated"), ("call-2", "gated")]),
                answer("done"),
            ],
            tools,
            "gated",
        );
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "run both".to_owned(),
            })
            .await
            .unwrap();

        // Both asks publish before either is answered.
        let first = wait_for_approval_request_for(&mut rx, "gated").await;
        let second = wait_for_approval_request_for(&mut rx, "gated").await;
        assert_eq!((first.as_str(), second.as_str()), ("call-1", "call-2"));

        // Approve #2 FIRST: its chain must start executing while #1 is still
        // undecided — no waiting on the earlier call's gate.
        handle
            .send(Command::Approve {
                call_id: "call-2".to_owned(),
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();
        wait_for_mark(&started, "call-2").await;
        assert!(
            !started.lock().unwrap().iter().any(|id| id == "call-1"),
            "call-1 must not execute before its own approval"
        );
        // …and #2's `Decided` commits immediately — while #1 is still
        // undecided — so a front-end folding the log clears the card at once.
        wait_for_decided(&store_dir, "call-2").await;
        let events = read_log(&store_dir);
        assert!(
            !events.iter().any(|e| matches!(
                &e.payload,
                EventPayload::Permission(crate::core::payload::PermissionEvent::Decided {
                    call_id,
                    ..
                }) if call_id == "call-1"
            )),
            "call-1 must still be undecided when call-2's decision commits"
        );

        // Approve #1 too: the turn settles. #2 was decided and executed first,
        // so its `Completed` committed first — the front-end saw it the moment
        // it finished — while the model still reads the results in call order.
        handle
            .send(Command::Approve {
                call_id: "call-1".to_owned(),
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();
        wait_for_settled(&mut rx).await;
        let events = read_log(&store_dir);
        assert_eq!(
            result_order(&events),
            vec!["call-2", "call-1"],
            "result events commit in completion order"
        );
        let runtime = crate::agent::rebuild_runtime(&events, vec![]);
        let result_ids: Vec<&str> = runtime
            .context
            .iter()
            .filter_map(|m| match m {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            result_ids,
            ["call-1", "call-2"],
            "the model reads tool results in `tool_call` order"
        );
        assert_eq!(finished.lock().unwrap().len(), 2, "both chains executed");
    }

    /// Approval → immediate execution: a single ask starts its tool as soon as
    /// the decision lands — observed mid-execution (started, not yet finished).
    #[tokio::test]
    async fn approval_starts_execution_immediately() {
        let started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let finished: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MarkingTool {
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
            delay: std::time::Duration::from_millis(500),
        }));
        let (handle, store_dir) = spawn_actor_with_tools(
            vec![named_calls_round(&[("call-1", "gated")]), answer("done")],
            tools,
            "gated",
        );
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "run".to_owned(),
            })
            .await
            .unwrap();

        let call_id = wait_for_approval_request_for(&mut rx, "gated").await;
        handle
            .send(Command::Approve {
                call_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();

        // The execution begins the moment the approval lands: the start marker
        // fires long before the 500ms tool could finish — nothing else is
        // waited on first.
        wait_for_mark(&started, "call-1").await;
        assert!(
            finished.lock().unwrap().is_empty(),
            "caught mid-execution: the approval started the tool at once"
        );
        wait_for_settled(&mut rx).await;
        assert_eq!(
            pair_log(&read_log(&store_dir)),
            vec![("call-1".to_owned(), CallPairing::Completed)],
        );
    }

    /// Cancel recalls an executing chain: an approved, mid-`invoke` chain is
    /// aborted — its side effect never completes (no `finished` marker) and
    /// the log pairs the call with a `cancelled` failure (`doc/permission.md`
    /// §5.2). Pre-fix, the detached chain ran to completion while the log said
    /// cancelled.
    #[tokio::test]
    async fn cancel_aborts_an_executing_chain_before_its_side_effect() {
        let started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let finished: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MarkingTool {
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
            // Effectively never finishes on its own — the abort is what stops it.
            delay: std::time::Duration::from_secs(3600),
        }));
        let (handle, store_dir) = spawn_actor_with_tools(
            vec![named_calls_round(&[("call-1", "gated")]), answer("done")],
            tools,
            "gated",
        );
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "run".to_owned(),
            })
            .await
            .unwrap();

        let call_id = wait_for_approval_request_for(&mut rx, "gated").await;
        handle
            .send(Command::Approve {
                call_id,
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Once,
            })
            .await
            .unwrap();
        // The chain is now mid-`invoke` (started, nowhere near finished).
        wait_for_mark(&started, "call-1").await;

        handle.send(Command::Cancel).await.unwrap();
        wait_for_interrupted(&mut rx).await;

        assert!(
            finished.lock().unwrap().is_empty(),
            "the chain was aborted mid-invoke — the side effect never completed"
        );
        assert_eq!(
            pair_log(&read_log(&store_dir)),
            vec![(
                "call-1".to_owned(),
                CallPairing::Failed("cancelled".to_owned())
            )],
        );
    }

    /// Same-round pin: approving one of three parallel asks with `session`
    /// scope pins an allow rule, and the matching still-pending asks
    /// auto-approve against it — audited as the rule's decision (`"policy"`),
    /// not a human's — and execute without further answers (`doc/permission.md`
    /// §5.1).
    #[tokio::test]
    async fn session_pin_auto_approves_matching_pending_asks() {
        let started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let finished: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MarkingTool {
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
            delay: std::time::Duration::ZERO,
        }));
        let (handle, store_dir) = spawn_actor_with_tools(
            vec![
                named_calls_round(&[
                    ("call-1", "gated"),
                    ("call-2", "gated"),
                    ("call-3", "gated"),
                ]),
                answer("done"),
            ],
            tools,
            "gated",
        );
        let mut rx = handle.subscribe();
        handle
            .send(Command::Send {
                text: "run all three".to_owned(),
            })
            .await
            .unwrap();

        // All three asks publish.
        for _ in 0..3 {
            wait_for_approval_request_for(&mut rx, "gated").await;
        }

        // Approve ONLY call-1, with `session` scope: the pin re-evaluates the
        // other two against the new rule and auto-approves them.
        handle
            .send(Command::Approve {
                call_id: "call-1".to_owned(),
                decision: ApprovalDecision::Approve,
                scope: ApprovalScope::Session,
            })
            .await
            .unwrap();
        wait_for_settled(&mut rx).await;

        // All three executed — call-2/3 without any further human answer.
        assert_eq!(finished.lock().unwrap().len(), 3, "all three chains ran");
        let events = read_log(&store_dir);
        let decided = |id: &str| {
            events.iter().find_map(|e| match &e.payload {
                EventPayload::Permission(crate::core::payload::PermissionEvent::Decided {
                    call_id,
                    decided_by,
                    scope,
                    ..
                }) if call_id == id => Some((decided_by.clone(), *scope)),
                _ => None,
            })
        };
        assert_eq!(
            decided("call-1"),
            Some(("user".to_owned(), Some(ApprovalScope::Session))),
            "the human answered call-1, scoped to the session"
        );
        for id in ["call-2", "call-3"] {
            assert_eq!(
                decided(id),
                Some(("policy".to_owned(), Some(ApprovalScope::Session))),
                "{id} was auto-approved by the pinned rule, audited as the rule's decision"
            );
        }
        assert_eq!(
            pair_log(&events),
            vec![
                ("call-1".to_owned(), CallPairing::Completed),
                ("call-2".to_owned(), CallPairing::Completed),
                ("call-3".to_owned(), CallPairing::Completed),
            ],
        );
    }
}
