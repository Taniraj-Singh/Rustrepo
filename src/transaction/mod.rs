//! Atomic write batches and the transaction facade.
//!
//! A [`WriteBatch`] groups several puts and deletes so they apply all-or-nothing.
//! Its serialized form is exactly what gets appended to the WAL, so the batch is
//! both the unit of atomicity *and* the unit of durability — recovery replays
//! the same bytes it wrote. The layout is:
//!
//! ```text
//!   [ sequence: u64 ][ count: u32 ][ record ... ]
//!   record = type(1) varint(key_len) key [ varint(value_len) value ]   // value only for Put
//! ```
//!
//! The sequence number in the header is the base; the i-th record in the batch
//! is applied at `sequence + i`. The DB assigns the base at commit time.
//!
//! [`Transaction`] is a thin, ergonomic wrapper around a batch for callers who
//! prefer transactional phrasing; it is committed through
//! [`crate::Cairn::commit`].

use crate::error::{Error, Result};
use crate::key::{Sequence, ValueType};
use crate::memtable::MemTable;
use crate::parser::ByteReader;
use crate::util::coding::{get_fixed32, get_fixed64};
use crate::util::varint::put_length_prefixed;

/// Byte size of the batch header (sequence + count).
const HEADER_LEN: usize = 12;

/// An ordered group of mutations applied atomically.
#[derive(Debug, Clone)]
pub struct WriteBatch {
    // The full serialized batch: 12-byte header followed by records.
    rep: Vec<u8>,
}

impl Default for WriteBatch {
    fn default() -> WriteBatch {
        WriteBatch::new()
    }
}

impl WriteBatch {
    /// Creates an empty batch.
    pub fn new() -> WriteBatch {
        WriteBatch {
            rep: vec![0u8; HEADER_LEN],
        }
    }

    /// Adopts a serialized batch (e.g. a WAL record), validating its structure.
    pub fn from_bytes(bytes: &[u8]) -> Result<WriteBatch> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::corruption("write batch: shorter than header"));
        }
        let batch = WriteBatch {
            rep: bytes.to_vec(),
        };
        // Validate by walking every record; rejects malformed WAL content.
        batch.for_each(|_, _, _| Ok(()))?;
        Ok(batch)
    }

    /// Queues setting `key` to `value`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.rep.push(ValueType::Value.as_byte());
        put_length_prefixed(&mut self.rep, key);
        put_length_prefixed(&mut self.rep, value);
        self.set_count(self.count() + 1);
    }

    /// Queues deleting `key`.
    pub fn delete(&mut self, key: &[u8]) {
        self.rep.push(ValueType::Deletion.as_byte());
        put_length_prefixed(&mut self.rep, key);
        self.set_count(self.count() + 1);
    }

    /// The number of operations in the batch.
    pub fn count(&self) -> u32 {
        get_fixed32(&self.rep[8..12]).unwrap_or(0)
    }

    fn set_count(&mut self, n: u32) {
        self.rep[8..12].copy_from_slice(&n.to_le_bytes());
    }

    /// The base sequence number stamped in the header.
    pub fn sequence(&self) -> Sequence {
        get_fixed64(&self.rep[0..8]).unwrap_or(0)
    }

    /// Stamps the base sequence number (done by the DB at commit).
    pub fn set_sequence(&mut self, seq: Sequence) {
        self.rep[0..8].copy_from_slice(&seq.to_le_bytes());
    }

    /// Whether the batch has no operations.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// The serialized batch bytes (the WAL payload).
    pub fn as_bytes(&self) -> &[u8] {
        &self.rep
    }

    /// Approximate serialized size in bytes.
    pub fn approximate_size(&self) -> usize {
        self.rep.len()
    }

    /// Clears the batch, keeping capacity.
    pub fn clear(&mut self) {
        self.rep.clear();
        self.rep.resize(HEADER_LEN, 0);
    }

    /// Invokes `f` for each operation in order.
    ///
    /// Validates that the record stream is well-formed and that the number of
    /// records matches the header count — the two ways a serialized batch read
    /// from a WAL can be corrupt.
    pub fn for_each<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(ValueType, &[u8], &[u8]) -> Result<()>,
    {
        let mut r = ByteReader::new(&self.rep[HEADER_LEN..]);
        let expected = self.count();
        let mut seen = 0u32;
        while !r.is_empty() {
            let tag = r.read_u8()?;
            let value_type = ValueType::from_byte(tag)?;
            let key = r.read_length_prefixed()?;
            let value = match value_type {
                ValueType::Value => r.read_length_prefixed()?,
                ValueType::Deletion => &[][..],
            };
            f(value_type, key, value)?;
            seen += 1;
            if seen > expected {
                return Err(Error::corruption("write batch: more records than count"));
            }
        }
        if seen != expected {
            return Err(Error::corruption("write batch: record count mismatch"));
        }
        Ok(())
    }

    /// Applies every operation to `memtable`, assigning sequence numbers from the
    /// batch's base. Returns the number of operations applied.
    pub fn insert_into(&self, memtable: &mut MemTable) -> Result<u32> {
        let base = self.sequence();
        let mut i: u64 = 0;
        self.for_each(|value_type, key, value| {
            memtable.add(base + i, value_type, key, value);
            i += 1;
            Ok(())
        })?;
        Ok(i as u32)
    }
}

