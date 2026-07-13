//! [`StatusHub`]: a process-wide broadcast of per-session *activity status*, so a
//! front-end can light up a whole list of sessions — across every workspace —
//! from one stream instead of subscribing to each session individually.
//!
//! This complements the per-session [`EventBus`](crate::session::EventBus): that
//! fans out one session's committed events to whoever is viewing *that* session;
//! this fans out a coarse `running | awaiting_approval | idle` status for *all*
//! sessions to whoever is viewing the *list*. Like the bus, publishing is
//! best-effort and off the hot turn path (called at turn boundaries, not per
//! token), and a lagging subscriber resyncs from [`snapshot`](StatusHub::snapshot)
//! rather than blocking a publisher.
//!
//! The hub also retains the last-known status per session in an in-memory map so
//! a client connecting mid-flight gets a full picture (the snapshot) before the
//! live deltas. The map is process-lifetime and never pruned — one small entry
//! per session touched this run; status is live-only and not persisted (a fresh
//! gateway starts with an empty map, and a cold session simply has no entry).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::core::SessionId;

use super::workspace::WorkspaceId;

/// Default capacity of the status broadcast channel. A subscriber that falls this
/// many deltas behind gets a `Lagged` error and should resync from `snapshot`.
const DEFAULT_CAPACITY: usize = 1024;

/// A session's coarse activity status, as the session list renders it.
///
/// `Idle` is the resting state (no turn running); the front-end further splits it
/// into "seen" vs "unseen" client-side by comparing [`SessionStatus::latest_seq`]
/// against a locally-remembered acknowledged seq — the gateway cannot know what a
/// user has *looked at*, only what the session is *doing*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    /// A turn is running (the actor is mid-turn).
    Running,
    /// A tool call is blocked pending user approval. Reserved: the approval
    /// feature is not built yet, so this variant is never published — the wire
    /// type and the front-end icon exist so wiring it later is a one-line publish.
    AwaitingApproval,
    /// No turn running. The common resting state.
    Idle,
}

/// One session's status on the wire.
///
/// Which session, in which workspace, its status, and the latest committed event
/// `seq` (the same monotonic sequence the SSE stream uses as its resume cursor).
/// `latest_seq` lets the front-end decide "unseen" without a second request.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct SessionStatus {
    /// The session this status is for.
    pub session_id: SessionId,
    /// The workspace the session belongs to (or the `"none"` sentinel), so a
    /// list scoped to one workspace can filter, and a future dashboard rollup can
    /// group — without the client re-deriving it.
    pub workspace_id: WorkspaceId,
    /// The coarse activity status.
    pub status: ActivityStatus,
    /// The session's latest committed event `seq` at the time of this status.
    pub latest_seq: u64,
}

/// A cheap, clonable handle to publish and subscribe to session status. Shares
/// one underlying channel + status map across clones (an `Arc` inside).
#[derive(Clone)]
pub struct StatusHub {
    inner: Arc<StatusHubInner>,
}

struct StatusHubInner {
    /// Last-known status per session, for the connect-time snapshot. Guarded by a
    /// std `RwLock` — every access is a short synchronous read/write with no await
    /// held across it.
    map: RwLock<HashMap<SessionId, SessionStatus>>,
    /// The process-wide delta stream. A publish updates `map` then sends here.
    tx: broadcast::Sender<SessionStatus>,
}

impl StatusHub {
    /// A hub with the default channel capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A hub with an explicit channel capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(StatusHubInner {
                map: RwLock::new(HashMap::new()),
                tx,
            }),
        }
    }

    /// Publish a status: record it as the session's last-known status
    /// (last-write-wins by session id) and broadcast it to live subscribers.
    /// Best-effort: the send is dropped when there are no subscribers.
    pub fn publish(&self, status: SessionStatus) {
        if let Ok(mut map) = self.inner.map.write() {
            map.insert(status.session_id.clone(), status.clone());
        }
        // `send` errors only when there are no receivers — normal (a headless run
        // with no list open), so the result is discarded.
        let _ = self.inner.tx.send(status);
    }

    /// Mark a session `Idle` while preserving its recorded workspace + latest seq.
    ///
    /// A no-op if the session has no recorded status (never ran a turn) or is
    /// already `Idle`. This is the terminal transition an actor publishes on every
    /// exit path (turn settle, cancel, idle-eviction, shutdown) so a session never
    /// gets stuck showing a spinner after its actor is gone.
    pub fn mark_idle(&self, session_id: &SessionId) {
        // Read the current entry, decide, and build the next value without holding
        // the lock across the broadcast send.
        let next = {
            let Ok(map) = self.inner.map.read() else {
                return;
            };
            match map.get(session_id) {
                Some(cur) if cur.status != ActivityStatus::Idle => SessionStatus {
                    status: ActivityStatus::Idle,
                    ..cur.clone()
                },
                _ => return, // absent, or already idle — nothing to publish
            }
        };
        self.publish(next);
    }

    /// A point-in-time copy of every session's last-known status, for a client
    /// connecting mid-flight to seed its view before live deltas arrive.
    #[must_use]
    pub fn snapshot(&self) -> Vec<SessionStatus> {
        self.inner
            .map
            .read()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default()
    }

    /// The last-known activity status for one session, or `None` if it has never
    /// published one (no actor has run a turn for it this process). Used to gate
    /// destructive lifecycle ops (`archive`): a `Running` session must not be
    /// retired out from under a live turn (`doc/session-storage.md` §9).
    #[must_use]
    pub fn status_of(&self, session_id: &SessionId) -> Option<ActivityStatus> {
        self.inner
            .map
            .read()
            .ok()
            .and_then(|map| map.get(session_id).map(|s| s.status))
    }

    /// Subscribe to status deltas published from now on. A receiver that falls
    /// more than the channel capacity behind gets a `Lagged` error and should
    /// resync via [`snapshot`](Self::snapshot).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SessionStatus> {
        self.inner.tx.subscribe()
    }
}

