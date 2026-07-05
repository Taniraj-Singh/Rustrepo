//! # Cairn
//!
//! Cairn is an embedded, **dependency-free** key-value storage engine built on a
//! log-structured merge-tree (LSM-tree). It is a from-scratch, safe-Rust storage
//! engine in the spirit of LevelDB/RocksDB, implemented entirely against the
//! standard library.
//!
//! ## What it gives you
//!
//! * An ordered, persistent `key -> value` map with `put` / `get` / `delete`.
//! * Crash recovery via a write-ahead log (WAL).
//! * Atomic multi-key writes ([`WriteBatch`]) and MVCC [`Snapshot`]s.
//! * Range scans through a unified [`iterator`] framework.
//! * On-disk [`table`]s (SSTables) with per-block checksums, a sparse index,
//!   bloom filters, and pluggable [`compression`].
//! * A [`version`]/[`manifest`] layer that tracks the live set of tables and
//!   survives restarts.
//!
//! ## The write path
//!
//! ```text
//!   put(k, v)
//!      │
//!      ▼
//!   [WAL append]  ──►  durable on disk (recovery replays this)
//!      │
//!      ▼
//!   [MemTable]    ──►  in-memory sorted map (skip list)
//!      │  (when full)
//!      ▼
//!   [flush]       ──►  immutable SSTable in level 0
//!      │  (background)
//!      ▼
//!   [compaction]  ──►  merge/rewrite tables into deeper levels
//! ```
//!
//! ## The read path
//!
//! A lookup consults, in order: the active memtable, the memtable being
//! flushed, then each level's tables (newest first), short-circuiting on the
//! first match. Bloom filters and the sparse block index keep the number of
//! block reads small.
//!
//! ## Safety on untrusted input
//!
//! Every on-disk structure is parsed defensively. Lengths, offsets, and counts
//! read from a file are validated against the remaining buffer *before* they are
//! used to index or allocate; all arithmetic on untrusted values is checked;
//! recursion is bounded. A corrupt or hostile file yields a structured
//! [`Error`] — never a panic, out-of-bounds read, infinite loop, or unbounded
//! allocation. See the `parser` module and the fuzz targets under `fuzz/`.
//!
//! ## Quick start
//!
//! ```no_run
//! use cairn::{Cairn, Config};
//!
//! # fn main() -> cairn::Result<()> {
//! let mut db = Cairn::open("/tmp/cairn-demo", Config::default())?;
//! db.put(b"alpha", b"one")?;
//! db.put(b"beta", b"two")?;
//! assert_eq!(db.get(b"alpha")?.as_deref(), Some(&b"one"[..]));
//! db.delete(b"alpha")?;
//! assert_eq!(db.get(b"alpha")?, None);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

pub mod bloom;
pub mod cache;
pub mod checksum;
pub mod compression;
pub mod config;
pub mod db;
pub mod error;
pub mod index;
pub mod iterator;
pub mod key;
pub mod manifest;
pub mod memtable;
pub mod options;
pub mod parser;
pub mod recovery;
pub mod serializer;
pub mod snapshot;
pub mod storage;
pub mod table;
pub mod transaction;
pub mod util;
pub mod version;

pub use crate::config::{CompressionKind, Config};
pub use crate::db::{Cairn, DbIterator, LevelSummary, RangeScan};
pub use crate::error::{Error, Result};
pub use crate::iterator::KvIterator;
pub use crate::key::{Sequence, ValueType};
pub use crate::options::{ReadOptions, WriteOptions};
pub use crate::snapshot::Snapshot;
pub use crate::transaction::{Transaction, WriteBatch};

/// The crate version, taken from `Cargo.toml` at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The four-byte magic that prefixes every Cairn on-disk file family.
pub const MAGIC: &[u8; 4] = b"CARN";
