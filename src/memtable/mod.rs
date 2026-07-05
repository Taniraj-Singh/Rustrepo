//! The in-memory write buffer.
//!
//! A [`MemTable`] is an ordered map from *internal key* to value that absorbs
//! writes before they are flushed to an SSTable. Because internal keys sort by
//! user key ascending and sequence descending, the newest version of a key is
//! always the first one at or after a lookup position — which is exactly what a
//! point read wants.
//!
//! The map is a [`BTreeMap`] keyed by a [`MemKey`] whose ordering delegates to
//! [`InternalKeyComparator`]. Writes are `O(log n)`; a read is a single range
//! probe. Iteration (for flushing and for range scans) takes an ordered snapshot
//! of the entries — the buffer is bounded by the configured write-buffer size,
//! so the snapshot is cheap relative to the work of flushing it.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::iterator::KvIterator;
use crate::key::{InternalKey, ParsedInternalKey, Sequence, ValueType, VALUE_TYPE_FOR_SEEK};
use crate::util::comparator::{Comparator, InternalKeyComparator};

/// Rough per-entry bookkeeping overhead added to the memory estimate.
const ENTRY_OVERHEAD: usize = 24;

/// A map key that orders by internal-key semantics rather than raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemKey(Vec<u8>);

impl Ord for MemKey {
    fn cmp(&self, other: &MemKey) -> Ordering {
        InternalKeyComparator::new().compare(&self.0, &other.0)
    }
}

impl PartialOrd for MemKey {
    fn partial_cmp(&self, other: &MemKey) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The outcome of a memtable lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupResult {
    /// The key is set to this value at or below the requested sequence.
    Found(Vec<u8>),
    /// The key is deleted (a tombstone shadows it) at or below the sequence.
    Deleted,
    /// The key does not appear in this memtable at all.
    NotFound,
}

/// An ordered, in-memory buffer of recent writes.
#[derive(Debug, Default)]
pub struct MemTable {
    map: BTreeMap<MemKey, Vec<u8>>,
    memory_usage: usize,
}

impl MemTable {
    /// Creates an empty memtable.
    pub fn new() -> MemTable {
        MemTable::default()
    }

    /// Number of entries (distinct internal keys).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the memtable holds no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Approximate heap footprint, used to decide when to flush.
    pub fn approximate_memory_usage(&self) -> usize {
        self.memory_usage
    }

    /// Inserts a versioned mutation.
    ///
    /// `value` is ignored for [`ValueType::Deletion`] (tombstones carry no
    /// value). Each call uses a unique sequence number, so entries never
    /// collide.
    pub fn add(&mut self, seq: Sequence, value_type: ValueType, user_key: &[u8], value: &[u8]) {
        let internal_key = InternalKey::new(user_key, seq, value_type).into_bytes();
        let stored_value = match value_type {
            ValueType::Value => value.to_vec(),
            ValueType::Deletion => Vec::new(),
        };
        self.memory_usage += internal_key.len() + stored_value.len() + ENTRY_OVERHEAD;
        self.map.insert(MemKey(internal_key), stored_value);
    }

    /// Looks up `user_key` as visible at `seq`.
    ///
    /// Returns the newest version whose sequence is `<= seq`: a value, a
    /// tombstone, or absence.
    pub fn get(&self, user_key: &[u8], seq: Sequence) -> LookupResult {
        let lookup = InternalKey::new(user_key, seq, VALUE_TYPE_FOR_SEEK).into_bytes();
        // The first entry >= lookup is the newest version at or below `seq`.
        if let Some((key, value)) = self.map.range(MemKey(lookup)..).next() {
            if let Ok(parsed) = ParsedInternalKey::parse(&key.0) {
                if parsed.user_key == user_key {
                    return match parsed.value_type {
                        ValueType::Value => LookupResult::Found(value.clone()),
                        ValueType::Deletion => LookupResult::Deleted,
                    };
                }
            }
        }
        LookupResult::NotFound
    }

