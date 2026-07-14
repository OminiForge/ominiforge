//! Per-workspace sandbox configuration (`doc/workspace-config.md`).
//!
//! A workspace-level override layer for sandbox policy, sitting between the
//! profile and the gateway default in the resolution chain
//! (`doc/sandbox.md` §6.2):
//!
//! ```text
//! workspace.toml  >  profile [network]  >  gateway default_network  >  Open
//! ```
//!
//! ## Why gateway-side, not in the project directory
//!
//! These files live under the **gateway's** `.omini/workspaces/`, keyed by an
//! opaque [`WorkspaceId`] hash of the workspace path — deliberately *not* in the
//! project directory. The project directory is agent-writable (`doc/sandbox.md`
//! §3.3: "app treats the workspace as an ordinary directory"), so reading a
//! security policy from there would let an agent widen its own network/permission
//! grants — a privilege escalation against the secret-store threat model. The
//! gateway directory is deployer-controlled and trusted.
//!
//! ## Lifecycle / GC
//!
//! A config can outlive its workspace (the project is moved or deleted while the
//! policy file remains). We **never** auto-delete: a missing path may be
//! transient (an unmounted disk, a project mid-move, a temporarily removed
//! worktree), and silently deleting a hand-written policy is unrecoverable data
//! loss (Karpathy §12). Instead orphans are surfaced ([`list_orphans`]) and
//! removed only on an explicit [`delete`] — mirroring session archive's
//! "explicit, one-way" retirement.
//!
//! [`list_orphans`]: WorkspaceConfigStore::list_orphans
//! [`delete`]: WorkspaceConfigStore::delete

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, NetworkSection};

use super::workspace::{WorkspaceId, WorkspaceRegistry};

/// The on-disk shape of `<gateway>/.omini/workspaces/<id>.toml`.
///
/// Only `[network]` and `[[mounts]]` are defined today; the record is
/// intentionally open for a future permission-gating section and workspace
/// memory (`doc/workspace-config.md`) without a schema change forcing every file
/// to be rewritten. Unknown keys are ignored for forward compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Sandbox network egress override for sessions in this workspace. `None`
    /// (section absent) falls through to the profile / gateway default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkSection>,
    /// Auxiliary sandbox mounts (`doc/sandbox.md` §3.7): each binds a named
    /// anchor's host directory into the guest. Empty (section absent) = only the
    /// workspace mount (§3.3).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountSpec>,
}

/// One entry in a workspace's `[[mounts]]`: bind a named anchor's host directory
/// into the guest (`doc/sandbox.md` §3.7). The anchor names a *sharing scope*
/// (session-private / workspace-shared / gateway-global) rather than a fixed
/// purpose — the user composes what to put there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// Sharing scope: `session` (per-session private), `workspace` (shared across
    /// sessions in this workspace), or `gateway` (global). Resolved to a host
    /// root in the registry, which owns the ids and gateway root.
    pub anchor: String,
    /// Relative subpath under the anchor root. Absent = the anchor root itself.
    /// A `..` escape is rejected at resolution (fail-loud).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Absolute mount point inside the guest.
    pub guest: String,
    /// Mount read-only. Defaults to `false` (read-write).
    #[serde(default)]
    pub ro: bool,
}

/// Loads and removes per-workspace configs under a gateway directory.
///
/// The directory is `<gateway_workspace>/.omini/workspaces/` — the same family
/// as `workspaces.json` (`super::workspace`), so per-workspace server-side state
/// lives in one trusted place.
#[derive(Debug, Clone)]
pub struct WorkspaceConfigStore {
    /// Directory holding `<workspace_id>.toml` files.
    dir: PathBuf,
}

impl WorkspaceConfigStore {
    /// Build a store rooted at `dir` (`<gateway>/.omini/workspaces/`). The
    /// directory is created lazily on first write; a missing directory reads as
    /// "no configs".
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Path of the config file for a given workspace id.
    fn path_for_id(&self, id: &WorkspaceId) -> PathBuf {
        self.dir.join(format!("{}.toml", id.0))
    }

    /// Load the config for `workspace_path`, or `Ok(None)` if none exists.
    ///
    /// The path is canonicalized before hashing so it matches the id derived at
    /// session-create time. A canonicalization failure (the workspace path does
    /// not exist) means there can be no live session for it either, so it is
    /// treated as "no config" rather than an error.
    ///
    /// # Errors
    /// A present-but-malformed file fails loud with its path — a broken policy
    /// must not silently fall through to a weaker default (Karpathy §12).
    pub fn load(&self, workspace_path: &Path) -> Result<Option<WorkspaceConfig>, ConfigError> {
        let Ok(canonical) = std::fs::canonicalize(workspace_path) else {
            return Ok(None);
        };
        let id = WorkspaceId::from_path(&canonical);
        let path = self.path_for_id(&id);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        let config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        Ok(Some(config))
    }

