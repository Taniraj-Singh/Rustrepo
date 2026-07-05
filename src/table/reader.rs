//! Opens an SSTable and serves point lookups and range scans from it.
//!
//! [`TableReader::open`] bootstraps from the footer: it reads the index block
//! into an in-memory [`SparseIndex`] and loads the bloom filter. A point lookup
//! ([`TableReader::get`]) then costs at most one bloom check plus one data-block
//! read; range scans use [`TableReader::iter`], a two-level cursor that walks the
//! index and, for each block, a [`BlockIter`].
//!
//! Decoded data blocks are shared through the [`BlockCache`], so repeated reads
//! of a hot block avoid both the disk and the decode cost.

use std::sync::Arc;

use crate::cache::BlockCache;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::{BlockHandle, SparseIndex};
use crate::iterator::KvIterator;
use crate::key::{ParsedInternalKey, ValueType};
use crate::storage::block::read_block;
use crate::storage::file::RandomAccessFile;
use crate::table::block::{Block, BlockIter};
use crate::table::{Footer, FOOTER_SIZE};
use crate::util::comparator::InternalKeyComparator;
use crate::util::metrics::Metrics;
use crate::{bloom, log_at};

/// A read-only view over one SSTable file.
#[derive(Debug)]
pub struct TableReader {
    file: RandomAccessFile,
    table_id: u64,
    index: SparseIndex,
    filter: Option<Vec<u8>>,
    cmp: InternalKeyComparator,
    verify_checksums: bool,
    max_block_size: usize,
    cache: Arc<BlockCache>,
    metrics: Arc<Metrics>,
}

impl TableReader {
    /// Opens the table in `file`, identified by `table_id` for caching.
    pub fn open(
        file: RandomAccessFile,
        table_id: u64,
        config: &Config,
        cache: Arc<BlockCache>,
        metrics: Arc<Metrics>,
    ) -> Result<Arc<TableReader>> {
        let len = file.len();
        if len < FOOTER_SIZE as u64 {
            return Err(Error::corruption("table: file smaller than footer"));
        }
        let footer_bytes = file.read_at(len - FOOTER_SIZE as u64, FOOTER_SIZE)?;
        let footer = Footer::decode(&footer_bytes)?;

        let verify = config.paranoid_checks;
        let max_block = config.max_block_size;

        // Load and parse the index block into an in-memory sparse index.
        let index_bytes = read_block(&file, footer.index_handle, verify, max_block)?;
        let index = build_index(index_bytes)?;

        // Load the bloom filter, if the table has one.
        let filter = if footer.filter_handle.size > 0 {
            Some(read_block(&file, footer.filter_handle, verify, max_block)?)
        } else {
            None
        };

        log_at!(
            crate::util::logging::Level::Debug,
            "opened table {} ({} blocks, filter={})",
            table_id,
            index.len(),
            filter.is_some()
        );

        Ok(Arc::new(TableReader {
            file,
            table_id,
            index,
            filter,
            cmp: InternalKeyComparator::new(),
            verify_checksums: verify,
            max_block_size: max_block,
            cache,
            metrics,
        }))
    }

    /// This table's id.
    pub fn table_id(&self) -> u64 {
        self.table_id
    }

    /// Number of data blocks in the table.
    pub fn block_count(&self) -> usize {
        self.index.len()
    }

    /// Looks up `user_key` at the sequence encoded in `internal_lookup_key`.
    ///
    /// Returns the value type and value of the newest matching entry at or below
    /// that sequence, or `None` if the key is absent from this table. The value
    /// type lets the caller distinguish a real value from a tombstone.
    pub fn get(
        &self,
        internal_lookup_key: &[u8],
        user_key: &[u8],
    ) -> Result<Option<(ValueType, Vec<u8>)>> {
        // Bloom filter: a definite "no" avoids the block read entirely.
        if let Some(filter) = &self.filter {
            let present = bloom::may_contain(filter, user_key);
            self.metrics.record_bloom(present);
            if !present {
                return Ok(None);
            }
        }

        let handle = match self.index.find_block(internal_lookup_key, &self.cmp) {
            Some((_, handle)) => handle,
            None => return Ok(None),
        };

        let block = self.read_data_block(handle)?;
        let mut it = block.iter();
        it.seek(internal_lookup_key);
        it.status()?;

        if it.valid() {
            let parsed = ParsedInternalKey::parse(it.key())?;
            if parsed.user_key == user_key {
                return Ok(Some((parsed.value_type, it.value().to_vec())));
            }
        }
        Ok(None)
    }

    /// Reads and decodes the data block at `handle`, using the block cache.
    fn read_data_block(&self, handle: BlockHandle) -> Result<Block> {
        if let Some(bytes) = self.cache.get(self.table_id, handle.offset) {
            self.metrics.record_cache(true);
            return Block::new(bytes);
        }
        self.metrics.record_cache(false);
        self.metrics.inc_blocks_read();

        let raw = read_block(
            &self.file,
            handle,
            self.verify_checksums,
            self.max_block_size,
        )?;
        let arc: Arc<[u8]> = Arc::from(raw.into_boxed_slice());
        self.cache.insert(self.table_id, handle.offset, arc.clone());
        Block::new(arc)
    }

    /// Creates a forward cursor over the whole table.
    pub fn iter(self: &Arc<TableReader>) -> Box<dyn KvIterator> {
        Box::new(TableIter {
            reader: Arc::clone(self),
            block_index: 0,
            block_iter: None,
            valid: false,
            error: None,
        })
    }
}

