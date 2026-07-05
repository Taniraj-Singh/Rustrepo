//! Point-in-time read views (MVCC snapshots).
//!
//! Every mutation is stamped with an increasing sequence number. A [`Snapshot`]
//! captures one such number; reads taken against it see only versions with a
//! sequence at or below it, giving a stable view even as concurrent writes and
//! compactions proceed.
//!
//! Snapshots also *protect* the versions they can see. Compaction consults the
//! [`SnapshotList`] for the oldest live snapshot and never drops a version newer
//! than it, so a snapshot always resolves to the data it was promised. A
//! `Snapshot` releases its protection automatically on drop.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::key::Sequence;

/// Reference-counted set of the sequence numbers pinned by live snapshots.
#[derive(Debug, Default)]
struct Inner {
    // sequence -> number of live snapshots at that sequence
    counts: BTreeMap<Sequence, usize>,
}

/// Tracks live snapshots so compaction knows which versions must be preserved.
#[derive(Debug, Clone, Default)]
pub struct SnapshotList {
    inner: Arc<Mutex<Inner>>,
}

impl SnapshotList {
    /// Creates an empty snapshot list.
    pub fn new() -> SnapshotList {
        SnapshotList::default()
    }

    /// Pins `sequence`, returning a snapshot that unpins it on drop.
    pub fn snapshot(&self, sequence: Sequence) -> Snapshot {
        {
            let mut inner = self.inner.lock().expect("snapshot list poisoned");
            *inner.counts.entry(sequence).or_insert(0) += 1;
        }
        Snapshot {
            sequence,
            list: Arc::clone(&self.inner),
        }
    }

    /// The oldest sequence any live snapshot depends on, if any.
    ///
    /// Compaction must not discard versions newer than this.
    pub fn oldest(&self) -> Option<Sequence> {
        let inner = self.inner.lock().expect("snapshot list poisoned");
        inner.counts.keys().next().copied()
    }

    /// Number of distinct pinned sequences (for tests and metrics).
    pub fn active_count(&self) -> usize {
        self.inner
            .lock()
            .expect("snapshot list poisoned")
            .counts
            .len()
    }
}

/// A stable read position at a particular sequence number.
///
/// Reads performed with this snapshot observe the database as of its sequence.
/// Dropping it releases the protection it holds over old versions.
#[derive(Debug)]
pub struct Snapshot {
    sequence: Sequence,
    list: Arc<Mutex<Inner>>,
}

impl Snapshot {
    /// The sequence number this snapshot reads at.
    pub fn sequence(&self) -> Sequence {
        self.sequence
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let mut inner = self.list.lock().expect("snapshot list poisoned");
        if let Some(count) = inner.counts.get_mut(&self.sequence) {
            *count -= 1;
            if *count == 0 {
                inner.counts.remove(&self.sequence);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_tracks_live_snapshots() {
        let list = SnapshotList::new();
        assert_eq!(list.oldest(), None);

        let s1 = list.snapshot(10);
        let s2 = list.snapshot(20);
        assert_eq!(list.oldest(), Some(10));
        assert_eq!(s1.sequence(), 10);
        assert_eq!(s2.sequence(), 20);

        drop(s1);
        assert_eq!(list.oldest(), Some(20));
        drop(s2);
        assert_eq!(list.oldest(), None);
    }

    #[test]
    fn refcounts_same_sequence() {
        let list = SnapshotList::new();
        let a = list.snapshot(5);
        let b = list.snapshot(5);
        assert_eq!(list.active_count(), 1);
        drop(a);
        // Still pinned by `b`.
        assert_eq!(list.oldest(), Some(5));
        drop(b);
        assert_eq!(list.oldest(), None);
    }
}
