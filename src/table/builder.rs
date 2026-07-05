//! Writes an SSTable file from a stream of sorted internal-key entries.
//!
//! [`TableBuilder`] is fed entries in ascending internal-key order (the memtable
//! and compaction both produce them that way). It accumulates entries into a
//! data block until the block reaches the configured size, frames and writes the
//! block, and records a separator + [`BlockHandle`] for it. On [`finish`], it
//! writes the bloom filter block, the index block (itself a normal block whose
//! values are encoded handles), and the fixed footer.

use crate::bloom;
use crate::compression::compress_or_store;
use crate::config::{CompressionKind, Config};
use crate::error::{Error, Result};
use crate::index::BlockHandle;
use crate::storage::block::frame_block;
use crate::storage::file::WritableFile;
use crate::table::block::BlockBuilder;
use crate::table::Footer;

/// Facts about a finished table, needed to register it in a version.
#[derive(Debug, Clone)]
pub struct TableBuildResult {
    /// Number of entries written.
    pub num_entries: u64,
    /// Total file size in bytes.
    pub file_size: u64,
    /// Smallest internal key in the table (`None` if empty).
    pub smallest: Option<Vec<u8>>,
    /// Largest internal key in the table (`None` if empty).
    pub largest: Option<Vec<u8>>,
}

/// Builds one SSTable into a [`WritableFile`].
#[derive(Debug)]
pub struct TableBuilder {
    file: WritableFile,
    block_size: usize,
    compression: CompressionKind,
    bloom_bits_per_key: usize,

    data_block: BlockBuilder,
    // Pending index entries: (separator internal key, data block handle).
    index_entries: Vec<(Vec<u8>, BlockHandle)>,
    bloom_hashes: Vec<u32>,

    offset: u64,
    num_entries: u64,
    smallest: Option<Vec<u8>>,
    largest: Vec<u8>,
    finished: bool,
}

impl TableBuilder {
    /// Creates a builder writing into `file`, configured by `config`.
    pub fn new(file: WritableFile, config: &Config) -> TableBuilder {
        TableBuilder {
            file,
            block_size: config.block_size,
            compression: config.compression,
            bloom_bits_per_key: config.bloom_bits_per_key,
            data_block: BlockBuilder::new(config.block_restart_interval),
            index_entries: Vec::new(),
            bloom_hashes: Vec::new(),
            offset: 0,
            num_entries: 0,
            smallest: None,
            largest: Vec::new(),
            finished: false,
        }
    }

    /// Number of entries added so far.
    pub fn num_entries(&self) -> u64 {
        self.num_entries
    }

    /// Bytes written to the file so far (excludes buffered data and the footer).
    pub fn file_offset(&self) -> u64 {
        self.offset
    }

    /// Adds one entry. `internal_key` must be greater than every prior key.
    pub fn add(&mut self, internal_key: &[u8], value: &[u8]) -> Result<()> {
        if self.finished {
            return Err(Error::invalid("TableBuilder::add after finish"));
        }
        if internal_key.len() < 8 {
            return Err(Error::corruption(
                "table: internal key shorter than trailer",
            ));
        }

        // Feed the bloom filter with the user-key portion.
        if self.bloom_bits_per_key > 0 {
            let user_key = &internal_key[..internal_key.len() - 8];
            self.bloom_hashes.push(bloom::hash_key(user_key));
        }

        if self.smallest.is_none() {
            self.smallest = Some(internal_key.to_vec());
        }
        self.largest.clear();
        self.largest.extend_from_slice(internal_key);

        self.data_block.add(internal_key, value);
        self.num_entries += 1;

        if self.data_block.size_estimate() >= self.block_size {
            self.flush_data_block()?;
        }
        Ok(())
    }

    /// Frames and writes the current data block, recording its index entry.
    fn flush_data_block(&mut self) -> Result<()> {
        if self.data_block.is_empty() {
            return Ok(());
        }
        let separator = self.data_block.last_key().to_vec();
        let payload = self.data_block.finish().to_vec();
        let handle = self.write_block(&payload)?;
        self.index_entries.push((separator, handle));
        self.data_block.reset();
        Ok(())
    }

