//! Locating blocks within a table.
//!
//! A [`BlockHandle`] is an `(offset, size)` pointer into a table file — the
//! address of a block. A [`SparseIndex`] is the in-memory search structure the
//! table reader builds from a table's index block: one entry per data block,
//! holding a *separator* key that is `>=` every key in that block and `<` the
//! first key of the next. Binary searching the separators turns a key lookup
//! into "which single data block could hold this key?", so a point read touches
//! at most one data block.
//!
//! Keeping this structure in its own module separates the *on-disk* table format
//! (in [`crate::table`]) from the *in-memory* lookup logic, and lets both share
//! one definition of a block address.

use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::parser::ByteReader;
use crate::serializer::Encoder;
use crate::util::comparator::Comparator;

/// The address of a block within a table file: a byte offset and a byte length.
///
/// The length covers only the block's payload; the fixed-size block trailer
/// (compression byte + checksum) that follows on disk is added by readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockHandle {
    /// Byte offset of the block from the start of the file.
    pub offset: u64,
    /// Byte length of the block payload.
    pub size: u64,
}

impl BlockHandle {
    /// Creates a handle.
    pub fn new(offset: u64, size: u64) -> BlockHandle {
        BlockHandle { offset, size }
    }

    /// Appends this handle to `enc` as two varints (offset then size).
    pub fn encode(&self, enc: &mut Encoder) {
        enc.put_varint64(self.offset).put_varint64(self.size);
    }

    /// Encodes the handle to a fresh byte vector.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut enc = Encoder::with_capacity(20);
        self.encode(&mut enc);
        enc.into_bytes()
    }

    /// Reads a handle from `reader` (two varints).
    pub fn decode(reader: &mut ByteReader<'_>) -> Result<BlockHandle> {
        let offset = reader.read_varint64()?;
        let size = reader.read_varint64()?;
        Ok(BlockHandle { offset, size })
    }

    /// Decodes a handle from the front of `bytes`.
    pub fn decode_from(bytes: &[u8]) -> Result<BlockHandle> {
        let mut r = ByteReader::new(bytes);
        BlockHandle::decode(&mut r)
    }

    /// The `[offset, offset + size)` end position, checked for overflow.
    pub fn end(&self) -> Result<u64> {
        self.offset
            .checked_add(self.size)
            .ok_or_else(|| Error::corruption("block handle offset+size overflows"))
    }
}

/// An in-memory index of a table's data blocks, ordered by separator key.
#[derive(Debug, Default, Clone)]
pub struct SparseIndex {
    // Parallel arrays: separators[i] bounds the keys in block handles[i].
    separators: Vec<Vec<u8>>,
    handles: Vec<BlockHandle>,
}

impl SparseIndex {
    /// Creates an empty index.
    pub fn new() -> SparseIndex {
        SparseIndex::default()
    }

    /// Appends a block's separator and handle.
    ///
    /// Callers must push entries in ascending separator order (the table builder
    /// naturally does, since it emits blocks in key order).
    pub fn push(&mut self, separator: Vec<u8>, handle: BlockHandle) {
        self.separators.push(separator);
        self.handles.push(handle);
    }

    /// Number of indexed blocks.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Whether the index references no blocks.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// The handle of the `i`th block.
    pub fn handle_at(&self, i: usize) -> Option<BlockHandle> {
        self.handles.get(i).copied()
    }

    /// The separator of the `i`th block.
    pub fn separator_at(&self, i: usize) -> Option<&[u8]> {
        self.separators.get(i).map(|v| v.as_slice())
    }

    /// Finds the first block whose separator is `>= target`.
    ///
    /// That is the only data block that could contain `target`; a later block
    /// starts after `target`, and an earlier block ended before it. Returns the
    /// block index and handle, or `None` if `target` sorts after every block
    /// (and so is not in the table).
    pub fn find_block<C: Comparator + ?Sized>(
        &self,
        target: &[u8],
        cmp: &C,
    ) -> Option<(usize, BlockHandle)> {
        let mut lo = 0;
        let mut hi = self.separators.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match cmp.compare(&self.separators[mid], target) {
                Ordering::Less => lo = mid + 1,
                _ => hi = mid,
            }
        }
        if lo < self.handles.len() {
            Some((lo, self.handles[lo]))
        } else {
            None
        }
    }

    /// Iterates the handles in block order.
    pub fn handles(&self) -> impl Iterator<Item = BlockHandle> + '_ {
        self.handles.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::comparator::BytewiseComparator;

    #[test]
    fn handle_roundtrips() {
        let h = BlockHandle::new(1234, 5678);
        let bytes = h.to_vec();
        assert_eq!(BlockHandle::decode_from(&bytes).unwrap(), h);
        assert_eq!(h.end().unwrap(), 1234 + 5678);
    }

    #[test]
    fn handle_decode_rejects_truncation() {
        assert!(BlockHandle::decode_from(&[]).is_err());
        assert!(BlockHandle::decode_from(&[0x80]).is_err());
    }

    #[test]
    fn end_overflow_is_error() {
        let h = BlockHandle::new(u64::MAX, 1);
        assert!(h.end().is_err());
    }

    #[test]
    fn find_block_picks_correct_partition() {
        let cmp = BytewiseComparator;
        let mut idx = SparseIndex::new();
        idx.push(b"c".to_vec(), BlockHandle::new(0, 10));
        idx.push(b"f".to_vec(), BlockHandle::new(10, 10));
        idx.push(b"m".to_vec(), BlockHandle::new(20, 10));

        // "a" <= "c" -> block 0
        assert_eq!(idx.find_block(b"a", &cmp).unwrap().0, 0);
        // "c" -> block 0 (separator >= target)
        assert_eq!(idx.find_block(b"c", &cmp).unwrap().0, 0);
        // "d" -> block 1
        assert_eq!(idx.find_block(b"d", &cmp).unwrap().0, 1);
        // "m" -> block 2
        assert_eq!(idx.find_block(b"m", &cmp).unwrap().0, 2);
        // "z" -> past the end
        assert!(idx.find_block(b"z", &cmp).is_none());
    }

    #[test]
    fn empty_index_finds_nothing() {
        let cmp = BytewiseComparator;
        assert!(SparseIndex::new().find_block(b"x", &cmp).is_none());
    }
}