/// Parses an index block into a [`SparseIndex`].
fn build_index(index_bytes: Vec<u8>) -> Result<SparseIndex> {
    let block = Block::new(Arc::from(index_bytes.into_boxed_slice()))?;
    let mut it = block.iter();
    it.seek_to_first();
    let mut index = SparseIndex::new();
    while it.valid() {
        let separator = it.key().to_vec();
        let handle = BlockHandle::decode_from(it.value())?;
        index.push(separator, handle);
        it.next();
    }
    it.status()?;
    Ok(index)
}

/// A two-level cursor: an outer walk over the index and an inner [`BlockIter`].
struct TableIter {
    reader: Arc<TableReader>,
    block_index: usize,
    block_iter: Option<BlockIter>,
    valid: bool,
    error: Option<String>,
}

impl TableIter {
    /// Loads block `idx`, replacing the inner iterator. Records any error.
    fn set_block(&mut self, idx: usize) {
        match self.reader.index.handle_at(idx) {
            Some(handle) => match self.reader.read_data_block(handle) {
                Ok(block) => {
                    self.block_iter = Some(block.iter());
                    self.block_index = idx;
                }
                Err(e) => {
                    self.error = Some(e.to_string());
                    self.block_iter = None;
                }
            },
            None => {
                self.block_iter = None;
            }
        }
    }

    fn cur_valid(&self) -> bool {
        self.block_iter.as_ref().is_some_and(|it| it.valid())
    }

    /// Advances across block boundaries until positioned on a valid entry or the
    /// table is exhausted.
    fn settle_forward(&mut self) {
        while self.block_iter.is_some() && !self.cur_valid() {
            if self.error.is_some() {
                break;
            }
            let next = self.block_index + 1;
            if next >= self.reader.index.len() {
                self.block_iter = None;
                break;
            }
            self.set_block(next);
            if let Some(it) = self.block_iter.as_mut() {
                it.seek_to_first();
            }
        }
        self.valid = self.cur_valid();
    }
}

impl KvIterator for TableIter {
    fn valid(&self) -> bool {
        self.valid
    }

    fn seek_to_first(&mut self) {
        self.error = None;
        if self.reader.index.is_empty() {
            self.block_iter = None;
            self.valid = false;
            return;
        }
        self.set_block(0);
        if let Some(it) = self.block_iter.as_mut() {
            it.seek_to_first();
        }
        self.settle_forward();
    }

    fn seek(&mut self, target: &[u8]) {
        self.error = None;
        let start = match self.reader.index.find_block(target, &self.reader.cmp) {
            Some((idx, _)) => idx,
            None => {
                self.block_iter = None;
                self.valid = false;
                return;
            }
        };
        self.set_block(start);
        if let Some(it) = self.block_iter.as_mut() {
            it.seek(target);
        }
        self.settle_forward();
    }

    fn next(&mut self) {
        if let Some(it) = self.block_iter.as_mut() {
            it.next();
        }
        self.settle_forward();
    }

    fn key(&self) -> &[u8] {
        match &self.block_iter {
            Some(it) if self.valid => it.key(),
            _ => &[],
        }
    }

    fn value(&self) -> &[u8] {
        match &self.block_iter {
            Some(it) if self.valid => it.value(),
            _ => &[],
        }
    }

    fn status(&self) -> Result<()> {
        if let Some(msg) = &self.error {
            return Err(Error::corruption(msg.clone()));
        }
        match &self.block_iter {
            Some(it) => it.status(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterator::drain_to_vec;
    use crate::key::{InternalKey, LookupKey, ValueType};
    use crate::storage::file::WritableFile;
    use crate::table::TableBuilder;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-tablereader-{}-{}", std::process::id(), name));
        p
    }

    fn build_table(path: &PathBuf, n: u32, config: &Config) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = (0..n)
            .map(|i| {
                (
                    InternalKey::new(format!("k{i:05}").as_bytes(), 1, ValueType::Value)
                        .into_bytes(),
                    format!("v{i}").into_bytes(),
                )
            })
            .collect();
        entries.sort();
        let file = WritableFile::create(path).unwrap();
        let mut b = TableBuilder::new(file, config);
        for (k, v) in &entries {
            b.add(k, v).unwrap();
        }
        b.finish().unwrap();
        entries
    }

    fn open(path: &PathBuf, config: &Config) -> Arc<TableReader> {
        let file = RandomAccessFile::open(path).unwrap();
        let cache = Arc::new(BlockCache::new(1 << 20));
        let metrics = Arc::new(Metrics::new());
        TableReader::open(file, 7, config, cache, metrics).unwrap()
    }

    #[test]
    fn full_scan_matches_input() {
        let path = temp_path("scan");
        let config = Config::default().block_size(256); // force many blocks
        let entries = build_table(&path, 300, &config);
        let reader = open(&path, &config);
        let mut it = reader.iter();
        it.seek_to_first();
        assert_eq!(drain_to_vec(&mut it), entries);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn seek_within_table() {
        let path = temp_path("seek");
        let config = Config::default().block_size(256);
        build_table(&path, 300, &config);
        let reader = open(&path, &config);
        let mut it = reader.iter();
        let target = InternalKey::new(b"k00150", 1, ValueType::Value).into_bytes();
        it.seek(&target);
        assert!(it.valid());
        assert_eq!(&it.key()[..6], b"k00150");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn point_lookups_hit_and_miss() {
        let path = temp_path("point");
        let config = Config::default();
        build_table(&path, 100, &config);
        let reader = open(&path, &config);

        let lk = LookupKey::new(b"k00050", 100);
        assert_eq!(
            reader.get(lk.internal_key(), b"k00050").unwrap().unwrap().1,
            b"v50".to_vec()
        );
        let miss = LookupKey::new(b"absent", 100);
        assert!(reader
            .get(miss.internal_key(), b"absent")
            .unwrap()
            .is_none());
        std::fs::remove_file(&path).ok();
    }
}