    /// List configs whose workspace path no longer resolves — orphans a human or
    /// ops tool can then explicitly [`delete`](Self::delete).
    ///
    /// `registry` supplies the `id → path` reverse lookup so an orphan can be
    /// shown with the path it *was* for (best-effort — `None` if the id predates
    /// the registry seed). An id is orphaned when its recorded path fails to
    /// canonicalize (gone), or when the registry has no path for it at all.
    ///
    /// Read-only: this never deletes. A missing/unreadable directory yields an
    /// empty list.
    #[must_use]
    pub fn list_orphans(&self, registry: &WorkspaceRegistry) -> Vec<(WorkspaceId, Option<PathBuf>)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut orphans = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let id = WorkspaceId(stem.to_owned());
            let known_path = registry.path_for(&id);
            let alive = known_path
                .as_deref()
                .is_some_and(|p| std::fs::canonicalize(p).is_ok());
            if !alive {
                orphans.push((id, known_path));
            }
        }
        orphans
    }

    /// Delete the config for `id`. Idempotent: a missing file is `Ok(())`.
    ///
    /// # Errors
    /// An io error other than "not found" while removing the file.
    pub fn delete(&self, id: &WorkspaceId) -> Result<(), ConfigError> {
        let path = self.path_for_id(id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ConfigError::Io { path, source }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Write a config file for `workspace_path`'s id into `dir`.
    fn write_config(dir: &Path, workspace_path: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let canonical = std::fs::canonicalize(workspace_path).unwrap();
        let id = WorkspaceId::from_path(&canonical);
        std::fs::write(dir.join(format!("{}.toml", id.0)), body).unwrap();
    }

    #[test]
    fn load_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let store = WorkspaceConfigStore::new(tmp.path().join("workspaces"));
        assert_eq!(store.load(&ws).unwrap(), None);
    }

    #[test]
    fn load_reads_network_section() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = tmp.path().join("workspaces");
        write_config(
            &dir,
            &ws,
            "[network]\npolicy = \"isolated\"\n",
        );
        let store = WorkspaceConfigStore::new(dir);
        let cfg = store.load(&ws).unwrap().unwrap();
        assert_eq!(cfg.network.unwrap().policy.as_deref(), Some("isolated"));
    }

    #[test]
    fn load_reads_mounts_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = tmp.path().join("workspaces");
        // Two mounts: one full spec, one minimal (path absent, ro defaults false).
        write_config(
            &dir,
            &ws,
            "[[mounts]]\nanchor = \"workspace\"\npath = \"cache\"\nguest = \"/cache\"\nro = true\n\n\
             [[mounts]]\nanchor = \"session\"\nguest = \"/work\"\n",
        );
        let store = WorkspaceConfigStore::new(dir);
        let cfg = store.load(&ws).unwrap().unwrap();
        assert_eq!(cfg.mounts.len(), 2);
        assert_eq!(cfg.mounts[0].anchor, "workspace");
        assert_eq!(cfg.mounts[0].path.as_deref(), Some("cache"));
        assert!(cfg.mounts[0].ro);
        // Minimal entry: path absent, ro defaults to read-write.
        assert_eq!(cfg.mounts[1].anchor, "session");
        assert_eq!(cfg.mounts[1].path, None);
        assert!(!cfg.mounts[1].ro);
    }

    #[test]
    fn load_fails_loud_on_malformed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = tmp.path().join("workspaces");
        write_config(&dir, &ws, "this is = not valid toml ][");
        let store = WorkspaceConfigStore::new(dir);
        assert!(store.load(&ws).is_err());
    }

    #[test]
    fn orphan_is_a_config_whose_path_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        let dead = tmp.path().join("dead");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&dead).unwrap();
        let dir = tmp.path().join("workspaces");
        write_config(&dir, &live, "[network]\npolicy = \"open\"\n");
        write_config(&dir, &dead, "[network]\npolicy = \"open\"\n");

        // Seed the registry with both paths, then remove one on disk.
        let mut registry = WorkspaceRegistry::load(tmp.path().join("workspaces.json"));
        registry.record(&live).unwrap();
        let dead_canonical = std::fs::canonicalize(&dead).unwrap();
        registry.record(&dead).unwrap();
        std::fs::remove_dir_all(&dead).unwrap();

        let store = WorkspaceConfigStore::new(dir);
        let orphans = store.list_orphans(&registry);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].0, WorkspaceId::from_path(&dead_canonical));
    }

    #[test]
    fn delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = tmp.path().join("workspaces");
        write_config(&dir, &ws, "[network]\npolicy = \"open\"\n");
        let canonical = std::fs::canonicalize(&ws).unwrap();
        let id = WorkspaceId::from_path(&canonical);
        let store = WorkspaceConfigStore::new(dir);

        store.delete(&id).unwrap();
        assert_eq!(store.load(&ws).unwrap(), None);
        // Second delete on the now-absent file is still Ok.
        store.delete(&id).unwrap();
    }
}
