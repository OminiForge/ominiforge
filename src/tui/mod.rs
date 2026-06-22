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
//! Submodules:
//! - [`theme`] — Catppuccin Mocha palette mapped to semantic roles.
//! - [`conversation`] — the typed [`Block`] model and its styled rendering.
//! - [`input`] — the editable input line with a cursor.
//!
//! See `doc/architecture.md` §3.2 and `doc/phase2-plan.md` Step 6.

mod conversation;
mod input;
mod statusbar;
mod statusline;
mod theme;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::ExecutableCommand;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block as WidgetBlock, Borders, Paragraph, Wrap};
use tokio::sync::{mpsc, oneshot};

use crate::agent::{Agent, BlockKind, SessionRuntime, StreamSink, TurnOutcome};
use crate::core::payload::{EventPayload, ToolEvent, ToolOutput};
use crate::core::{CoreEvent, SessionId};
use crate::llm::Message;
use crate::session::{EventBus, SessionStore, SessionWriter};

use conversation::{Block, ToolStatus};
use input::Input;
use statusbar::StatusBar;
use statusline::Follow;

/// Run the TUI: optionally a session selector, then a multi-turn conversation loop.
///
/// With `resume` false the conversation starts empty and the session file is
/// created lazily on the first message (so quitting without typing leaves no
/// empty session behind); with `resume` true a full-screen picker lists existing
/// sessions first. Streams live. `mcp_clients` is held for the whole session —
/// dropping a client kills its subprocess, so the binding must outlive the loop.
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
    // Disable all common mouse-reporting protocols so the terminal stops sending
    // scroll/click bytes. Without this, some terminals (Zed, …) emit X10 mouse
    // sequences even without EnableMouseCapture, which corrupt the input.
    let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
    let _ = io::stdout().flush();
    // Restore the terminal on panic *before* the default hook prints the trace.
    // Without this a panic leaves the terminal in raw mode + alt screen (a
    // blanked, unusable shell) — the worst-possible TUI failure.
    install_panic_hook();
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

/// Best-effort terminal restoration: leave raw mode, the alt screen, and mouse
/// capture. Used by both the normal exit path and the panic hook; ignores errors
/// because by the time it runs (especially mid-panic) there is nothing useful to
/// do with them.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
}

/// Chain a terminal-restoring step in front of the existing panic hook, so a
/// panic inside the TUI returns the user to a usable shell before the backtrace
/// prints, instead of leaving a blanked raw-mode screen.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

/// The bits needed to mint a fresh session on demand. Held in `run_app` so the
/// first message can create the session lazily (deferred-creation), rather than
/// creating it the moment the TUI opens.
struct SessionFactory {
    store: SessionStore,
    system: Vec<Message>,
    profile_name: String,
    workspace: std::path::PathBuf,
    tool_names: Vec<String>,
    bus: EventBus,
}

impl SessionFactory {
    /// Create a brand-new session (writer with the bus attached + a fresh
    /// runtime seeded with the system prompt).
    fn create(&self) -> Result<(SessionWriter, SessionRuntime)> {
        let writer = self
            .store
            .create_new(
                Some(self.profile_name.clone()),
                Some(self.workspace.clone()),
                self.tool_names.clone(),
            )
            .context("failed to create session")?;
        Ok((
            writer.with_bus(self.bus.clone()),
            SessionRuntime::new(self.system.clone()),
        ))
    }
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
    let agent = Arc::new(agent);
    let bus = EventBus::new();
    let mut bus_rx = bus.subscribe();

    let factory = SessionFactory {
        store,
        system,
        profile_name,
        workspace,
        tool_names,
        bus: bus.clone(),
    };

    let mut state = AppState::new(
        &resolved.provider_name,
        &resolved.model_id,
        resolved.context_window,
        resolved.max_output_tokens,
    );

    // Detect the workspace environment once and cache it for the status bar.
    // Git status is refreshed after each turn (the agent edits files); language
    // and env are fixed for the session.
    let mut status_bar = StatusBar::detect(&factory.workspace);

