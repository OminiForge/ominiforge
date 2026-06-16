//! The append-only `events.jsonl` event log.
//!
//! `events.jsonl` is the source of truth (see `doc/session-storage.md` §3).
//! One event per line. The `session_id` is *omitted* on disk — it is the
//! session directory name — and restored on read. A single writer holds an
//! exclusive advisory lock on the file for its lifetime (§8); readers do not
//! need the lock since the file is append-only.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::{Result, SessionError};
use crate::core::{CoreEvent, EventSource, SessionId, TurnId};
use crate::core::{EventId, EventPayload};

/// An exclusively-locked handle to a session's `events.jsonl`, open for append.
///
/// The lock is released automatically when this value is dropped (or when the
/// process exits — the kernel reclaims advisory locks, so there are no stale
/// locks to clean up).
#[derive(Debug)]
pub struct EventLog {
    file: File,
    path: PathBuf,
}

impl EventLog {
    /// Open `path` for appending, taking an exclusive lock.
    ///
    /// # Errors
    /// Returns [`SessionError::Locked`] if another writer holds the lock, or
    /// [`SessionError::Io`] on other filesystem failures.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;

        match file.try_lock() {
            Ok(()) => Ok(Self {
                file,
                path: path.to_path_buf(),
            }),
            Err(TryLockError::WouldBlock) => Err(SessionError::Locked {
                path: path.to_path_buf(),
            }),
            Err(TryLockError::Error(source)) => Err(SessionError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Append one event, serialized as a single JSON line with `session_id`
    /// stripped.
    ///
    /// # Errors
    /// Returns [`SessionError::Event`] on serialization failure or
    /// [`SessionError::Io`] on write failure.
    pub fn append(&mut self, event: &CoreEvent) -> Result<()> {
        let record = EventRecord::from_event(event);
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|source| SessionError::Io {
                path: self.path.clone(),
                source,
            })
    }

    /// The path of the underlying log file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read every event from `path`, restoring each `session_id` from `session_id`.
///
/// This does not take the lock: the file is append-only and safe to read
/// concurrently with a writer.
///
/// # Errors
/// Returns [`SessionError::Io`] if the file cannot be read or
/// [`SessionError::Event`] if a line cannot be parsed.
pub fn read_events(path: &Path, session_id: &SessionId) -> Result<Vec<CoreEvent>> {
    let file = File::open(path).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = BufReader::new(file);

    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record: EventRecord = serde_json::from_str(&line)?;
        events.push(record.into_event(session_id.clone()));
    }
    Ok(events)
}

/// The on-disk form of a [`CoreEvent`]: identical, minus `session_id`.
///
/// Built from `CoreEvent` by exhaustive destructuring so that adding a field to
/// `CoreEvent` is a compile error here rather than a silently dropped field.
#[derive(Debug, Serialize, Deserialize)]
struct EventRecord {
    schema_version: String,
    seq: u64,
    timestamp: chrono::DateTime<chrono::Utc>,
    source: EventSource,
    #[serde(default)]
    parent_event_id: Option<EventId>,
    #[serde(default)]
    turn_id: Option<TurnId>,
    payload: EventPayload,
}

impl EventRecord {
    fn from_event(event: &CoreEvent) -> Self {
        // Exhaustive destructure: a new CoreEvent field won't compile until
        // handled here. `session_id` is intentionally dropped (it is the
        // directory name).
        let CoreEvent {
            schema_version,
            seq,
            session_id: _,
            timestamp,
            source,
            parent_event_id,
            turn_id,
            payload,
        } = event;
        Self {
            schema_version: schema_version.clone(),
            seq: *seq,
            timestamp: *timestamp,
            source: source.clone(),
            parent_event_id: parent_event_id.clone(),
            turn_id: turn_id.clone(),
            payload: payload.clone(),
        }
    }

    fn into_event(self, session_id: SessionId) -> CoreEvent {
        CoreEvent {
            schema_version: self.schema_version,
            seq: self.seq,
            session_id,
            timestamp: self.timestamp,
            source: self.source,
            parent_event_id: self.parent_event_id,
            turn_id: self.turn_id,
            payload: self.payload,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::payload::SessionEvent;
    use crate::core::{SCHEMA_VERSION, SourceKind};
    use chrono::{TimeZone, Utc};

    fn created_event(session_id: &SessionId, seq: u64) -> CoreEvent {
        CoreEvent {
            schema_version: SCHEMA_VERSION.to_owned(),
            seq,
            session_id: session_id.clone(),
            timestamp: Utc.with_ymd_and_hms(2026, 6, 11, 10, 0, 0).unwrap(),
            source: EventSource {
                kind: SourceKind::Runtime,
                id: "ominiforge".to_owned(),
            },
            parent_event_id: None,
            turn_id: None,
            payload: EventPayload::Session(SessionEvent::Created {
                profile_id: Some("coding-agent".to_owned()),
                tools: vec!["shell".to_owned()],
                workspace: None,
            }),
        }
    }

    #[test]
    fn disk_lines_omit_session_id_but_read_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sid = SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned());

        {
            let mut log = EventLog::open(&path).unwrap();
            log.append(&created_event(&sid, 0)).unwrap();
        }

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("session_id"));
        assert!(!raw.contains(&sid.0));
        assert_eq!(raw.lines().count(), 1);

        let events = read_events(&path, &sid).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], created_event(&sid, 0));
    }

    #[test]
    fn second_writer_is_locked_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");

        let _first = EventLog::open(&path).unwrap();
        match EventLog::open(&path) {
            Err(SessionError::Locked { .. }) => {}
            other => panic!("expected Locked, got {other:?}"),
        }
    }

    #[test]
    fn append_preserves_order_and_seq() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sid = SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned());

        {
            let mut log = EventLog::open(&path).unwrap();
            for seq in 0..3 {
                log.append(&created_event(&sid, seq)).unwrap();
            }
        }

        let events = read_events(&path, &sid).unwrap();
        let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }
}
