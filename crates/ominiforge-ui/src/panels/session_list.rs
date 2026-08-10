//! Session list panel: the session management surface (doc/gpui-app.md §3.3).
//!
//! Lists active sessions (title, activity status, origin badge) plus a
//! collapsible archived region, and drives the full lifecycle — create,
//! select, archive, delete — through the [`ClientProtocol`] trait. Split into
//! a transport-independent [`SessionListState`] (a pure fold of metas +
//! statuses + acknowledged seqs into render rows — unit-testable without a
//! UI) and the [`SessionList`] view (gpui: async protocol plumbing, pointer
//! affordances, layout). The view is a thin shell; every interesting rule
//! lives in the state.
//!
//! Per the Phase 3.5 decisions: no global keybindings are registered yet
//! (keymap semantics unfinalized); the seen/unseen acknowledged-seqs are
//! in-memory only (persisted storage lands with Phase 5 config).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use gpui::{Context, ElementId, EventEmitter, Render, Styled, div, prelude::*};
use ominiforge::core::SessionId;
use ominiforge::gateway::{ActivityStatus, SessionStatus};
use ominiforge::monitor::SessionSummary;
use ominiforge::session::{OriginKind, SessionMeta};
use ominiforge_net::ClientProtocol;

use crate::theme::Theme;

/// Element id of the panel root, used by tests via `debug_bounds`.
pub const SESSION_LIST_PANEL_ID: &str = "session-list-panel";

/// What a session row's activity icon renders: `running` / `awaiting` /
/// `unseen` / `seen`.
///
/// `unseen`/`seen` are the client-side refinement of the backend's `idle`:
/// the gateway cannot know what the user has *looked at*, only what the
/// session is *doing* (mirrors the web client's status layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    /// A turn is running.
    Running,
    /// Suspended awaiting a user decision (e.g. a gated tool call).
    Awaiting,
    /// Idle with new committed events past the acknowledged seq (unread).
    Unseen,
    /// Idle and fully seen.
    Seen,
}

/// One renderable session row — the folded projection of a session's meta,
/// its summary (title/sort key), and its live status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The session id.
    pub id: SessionId,
    /// Display title: first user input (clipped) or a shortened id.
    pub title: String,
    /// The origin badge (`fork` / `compacted` / `reconfigured`), if any.
    pub badge: Option<&'static str>,
    /// The folded activity status.
    pub status: RowStatus,
    /// Sort key: last user message, falling back to creation time.
    pub activity: DateTime<Utc>,
}

/// Transport-independent session-list state: the metas/statuses/ack fold.
///
/// Pure data + pure methods — no gpui, no async — so the fold rules (titling,
/// ordering, seen/unseen) are unit-testable without a window.
#[derive(Debug, Default)]
pub struct SessionListState {
    /// Active sessions' metadata, as loaded (unsorted).
    metas: Vec<SessionMeta>,
    /// Per-session summaries (title + sort key), keyed by id. Absent until
    /// that session's summary backfills.
    summaries: HashMap<String, SessionSummary>,
    /// Last-known activity status per session (last-write-wins by id).
    statuses: HashMap<String, ActivityStatus>,
    /// Latest committed seq per session (from the status stream).
    latest_seqs: HashMap<String, u64>,
    /// Acknowledged (seen-up-to) seq per session. In-memory only for 3.5.
    acked: HashMap<String, u64>,
    /// The currently-open session (drives the active highlight).
    pub active: Option<SessionId>,
    /// A lifecycle/listing problem to surface (fail loud).
    pub notice: Option<String>,
}

impl SessionListState {
    /// Seed the active session list from metadata (rows render immediately,
    /// before any summary backfills).
    pub fn load(&mut self, metas: Vec<SessionMeta>) {
        self.metas = metas;
    }

    /// Fold one session's summary (title + sort key) into the list.
    pub fn apply_summary(&mut self, summary: SessionSummary, fallback_id: &SessionId) {
        // `SessionSummary` carries no id; the caller pairs it with the id it
        // requested. Keyed by the requested id.
        self.summaries.insert(fallback_id.0.clone(), summary);
    }

