//! Chat panel: the agent conversation surface (doc/gpui-app.md §3.3).
//!
//! A message list (user turns, assistant text / reasoning, tool cards) plus a
//! single-line input box, driven entirely through the [`ClientProtocol`] trait
//! — so the identical panel code runs against a local in-process core or a
//! remote transport. Split into a transport-independent [`ChatState`] (a pure
//! fold of protocol events into render rows — unit-testable without a UI) and
//! the [`Chat`] view (gpui: async protocol plumbing, keyboard, layout). The
//! view is a thin shell over the state; every interesting rule lives in the
//! state.
//!
//! ## Terminology
//!
//! Two things both informally called "view" are kept distinct here:
//! [`SessionView`] is the backend-folded **data snapshot** (input); the
//! **element tree** `Chat::render` produces is the UI (output). Code and docs
//! do not use "view" for the latter.

use std::sync::Arc;

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, KeyBinding, Render, Styled, Task, Window, div,
    prelude::*,
};
use ominiforge::core::SessionId;
use ominiforge::core::payload::{
    BlockContent, ErrorEvent, EventPayload, ModelEvent, ToolEvent, TurnEvent,
};
use ominiforge::gateway::view::SessionView;
use ominiforge::gateway::{Delta, GatewayEvent};
use ominiforge_net::{ClientProtocol, ConnectionState};

use crate::theme::Theme;

// gpui's `actions!` macro derives `PartialEq` without `Eq`; not fixable
// downstream (same allowance as Phase 3.2).
#[allow(clippy::derive_partial_eq_without_eq)]
mod chat_actions {
    gpui::actions!(
        chat,
        [
            /// Send the input box contents (Enter).
            Send,
            /// Scroll the message list down one line (j).
            ScrollDown,
            /// Scroll the message list up one line (k).
            ScrollUp,
            /// Close the chat panel (q).
            Close,
        ]
    );
}
use chat_actions::{Close, ScrollDown, ScrollUp, Send};

/// Key context for this panel's bindings (doc/gpui-app.md §2.3).
pub const CHAT_CONTEXT: &str = "Chat";

/// Element id of the panel root, used by tests via `debug_bounds`.
pub const CHAT_PANEL_ID: &str = "chat-panel";

/// Register the chat panel's keybindings on the app.
pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("enter", Send, Some(CHAT_CONTEXT)),
        KeyBinding::new("j", ScrollDown, Some(CHAT_CONTEXT)),
        KeyBinding::new("k", ScrollUp, Some(CHAT_CONTEXT)),
        KeyBinding::new("q", Close, Some(CHAT_CONTEXT)),
    ]);
}

/// One renderable row in the message list — the panel's folded conversation.
///
/// Derived from the committed [`SessionView`] items plus live events. `Row`
/// is the **render model**; the protocol's `ViewItem`/`CoreEvent` are the
/// transport/data model. Keeping them separate lets the render model grow
/// tool-specific presentation (`doc/gpui-design.md` §4) without touching the
/// protocol fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A user turn. `seq` is the committed fork point; `None` while the row
    /// is a locally-optimistic pending send not yet confirmed by the backend.
    User {
        text: String,
        seq: Option<u64>,
        /// Delivery state of an optimistic send.
        pending: PendingState,
    },
    /// Assistant answer text (settled or still streaming).
    Text { text: String },
    /// Assistant reasoning (dimmed).
    Reasoning { text: String },
    /// A tool call card. `call_seq` (the seq of the model's `ToolCall` block)
    /// pairs the card with its `ToolEvent::Completed/Failed` outcome (which
    /// points back via `tool_call_event_id.seq`).
    Tool {
        call_seq: u64,
        name: String,
        summary: Option<String>,
        /// `true` once the result committed.
        done: bool,
        /// `true` if the call failed.
        error: bool,
    },
    /// A committed error.
    Error { message: String },
}

/// Delivery state of a locally-optimistic user row (`doc/gpui-design.md` §4):
/// every optimistic row must resolve to Confirmed or Failed — never hang.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// Not optimistic (came from the committed fold).
    Settled,
    /// Sent, awaiting the backend's `TurnEvent::Started` confirmation.
    Pending,
    /// The send call itself failed; show with a retry affordance.
    Failed,
}

