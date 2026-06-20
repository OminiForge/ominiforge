//! TUI: full-screen terminal interface that renders a turn as it streams.
//!
//! The render loop never blocks on the model.
//!
//! A turn runs in a background task; its live token deltas arrive over an mpsc
//! channel (via a [`StreamSink`]) and tool-execution results arrive over the
//! session [`EventBus`] (`doc/monitor.md` §9 — the online consumer Step 4 built
//! the bus for). The loop drains both, redraws, and polls the keyboard on a
//! short tick, so output appears as it is produced. The finished turn returns
//! its `(writer, runtime, outcome)` over a oneshot so the next turn continues
//! the same session.
//!
//! See `doc/architecture.md` §3.2 and `doc/phase2-plan.md` Step 6.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::{mpsc, oneshot};

use crate::agent::{Agent, BlockKind, SessionRuntime, StreamSink, TurnOutcome};
use crate::core::payload::{EventPayload, ToolEvent, ToolOutput};
use crate::core::{CoreEvent, SessionId};
use crate::llm::Message;
use crate::session::{EventBus, SessionStore, SessionWriter};

/// Run the TUI: optionally a session selector, then a multi-turn conversation loop.
///
/// With `resume` false a fresh session starts immediately; with `resume` true a
/// full-screen picker lists existing sessions first. Streams live. `mcp_clients`
/// is held for the whole session — dropping a client kills its subprocess, so
/// the binding must outlive the loop.
///
/// # Errors
/// Returns errors from terminal setup, session I/O, or agent turn failures.
// `async` is kept (the public entry the CLI awaits) even though the body spawns
// the turn rather than awaiting it; `spawn` needs the surrounding runtime.
#[allow(clippy::too_many_arguments, clippy::unused_async)]
pub async fn run(
    agent: Agent,
    store: SessionStore,
    system: Vec<Message>,
    profile_name: String,
    tool_names: Vec<String>,
    workspace: std::path::PathBuf,
    resolved: crate::config::ResolvedModel,
    mcp_clients: Vec<Arc<crate::mcp::McpClient>>,
    resume: bool,
) -> Result<()> {
    // Keep the MCP subprocesses alive for the whole session (named, not `_`, so
    // it is not dropped immediately).
    let _mcp_clients = mcp_clients;

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let result = run_app(
        &mut terminal,
        agent,
        store,
        system,
        profile_name,
        tool_names,
        workspace,
        resolved,
        resume,
    )
    .await;

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    result
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent: Agent,
    store: SessionStore,
    system: Vec<Message>,
    profile_name: String,
    tool_names: Vec<String>,
    workspace: std::path::PathBuf,
    resolved: crate::config::ResolvedModel,
    resume: bool,
) -> Result<()> {
    // Choose the starting session. `--resume` opens the picker (cancellable →
    // clean exit); otherwise start fresh. `history` is the resumed conversation
    // to render (empty for a fresh session).
    let Some((writer, runtime, history)) = start_session(
        terminal,
        &store,
        &system,
        &profile_name,
        &workspace,
        &tool_names,
        resume,
    )?
    else {
        return Ok(()); // user cancelled the picker — clean exit
    };

    let agent = Arc::new(agent);
    let bus = EventBus::new();
    let mut bus_rx = bus.subscribe();

    let mut state = AppState::new(
        writer.session_id().clone(),
        &resolved.provider_name,
        &resolved.model_id,
    );
    // Seed the conversation pane with the resumed history so the user sees what
    // this session was about (issue: a resumed session looked empty).
    state.seed_history(&history);

    // The session writer + runtime live here between turns; during a turn they
    // are moved into the background task and returned over the oneshot. Each
    // writer carries a clone of the bus so its events reach the live loop.
    let mut session = Some((writer.with_bus(bus.clone()), runtime));
    let mut input = String::new();

    // Set while a turn runs: the live-delta receiver and the completion channel.
    let mut active: Option<(
        mpsc::UnboundedReceiver<UiDelta>,
        oneshot::Receiver<TurnResult>,
    )> = None;

    loop {
        terminal.draw(|f| render_chat(f, &state, &input, active.is_some()))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                break;
            }
            // Input is only accepted between turns; a running turn ignores keys
            // except Ctrl-C above.
            if active.is_none() {
                match key.code {
                    KeyCode::Enter if !input.trim().is_empty() => {
                        if let Some((writer, runtime)) = session.take() {
                            let prompt = input.trim().to_owned();
                            input.clear();
                            state.push_user(&prompt);
                            active = Some(spawn_turn(Arc::clone(&agent), writer, runtime, prompt));
                        }
                    }
                    KeyCode::Char(c) => input.push(c),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    _ => {}
                }
            }
        }

        // Drain live token deltas from the running turn's sink.
        if let Some((delta_rx, _)) = active.as_mut() {
            while let Ok(delta) = delta_rx.try_recv() {
                state.apply_delta(delta);
            }
        }

        // Drain tool-execution + lifecycle events from the bus (the persisted,
        // authoritative stream; the sink does not see tool *results*).
        while let Ok(event) = bus_rx.try_recv() {
            state.apply_event(&event);
        }

        // Check whether the turn finished.
        if let Some((mut delta_rx, mut done_rx)) = active.take() {
            match done_rx.try_recv() {
                Ok(result) => {
                    // Drain any deltas that landed between the last poll and the task ending.
                    while let Ok(delta) = delta_rx.try_recv() {
                        state.apply_delta(delta);
                    }
                    match result {
                        Ok((writer, runtime, outcome)) => {
                            state.push_summary(&outcome);
                            // Auto-compaction: once the running estimate crosses
                            // the compaction limit, summarize and switch to a
                            // fresh session before the next turn
                            // (`doc/context-management.md` §4).
                            let over = outcome
                                .context_limit
                                .is_some_and(|l| outcome.context_tokens >= l);
                            session = Some(if over {
                                compact_session(&agent, &store, &bus, writer, runtime, &mut state)
                                    .await
                            } else {
                                (writer, runtime)
                            });
                        }
                        Err(e) => {
                            state.push_error(&format!("turn failed: {e}"));
                            // The session writer/runtime were consumed by the failed
                            // task; without them we cannot continue, so exit the loop.
                            break;
                        }
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // Still running — put the channels back and keep looping.
                    active = Some((delta_rx, done_rx));
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    state.push_error("turn task ended without a result");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// What a finished turn hands back: the session writer + runtime to continue
/// with, and the turn outcome (for the token-usage footer).
type TurnResult = Result<(SessionWriter, SessionRuntime, TurnOutcome), crate::agent::AgentError>;

/// Spawn the turn on a background task so the render loop keeps drawing. The
/// writer and runtime move in and come back out (with the outcome) over the
/// oneshot; live deltas stream out over the mpsc as the model produces them.
fn spawn_turn(
    agent: Arc<Agent>,
    mut writer: SessionWriter,
    mut runtime: SessionRuntime,
    prompt: String,
) -> (
    mpsc::UnboundedReceiver<UiDelta>,
    oneshot::Receiver<TurnResult>,
) {
    let (delta_tx, delta_rx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = oneshot::channel();

    tokio::spawn(async move {
        let mut sink = ChannelSink { tx: delta_tx };
        let result = agent
            .run_turn_with_sink(&mut writer, &mut runtime, prompt, &mut sink)
            .await
            .map(|outcome| (writer, runtime, outcome));
        // The receiver may already be gone (loop exited); ignore that.
        let _ = done_tx.send(result);
    });

    (delta_rx, done_rx)
}

/// Compact the current session: summarize, create a compaction session, and swap
/// to it so the conversation continues with a smaller context
/// (`doc/context-management.md` §4). A failure is reported in the conversation
/// but non-fatal — the original session is kept and returned unchanged.
async fn compact_session(
    agent: &Agent,
    store: &SessionStore,
    bus: &EventBus,
    writer: SessionWriter,
    runtime: SessionRuntime,
    state: &mut AppState,
) -> (SessionWriter, SessionRuntime) {
    match do_compact(agent, store, bus, &writer, &runtime).await {
        Ok((new_writer, new_runtime)) => {
            state.push_note(&format!("compacted → session {}", new_writer.session_id()));
            state.set_session(new_writer.session_id());
            (new_writer, new_runtime)
        }
        Err(e) => {
            state.push_error(&format!("compaction failed: {e}"));
            (writer, runtime)
        }
    }
}

/// Generate a summary, create a compaction session, and return its writer (bus
/// attached) and runtime ready to continue. Mirrors the CLI's former
/// `do_compact` path so a compaction session looks the same regardless of UI.
async fn do_compact(
    agent: &Agent,
    store: &SessionStore,
    bus: &EventBus,
    writer: &SessionWriter,
    runtime: &SessionRuntime,
) -> Result<(SessionWriter, SessionRuntime)> {
    let snapshot = agent
        .compact(runtime, None)
        .await
        .context("failed to generate summary")?
        .context("nothing to compact")?;

    let old_sid = writer.session_id().clone();
    let meta = store.read_meta(&old_sid)?;
    let new_writer = store
        .create_compaction(
            old_sid,
            meta.profile_id,
            meta.workspace,
            Vec::new(),
            &snapshot,
        )
        .context("failed to create compaction session")?
        .with_bus(bus.clone());
    let new_runtime = SessionRuntime::new(snapshot);
    Ok((new_writer, new_runtime))
}

/// What `start_session` hands back: the writer, runtime, and the rebuilt
/// conversation history to render (empty for a fresh session).
type StartedSession = (SessionWriter, SessionRuntime, Vec<Message>);

/// Pick the starting session: with `resume`, show the picker (returns `None` if
/// the user cancels or there are no sessions); otherwise create a fresh one.
#[allow(clippy::too_many_arguments)]
fn start_session(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    store: &SessionStore,
    system: &[Message],
    profile_name: &str,
    workspace: &std::path::Path,
    tool_names: &[String],
    resume: bool,
) -> Result<Option<StartedSession>> {
    if resume {
        select_session(terminal, store)?
            .map(|sid| open_session(store, &sid, system.to_vec()))
            .transpose()
    } else {
        let (w, r) = create_session(store, profile_name, workspace, tool_names, system)?;
        Ok(Some((w, r, Vec::new())))
    }
}

/// Create a brand-new session seeded with the system prompt.
fn create_session(
    store: &SessionStore,
    profile_name: &str,
    workspace: &std::path::Path,
    tool_names: &[String],
    system: &[Message],
) -> Result<(SessionWriter, SessionRuntime)> {
    let writer = store
        .create_new(
            Some(profile_name.to_owned()),
            Some(workspace.to_path_buf()),
            tool_names.to_vec(),
        )
        .context("failed to create session")?;
    Ok((writer, SessionRuntime::new(system.to_vec())))
}

/// Reopen `sid` for appending and rebuild its runtime + conversation history
/// from the event log. The returned `Vec<Message>` is the full rebuilt context
/// (system seed included) so the caller can render what the session was about.
fn open_session(
    store: &SessionStore,
    sid: &SessionId,
    system: Vec<Message>,
) -> Result<(SessionWriter, SessionRuntime, Vec<Message>)> {
    let events = store
        .read_events(sid)
        .with_context(|| format!("failed to read session {sid}"))?;
    let writer = store
        .open(sid)
        .with_context(|| format!("failed to open session {sid}"))?;
    let runtime = crate::agent::rebuild_runtime(&events, system);
    let history = runtime.context.clone();
    Ok((writer, runtime, history))
}

/// A row in the session picker: id plus a human summary of what it holds.
struct SessionRow {
    id: SessionId,
    created: String,
    turns: usize,
    preview: String,
}

/// Build the picker rows from the store: each session's creation time (local),
/// user-turn count, and a preview of its first user prompt — so the list is
/// meaningful instead of opaque ids. Unreadable sessions are skipped.
fn session_rows(store: &SessionStore) -> Vec<SessionRow> {
    let ids = store.list().unwrap_or_default();
    ids.into_iter()
        .map(|id| {
            let created = store.read_meta(&id).map_or_else(
                |_| "?".to_owned(),
                |m| {
                    m.created_at
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                },
            );
            let (turns, preview) = store.read_events(&id).map_or((0, String::new()), |events| {
                let turns = events
                    .iter()
                    .filter(|e| {
                        matches!(
                            &e.payload,
                            EventPayload::Turn(crate::core::payload::TurnEvent::Started { .. })
                        )
                    })
                    .count();
                let preview = events
                    .iter()
                    .find_map(|e| match &e.payload {
                        EventPayload::Turn(crate::core::payload::TurnEvent::Started {
                            input: Some(input),
                            ..
                        }) => Some(first_line(input, 60)),
                        _ => None,
                    })
                    .unwrap_or_default();
                (turns, preview)
            });
            SessionRow {
                id,
                created,
                turns,
                preview,
            }
        })
        .collect()
}

/// Show the session picker. Returns the chosen session id, or `None` if the user
/// cancelled (q/Esc) or there are no sessions — both are clean, non-error exits.
fn select_session(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    store: &SessionStore,
) -> Result<Option<SessionId>> {
    let rows = session_rows(store);
    if rows.is_empty() {
        return Ok(None);
    }

    let mut selected = 0;
    loop {
        terminal.draw(|f| render_selector(f, &rows, selected))?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Up if selected > 0 => selected -= 1,
                KeyCode::Down if selected + 1 < rows.len() => selected += 1,
                KeyCode::Enter => return Ok(Some(rows[selected].id.clone())),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                _ => {}
            }
        }
    }
}

fn render_selector(f: &mut Frame, rows: &[SessionRow], selected: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Resume a session  (↑/↓ to move, Enter to open, q/Esc to cancel)");

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let preview = if row.preview.is_empty() {
                "(no messages yet)".to_owned()
            } else {
                row.preview.clone()
            };
            let label = format!("{}  ·  {} turn(s)  ·  {}", row.created, row.turns, preview);
            selectable_line(&label, i == selected)
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        f.area(),
    );
}

/// A selector row, highlighted when it is the current selection.
fn selectable_line(text: &str, selected: bool) -> Line<'static> {
    if selected {
        Line::from(format!("→ {text}")).style(Style::default().add_modifier(Modifier::REVERSED))
    } else {
        Line::from(format!("  {text}"))
    }
}

fn render_chat(f: &mut Frame, state: &AppState, input: &str, busy: bool) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(f.area());

    // Conversation: keep the tail visible by scrolling so the last lines show.
    let conv = state.lines.join("\n");
    let total_lines = u16::try_from(state.lines.len()).unwrap_or(u16::MAX);
    let viewport = chunks[0].height.saturating_sub(2); // minus the borders
    let scroll = total_lines.saturating_sub(viewport);

    let title = format!(
        "session {} · {}/{}",
        state.session_id, state.provider, state.model
    );
    f.render_widget(
        Paragraph::new(conv)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        chunks[0],
    );

    let input_title = if busy {
        "working… (Ctrl-C to quit)"
    } else {
        "message (Enter to send · Ctrl-C to quit)"
    };
    f.render_widget(
        Paragraph::new(input).block(Block::default().borders(Borders::ALL).title(input_title)),
        chunks[1],
    );
}

