//! Sorted String Tables (SSTables): the immutable, on-disk building block of the
//! LSM tree.
//!
//! A table is written once, by [`TableBuilder`], and thereafter only read, by
//! [`TableReader`]. Its layout, from the start of the file:
//!
//! ```text
//!   [ data block 0 ]        prefix-compressed sorted entries (crate::table::block)
//!   [ data block 1 ]
//!         ...
//!   [ filter block ]        one bloom filter over all keys (optional)
//!   [ index block ]         one entry per data block: separator -> BlockHandle
//!   [ footer: 40 bytes ]    handles for the index and filter blocks + magic
//! ```
//!
//! Every block carries the storage-layer checksum/compression trailer
//! (crate::storage::block), so integrity is verified block by block on read. The
//! footer is a fixed size at a fixed position (the file's tail), which is how a
//! reader bootstraps: read the last 40 bytes, verify the magic, and follow the
//! handles inward.

pub mod block;
pub mod builder;
pub mod reader;

pub use block::{Block, BlockBuilder, BlockIter};
pub use builder::{TableBuildResult, TableBuilder};
pub use reader::TableReader;

use crate::error::{Error, Result};
use crate::index::BlockHandle;
use crate::util::coding::{get_fixed64, put_fixed64};

/// Byte size of the fixed table footer.
pub const FOOTER_SIZE: usize = 40;

/// Magic number stamped at the very end of every table file.
///
/// A reader that does not find this at the expected offset rejects the file —
/// the first line of defense against pointing the engine at a non-table.
pub const TABLE_MAGIC: u64 = 0xC0DE_CA1B_5570_0001;

/// The fixed trailer that lets a reader find a table's index and filter blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Footer {
    /// Handle of the index block.
    pub index_handle: BlockHandle,
    /// Handle of the filter (bloom) block; `size == 0` means no filter.
    pub filter_handle: BlockHandle,
}

impl Footer {
    /// Encodes the footer to its fixed 40-byte representation.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FOOTER_SIZE);
        put_fixed64(&mut out, self.index_handle.offset);
        put_fixed64(&mut out, self.index_handle.size);
        put_fixed64(&mut out, self.filter_handle.offset);
        put_fixed64(&mut out, self.filter_handle.size);
        put_fixed64(&mut out, TABLE_MAGIC);
        debug_assert_eq!(out.len(), FOOTER_SIZE);
        out
    }

    /// Decodes a footer from the trailing bytes of a table file.
    ///
    /// `bytes` must be at least [`FOOTER_SIZE`] long; only the final
    /// [`FOOTER_SIZE`] bytes are read. Fails if the magic does not match.
    pub fn decode(bytes: &[u8]) -> Result<Footer> {
        if bytes.len() < FOOTER_SIZE {
            return Err(Error::corruption("table footer: too short"));
        }
        let f = &bytes[bytes.len() - FOOTER_SIZE..];
        let index_handle = BlockHandle::new(get_fixed64_at(f, 0)?, get_fixed64_at(f, 8)?);
        let filter_handle = BlockHandle::new(get_fixed64_at(f, 16)?, get_fixed64_at(f, 24)?);
        let magic = get_fixed64_at(f, 32)?;
        if magic != TABLE_MAGIC {
            return Err(Error::corruption("table footer: bad magic"));
        }
        Ok(Footer {
            index_handle,
            filter_handle,
        })
    }
}

fn get_fixed64_at(buf: &[u8], off: usize) -> Result<u64> {
    get_fixed64(&buf[off..off + 8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_roundtrips() {
        let f = Footer {
            index_handle: BlockHandle::new(1000, 200),
            filter_handle: BlockHandle::new(900, 100),
        };
        let bytes = f.encode();
        assert_eq!(bytes.len(), FOOTER_SIZE);
        assert_eq!(Footer::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn footer_rejects_bad_magic() {
        let f = Footer::default();
        let mut bytes = f.encode();
        bytes[FOOTER_SIZE - 1] ^= 0xff;
        assert!(Footer::decode(&bytes).is_err());
    }

    #[test]
    fn footer_rejects_short_input() {
        assert!(Footer::decode(&[0u8; 10]).is_err());
    }
}
