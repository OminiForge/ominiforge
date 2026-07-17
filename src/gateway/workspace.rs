//! Workspace grouping: sessions organized by their working directory.
//!
//! A workspace is identified by an opaque hash of its absolute path. Sessions
//! without a workspace (restricted sessions, `SessionMeta.workspace = None`) are
//! grouped under a special `"none"` workspace ID.
//!
//! The [`WorkspaceRegistry`] persists a `hash → canonical path` map so a
//! workspace is addressable by its opaque id without the real filesystem path
//! ever travelling to the client: the URL carries only the hash, and the
//! gateway resolves the path server-side when creating a session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::session::SessionMeta;

/// Workspace identifier: either a hash of the absolute path, or the sentinel
/// `"none"` for sessions without a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct WorkspaceId(pub String);

impl WorkspaceId {
    /// Sentinel ID for sessions without a workspace.
    pub const NONE: &'static str = "none";

    /// Derive a workspace ID from an absolute path: a stable FNV-1a hash of the
    /// path bytes, hex-encoded (16 chars / 64 bits).
    ///
    /// FNV-1a is a fixed algorithm — its output for a given input never changes,
    /// unlike `std`'s `DefaultHasher`, whose algorithm is explicitly allowed to
    /// change between compiler versions. That stability matters because this id is
    /// **persisted** (in `workspaces.json` and used to route URLs): a toolchain
    /// upgrade must not silently invalidate every stored workspace id. The id is
    /// also opaque — the path is not recoverable from it — so it can live in a URL
    /// without leaking the filesystem layout. The gateway maps it back to a path
    /// via [`WorkspaceRegistry`].
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Self {
        Self(fnv1a_hex(path.as_os_str().as_encoded_bytes()))
    }

    /// The sentinel for no-workspace sessions.
    #[must_use]
    pub fn none() -> Self {
        Self(Self::NONE.to_owned())
    }
}

/// FNV-1a 64-bit hash of `bytes`, lower-hex encoded to 16 chars. A fixed,
/// version-stable algorithm (offset basis + prime are constants), so a persisted
/// workspace id derived from a path stays the same across builds and toolchains.
fn fnv1a_hex(bytes: &[u8]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// A persisted `hash → canonical path` map, stored as JSON at
/// `<gateway_workspace>/.omini/workspaces.json`. Lets the gateway resolve an
/// opaque [`WorkspaceId`] (from a URL) back to the workspace path it must create
/// a session in — without the path ever being sent to the client.
///
/// The map is best-effort and self-healing: it is seeded opportunistically from
/// existing sessions' metadata (each session's `session.toml` already records
/// its canonical workspace), so a workspace that predates this file is still
/// addressable after one `list` pass. A missing or unreadable file is treated as
/// empty, never fatal.
#[derive(Debug, Default)]
pub struct WorkspaceRegistry {
    /// Path to the JSON store (`.../.omini/workspaces.json`).
    store_path: PathBuf,
    /// In-memory `hash → canonical path` map.
    map: HashMap<String, PathBuf>,
}

/// The on-disk shape of `workspaces.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspacesFile {
    /// `hash → canonical path`.
    workspaces: HashMap<String, PathBuf>,
}