/// A transactional facade over a [`WriteBatch`].
///
/// Stages mutations and is committed atomically through
/// [`crate::Cairn::commit`]. Reads during a transaction go through the DB (and
/// optionally a snapshot); this type owns only the pending writes.
#[derive(Debug, Default)]
pub struct Transaction {
    batch: WriteBatch,
}

impl Transaction {
    /// Starts an empty transaction.
    pub fn new() -> Transaction {
        Transaction {
            batch: WriteBatch::new(),
        }
    }

    /// Stages a put.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> &mut Self {
        self.batch.put(key, value);
        self
    }

    /// Stages a delete.
    pub fn delete(&mut self, key: &[u8]) -> &mut Self {
        self.batch.delete(key);
        self
    }

    /// Number of staged operations.
    pub fn len(&self) -> u32 {
        self.batch.count()
    }

    /// Whether nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    /// Consumes the transaction, yielding its batch for commit.
    pub fn into_batch(self) -> WriteBatch {
        self.batch
    }

    /// Borrows the underlying batch.
    pub fn batch(&self) -> &WriteBatch {
        &self.batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_records_and_counts() {
        let mut b = WriteBatch::new();
        b.put(b"a", b"1");
        b.put(b"b", b"2");
        b.delete(b"a");
        assert_eq!(b.count(), 3);

        let mut ops = Vec::new();
        b.for_each(|t, k, v| {
            ops.push((t, k.to_vec(), v.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(ops[0], (ValueType::Value, b"a".to_vec(), b"1".to_vec()));
        assert_eq!(ops[2], (ValueType::Deletion, b"a".to_vec(), Vec::new()));
    }

    #[test]
    fn batch_roundtrips_through_bytes() {
        let mut b = WriteBatch::new();
        b.set_sequence(500);
        b.put(b"key", b"value");
        b.delete(b"gone");
        let bytes = b.as_bytes().to_vec();
        let decoded = WriteBatch::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.sequence(), 500);
        assert_eq!(decoded.count(), 2);
    }

    #[test]
    fn insert_assigns_sequential_sequences() {
        let mut b = WriteBatch::new();
        b.set_sequence(10);
        b.put(b"k", b"v1");
        b.put(b"k", b"v2");
        let mut m = MemTable::new();
        assert_eq!(b.insert_into(&mut m).unwrap(), 2);
        // Second put has sequence 11 and should win.
        assert_eq!(
            m.get(b"k", 100),
            crate::memtable::LookupResult::Found(b"v2".to_vec())
        );
    }

    #[test]
    fn corrupt_batch_is_rejected() {
        // Header claims count 5 but there are no records.
        let mut rep = vec![0u8; HEADER_LEN];
        rep[8..12].copy_from_slice(&5u32.to_le_bytes());
        assert!(WriteBatch::from_bytes(&rep).is_err());

        // Truncated record (type byte with no key).
        let mut rep2 = vec![0u8; HEADER_LEN];
        rep2[8..12].copy_from_slice(&1u32.to_le_bytes());
        rep2.push(ValueType::Value.as_byte());
        assert!(WriteBatch::from_bytes(&rep2).is_err());
    }

    #[test]
    fn transaction_wraps_batch() {
        let mut t = Transaction::new();
        t.put(b"x", b"1").delete(b"y");
        assert_eq!(t.len(), 2);
        let batch = t.into_batch();
        assert_eq!(batch.count(), 2);
    }
}
