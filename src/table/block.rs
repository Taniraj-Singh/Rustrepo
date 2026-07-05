//! The data-block format: prefix-compressed sorted entries with restart points.
//!
//! A block stores a run of sorted `(key, value)` entries. To save space, each
//! key records only the bytes that differ from its predecessor:
//!
//! ```text
//!   entry = varint(shared) varint(non_shared) varint(value_len)
//!           key_delta[non_shared] value[value_len]
//!   key   = previous_key[..shared] ++ key_delta
//! ```
//!
//! Every `restart_interval` entries the prefix compression resets — a *restart
//! point* stores a full key (`shared == 0`). The block ends with the array of
//! restart offsets and their count:
//!
//! ```text
//!   [ entries... ][ restart_u32 * num_restarts ][ num_restarts: u32 ]
//! ```
//!
//! Restart points make [`BlockIter::seek`] a binary search over restarts
//! followed by a short linear scan, rather than a scan from the block start.
//!
//! The reader ([`Block`] / [`BlockIter`]) treats the bytes as untrusted: every
//! offset, shared-prefix length, and entry extent is validated against the block
//! bounds before use, so a corrupt block makes the iterator invalid (with a
//! recorded [`status`](crate::iterator::KvIterator::status)) rather than panics.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::iterator::KvIterator;
use crate::parser::ByteReader;
use crate::util::bytes::common_prefix_len;
use crate::util::coding::{get_fixed32, put_fixed32};
use crate::util::comparator::{Comparator, InternalKeyComparator};
use crate::util::varint::{put_varint32, varint_len};

/// Builds a single prefix-compressed block from sorted entries.
#[derive(Debug)]
pub struct BlockBuilder {
    buf: Vec<u8>,
    restarts: Vec<u32>,
    restart_interval: usize,
    counter: usize,
    last_key: Vec<u8>,
    finished: bool,
}

impl BlockBuilder {
    /// Creates a builder that resets prefix compression every `restart_interval`
    /// entries.
    pub fn new(restart_interval: usize) -> BlockBuilder {
        BlockBuilder {
            buf: Vec::new(),
            // The first entry is always a restart at offset 0.
            restarts: vec![0],
            restart_interval: restart_interval.max(1),
            counter: 0,
            last_key: Vec::new(),
            finished: false,
        }
    }

    /// Whether nothing has been added yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// An estimate of the finished block size, used to decide when to flush.
    pub fn size_estimate(&self) -> usize {
        self.buf.len() + self.restarts.len() * 4 + 4
    }

    /// The most recently added key (the block's current largest key).
    pub fn last_key(&self) -> &[u8] {
        &self.last_key
    }

    /// Appends an entry.
    ///
    /// Keys must be added in the caller's comparator order (internal-key order
    /// for the engine's data blocks). Because a block is comparator-agnostic —
    /// it only prefix-compresses and records restarts — the ordering contract is
    /// the caller's to keep; the reader applies the same comparator on seek.
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        debug_assert!(!self.finished);

        let shared = if self.counter < self.restart_interval {
            common_prefix_len(&self.last_key, key)
        } else {
            // Start a new restart run with a full key.
            self.restarts.push(self.buf.len() as u32);
            self.counter = 0;
            0
        };
        let non_shared = key.len() - shared;

        put_varint32(&mut self.buf, shared as u32);
        put_varint32(&mut self.buf, non_shared as u32);
        put_varint32(&mut self.buf, value.len() as u32);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.extend_from_slice(value);

        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.counter += 1;
    }

    /// Finalizes the block, appending the restart array, and returns its bytes.
    pub fn finish(&mut self) -> &[u8] {
        for &r in &self.restarts {
            put_fixed32(&mut self.buf, r);
        }
        put_fixed32(&mut self.buf, self.restarts.len() as u32);
        self.finished = true;
        &self.buf
    }

    /// Clears the builder for reuse, keeping its allocation.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.restarts.clear();
        self.restarts.push(0);
        self.counter = 0;
        self.last_key.clear();
        self.finished = false;
    }

    /// Exact encoded size a key/value pair would add (for size accounting).
    pub fn estimate_entry_cost(key: &[u8], value: &[u8]) -> usize {
        varint_len(key.len() as u64)
            + varint_len(key.len() as u64)
            + varint_len(value.len() as u64)
            + key.len()
            + value.len()
    }
}

