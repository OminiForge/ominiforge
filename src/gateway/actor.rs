//! [`SessionActor`]: one tokio task that owns a single live session.
//!
//! The session store enforces a single writer per session via an OS file lock
//! held for the [`SessionWriter`]'s lifetime (`src/session`). A network gateway
//! has many clients fanning into one session, so they must serialize through one
//! owner — this actor. It owns the `(SessionWriter, SessionRuntime)` pair between
//! turns (exactly as the TUI holds it between turns) and processes commands from
//! an mpsc inbox one at a time, so two turns never interleave on one session.
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
//! dropped (releasing the lock) and the actor rebuilds the runtime from the log
//! — the same recovery the TUI uses, grounded in the log being the source of
//! truth.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::agent::{Agent, BlockKind, SessionRuntime, StreamSink, TurnOutcome};
use crate::core::{CoreEvent, SessionId};
use crate::llm::Message;
use crate::session::{EventBus, SessionStore, SessionWriter};

/// Capacity of the per-session outbound broadcast. A subscriber that lags past
/// this many buffered items gets a `Lagged` error and should resync from the log
/// (committed events) — deltas it missed are simply gone (ephemeral).
const OUTBOUND_CAPACITY: usize = 1024;

/// Capacity of an actor's command inbox.
const INBOX_CAPACITY: usize = 64;

/// What a front-end sees on the wire for one session. Tagged JSON so a client
/// can switch on `type`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayEvent {
    /// A committed, persisted event. `seq` is its session sequence number — the
    /// SSE `Last-Event-ID` for resume.
    Event {
        seq: u64,
        #[serde(flatten)]
        event: Box<CoreEvent>,
    },
    /// A live token-level delta. Ephemeral: not persisted, not replayed on
    /// reconnect (the committed `Event` is the authoritative record).
    Delta(Delta),
    /// A turn settled. `incomplete` is `None` on a clean finish, else a short
    /// reason (round budget, plan stall, hook block).
    TurnSettled { incomplete: Option<String> },
    /// The session was compacted into a new one; the client should follow
    /// `new_session_id` for subsequent events.
    Compacted { new_session_id: String },
    /// A non-fatal note (compaction failure, etc.) for display.
    Notice { message: String },
}

/// A live streaming delta, mirroring [`StreamSink`] callbacks.
#[derive(Debug, Clone, Serialize)]
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
type TurnResult = Result<(SessionWriter, SessionRuntime, TurnOutcome), crate::agent::AgentError>;

