//! SQLite-backed store for provider API keys.
//!
//! Keys the user enters in the Web settings UI land here — a single `SQLite`
//! file at the gateway's primary config root (`<root>/secrets.db`) — **not** in
//! `providers.toml` (git-tracked) and **not** in the process environment. The
//! agent's `shell`/MCP subprocesses inherit the process environment, so a key
//! placed there is readable via `env`; keeping it in this store (read only at
//! model-resolution time, never exported to a subprocess overlay) is what keeps
//! it out of the model's reach.
//!
//! At-rest encryption is intentionally not done here (`doc/design/runtime-architecture.md`
//! secret model): transport is protected by HTTPS and access by the planned
//! login layer. The store lives at the config root, not the session workspace,
//! so it is outside the directory the agent's tools operate in.
//!
//! One connection is opened per operation. `Connection` is neither `Clone` nor
//! `Sync`, while [`crate::config::ConfigStore`] (which resolves keys) is cloned
//! across sessions; opening a fresh connection to a local file per call is cheap
//! and sidesteps sharing a connection across threads.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// The store's own error, mapped into [`crate::config::ConfigError::Secret`] at
/// the resolve boundary.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The `SQLite` database could not be opened, migrated, or queried.
    #[error("secret store error at {path}: {source}")]
    Db {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

type Result<T> = std::result::Result<T, SecretError>;

/// A handle to the provider-secret database at a fixed path.
///
/// Cheap to construct and clone (it only holds the path); each method opens its
/// own connection. Construction does no IO — the file and schema are created
/// lazily on the first write ([`set`](Self::set)).
#[derive(Debug, Clone)]
pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    /// The database file name under the config root.
    pub const FILE_NAME: &'static str = "secrets.db";

    /// A store over `<root>/secrets.db` for a config root (the `.omini` dir).
    #[must_use]
    pub fn at_root(root: &Path) -> Self {
        Self {
            path: root.join(Self::FILE_NAME),
        }
    }

    /// A store over an explicit database path (mainly for tests).
    #[must_use]
    pub const fn at_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// The database path this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a connection with the schema applied. Creates the file (and parent
    /// directory) if absent, so the first `set` on a fresh root just works.
    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SecretError::Db {
                path: self.path.clone(),
                source: rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(e.to_string()),
                ),
            })?;
        }
        let conn = Connection::open(&self.path).map_err(|e| self.db_err(e))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS secrets (\
                 provider TEXT PRIMARY KEY NOT NULL, \
                 api_key  TEXT NOT NULL\
             )",
            [],
        )
        .map_err(|e| self.db_err(e))?;
        Ok(conn)
    }

    /// Open the database read-only *without* creating it. Returns `None` when the
    /// file does not exist yet — the common case on a root that has never had a
    /// key set, where a read must not create an empty database as a side effect.
    fn open_existing(&self) -> Result<Option<Connection>> {
        if !self.path.exists() {
            return Ok(None);
        }
        Ok(Some(self.open()?))
    }

    /// The API key stored for `provider`, or `None` if none is set (or the
    /// database does not exist yet). Read-only: never creates the database.
    ///
    /// # Errors
    /// [`SecretError::Db`] if an existing database cannot be opened or queried.
    pub fn get(&self, provider: &str) -> Result<Option<String>> {
        let Some(conn) = self.open_existing()? else {
            return Ok(None);
        };
        conn.query_row(
            "SELECT api_key FROM secrets WHERE provider = ?1",
            [provider],
            |row| row.get::<_, String>(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(self.db_err(other)),
        })
    }

    /// Store (insert or replace) the API key for `provider`. Creates the
    /// database on first use.
    ///
    /// # Errors
    /// [`SecretError::Db`] if the database cannot be opened or written.
    pub fn set(&self, provider: &str, api_key: &str) -> Result<()> {
        let conn = self.open()?;
        conn.execute(
            "INSERT INTO secrets (provider, api_key) VALUES (?1, ?2) \
             ON CONFLICT(provider) DO UPDATE SET api_key = excluded.api_key",
            [provider, api_key],
        )
        .map_err(|e| self.db_err(e))?;
        Ok(())
    }

    /// Remove any stored key for `provider`. Returns `true` if a row was
    /// deleted. A no-op (and `false`) when the database does not exist.
    ///
    /// # Errors
    /// [`SecretError::Db`] if an existing database cannot be opened or written.
    pub fn delete(&self, provider: &str) -> Result<bool> {
        let Some(conn) = self.open_existing()? else {
            return Ok(false);
        };
        let n = conn
            .execute("DELETE FROM secrets WHERE provider = ?1", [provider])
            .map_err(|e| self.db_err(e))?;
        Ok(n > 0)
    }

    /// The provider names that have a key stored, sorted. Empty when the
    /// database does not exist. Never returns the key values — the settings UI
    /// shows only *whether* a provider is configured.
    ///
    /// # Errors
    /// [`SecretError::Db`] if an existing database cannot be opened or queried.
    pub fn list_names(&self) -> Result<Vec<String>> {
        let Some(conn) = self.open_existing()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn
            .prepare("SELECT provider FROM secrets ORDER BY provider")
            .map_err(|e| self.db_err(e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| self.db_err(e))?;
        let mut names = Vec::new();
        for row in rows {
            names.push(row.map_err(|e| self.db_err(e))?);
        }
        Ok(names)
    }

    fn db_err(&self, source: rusqlite::Error) -> SecretError {
        SecretError::Db {
            path: self.path.clone(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn temp_store() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::at_root(dir.path());
        (dir, store)
    }

    /// A read on a never-written root does not create the database file and
    /// returns `None` — so merely resolving a model on an env-var-only setup
    /// leaves no stray `secrets.db` behind.
    #[test]
    fn get_on_absent_db_returns_none_without_creating() {
        let (_d, store) = temp_store();
        assert_eq!(store.get("openai-main").unwrap(), None);
        assert!(!store.path().exists(), "read must not create the database");
    }

    /// `set` then `get` round-trips the key; a second `set` for the same
    /// provider overwrites (upsert), not duplicates.
    #[test]
    fn set_get_and_overwrite() {
        let (_d, store) = temp_store();
        store.set("openai-main", "sk-first").unwrap();
        assert_eq!(
            store.get("openai-main").unwrap().as_deref(),
            Some("sk-first")
        );
        store.set("openai-main", "sk-second").unwrap();
        assert_eq!(
            store.get("openai-main").unwrap().as_deref(),
            Some("sk-second")
        );
        assert_eq!(store.list_names().unwrap(), vec!["openai-main".to_owned()]);
    }

    /// `delete` reports whether a row existed and makes the key unresolvable.
    #[test]
    fn delete_reports_and_removes() {
        let (_d, store) = temp_store();
        store.set("p", "k").unwrap();
        assert!(store.delete("p").unwrap(), "existing key deletion is true");
        assert_eq!(store.get("p").unwrap(), None);
        assert!(!store.delete("p").unwrap(), "second delete finds nothing");
    }

    /// `list_names` returns only the provider names, sorted, never the values.
    #[test]
    fn list_names_sorted_no_values() {
        let (_d, store) = temp_store();
        store.set("zeta", "k1").unwrap();
        store.set("alpha", "k2").unwrap();
        assert_eq!(store.list_names().unwrap(), vec!["alpha", "zeta"]);
    }
}