/// A parsed, immutable block ready to be iterated.
#[derive(Debug, Clone)]
pub struct Block {
    data: Arc<[u8]>,
    // Offset where the restart array begins (= end of the entries region).
    restart_offset: usize,
    num_restarts: usize,
}

impl Block {
    /// Parses the structural trailer of a decoded block payload.
    ///
    /// Validates that the restart count and array fit within the block, so all
    /// later offset math in [`BlockIter`] operates on a trustworthy layout.
    pub fn new(data: Arc<[u8]>) -> Result<Block> {
        if data.len() < 4 {
            return Err(Error::corruption("block: shorter than restart count"));
        }
        let num_restarts = get_fixed32(&data[data.len() - 4..])? as usize;
        // Space needed for the restart array plus its count.
        let trailer = num_restarts
            .checked_mul(4)
            .and_then(|x| x.checked_add(4))
            .ok_or_else(|| Error::corruption("block: restart array size overflows"))?;
        if trailer > data.len() {
            return Err(Error::corruption("block: restart array exceeds block"));
        }
        let restart_offset = data.len() - trailer;
        Ok(Block {
            data,
            restart_offset,
            num_restarts,
        })
    }

    /// Number of restart points.
    pub fn restart_count(&self) -> usize {
        self.num_restarts
    }

    /// Creates a cursor over this block using internal-key ordering.
    pub fn iter(&self) -> BlockIter {
        BlockIter {
            data: self.data.clone(),
            restart_offset: self.restart_offset,
            num_restarts: self.num_restarts,
            cmp: InternalKeyComparator::new(),
            next_entry: 0,
            key: Vec::new(),
            value_range: (0, 0),
            valid: false,
            error: None,
        }
    }
}

/// A cursor over a single [`Block`].
#[derive(Debug)]
pub struct BlockIter {
    data: Arc<[u8]>,
    restart_offset: usize,
    num_restarts: usize,
    cmp: InternalKeyComparator,
    // Offset of the next entry to decode within the entries region.
    next_entry: usize,
    // Reconstructed current key (owned because of prefix compression).
    key: Vec<u8>,
    // Byte range of the current value within `data`.
    value_range: (usize, usize),
    valid: bool,
    error: Option<&'static str>,
}

impl BlockIter {
    fn restart_point(&self, idx: usize) -> Result<usize> {
        let at = self.restart_offset + idx * 4;
        Ok(get_fixed32(self.data.get(at..).ok_or(Error::eof("restart"))?)? as usize)
    }

    fn set_error(&mut self, msg: &'static str) {
        self.error = Some(msg);
        self.valid = false;
    }

    /// Positions `next_entry` at restart `idx` and clears the running key.
    fn seek_to_restart(&mut self, idx: usize) {
        self.key.clear();
        match self.restart_point(idx) {
            Ok(off) => {
                self.next_entry = off;
                self.valid = false;
            }
            Err(_) => self.set_error("block: bad restart offset"),
        }
    }

    /// Decodes the entry at `next_entry` into the current position.
    fn step(&mut self) -> bool {
        if self.error.is_some() || self.next_entry >= self.restart_offset {
            self.valid = false;
            return false;
        }
        let off = self.next_entry;
        // Entry header: three varints (shared, non_shared, value_len).
        let mut r = ByteReader::new(&self.data[off..self.restart_offset]);
        let (shared, non_shared, value_len) =
            match (r.read_varint32(), r.read_varint32(), r.read_varint32()) {
                (Ok(s), Ok(n), Ok(v)) => (s as usize, n as usize, v as usize),
                _ => {
                    self.set_error("block: truncated entry header");
                    return false;
                }
            };

        let key_off = off + r.position();
        let Some(value_off) = key_off.checked_add(non_shared) else {
            self.set_error("block: key extent overflows");
            return false;
        };
        let Some(end) = value_off.checked_add(value_len) else {
            self.set_error("block: value extent overflows");
            return false;
        };
        if end > self.restart_offset {
            self.set_error("block: entry exceeds block bounds");
            return false;
        }
        if shared > self.key.len() {
            self.set_error("block: shared prefix exceeds previous key");
            return false;
        }

        self.key.truncate(shared);
        self.key.extend_from_slice(&self.data[key_off..value_off]);
        self.value_range = (value_off, end);
        self.next_entry = end;
        self.valid = true;
        true
    }

