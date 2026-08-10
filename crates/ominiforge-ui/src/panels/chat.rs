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
    /// The in-progress edit of a committed user turn (`doc/gpui-design.md`
    /// §4.2 edit-as-fork): `Some((seq, original))` while a settled user row is
    /// loaded in the input box for editing. `seq` is that turn's committed
    /// fork point; `original` is its committed text (restored on cancel).
    /// `None` in the normal compose state. Per the resolved semantics, an
    /// editing send ALWAYS forks (no unchanged-text special case).
    pub editing: Option<(u64, String)>,
    /// A subscription/transport problem to surface (offline, dead stream).
    /// `None` when healthy.
    pub notice: Option<String>,
}

/// What a send resolves to, decided by [`ChatState::resolve_send`] (the single
/// place the compose-vs-fork rule lives, so the view stays a thin shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendAction {
    /// Empty input or no open session: nothing to do.
    Noop,
    /// A normal send into the current session.
    Compose {
        /// The session to send into.
        session: SessionId,
        /// The message text.
        text: String,
    },
    /// An editing send: fork the current session at `fork_seq`, then send
    /// `text` into the new branch.
    EditFork {
        /// The session being branched.
        session: SessionId,
        /// The committed user-turn seq to fork at.
        fork_seq: u64,
        /// The (edited) message text to send into the branch.
        text: String,
    },
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

    // ---- Edit-as-fork (doc/gpui-design.md §4.2) ----

    /// Begin editing a committed user turn: load its text into the input box
    /// and remember the fork point + original text. The first settled user
    /// turn is excluded (forking an empty prefix is a no-op — §4.2), as is a
    /// pending/unknown seq. No-op unless the seq names a settled user row.
    pub fn begin_edit(&mut self, seq: u64) {
        if self.first_settled_user_seq() == Some(seq) {
            return;
        }
        for row in &self.rows {
            if let Row::User {
                text,
                seq: Some(s),
                pending: PendingState::Settled,
            } = row
                && *s == seq
            {
                self.editing = Some((seq, text.clone()));
                self.input = text.clone();
                return;
            }
        }
    }

    /// The seq of the first settled (committed) user turn, if any.
    fn first_settled_user_seq(&self) -> Option<u64> {
        self.rows.iter().find_map(|r| match r {
            Row::User {
                seq: Some(s),
                pending: PendingState::Settled,
                ..
            } => Some(*s),
            _ => None,
        })
    }

    /// Cancel the edit: clear the input and leave edit mode. The committed row
    /// is untouched (the log is immutable).
    pub fn cancel_edit(&mut self) {
        if self.editing.take().is_some() {
            self.input.clear();
        }
    }

    /// Decide what a send does, consuming the draft. Pure: the view performs
    /// the returned effect. An editing send resolves to [`SendAction::EditFork`]
    /// unconditionally (sending the edited turn always forks — no
    /// unchanged-text diff), and clears the edit state. On a no-op the draft
    /// is restored (nothing consumed).
    pub fn resolve_send(&mut self) -> SendAction {
        let text = std::mem::take(&mut self.input);
        let Some(session) = self.session.clone() else {
            self.input = text;
            return SendAction::Noop;
        };
        if text.trim().is_empty() {
            self.input = text;
            return SendAction::Noop;
        }
        if let Some((fork_seq, _original)) = self.editing.take() {
            return SendAction::EditFork {
                session,
                fork_seq,
                text,
            };
        }
        SendAction::Compose { session, text }
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

/// Emitted when an editing send forks the session: carries the new branch id
/// so the workspace can switch to it (doc/gpui-design.md §4.2 edit-as-fork).
pub struct SessionSelected {
    /// The session to open.
    pub session_id: SessionId,
}

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
        match self.state.resolve_send() {
            SendAction::Noop => {}
            SendAction::Compose { session, text } => self.compose_send(&session, text, cx),
            SendAction::EditFork {
                session,
                fork_seq,
                text,
            } => self.edit_fork_send(&session, fork_seq, text, cx),
        }
        cx.notify();
    }

    /// A normal send into the current session: optimistic render, then confirm
    /// against the committed `TurnEvent::Started` or mark failed
    /// (doc/gpui-design.md §4).
    // `&mut self`/`cx` are used via `cx.spawn`/`cx.listener`; clippy's
    // needless_pass_by_ref_mut misses that capture. Same allowance as below.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn compose_send(&mut self, session: &SessionId, text: String, cx: &mut Context<Self>) {
        self.state.rows.push(Row::User {
            text: text.clone(),
            seq: None,
            pending: PendingState::Pending,
        });
        self.state.turn_running = true;
        let client = Arc::clone(&self.client);
        let session = session.clone();
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
    }

    /// An editing send: fork at the edited turn's seq, send the edited text
    /// into the new branch, then emit [`SessionSelected`] so the workspace
    /// switches to it. No optimistic row here — the branch is a fresh session
    /// the workspace opens; the original session's log is untouched.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn edit_fork_send(
        &mut self,
        session: &SessionId,
        fork_seq: u64,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let client = Arc::clone(&self.client);
        let session = session.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                let new_id = client.fork_session(&session, fork_seq).await?;
                client.send_message(&new_id, text, None, None).await?;
                anyhow::Ok(new_id)
            }
            .await;
            let _ = this.update(cx, |chat, cx| match result {
                Ok(new_id) => cx.emit(SessionSelected { session_id: new_id }),
                Err(e) => {
                    chat.state.notice = Some(format!("fork failed: {e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Begin editing the committed user turn at `seq` (hover affordance).
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn begin_edit(&mut self, seq: u64, cx: &mut Context<Self>) {
        self.state.begin_edit(seq);
        cx.notify();
    }

    /// Exit edit mode (the input-region cancel affordance).
    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.state.cancel_edit();
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
impl EventEmitter<SessionSelected> for Chat {}

impl Focusable for Chat {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Chat {
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let connected = self.client.connection_state() != ConnectionState::Disconnected;

        // The first settled user turn has no edit affordance (forking an empty
        // prefix is a no-op — §4.2). Computed once for the whole list.
        let first_user_seq = self.state.rows.iter().find_map(|r| match r {
            Row::User {
                seq: Some(s),
                pending: PendingState::Settled,
                ..
            } => Some(*s),
            _ => None,
        });
        let editing_seq = self.state.editing.as_ref().map(|(seq, _)| *seq);

        let list = self
            .state
            .visible_rows()
            .skip(self.scroll)
            .map(|row| match row {
                RowRef::User { text, seq, pending } => {
                    render_user_row(text, seq, pending, first_user_seq, editing_seq, &theme, cx)
                }
                other => render_row(&other, &theme),
            })
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
                // Edit-mode banner (§4.2): marks that a send will branch, with a
                // cancel affordance. Absent in the normal compose state.
                .when_some(
                    self.state.editing.as_ref().map(|(seq, _)| *seq),
                    |el, seq| {
                        el.child(
                            div()
                                .id("chat-editing")
                                .flex()
                                .items_center()
                                .justify_between()
                                .pb_1()
                                .child(
                                    div()
                                        .text_color(theme.text_tertiary)
                                        .child(format!("editing turn #{seq} — send branches")),
                                )
                                .child(
                                    div()
                                        .id("chat-edit-cancel")
                                        .cursor_pointer()
                                        .text_color(theme.text_disabled)
                                        .hover(|s| s.text_color(theme.text_secondary))
                                        .child("esc")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.cancel_edit(cx)),
                                        ),
                                ),
                        )
                    },
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().child(if connected {
                            self.state.input.clone()
                        } else {
                            "(disconnected)".to_owned()
                        }))
                        .child(self.send_button(&theme, connected, cx)),
                ),
        )
    }
}

/// The send affordance. In edit mode it carries a branch glyph (`⤦`) to
/// hint the send forks (without the word "fork"); in compose mode a plain
/// arrow. The accent marks it the screen's single primary action.
impl Chat {
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn send_button(
        &self,
        theme: &Theme,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let editing = self.state.editing.is_some();
        let glyph = if editing { "⤦" } else { "→" };
        let mut el = div()
            .id("chat-send")
            .text_color(if connected {
                theme.accent
            } else {
                theme.text_disabled
            })
            .child(glyph);
        if connected {
            el = el
                .cursor_pointer()
                .hover(|s| s.text_color(theme.accent_hover))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.send(&Send, window, cx);
                }));
        }
        el.into_any_element()
    }
}