impl WorkspaceRegistry {
    /// Load the registry from `store_path`, or start empty if the file is absent
    /// or unreadable (best-effort — the map is a cache, rebuildable from session
    /// metadata).
    ///
    /// Entries whose stored id does not match the current hash of their path are
    /// dropped on load: this self-heals a `workspaces.json` written by an older
    /// hash algorithm (its ids simply don't validate), and `seed_from_metas` then
    /// rebuilds them under the current one. So a hash-algorithm change degrades to
    /// "re-seed from session metadata", never to a permanently broken map.
    #[must_use]
    pub fn load(store_path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&store_path)
            .ok()
            .and_then(|text| serde_json::from_str::<WorkspacesFile>(&text).ok())
            .map(|f| f.workspaces)
            .unwrap_or_default()
            .into_iter()
            .filter(|(id, path)| *id == WorkspaceId::from_path(path).0)
            .collect();
        Self { store_path, map }
    }

    /// Record `path` (canonicalized) under its derived id and persist the map.
    /// Returns the id the caller should route to. Idempotent: re-recording the
    /// same path is a no-op write of the same entry.
    ///
    /// # Errors
    /// Canonicalization failure (the path does not exist) or an io/serialize
    /// error persisting the map.
    pub fn record(&mut self, path: &Path) -> Result<WorkspaceId> {
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("workspace path does not exist: {}", path.display()))?;
        let id = WorkspaceId::from_path(&canonical);
        self.map.insert(id.0.clone(), canonical);
        self.save()?;
        Ok(id)
    }

    /// Resolve `id` back to its canonical path, if known.
    #[must_use]
    pub fn path_for(&self, id: &WorkspaceId) -> Option<PathBuf> {
        self.map.get(&id.0).cloned()
    }

    /// Seed the map from session metadata: every session's canonical workspace
    /// becomes a `hash → path` entry. Persists once if anything new was learned.
    /// This is how pre-existing workspaces (created before `workspaces.json`)
    /// become addressable — a single `list` pass over the store recovers them.
    ///
    /// # Errors
    /// An io/serialize error persisting the map (only attempted when the seed
    /// added at least one new entry).
    pub fn seed_from_metas(&mut self, metas: &[SessionMeta]) -> Result<()> {
        let mut changed = false;
        for meta in metas {
            if let Some(ws) = meta.workspace.as_deref() {
                let id = WorkspaceId::from_path(ws);
                self.map.entry(id.0).or_insert_with(|| {
                    changed = true;
                    ws.to_path_buf()
                });
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// Persist the map to `store_path`, creating the parent directory if needed.
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = WorkspacesFile {
            workspaces: self.map.clone(),
        };
        let json =
            serde_json::to_string_pretty(&file).context("failed to serialize workspaces.json")?;
        std::fs::write(&self.store_path, json)
            .with_context(|| format!("failed to write {}", self.store_path.display()))
    }
}

/// One workspace in the dashboard: its path (or `None` for the `"none"` group),
/// the session IDs belonging to it, and the most recent session timestamp.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct WorkspaceSummary {
    /// Workspace identifier (hash of path, or `"none"`).
    pub id: WorkspaceId,
    /// Absolute path, or `None` for the no-workspace group.
    pub path: Option<PathBuf>,
    /// Number of sessions in this workspace.
    pub session_count: usize,
    /// Most recent session creation time (ISO 8601), or `None` if no sessions.
    pub latest_session_time: Option<String>,
}

