//! Session storage: append-only event log plus `session.toml` metadata.
//!
//! Layout (see `doc/session-storage.md`):
//!
//! ```text
//! <root>/                      # e.g. .omini/sessions
//!   <session_id>/              # directory name == session id (ULID)
//!     session.toml             # pure metadata
//!     events.jsonl             # event stream, source of truth
//!     context_snapshot.json    # only for fork/compaction/reconfiguration
//!     artifacts/               # tool outputs (later phase)
//! ```
//!
//! Phase 1 implements `new` sessions (create, append, read). Fork and
//! compaction — which require a context snapshot — land with the `context`
//! module.

mod error;
mod event_log;
mod id;
mod meta;

pub use error::{Result, SessionError};
pub use event_log::EventLog;
pub use meta::{Origin, OriginKind, SessionMeta};

use std::path::PathBuf;

use chrono::Utc;

use crate::core::payload::SessionEvent;
use crate::core::{
    CoreEvent, EventPayload, EventSource, SCHEMA_VERSION, SessionId, SourceKind, TurnId,
};

const META_FILE: &str = "session.toml";
const EVENTS_FILE: &str = "events.jsonl";

/// Owns the root directory under which all session directories live, and mints,
/// opens, and reads sessions.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Create a store rooted at `root` (e.g. `.omini/sessions`). The directory
    /// is created on first write, not here.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory for `session_id` (whether or not it exists yet).
    #[must_use]
    pub fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(&session_id.0)
    }

    fn meta_path(&self, session_id: &SessionId) -> PathBuf {
        self.session_dir(session_id).join(META_FILE)
    }

    fn events_path(&self, session_id: &SessionId) -> PathBuf {
        self.session_dir(session_id).join(EVENTS_FILE)
    }

    /// Create a brand-new (`origin.kind = new`) session: mint an id, write
    /// `session.toml`, and emit the opening [`SessionEvent::Created`] event.
    ///
    /// Returns a locked [`SessionWriter`] positioned after the `Created` event.
    ///
    /// # Errors
    /// Filesystem or serialization failures surface as [`SessionError`].
    pub fn create_new(
        &self,
        profile_id: Option<String>,
        workspace: Option<PathBuf>,
        tools: Vec<String>,
    ) -> Result<SessionWriter> {
        let session_id = id::generate();
        let dir = self.session_dir(&session_id);
        std::fs::create_dir_all(&dir).map_err(|source| SessionError::Io {
            path: dir.clone(),
            source,
        })?;

        let meta = SessionMeta {
            id: session_id.clone(),
            profile_id: profile_id.clone(),
            created_at: Utc::now(),
            workspace: workspace.clone(),
            origin: Origin::new(),
        };
        self.write_meta(&meta)?;

        let log = EventLog::open(&self.events_path(&session_id))?;
        let mut writer = SessionWriter {
            session_id,
            log,
            next_seq: 0,
        };

        let created = EventPayload::Session(SessionEvent::Created {
            profile_id,
            tools,
            workspace,
        });
        writer.append(runtime_source(), created, None, None)?;
        Ok(writer)
    }

    /// Reopen an existing session for appending: take the event-log lock and
    /// position the writer's `seq` just past the last persisted event.
    ///
    /// The session must already exist (its `session.toml` is read to confirm).
    /// Used to continue a session across process restarts (`--resume` /
    /// `--continue`); the caller rebuilds the in-memory [`crate::agent::SessionRuntime`]
    /// separately from the event stream.
    ///
    /// # Errors
    /// [`SessionError::NotFound`] if the session does not exist,
    /// [`SessionError::Locked`] if another writer holds it, otherwise a
    /// filesystem or parse error.
    pub fn open(&self, session_id: &SessionId) -> Result<SessionWriter> {
        // Confirm the session exists (and is well-formed) before locking.
        let _ = self.read_meta(session_id)?;

        let events = self.read_events(session_id)?;
        let next_seq = events.last().map_or(0, |e| e.seq + 1);

        let log = EventLog::open(&self.events_path(session_id))?;
        Ok(SessionWriter {
            session_id: session_id.clone(),
            log,
            next_seq,
        })
    }

    /// All session ids in this store, newest first.
    ///
    /// Session directory names are ULIDs whose millisecond-timestamp prefix
    /// sorts in creation order, so sorting the directory names descending yields
    /// newest-first. Used by `--resume` (no id) to show the user what sessions
    /// exist so they can pick one.
    ///
    /// # Errors
    /// [`SessionError::Io`] if the store root cannot be read. Returns an empty
    /// vec when the root does not exist yet or holds no sessions.
    pub fn list(&self) -> Result<Vec<SessionId>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(SessionError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };

        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| SessionError::Io {
                path: self.root.clone(),
                source,
            })?;
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                ids.push(name);
            }
        }
        // ULID order == creation order; reverse for newest-first.
        ids.sort_unstable_by(|a, b| b.cmp(a));
        Ok(ids.into_iter().map(SessionId).collect())
    }

    /// Read the `session.toml` for an existing session.
    ///
    /// # Errors
    /// [`SessionError::NotFound`] if the metadata file is absent, otherwise a
    /// filesystem or parse error.
    pub fn read_meta(&self, session_id: &SessionId) -> Result<SessionMeta> {
        let path = self.meta_path(session_id);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(SessionError::NotFound(session_id.clone()));
            }
            Err(source) => return Err(SessionError::Io { path, source }),
        };
        Ok(toml::from_str(&text)?)
    }

    /// Read the full event stream for a session, with `session_id` restored on
    /// every event.
    ///
    /// # Errors
    /// Filesystem or parse errors surface as [`SessionError`].
    pub fn read_events(&self, session_id: &SessionId) -> Result<Vec<CoreEvent>> {
        event_log::read_events(&self.events_path(session_id), session_id)
    }

    fn write_meta(&self, meta: &SessionMeta) -> Result<()> {
        let path = self.meta_path(&meta.id);
        let text = toml::to_string(meta)?;
        std::fs::write(&path, text).map_err(|source| SessionError::Io { path, source })
    }
}