    /// Takes an ordered snapshot iterator over `(internal key, value)` pairs.
    pub fn iter(&self) -> MemTableIter {
        let entries: Vec<(Vec<u8>, Vec<u8>)> = self
            .map
            .iter()
            .map(|(k, v)| (k.0.clone(), v.clone()))
            .collect();
        MemTableIter {
            entries,
            pos: usize::MAX,
            cmp: InternalKeyComparator::new(),
        }
    }
}

/// A cursor over a memtable snapshot, in internal-key order.
#[derive(Debug)]
pub struct MemTableIter {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos: usize,
    cmp: InternalKeyComparator,
}

impl KvIterator for MemTableIter {
    fn valid(&self) -> bool {
        self.pos < self.entries.len()
    }

    fn seek_to_first(&mut self) {
        self.pos = 0;
    }

    fn seek(&mut self, target: &[u8]) {
        // First index whose internal key is >= target, by internal ordering.
        self.pos = self
            .entries
            .partition_point(|(k, _)| self.cmp.compare(k, target) == Ordering::Less);
    }

    fn next(&mut self) {
        if self.pos < self.entries.len() {
            self.pos += 1;
        }
    }

    fn key(&self) -> &[u8] {
        &self.entries[self.pos].0
    }

    fn value(&self) -> &[u8] {
        &self.entries[self.pos].1
    }

    fn status(&self) -> crate::error::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterator::drain_to_vec;

    #[test]
    fn newest_version_wins() {
        let mut m = MemTable::new();
        m.add(1, ValueType::Value, b"k", b"v1");
        m.add(2, ValueType::Value, b"k", b"v2");
        // At seq >= 2, the newest value is v2.
        assert_eq!(m.get(b"k", 5), LookupResult::Found(b"v2".to_vec()));
        // At seq 1, only v1 is visible.
        assert_eq!(m.get(b"k", 1), LookupResult::Found(b"v1".to_vec()));
    }

    #[test]
    fn tombstone_hides_older_value() {
        let mut m = MemTable::new();
        m.add(1, ValueType::Value, b"k", b"v1");
        m.add(2, ValueType::Deletion, b"k", b"");
        assert_eq!(m.get(b"k", 5), LookupResult::Deleted);
        // Snapshot before the delete still sees the value.
        assert_eq!(m.get(b"k", 1), LookupResult::Found(b"v1".to_vec()));
    }

    #[test]
    fn absent_key_is_not_found() {
        let mut m = MemTable::new();
        m.add(1, ValueType::Value, b"a", b"1");
        assert_eq!(m.get(b"z", 5), LookupResult::NotFound);
    }

    #[test]
    fn iterates_in_internal_order() {
        let mut m = MemTable::new();
        m.add(1, ValueType::Value, b"b", b"1");
        m.add(2, ValueType::Value, b"a", b"2");
        m.add(3, ValueType::Value, b"a", b"3"); // newer version of a
        let mut it = m.iter();
        it.seek_to_first();
        let out = drain_to_vec(&mut it);
        // User keys ascending; for "a", the higher sequence (3) comes first.
        let users: Vec<Vec<u8>> = out.iter().map(|(k, _)| k[..1].to_vec()).collect();
        assert_eq!(users, vec![b"a".to_vec(), b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(out[0].1, b"3"); // seq 3 first
        assert_eq!(out[1].1, b"2");
    }

    #[test]
    fn seek_uses_internal_order() {
        let mut m = MemTable::new();
        for (i, k) in [b"a", b"c", b"e"].iter().enumerate() {
            m.add(i as u64 + 1, ValueType::Value, *k, b"x");
        }
        let mut it = m.iter();
        let target = InternalKey::new(b"b", 100, VALUE_TYPE_FOR_SEEK).into_bytes();
        it.seek(&target);
        assert!(it.valid());
        assert_eq!(&it.key()[..1], b"c");
    }

    #[test]
    fn memory_usage_grows() {
        let mut m = MemTable::new();
        assert_eq!(m.approximate_memory_usage(), 0);
        m.add(1, ValueType::Value, b"key", b"value");
        assert!(m.approximate_memory_usage() > 0);
    }
}