/// Transport-independent conversation state: the protocol event fold.
///
/// Holds the committed rows, the in-flight streaming text block, the draft
/// input, and a connection/health note. Pure data + pure methods — no gpui,
/// no async — so the fold rules are unit-testable without a window.
#[derive(Debug, Default)]
pub struct ChatState {
    /// Settled + optimistic rows, oldest first.
    pub rows: Vec<Row>,
    /// The session this state mirrors (set once opened).
    pub session: Option<SessionId>,
    /// Whether the subscription passed the replay boundary (live events fold).
    replay_done: bool,
    /// Whether a turn is currently running (drives the cancel affordance).
    pub turn_running: bool,
    /// The in-flight streaming text/reasoning block, folded from `Delta`s and
    /// not yet settled into a row.
    streaming: Option<(bool, String)>,
    /// The draft input text.
    pub input: String,
    /// A subscription/transport problem to surface (offline, dead stream).
    /// `None` when healthy.
    pub notice: Option<String>,
}

impl ChatState {
    /// Seed the state from the committed data snapshot (how a session opens).
    pub fn open(&mut self, session: SessionId, view: &SessionView) {
        self.session = Some(session);
        self.turn_running = view.turn_running;
        self.rows.clear();
        self.streaming = None;
        for item in &view.items {
            use ominiforge::gateway::view::ViewItem as V;
            let row = match item {
                V::User { text, seq, .. } => Row::User {
                    text: text.clone(),
                    seq: Some(*seq),
                    pending: PendingState::Settled,
                },
                V::Text { text, .. } => Row::Text { text: text.clone() },
                V::Reasoning { text, .. } => Row::Reasoning { text: text.clone() },
                V::Tool {
                    id,
                    name,
                    summary,
                    status,
                    ..
                } => {
                    use ominiforge::gateway::view::ViewToolStatus as S;
                    Row::Tool {
                        call_seq: *id,
                        name: name.clone(),
                        summary: summary.clone(),
                        done: !matches!(status, S::Running),
                        error: matches!(status, S::Error),
                    }
                }
                V::Error { message, .. } => Row::Error {
                    message: message.clone(),
                },
                // Todo / Activity cards are secondary; Phase 3.4 renders the
                // conversation core. They land with richer cards later.
                V::Todo { .. } | V::Activity { .. } => continue,
            };
            self.rows.push(row);
        }
    }

    /// Fold one protocol event. Committed events before `ReplayEnd` are
    /// history (already reflected in [`open`](Self::open)) and skipped; live
    /// events after the boundary mutate state.
    pub fn apply(&mut self, event: &GatewayEvent) {
        match event {
            GatewayEvent::ReplayEnd => self.replay_done = true,
            GatewayEvent::Delta(delta) if self.replay_done => self.apply_delta(delta),
            GatewayEvent::Event { event } if self.replay_done => {
                self.apply_committed(&event.payload, event.seq);
            }
            GatewayEvent::TurnSettled { .. } => {
                self.turn_running = false;
                self.settle_streaming();
            }
            _ => {}
        }
    }

    /// Fold one live committed event: confirm an optimistic user row, open a
    /// tool card, settle a tool card. These are the committed counterparts
    /// that keep the front-end rows a faithful projection of the event log.
    fn apply_committed(&mut self, payload: &EventPayload, seq: u64) {
        match payload {
            EventPayload::Turn(TurnEvent::Started {
                input: Some(input), ..
            }) => self.confirm_user_row(input, seq),
            EventPayload::Model(ModelEvent::ContentBlock {
                content: BlockContent::ToolCall { name, summary, .. },
                ..
            }) => {
                // The pairing key is this block's own seq (a `ToolEvent`
                // points back at it via `tool_call_event_id.seq`).
                self.rows.push(Row::Tool {
                    call_seq: seq,
                    name: name.clone(),
                    summary: summary.clone(),
                    done: false,
                    error: false,
                });
            }
            EventPayload::Tool(ToolEvent::Completed {
                tool_call_event_id, ..
            }) => self.settle_tool(tool_call_event_id.seq, false),
            EventPayload::Tool(ToolEvent::Failed {
                tool_call_event_id, ..
            }) => self.settle_tool(tool_call_event_id.seq, true),
            EventPayload::Error(ErrorEvent::Raised(detail)) => {
                self.rows.push(Row::Error {
                    message: detail.message.clone(),
                });
            }
            _ => {}
        }
    }

