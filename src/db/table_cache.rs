//! Caches open [`TableReader`]s by file number.
//!
//! Opening a table parses its footer and index and installs a file handle;
//! doing that on every read would be wasteful, so opened tables are memoized
//! here. The cache is keyed by file number and shares the database's block cache
//! and metrics, so all tables draw decoded blocks from one bounded pool.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cache::BlockCache;
use crate::config::Config;
use crate::error::Result;
use crate::storage::{table_file_name, RandomAccessFile};
use crate::table::TableReader;
use crate::util::metrics::Metrics;

/// A memoizing opener for table files.
#[derive(Debug)]
pub struct TableCache {
    dbname: PathBuf,
    config: Config,
    block_cache: Arc<BlockCache>,
    metrics: Arc<Metrics>,
    open: Mutex<HashMap<u64, Arc<TableReader>>>,
}

impl TableCache {
    /// Creates a table cache rooted at `dbname`.
    pub fn new(
        dbname: PathBuf,
        config: Config,
        block_cache: Arc<BlockCache>,
        metrics: Arc<Metrics>,
    ) -> TableCache {
        TableCache {
            dbname,
            config,
            block_cache,
            metrics,
            open: Mutex::new(HashMap::new()),
        }
    }

    /// Returns a reader for table `number`, opening it if not already cached.
    pub fn get(&self, number: u64) -> Result<Arc<TableReader>> {
        {
            let guard = self.open.lock().expect("table cache poisoned");
            if let Some(reader) = guard.get(&number) {
                return Ok(Arc::clone(reader));
            }
        }
        // Open outside the lock so a slow open does not block other lookups,
        // then install; a race just means two opens and one wins the insert.
        let path = self.dbname.join(table_file_name(number));
        let file = RandomAccessFile::open(&path)?;
        let reader = TableReader::open(
            file,
            number,
            &self.config,
            Arc::clone(&self.block_cache),
            Arc::clone(&self.metrics),
        )?;

        let mut guard = self.open.lock().expect("table cache poisoned");
        let entry = guard.entry(number).or_insert(reader);
        Ok(Arc::clone(entry))
    }

    /// Drops the cached reader for `number` (e.g. after the file is compacted
    /// away). The underlying file is closed once all outstanding `Arc`s drop.
    pub fn evict(&self, number: u64) {
        self.open
            .lock()
            .expect("table cache poisoned")
            .remove(&number);
    }

    /// Number of currently-open tables.
    pub fn open_count(&self) -> usize {
        self.open.lock().expect("table cache poisoned").len()
    }
}
