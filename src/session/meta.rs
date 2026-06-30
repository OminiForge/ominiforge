//! The `session.toml` data model: pure metadata, no runtime state.
//!
//! Mirrors `doc/session-storage.md` §2. There is deliberately no `status`
//! field (a session exists, therefore it is usable) and no `system_prompt`
//! (that lives in the context snapshot / messages, not here).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::SessionId;

/// The contents of a session's `session.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct SessionMeta {
    /// The session id (also the directory name).
    pub id: SessionId,

    /// The profile this session is bound to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,

    /// When the session was created.
    pub created_at: DateTime<Utc>,

    /// The working directory. `None` means filesystem tools are restricted
    /// (research / chat / planning sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,

    /// How this session came to exist.
    pub origin: Origin,
}

/// How a session was born. `kind` carries the semantics; `parent_id` and
/// `fork_at_seq` are shared across the non-`new` kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct Origin {
    pub kind: OriginKind,

    /// The session this one derives from. Absent only for `new`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,

    /// The parent seq this session forked at. Present only for `fork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_at_seq: Option<u64>,
}

/// The four ways a session can come into being. See `doc/session-storage.md` §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    /// A user starting a fresh conversation. No parent, no context snapshot.
    New,
    /// Branching from a point in another session to explore separately.
    Fork,
    /// A lossy summary created when context overflows.
    Compaction,
    /// A config change (system prompt / tool set) materialized as a new session.
    Reconfiguration,
}

impl Origin {
    /// Origin for a brand-new session with no parent.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            kind: OriginKind::New,
            parent_id: None,
            fork_at_seq: None,
        }
    }

    /// Origin for a compaction session.
    #[must_use]
    pub const fn compaction(parent_id: SessionId) -> Self {
        Self {
            kind: OriginKind::Compaction,
            parent_id: Some(parent_id),
            fork_at_seq: None,
        }
    }

    /// Origin for a fork: branched from `parent_id` at `fork_at_seq`
    /// (`doc/session-storage.md` §5, `doc/architecture.md` §6.1).
    #[must_use]
    pub const fn fork(parent_id: SessionId, fork_at_seq: u64) -> Self {
        Self {
            kind: OriginKind::Fork,
            parent_id: Some(parent_id),
            fork_at_seq: Some(fork_at_seq),
        }
    }

    /// Origin for a reconfiguration: a config change (profile / model / tool set)
    /// materialized as a new session seeded with `parent_id`'s full context
    /// (`doc/profile.md` §5). Like compaction it carries no `fork_at_seq` — the
    /// whole conversation moves to the new config, not a branch point.
    #[must_use]
    pub const fn reconfiguration(parent_id: SessionId) -> Self {
        Self {
            kind: OriginKind::Reconfiguration,
            parent_id: Some(parent_id),
            fork_at_seq: None,
        }
    }
}

impl Default for Origin {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use chrono::TimeZone;

    #[test]
    fn new_session_meta_round_trips_through_toml() {
        let meta = SessionMeta {
            id: SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned()),
            profile_id: Some("coding-agent".to_owned()),
            created_at: Utc.with_ymd_and_hms(2026, 6, 11, 10, 0, 0).unwrap(),
            workspace: Some(PathBuf::from("/home/user/project/foo")),
            origin: Origin::new(),
        };

        let text = toml::to_string(&meta).unwrap();
        let decoded: SessionMeta = toml::from_str(&text).unwrap();
        assert_eq!(meta, decoded);
    }

    #[test]
    fn new_origin_omits_parent_fields() {
        let meta = SessionMeta {
            id: SessionId("01J5M3HKEA7V2X3P1YKRN9C4WG".to_owned()),
            profile_id: None,
            created_at: Utc.with_ymd_and_hms(2026, 6, 11, 10, 0, 0).unwrap(),
            workspace: None,
            origin: Origin::new(),
        };

        let text = toml::to_string(&meta).unwrap();
        assert!(text.contains("kind = \"new\""));
        assert!(!text.contains("parent_id"));
        assert!(!text.contains("fork_at_seq"));
        assert!(!text.contains("profile_id"));
    }
}
