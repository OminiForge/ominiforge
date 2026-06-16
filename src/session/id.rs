//! Session id generation.
//!
//! [`SessionId`] itself lives in `core` as a transparent string newtype so the
//! core protocol carries no id-generation dependency. The ULID scheme — and the
//! `ulid` crate — live here, at the storage layer that actually mints ids.

use crate::core::SessionId;

/// Mint a fresh session id: a ULID (time-sortable prefix + random suffix,
/// 26 Crockford-base32 chars), so `ls` lists session directories in creation
/// order. See `doc/session-storage.md` §1.
#[must_use]
pub fn generate() -> SessionId {
    SessionId(ulid::Ulid::new().to_string())
}

#[cfg(test)]
mod tests {
    use super::generate;

    #[test]
    fn generated_ids_are_26_char_and_unique() {
        let a = generate();
        let b = generate();
        assert_eq!(a.0.len(), 26);
        assert_ne!(a, b);
    }

    #[test]
    fn ids_sort_in_creation_order() {
        // ULID's millisecond timestamp prefix makes later ids sort later.
        let first = generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = generate();
        assert!(second.0 > first.0);
    }
}
