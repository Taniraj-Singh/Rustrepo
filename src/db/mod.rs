//! The database facade: [`Cairn`], the type applications actually use.
//!
//! `Cairn` ties every other module into a working LSM engine. A single mutex
//! guards the mutable core (the active memtable, the current WAL, and the
//! [`VersionSet`]); the block cache, table cache, metrics, and snapshot list are
//! shared and internally synchronized. Flushing and compaction run inline under
//! the write path — there is no background thread — which keeps the concurrency
//! model small and easy to reason about.
//!
//! ## Durability and recovery
//!
//! Each write is appended to the WAL before it touches the memtable, so an
//! acknowledged write survives a crash. On open, the previous WAL is replayed
//! ([`crate::recovery`]) and materialized into a table, and a fresh WAL is
//! started. Metadata durability rides on the manifest ([`crate::manifest`]).
//!
//! ## Reads
//!
//! A point lookup checks the memtable, then level-0 tables newest-first, then one
//! table per deeper level (located by binary search), stopping at the first
//! match. Bloom filters and the block cache keep the cost low. Range scans use
//! [`DbIterator`], which merges all sources and applies MVCC visibility.

mod iter;
mod table_cache;

pub use iter::DbIterator;
pub use table_cache::TableCache;

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cache::BlockCache;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::iterator::{KvIterator, MergingIterator};
use crate::key::{LookupKey, ParsedInternalKey, Sequence, ValueType};
use crate::manifest::current_file_path;
use crate::memtable::{LookupResult, MemTable};
use crate::snapshot::{Snapshot, SnapshotList};
use crate::storage::{table_file_name, wal_file_name, LogWriter, WritableFile, LOCK_FILE};
use crate::table::TableBuilder;
use crate::transaction::{Transaction, WriteBatch};
use crate::util::logging::Level;
use crate::util::metrics::{Metrics, MetricsSnapshot};
use crate::version::{FileMetaData, Version, VersionEdit, VersionSet};
use crate::{log_at, recovery};

/// The mutable core of a database, guarded by a single mutex.
#[derive(Debug)]
struct DbInner {
    mem: MemTable,
    wal: LogWriter,
    versions: VersionSet,
}

/// An embedded LSM-tree key-value database.
///
/// Open one with [`Cairn::open`]. All methods take `&self`; the type is `Send +
/// Sync` and may be shared across threads behind an `Arc`.
#[derive(Debug)]
pub struct Cairn {
    inner: Mutex<DbInner>,
    config: Config,
    dbname: PathBuf,
    block_cache: Arc<BlockCache>,
    metrics: Arc<Metrics>,
    table_cache: TableCache,
    snapshots: SnapshotList,
    // Held for the database's lifetime; releases the directory lock on drop.
    _lock: FileLock,
}

impl Cairn {
    /// Opens (or creates) the database in directory `path`.
    pub fn open(path: impl AsRef<Path>, config: Config) -> Result<Cairn> {
        config.validate()?;
        let dbname = path.as_ref().to_path_buf();
        let exists = current_file_path(&dbname).exists();

        if exists && config.error_if_exists {
            return Err(Error::invalid(format!(
                "database already exists at {}",
                dbname.display()
            )));
        }
        if !exists && !config.create_if_missing {
            return Err(Error::MissingFile(dbname.display().to_string()));
        }
        std::fs::create_dir_all(&dbname)?;

        let lock = FileLock::acquire(dbname.join(LOCK_FILE))?;
        let block_cache = Arc::new(BlockCache::new(config.block_cache_capacity));
        let metrics = Arc::new(Metrics::new());
        let table_cache = TableCache::new(
            dbname.clone(),
            config.clone(),
            Arc::clone(&block_cache),
            Arc::clone(&metrics),
        );

        // Recover or initialize the version set, WAL, and memtable.
        let (versions, mem, wal, recovered_nonempty) = if exists {
            let mut versions = VersionSet::recover(&dbname, config.clone())?;
            let mut mem = MemTable::new();
            let old_log = versions.log_number();
            let wal_path = dbname.join(wal_file_name(old_log));
            if wal_path.exists() {
                if let Some(max) =
                    recovery::replay_wal_file(&wal_path, &mut mem, config.paranoid_checks)?
                {
                    versions.set_last_sequence(max);
                }
            }
            let nonempty = !mem.is_empty();
            let wal = start_new_wal(&dbname, &mut versions)?;
            // The obsolete WAL's data now lives in `mem`; it will be flushed
            // below. Remove the old segment.
            let _ = std::fs::remove_file(&wal_path);
            (versions, mem, wal, nonempty)
        } else {
            let mut versions = VersionSet::create(&dbname, config.clone())?;
            let wal = start_new_wal(&dbname, &mut versions)?;
            (versions, MemTable::new(), wal, false)
        };

        log_at!(
            Level::Info,
            "opened database at {} (last_seq={})",
            dbname.display(),
            versions.last_sequence()
        );

        let db = Cairn {
            inner: Mutex::new(DbInner { mem, wal, versions }),
            config,
            dbname,
            block_cache,
            metrics,
            table_cache,
            snapshots: SnapshotList::new(),
            _lock: lock,
        };

        // Materialize any WAL-recovered data into a level-0 table so the fresh
        // WAL starts empty.
        if recovered_nonempty {
            db.flush()?;
        }
        Ok(db)
    }

