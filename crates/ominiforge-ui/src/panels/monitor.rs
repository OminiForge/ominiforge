//! Monitor panel: the usage / trace surface (doc/gpui-app.md §3.3).
//!
//! Shows one session's folded usage statistics (turns, model requests, tool
//! calls, token totals, cache-hit rate, context occupancy, top tools) plus a
//! per-turn trace rebuilt from the event stream (doc/monitor.md §5: `turn_id`
//! groups, `seq` orders, start/stop pairs give durations — no span concept).
//!
//! Split into a transport-independent [`MonitorState`] (a pure fold of the
//! [`SessionSummary`] snapshot + session events into render rows — unit-testable
//! without a UI) and the [`Monitor`] view (gpui: async protocol plumbing +
//! layout). The view is a thin shell; every interesting rule lives in the
//! state.
//!
//! Two fold regimes share one event stream but stay strictly separate:
//!
//! - **Usage** is seeded from the persisted [`SessionSummary`] and advanced
//!   ONLY by live (post-`ReplayEnd`) events — folding replayed history would
//!   double-count what the snapshot already accounts for.
//! - **Trace** folds ALL events regardless of the replay boundary, so an
//!   already-finished session still shows its history; the boundary matters
//!   only for the usage tallies.
//!
//! Cost display is deliberately absent: cost estimation was removed from the
//! core summary (doc/monitor.md §6) — token `usage` is the persisted fact.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use gpui::{Context, Render, Styled, div, prelude::*, px};
use ominiforge::core::payload::{ErrorEvent, EventPayload, ModelEvent, ToolEvent, TurnEvent};
use ominiforge::core::{CoreEvent, SessionId, TurnId};
use ominiforge::gateway::GatewayEvent;
use ominiforge::monitor::SessionSummary;
use ominiforge_net::ClientProtocol;

use crate::theme::Theme;

/// Element id of the panel root, used by tests via `debug_bounds`.
pub const MONITOR_PANEL_ID: &str = "monitor-panel";

/// One renderable trace entry within a turn's waterfall — a model request or
/// a tool call with its outcome and duration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEntry {
    /// A model request. `duration_ms`/`usage` fill in on completion; `None`
    /// while the request is still in flight.
    Request {
        /// The model id (e.g. `gpt-4o`).
        model: String,
        /// Wall-clock duration, `None` until `RequestCompleted`.
        duration_ms: Option<u64>,
        /// `(input, output)` tokens, `None` until `RequestCompleted`.
        usage: Option<(u32, u32)>,
        /// `true` if the request failed.
        failed: bool,
    },
    /// A tool call. `duration_ms` fills in when the call settles.
    Tool {
        /// The tool name.
        name: String,
        /// Wall-clock duration, `None` while running.
        duration_ms: Option<u64>,
        /// `true` if the call failed.
        failed: bool,
    },
}

/// One turn's trace: the chronological entries reconstructed from the events
/// carrying its `turn_id` (doc/monitor.md §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceTurn {
    /// The turn identifier.
    pub turn_id: TurnId,
    /// The opening user input, if the turn carried one.
    pub input: Option<String>,
    /// Entries in `seq` order.
    pub entries: Vec<TraceEntry>,
}

/// Transport-independent monitor state: the summary + trace fold.
///
/// Pure data + pure methods — no gpui, no async — so the fold rules (live
/// usage accounting, turn grouping, start/stop pairing) are unit-testable
/// without a window.
#[derive(Debug, Default)]
pub struct MonitorState {
    /// The session this state mirrors (set once opened).
    pub session: Option<SessionId>,
    /// The folded usage statistics. Seeded from the persisted summary, then
    /// advanced by live events only (the replay half of the stream is already
    /// accounted for in the snapshot — folding it would double-count).
    pub summary: SessionSummary,
    /// Whether the subscription passed the replay boundary (live events now
    /// advance the usage tallies). The trace folds regardless of this flag.
    replay_done: bool,
    /// Per-turn traces, in first-seen order (turns are sequential).
    turns: Vec<TraceTurn>,
    /// `turn_id` → index into `turns` (avoid a linear scan per event).
    turn_index: HashMap<String, usize>,
    /// In-flight model requests: `request_id` → `(turns_idx, entries_idx)`.
    /// Indices are stable — turns and entries are only ever appended.
    open_requests: HashMap<String, (usize, usize)>,
    /// In-flight tool calls: tool-call seq → `(turns_idx, entries_idx)`.
    open_tools: HashMap<u64, (usize, usize)>,
    /// A subscription/transport problem to surface (fail loud).
    pub notice: Option<String>,
}