/// Group sessions by workspace: read all session metadata, hash their workspace
/// paths, and return one [`WorkspaceSummary`] per unique workspace. Sessions
/// without a workspace land in the `"none"` group.
///
/// The returned list is sorted by `latest_session_time` descending (most
/// recently active workspace first), with the `"none"` group pinned last.
pub fn group_sessions(metas: Vec<SessionMeta>) -> Vec<WorkspaceSummary> {
    let mut groups: HashMap<WorkspaceId, Vec<SessionMeta>> = HashMap::new();

    for meta in metas {
        let id = meta
            .workspace
            .as_deref()
            .map_or_else(WorkspaceId::none, WorkspaceId::from_path);
        groups.entry(id).or_default().push(meta);
    }

    let mut summaries: Vec<WorkspaceSummary> = groups
        .into_iter()
        .map(|(id, sessions)| {
            let path = sessions.first().and_then(|s| s.workspace.clone());
            let session_count = sessions.len();
            let latest_session_time = sessions
                .iter()
                .map(|s| &s.created_at)
                .max()
                .map(chrono::DateTime::to_rfc3339);
            WorkspaceSummary {
                id,
                path,
                session_count,
                latest_session_time,
            }
        })
        .collect();

    // Sort: most recent first, `"none"` group pinned last.
    summaries.sort_by(|a, b| {
        if a.id.0 == WorkspaceId::NONE {
            return std::cmp::Ordering::Greater;
        }
        if b.id.0 == WorkspaceId::NONE {
            return std::cmp::Ordering::Less;
        }
        b.latest_session_time.cmp(&a.latest_session_time)
    });

    summaries
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::core::SessionId;
    use crate::session::Origin;
    use chrono::{TimeZone, Utc};

    fn meta(id: &str, workspace: Option<&str>, created_at: i64) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_owned()),
            profile_id: None,
            model: None,
            created_at: Utc.timestamp_opt(created_at, 0).unwrap(),
            workspace: workspace.map(PathBuf::from),
            sandbox: None,
            origin: Origin::new(),
            archived: false,
        }
    }

    #[test]
    fn from_path_produces_stable_hash() {
        let path = std::path::Path::new("/home/user/project");
        let id1 = WorkspaceId::from_path(path);
        let id2 = WorkspaceId::from_path(path);
        assert_eq!(id1, id2);
        assert_eq!(id1.0.len(), 16);
    }

    #[test]
    fn different_paths_produce_different_ids() {
        let id1 = WorkspaceId::from_path(std::path::Path::new("/home/user/proj1"));
        let id2 = WorkspaceId::from_path(std::path::Path::new("/home/user/proj2"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn none_id_is_sentinel() {
        assert_eq!(WorkspaceId::none().0, "none");
    }

    #[test]
    fn group_sessions_clusters_by_workspace() {
        let metas = vec![
            meta("s1", Some("/home/user/proj1"), 100),
            meta("s2", Some("/home/user/proj1"), 200),
            meta("s3", Some("/home/user/proj2"), 150),
            meta("s4", None, 50),
        ];

        let groups = group_sessions(metas);
        assert_eq!(groups.len(), 3);

        // Find proj1 group (2 sessions, latest = 200)
        let proj1 = groups
            .iter()
            .find(|g| g.path.as_deref() == Some(std::path::Path::new("/home/user/proj1")))
            .unwrap();
        assert_eq!(proj1.session_count, 2);
        assert_eq!(
            &proj1.latest_session_time.as_deref().unwrap()[..10],
            "1970-01-01"
        );

        // None group should be last
        assert_eq!(groups.last().unwrap().id.0, "none");
    }

    #[test]
    fn group_sessions_sorts_by_recent_activity() {
        let metas = vec![
            meta("s1", Some("/old"), 100),
            meta("s2", Some("/recent"), 300),
            meta("s3", Some("/middle"), 200),
        ];

        let groups = group_sessions(metas);
        assert_eq!(
            groups[0].path.as_deref(),
            Some(std::path::Path::new("/recent"))
        );
        assert_eq!(
            groups[1].path.as_deref(),
            Some(std::path::Path::new("/middle"))
        );
        assert_eq!(
            groups[2].path.as_deref(),
            Some(std::path::Path::new("/old"))
        );
    }

    #[test]
    fn none_group_always_pinned_last() {
        let metas = vec![
            meta("s1", None, 500), // none group, most recent
            meta("s2", Some("/proj"), 100),
        ];

        let groups = group_sessions(metas);
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0].path.as_deref(),
            Some(std::path::Path::new("/proj"))
        );
        assert_eq!(groups[1].id.0, "none");
    }

    /// `record` inserts a `hash → canonical path` entry and `path_for` resolves
    /// it back — the core round-trip the create-by-id flow depends on. The id is
    /// exactly `WorkspaceId::from_path` of the canonicalized path.
    #[test]
    fn registry_record_then_path_for_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let canonical = std::fs::canonicalize(&ws).unwrap();

        let mut reg = WorkspaceRegistry::load(dir.path().join("workspaces.json"));
        let id = reg.record(&ws).unwrap();
        assert_eq!(id, WorkspaceId::from_path(&canonical));
        assert_eq!(reg.path_for(&id), Some(canonical));
    }

    /// `record` persists to disk: a fresh `load` of the same path sees the entry
    /// (so a gateway restart keeps workspaces addressable).
    #[test]
    fn registry_record_persists_across_load() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&ws).unwrap();
        let store = dir.path().join("workspaces.json");

        let id = {
            let mut reg = WorkspaceRegistry::load(store.clone());
            reg.record(&ws).unwrap()
        };
        // Reload from disk — the entry survives.
        let reg = WorkspaceRegistry::load(store);
        assert_eq!(reg.path_for(&id), Some(std::fs::canonicalize(&ws).unwrap()));
    }

    /// `record` on a path that does not exist is an error (canonicalize fails) —
    /// surfaced so the endpoint can return 400 rather than storing a dead entry.
    #[test]
    fn registry_record_missing_path_errs() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = WorkspaceRegistry::load(dir.path().join("workspaces.json"));
        assert!(reg.record(&dir.path().join("nope")).is_err());
    }

    /// `seed_from_metas` learns every session's workspace, making a workspace
    /// that predates `workspaces.json` addressable after one list pass. Sessions
    /// with no workspace are skipped.
    #[test]
    fn registry_seeds_from_session_metas() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = WorkspaceRegistry::load(dir.path().join("workspaces.json"));

        let metas = vec![
            meta("s1", Some("/home/user/proj-a"), 100),
            meta("s2", Some("/home/user/proj-a"), 200), // dup path → one entry
            meta("s3", Some("/home/user/proj-b"), 150),
            meta("s4", None, 50), // no workspace → skipped
        ];
        reg.seed_from_metas(&metas).unwrap();

        let a = WorkspaceId::from_path(std::path::Path::new("/home/user/proj-a"));
        let b = WorkspaceId::from_path(std::path::Path::new("/home/user/proj-b"));
        assert_eq!(reg.path_for(&a), Some(PathBuf::from("/home/user/proj-a")));
        assert_eq!(reg.path_for(&b), Some(PathBuf::from("/home/user/proj-b")));
        // The no-workspace session contributed nothing.
        assert_eq!(reg.path_for(&WorkspaceId::none()), None);
    }

    /// FNV-1a is a fixed algorithm: known inputs hash to known outputs. Pinning
    /// them guards against an accidental change to the constants or byte handling
    /// that would silently invalidate every persisted workspace id. The empty
    /// input must equal the FNV offset basis; `"a"` is a standard test vector.
    #[test]
    fn fnv1a_is_stable_for_known_vectors() {
        assert_eq!(fnv1a_hex(b""), "cbf29ce484222325"); // offset basis
        assert_eq!(fnv1a_hex(b"a"), "af63dc4c8601ec8c"); // FNV-1a-64 test vector
        // Same input → same output, always (the property the persisted id relies on).
        assert_eq!(
            WorkspaceId::from_path(std::path::Path::new("/home/user/proj")).0,
            WorkspaceId::from_path(std::path::Path::new("/home/user/proj")).0
        );
    }

    /// `load` drops entries whose stored id does not match the current hash of
    /// their path. This self-heals a `workspaces.json` left by an older hash
    /// algorithm: its stale ids don't validate and are discarded, leaving the map
    /// to be rebuilt by `seed_from_metas` — never a permanently broken map.
    #[test]
    fn load_discards_entries_with_mismatched_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("workspaces.json");
        // Hand-write a file with one VALID entry (id matches from_path) and one
        // STALE entry (a bogus id, as an old algorithm would have produced).
        let valid_path = "/home/user/keep";
        let valid_id = WorkspaceId::from_path(std::path::Path::new(valid_path)).0;
        let json = format!(
            r#"{{"workspaces":{{"{valid_id}":"{valid_path}","deadbeefdeadbeef":"/home/user/drop"}}}}"#
        );
        std::fs::write(&store, json).unwrap();

        let reg = WorkspaceRegistry::load(store);
        // The valid entry survives; the mismatched (old-algorithm) one is gone.
        assert_eq!(
            reg.path_for(&WorkspaceId(valid_id)),
            Some(PathBuf::from(valid_path))
        );
        assert_eq!(
            reg.path_for(&WorkspaceId("deadbeefdeadbeef".to_owned())),
            None
        );
    }
}