/// A single writer for one session.
///
/// Holds the exclusive event-log lock and stamps the envelope
/// (`schema_version`, monotonic `seq`, `session_id`, `timestamp`) onto each
/// appended event so callers supply only the meaningful parts.
#[derive(Debug)]
pub struct SessionWriter {
    session_id: SessionId,
    log: EventLog,
    next_seq: u64,
}

impl SessionWriter {
    /// The session being written.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The seq the next appended event will receive.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Append an event, filling in the envelope. Returns the assigned seq.
    ///
    /// # Errors
    /// Serialization or filesystem failures surface as [`SessionError`].
    pub fn append(
        &mut self,
        source: EventSource,
        payload: EventPayload,
        parent_event_id: Option<crate::core::EventId>,
        turn_id: Option<TurnId>,
    ) -> Result<u64> {
        let seq = self.next_seq;
        let event = CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            session_id: self.session_id.clone(),
            timestamp: Utc::now(),
            source,
            parent_event_id,
            turn_id,
            payload,
        };
        self.log.append(&event)?;
        self.next_seq += 1;
        Ok(seq)
    }
}

/// The event source for runtime-emitted events (`SessionEvent::Created`, etc.).
fn runtime_source() -> EventSource {
    EventSource {
        kind: SourceKind::Runtime,
        id: "ominiforge".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::payload::ModelEvent;

    fn model_source() -> EventSource {
        EventSource {
            kind: SourceKind::Model,
            id: "test".to_owned(),
        }
    }

    /// A minimal `ModelEvent::RequestStarted` payload for append tests.
    fn request_started() -> EventPayload {
        EventPayload::Model(ModelEvent::RequestStarted {
            request_id: "r1".to_owned(),
            provider: "test".to_owned(),
            model: "m".to_owned(),
            temperature: 0.0,
            max_tokens: None,
            tool_schemas_count: 0,
            input_tokens_estimate: 0,
        })
    }

    #[test]
    fn create_new_writes_meta_and_first_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());

        let writer = store
            .create_new(Some("coding".to_owned()), None, vec!["shell".to_owned()])
            .unwrap();
        let sid = writer.session_id().clone();
        drop(writer);

        let meta = store.read_meta(&sid).unwrap();
        assert_eq!(meta.id, sid);
        assert_eq!(meta.profile_id.as_deref(), Some("coding"));
        assert_eq!(meta.origin.kind, OriginKind::New);

        let events = store.read_events(&sid).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 0);
        assert!(matches!(
            events[0].payload,
            EventPayload::Session(SessionEvent::Created { .. })
        ));
    }

    #[test]
    fn appends_get_monotonic_seq() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut writer = store.create_new(None, None, vec![]).unwrap();

        let payload = EventPayload::Model(ModelEvent::RequestStarted {
            request_id: "r1".to_owned(),
            provider: "test".to_owned(),
            model: "m".to_owned(),
            temperature: 0.0,
            max_tokens: None,
            tool_schemas_count: 0,
            input_tokens_estimate: 0,
        });
        let seq = writer.append(model_source(), payload, None, None).unwrap();
        assert_eq!(seq, 1, "Created took seq 0");
        assert_eq!(writer.next_seq(), 2);
    }

    #[test]
    fn reopening_the_log_while_writer_is_alive_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();

        let events_path = store.session_dir(&sid).join(EVENTS_FILE);
        match EventLog::open(&events_path) {
            Err(SessionError::Locked { .. }) => {}
            other => panic!("expected Locked, got {other:?}"),
        }
    }

    #[test]
    fn read_meta_missing_session_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let missing = SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned());
        match store.read_meta(&missing) {
            Err(SessionError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Reopening a session for append continues the seq from where it left off,
    /// and the combined stream reads back contiguous. This is the storage half of
    /// session resume (`--resume` / `--continue`).
    #[test]
    fn open_continues_seq_across_a_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());

        // First writer: Created (seq 0) + one event (seq 1), then drop to release
        // the lock.
        let mut writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();
        writer
            .append(model_source(), request_started(), None, None)
            .unwrap();
        assert_eq!(writer.next_seq(), 2);
        drop(writer);

        // Reopen: seq must resume at 2, not restart at 0.
        let mut reopened = store.open(&sid).unwrap();
        assert_eq!(reopened.next_seq(), 2);
        let seq = reopened
            .append(model_source(), request_started(), None, None)
            .unwrap();
        assert_eq!(seq, 2);
        drop(reopened);

        // The full stream is contiguous 0,1,2.
        let events = store.read_events(&sid).unwrap();
        let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    /// Opening a session while a writer still holds it is locked out — the
    /// single-writer invariant holds across the create/open boundary.
    #[test]
    fn open_while_writer_alive_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let writer = store.create_new(None, None, vec![]).unwrap();
        let sid = writer.session_id().clone();

        match store.open(&sid) {
            Err(SessionError::Locked { .. }) => {}
            other => panic!("expected Locked, got {other:?}"),
        }
    }

    /// Opening a session that was never created is `NotFound`.
    #[test]
    fn open_missing_session_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let missing = SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned());
        match store.open(&missing) {
            Err(SessionError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// `list` returns all sessions newest-first, and is empty for an absent
    /// store.
    #[test]
    fn list_returns_sessions_newest_first_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());

        // Absent root → empty (not an error).
        assert!(store.list().unwrap().is_empty());

        let first = store.create_new(None, None, vec![]).unwrap();
        let first_id = first.session_id().clone();
        drop(first);
        // ULID prefix is millisecond time; ensure the second sorts strictly later.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = store.create_new(None, None, vec![]).unwrap();
        let second_id = second.session_id().clone();
        drop(second);

        assert!(second_id.0 > first_id.0, "ULIDs sort in creation order");
        // Newest first.
        assert_eq!(store.list().unwrap(), vec![second_id, first_id]);
    }
}