/// A live delta forwarded from the running turn's [`StreamSink`] to the UI loop.
enum UiDelta {
    /// A new block opened; carries an owned label for the side-channel.
    BlockStart(Block_),
    /// Assistant answer text.
    Text(String),
    /// Reasoning / thinking text.
    Reasoning(String),
    /// Tool-call argument JSON fragment.
    ToolArgs(String),
}

/// Owned counterpart of [`BlockKind`] (which borrows the tool name), so it can
/// cross the channel.
enum Block_ {
    Text,
    Reasoning,
    Tool(String),
}

/// Which kind of line the last conversation entry is, so a text/reasoning delta
/// appends to it instead of starting a new line each time.
#[derive(PartialEq, Eq)]
enum Open {
    None,
    Answer,
    Reasoning,
    Tool,
}

/// All TUI state: the session identity for the header and the rendered lines.
struct AppState {
    session_id: String,
    provider: String,
    model: String,
    lines: Vec<String>,
    open: Open,
}

impl AppState {
    fn new(sid: SessionId, provider: &str, model: &str) -> Self {
        Self {
            session_id: sid.0,
            provider: provider.to_owned(),
            model: model.to_owned(),
            lines: Vec::new(),
            open: Open::None,
        }
    }

    fn push_user(&mut self, prompt: &str) {
        self.lines.push(String::new());
        self.lines.push(format!("> {prompt}"));
        self.open = Open::None;
    }

