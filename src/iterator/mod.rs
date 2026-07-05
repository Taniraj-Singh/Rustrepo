//! The iterator framework that unifies scanning over memtables and tables.
//!
//! Every ordered source in the engine — a memtable, a data block, a whole table,
//! a level, the entire database — exposes the same cursor-style [`KvIterator`]
//! interface. That uniformity is what lets a [`MergingIterator`] fold many
//! sources into one sorted stream, and what lets the compaction and scan code be
//! written once regardless of where the data lives.
//!
//! The interface is a *cursor*, not a Rust [`Iterator`]: it supports positioning
//! (`seek`, `seek_to_first`) as well as advancing, which point lookups and
//! merging both require. Keys returned are always *internal* keys (user key +
//! trailer); higher layers strip the trailer and apply MVCC visibility.
//!
//! Errors are surfaced out-of-band via [`KvIterator::status`]: a cursor that hits
//! a corrupt block becomes invalid and records the error, rather than each
//! `next`/`key` call returning a `Result`.

pub mod merging;

pub use merging::MergingIterator;

use crate::error::Result;

/// A forward, seekable cursor over sorted `(internal key, value)` pairs.
///
/// A fresh iterator is *not* positioned; call [`seek_to_first`](Self::seek_to_first)
/// or [`seek`](Self::seek) before reading. While [`valid`](Self::valid) is true,
/// [`key`](Self::key) and [`value`](Self::value) return the current entry.
pub trait KvIterator {
    /// Whether the cursor is positioned on a valid entry.
    fn valid(&self) -> bool;

    /// Positions the cursor at the first (smallest) entry.
    fn seek_to_first(&mut self);

    /// Positions the cursor at the first entry whose key is `>= target`.
    ///
    /// `target` is compared with the iterator's own key ordering (internal-key
    /// order for engine iterators).
    fn seek(&mut self, target: &[u8]);

    /// Advances to the next entry. Requires [`valid`](Self::valid).
    fn next(&mut self);

    /// The current entry's key. Only meaningful while [`valid`](Self::valid).
    fn key(&self) -> &[u8];

    /// The current entry's value. Only meaningful while [`valid`](Self::valid).
    fn value(&self) -> &[u8];

    /// Any error the cursor has encountered. `Ok(())` if healthy.
    fn status(&self) -> Result<()>;
}

/// Forwarding impl so a boxed iterator is itself a [`KvIterator`], letting
/// `Box<dyn KvIterator>` be stored, passed, and coerced uniformly.
impl KvIterator for Box<dyn KvIterator> {
    fn valid(&self) -> bool {
        (**self).valid()
    }
    fn seek_to_first(&mut self) {
        (**self).seek_to_first()
    }
    fn seek(&mut self, target: &[u8]) {
        (**self).seek(target)
    }
    fn next(&mut self) {
        (**self).next()
    }
    fn key(&self) -> &[u8] {
        (**self).key()
    }
    fn value(&self) -> &[u8] {
        (**self).value()
    }
    fn status(&self) -> Result<()> {
        (**self).status()
    }
}

/// An iterator that is always empty. Useful as a placeholder and in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyIterator;

impl KvIterator for EmptyIterator {
    fn valid(&self) -> bool {
        false
    }
    fn seek_to_first(&mut self) {}
    fn seek(&mut self, _target: &[u8]) {}
    fn next(&mut self) {}
    fn key(&self) -> &[u8] {
        &[]
    }
    fn value(&self) -> &[u8] {
        &[]
    }
    fn status(&self) -> Result<()> {
        Ok(())
    }
}

/// A simple in-memory iterator over a pre-sorted vector of `(key, value)` pairs.
///
/// Primarily a test double for exercising [`MergingIterator`] and higher layers
/// without building real tables, but also a clean reference implementation of
/// the [`KvIterator`] contract.
#[derive(Debug, Clone)]
pub struct VecIterator {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos: usize,
}

impl VecIterator {
    /// Builds an iterator over `entries`, which must already be sorted by key.
    pub fn new(entries: Vec<(Vec<u8>, Vec<u8>)>) -> VecIterator {
        VecIterator {
            entries,
            pos: usize::MAX,
        }
    }
}

impl KvIterator for VecIterator {
    fn valid(&self) -> bool {
        self.pos < self.entries.len()
    }

    fn seek_to_first(&mut self) {
        self.pos = 0;
    }

    fn seek(&mut self, target: &[u8]) {
        // First index whose key is >= target.
        self.pos = self.entries.partition_point(|(k, _)| k.as_slice() < target);
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

    fn status(&self) -> Result<()> {
        Ok(())
    }
}

/// Collects every `(key, value)` an iterator yields from its current position to
/// the end, as owned pairs. A convenience for tests.
pub fn drain_to_vec(it: &mut dyn KvIterator) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    while it.valid() {
        out.push((it.key().to_vec(), it.value().to_vec()));
        it.next();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_iterator_is_never_valid() {
        let mut it = EmptyIterator;
        it.seek_to_first();
        assert!(!it.valid());
        assert!(it.status().is_ok());
    }

    #[test]
    fn vec_iterator_scans_and_seeks() {
        let entries = vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
            (b"e".to_vec(), b"5".to_vec()),
        ];
        let mut it = VecIterator::new(entries);
        it.seek_to_first();
        assert!(it.valid());
        assert_eq!(it.key(), b"a");

        it.seek(b"b");
        assert_eq!(it.key(), b"c");
        it.seek(b"e");
        assert_eq!(it.key(), b"e");
        it.next();
        assert!(!it.valid());

        it.seek(b"z");
        assert!(!it.valid());
    }

    #[test]
    fn drain_collects_all() {
        let entries = vec![
            (b"x".to_vec(), b"1".to_vec()),
            (b"y".to_vec(), b"2".to_vec()),
        ];
        let mut it = VecIterator::new(entries.clone());
        it.seek_to_first();
        assert_eq!(drain_to_vec(&mut it), entries);
    }
}