    /// Match the optimistic pending user row to its committed `TurnEvent::Started`.
    /// The text is the identity: find the earliest still-pending row with the
    /// same text and mark it settled with its committed seq. If none matches
    /// (a turn from another client), append it as a fresh settled row.
    fn confirm_user_row(&mut self, input: &str, seq: u64) {
        for row in &mut self.rows {
            if let Row::User {
                text,
                seq: row_seq,
                pending,
            } = row
                && *pending == PendingState::Pending
                && text == input
            {
                *row_seq = Some(seq);
                *pending = PendingState::Settled;
                return;
            }
        }
        self.rows.push(Row::User {
            text: input.to_owned(),
            seq: Some(seq),
            pending: PendingState::Settled,
        });
    }

    fn settle_tool(&mut self, call_seq: u64, error: bool) {
        for row in self.rows.iter_mut().rev() {
            if let Row::Tool {
                call_seq: id,
                done,
                error: err,
                ..
            } = row
                && *id == call_seq
            {
                *done = true;
                *err = error;
                return;
            }
        }
    }

    fn apply_delta(&mut self, delta: &Delta) {
        match delta {
            Delta::Text { text, .. } => self.push_streaming(false, text),
            Delta::Reasoning { text, .. } => self.push_streaming(true, text),
            _ => {}
        }
    }

    fn push_streaming(&mut self, reasoning: bool, text: &str) {
        match &mut self.streaming {
            Some((r, buf)) if *r == reasoning => buf.push_str(text),
            _ => {
                self.settle_streaming();
                self.streaming = Some((reasoning, text.to_owned()));
            }
        }
    }

    /// Flush the in-flight streaming block into a settled row, if any.
    fn settle_streaming(&mut self) {
        if let Some((reasoning, text)) = self.streaming.take() {
            let row = if reasoning {
                Row::Reasoning { text }
            } else {
                Row::Text { text }
            };
            self.rows.push(row);
        }
    }

    /// Iterate the rows to render: settled rows plus the in-flight streaming
    /// block, by reference (no per-frame clone).
    pub fn visible_rows(&self) -> impl Iterator<Item = RowRef<'_>> {
        let streaming = self.streaming.as_ref().map(|(reasoning, text)| {
            if *reasoning {
                RowRef::Reasoning(text)
            } else {
                RowRef::Text(text)
            }
        });
        self.rows.iter().map(RowRef::from).chain(streaming)
    }
}