    /// Decodes just the key at restart `idx` (always a full key).
    fn key_at_restart(&self, idx: usize) -> Option<Vec<u8>> {
        let off = self.restart_point(idx).ok()?;
        if off >= self.restart_offset {
            return None;
        }
        let region = self.data.get(off..self.restart_offset)?;
        let mut r = ByteReader::new(region);
        let shared = r.read_varint32().ok()? as usize;
        let non_shared = r.read_varint32().ok()? as usize;
        let _value_len = r.read_varint32().ok()?;
        if shared != 0 {
            return None; // a restart entry must be a full key
        }
        let key_off = off + r.position();
        let end = key_off.checked_add(non_shared)?;
        self.data.get(key_off..end).map(|s| s.to_vec())
    }
}

impl KvIterator for BlockIter {
    fn valid(&self) -> bool {
        self.valid
    }

    fn seek_to_first(&mut self) {
        if self.num_restarts == 0 {
            self.valid = false;
            return;
        }
        self.seek_to_restart(0);
        self.step();
    }

    fn seek(&mut self, target: &[u8]) {
        if self.num_restarts == 0 {
            self.valid = false;
            return;
        }
        // Binary search restarts for the last one whose key is < target.
        let mut left = 0usize;
        let mut right = self.num_restarts - 1;
        while left < right {
            let mid = left + (right - left).div_ceil(2);
            match self.key_at_restart(mid) {
                Some(key) => {
                    if self.cmp.compare(&key, target) == std::cmp::Ordering::Less {
                        left = mid;
                    } else {
                        right = mid - 1;
                    }
                }
                None => {
                    self.set_error("block: bad restart key");
                    return;
                }
            }
        }
        // Linear scan from the chosen restart to the first key >= target.
        self.seek_to_restart(left);
        while self.step() {
            if self.cmp.compare(&self.key, target) != std::cmp::Ordering::Less {
                return;
            }
        }
    }

    fn next(&mut self) {
        self.step();
    }

    fn key(&self) -> &[u8] {
        &self.key
    }

    fn value(&self) -> &[u8] {
        if self.valid {
            &self.data[self.value_range.0..self.value_range.1]
        } else {
            &[]
        }
    }

    fn status(&self) -> Result<()> {
        match self.error {
            Some(msg) => Err(Error::corruption(msg)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterator::drain_to_vec;
    use crate::key::{InternalKey, ValueType};

    fn ik(user: &[u8], seq: u64) -> Vec<u8> {
        InternalKey::new(user, seq, ValueType::Value).into_bytes()
    }

    fn build(entries: &[(Vec<u8>, Vec<u8>)], interval: usize) -> Block {
        let mut b = BlockBuilder::new(interval);
        for (k, v) in entries {
            b.add(k, v);
        }
        let bytes = b.finish().to_vec();
        Block::new(Arc::from(bytes.into_boxed_slice())).unwrap()
    }

    #[test]
    fn roundtrips_all_entries() {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = (0..100u32)
            .map(|i| {
                (
                    ik(format!("key{i:04}").as_bytes(), 1),
                    format!("val{i}").into_bytes(),
                )
            })
            .collect();
        entries.sort();
        for interval in [1usize, 4, 16] {
            let block = build(&entries, interval);
            let mut it = block.iter();
            it.seek_to_first();
            assert_eq!(drain_to_vec(&mut it), entries);
        }
    }

    #[test]
    fn seek_finds_correct_entry() {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = (0..50u32)
            .map(|i| (ik(format!("k{i:03}").as_bytes(), 1), vec![i as u8]))
            .collect();
        entries.sort();
        let block = build(&entries, 8);
        let mut it = block.iter();
        it.seek(&ik(b"k025", 1));
        assert!(it.valid());
        assert_eq!(&it.key()[..4], b"k025");
    }

    #[test]
    fn seek_past_end_is_invalid() {
        let entries = vec![(ik(b"a", 1), b"1".to_vec()), (ik(b"b", 1), b"2".to_vec())];
        let block = build(&entries, 4);
        let mut it = block.iter();
        it.seek(&ik(b"z", 1));
        assert!(!it.valid());
    }

    #[test]
    fn corrupt_restart_count_is_rejected() {
        let entries = vec![(ik(b"a", 1), b"1".to_vec())];
        let mut b = BlockBuilder::new(4);
        for (k, v) in &entries {
            b.add(k, v);
        }
        let mut bytes = b.finish().to_vec();
        // Overwrite the restart count with something enormous.
        let n = bytes.len();
        bytes[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Block::new(Arc::from(bytes.into_boxed_slice())).is_err());
    }
}