    // The session writer + runtime live here between turns. With `--resume` we
    // open the chosen session up front; otherwise we start with `None` and mint
    // the session on the first message (deferred creation).
    let mut session: Option<(SessionWriter, SessionRuntime)> = if resume {
        let Some((writer, runtime, history)) =
            select_and_open(terminal, &factory.store, &factory.system)?
        else {
            return Ok(()); // user cancelled the picker — clean exit
        };
        state.set_session(writer.session_id());
        state.seed_history(&history);
        Some((writer.with_bus(bus.clone()), runtime))
    } else {
        None
    };

    let mut input = Input::default();

    // Set while a turn runs: the live-delta receiver and the completion channel.
    let mut active: Option<ActiveTurn> = None;

    loop {
        terminal.draw(|f| render_chat(f, &mut state, &input, active.is_some(), &status_bar))?;

        // Block briefly for the first event, then drain the whole backlog before
        // redrawing (see `drain_input`).
        match drain_input(&mut state, &mut input, active.is_some())? {
            InputAction::Quit => return Ok(()),
            InputAction::Send(send) => match ensure_session(&mut session, &factory, &mut state) {
                Ok((writer, runtime)) => {
                    state.push(Block::User(send.clone()));
                    state.follow_bottom();
                    state.streaming = true;
                    active = Some(spawn_turn(Arc::clone(&agent), writer, runtime, send));
                }
                Err(e) => {
                    state.push(Block::Error(format!("could not start session: {e}")));
                }
            },
            InputAction::None => {}
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
        if let Some((delta_rx, done_rx)) = active.take() {
            match poll_turn(
                delta_rx,
                done_rx,
                &agent,
                &factory.store,
                &bus,
                &mut state,
                &mut session,
            )
            .await
            {
                TurnPoll::Running(channels) => active = Some(channels),
                TurnPoll::Continue => {
                    // The turn may have edited files; refresh git so the dirty
                    // marker reflects reality.
                    status_bar.refresh_git(&factory.workspace);
                }
                TurnPoll::Stop => break,
            }
        }
    }

    Ok(())
}

/// The result of polling a running turn for completion.
enum TurnPoll {
    /// Still running; carries the channels back to keep polling.
    Running(ActiveTurn),
    /// Finished (or compacted); the loop continues with `session` updated.
    Continue,
    /// A fatal error consumed the session; the loop must exit.
    Stop,
}

/// The live channels of a running turn: its delta receiver and completion
/// oneshot.
type ActiveTurn = (
    mpsc::UnboundedReceiver<UiDelta>,
    oneshot::Receiver<TurnResult>,
);

/// Poll the running turn once. On completion, drain trailing deltas, append the
/// summary, and either continue with the returned session or (over the
/// compaction limit) compact into a fresh one. A task error is fatal — the
/// session was consumed and cannot continue.
#[allow(clippy::too_many_arguments)]
async fn poll_turn(
    mut delta_rx: mpsc::UnboundedReceiver<UiDelta>,
    mut done_rx: oneshot::Receiver<TurnResult>,
    agent: &Agent,
    store: &SessionStore,
    bus: &EventBus,
    state: &mut AppState,
    session: &mut Option<(SessionWriter, SessionRuntime)>,
) -> TurnPoll {
    match done_rx.try_recv() {
        Ok(result) => {
            // Drain any deltas that landed between the last poll and the task ending.
            while let Ok(delta) = delta_rx.try_recv() {
                state.apply_delta(delta);
            }
            match result {
                Ok((writer, runtime, outcome)) => {
                    state.finish_open_thinking();
                    state.push_summary(&outcome);
                    // Auto-compaction: once the running estimate crosses the
                    // compaction limit, summarize and switch to a fresh session
                    // before the next turn (`doc/context-management.md` §4).
                    let over = outcome
                        .context_limit
                        .is_some_and(|l| outcome.context_tokens >= l);
                    *session = Some(if over {
                        compact_session(agent, store, bus, writer, runtime, state).await
                    } else {
                        (writer, runtime)
                    });
                    TurnPoll::Continue
                }
                Err(e) => {
                    state.push(Block::Error(format!("turn failed: {e}")));
                    // The session writer/runtime were consumed by the failed
                    // task; without them we cannot continue.
                    TurnPoll::Stop
                }
            }
        }
        // Still running — hand the channels back to keep looping.
        Err(oneshot::error::TryRecvError::Empty) => TurnPoll::Running((delta_rx, done_rx)),
        Err(oneshot::error::TryRecvError::Closed) => {
            state.push(Block::Error("turn task ended without a result".to_owned()));
            TurnPoll::Stop
        }
    }
}

/// What draining the input backlog asks the loop to do next.
enum InputAction {
    /// Nothing to do this frame.
    None,
    /// The user pressed Ctrl-C — quit.
    Quit,
    /// The user submitted a message to send.
    Send(String),
}

/// Block briefly for the first event, then drain the whole backlog before the
/// caller redraws. A mouse-wheel notch emits a burst of scroll events; handling
/// them one-per-frame (each frame re-renders + re-wraps the entire history) is
/// what makes scrolling feel laggy. Coalescing the burst into a single redraw
/// keeps it smooth regardless of history size.
fn drain_input(state: &mut AppState, input: &mut Input, busy: bool) -> Result<InputAction> {
    if !event::poll(Duration::from_millis(50))? {
        return Ok(InputAction::None);
    }
    let mut action = InputAction::None;
    loop {
        match event::read()? {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    return Ok(InputAction::Quit);
                }
                if let Some(s) = handle_key(key, state, input, busy) {
                    action = InputAction::Send(s);
                }
            }
            _ => {}
        }
        if !event::poll(Duration::ZERO)? {
            break;
        }
    }
    Ok(action)
}