    // ---- writes ------------------------------------------------------------

    /// Sets `key` to `value`.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.put(key, value);
        self.write(batch)
    }

    /// Deletes `key` (a no-op if it is absent).
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.delete(key);
        self.write(batch)
    }

    /// Commits a transaction atomically.
    pub fn commit(&self, txn: Transaction) -> Result<()> {
        self.write(txn.into_batch())
    }

    /// Applies a [`WriteBatch`] atomically and durably.
    pub fn write(&self, mut batch: WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        self.validate_batch(&batch)?;

        let mut inner = self.lock();
        self.make_room_for_write(&mut inner)?;

        let base_seq = inner.versions.last_sequence() + 1;
        batch.set_sequence(base_seq);
        let count = batch.count();

        // WAL first: durability before the write is visible.
        inner.wal.add_record(batch.as_bytes())?;
        if self.config.sync_writes {
            inner.wal.sync()?;
            self.metrics.inc_wal_syncs();
        } else {
            inner.wal.flush()?;
        }
        self.metrics.add_wal_bytes(batch.approximate_size() as u64);

        batch.insert_into(&mut inner.mem)?;
        inner
            .versions
            .set_last_sequence(base_seq + count as u64 - 1);

        self.record_batch_metrics(&batch);
        Ok(())
    }

    // ---- reads -------------------------------------------------------------

    /// Reads the current value of `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let seq = self.lock().versions.last_sequence();
        self.lookup(key, seq)
    }

    /// Reads `key` as of `snapshot`.
    pub fn get_at(&self, key: &[u8], snapshot: &Snapshot) -> Result<Option<Vec<u8>>> {
        self.lookup(key, snapshot.sequence())
    }

    fn lookup(&self, key: &[u8], seq: Sequence) -> Result<Option<Vec<u8>>> {
        if key.len() > self.config.max_key_size {
            return Err(Error::limit(
                "key length",
                key.len() as u64,
                self.config.max_key_size as u64,
            ));
        }
        self.metrics.inc_gets();

        // Memtable first, under the lock; capture a version snapshot for the
        // table search that follows without the lock held.
        let version = {
            let inner = self.lock();
            match inner.mem.get(key, seq) {
                LookupResult::Found(v) => return Ok(Some(v)),
                LookupResult::Deleted => return Ok(None),
                LookupResult::NotFound => {}
            }
            inner.versions.current()
        };
        self.search_tables(&version, key, seq)
    }

    /// Searches the on-disk levels for `key` at sequence `seq`.
    fn search_tables(
        &self,
        version: &Version,
        key: &[u8],
        seq: Sequence,
    ) -> Result<Option<Vec<u8>>> {
        let lookup = LookupKey::new(key, seq);
        for level in 0..version.num_levels() {
            let candidates = self.level_candidates(version, level, key);
            for file in candidates {
                let reader = self.table_cache.get(file.number)?;
                match reader.get(lookup.internal_key(), key)? {
                    Some((ValueType::Value, value)) => return Ok(Some(value)),
                    Some((ValueType::Deletion, _)) => return Ok(None),
                    None => continue,
                }
            }
        }
        Ok(None)
    }

    /// The files at `level` that could contain `key`, in read order.
    fn level_candidates(
        &self,
        version: &Version,
        level: usize,
        key: &[u8],
    ) -> Vec<Arc<FileMetaData>> {
        if level == 0 {
            // Level 0 tables overlap; check every one that spans the key,
            // newest-first (the level is already sorted that way).
            version
                .files_at(0)
                .iter()
                .filter(|f| key_in_file(f, key))
                .cloned()
                .collect()
        } else {
            // Deeper levels are disjoint and sorted; at most one file matches.
            find_file_for_key(version.files_at(level), key)
                .into_iter()
                .collect()
        }
    }

    // ---- snapshots and scans ----------------------------------------------

    /// Takes a snapshot at the current sequence.
    pub fn snapshot(&self) -> Snapshot {
        let seq = self.lock().versions.last_sequence();
        self.snapshots.snapshot(seq)
    }

    /// Returns a forward scan over all live keys at the current sequence.
    pub fn scan(&self) -> Result<DbIterator> {
        let seq = self.lock().versions.last_sequence();
        self.scan_at_sequence(seq)
    }

    /// Returns a forward scan as of `snapshot`.
    pub fn scan_at(&self, snapshot: &Snapshot) -> Result<DbIterator> {
        self.scan_at_sequence(snapshot.sequence())
    }

    fn scan_at_sequence(&self, seq: Sequence) -> Result<DbIterator> {
        // Capture the memtable snapshot and the version under the lock, then
        // open table iterators without holding it.
        let (mem_iter, version) = {
            let inner = self.lock();
            (inner.mem.iter(), inner.versions.current())
        };

        let mut children: Vec<Box<dyn KvIterator>> = vec![Box::new(mem_iter)];
        for level in 0..version.num_levels() {
            for file in version.files_at(level) {
                let reader = self.table_cache.get(file.number)?;
                children.push(reader.iter());
            }
        }
        Ok(DbIterator::new(MergingIterator::new(children), seq))
    }

    // ---- maintenance -------------------------------------------------------

    /// Forces the active memtable to be flushed to a level-0 table.
    pub fn flush(&self) -> Result<()> {
        let mut inner = self.lock();
        self.flush_locked(&mut inner)
    }

    /// Runs compaction until the tree satisfies its level bounds.
    pub fn compact(&self) -> Result<()> {
        let mut inner = self.lock();
        self.maybe_compact(&mut inner)
    }

    /// A snapshot of the engine's runtime counters.
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// The active configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The database directory.
    pub fn path(&self) -> &Path {
        &self.dbname
    }

    /// Number of currently open table files.
    pub fn open_table_count(&self) -> usize {
        self.table_cache.open_count()
    }

    /// Number of blocks currently held in the shared block cache.
    pub fn cached_block_count(&self) -> usize {
        self.block_cache.len()
    }

    // ---- internals ---------------------------------------------------------

    fn lock(&self) -> std::sync::MutexGuard<'_, DbInner> {
        self.inner.lock().expect("database mutex poisoned")
    }

    fn validate_batch(&self, batch: &WriteBatch) -> Result<()> {
        batch.for_each(|_, key, value| {
            if key.len() > self.config.max_key_size {
                return Err(Error::limit(
                    "key length",
                    key.len() as u64,
                    self.config.max_key_size as u64,
                ));
            }
            if value.len() > self.config.max_value_size {
                return Err(Error::limit(
                    "value length",
                    value.len() as u64,
                    self.config.max_value_size as u64,
                ));
            }
            Ok(())
        })
    }

    fn record_batch_metrics(&self, batch: &WriteBatch) {
        let mut puts = 0u64;
        let mut deletes = 0u64;
        let _ = batch.for_each(|t, _, _| {
            match t {
                ValueType::Value => puts += 1,
                ValueType::Deletion => deletes += 1,
            }
            Ok(())
        });
        self.metrics.add_puts(puts);
        self.metrics.add_deletes(deletes);
    }

    /// Flushes the memtable if it has grown past the write-buffer size, then
    /// compacts if the tree is out of shape.
    fn make_room_for_write(&self, inner: &mut DbInner) -> Result<()> {
        if inner.mem.approximate_memory_usage() < self.config.write_buffer_size {
            return Ok(());
        }
        self.flush_locked(inner)?;
        self.maybe_compact(inner)?;
        Ok(())
    }

    /// Writes the current memtable to a level-0 table and starts a fresh WAL.
    fn flush_locked(&self, inner: &mut DbInner) -> Result<()> {
        if inner.mem.is_empty() {
            return Ok(());
        }
        let mem = std::mem::take(&mut inner.mem);
        self.write_level0_table(&mem, &mut inner.versions)?;
        inner.wal = start_new_wal(&self.dbname, &mut inner.versions)?;
        self.metrics.inc_flushes();
        Ok(())
    }

    /// Builds a level-0 table from `mem` and registers it in a new version.
    fn write_level0_table(&self, mem: &MemTable, versions: &mut VersionSet) -> Result<()> {
        let number = versions.new_file_number();
        let path = self.dbname.join(table_file_name(number));
        let file = WritableFile::create(&path)?;
        let mut builder = TableBuilder::new(file, &self.config);

        let mut it = mem.iter();
        it.seek_to_first();
        while it.valid() {
            builder.add(it.key(), it.value())?;
            it.next();
        }
        it.status()?;

        let result = builder.finish()?;
        let meta = FileMetaData::new(
            number,
            result.file_size,
            result.smallest.unwrap_or_default(),
            result.largest.unwrap_or_default(),
        );
        let mut edit = VersionEdit::new();
        edit.add_file(0, meta);
        versions.log_and_apply(edit)?;
        log_at!(Level::Debug, "flushed memtable to table {number}");
        Ok(())
    }

    /// Repeatedly compacts the shallowest out-of-bound level until none remain.
    fn maybe_compact(&self, inner: &mut DbInner) -> Result<()> {
        // A generous safety bound; each step strictly shrinks the shallow levels.
        for _ in 0..1000 {
            let level = match inner.versions.current().pick_compaction_level(&self.config) {
                Some(l) => l,
                None => return Ok(()),
            };
            self.compact_level(inner, level)?;
        }
        Ok(())
    }

    /// Merges level `level` (and overlapping `level+1` files) into `level+1`.
    fn compact_level(&self, inner: &mut DbInner, level: usize) -> Result<()> {
        let version = inner.versions.current();
        let num_levels = version.num_levels();

        let mut inputs_low = version.files_at(level).to_vec();
        if inputs_low.is_empty() {
            return Ok(());
        }
        // For deeper levels, one file at a time keeps compactions bounded.
        if level > 0 {
            inputs_low.truncate(1);
        }

        let (smallest, largest) = key_range(&inputs_low);
        let inputs_high = version.overlapping_files(level + 1, &smallest, &largest);

        // Merge every input table.
        let mut children: Vec<Box<dyn KvIterator>> = Vec::new();
        for file in inputs_low.iter().chain(inputs_high.iter()) {
            let reader = self.table_cache.get(file.number)?;
            children.push(reader.iter());
        }
        let mut merged = MergingIterator::new(children);
        merged.seek_to_first();

        let is_last_level = level + 1 >= num_levels - 1;
        let oldest_snapshot = self
            .snapshots
            .oldest()
            .unwrap_or_else(|| inner.versions.last_sequence());
        let max_output_size =
            (self.config.write_buffer_size as u64 * 2).max(self.config.base_level_size);

        let mut outputs: Vec<FileMetaData> = Vec::new();
        let mut builder: Option<TableBuilder> = None;
        let mut builder_number = 0u64;
        let mut last_user: Option<Vec<u8>> = None;
        let mut last_seq_for_key = u64::MAX;

        while merged.valid() {
            let internal_key = merged.key().to_vec();
            let parsed = ParsedInternalKey::parse(&internal_key)?;

            if last_user.as_deref() != Some(parsed.user_key) {
                last_user = Some(parsed.user_key.to_vec());
                last_seq_for_key = u64::MAX;
            }

            // Drop rules: a version already shadowed by one visible to every
            // snapshot, or a tombstone that has reached the bottom level.
            let drop = if last_seq_for_key <= oldest_snapshot {
                true
            } else {
                parsed.value_type == ValueType::Deletion
                    && parsed.sequence <= oldest_snapshot
                    && is_last_level
            };
            last_seq_for_key = parsed.sequence;

            if !drop {
                if builder.is_none() {
                    builder_number = inner.versions.new_file_number();
                    let path = self.dbname.join(table_file_name(builder_number));
                    builder = Some(TableBuilder::new(
                        WritableFile::create(&path)?,
                        &self.config,
                    ));
                }
                let b = builder.as_mut().expect("builder present");
                b.add(&internal_key, merged.value())?;
                if b.file_offset() >= max_output_size {
                    let result = builder.take().expect("builder present").finish()?;
                    outputs.push(file_meta(builder_number, result));
                }
            }
            merged.next();
        }
        merged.status()?;
        if let Some(b) = builder.take() {
            let result = b.finish()?;
            outputs.push(file_meta(builder_number, result));
        }

        // Swap inputs for outputs atomically in a single version edit.
        let mut edit = VersionEdit::new();
        for f in &inputs_low {
            edit.delete_file(level, f.number);
        }
        for f in &inputs_high {
            edit.delete_file(level + 1, f.number);
        }
        for out in &outputs {
            edit.add_file(level + 1, out.clone());
        }
        inner.versions.log_and_apply(edit)?;
        self.metrics.inc_compactions();

        // Reclaim the obsolete input files.
        for f in inputs_low.iter().chain(inputs_high.iter()) {
            self.table_cache.evict(f.number);
            let _ = std::fs::remove_file(self.dbname.join(table_file_name(f.number)));
        }
        log_at!(
            Level::Info,
            "compacted level {level}: {} + {} files -> {} files",
            inputs_low.len(),
            inputs_high.len(),
            outputs.len()
        );
        Ok(())
    }
}