/// Render a user row with its edit affordance. The entry is near-invisible
/// (`text_disabled`) until the row is hovered (`group_hover` lifts it one
/// step), and clicking it enters edit mode — but never on the first settled
/// user turn, a pending row, or a failed row (§4.2). The row being edited is
/// accent-tinted to show which turn is loaded in the input.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_pass_by_ref_mut)]
fn render_user_row(
    text: &str,
    seq: Option<u64>,
    pending: PendingState,
    first_user_seq: Option<u64>,
    editing_seq: Option<u64>,
    theme: &Theme,
    cx: &mut Context<Chat>,
) -> gpui::AnyElement {
    let marker = match pending {
        PendingState::Settled => ">",
        PendingState::Pending => "…",
        PendingState::Failed => "✗",
    };
    let is_editing = editing_seq.is_some() && seq == editing_seq;
    let color = if is_editing {
        theme.accent_ink
    } else {
        match pending {
            PendingState::Failed => theme.state_error,
            _ => theme.user_text,
        }
    };
    // Editable only when settled and not the first user turn.
    let editable = pending == PendingState::Settled && seq.is_some() && seq != first_user_seq;

    let mut row = div()
        .id(("user-row", seq.unwrap_or(0)))
        .group("user-turn")
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .text_color(color)
        .child(format!("{marker} {text}"));

    if editable {
        let seq = seq.unwrap_or(0);
        row = row.child(
            div()
                .id(("user-edit", seq))
                .cursor_pointer()
                .text_color(theme.text_disabled)
                .opacity(0.0)
                .group_hover("user-turn", |s| {
                    s.opacity(1.0).text_color(theme.text_secondary)
                })
                .child("✎")
                .on_click(cx.listener(move |this, _, _, cx| this.begin_edit(seq, cx))),
        );
    }
    row.into_any_element()
}