impl MonitorState {
    /// Seed the state for a session: the persisted summary snapshot.
    pub fn open(&mut self, session: SessionId, summary: SessionSummary) {
        self.session = Some(session);
        self.summary = summary;
    }

    /// Fold one protocol event. The trace folds every event (so history shows
    /// for an already-finished session); the usage tallies advance only on
    /// live (post-`ReplayEnd`) events, which the persisted snapshot does not
    /// yet account for.
    pub fn apply(&mut self, event: &GatewayEvent) {
        match event {
            GatewayEvent::ReplayEnd => self.replay_done = true,
            GatewayEvent::Event { event } => self.apply_committed(event),
            _ => {}
        }
    }

    /// Fold one committed event into the trace (always) and the usage tallies
    /// (live events only). Split into per-domain handlers to keep each rule
    /// small. `turn_id` gates trace attribution: `Some` for turn-scoped
    /// events (the norm), `None` for session-scoped ones.
    fn apply_committed(&mut self, event: &CoreEvent) {
        let turn_id = event.turn_id.as_ref();
        match &event.payload {
            EventPayload::Turn(TurnEvent::Started { input, .. }) => {
                if self.replay_done {
                    self.summary.total_turns = self.summary.total_turns.saturating_add(1);
                }
                if let Some(id) = turn_id {
                    let idx = self.ensure_turn(id);
                    if let Some(text) = input
                        && !text.trim().is_empty()
                    {
                        self.turns[idx].input = Some(text.clone());
                    }
                }
            }
            EventPayload::Model(payload) => self.fold_model(payload, turn_id),
            EventPayload::Tool(payload) => self.fold_tool(payload, turn_id),
            EventPayload::Error(ErrorEvent::Raised(detail)) => {
                if self.replay_done {
                    *self.summary.errors.entry(detail.code.clone()).or_insert(0) += 1;
                }
            }
            EventPayload::Session(_)
            | EventPayload::Artifact(_)
            | EventPayload::Injection(_)
            | EventPayload::Hook(_)
            | EventPayload::Permission(_)
            // Turn lifecycle events beyond Started carry no usage/trace info.
            | EventPayload::Turn(_) => {}
        }
    }

    fn fold_model(&mut self, payload: &ModelEvent, turn_id: Option<&TurnId>) {
        match payload {
            ModelEvent::RequestStarted {
                request_id,
                model,
                input_tokens_estimate,
                ..
            } => {
                if self.replay_done {
                    // Last request sent the largest prefix (doc/monitor.md §8).
                    self.summary.context_tokens = *input_tokens_estimate;
                }
                if let Some(id) = turn_id {
                    let idx = self.ensure_turn(id);
                    self.turns[idx].entries.push(TraceEntry::Request {
                        model: model.clone(),
                        duration_ms: None,
                        usage: None,
                        failed: false,
                    });
                    self.open_requests
                        .insert(request_id.clone(), (idx, self.turns[idx].entries.len() - 1));
                }
            }
            ModelEvent::RequestCompleted {
                request_id,
                usage,
                duration_ms,
                ..
            } => {
                if self.replay_done {
                    self.summary.total_model_requests =
                        self.summary.total_model_requests.saturating_add(1);
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
                    self.recompute_cache_hit_rate();
                }
                if let Some((t, s)) = self.open_requests.remove(request_id) {
                    self.with_entry(t, s, |e| {
                        if let TraceEntry::Request {
                            duration_ms: d,
                            usage: u,
                            ..
                        } = e
                        {
                            *d = Some(*duration_ms);
                            *u = Some((usage.input_tokens, usage.output_tokens));
                        }
                    });
                }
            }
            ModelEvent::RequestFailed {
                request_id,
                duration_ms,
                error,
                ..
            } => {
                if self.replay_done {
                    *self.summary.errors.entry(error.code.clone()).or_insert(0) += 1;
                }
                if let Some((t, s)) = self.open_requests.remove(request_id) {
                    self.with_entry(t, s, |e| {
                        if let TraceEntry::Request {
                            duration_ms: d,
                            failed,
                            ..
                        } = e
                        {
                            *d = Some(*duration_ms);
                            *failed = true;
                        }
                    });
                }
            }
            // ContentBlock carries the model's content; the trace keys on the
            // request/response lifecycle instead.
            ModelEvent::ContentBlock { .. } => {}
        }
    }