    /// Fold one status delta (snapshot entry or live): last-write-wins by id,
    /// and track the session's latest committed seq for the unseen split.
    pub fn apply_status(&mut self, status: &SessionStatus) {
        self.statuses
            .insert(status.session_id.0.clone(), status.status);
        let entry = self
            .latest_seqs
            .entry(status.session_id.0.clone())
            .or_insert(0);
        if status.latest_seq > *entry {
            *entry = status.latest_seq;
        }
    }

    /// Mark a session seen up to `seq` (called when the user opens it).
    /// Monotonic — a smaller seq never lowers the watermark.
    pub fn mark_seen(&mut self, id: &SessionId) {
        let seq = self.latest_seqs.get(&id.0).copied().unwrap_or(0);
        let cur = self.acked.entry(id.0.clone()).or_insert(0);
        if seq > *cur {
            *cur = seq;
        }
    }

    /// Remove a session from the list (after archive/delete succeeds).
    pub fn remove(&mut self, id: &SessionId) {
        self.metas.retain(|m| &m.id != id);
    }

    /// The folded activity status for one session.
    fn row_status(&self, id: &SessionId) -> RowStatus {
        match self.statuses.get(&id.0) {
            Some(ActivityStatus::Running) => RowStatus::Running,
            Some(ActivityStatus::AwaitingInput) => RowStatus::Awaiting,
            _ => {
                let latest = self.latest_seqs.get(&id.0).copied().unwrap_or(0);
                let acked = self.acked.get(&id.0).copied().unwrap_or(0);
                if latest > acked {
                    RowStatus::Unseen
                } else {
                    RowStatus::Seen
                }
            }
        }
    }

    /// The folded, sorted active rows (most-recently-active first).
    #[must_use]
    pub fn rows(&self) -> Vec<SessionRow> {
        let mut rows: Vec<SessionRow> = self.metas.iter().map(|m| self.fold_row(m)).collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.activity));
        rows
    }

    fn fold_row(&self, meta: &SessionMeta) -> SessionRow {
        let summary = self.summaries.get(&meta.id.0);
        let title = summary
            .and_then(|s| s.first_user_input.as_deref())
            .map_or_else(|| short_id(&meta.id.0), clip_title);
        let badge = match meta.origin.kind {
            OriginKind::Fork => Some("fork"),
            OriginKind::Compaction => Some("compacted"),
            OriginKind::Reconfiguration => Some("reconfigured"),
            OriginKind::New => None,
        };
        let activity = summary
            .and_then(|s| s.last_user_message_at)
            .unwrap_or(meta.created_at);
        SessionRow {
            id: meta.id.clone(),
            title,
            badge,
            status: self.row_status(&meta.id),
            activity,
        }
    }
}

/// Clip a title to its first line, truncated to `n` chars (display-only; the
/// backend stores the full input).
fn clip_title(s: &str) -> String {
    const MAX: usize = 60;
    let line = s.lines().next().unwrap_or(s).trim();
    if line.chars().count() > MAX {
        format!("{}…", line.chars().take(MAX).collect::<String>())
    } else {
        line.to_owned()
    }
}

/// Shorten a session id for display when there is no title yet.
fn short_id(id: &str) -> String {
    if id.len() > 14 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_owned()
    }
}

/// Emitted when the user selects (opens) a session.
pub struct SessionChosen {
    /// The session to open.
    pub session_id: SessionId,
}

/// The session list view: protocol plumbing + pointer affordances over
/// [`SessionListState`].
pub struct SessionList {
    client: Arc<dyn ClientProtocol>,
    state: SessionListState,
    /// The archived region's folded rows + whether it is expanded.
    archived: Vec<SessionMeta>,
    archived_open: bool,
    /// The session pending a two-step destructive confirm (archive or delete).
    confirming: Option<(SessionId, ConfirmKind)>,
}

/// Which destructive action a two-step confirm gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmKind {
    Archive,
    Delete,
}

impl SessionList {
    /// Create a session list attached to `client`. Loads the active list and
    /// subscribes to the gateway-wide status stream.
    pub fn new(client: Arc<dyn ClientProtocol>, cx: &mut Context<Self>) -> Self {
        let list = Self {
            client,
            state: SessionListState::default(),
            archived: Vec::new(),
            archived_open: false,
            confirming: None,
        };
        list.refresh(cx);
        list.subscribe_status(cx);
        list
    }