/// One live session's driver. Spawned by the registry; runs until idle-evicted
/// or told to shut down.
pub struct SessionActor {
    agent: Arc<Agent>,
    store: SessionStore,
    /// System seed for rebuilding the runtime after a cancel/abort.
    system: Vec<Message>,
    session_id: SessionId,
    bus: EventBus,
    outbound: broadcast::Sender<GatewayEvent>,
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
    /// lifetime (per-session MCP isolation).
    pub fn spawn(
        agent: Arc<Agent>,
        store: SessionStore,
        system: Vec<Message>,
        session: Session,
        idle_timeout: std::time::Duration,
        mcp_clients: Vec<Arc<crate::mcp::McpClient>>,
    ) -> ActorHandle {
        let (inbox_tx, inbox_rx) = mpsc::channel(INBOX_CAPACITY);
        let (outbound, _) = broadcast::channel(OUTBOUND_CAPACITY);

        let bus = EventBus::new();
        let session_id = session.0.session_id().clone();
        // Attach the actor's bus to the writer so every appended event is
        // published; the forwarder below turns those into outbound `Event`s.
        let session = (session.0.with_bus(bus.clone()), session.1);

        let actor = Self {
            agent,
            store,
            system,
            session_id,
            bus: bus.clone(),
            outbound: outbound.clone(),
            inbox: inbox_rx,
            idle_timeout,
            deferred: VecDeque::new(),
            _mcp_clients: mcp_clients,
        };

        // Forward committed events from the session bus onto the outbound stream.
        spawn_event_forwarder(&bus, outbound.clone());

        tokio::spawn(actor.run(session));

        ActorHandle {
            inbox: inbox_tx,
            outbound,
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
                        // exit so the CLI/TUI can reopen this session.
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
                // No turn running while idle — nothing to cancel.
                Command::Cancel => {}
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

        let mut handle: JoinHandle<TurnResult> = tokio::spawn(async move {
            let mut writer = writer;
            let mut runtime = runtime;
            let mut sink = BroadcastSink { tx: outbound };
            agent
                .run_turn_with_sink(&mut writer, &mut runtime, text, &mut sink)
                .await
                .map(|outcome| (writer, runtime, outcome))
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
                        handle.abort();
                        let _ = handle.await;
                        // The aborted task dropped the writer (lock released).
                        // Nothing to return — the actor is stopping.
                        return None;
                    }
                    // Turns never overlap: defer until this one settles.
                    Some(cmd @ (Command::Send { .. } | Command::Compact { .. })) => {
                        self.deferred.push_back(cmd);
                    }
                    None => {
                        // All handles dropped; let the turn finish, then exit.
                        return self.on_turn_done(handle.await).await;
                    }
                }
            }
        }
    }

    /// Handle a finished turn: emit `TurnSettled`, auto-compact if over the
    /// Handle a finished turn: emit `TurnSettled`, auto-compact if over the
    /// limit, and return the session to resume with. A hard error emits a notice
    /// and rebuilds from the log so the session stays usable. `None` means the
    /// session could not be reopened — the actor stops.
    async fn on_turn_done(
        &mut self,
        res: Result<TurnResult, tokio::task::JoinError>,
    ) -> Option<Session> {
        match res {
            Ok(Ok((writer, runtime, outcome))) => {
                let incomplete = outcome.incomplete.as_ref().map(|r| format!("{r:?}"));
                let _ = self
                    .outbound
                    .send(GatewayEvent::TurnSettled { incomplete });

                let over = outcome
                    .context_limit
                    .is_some_and(|l| outcome.context_tokens >= l);
                if over {
                    Some(self.compact((writer, runtime), None).await)
                } else {
                    Some((writer, runtime))
                }
            }
            Ok(Err(e)) => {
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("turn failed: {e}"),
                });
                self.reopen_after_abort()
            }
            Err(join_err) => {
                // The task panicked or was cancelled out from under us.
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("turn task ended unexpectedly: {join_err}"),
                });
                self.reopen_after_abort()
            }
        }
    }

    /// Abort a running turn and rebuild the session from the log. The aborted
    /// task drops the writer (releasing the lock); awaiting the handle guarantees
    /// that drop has happened before we reopen. `None` means reopen failed and
    /// the actor stops.
    async fn cancel_turn(&self, handle: JoinHandle<TurnResult>) -> Option<Session> {
        handle.abort();
        let _ = handle.await;
        let _ = self.outbound.send(GatewayEvent::Notice {
            message: "turn cancelled".to_owned(),
        });
        self.reopen_after_abort()
    }

    /// Reopen the current session and rebuild its runtime from the event log,
    /// reattaching the bus. Used after an abort/error consumed the live pair.
    /// `None` if the session cannot be reopened (e.g. the lock is somehow still
    /// held) — the actor stops rather than panic.
    fn reopen_after_abort(&self) -> Option<Session> {
        let events = self.store.read_events(&self.session_id).unwrap_or_default();
        let runtime = crate::agent::rebuild_runtime(&events, self.system.clone());
        match self.store.open(&self.session_id) {
            Ok(writer) => Some((writer.with_bus(self.bus.clone()), runtime)),
            Err(e) => {
                let _ = self.outbound.send(GatewayEvent::Notice {
                    message: format!("could not reopen session after abort: {e}"),
                });
                None
            }
        }
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
        match self
            .store
            .create_compaction(old_sid, meta.profile_id, meta.workspace, Vec::new(), &snapshot)
        {
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

/// Forward every committed [`CoreEvent`] from the session bus onto the outbound
/// [`GatewayEvent`] stream, tagged with its seq for SSE resume. Runs until the
/// bus has no more senders (the actor and its writer dropped).
fn spawn_event_forwarder(bus: &EventBus, outbound: broadcast::Sender<GatewayEvent>) {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = outbound.send(GatewayEvent::Event {
                        seq: event.seq,
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
        let _ = self
            .tx
            .send(GatewayEvent::Delta(Delta::Text {
                index,
                text: text.to_owned(),
            }));
    }

    fn on_reasoning(&mut self, index: u32, text: &str) {
        let _ = self
            .tx
            .send(GatewayEvent::Delta(Delta::Reasoning {
                index,
                text: text.to_owned(),
            }));
    }

    fn on_tool_call_delta(&mut self, index: u32, json_delta: &str) {
        let _ = self
            .tx
            .send(GatewayEvent::Delta(Delta::ToolArgs {
                index,
                json: json_delta.to_owned(),
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
            Arc::new(agent),
            store,
            system,
            (writer, runtime),
            std::time::Duration::from_secs(3600),
            Vec::new(),
        );
        (handle, dir)
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
}