/// Builds file metadata from a finished table build.
fn file_meta(number: u64, result: crate::table::TableBuildResult) -> FileMetaData {
    FileMetaData::new(
        number,
        result.file_size,
        result.smallest.unwrap_or_default(),
        result.largest.unwrap_or_default(),
    )
}

/// The user-key portion of an internal key.
fn user_key(internal: &[u8]) -> &[u8] {
    if internal.len() >= 8 {
        &internal[..internal.len() - 8]
    } else {
        internal
    }
}

/// Whether `key` falls within a file's `[smallest, largest]` user-key range.
fn key_in_file(file: &FileMetaData, key: &[u8]) -> bool {
    user_key(&file.smallest) <= key && key <= user_key(&file.largest)
}

/// The combined user-key range of a set of files.
fn key_range(files: &[Arc<FileMetaData>]) -> (Vec<u8>, Vec<u8>) {
    let mut smallest = user_key(&files[0].smallest).to_vec();
    let mut largest = user_key(&files[0].largest).to_vec();
    for f in &files[1..] {
        let fs = user_key(&f.smallest);
        let fl = user_key(&f.largest);
        if fs < smallest.as_slice() {
            smallest = fs.to_vec();
        }
        if fl > largest.as_slice() {
            largest = fl.to_vec();
        }
    }
    (smallest, largest)
}