    /// Render a resumed session's rebuilt context into the conversation pane so
    /// the user sees what the session was about (the system seed is skipped — it
    /// is identity, not conversation). Tool results are rebuilt as `Tool`
    /// messages; their content is shown indented under the call.
    fn seed_history(&mut self, history: &[Message]) {
        let mut shown = false;
        for msg in history {
            match msg {
                Message::System { .. } => {} // identity, not conversation
                Message::User { content } => {
                    self.lines.push(String::new());
                    self.lines.push(format!("> {content}"));
                    shown = true;
                }
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    if let Some(text) = content {
                        self.lines.push(text.clone());
                    }
                    for call in tool_calls {
                        self.lines
                            .push(format!("[tool: {}] {}", call.name, call.arguments));
                    }
                    shown = true;
                }
                Message::Tool { content, .. } => {
                    self.lines.push(format!("  ↳ {}", first_line(content, 200)));
                    shown = true;
                }
            }
        }
        if shown {
            self.lines.push(String::new());
            self.lines.push("── resumed; continue below ──".to_owned());
        }
        self.open = Open::None;
    }

    /// Fold one live delta into the rendered lines, appending to the open line
    /// when the channel is unchanged (mirrors the CLI sink's channel tracking).
    fn apply_delta(&mut self, delta: UiDelta) {
        match delta {
            UiDelta::BlockStart(Block_::Text) => self.open = Open::Answer,
            UiDelta::BlockStart(Block_::Reasoning) => {
                self.lines.push("[thinking] ".to_owned());
                self.open = Open::Reasoning;
            }
            UiDelta::BlockStart(Block_::Tool(name)) => {
                self.lines.push(format!("[tool: {name}] "));
                self.open = Open::Tool;
            }
            UiDelta::Text(text) => self.append(Open::Answer, &text),
            UiDelta::Reasoning(text) => self.append(Open::Reasoning, &text),
            UiDelta::ToolArgs(text) => self.append(Open::Tool, &text),
        }
    }

    /// Append `text` to the last line if the open channel matches; otherwise
    /// start a fresh line in that channel.
    fn append(&mut self, channel: Open, text: &str) {
        if self.open == channel
            && let Some(last) = self.lines.last_mut()
        {
            last.push_str(text);
        } else {
            self.lines.push(text.to_owned());
            self.open = channel;
        }
    }

    /// React to a persisted bus event — tool completions/failures, which the
    /// live sink does not carry (it only sees the model's call request).
    fn apply_event(&mut self, event: &CoreEvent) {
        match &event.payload {
            EventPayload::Tool(ToolEvent::Completed {
                result,
                output_bytes,
                ..
            }) => {
                let tag = if result.is_error { "error" } else { "ok" };
                self.lines
                    .push(format!("  ↳ tool {tag} ({output_bytes} bytes)"));
                if let Some(first) = first_text_line(result) {
                    self.lines.push(format!("    {first}"));
                }
                self.open = Open::None;
            }
            EventPayload::Tool(ToolEvent::Failed { error, .. }) => {
                self.lines.push(format!(
                    "  ↳ tool failed: [{}] {}",
                    error.code, error.message
                ));
                self.open = Open::None;
            }
            _ => {}
        }
    }

    fn push_summary(&mut self, outcome: &TurnOutcome) {
        let ctx = outcome.context_limit.map_or_else(
            || format!("~{} tokens", outcome.context_tokens),
            |limit| {
                let pct = if limit == 0 {
                    100
                } else {
                    (u64::from(outcome.context_tokens) * 100 / u64::from(limit)).min(999)
                };
                format!("ctx ~{}/{} ({pct}%)", outcome.context_tokens, limit)
            },
        );
        self.lines.push(format!(
            "[{} round(s) · {}in/{}out · {ctx}]",
            outcome.rounds, outcome.usage.input_tokens, outcome.usage.output_tokens
        ));
        self.open = Open::None;
    }

    fn push_error(&mut self, msg: &str) {
        self.lines.push(format!("[error] {msg}"));
        self.open = Open::None;
    }

    /// A neutral status note (e.g. an auto-compaction notice).
    fn push_note(&mut self, msg: &str) {
        self.lines.push(format!("[{msg}]"));
        self.open = Open::None;
    }

    /// Point the header at a new session id (after compaction switches sessions).
    fn set_session(&mut self, sid: &SessionId) {
        self.session_id.clone_from(&sid.0);
    }
}