/// Handle one key press. Returns `Some(prompt)` when the user submitted a
/// message to send (Enter with non-empty input, between turns). All other keys
/// edit the input or scroll the conversation in place.
fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    input: &mut Input,
    busy: bool,
) -> Option<String> {
    // Scrolling works whether or not a turn is running. End/Home jump to the
    // bottom (resuming follow) / top — matching the status-line hint.
    match key.code {
        KeyCode::PageUp => {
            state.scroll_up(state.viewport.max(1));
            return None;
        }
        KeyCode::PageDown => {
            state.scroll_down(state.viewport.max(1));
            return None;
        }
        KeyCode::Up => {
            state.scroll_up(1);
            return None;
        }
        KeyCode::Down => {
            state.scroll_down(1);
            return None;
        }
        KeyCode::End => {
            state.follow_bottom();
            return None;
        }
        KeyCode::Home => {
            state.scroll_to_top();
            return None;
        }
        _ => {}
    }

    // Editing keys are always available — the user can compose the next message
    // while a turn streams. Only submission (bare Enter) is gated on idle.
    match key.code {
        // Alt/Shift+Enter inserts a newline; a bare Enter submits (only when idle).
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            input.newline();
        }
        KeyCode::Enter if !busy => {
            let text = input.trimmed();
            if text.is_empty() {
                return None;
            }
            input.clear();
            return Some(text);
        }
        KeyCode::Char(c) => input.insert(c),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        _ => {}
    }
    None
}

/// Ensure a live session exists, creating one lazily if this is the first
/// message. Returns the writer + runtime to run the turn with.
fn ensure_session(
    session: &mut Option<(SessionWriter, SessionRuntime)>,
    factory: &SessionFactory,
    state: &mut AppState,
) -> Result<(SessionWriter, SessionRuntime)> {
    if let Some(pair) = session.take() {
        return Ok(pair);
    }
    let (writer, runtime) = factory.create()?;
    state.set_session(writer.session_id());
    Ok((writer, runtime))
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
            state.push(Block::Note(format!(
                "compacted → session {}",
                new_writer.session_id()
            )));
            state.set_session(new_writer.session_id());
            (new_writer, new_runtime)
        }
        Err(e) => {
            state.push(Block::Error(format!("compaction failed: {e}")));
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

/// Open the picker and the chosen session: returns the writer, runtime, and the
/// rebuilt conversation history (empty if cancelled → `None`).
fn select_and_open(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    store: &SessionStore,
    system: &[Message],
) -> Result<Option<(SessionWriter, SessionRuntime, Vec<Message>)>> {
    select_session(terminal, store)?
        .map(|sid| open_session(store, &sid, system.to_vec()))
        .transpose()
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
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
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
    let block = WidgetBlock::default()
        .borders(Borders::ALL)
        .border_style(theme::fg(theme::dim()))
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
        Line::from(format!("→ {text}"))
            .style(theme::fg(theme::user()).add_modifier(Modifier::REVERSED))
    } else {
        Line::from(format!("  {text}")).style(theme::fg(theme::text()))
    }
}

