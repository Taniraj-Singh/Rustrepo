//! Key ordering.
//!
//! A [`Comparator`] defines a total order over byte-string keys. The engine is
//! generic over user ordering through this trait, though the default —
//! [`BytewiseComparator`] — is unsigned lexicographic order, which is what
//! almost everyone wants.
//!
//! Internally the engine never orders *user* keys directly; it orders *internal*
//! keys (user key + sequence + type) via [`InternalKeyComparator`], which layers
//! the MVCC tie-break on top of the user order: equal user keys sort by
//! descending sequence number so the newest version of a key is encountered
//! first during a scan.

use std::cmp::Ordering;
use std::fmt;

use crate::util::bytes::common_prefix_len;
use crate::util::coding::get_fixed64;

/// A total order over keys, plus two optional "key shortening" hooks used to
/// keep index entries small.
pub trait Comparator: fmt::Debug + Send + Sync {
    /// Orders `a` relative to `b`.
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering;

    /// A stable name for this ordering. Recorded in table metadata so a table
    /// written with one comparator is never read back with an incompatible one.
    fn name(&self) -> &'static str;

    /// Replaces `start` with a short key in `[start, limit)` if possible.
    ///
    /// Used when building an index: the separator between two data blocks need
    /// only be *some* key that is `>=` the last key of the earlier block and
    /// `<` the first key of the later block. A shorter separator shrinks the
    /// index. The default leaves `start` unchanged, which is always valid.
    fn find_shortest_separator(&self, _start: &mut Vec<u8>, _limit: &[u8]) {}

    /// Replaces `key` with a short key that is `>= key`. Used for the final
    /// index entry, which has no successor block to bound it. The default is a
    /// no-op.
    fn find_short_successor(&self, _key: &mut Vec<u8>) {}
}

/// Unsigned lexicographic ("memcmp") ordering of raw bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct BytewiseComparator;

impl Comparator for BytewiseComparator {
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    fn name(&self) -> &'static str {
        "cairn.BytewiseComparator"
    }

    fn find_shortest_separator(&self, start: &mut Vec<u8>, limit: &[u8]) {
        let shared = common_prefix_len(start, limit);
        if shared >= start.len().min(limit.len()) {
            // One is a prefix of the other; no shorter separator exists.
            return;
        }
        // Try to bump the first differing byte of `start` by one, as long as it
        // stays strictly below the corresponding byte of `limit`.
        let diff = shared;
        let byte = start[diff];
        if byte < 0xff && byte + 1 < limit[diff] {
            start[diff] += 1;
            start.truncate(diff + 1);
        }
    }

    fn find_short_successor(&self, key: &mut Vec<u8>) {
        // Find the first byte that can be incremented, drop the tail.
        for i in 0..key.len() {
            if key[i] != 0xff {
                key[i] += 1;
                key.truncate(i + 1);
                return;
            }
        }
        // All 0xff: `key` is already the largest key of its length; leave it.
    }
}

/// Orders internal keys: user order first, then *descending* trailer (sequence
/// and type packed into the last eight bytes) so newer versions sort earlier.
#[derive(Debug, Clone, Copy, Default)]
pub struct InternalKeyComparator {
    user: BytewiseComparator,
}

impl InternalKeyComparator {
    /// Creates an internal comparator over the default bytewise user order.
    pub fn new() -> Self {
        InternalKeyComparator {
            user: BytewiseComparator,
        }
    }

    /// The wrapped user comparator (compares user keys only).
    pub fn user_comparator(&self) -> &BytewiseComparator {
        &self.user
    }
}

impl Comparator for InternalKeyComparator {
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        // Defensive: a well-formed internal key is at least 8 bytes. If we are
        // ever handed a short key, fall back to a raw comparison rather than
        // slicing out of bounds.
        if a.len() < 8 || b.len() < 8 {
            return a.cmp(b);
        }
        let (a_user, a_tail) = a.split_at(a.len() - 8);
        let (b_user, b_tail) = b.split_at(b.len() - 8);
        match self.user.compare(a_user, b_user) {
            Ordering::Equal => {
                // Larger trailer (newer) should compare Less (sort first).
                let a_num = get_fixed64(a_tail).unwrap_or(0);
                let b_num = get_fixed64(b_tail).unwrap_or(0);
                b_num.cmp(&a_num)
            }
            other => other,
        }
    }

    fn name(&self) -> &'static str {
        "cairn.InternalKeyComparator"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{pack_trailer, ValueType};

    #[test]
    fn bytewise_orders_like_slices() {
        let c = BytewiseComparator;
        assert_eq!(c.compare(b"a", b"b"), Ordering::Less);
        assert_eq!(c.compare(b"abc", b"ab"), Ordering::Greater);
        assert_eq!(c.compare(b"x", b"x"), Ordering::Equal);
    }

    #[test]
    fn shortest_separator_shrinks() {
        let c = BytewiseComparator;
        let mut start = b"helloworld".to_vec();
        c.find_shortest_separator(&mut start, b"hellozzz");
        // Should shorten to something >= "helloworld" and < "hellozzz".
        assert!(c.compare(&start, b"helloworld") != Ordering::Less);
        assert!(c.compare(&start, b"hellozzz") == Ordering::Less);
        assert!(start.len() < b"helloworld".len());
    }

    #[test]
    fn short_successor_shrinks() {
        let c = BytewiseComparator;
        let mut k = b"abz".to_vec();
        c.find_short_successor(&mut k);
        assert!(c.compare(&k, b"abz") != Ordering::Less);
        assert!(k.len() <= 3);
    }

    #[test]
    fn internal_order_is_user_then_desc_seq() {
        let ic = InternalKeyComparator::new();
        let mut older = b"key".to_vec();
        older.extend_from_slice(&pack_trailer(5, ValueType::Value).to_le_bytes());
        let mut newer = b"key".to_vec();
        newer.extend_from_slice(&pack_trailer(9, ValueType::Value).to_le_bytes());
        // Same user key, higher sequence sorts first (Less).
        assert_eq!(ic.compare(&newer, &older), Ordering::Less);

        let mut other = b"kez".to_vec();
        other.extend_from_slice(&pack_trailer(1, ValueType::Value).to_le_bytes());
        assert_eq!(ic.compare(&newer, &other), Ordering::Less);
    }
}