impl Default for StatusHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn status(id: &str, status: ActivityStatus, seq: u64) -> SessionStatus {
        SessionStatus {
            session_id: SessionId(id.to_owned()),
            workspace_id: WorkspaceId::none(),
            status,
            latest_seq: seq,
        }
    }

    /// A subscriber receives statuses published after it subscribed, in order —
    /// the core delta contract the list's live stream relies on.
    #[tokio::test]
    async fn subscriber_receives_published_statuses_in_order() {
        let hub = StatusHub::new();
        let mut rx = hub.subscribe();

        hub.publish(status("s1", ActivityStatus::Running, 3));
        hub.publish(status("s1", ActivityStatus::Idle, 7));

        let first = rx.recv().await.unwrap();
        assert_eq!(first.session_id.0, "s1");
        assert_eq!(first.status, ActivityStatus::Running);
        let second = rx.recv().await.unwrap();
        assert_eq!(second.status, ActivityStatus::Idle);
        assert_eq!(second.latest_seq, 7);
    }

    /// The snapshot reflects the *last* status per session (last-write-wins), not
    /// the whole history — a mid-flight client sees each session once, current.
    #[test]
    fn snapshot_is_last_write_wins_per_session() {
        let hub = StatusHub::new();
        hub.publish(status("s1", ActivityStatus::Running, 1));
        hub.publish(status("s2", ActivityStatus::Running, 1));
        hub.publish(status("s1", ActivityStatus::Idle, 5)); // supersedes s1

        let mut snap = hub.snapshot();
        snap.sort_by(|a, b| a.session_id.0.cmp(&b.session_id.0));
        assert_eq!(snap.len(), 2, "one entry per session");
        assert_eq!(snap[0].session_id.0, "s1");
        assert_eq!(snap[0].status, ActivityStatus::Idle, "latest wins");
        assert_eq!(snap[0].latest_seq, 5);
        assert_eq!(snap[1].status, ActivityStatus::Running);
    }

    /// `mark_idle` flips a running session to idle while keeping its seq +
    /// workspace, and broadcasts the transition. This is the eviction safety net:
    /// an actor dying mid-turn must not leave a stuck `Running`.
    #[tokio::test]
    async fn mark_idle_transitions_running_to_idle_preserving_seq() {
        let hub = StatusHub::new();
        hub.publish(status("s1", ActivityStatus::Running, 9));
        let mut rx = hub.subscribe();

        hub.mark_idle(&SessionId("s1".to_owned()));

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.status, ActivityStatus::Idle);
        assert_eq!(ev.latest_seq, 9, "seq is preserved across the idle flip");
        assert_eq!(hub.snapshot()[0].status, ActivityStatus::Idle);
    }

    /// `mark_idle` on an unknown or already-idle session publishes nothing, so an
    /// actor's terminal cleanup on a session that never ran (or already settled)
    /// is silent rather than emitting a redundant delta.
    #[test]
    fn mark_idle_is_noop_when_absent_or_already_idle() {
        let hub = StatusHub::new();
        // Absent: nothing recorded.
        hub.mark_idle(&SessionId("ghost".to_owned()));
        assert!(hub.snapshot().is_empty());

        // Already idle: entry stays, but no second delta is produced.
        hub.publish(status("s1", ActivityStatus::Idle, 2));
        let mut rx = hub.subscribe();
        hub.mark_idle(&SessionId("s1".to_owned()));
        assert!(
            rx.try_recv().is_err(),
            "no delta when the session is already idle"
        );
    }

    /// `status_of` reflects the current per-session status (or `None` when
    /// unknown) — the read the archive guard depends on to reject retiring a
    /// `Running` session while letting an `Idle` (or never-run) one through.
    #[test]
    fn status_of_reads_current_or_none() {
        let hub = StatusHub::new();
        let s1 = SessionId("s1".to_owned());

        assert_eq!(hub.status_of(&s1), None, "never published → unknown");

        hub.publish(status("s1", ActivityStatus::Running, 1));
        assert_eq!(hub.status_of(&s1), Some(ActivityStatus::Running));

        hub.publish(status("s1", ActivityStatus::Idle, 3));
        assert_eq!(hub.status_of(&s1), Some(ActivityStatus::Idle), "latest wins");
    }
}