/// Binary-searches a sorted, disjoint level for the file containing `key`.
fn find_file_for_key(files: &[Arc<FileMetaData>], key: &[u8]) -> Option<Arc<FileMetaData>> {
    // First file whose largest key is >= key.
    let idx = files.partition_point(|f| user_key(&f.largest) < key);
    let file = files.get(idx)?;
    if user_key(&file.smallest) <= key {
        Some(Arc::clone(file))
    } else {
        None
    }
}

/// Allocates a new WAL file, records it as the current log, and persists the
/// change to the manifest.
fn start_new_wal(dbname: &Path, versions: &mut VersionSet) -> Result<LogWriter> {
    let number = versions.new_file_number();
    versions.set_log_number(number);
    let wal = LogWriter::create(dbname.join(wal_file_name(number)))?;
    versions.log_and_apply(VersionEdit::new())?;
    Ok(wal)
}

/// An advisory directory lock backed by an exclusively-created `LOCK` file.
///
/// Prevents two handles in the same process (and, on most platforms, two
/// processes) from opening the same database at once. Released on drop. A crash
/// can leave a stale lock behind, which must be removed manually — the honest
/// cost of a dependency-free, portable lock.
#[derive(Debug)]
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(path: PathBuf) -> Result<FileLock> {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(FileLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(Error::AlreadyLocked(path.display().to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-db-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn put_get_delete() {
        let dir = temp_dir("basic");
        let db = Cairn::open(&dir, Config::default()).unwrap();
        db.put(b"alpha", b"one").unwrap();
        db.put(b"beta", b"two").unwrap();
        assert_eq!(db.get(b"alpha").unwrap().as_deref(), Some(&b"one"[..]));
        assert_eq!(db.get(b"missing").unwrap(), None);
        db.delete(b"alpha").unwrap();
        assert_eq!(db.get(b"alpha").unwrap(), None);
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn survives_flush_and_reopen() {
        let dir = temp_dir("reopen");
        {
            let db = Cairn::open(&dir, Config::default().write_buffer_size(4096)).unwrap();
            for i in 0..2000u32 {
                db.put(
                    format!("key{i:05}").as_bytes(),
                    format!("val{i}").as_bytes(),
                )
                .unwrap();
            }
            // Enough writes to force flushes to disk.
            assert!(db.metrics().flushes > 0);
        }
        let db = Cairn::open(&dir, Config::default()).unwrap();
        for i in 0..2000u32 {
            assert_eq!(
                db.get(format!("key{i:05}").as_bytes()).unwrap().as_deref(),
                Some(format!("val{i}").as_bytes())
            );
        }
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_is_stable() {
        let dir = temp_dir("snapshot");
        let db = Cairn::open(&dir, Config::default()).unwrap();
        db.put(b"k", b"v1").unwrap();
        let snap = db.snapshot();
        db.put(b"k", b"v2").unwrap();
        assert_eq!(db.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));
        assert_eq!(db.get_at(b"k", &snap).unwrap().as_deref(), Some(&b"v1"[..]));
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_returns_sorted_live_keys() {
        let dir = temp_dir("scan");
        let db = Cairn::open(&dir, Config::default().write_buffer_size(8192)).unwrap();
        for i in 0..500u32 {
            db.put(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
        db.delete(b"k0100").unwrap();
        let mut it = db.scan().unwrap();
        it.seek_to_first();
        let mut count = 0;
        let mut prev: Option<Vec<u8>> = None;
        while it.valid() {
            if let Some(p) = &prev {
                assert!(it.key() > p.as_slice(), "keys must be sorted");
            }
            assert_ne!(it.key(), b"k0100"); // deleted
            prev = Some(it.key().to_vec());
            count += 1;
            it.next();
        }
        assert_eq!(count, 499);
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compaction_keeps_reads_correct() {
        let dir = temp_dir("compact");
        let mut config = Config::default().write_buffer_size(4096);
        config.level0_compaction_trigger = 2;
        let db = Cairn::open(&dir, config).unwrap();
        // Overwrite the same keys many times to exercise version collapse.
        for round in 0..20u32 {
            for i in 0..200u32 {
                db.put(
                    format!("k{i:04}").as_bytes(),
                    format!("r{round}v{i}").as_bytes(),
                )
                .unwrap();
            }
        }
        db.compact().unwrap();
        assert!(db.metrics().compactions > 0);
        // Latest value for each key must be the final round.
        for i in 0..200u32 {
            assert_eq!(
                db.get(format!("k{i:04}").as_bytes()).unwrap().as_deref(),
                Some(format!("r19v{i}").as_bytes())
            );
        }
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn second_open_is_locked_out() {
        let dir = temp_dir("lock");
        let db = Cairn::open(&dir, Config::default()).unwrap();
        assert!(matches!(
            Cairn::open(&dir, Config::default()),
            Err(Error::AlreadyLocked(_))
        ));
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }
}
