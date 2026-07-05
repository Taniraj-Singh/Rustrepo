//! Per-operation read and write options.
//!
//! Where [`Config`](crate::Config) fixes database-wide policy at open time,
//! [`ReadOptions`] and [`WriteOptions`] tune an individual call. They are small,
//! cheap value types with chainable setters:
//!
//! ```
//! use cairn::options::{ReadOptions, WriteOptions};
//!
//! let ro = ReadOptions::new().fill_cache(false);
//! let wo = WriteOptions::new().sync(true);
//! assert!(!ro.should_fill_cache());
//! assert!(wo.is_sync());
//! ```
//!
//! The database exposes `*_with` variants — [`Cairn::get_with`],
//! [`Cairn::write_with`], [`Cairn::scan_with`] — that take these.
//!
//! [`Cairn::get_with`]: crate::Cairn::get_with
//! [`Cairn::write_with`]: crate::Cairn::write_with
//! [`Cairn::scan_with`]: crate::Cairn::scan_with

use crate::key::Sequence;
use crate::snapshot::Snapshot;

/// Options controlling a single read (`get` or `scan`).
#[derive(Debug, Clone, Copy)]
pub struct ReadOptions {
    // The sequence to read at; `None` means "latest".
    snapshot: Option<Sequence>,
    // Whether blocks read to serve this request may populate the block cache.
    fill_cache: bool,
}

impl Default for ReadOptions {
    fn default() -> ReadOptions {
        ReadOptions {
            snapshot: None,
            fill_cache: true,
        }
    }
}

impl ReadOptions {
    /// Default read options: latest data, populate the cache.
    pub fn new() -> ReadOptions {
        ReadOptions::default()
    }

    /// Reads as of `snapshot` instead of the latest state.
    pub fn snapshot(mut self, snapshot: &Snapshot) -> ReadOptions {
        self.snapshot = Some(snapshot.sequence());
        self
    }

    /// Reads at an explicit sequence number.
    pub fn at_sequence(mut self, seq: Sequence) -> ReadOptions {
        self.snapshot = Some(seq);
        self
    }

    /// Sets whether reads may populate the block cache. Disabling is useful for
    /// large one-off scans that should not evict hot blocks.
    pub fn fill_cache(mut self, yes: bool) -> ReadOptions {
        self.fill_cache = yes;
        self
    }

    /// The snapshot sequence to read at, if pinned.
    pub fn snapshot_sequence(&self) -> Option<Sequence> {
        self.snapshot
    }

    /// Whether reads should populate the block cache.
    pub fn should_fill_cache(&self) -> bool {
        self.fill_cache
    }
}

/// Options controlling a single write (`put`, `delete`, or `write`).
///
/// The default (non-syncing) write flushes to the OS but does not fsync.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
    sync: bool,
}

impl WriteOptions {
    /// Default write options: flush to the OS but do not fsync.
    pub fn new() -> WriteOptions {
        WriteOptions::default()
    }

    /// If `true`, fsync the WAL before the write returns, guaranteeing the write
    /// survives a power loss. Slower, but durable per call.
    pub fn sync(mut self, yes: bool) -> WriteOptions {
        self.sync = yes;
        self
    }

    /// Whether this write fsyncs the WAL.
    pub fn is_sync(&self) -> bool {
        self.sync
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_defaults() {
        let ro = ReadOptions::new();
        assert_eq!(ro.snapshot_sequence(), None);
        assert!(ro.should_fill_cache());
    }

    #[test]
    fn read_builder() {
        let ro = ReadOptions::new().at_sequence(42).fill_cache(false);
        assert_eq!(ro.snapshot_sequence(), Some(42));
        assert!(!ro.should_fill_cache());
    }

    #[test]
    fn write_defaults_and_builder() {
        assert!(!WriteOptions::new().is_sync());
        assert!(WriteOptions::new().sync(true).is_sync());
    }
}