/// A borrowed view of a [`Row`] for rendering — avoids cloning the row vec
/// every frame (`doc/gpui-design.md` §3, performance-as-design).
#[derive(Debug, Clone, Copy)]
pub enum RowRef<'a> {
    /// A user turn.
    User {
        text: &'a str,
        seq: Option<u64>,
        pending: PendingState,
    },
    /// Assistant text.
    Text(&'a str),
    /// Reasoning text.
    Reasoning(&'a str),
    /// A tool card.
    Tool {
        name: &'a str,
        summary: Option<&'a str>,
        done: bool,
        error: bool,
    },
    /// An error.
    Error(&'a str),
}

impl<'a> From<&'a Row> for RowRef<'a> {
    fn from(row: &'a Row) -> Self {
        match row {
            Row::User { text, seq, pending } => Self::User {
                text,
                seq: *seq,
                pending: *pending,
            },
            Row::Text { text } => Self::Text(text),
            Row::Reasoning { text } => Self::Reasoning(text),
            Row::Tool {
                name,
                summary,
                done,
                error,
                ..
            } => Self::Tool {
                name,
                summary: summary.as_deref(),
                done: *done,
                error: *error,
            },
            Row::Error { message } => Self::Error(message),
        }
    }
}

/// Emitted when the user asks to close the panel (q). A struct now; promote
/// to an enum when a second panel event exists (YAGNI).
pub struct ChatClosed;

/// The chat panel view: protocol plumbing + keyboard + layout over
/// [`ChatState`].
pub struct Chat {
    client: Arc<dyn ClientProtocol>,
    state: ChatState,
    focus_handle: FocusHandle,
    /// Scroll offset in rows for j/k navigation (line-based for now).
    scroll: usize,
    /// The live subscription task; held so it is not dropped (which would
    /// cancel the stream fold) until the panel itself is dropped.
    #[allow(dead_code)]
    subscription: Option<Task<()>>,
}

impl Chat {
    /// Create a chat panel attached to `client`. No session is open until
    /// [`open_session`](Self::open_session) is called.
    pub fn new(client: Arc<dyn ClientProtocol>, cx: &mut Context<Self>) -> Self {
        Self {
            client,
            state: ChatState::default(),
            focus_handle: cx.focus_handle(),
            scroll: 0,
            subscription: None,
        }
    }

    /// Open a session: load its committed snapshot, then subscribe to its live
    /// event stream and fold updates into the state. Failures surface in
    /// `state.notice` rather than vanishing (fail loud).
    pub fn open_session(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        self.subscription = Some(cx.spawn(async move |this, cx| {
            match client.session_view(&session).await {
                Ok(view) => {
                    let _ = this.update(cx, |chat, cx| {
                        chat.state.open(session.clone(), &view);
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |chat, cx| {
                        chat.state.notice = Some(format!("failed to load session: {e:#}"));
                        cx.notify();
                    });
                    return;
                }
            }
            match client.subscribe_session(&session).await {
                Ok(mut stream) => {
                    use futures_lite::StreamExt as _;
                    while let Some(event) = stream.next().await {
                        if this
                            .update(cx, |chat, cx| {
                                chat.state.apply(&event);
                                cx.notify();
                            })
                            .is_err()
                        {
                            // Panel dropped: stop the fold quietly.
                            return;
                        }
                    }
                    // The stream ended while the panel is still alive (e.g. a
                    // dropped remote connection). Say so instead of going dead.
                    let _ = this.update(cx, |chat, cx| {
                        chat.state.notice = Some("connection closed".to_owned());
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |chat, cx| {
                        chat.state.notice = Some(format!("subscription failed: {e:#}"));
                        cx.notify();
                    });
                }
            }
        }));
    }

    /// The current conversation state (for tests and the workspace).
    #[must_use]
    pub const fn state(&self) -> &ChatState {
        &self.state
    }

    fn send(&mut self, _: &Send, _window: &mut Window, cx: &mut Context<Self>) {
        let text = std::mem::take(&mut self.state.input);
        if text.trim().is_empty() {
            return;
        }
        let Some(session) = self.state.session.clone() else {
            return;
        };
        // Optimistic render (doc/gpui-design.md §4): show immediately, then
        // confirm against the committed `TurnEvent::Started` or mark failed.
        self.state.rows.push(Row::User {
            text: text.clone(),
            seq: None,
            pending: PendingState::Pending,
        });
        self.state.turn_running = true;
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            if let Err(e) = client.send_message(&session, text, None, None).await {
                let _ = this.update(cx, |chat, cx| {
                    chat.state.fail_pending_user();
                    chat.state.notice = Some(format!("send failed: {e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn scroll_down(&mut self, _: &ScrollDown, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll = self.scroll.saturating_add(1);
        cx.notify();
    }

    fn scroll_up(&mut self, _: &ScrollUp, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll = self.scroll.saturating_sub(1);
        cx.notify();
    }

    // `&mut self` is required by gpui's action-handler signature even though
    // close only emits an event.
    #[allow(clippy::unused_self)]
    fn close(&mut self, _: &Close, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ChatClosed);
    }
}

impl ChatState {
    /// Mark the earliest still-pending user row as failed (send errored).
    fn fail_pending_user(&mut self) {
        for row in self.rows.iter_mut().rev() {
            if let Row::User { pending, .. } = row
                && *pending == PendingState::Pending
            {
                *pending = PendingState::Failed;
                return;
            }
        }
    }
}

impl EventEmitter<ChatClosed> for Chat {}

impl Focusable for Chat {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Chat {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let connected = self.client.connection_state() != ConnectionState::Disconnected;

        let list = self
            .state
            .visible_rows()
            .skip(self.scroll)
            .map(|row| render_row(&row, &theme))
            .collect::<Vec<_>>();

        let mut root = div()
            .id(CHAT_PANEL_ID)
            .key_context(CHAT_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::close))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.canvas_base)
            .text_color(theme.text_primary);

        if let Some(notice) = &self.state.notice {
            root = root.child(
                div()
                    .id("chat-notice")
                    .px_2()
                    .py_1()
                    .bg(theme.canvas_overlay)
                    .text_color(theme.state_running)
                    .child(notice.clone()),
            );
        }

        root.child(
            div()
                .id("chat-messages")
                .flex_1()
                .flex_col()
                .gap_1()
                .p_2()
                .overflow_hidden()
                .children(list),
        )
        .child(
            div()
                .id("chat-input")
                .p_2()
                .bg(theme.canvas_raised)
                .border_t_1()
                .border_color(theme.border_default)
                .child(if connected {
                    self.state.input.clone()
                } else {
                    "(disconnected)".to_owned()
                }),
        )
    }
}

fn render_row(row: &RowRef<'_>, theme: &Theme) -> gpui::AnyElement {
    match row {
        RowRef::User { text, pending, .. } => {
            let marker = match pending {
                PendingState::Settled => ">",
                PendingState::Pending => "…",
                PendingState::Failed => "✗",
            };
            let color = match pending {
                PendingState::Failed => theme.state_error,
                _ => theme.user_text,
            };
            div()
                .text_color(color)
                .child(format!("{marker} {text}"))
                .into_any_element()
        }
        RowRef::Text(text) => div().child((*text).to_owned()).into_any_element(),
        RowRef::Reasoning(text) => div()
            .text_color(theme.reasoning_text)
            .italic()
            .child((*text).to_owned())
            .into_any_element(),
        RowRef::Tool {
            name,
            summary,
            done,
            error,
        } => {
            let (status, color) = if *error {
                ("✗", theme.state_error)
            } else if *done {
                ("✓", theme.state_done)
            } else {
                ("…", theme.state_running)
            };
            let label = summary.map_or_else(
                || format!("{status} {name}"),
                |s| format!("{status} {name}: {s}"),
            );
            div().text_color(color).child(label).into_any_element()
        }
        RowRef::Error(message) => div()
            .text_color(theme.state_error)
            .child(format!("error: {message}"))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use ominiforge::gateway::view::{SessionView, ViewItem, ViewToolStatus};

    fn view_with(items: Vec<ViewItem>, turn_running: bool) -> SessionView {
        SessionView {
            items,
            last_seq: Some(1),
            turn_running,
            runtime_models: Vec::new(),
        }
    }

    /// Opening folds the committed snapshot into render rows.
    #[test]
    fn open_folds_committed_view() {
        let mut state = ChatState::default();
        let view = view_with(
            vec![
                ViewItem::User {
                    id: 1,
                    text: "hello".into(),
                    seq: 7,
                },
                ViewItem::Text {
                    id: 2,
                    text: "hi there".into(),
                },
                ViewItem::Tool {
                    id: 3,
                    seq: 3,
                    name: "read".into(),
                    args: String::new(),
                    status: ViewToolStatus::Done,
                    summary: Some("lib.rs".into()),
                    result: None,
                    diagnostics: None,
                    error_code: None,
                    call_id: None,
                    approval_pending: false,
                    view: None,
                },
            ],
            false,
        );
        state.open(SessionId("s1".into()), &view);
        assert_eq!(state.rows.len(), 3);
        assert!(
            matches!(&state.rows[0], Row::User { text, seq: Some(7), pending: PendingState::Settled } if text == "hello")
        );
        assert!(matches!(&state.rows[1], Row::Text { text } if text == "hi there"));
        assert!(matches!(&state.rows[2], Row::Tool { name, done: true, .. } if name == "read"));
    }

    /// Deltas before the replay boundary are history; after `ReplayEnd` they
    /// fold into a live streaming row, then settle on turn end.
    #[test]
    fn deltas_fold_only_after_replay_end() {
        let mut state = ChatState::default();
        state.open(SessionId("s".into()), &view_with(Vec::new(), true));

        state.apply(&GatewayEvent::Delta(Delta::Text {
            index: 0,
            text: "stale".into(),
        }));
        assert!(state.visible_rows().next().is_none());

        state.apply(&GatewayEvent::ReplayEnd);
        state.apply(&GatewayEvent::Delta(Delta::Text {
            index: 0,
            text: "Hel".into(),
        }));
        state.apply(&GatewayEvent::Delta(Delta::Text {
            index: 0,
            text: "lo".into(),
        }));
        let rows: Vec<_> = state.visible_rows().collect();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0], RowRef::Text(t) if t == "Hello"));

        state.apply(&GatewayEvent::TurnSettled { incomplete: None });
        assert!(state.streaming.is_none());
        assert!(!state.turn_running);
        assert!(matches!(&state.rows[0], Row::Text { text } if text == "Hello"));
    }

    /// An optimistic user row is confirmed by the committed `TurnEvent::Started`
    /// (matched by text, upgraded with its seq), not duplicated.
    #[test]
    fn committed_turn_confirms_optimistic_row() {
        let mut state = ChatState::default();
        state.open(SessionId("s".into()), &view_with(Vec::new(), true));
        state.apply(&GatewayEvent::ReplayEnd);

        // The optimistic row the view pushed on send.
        state.rows.push(Row::User {
            text: "fix the bug".into(),
            seq: None,
            pending: PendingState::Pending,
        });

        let committed = committed_turn_started("fix the bug", 42);
        state.apply(&committed);

        assert_eq!(state.rows.len(), 1);
        assert!(
            matches!(&state.rows[0], Row::User { text, seq: Some(42), pending: PendingState::Settled } if text == "fix the bug")
        );
    }

    /// A committed `TurnEvent::Started` with no matching optimistic row (a turn
    /// from another client) appends a settled row.
    #[test]
    fn unmatched_committed_turn_appends_row() {
        let mut state = ChatState::default();
        state.open(SessionId("s".into()), &view_with(Vec::new(), true));
        state.apply(&GatewayEvent::ReplayEnd);

        let committed = committed_turn_started("from elsewhere", 9);
        state.apply(&committed);
        assert!(
            matches!(&state.rows[0], Row::User { text, seq: Some(9), pending: PendingState::Settled } if text == "from elsewhere")
        );
    }

    /// A live tool call opens a running card; its `ToolEvent::Completed`
    /// (paired by event id) settles it.
    #[test]
    fn live_tool_card_opens_and_settles() {
        let mut state = ChatState::default();
        state.open(SessionId("s".into()), &view_with(Vec::new(), true));
        state.apply(&GatewayEvent::ReplayEnd);

        // Model emits the tool call block at seq 10.
        state.apply(&committed_tool_call(10, "read", Some("lib.rs")));
        assert!(
            matches!(&state.rows[0], Row::Tool { name, done: false, error: false, .. } if name == "read")
        );

        // The tool completes, pointing back at the call's event id (10).
        state.apply(&committed_tool_done(10));
        assert!(matches!(
            &state.rows[0],
            Row::Tool {
                done: true,
                error: false,
                ..
            }
        ));
    }

    // -- helpers: build committed `GatewayEvent::Event` frames --

    fn frame(seq: u64, payload: EventPayload) -> GatewayEvent {
        use chrono::{TimeZone, Utc};
        use ominiforge::core::{EventSource, SCHEMA_VERSION, SourceKind};
        GatewayEvent::Event {
            event: Box::new(ominiforge::core::CoreEvent {
                schema_version: SCHEMA_VERSION.to_owned(),
                seq,
                session_id: SessionId("s".into()),
                timestamp: Utc.timestamp_opt(0, 0).unwrap(),
                source: EventSource {
                    kind: SourceKind::Runtime,
                    id: "test".into(),
                },
                parent_event_id: None,
                turn_id: None,
                payload,
            }),
        }
    }

    fn committed_turn_started(input: &str, seq: u64) -> GatewayEvent {
        frame(
            seq,
            EventPayload::Turn(TurnEvent::Started {
                turn_id: ominiforge::core::TurnId("t".into()),
                input: Some(input.to_owned()),
            }),
        )
    }

    fn committed_tool_call(seq: u64, name: &str, summary: Option<&str>) -> GatewayEvent {
        frame(
            seq,
            EventPayload::Model(ModelEvent::ContentBlock {
                request_id: "r".into(),
                index: 0,
                content: BlockContent::ToolCall {
                    id: format!("call-{seq}"),
                    name: name.to_owned(),
                    arguments: "{}".into(),
                    summary: summary.map(str::to_owned),
                },
            }),
        )
    }

    fn committed_tool_done(call_seq: u64) -> GatewayEvent {
        frame(
            call_seq + 100,
            EventPayload::Tool(ToolEvent::Completed {
                tool_call_event_id: ominiforge::core::EventId {
                    session_id: SessionId("s".into()),
                    seq: call_seq,
                },
                result: ominiforge::core::payload::ToolOutput {
                    content: Vec::new(),
                    is_error: false,
                    error_code: None,
                },
                duration_ms: 1,
                output_bytes: 0,
                artifacts_created: Vec::new(),
            }),
        )
    }
}