/// The first non-empty text line of a tool result, for a one-line preview.
fn first_text_line(output: &ToolOutput) -> Option<String> {
    output.content.iter().find_map(|c| match c {
        crate::core::payload::Content::Text(t) => t
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(ToOwned::to_owned),
        _ => None,
    })
}

/// First line of `s`, trimmed and truncated to `max` chars with an ellipsis.
/// Used for compact one-line previews in the picker and resumed history.
fn first_line(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > max {
        let truncated: String = line.chars().take(max).collect();
        format!("{truncated}…")
    } else {
        line.to_owned()
    }
}

/// A [`StreamSink`] that forwards every live delta to the UI loop over a channel.
/// Cheap and non-blocking (unbounded send), as the hot-path contract requires.
struct ChannelSink {
    tx: mpsc::UnboundedSender<UiDelta>,
}

impl StreamSink for ChannelSink {
    fn on_block_start(&mut self, _index: u32, block: BlockKind<'_>) {
        let owned = match block {
            BlockKind::Text => Block_::Text,
            BlockKind::Reasoning => Block_::Reasoning,
            BlockKind::ToolCall { name } => Block_::Tool(name.to_owned()),
        };
        let _ = self.tx.send(UiDelta::BlockStart(owned));
    }

    fn on_text(&mut self, _index: u32, text: &str) {
        let _ = self.tx.send(UiDelta::Text(text.to_owned()));
    }