    /// Compresses (if worthwhile), frames, and appends a block; returns its
    /// handle. The handle's `size` is the payload length; the reader adds the
    /// trailer.
    fn write_block(&mut self, payload: &[u8]) -> Result<BlockHandle> {
        let (kind, body) = compress_or_store(self.compression, payload);
        let framed = frame_block(&body, kind);
        let handle = BlockHandle::new(self.offset, body.len() as u64);
        self.file.append(&framed)?;
        self.offset += framed.len() as u64;
        Ok(handle)
    }

    /// Finishes the table: filter block, index block, footer, and fsync.
    pub fn finish(mut self) -> Result<TableBuildResult> {
        if self.finished {
            return Err(Error::invalid("TableBuilder finished twice"));
        }
        self.flush_data_block()?;

        // Filter block: a single bloom filter over every key, if enabled.
        let filter_handle = if self.bloom_bits_per_key > 0 && !self.bloom_hashes.is_empty() {
            let filter = bloom::build_from_hashes(&self.bloom_hashes, self.bloom_bits_per_key);
            self.write_block(&filter)?
        } else {
            BlockHandle::new(self.offset, 0)
        };

        // Index block: reuse the block format, one entry per data block.
        let mut index_builder = BlockBuilder::new(1);
        for (separator, handle) in &self.index_entries {
            index_builder.add(separator, &handle.to_vec());
        }
        let index_payload = index_builder.finish().to_vec();
        let index_handle = self.write_block(&index_payload)?;

        // Footer at the very end.
        let footer = Footer {
            index_handle,
            filter_handle,
        };
        let footer_bytes = footer.encode();
        self.file.append(&footer_bytes)?;
        self.offset += footer_bytes.len() as u64;

        self.file.sync()?;
        self.finished = true;

        Ok(TableBuildResult {
            num_entries: self.num_entries,
            file_size: self.offset,
            smallest: self.smallest,
            largest: if self.num_entries == 0 {
                None
            } else {
                Some(self.largest)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{InternalKey, ValueType};
    use crate::table::TableReader;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-tablebuild-{}-{}", std::process::id(), name));
        p
    }

    #[test]
    fn builds_a_table_that_reads_back() {
        let path = temp_path("basic");
        let config = Config::default();
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = (0..500u32)
            .map(|i| {
                (
                    InternalKey::new(format!("key{i:05}").as_bytes(), 1, ValueType::Value)
                        .into_bytes(),
                    format!("value-{i}").into_bytes(),
                )
            })
            .collect();
        entries.sort();

        let result = {
            let file = WritableFile::create(&path).unwrap();
            let mut b = TableBuilder::new(file, &config);
            for (k, v) in &entries {
                b.add(k, v).unwrap();
            }
            b.finish().unwrap()
        };
        assert_eq!(result.num_entries, 500);
        assert!(result.file_size > 0);

        // Read it back and check a handful of point lookups.
        let file = crate::storage::RandomAccessFile::open(&path).unwrap();
        let cache = Arc::new(crate::cache::BlockCache::new(1 << 20));
        let metrics = Arc::new(crate::util::metrics::Metrics::new());
        let reader = TableReader::open(file, 1, &config, cache, metrics).unwrap();

        for i in [0u32, 42, 250, 499] {
            let user = format!("key{i:05}");
            let lookup = crate::key::LookupKey::new(user.as_bytes(), 100);
            let got = reader.get(lookup.internal_key(), user.as_bytes()).unwrap();
            assert_eq!(got.unwrap().1, format!("value-{i}").into_bytes());
        }
        // A key that does not exist.
        let miss = crate::key::LookupKey::new(b"nope", 100);
        assert!(reader.get(miss.internal_key(), b"nope").unwrap().is_none());

        std::fs::remove_file(&path).ok();
    }
}