    fn fold_tool(&mut self, payload: &ToolEvent, turn_id: Option<&TurnId>) {
        match payload {
            ToolEvent::Started {
                tool_call_event_id,
                tool_name,
                ..
            } => {
                if self.replay_done {
                    self.summary.total_tool_calls = self.summary.total_tool_calls.saturating_add(1);
                    *self
                        .summary
                        .tools_used
                        .entry(tool_name.clone())
                        .or_insert(0) += 1;
                }
                if let Some(id) = turn_id {
                    let idx = self.ensure_turn(id);
                    self.turns[idx].entries.push(TraceEntry::Tool {
                        name: tool_name.clone(),
                        duration_ms: None,
                        failed: false,
                    });
                    self.open_tools.insert(
                        tool_call_event_id.seq,
                        (idx, self.turns[idx].entries.len() - 1),
                    );
                }
            }
            ToolEvent::Completed {
                tool_call_event_id,
                duration_ms,
                ..
            } => {
                if let Some((t, s)) = self.open_tools.remove(&tool_call_event_id.seq) {
                    self.with_entry(t, s, |e| {
                        if let TraceEntry::Tool { duration_ms: d, .. } = e {
                            *d = Some(*duration_ms);
                        }
                    });
                }
            }
            ToolEvent::Failed {
                tool_call_event_id,
                duration_ms,
                error,
            } => {
                if self.replay_done {
                    self.summary.total_tool_failures =
                        self.summary.total_tool_failures.saturating_add(1);
                    *self.summary.errors.entry(error.code.clone()).or_insert(0) += 1;
                }
                if let Some((t, s)) = self.open_tools.remove(&tool_call_event_id.seq) {
                    self.with_entry(t, s, |e| {
                        if let TraceEntry::Tool {
                            duration_ms: d,
                            failed,
                            ..
                        } = e
                        {
                            *d = Some(*duration_ms);
                            *failed = true;
                        }
                    });
                }
            }
        }
    }

    /// The index of `turn_id`'s trace, creating the turn on first sight.
    /// Looks up by `&str` borrow (no per-event allocation); allocates the key
    /// only when inserting a brand-new turn.
    fn ensure_turn(&mut self, turn_id: &TurnId) -> usize {
        if let Some(&idx) = self.turn_index.get(turn_id.0.as_str()) {
            return idx;
        }
        let idx = self.turns.len();
        self.turns.push(TraceTurn {
            turn_id: turn_id.clone(),
            input: None,
            entries: Vec::new(),
        });
        self.turn_index.insert(turn_id.0.clone(), idx);
        idx
    }

    /// Mutate one trace entry by `(turns_idx, entries_idx)`, if still in range.
    fn with_entry(&mut self, turn: usize, slot: usize, f: impl FnOnce(&mut TraceEntry)) {
        if let Some(entry) = self
            .turns
            .get_mut(turn)
            .and_then(|t| t.entries.get_mut(slot))
        {
            f(entry);
        }
    }

    /// Recompute the derived ratio after live usage lands (mirrors
    /// `Monitor::summary`'s rule: 0.0 with no input tokens).
    fn recompute_cache_hit_rate(&mut self) {
        self.summary.cache_hit_rate = if self.summary.total_input_tokens == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                self.summary.total_cache_read_tokens as f64 / self.summary.total_input_tokens as f64
            }
        };
    }

    /// The folded trace turns to render, oldest first.
    #[must_use]
    pub fn trace(&self) -> &[TraceTurn] {
        &self.turns
    }

    /// Top tools by call count, capped at `cap`, with each bar's width as a
    /// percentage of this set's max (mirrors the web client's `topTools`).
    #[must_use]
    pub fn top_tools(&self, cap: usize) -> Vec<(String, u64, f64)> {
        let mut entries: Vec<(String, u64)> = self
            .summary
            .tools_used
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(cap);
        #[allow(clippy::cast_precision_loss)]
        let max = entries.iter().map(|e| e.1).max().unwrap_or(1).max(1) as f64;
        entries
            .into_iter()
            .map(|(tool, count)| {
                #[allow(clippy::cast_precision_loss)]
                (tool, count, count as f64 / max * 100.0)
            })
            .collect()
    }
}