    fn on_reasoning(&mut self, _index: u32, text: &str) {
        let _ = self.tx.send(UiDelta::Reasoning(text.to_owned()));
    }

    fn on_tool_call_delta(&mut self, _index: u32, json_delta: &str) {
        let _ = self.tx.send(UiDelta::ToolArgs(json_delta.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SessionId;
    use crate::llm::{Message, ToolCall};

    fn state() -> AppState {
        AppState::new(SessionId("01TEST".to_owned()), "prov", "model")
    }

    /// A resumed session's rebuilt context must be rendered into the pane so the
    /// user sees what it was about: the system seed is hidden, user turns show as
    /// `> ...`, assistant text/tool-calls show, tool results show indented, and a
    /// separator marks where the live continuation begins. This is the fix for
    /// "a resumed session looked empty".
    #[test]
    fn seed_history_renders_prior_conversation_not_system() {
        let mut s = state();
        s.seed_history(&[
            Message::System {
                content: "you are an agent".to_owned(),
            },
            Message::User {
                content: "remember 42".to_owned(),
            },
            Message::Assistant {
                content: Some("noted 42".to_owned()),
                tool_calls: vec![ToolCall {
                    id: "c1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: r#"{"command":"echo hi"}"#.to_owned(),
                }],
            },
            Message::Tool {
                tool_call_id: "c1".to_owned(),
                content: "hi".to_owned(),
            },
        ]);
        let view = s.lines.join("\n");

        // System identity is NOT shown as conversation.
        assert!(
            !view.contains("you are an agent"),
            "system seed must be hidden"
        );
        // The prior user turn and assistant reply ARE shown.
        assert!(
            view.contains("> remember 42"),
            "user turn missing: {view:?}"
        );
        assert!(view.contains("noted 42"), "assistant text missing");
        // The tool call and its result are shown.
        assert!(view.contains("[tool: shell]"), "tool call missing");
        assert!(view.contains("↳ hi"), "tool result missing");
        // A separator marks where the live continuation starts.
        assert!(
            view.contains("resumed; continue below"),
            "resume separator missing"
        );
    }

    /// Seeding an empty history (a fresh session) adds nothing — no spurious
    /// separator, so a new session starts with a clean pane.
    #[test]
    fn seed_history_empty_is_noop() {
        let mut s = state();
        s.seed_history(&[]);
        assert!(s.lines.is_empty(), "empty history must not render anything");
    }

    /// `first_line` collapses to the first non-empty line and truncates with an
    /// ellipsis, so picker rows and previews stay one line.
    #[test]
    fn first_line_truncates_and_trims() {
        assert_eq!(first_line("  hello  \nworld", 80), "hello");
        assert_eq!(first_line("\n\nfirst real", 80), "first real");
        assert_eq!(first_line(&"x".repeat(100), 5), "xxxxx…");
        assert_eq!(first_line("", 10), "");
    }
}