    /// Reload the active session list and backfill summaries.
    pub fn refresh(&self, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            match client.list_sessions().await {
                Ok(metas) => {
                    let _ = this.update(cx, |list, cx| {
                        list.state.load(metas.clone());
                        cx.notify();
                    });
                    // Backfill summaries sequentially (titles + sort keys).
                    for meta in metas {
                        if let Ok(summary) = client.session_summary(&meta.id).await {
                            let updated = this.update(cx, |list, cx| {
                                list.state.apply_summary(summary, &meta.id);
                                cx.notify();
                            });
                            if updated.is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = this.update(cx, |list, cx| {
                        list.state.notice = Some(format!("failed to list sessions: {e:#}"));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Subscribe to the gateway-wide status stream (snapshot then live).
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn subscribe_status(&self, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            use futures_lite::StreamExt as _;
            if let Ok(mut stream) = client.subscribe_status().await {
                while let Some(status) = stream.next().await {
                    if this
                        .update(cx, |list, cx| {
                            list.state.apply_status(&status);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        })
        .detach();
    }

    /// The current state (for tests and the workspace).
    #[must_use]
    pub const fn state(&self) -> &SessionListState {
        &self.state
    }

    /// Create a new session and emit it for the workspace to open.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn create(&mut self, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| match client.create_session().await {
            Ok(id) => {
                let _ = this.update(cx, |list, cx| {
                    list.state.active = Some(id.clone());
                    cx.emit(SessionChosen { session_id: id });
                    list.refresh(cx);
                });
            }
            Err(e) => {
                let _ = this.update(cx, |list, cx| {
                    list.state.notice = Some(format!("create failed: {e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Select a session: mark it seen, set active, emit for the workspace.
    fn select(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.state.mark_seen(&id);
        self.state.active = Some(id.clone());
        cx.emit(SessionChosen { session_id: id });
        cx.notify();
    }

    /// First step of a destructive action: arm the confirm. Second click on
    /// the armed row confirms.
    fn arm_confirm(&mut self, id: SessionId, kind: ConfirmKind, cx: &mut Context<Self>) {
        if self
            .confirming
            .as_ref()
            .is_some_and(|(i, k)| *i == id && *k == kind)
        {
            self.confirming = None;
            match kind {
                ConfirmKind::Archive => self.archive(id, cx),
                ConfirmKind::Delete => self.delete(id, cx),
            }
        } else {
            self.confirming = Some((id, kind));
            cx.notify();
        }
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn archive(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(
            async move |this, cx| match client.archive_session(&id).await {
                Ok(()) => {
                    let _ = this.update(cx, |list, cx| {
                        list.state.remove(&id);
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |list, cx| {
                        list.state.notice = Some(format!("archive failed: {e:#}"));
                        cx.notify();
                    });
                }
            },
        )
        .detach();
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn delete(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(
            async move |this, cx| match client.delete_session(&id).await {
                Ok(()) => {
                    let _ = this.update(cx, |list, cx| {
                        list.archived.retain(|m| m.id != id);
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |list, cx| {
                        list.state.notice = Some(format!("delete failed: {e:#}"));
                        cx.notify();
                    });
                }
            },
        )
        .detach();
    }

    /// Toggle the archived region; loads it on first expand.
    fn toggle_archived(&mut self, cx: &mut Context<Self>) {
        self.archived_open = !self.archived_open;
        if self.archived_open {
            let client = Arc::clone(&self.client);
            cx.spawn(async move |this, cx| {
                if let Ok(metas) = client.list_archived_sessions().await {
                    let _ = this.update(cx, |list, cx| {
                        list.archived = metas;
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        cx.notify();
    }
}

impl EventEmitter<SessionChosen> for SessionList {}

impl Render for SessionList {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();

        let mut root = div()
            .id(SESSION_LIST_PANEL_ID)
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

        // New-session affordance (the screen's single primary action).
        root = root.child(
            div()
                .id("session-new")
                .cursor_pointer()
                .m_2()
                .px_2()
                .py_1()
                .bg(theme.accent)
                .text_color(theme.canvas_base)
                .child("+ New session")
                .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
        );

        // Active session rows.
        let active = self.state.active.clone();
        let confirming = self.confirming.clone();
        let rows = self
            .state
            .rows()
            .into_iter()
            .map(|row| {
                let is_active = active.as_ref() == Some(&row.id);
                let confirming_archive = confirming
                    .as_ref()
                    .is_some_and(|(i, k)| *i == row.id && *k == ConfirmKind::Archive);
                render_session_row(&row, is_active, confirming_archive, &theme, cx)
            })
            .collect::<Vec<_>>();

        root.child(
            div()
                .id("session-rows")
                .flex_1()
                .flex_col()
                .overflow_hidden()
                .children(rows),
        )
        .child(self.render_archived(&theme, cx))
    }
}

impl SessionList {
    /// The archived region: a collapsible list pinned to the panel bottom,
    /// read-only rows whose only action is the two-step permanent delete.
    fn render_archived(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut section = div()
            .id("session-archived")
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border_subtle);

        let header = div()
            .id("session-archived-toggle")
            .cursor_pointer()
            .px_2()
            .py_1()
            .text_color(theme.text_tertiary)
            .child(format!(
                "{} Archived ({})",
                if self.archived_open { "▾" } else { "▸" },
                self.archived.len()
            ))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_archived(cx)));
        section = section.child(header);

        if self.archived_open {
            let confirming = self.confirming.clone();
            let rows = self
                .archived
                .iter()
                .map(|meta| {
                    let confirming_delete = confirming
                        .as_ref()
                        .is_some_and(|(i, k)| *i == meta.id && *k == ConfirmKind::Delete);
                    render_archived_row(meta, confirming_delete, theme, cx)
                })
                .collect::<Vec<_>>();
            section = section.child(div().flex().flex_col().children(rows));
        }
        section.into_any_element()
    }
}

/// Render one active session row: title, status icon, origin badge, and the
/// archive affordance (two-step confirm).
#[allow(clippy::needless_pass_by_ref_mut)]
fn render_session_row(
    row: &SessionRow,
    is_active: bool,
    confirming_archive: bool,
    theme: &Theme,
    cx: &mut Context<SessionList>,
) -> gpui::AnyElement {
    let id = row.id.clone();
    let (status_glyph, status_color) = match row.status {
        RowStatus::Running => ("●", theme.state_running),
        RowStatus::Awaiting => ("◐", theme.state_running),
        RowStatus::Unseen => ("●", theme.accent),
        RowStatus::Seen => ("○", theme.text_disabled),
    };

    let mut container = div()
        .id(ElementId::Name(format!("session-row-{}", row.id.0).into()))
        .group("session-row")
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1()
        .cursor_pointer()
        .when(is_active, |el| el.bg(theme.accent_dim))
        .hover(|s| s.bg(theme.canvas_overlay));

    let mut main = div()
        .flex()
        .items_center()
        .gap_2()
        .text_color(status_color)
        .child(status_glyph)
        .child(
            div()
                .text_color(if is_active {
                    theme.text_primary
                } else {
                    theme.text_secondary
                })
                .child(row.title.clone()),
        );
    if let Some(badge) = row.badge {
        main = main.child(
            div()
                .text_color(theme.text_tertiary)
                .child(format!("[{badge}]")),
        );
    }
    let select_id = id.clone();
    container = container.child(
        div()
            .id(ElementId::Name(
                format!("session-select-{}", row.id.0).into(),
            ))
            .flex_1()
            .child(main)
            .on_click(cx.listener(move |this, _, _, cx| this.select(select_id.clone(), cx))),
    );

    // Archive affordance: faint until hovered; two-step confirm inline.
    let archive_label = if confirming_archive {
        "confirm?"
    } else {
        "▼"
    };
    let archive_color = if confirming_archive {
        theme.state_running
    } else {
        theme.text_disabled
    };
    container = container.child(
        div()
            .id(ElementId::Name(
                format!("session-archive-{}", row.id.0).into(),
            ))
            .cursor_pointer()
            .text_color(archive_color)
            .child(archive_label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.arm_confirm(id.clone(), ConfirmKind::Archive, cx);
            })),
    );

    container.into_any_element()
}

/// Render one archived row: read-only, with the two-step permanent delete.
#[allow(clippy::needless_pass_by_ref_mut)]
fn render_archived_row(
    meta: &SessionMeta,
    confirming_delete: bool,
    theme: &Theme,
    cx: &mut Context<SessionList>,
) -> gpui::AnyElement {
    let id = meta.id.clone();
    let delete_label = if confirming_delete { "confirm?" } else { "✗" };
    let delete_color = if confirming_delete {
        theme.state_error
    } else {
        theme.text_disabled
    };
    div()
        .id(ElementId::Name(
            format!("archived-row-{}", meta.id.0).into(),
        ))
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1()
        .text_color(theme.text_tertiary)
        .child(short_id(&meta.id.0))
        .child(
            div()
                .id(ElementId::Name(
                    format!("archived-delete-{}", meta.id.0).into(),
                ))
                .cursor_pointer()
                .text_color(delete_color)
                .child(delete_label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.arm_confirm(id.clone(), ConfirmKind::Delete, cx);
                })),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use chrono::TimeZone;
    use ominiforge::gateway::WorkspaceId;
    use ominiforge::session::Origin;

    fn meta(id: &str, created_secs: i64, origin: Origin) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_owned()),
            profile_id: None,
            model: None,
            created_at: Utc.timestamp_opt(created_secs, 0).unwrap(),
            workspace: None,
            sandbox: None,
            origin,
            archived: false,
        }
    }

    fn status(id: &str, status: ActivityStatus, seq: u64) -> SessionStatus {
        SessionStatus {
            session_id: SessionId(id.to_owned()),
            workspace_id: WorkspaceId::none(),
            status,
            latest_seq: seq,
        }
    }

    /// Rows are titled by first user input (clipped) or a shortened id, and
    /// ordered most-recently-active first (last user message, else created).
    #[test]
    fn rows_are_titled_and_ordered_by_activity() {
        let mut state = SessionListState::default();
        state.load(vec![
            meta("aaa", 100, Origin::new()),
            meta("bbb", 200, Origin::new()),
        ]);
        // `aaa` has a later user message than `bbb`'s creation → floats to top.
        let s = SessionSummary {
            first_user_input: Some("fix the login bug".into()),
            last_user_message_at: Some(Utc.timestamp_opt(500, 0).unwrap()),
            ..SessionSummary::default()
        };
        state.apply_summary(s, &SessionId("aaa".into()));

        let rows = state.rows();
        assert_eq!(rows[0].id.0, "aaa", "most-recent activity first");
        assert_eq!(rows[0].title, "fix the login bug");
        assert_eq!(rows[1].id.0, "bbb");
        assert_eq!(rows[1].title, "bbb", "no summary → short id");
    }

    /// An idle session with a latest seq beyond the acknowledged watermark is
    /// `unseen`; marking it seen flips it to `seen`. Running/awaiting bypass
    /// the seq comparison entirely.
    #[test]
    fn unseen_seen_split_tracks_acknowledged_seq() {
        let mut state = SessionListState::default();
        state.load(vec![meta("s1", 100, Origin::new())]);
        state.apply_status(&status("s1", ActivityStatus::Idle, 5));
        assert_eq!(state.rows()[0].status, RowStatus::Unseen);

        state.mark_seen(&SessionId("s1".into()));
        assert_eq!(state.rows()[0].status, RowStatus::Seen);

        // A new event past the watermark makes it unseen again.
        state.apply_status(&status("s1", ActivityStatus::Idle, 9));
        assert_eq!(state.rows()[0].status, RowStatus::Unseen);

        // Running bypasses the seq split.
        state.apply_status(&status("s1", ActivityStatus::Running, 10));
        assert_eq!(state.rows()[0].status, RowStatus::Running);
    }

    /// Origin badges map to their display labels; `new` has none.
    #[test]
    fn origin_badges() {
        let mut state = SessionListState::default();
        state.load(vec![
            meta("n", 1, Origin::new()),
            meta("f", 2, Origin::fork(SessionId("p".into()), 3)),
            meta("c", 3, Origin::compaction(SessionId("p".into()))),
        ]);
        let rows = state.rows();
        let badge = |id: &str| rows.iter().find(|r| r.id.0 == id).unwrap().badge;
        assert_eq!(badge("n"), None);
        assert_eq!(badge("f"), Some("fork"));
        assert_eq!(badge("c"), Some("compacted"));
    }

    /// A removed session no longer appears (post archive/delete).
    #[test]
    fn remove_drops_row() {
        let mut state = SessionListState::default();
        state.load(vec![
            meta("x", 1, Origin::new()),
            meta("y", 2, Origin::new()),
        ]);
        state.remove(&SessionId("x".into()));
        assert_eq!(state.rows().len(), 1);
        assert_eq!(state.rows()[0].id.0, "y");
    }
}
