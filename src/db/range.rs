//! Bounded range and prefix scans.
//!
//! [`Cairn::scan`](crate::Cairn::scan) yields the whole keyspace; often a caller
//! wants only a slice of it. [`RangeScan`] wraps a [`DbIterator`] with an
//! optional exclusive upper bound and stops as soon as the cursor reaches it, so
//! `[start, end)` and prefix scans read no further than they must.

use crate::db::{Cairn, DbIterator};
use crate::error::Result;

/// A forward scan bounded above by an exclusive end key.
pub struct RangeScan {
    inner: DbIterator,
    // Exclusive upper bound; `None` scans to the end of the keyspace.
    upper: Option<Vec<u8>>,
}

impl RangeScan {
    fn new(inner: DbIterator, upper: Option<Vec<u8>>) -> RangeScan {
        RangeScan { inner, upper }
    }

    /// Whether the cursor is on a valid entry within the range.
    pub fn valid(&self) -> bool {
        if !self.inner.valid() {
            return false;
        }
        match &self.upper {
            Some(end) => self.inner.key() < end.as_slice(),
            None => true,
        }
    }

    /// The current key.
    pub fn key(&self) -> &[u8] {
        self.inner.key()
    }

    /// The current value.
    pub fn value(&self) -> &[u8] {
        self.inner.value()
    }

    /// Advances to the next key in range.
    pub fn next(&mut self) {
        self.inner.next();
    }

    /// Any error surfaced by the underlying scan.
    pub fn status(&self) -> Result<()> {
        self.inner.status()
    }

    /// Collects the remaining in-range entries into owned pairs.
    pub fn collect_pairs(mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        while self.valid() {
            out.push((self.key().to_vec(), self.value().to_vec()));
            self.next();
        }
        out
    }

    /// Counts the remaining in-range entries, consuming the scan.
    pub fn count(mut self) -> usize {
        let mut n = 0;
        while self.valid() {
            n += 1;
            self.next();
        }
        n
    }
}

/// Returns the shortest key that is strictly greater than every key with the
/// given prefix, or `None` if the prefix is all `0xff` (no upper bound exists).
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last != 0xff {
            *last += 1;
            return Some(end);
        }
        end.pop();
    }
    None
}

impl Cairn {
    /// Scans the half-open user-key range `[start, end)` at the latest sequence.
    pub fn range(&self, start: &[u8], end: &[u8]) -> Result<RangeScan> {
        let mut it = self.scan()?;
        it.seek(start);
        Ok(RangeScan::new(it, Some(end.to_vec())))
    }

    /// Scans from `start` to the end of the keyspace.
    pub fn range_from(&self, start: &[u8]) -> Result<RangeScan> {
        let mut it = self.scan()?;
        it.seek(start);
        Ok(RangeScan::new(it, None))
    }

    /// Scans every key beginning with `prefix`.
    pub fn prefix_scan(&self, prefix: &[u8]) -> Result<RangeScan> {
        let mut it = self.scan()?;
        it.seek(prefix);
        Ok(RangeScan::new(it, prefix_successor(prefix)))
    }
}

#[cfg(test)]
mod tests {
    use crate::{Cairn, Config};
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-range-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn populated(name: &str) -> (PathBuf, Cairn) {
        let dir = temp_dir(name);
        let db = Cairn::open(&dir, Config::default().write_buffer_size(8192)).unwrap();
        for i in 0..1000u32 {
            db.put(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
        db.put(b"other:1", b"x").unwrap();
        (dir, db)
    }

    #[test]
    fn half_open_range() {
        let (dir, db) = populated("half-open");
        let scan = db.range(b"k0100", b"k0200").unwrap();
        let pairs = scan.collect_pairs();
        assert_eq!(pairs.len(), 100);
        assert_eq!(pairs.first().unwrap().0, b"k0100");
        // Exclusive upper bound: k0200 is not included.
        assert_eq!(pairs.last().unwrap().0, b"k0199");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefix_scan_stops_at_boundary() {
        let (dir, db) = populated("prefix");
        // "k00" matches k0000..k0099.
        let count = db.prefix_scan(b"k00").unwrap().count();
        assert_eq!(count, 100);
        // A prefix matching nothing.
        assert_eq!(db.prefix_scan(b"zzz").unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn range_from_reads_to_end() {
        let (dir, db) = populated("from");
        let scan = db.range_from(b"k0995").unwrap();
        let pairs = scan.collect_pairs();
        // k0995..k0999 (5) plus the "other:1" key that sorts after.
        assert!(pairs.len() >= 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefix_successor_edges() {
        assert_eq!(super::prefix_successor(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(super::prefix_successor(&[0x61, 0xff]), Some(b"b".to_vec()));
        assert_eq!(super::prefix_successor(&[0xff, 0xff]), None);
    }
}
