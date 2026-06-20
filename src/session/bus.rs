//! `EventBus`: a broadcast channel that fans out persisted events to live
//! subscribers (the monitor, the TUI) without touching the write path.
//!
//! Events are published *after* they are durably appended to `events.jsonl`, so
//! the log stays the source of truth and a subscriber only ever sees committed
//! events (`doc/monitor.md` §9). Publishing is best-effort: if there are no
//! subscribers, or the channel is full and a slow subscriber lags, the send is
//! dropped rather than blocking the agent loop. A subscriber that wants exact
//! history reads the log; the bus is for liveness, not durability.

use tokio::sync::broadcast;

use crate::core::CoreEvent;

/// Default capacity of the broadcast channel. A lagging subscriber past this
/// many buffered events sees a `Lagged` error and resyncs from the log.
const DEFAULT_CAPACITY: usize = 1024;

/// A clonable handle to publish events to all live subscribers.
///
/// Cheap to clone (it shares the underlying channel). A [`SessionWriter`] holds
/// one to publish each appended event; front-ends call [`EventBus::subscribe`]
/// to receive them.
///
/// [`SessionWriter`]: super::SessionWriter
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<CoreEvent>,
}

impl EventBus {
    /// A bus with the default channel capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A bus with an explicit channel capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event to all current subscribers. Best-effort: returns the
    /// number of subscribers that received it (`0` if none), never errors.
    pub fn publish(&self, event: &CoreEvent) {
        // `send` errors only when there are no receivers; that is normal (a
        // headless run with nothing watching), so the result is discarded.
        let _ = self.sender.send(event.clone());
    }

    /// Subscribe to events published from now on. The returned receiver yields
    /// every subsequent event; if it falls more than the channel capacity
    /// behind, it gets a `Lagged` error and should resync from the log.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::payload::{SessionEvent, TurnEvent};
    use crate::core::{EventPayload, EventSource, SCHEMA_VERSION, SessionId, SourceKind, TurnId};

    fn ev(seq: u64, payload: EventPayload) -> CoreEvent {
        CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            session_id: SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned()),
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

    /// A subscriber receives events published after it subscribed, in order.
    #[tokio::test]
    async fn subscriber_receives_published_events_in_order() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.publish(&ev(
            0,
            EventPayload::Session(SessionEvent::Created {
                profile_id: None,
                tools: vec![],
                workspace: None,
            }),
        ));
        bus.publish(&ev(
            1,
            EventPayload::Turn(TurnEvent::Started {
                turn_id: TurnId("t".to_owned()),
                input: Some("hi".to_owned()),
            }),
        ));

        assert_eq!(rx.recv().await.unwrap().seq, 0);
        assert_eq!(rx.recv().await.unwrap().seq, 1);
    }

    /// Publishing with no subscribers is a no-op, not an error — the headless
    /// case (a `run` with nothing watching).
    #[test]
    fn publish_without_subscribers_is_fine() {
        let bus = EventBus::new();
        bus.publish(&ev(0, EventPayload::Session(SessionEvent::Paused)));
    }
}