/// Minimum usable terminal size. Below this the stacked layout (conversation +
/// status bar + input + status line) has no room for the conversation, so we
/// show a hint instead of rendering a broken UI.
const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;

fn render_chat(
    f: &mut Frame,
    state: &mut AppState,
    input: &Input,
    busy: bool,
    status_bar: &statusbar::StatusBar,
) {
    let area = f.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "terminal too small\nneed at least {MIN_WIDTH}×{MIN_HEIGHT}, have {}×{}",
            area.width, area.height
        );
        f.render_widget(
            Paragraph::new(msg)
                .style(theme::fg(theme::warn()))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    // The input box grows with its content (multi-line messages) up to a cap, so
    // a long paste is visible without letting it swallow the conversation.
    let input_rows = input.line_count().clamp(1, 8);
    let input_height = input_rows + 2; // borders

    // 4-region layout:
    //   conversation pane (fills remaining space)
    //   env status bar (1 line, above input)
    //   input box (grows with content, bordered)
    //   AI status line (1 line, below input)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    let viewport = chunks[0].height.saturating_sub(2);
    let width = chunks[0].width.saturating_sub(2);
    state.viewport = viewport;

    let (lines, total) = state.cached_render(width);
    let para = Paragraph::new(lines)
        .block(
            WidgetBlock::default()
                .borders(Borders::ALL)
                .border_style(theme::fg(theme::dim())),
        )
        .wrap(Wrap { trim: false });
    let scroll = state.scroll_offset(total, viewport);
    f.render_widget(para.scroll((scroll, 0)), chunks[0]);

    // Env status bar (cwd · git · language · env) above the input.
    f.render_widget(Paragraph::new(status_bar.render()), chunks[1]);

    // Input box with cursor. The title doubles as the always-visible key-hint
    // bar (the cheapest discoverability tool — no extra row).
    let input_title = if busy {
        "working…  ·  type next message · Enter queues after · Ctrl-C quit"
    } else {
        "message  ·  Enter send · Alt-Enter newline · PgUp/PgDn scroll · End bottom · Ctrl-C quit"
    };
    f.render_widget(
        Paragraph::new(input.text())
            .style(theme::fg(theme::text()))
            .block(
                WidgetBlock::default()
                    .borders(Borders::ALL)
                    .border_style(theme::fg(theme::dim()))
                    .title(input_title),
            )
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    if !busy {
        let (row, col) = input.cursor_pos();
        let cx = chunks[2].x + 1 + col;
        let cy = chunks[2].y + 1 + row;
        f.set_cursor_position((cx, cy));
    }

    // AI status line (model · context gauge · scroll hint) below the input.
    f.render_widget(
        Paragraph::new(statusline::render(
            &state.model_label,
            state.context_tokens,
            state.context_window,
            state.threshold(),
            state.follow(),
        )),
        chunks[3],
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

/// All TUI state: identity, conversation blocks, scroll, and context-usage tracking.
struct AppState {
    session_id: Option<String>,
    /// `provider/model` label for the status line.
    model_label: String,
    blocks: Vec<Block>,
    /// Actual model context window (tokens), from the resolved config.
    context_window: u32,
    /// `max_output_tokens` from the resolved config — used to compute threshold.
    max_output_tokens: u32,
    /// Running input-token estimate from the most recent turn summary.
    context_tokens: u32,
    /// True while a turn is streaming (so the follow indicator shows "new output").
    streaming: bool,
    /// Pinned absolute top line when the user has scrolled up; `None` = follow
    /// the bottom. An absolute position is immune to new content being appended
    /// below — unlike a distance-from-bottom which drifts down as `total` grows.
    pinned_top: Option<u16>,
    /// Visual-line total from the last render frame, used by `scroll_up`/`scroll_down` to
    /// convert between absolute top and relative positions.
    last_total: u16,
    /// Last known inner height of the conversation pane (set during render).
    viewport: u16,
    /// Bumped on every change to `blocks`. The render cache compares it to know
    /// when its memoized lines are stale.
    rev: u64,
    /// Memoized rendered lines + wrapped total, valid for a `(rev, width)` pair.
    /// Re-rendering parses markdown for every answer block and re-wraps the whole
    /// history; doing that every frame is what makes scrolling a long
    /// conversation lag. Pure-scroll frames (blocks unchanged) reuse this.
    cache: Option<RenderCache>,
}

/// Memoized conversation render, keyed by the blocks revision and pane width.
struct RenderCache {
    rev: u64,
    width: u16,
    lines: Vec<Line<'static>>,
    total: u16,
}

impl AppState {
    fn new(provider: &str, model: &str, context_window: u32, max_output_tokens: u32) -> Self {
        Self {
            session_id: None,
            model_label: format!("{provider}/{model}"),
            blocks: Vec::new(),
            context_window,
            max_output_tokens,
            context_tokens: 0,
            streaming: false,
            pinned_top: None,
            last_total: 0,
            viewport: 0,
            rev: 0,
            cache: None,
        }
    }

    /// Mark the conversation dirty so the next render rebuilds its line cache.
    /// Called from every mutator that changes `blocks`.
    const fn touch(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }

    /// Append a block.
    fn push(&mut self, block: Block) {
        self.blocks.push(block);
        self.touch();
    }

    /// Render all blocks to styled lines for the conversation pane, with one
    /// blank line between blocks so turns, answers and tool cards don't run
    /// together (whitespace separates better than borders here).
    fn render_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for block in &self.blocks {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(block.render());
        }
        lines
    }

    /// Render the conversation lines and their wrapped total for `width`,
    /// reusing the memoized result when neither the blocks (`rev`) nor the width
    /// changed. Returns clones of the cached lines (cheap relative to parsing
    /// markdown + re-wrapping the whole history every frame).
    fn cached_render(&mut self, width: u16) -> (Vec<Line<'static>>, u16) {
        let fresh = self
            .cache
            .as_ref()
            .is_some_and(|c| c.rev == self.rev && c.width == width);
        if let Some(c) = self.cache.as_ref().filter(|_| fresh) {
            return (c.lines.clone(), c.total);
        }
        let lines = self.render_lines();
        let probe = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let total = u16::try_from(probe.line_count(width)).unwrap_or(u16::MAX);
        self.cache = Some(RenderCache {
            rev: self.rev,
            width,
            lines: lines.clone(),
            total,
        });
        (lines, total)
    }

    /// The compaction threshold (fraction of window): `(window × 0.8 − max_output) / window`.
    /// Falls back to `0.8` when window is zero to keep the gauge sensible.
    fn threshold(&self) -> f32 {
        if self.context_window == 0 {
            return 0.8;
        }
        let window = f64::from(self.context_window);
        let budget = f64::from(self.max_output_tokens)
            .mul_add(-1.0, window * 0.8)
            .max(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let t = (budget / window) as f32;
        t
    }

    /// Derive the scroll-follow state for the status line.
    const fn follow(&self) -> Follow {
        if self.pinned_top.is_none() {
            Follow::AtBottom
        } else if self.streaming {
            Follow::NewOutput
        } else {
            Follow::ScrolledUp
        }
    }

    /// Snap back to following the bottom (newest output visible).
    const fn follow_bottom(&mut self) {
        self.pinned_top = None;
    }

    /// Pin the viewport to the very top of the conversation.
    const fn scroll_to_top(&mut self) {
        self.pinned_top = Some(0);
    }

    /// Scroll up by `n` visual lines, pinning the viewport at an absolute position.
    fn scroll_up(&mut self, n: u16) {
        let current = self
            .pinned_top
            .unwrap_or_else(|| self.last_total.saturating_sub(self.viewport));
        self.pinned_top = Some(current.saturating_sub(n));
    }

    /// Scroll down by `n` visual lines; reaches bottom → resume following.
    const fn scroll_down(&mut self, n: u16) {
        if let Some(top) = self.pinned_top {
            let new_top = top.saturating_add(n);
            let max_top = self.last_total.saturating_sub(self.viewport);
            if new_top >= max_top {
                self.pinned_top = None;
            } else {
                self.pinned_top = Some(new_top);
            }
        }
    }

    /// Compute the ratatui scroll offset (top visual line to show).
    fn scroll_offset(&mut self, total: u16, viewport: u16) -> u16 {
        self.last_total = total;
        self.viewport = viewport; // keep in sync so scroll_up/down see the right value
        let max_top = total.saturating_sub(viewport);
        self.pinned_top.map_or(max_top, |t| t.min(max_top))
    }

    /// Render a resumed session's rebuilt context into the conversation as
    /// blocks so the user sees what it was about (the system seed is skipped —
    /// it is identity, not conversation).
    fn seed_history(&mut self, history: &[Message]) {
        let mut shown = false;
        for msg in history {
            match msg {
                Message::System { .. } => {} // identity, not conversation
                Message::User { content } => {
                    self.blocks.push(Block::User(content.clone()));
                    shown = true;
                }
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    if let Some(text) = content {
                        self.blocks.push(Block::Answer {
                            text: text.clone(),
                            done: true, // history is already complete
                        });
                    }
                    for call in tool_calls {
                        self.blocks.push(Block::Tool {
                            name: call.name.clone(),
                            args: call.arguments.clone(),
                            status: ToolStatus::Done {
                                error: false,
                                summary: String::new(),
                            },
                        });
                    }
                    shown = true;
                }
                Message::Tool { content, .. } => {
                    // Attach the result to the most recent tool card.
                    self.set_last_tool_result(false, &first_line(content, 200));
                    shown = true;
                }
            }
        }
        if shown {
            self.blocks
                .push(Block::Separator("resumed; continue below".to_owned()));
        }
        self.touch();
    }

    /// Fold one live delta into the conversation blocks. Text/reasoning/tool
    /// deltas append to the matching open block; a `BlockStart` finishes any
    /// open thinking/answer block and opens the new one.
    fn apply_delta(&mut self, delta: UiDelta) {
        self.touch();
        match delta {
            UiDelta::BlockStart(Block_::Text) => {
                self.finish_open_thinking();
                self.finish_open_answer();
            }
            UiDelta::BlockStart(Block_::Reasoning) => {
                self.finish_open_thinking();
                self.finish_open_answer();
                self.blocks.push(Block::Thinking {
                    text: String::new(),
                    started: Instant::now(),
                    elapsed: None,
                });
            }
            UiDelta::BlockStart(Block_::Tool(name)) => {
                self.finish_open_thinking();
                self.finish_open_answer();
                self.blocks.push(Block::Tool {
                    name,
                    args: String::new(),
                    status: ToolStatus::Running,
                });
            }
            UiDelta::Text(text) => self.append_answer(&text),
            UiDelta::Reasoning(text) => self.append_reasoning(&text),
            UiDelta::ToolArgs(text) => self.append_tool_args(&text),
        }
    }

    /// Append answer text, extending the last Answer block or starting one.
    fn append_answer(&mut self, text: &str) {
        if let Some(Block::Answer { text: s, .. }) = self.blocks.last_mut() {
            s.push_str(text);
        } else {
            self.blocks.push(Block::Answer {
                text: text.to_owned(),
                done: false,
            });
        }
    }

    /// Mark the last streaming answer block as complete so it re-renders as
    /// markdown. Called when any new block starts after an answer, and at turn end.
    fn finish_open_answer(&mut self) {
        if let Some(Block::Answer { done, .. }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, Block::Answer { done: false, .. }))
        {
            *done = true;
        }
    }

    /// Append reasoning text to the open (un-elapsed) thinking block.
    fn append_reasoning(&mut self, text: &str) {
        if let Some(Block::Thinking {
            text: s,
            elapsed: None,
            ..
        }) = self.blocks.last_mut()
        {
            s.push_str(text);
        } else {
            self.blocks.push(Block::Thinking {
                text: text.to_owned(),
                started: Instant::now(),
                elapsed: None,
            });
        }
    }

    /// Append argument JSON to the last (running) tool block.
    fn append_tool_args(&mut self, text: &str) {
        if let Some(Block::Tool { args, .. }) = self.blocks.last_mut() {
            args.push_str(text);
        }
    }

    /// Mark the most recent open thinking block as finished, recording how long
    /// it ran so it folds to a summary.
    fn finish_open_thinking(&mut self) {
        self.touch();
        if let Some(Block::Thinking {
            started, elapsed, ..
        }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(b, Block::Thinking { elapsed: None, .. }))
        {
            *elapsed = Some(started.elapsed());
        }
    }

    /// React to a persisted bus event — tool completions/failures, which the
    /// live sink does not carry (it only sees the model's call request). The
    /// result is attached to the most recent running tool card.
    fn apply_event(&mut self, event: &CoreEvent) {
        self.touch();
        match &event.payload {
            EventPayload::Tool(ToolEvent::Completed {
                result,
                output_bytes,
                ..
            }) => {
                let summary = first_text_line(result).map_or_else(
                    || format!("ok ({output_bytes} bytes)"),
                    |first| format!("{first}  ({output_bytes} bytes)"),
                );
                self.set_last_tool_result(result.is_error, &summary);
            }
            EventPayload::Tool(ToolEvent::Failed { error, .. }) => {
                self.set_last_tool_failed(&format!("[{}] {}", error.code, error.message));
            }
            _ => {}
        }
    }

    fn last_running_tool(&mut self) -> Option<&mut ToolStatus> {
        self.blocks.iter_mut().rev().find_map(|b| {
            if let Block::Tool { status: s @ ToolStatus::Running, .. } = b {
                Some(s)
            } else {
                None
            }
        })
    }

    fn set_last_tool_result(&mut self, error: bool, summary: &str) {
        if let Some(status) = self.last_running_tool() {
            *status = ToolStatus::Done { error, summary: summary.to_owned() };
        }
    }

    fn set_last_tool_failed(&mut self, msg: &str) {
        if let Some(status) = self.last_running_tool() {
            *status = ToolStatus::Failed(msg.to_owned());
        }
    }

    fn push_summary(&mut self, outcome: &TurnOutcome) {
        self.context_tokens = outcome.context_tokens;
        self.streaming = false;
        self.finish_open_answer();
        self.blocks.push(Block::Summary(format!(
            "[{} round(s) · {}in/{}out]",
            outcome.rounds, outcome.usage.input_tokens, outcome.usage.output_tokens
        )));
        self.touch();
    }

    /// Point the header at a new session id (after creation or compaction).
    fn set_session(&mut self, sid: &SessionId) {
        self.session_id = Some(sid.0.clone());
    }
}

/// The first non-empty text line of a tool result, for a one-line preview.
fn first_text_line(output: &ToolOutput) -> Option<String> {
    output.content.iter().find_map(|c| match c {
        crate::core::payload::Content::Text(t) => t
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| first_line(l, 200)),
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
        AppState::new("prov", "model", 1_000_000, 128_000)
    }

    /// The conversation rendered to a single string, for content assertions.
    fn rendered(s: &AppState) -> String {
        s.render_lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A resumed session's rebuilt context must be rendered into the pane so the
    /// user sees what it was about: the system seed is hidden, user turns show,
    /// assistant text/tool-calls show, tool results attach to the card, and a
    /// separator marks where the live continuation begins.
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
        let view = rendered(&s);

        assert!(
            !view.contains("you are an agent"),
            "system seed must be hidden"
        );
        assert!(view.contains("remember 42"), "user turn missing: {view:?}");
        assert!(view.contains("noted 42"), "assistant text missing");
        assert!(view.contains("tool: shell"), "tool call missing");
        assert!(view.contains("hi"), "tool result missing");
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
        assert!(
            s.blocks.is_empty(),
            "empty history must not render anything"
        );
    }

    /// Streaming reasoning then answer text: the thinking block folds (records
    /// elapsed) when the answer starts, and the answer accumulates. This is the
    /// "expand while thinking, collapse when output begins" behavior.
    #[test]
    fn reasoning_then_answer_folds_thinking() {
        let mut s = state();
        s.apply_delta(UiDelta::BlockStart(Block_::Reasoning));
        s.apply_delta(UiDelta::Reasoning("hmm".to_owned()));
        s.apply_delta(UiDelta::BlockStart(Block_::Text));
        s.apply_delta(UiDelta::Text("answer".to_owned()));

        let folded = matches!(
            s.blocks.first(),
            Some(Block::Thinking {
                elapsed: Some(_),
                ..
            })
        );
        assert!(folded, "thinking must fold once the answer starts");
        assert!(
            matches!(s.blocks.last(), Some(Block::Answer { text: a, done: false }) if a == "answer")
        );
    }

    /// A tool-call block's result is attached to the card from a bus Completed
    /// event, since the live sink never carries tool *results*.
    #[test]
    fn tool_result_attaches_to_card() {
        use crate::core::payload::{Content, ToolEvent, ToolOutput};
        use crate::core::{EventSource, SourceKind};

        let mut s = state();
        s.apply_delta(UiDelta::BlockStart(Block_::Tool("shell".to_owned())));
        s.apply_delta(UiDelta::ToolArgs(r#"{"command":"ls"}"#.to_owned()));

        let event = CoreEvent {
            schema_version: "1".to_owned(),
            seq: 0,
            session_id: SessionId("x".to_owned()),
            timestamp: chrono::Utc::now(),
            source: EventSource {
                kind: SourceKind::Runtime,
                id: "x".to_owned(),
            },
            parent_event_id: None,
            turn_id: None,
            payload: EventPayload::Tool(ToolEvent::Completed {
                tool_call_event_id: crate::core::EventId {
                    session_id: SessionId("x".to_owned()),
                    seq: 0,
                },
                result: ToolOutput {
                    content: vec![Content::Text("file.txt".to_owned())],
                    is_error: false,
                    error_code: None,
                },
                duration_ms: 5,
                output_bytes: 8,
                artifacts_created: vec![],
            }),
        };
        s.apply_event(&event);

        let done = matches!(
            s.blocks.last(),
            Some(Block::Tool {
                status: ToolStatus::Done { error: false, .. },
                ..
            })
        );
        assert!(done, "completed event must mark the card done");
        assert!(rendered(&s).contains("file.txt"), "result preview missing");
    }

    /// Scroll math: following the bottom shows the tail; scrolling up pins an
    /// absolute top that stays fixed even when new content is appended below.
    #[test]
    fn scroll_offset_follows_then_holds() {
        let mut s = state();
        // 100 total lines in a 10-high viewport: bottom-follow shows top=90.
        assert_eq!(s.scroll_offset(100, 10), 90);
        // Scroll up 5 → view pins at top=85.
        s.scroll_up(5);
        assert_eq!(s.scroll_offset(100, 10), 85);
        // NEW: 10 more lines arrive (total=110). Pinned top must NOT move — the
        // old distance-from-bottom approach would have returned 95 here.
        assert_eq!(
            s.scroll_offset(110, 10),
            85,
            "pinned top must not drift with new content"
        );
        // Scroll up far past the top → clamps at 0, never negative.
        s.scroll_up(1000);
        assert_eq!(s.scroll_offset(110, 10), 0);
        // Scroll back to bottom resumes following.
        s.follow_bottom();
        assert_eq!(s.scroll_offset(110, 10), 100);
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