fn render_row(row: &RowRef<'_>, theme: &Theme) -> gpui::AnyElement {
    match row {
        // User rows are rendered inline by `Chat::render` (they need `cx` for
        // the edit affordance); this arm is unreachable.
        RowRef::User { .. } => div().into_any_element(),
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

    // ---- Edit-as-fork (state fold) ----

    fn two_user_turns_state() -> ChatState {
        let mut state = ChatState::default();
        state.open(
            SessionId("s".into()),
            &view_with(
                vec![
                    ViewItem::User {
                        id: 1,
                        text: "first".into(),
                        seq: 1,
                    },
                    ViewItem::Text {
                        id: 2,
                        text: "reply".into(),
                    },
                    ViewItem::User {
                        id: 3,
                        text: "second".into(),
                        seq: 5,
                    },
                ],
                false,
            ),
        );
        state
    }

    /// Entering edit mode loads the committed turn's text into the input and
    /// records the fork point; the first user turn is not editable.
    #[test]
    fn begin_edit_loads_turn_and_skips_first() {
        let mut state = two_user_turns_state();
        // The first user turn (seq 1) is excluded.
        state.begin_edit(1);
        assert!(state.editing.is_none());
        assert!(state.input.is_empty());
        // A later turn (seq 5) enters edit mode with its text loaded.
        state.begin_edit(5);
        assert_eq!(state.editing, Some((5, "second".to_owned())));
        assert_eq!(state.input, "second");
    }

    /// Cancelling an edit clears the input and leaves the committed row
    /// untouched (the log is immutable).
    #[test]
    fn cancel_edit_restores_without_touching_rows() {
        let mut state = two_user_turns_state();
        state.begin_edit(5);
        state.input = "edited draft".into();
        state.cancel_edit();
        assert!(state.editing.is_none());
        assert!(state.input.is_empty());
        // The committed row still reads "second".
        assert!(
            state
                .rows
                .iter()
                .any(|r| matches!(r, Row::User { text, seq: Some(5), .. } if text == "second"))
        );
    }

    /// An editing send resolves to `EditFork` at the edited turn's seq, carrying
    /// the (edited) text — always, even if the text is unchanged (no diff).
    /// A normal send resolves to Compose.
    #[test]
    fn resolve_send_editing_always_forks_else_composes() {
        let mut state = two_user_turns_state();
        // Normal compose.
        state.input = "fresh message".into();
        assert_eq!(
            state.resolve_send(),
            SendAction::Compose {
                session: SessionId("s".into()),
                text: "fresh message".into()
            }
        );
        // Editing send — even with UNCHANGED text — forks at seq 5.
        state.begin_edit(5);
        assert_eq!(state.input, "second");
        assert_eq!(
            state.resolve_send(),
            SendAction::EditFork {
                session: SessionId("s".into()),
                fork_seq: 5,
                text: "second".into()
            }
        );
        assert!(state.editing.is_none(), "send clears edit mode");
        // An empty send is a no-op and restores the draft.
        state.input = "   ".into();
        assert_eq!(state.resolve_send(), SendAction::Noop);
        assert_eq!(state.input, "   ");
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
