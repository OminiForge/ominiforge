//! Per-session file snapshots: the shared state behind anchored [`edit`] patches.
//!
//! `read` records a short fingerprint (`tag`) of each file it returns; `edit`
//! cites that tag and refuses to apply when it no longer matches the file on
//! disk. This catches the "patch built from a stale read" case — a formatter, a
//! concurrent agent, or a manual save changing the file between the read and the
//! edit — instead of clobbering the wrong line.
//!
//! The store is assembly-scoped session state, created once and shared (cheap
//! `Arc` clone) into both the `read` and `edit` tools. This does not break the
//! "stateless request/response" tool principle (`doc/tool-protocol.md` §11): it
//! is construction-time shared state, not per-call streaming state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Per-session file fingerprints shared between `read` (writer) and `edit` (reader).
///
/// Keyed by the workspace-resolved absolute path both tools compute via
/// `resolve_in_workspace`, so they agree on the key.
#[derive(Clone, Default, Debug)]
pub struct SnapshotStore {
    inner: Arc<Mutex<HashMap<PathBuf, String>>>,
}

impl SnapshotStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the current `tag` for `path`, replacing any prior entry. Called by
    /// `read` after a successful read and by `edit` after a successful write.
    pub fn record(&self, path: &Path, tag: String) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(path.to_path_buf(), tag);
        }
    }

    /// The tag last recorded for `path`, or `None` if it was never read this
    /// session. `edit` treats `None` as "you must read this file first".
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<String> {
        self.inner.lock().ok().and_then(|m| m.get(path).cloned())
    }
}

/// Fingerprint `bytes` as a four-hex-digit tag (e.g. `1F2A`).
///
/// FNV-1a 32-bit folded to its low 16 bits. This is a change detector, not a
/// cryptographic hash: it only has to differ when the file's bytes differ
/// between a `read` and a later `edit`. Kept dependency-free on purpose.
#[must_use]
pub fn tag_of(bytes: &[u8]) -> String {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Fold the 32-bit hash into 16 bits so the tag stays short but still mixes
    // the whole digest.
    let folded = (hash >> 16) ^ (hash & 0xffff);
    format!("{folded:04X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same bytes always fingerprint to the same tag — `edit` relies on this
    /// to recognise an unchanged file.
    #[test]
    fn tag_is_stable_for_identical_bytes() {
        assert_eq!(tag_of(b"hello world"), tag_of(b"hello world"));
    }

    /// A one-byte change must change the tag, or a stale patch could slip
    /// through the verification `edit` performs.
    #[test]
    fn tag_changes_when_bytes_change() {
        assert_ne!(tag_of(b"hello world"), tag_of(b"hello worlA"));
        assert_ne!(tag_of(b"abc"), tag_of(b"abc\n"));
    }

    /// A tag is always four uppercase hex digits.
    #[test]
    fn tag_is_four_hex_digits() {
        let tag = tag_of(b"anything");
        assert_eq!(tag.len(), 4);
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    /// `record` then `get` round-trips; an unrecorded path is `None`.
    #[test]
    fn store_records_and_reads_back() {
        let store = SnapshotStore::new();
        let p = Path::new("/ws/a.rs");
        assert_eq!(store.get(p), None);
        store.record(p, "1F2A".to_owned());
        assert_eq!(store.get(p).as_deref(), Some("1F2A"));
    }

    /// A clone shares the same backing map (it is an `Arc` handle), so the store
    /// `read` writes is the store `edit` reads.
    #[test]
    fn clone_shares_state() {
        let a = SnapshotStore::new();
        let b = a.clone();
        a.record(Path::new("/ws/x"), "BEEF".to_owned());
        assert_eq!(b.get(Path::new("/ws/x")).as_deref(), Some("BEEF"));
    }
}
