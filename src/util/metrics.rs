//! Lightweight runtime counters.
//!
//! [`Metrics`] is a bag of monotonic atomic counters shared (behind an `Arc`)
//! by every component of a running database. Incrementing a counter is a single
//! relaxed atomic add, cheap enough to leave on in production. Call
//! [`Metrics::snapshot`] to read a consistent-enough copy for reporting.
//!
//! Counters are intentionally coarse; they answer "is the cache helping?" and
//! "how much compaction is happening?", not fine-grained latency questions.

use std::sync::atomic::{AtomicU64, Ordering};

/// A set of atomic counters describing engine activity since startup.
#[derive(Debug, Default)]
pub struct Metrics {
    /// `get` calls served.
    pub gets: AtomicU64,
    /// `put` operations applied (counts individual keys, not batches).
    pub puts: AtomicU64,
    /// `delete` operations applied.
    pub deletes: AtomicU64,
    /// Bytes appended to write-ahead logs.
    pub wal_bytes: AtomicU64,
    /// WAL fsync calls.
    pub wal_syncs: AtomicU64,
    /// Memtable-to-SSTable flushes performed.
    pub flushes: AtomicU64,
    /// Compactions performed.
    pub compactions: AtomicU64,
    /// Data blocks read from disk (i.e. block-cache misses that hit storage).
    pub blocks_read: AtomicU64,
    /// Block-cache hits.
    pub cache_hits: AtomicU64,
    /// Block-cache misses.
    pub cache_misses: AtomicU64,
    /// Lookups the bloom filter allowed through (possible positives).
    pub bloom_positives: AtomicU64,
    /// Lookups the bloom filter rejected (definite negatives — saved a read).
    pub bloom_negatives: AtomicU64,
}

impl Metrics {
    /// Creates a fresh, all-zero metric set.
    pub fn new() -> Metrics {
        Metrics::default()
    }

    #[inline]
    fn bump(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }

    /// Records a served `get`.
    #[inline]
    pub fn inc_gets(&self) {
        Self::bump(&self.gets, 1);
    }

    /// Records `n` applied puts.
    #[inline]
    pub fn add_puts(&self, n: u64) {
        Self::bump(&self.puts, n);
    }

    /// Records `n` applied deletes.
    #[inline]
    pub fn add_deletes(&self, n: u64) {
        Self::bump(&self.deletes, n);
    }

    /// Records `n` bytes written to a WAL.
    #[inline]
    pub fn add_wal_bytes(&self, n: u64) {
        Self::bump(&self.wal_bytes, n);
    }

    /// Records a WAL fsync.
    #[inline]
    pub fn inc_wal_syncs(&self) {
        Self::bump(&self.wal_syncs, 1);
    }

    /// Records a memtable flush.
    #[inline]
    pub fn inc_flushes(&self) {
        Self::bump(&self.flushes, 1);
    }

    /// Records a compaction.
    #[inline]
    pub fn inc_compactions(&self) {
        Self::bump(&self.compactions, 1);
    }

    /// Records a data block read from storage.
    #[inline]
    pub fn inc_blocks_read(&self) {
        Self::bump(&self.blocks_read, 1);
    }

    /// Records a block-cache lookup result.
    #[inline]
    pub fn record_cache(&self, hit: bool) {
        if hit {
            Self::bump(&self.cache_hits, 1);
        } else {
            Self::bump(&self.cache_misses, 1);
        }
    }

    /// Records a bloom-filter verdict (`true` = possible positive).
    #[inline]
    pub fn record_bloom(&self, positive: bool) {
        if positive {
            Self::bump(&self.bloom_positives, 1);
        } else {
            Self::bump(&self.bloom_negatives, 1);
        }
    }

    /// Takes a point-in-time copy of every counter.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        MetricsSnapshot {
            gets: load(&self.gets),
            puts: load(&self.puts),
            deletes: load(&self.deletes),
            wal_bytes: load(&self.wal_bytes),
            wal_syncs: load(&self.wal_syncs),
            flushes: load(&self.flushes),
            compactions: load(&self.compactions),
            blocks_read: load(&self.blocks_read),
            cache_hits: load(&self.cache_hits),
            cache_misses: load(&self.cache_misses),
            bloom_positives: load(&self.bloom_positives),
            bloom_negatives: load(&self.bloom_negatives),
        }
    }
}

/// An immutable copy of [`Metrics`], safe to print or compare in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// See [`Metrics::gets`].
    pub gets: u64,
    /// See [`Metrics::puts`].
    pub puts: u64,
    /// See [`Metrics::deletes`].
    pub deletes: u64,
    /// See [`Metrics::wal_bytes`].
    pub wal_bytes: u64,
    /// See [`Metrics::wal_syncs`].
    pub wal_syncs: u64,
    /// See [`Metrics::flushes`].
    pub flushes: u64,
    /// See [`Metrics::compactions`].
    pub compactions: u64,
    /// See [`Metrics::blocks_read`].
    pub blocks_read: u64,
    /// See [`Metrics::cache_hits`].
    pub cache_hits: u64,
    /// See [`Metrics::cache_misses`].
    pub cache_misses: u64,
    /// See [`Metrics::bloom_positives`].
    pub bloom_positives: u64,
    /// See [`Metrics::bloom_negatives`].
    pub bloom_negatives: u64,
}

impl MetricsSnapshot {
    /// Block-cache hit ratio in `[0, 1]`, or `0.0` if there were no lookups.
    pub fn cache_hit_ratio(&self) -> f64 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            0.0
        } else {
            self.cache_hits as f64 / total as f64
        }
    }

    /// Fraction of bloom lookups that avoided a disk read, in `[0, 1]`.
    pub fn bloom_reject_ratio(&self) -> f64 {
        let total = self.bloom_positives + self.bloom_negatives;
        if total == 0 {
            0.0
        } else {
            self.bloom_negatives as f64 / total as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let m = Metrics::new();
        m.inc_gets();
        m.add_puts(3);
        m.add_deletes(1);
        m.record_cache(true);
        m.record_cache(false);
        m.record_bloom(false);

        let s = m.snapshot();
        assert_eq!(s.gets, 1);
        assert_eq!(s.puts, 3);
        assert_eq!(s.deletes, 1);
        assert_eq!(s.cache_hits, 1);
        assert_eq!(s.cache_misses, 1);
        assert!((s.cache_hit_ratio() - 0.5).abs() < 1e-9);
        assert!((s.bloom_reject_ratio() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ratios_handle_zero() {
        let s = MetricsSnapshot::default();
        assert_eq!(s.cache_hit_ratio(), 0.0);
        assert_eq!(s.bloom_reject_ratio(), 0.0);
    }
}