/// The monitor panel view: protocol plumbing + layout over [`MonitorState`].
pub struct Monitor {
    client: Arc<dyn ClientProtocol>,
    state: MonitorState,
}

impl Monitor {
    /// Create an empty monitor panel (no session selected yet).
    pub fn new(client: Arc<dyn ClientProtocol>) -> Self {
        Self {
            client,
            state: MonitorState::default(),
        }
    }

    /// The current state (for tests and the workspace).
    #[must_use]
    pub const fn state(&self) -> &MonitorState {
        &self.state
    }

    /// Open a session: fetch its summary snapshot, then subscribe to its
    /// event stream so the panel folds both history (trace) and live updates
    /// (usage + trace). Failures surface in `state.notice` (fail loud).
    pub fn open(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.state = MonitorState::default();
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            match client.session_summary(&session).await {
                Ok(summary) => {
                    let _ = this.update(cx, |panel, cx| {
                        panel.state.open(session.clone(), summary);
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |panel, cx| {
                        panel.state.notice = Some(format!("failed to load summary: {e:#}"));
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
                            .update(cx, |panel, cx| {
                                panel.state.apply(&event);
                                cx.notify();
                            })
                            .is_err()
                        {
                            // Panel dropped: stop the fold quietly.
                            return;
                        }
                    }
                    let _ = this.update(cx, |panel, cx| {
                        panel.state.notice = Some("connection closed".to_owned());
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |panel, cx| {
                        panel.state.notice = Some(format!("subscription failed: {e:#}"));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }
}

impl Render for Monitor {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();

        let mut root = div()
            .id(MONITOR_PANEL_ID)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.canvas_raised)
            .text_color(theme.text_primary);

        if let Some(notice) = &self.state.notice {
            root = root.child(
                div()
                    .px_2()
                    .py_1()
                    .bg(theme.canvas_overlay)
                    .text_color(theme.state_error)
                    .child(notice.clone()),
            );
        }

        if self.state.session.is_none() {
            return root.child(
                div()
                    .p_2()
                    .text_color(theme.text_tertiary)
                    .child("No session selected"),
            );
        }

        root.child(self.render_stats(&theme))
            .child(self.render_tools(&theme))
            .child(self.render_trace(&theme))
    }
}

impl Monitor {
    /// The usage-statistics cards: turns, requests, tool calls, token totals,
    /// cache-hit rate, context occupancy.
    fn render_stats(&self, theme: &Theme) -> gpui::AnyElement {
        let summary = &self.state.summary;
        let stats = [
            ("turns", summary.total_turns.to_string()),
            ("requests", summary.total_model_requests.to_string()),
            (
                "tool calls",
                format!(
                    "{}{}",
                    summary.total_tool_calls,
                    if summary.total_tool_failures > 0 {
                        format!(" ({} failed)", summary.total_tool_failures)
                    } else {
                        String::new()
                    }
                ),
            ),
            ("in tok", summary.total_input_tokens.to_string()),
            ("out tok", summary.total_output_tokens.to_string()),
            ("cache", format!("{:.2}%", summary.cache_hit_rate * 100.0)),
            ("context", summary.context_tokens.to_string()),
        ];
        let mut row = div().flex().flex_row().flex_wrap().gap_2().p_2();
        for (label, value) in stats {
            row = row.child(
                div()
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .bg(theme.canvas_overlay)
                    .child(
                        div()
                            .text_color(theme.text_tertiary)
                            .child(label.to_owned()),
                    )
                    .child(div().child(value)),
            );
        }
        row.into_any_element()
    }

    /// The top-tools breakdown: a labelled bar per tool, scaled to the set's
    /// own max. Empty (no tool calls yet) renders nothing.
    #[allow(clippy::cast_possible_truncation)]
    fn render_tools(&self, theme: &Theme) -> gpui::AnyElement {
        let tools = self.state.top_tools(5);
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border_subtle);
        for (tool, count, pct) in tools {
            section = section.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().w(px(96.0)).child(tool))
                    .child(div().h(px(6.0)).w(px(pct as f32)).bg(theme.accent_dim))
                    .child(
                        div()
                            .text_color(theme.text_tertiary)
                            .child(count.to_string()),
                    ),
            );
        }
        section.into_any_element()
    }

    /// The per-turn trace waterfall: each turn's opening input as its header,
    /// then its request/tool entries with status-colour redundancy.
    fn render_trace(&self, theme: &Theme) -> gpui::AnyElement {
        let mut col = div()
            .id("monitor-trace")
            .flex_1()
            .flex_col()
            .overflow_hidden()
            .border_t_1()
            .border_color(theme.border_subtle)
            .p_2();
        for turn in self.state.trace() {
            let mut block = div().flex().flex_col().py_1();
            if let Some(input) = &turn.input {
                block = block.child(div().text_color(theme.user_text).child(input.clone()));
            }
            for entry in &turn.entries {
                let (label, detail, color) = render_entry(entry, theme);
                block = block.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .pl_2()
                        .child(div().text_color(color).child(label))
                        .child(div().text_color(theme.text_tertiary).child(detail)),
                );
            }
            col = col.child(block);
        }
        col.into_any_element()
    }
}

/// The (label, detail, color) projection of one trace entry. The status
/// color carries the redundancy (error/running/done), the label the kind.
fn render_entry(entry: &TraceEntry, theme: &Theme) -> (String, String, gpui::Hsla) {
    let (label, mut detail, duration_ms, failed) = match entry {
        TraceEntry::Request {
            model,
            duration_ms,
            usage,
            failed,
        } => {
            let mut d = model.clone();
            if let Some((i, o)) = usage {
                let _ = write!(d, " · {i}→{o} tok");
            }
            ("req", d, *duration_ms, *failed)
        }
        TraceEntry::Tool {
            name,
            duration_ms,
            failed,
        } => ("tool", name.clone(), *duration_ms, *failed),
    };
    if let Some(d) = duration_ms {
        let _ = write!(detail, " · {d}ms");
    }
    let color = if failed {
        theme.state_error
    } else if duration_ms.is_none() {
        theme.state_running
    } else {
        theme.state_done
    };
    (label.to_owned(), detail, color)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use ominiforge::core::payload::{
        ErrorDetail, ErrorSeverity, StopReason, ToolOutput, ToolSource, Usage,
    };
    use ominiforge::core::{EventId, EventSource, SCHEMA_VERSION, SourceKind};

    fn sid() -> SessionId {
        SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned())
    }

