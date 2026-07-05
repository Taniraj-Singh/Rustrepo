//! The user-facing scan iterator.
//!
//! Where [`crate::iterator`] cursors range over *internal* keys (every version
//! of every key), a [`DbIterator`] presents the logical, user-visible view at a
//! fixed sequence number: for each user key it yields the single newest version
//! visible at that sequence, and it hides keys whose newest visible version is a
//! tombstone.
//!
//! It is built on a [`MergingIterator`] that unions the memtable with every
//! relevant table. Because internal keys sort by user key ascending then
//! sequence descending, the first entry the merge produces for a user key at or
//! below the snapshot sequence is exactly the version to surface.

use crate::error::Result;
use crate::iterator::{KvIterator, MergingIterator};
use crate::key::{InternalKey, ParsedInternalKey, Sequence, ValueType, VALUE_TYPE_FOR_SEEK};

/// A forward cursor over the logical key/value pairs visible at a sequence.
pub struct DbIterator {
    inner: MergingIterator,
    sequence: Sequence,
    current_key: Vec<u8>,
    current_value: Vec<u8>,
    valid: bool,
}

impl DbIterator {
    /// Wraps a merged internal-key cursor, presenting entries visible at `seq`.
    pub fn new(inner: MergingIterator, seq: Sequence) -> DbIterator {
        DbIterator {
            inner,
            sequence: seq,
            current_key: Vec::new(),
            current_value: Vec::new(),
            valid: false,
        }
    }

    /// Whether positioned on a visible entry.
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// The current user key.
    pub fn key(&self) -> &[u8] {
        &self.current_key
    }

    /// The current value.
    pub fn value(&self) -> &[u8] {
        &self.current_value
    }

    /// Any error surfaced by the underlying merge.
    pub fn status(&self) -> Result<()> {
        self.inner.status()
    }

    /// Positions at the first visible entry.
    pub fn seek_to_first(&mut self) {
        self.inner.seek_to_first();
        self.find_next_visible();
    }

    /// Positions at the first visible entry whose user key is `>= target`.
    pub fn seek(&mut self, target: &[u8]) {
        let ikey = InternalKey::new(target, self.sequence, VALUE_TYPE_FOR_SEEK).into_bytes();
        self.inner.seek(&ikey);
        self.find_next_visible();
    }

    /// Advances to the next visible entry.
    pub fn next(&mut self) {
        // `process_user_key` already consumed every version of the last key.
        self.find_next_visible();
    }

    /// Scans forward until a user key with a visible value is found.
    fn find_next_visible(&mut self) {
        loop {
            if !self.inner.valid() {
                self.valid = false;
                return;
            }
            if let Some((key, value)) = self.process_user_key() {
                self.current_key = key;
                self.current_value = value;
                self.valid = true;
                return;
            }
            // Otherwise the whole user key was invisible or deleted; keep going —
            // the merge cursor has already advanced past it.
        }
    }

    /// Consumes every version of the user key at the cursor, returning its value
    /// if the newest version visible at the snapshot is a live value.
    fn process_user_key(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        // Safe: caller checked `inner.valid()`.
        let user_key = match ParsedInternalKey::parse(self.inner.key()) {
            Ok(p) => p.user_key.to_vec(),
            Err(_) => {
                // Corrupt key: skip it to avoid a stuck cursor. The error is
                // still observable through `status()`.
                self.inner.next();
                return None;
            }
        };

        let mut result: Option<(Vec<u8>, Vec<u8>)> = None;
        let mut decided = false;

        while self.inner.valid() {
            let parsed = match ParsedInternalKey::parse(self.inner.key()) {
                Ok(p) => p,
                Err(_) => {
                    self.inner.next();
                    continue;
                }
            };
            if parsed.user_key != user_key.as_slice() {
                break;
            }
            if !decided && parsed.sequence <= self.sequence {
                decided = true;
                if parsed.value_type == ValueType::Value {
                    result = Some((user_key.clone(), self.inner.value().to_vec()));
                }
                // A deletion leaves `result` as None: the key is hidden.
            }
            self.inner.next();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterator::VecIterator;

    fn ik(user: &[u8], seq: u64, t: ValueType) -> Vec<u8> {
        InternalKey::new(user, seq, t).into_bytes()
    }

    fn db_iter(entries: Vec<(Vec<u8>, Vec<u8>)>, seq: u64) -> DbIterator {
        let child = VecIterator::new(entries);
        DbIterator::new(MergingIterator::new(vec![Box::new(child)]), seq)
    }

    #[test]
    fn hides_older_versions() {
        // Two versions of "k"; internal order puts seq 2 before seq 1.
        let entries = vec![
            (ik(b"k", 2, ValueType::Value), b"new".to_vec()),
            (ik(b"k", 1, ValueType::Value), b"old".to_vec()),
        ];
        let mut it = db_iter(entries, 100);
        it.seek_to_first();
        assert!(it.valid());
        assert_eq!(it.key(), b"k");
        assert_eq!(it.value(), b"new");
        it.next();
        assert!(!it.valid());
    }

    #[test]
    fn respects_snapshot_sequence() {
        let entries = vec![
            (ik(b"k", 5, ValueType::Value), b"v5".to_vec()),
            (ik(b"k", 2, ValueType::Value), b"v2".to_vec()),
        ];
        // At sequence 3, only v2 is visible.
        let mut it = db_iter(entries, 3);
        it.seek_to_first();
        assert_eq!(it.value(), b"v2");
    }

    #[test]
    fn tombstones_hide_keys() {
        let entries = vec![
            (ik(b"a", 1, ValueType::Value), b"1".to_vec()),
            (ik(b"b", 3, ValueType::Deletion), b"".to_vec()),
            (ik(b"b", 2, ValueType::Value), b"old".to_vec()),
            (ik(b"c", 1, ValueType::Value), b"3".to_vec()),
        ];
        let mut it = db_iter(entries, 100);
        it.seek_to_first();
        let mut keys = Vec::new();
        while it.valid() {
            keys.push(it.key().to_vec());
            it.next();
        }
        assert_eq!(keys, vec![b"a".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn seek_to_user_key() {
        let entries = vec![
            (ik(b"a", 1, ValueType::Value), b"1".to_vec()),
            (ik(b"m", 1, ValueType::Value), b"2".to_vec()),
            (ik(b"z", 1, ValueType::Value), b"3".to_vec()),
        ];
        let mut it = db_iter(entries, 100);
        it.seek(b"b");
        assert!(it.valid());
        assert_eq!(it.key(), b"m");
    }
}
