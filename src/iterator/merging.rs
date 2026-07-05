//! A merging iterator that folds several sorted cursors into one.
//!
//! Given N child [`KvIterator`]s, each individually sorted in internal-key
//! order, [`MergingIterator`] yields their union in that same order. This is the
//! backbone of both range scans (merge the memtable with every relevant table)
//! and compaction (merge the inputs, then drop shadowed versions).
//!
//! Selection is a linear scan over the children on each step. The child count is
//! small — a handful of memtables plus the tables overlapping a key range — so a
//! linear scan is simpler than a heap and its constant factors win at this size.
//! When two children expose the same internal key (which the engine's unique
//! sequence numbers make rare), the lower-indexed child is preferred, giving a
//! deterministic order.

use crate::error::Result;
use crate::iterator::KvIterator;
use crate::util::comparator::{Comparator, InternalKeyComparator};

/// Merges several internal-key-ordered child iterators into one sorted stream.
pub struct MergingIterator {
    children: Vec<Box<dyn KvIterator>>,
    current: Option<usize>,
    cmp: InternalKeyComparator,
}

impl MergingIterator {
    /// Builds a merging iterator over `children`. Each child must yield keys in
    /// internal-key order.
    pub fn new(children: Vec<Box<dyn KvIterator>>) -> MergingIterator {
        MergingIterator {
            children,
            current: None,
            cmp: InternalKeyComparator::new(),
        }
    }

    /// Number of child iterators.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Finds the valid child with the smallest current key.
    fn find_smallest(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, child) in self.children.iter().enumerate() {
            if !child.valid() {
                continue;
            }
            match best {
                None => best = Some(i),
                Some(b) => {
                    if self.cmp.compare(child.key(), self.children[b].key())
                        == std::cmp::Ordering::Less
                    {
                        best = Some(i);
                    }
                }
            }
        }
        best
    }
}

impl KvIterator for MergingIterator {
    fn valid(&self) -> bool {
        self.current.is_some()
    }

    fn seek_to_first(&mut self) {
        for child in &mut self.children {
            child.seek_to_first();
        }
        self.current = self.find_smallest();
    }

    fn seek(&mut self, target: &[u8]) {
        for child in &mut self.children {
            child.seek(target);
        }
        self.current = self.find_smallest();
    }

    fn next(&mut self) {
        if let Some(i) = self.current {
            self.children[i].next();
            self.current = self.find_smallest();
        }
    }

    fn key(&self) -> &[u8] {
        match self.current {
            Some(i) => self.children[i].key(),
            None => &[],
        }
    }

    fn value(&self) -> &[u8] {
        match self.current {
            Some(i) => self.children[i].value(),
            None => &[],
        }
    }

    fn status(&self) -> Result<()> {
        for child in &self.children {
            child.status()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterator::{drain_to_vec, VecIterator};
    use crate::key::{InternalKey, ValueType};

    fn ik(user: &[u8], seq: u64) -> Vec<u8> {
        InternalKey::new(user, seq, ValueType::Value).into_bytes()
    }

    #[test]
    fn merges_two_sorted_streams() {
        // Internal keys sort by user key asc, then seq desc.
        let a = VecIterator::new(vec![
            (ik(b"a", 5), b"va".to_vec()),
            (ik(b"c", 5), b"vc".to_vec()),
        ]);
        let b = VecIterator::new(vec![
            (ik(b"b", 5), b"vb".to_vec()),
            (ik(b"d", 5), b"vd".to_vec()),
        ]);
        let mut m = MergingIterator::new(vec![Box::new(a), Box::new(b)]);
        m.seek_to_first();
        let out = drain_to_vec(&mut m);
        let users: Vec<Vec<u8>> = out.iter().map(|(k, _)| k[..k.len() - 8].to_vec()).collect();
        assert_eq!(
            users,
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn seek_positions_all_children() {
        let a = VecIterator::new(vec![
            (ik(b"a", 1), b"1".to_vec()),
            (ik(b"e", 1), b"5".to_vec()),
        ]);
        let b = VecIterator::new(vec![(ik(b"c", 1), b"3".to_vec())]);
        let mut m = MergingIterator::new(vec![Box::new(a), Box::new(b)]);
        m.seek(&ik(b"b", 1));
        assert!(m.valid());
        assert_eq!(&m.key()[..1], b"c");
    }

    #[test]
    fn same_key_prefers_lower_child() {
        // Same user key, same seq: child 0 should win deterministically.
        let a = VecIterator::new(vec![(ik(b"k", 9), b"from-a".to_vec())]);
        let b = VecIterator::new(vec![(ik(b"k", 9), b"from-b".to_vec())]);
        let mut m = MergingIterator::new(vec![Box::new(a), Box::new(b)]);
        m.seek_to_first();
        assert_eq!(m.value(), b"from-a");
    }

    #[test]
    fn empty_children_yield_nothing() {
        let mut m = MergingIterator::new(vec![]);
        m.seek_to_first();
        assert!(!m.valid());
    }
}