    fn ev(seq: u64, turn: Option<&str>, payload: EventPayload) -> CoreEvent {
        CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            session_id: sid(),
            timestamp: chrono::Utc::now(),
            source: EventSource {
                kind: SourceKind::Runtime,
                id: "ominiforge".to_owned(),
            },
            parent_event_id: None,
            turn_id: turn.map(|t| TurnId(t.to_owned())),
            payload,
        }
    }

    /// A live event on turn "t1" (the common case for most assertions).
    fn live(payload: EventPayload) -> GatewayEvent {
        GatewayEvent::Event {
            event: Box::new(ev(0, Some("t1"), payload)),
        }
    }

    fn usage(i: u32, o: u32, cache: u32) -> Usage {
        Usage {
            input_tokens: i,
            output_tokens: o,
            cache_read_tokens: cache,
            cache_write_tokens: 0,
        }
    }

    fn turn_started(input: &str) -> EventPayload {
        EventPayload::Turn(TurnEvent::Started {
            turn_id: TurnId("t1".to_owned()),
            input: Some(input.to_owned()),
        })
    }

    fn request_started(id: &str, model: &str, estimate: u32) -> EventPayload {
        EventPayload::Model(ModelEvent::RequestStarted {
            request_id: id.to_owned(),
            provider: "test".to_owned(),
            model: model.to_owned(),
            temperature: 0.0,
            max_tokens: None,
            tool_schemas_count: 0,
            input_tokens_estimate: estimate,
        })
    }

    fn request_completed(id: &str, usage: Usage, ms: u64) -> EventPayload {
        EventPayload::Model(ModelEvent::RequestCompleted {
            request_id: id.to_owned(),
            stop_reason: StopReason::EndTurn,
            usage,
            duration_ms: ms,
            time_to_first_token_ms: None,
            provider_request_id: None,
        })
    }

    fn tool_started(call_seq: u64, name: &str) -> EventPayload {
        EventPayload::Tool(ToolEvent::Started {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: call_seq,
            },
            tool_name: name.to_owned(),
            source: ToolSource::Builtin,
            input: serde_json::Value::Null,
            working_dir: None,
        })
    }

    fn tool_failed(call_seq: u64, ms: u64, code: &str) -> EventPayload {
        EventPayload::Tool(ToolEvent::Failed {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: call_seq,
            },
            duration_ms: ms,
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

    /// Replayed (pre-ReplayEnd) events build the trace so history shows for an
    /// already-finished session, but they must NOT advance the usage tallies —
    /// the persisted summary snapshot already accounts for them, and folding
    /// them would double-count. This is the usage/trace separation.
    #[test]
    fn replay_builds_trace_but_not_usage() {
        let mut state = MonitorState::default();
        state.open(sid(), SessionSummary::default());
        // Replay segment (no ReplayEnd yet).
        state.apply(&live(turn_started("hi")));
        state.apply(&live(request_completed("r1", usage(1000, 200, 250), 42)));
        assert_eq!(state.summary.total_turns, 0);
        assert_eq!(state.summary.total_model_requests, 0);
        assert_eq!(state.summary.total_input_tokens, 0);
        // …but the trace already has the turn + entry.
        assert_eq!(state.trace().len(), 1);
        assert_eq!(state.trace()[0].input.as_deref(), Some("hi"));
        // After the boundary, live events advance usage.
        state.apply(&GatewayEvent::ReplayEnd);
        state.apply(&live(turn_started("again")));
        assert_eq!(state.summary.total_turns, 1);
        assert_eq!(state.trace().len(), 1); // same turn_id folds together
    }

    /// A live request pair updates both the usage tallies (in/out/cache +
    /// derived hit rate + context) and the trace entry's duration/usage — the
    /// panel shows fresh numbers mid-session without re-fetching the summary.
    #[test]
    fn live_request_pair_folds_usage_and_trace() {
        let mut state = MonitorState::default();
        state.open(sid(), SessionSummary::default());
        state.apply(&GatewayEvent::ReplayEnd);
        state.apply(&live(request_started("r1", "gpt-4o", 1500)));
        state.apply(&live(request_completed("r1", usage(1000, 200, 250), 42)));

        assert_eq!(state.summary.total_model_requests, 1);
        assert_eq!(state.summary.total_input_tokens, 1000);
        assert_eq!(state.summary.total_output_tokens, 200);
        assert_eq!(state.summary.total_cache_read_tokens, 250);
        assert!((state.summary.cache_hit_rate - 0.25).abs() < f64::EPSILON);
        assert_eq!(state.summary.context_tokens, 1500);

        assert_eq!(
            state.trace()[0].entries[0],
            TraceEntry::Request {
                model: "gpt-4o".to_owned(),
                duration_ms: Some(42),
                usage: Some((1000, 200)),
                failed: false,
            }
        );
    }

    /// A tool start/complete pair settles the trace entry's duration via the
    /// `tool_call_event_id` pairing (not position); a failure marks it failed
    /// and tallies under the error code.
    #[test]
    fn tool_pair_settles_by_call_id() {
        let mut state = MonitorState::default();
        state.open(sid(), SessionSummary::default());
        state.apply(&GatewayEvent::ReplayEnd);
        state.apply(&live(tool_started(7, "shell")));
        state.apply(&live(tool_failed(7, 9, "execution_failed")));

        assert_eq!(state.summary.total_tool_calls, 1);
        assert_eq!(state.summary.total_tool_failures, 1);
        assert_eq!(*state.summary.tools_used.get("shell").unwrap(), 1);
        assert_eq!(*state.summary.errors.get("execution_failed").unwrap(), 1);
        assert_eq!(
            state.trace()[0].entries[0],
            TraceEntry::Tool {
                name: "shell".to_owned(),
                duration_ms: Some(9),
                failed: true,
            }
        );
    }

    /// Events from two distinct turns group under their own `turn_id` in
    /// first-seen order (doc/monitor.md §5) — one flat list would lose the
    /// per-turn waterfall.
    #[test]
    fn events_group_by_turn() {
        let mut state = MonitorState::default();
        state.open(sid(), SessionSummary::default());
        state.apply(&GatewayEvent::ReplayEnd);
        let started = |turn: &str, input: &str| GatewayEvent::Event {
            event: Box::new(ev(
                0,
                Some(turn),
                EventPayload::Turn(TurnEvent::Started {
                    turn_id: TurnId(turn.to_owned()),
                    input: Some(input.to_owned()),
                }),
            )),
        };
        state.apply(&started("t1", "first"));
        state.apply(&started("t2", "second"));
        let trace = state.trace();
        assert_eq!(trace.len(), 2);
        assert_eq!(trace[0].turn_id.0, "t1");
        assert_eq!(trace[0].input.as_deref(), Some("first"));
        assert_eq!(trace[1].turn_id.0, "t2");
    }

    /// The seed summary is the base the live fold continues from — opening a
    /// session shows its persisted totals immediately, and live events add on
    /// top rather than restarting from zero.
    #[test]
    fn live_fold_continues_from_seeded_summary() {
        let mut state = MonitorState::default();
        let seed = SessionSummary {
            total_turns: 3,
            total_input_tokens: 500,
            ..Default::default()
        };
        state.open(sid(), seed);
        state.apply(&GatewayEvent::ReplayEnd);
        state.apply(&live(EventPayload::Turn(TurnEvent::Started {
            turn_id: TurnId("t4".to_owned()),
            input: None,
        })));
        assert_eq!(state.summary.total_turns, 4);
        assert_eq!(state.summary.total_input_tokens, 500);
    }

    /// Events with no `turn_id` (session-scoped: Created/Ended, etc.) still
    /// count toward usage, but must NOT create a phantom "empty turn" in the
    /// trace — there is no turn to attribute them to. Collapsing them under a
    /// shared empty key would merge unrelated events into one bogus row.
    #[test]
    fn turnless_events_count_usage_but_skip_trace() {
        let mut state = MonitorState::default();
        state.open(sid(), SessionSummary::default());
        state.apply(&GatewayEvent::ReplayEnd);
        // A model request carrying no turn_id (defensive; protocol normally
        // scopes these to a turn).
        let no_turn = GatewayEvent::Event {
            event: Box::new(ev(0, None, request_started("r9", "gpt-4o", 10))),
        };
        state.apply(&no_turn);
        // Trace stays empty — no phantom turn created.
        assert!(state.trace().is_empty());
        assert!(state.turn_index.is_empty());
        // And no open_request leaked from a turn we never traced.
        assert!(state.open_requests.is_empty());
    }

    /// `top_tools` orders by count descending, caps the list, and scales bars
    /// to this set's own max (per-session breakdowns read on their own scale).
    #[test]
    fn top_tools_orders_and_scales() {
        let mut state = MonitorState::default();
        let mut seed = SessionSummary::default();
        seed.tools_used.insert("shell".to_owned(), 10);
        seed.tools_used.insert("read".to_owned(), 5);
        seed.tools_used.insert("write".to_owned(), 1);
        state.open(sid(), seed);
        let top = state.top_tools(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "shell");
        assert!((top[0].2 - 100.0).abs() < f64::EPSILON);
        assert_eq!(top[1].0, "read");
        assert!((top[1].2 - 50.0).abs() < f64::EPSILON);
    }

    /// A tool `Started` without its `Completed` stays in-flight (no duration)
    /// and renders as running — a crashed tool must not fake a zero-duration
    /// success.
    #[test]
    fn unsettled_tool_stays_running() {
        let mut state = MonitorState::default();
        state.open(sid(), SessionSummary::default());
        state.apply(&GatewayEvent::ReplayEnd);
        state.apply(&live(tool_started(3, "shell")));
        assert_eq!(
            state.trace()[0].entries[0],
            TraceEntry::Tool {
                name: "shell".to_owned(),
                duration_ms: None,
                failed: false,
            }
        );
    }

    /// The happy path: a started tool that completes settles OK (not failed),
    /// with its duration filled in.
    #[test]
    fn tool_completed_settles_ok() {
        let mut state = MonitorState::default();
        state.open(sid(), SessionSummary::default());
        state.apply(&GatewayEvent::ReplayEnd);
        state.apply(&live(tool_started(4, "read")));
        state.apply(&live(EventPayload::Tool(ToolEvent::Completed {
            tool_call_event_id: EventId {
                session_id: sid(),
                seq: 4,
            },
            result: ToolOutput {
                content: vec![],
                is_error: false,
                error_code: None,
            },
            duration_ms: 5,
            output_bytes: 0,
            artifacts_created: vec![],
        })));
        assert_eq!(
            state.trace()[0].entries[0],
            TraceEntry::Tool {
                name: "read".to_owned(),
                duration_ms: Some(5),
                failed: false,
            }
        );
        assert_eq!(state.summary.total_tool_failures, 0);
    }
}
