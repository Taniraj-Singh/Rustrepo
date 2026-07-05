//! Internal keys, sequence numbers, and value types.
//!
//! A user of Cairn deals in plain byte-string keys. Internally, every stored
//! entry is tagged with a **sequence number** (a monotonically increasing
//! version counter) and a **value type** (whether the entry sets a value or
//! deletes the key). These three pieces combine into an *internal key*:
//!
//! ```text
//!   internal_key = user_key ++ little_endian_u64(trailer)
//!   trailer      = (sequence << 8) | value_type
//! ```
//!
//! The eight-byte trailer packs a 56-bit sequence number and an 8-bit type.
//! Ordering internal keys (see
//! [`InternalKeyComparator`](crate::util::comparator::InternalKeyComparator))
//! sorts by user key ascending, then by sequence *descending*, so the most
//! recent version of any key is found first.

use crate::error::{Error, Result};
use crate::util::coding::{get_fixed64, put_fixed64};
use crate::util::varint::{decode_varint64, put_varint64};

/// A version counter. Every mutation gets a unique, increasing sequence number;
/// snapshots and reads are positioned at a particular sequence.
pub type Sequence = u64;

/// The largest representable sequence number. The top eight bits of the trailer
/// are reserved for the value type, leaving 56 bits for the sequence.
pub const MAX_SEQUENCE: Sequence = (1u64 << 56) - 1;

/// Width in bytes of the packed trailer appended to every user key.
pub const TRAILER_LEN: usize = 8;

/// Distinguishes a value write from a deletion (tombstone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueType {
    /// The key is deleted at this sequence (a tombstone).
    Deletion = 0,
    /// The key is set to an associated value at this sequence.
    Value = 1,
}

/// The value type used when constructing a lookup key.
///
/// It is the numerically largest type, which — combined with descending trailer
/// order — makes a lookup at sequence `s` land on the newest entry whose
/// sequence is `<= s`, regardless of that entry's own type.
pub const VALUE_TYPE_FOR_SEEK: ValueType = ValueType::Value;

impl ValueType {
    /// Returns the on-disk byte for this type.
    pub fn as_byte(self) -> u8 {
        self as u8
    }

    /// Parses a type byte, rejecting anything outside the known set.
    pub fn from_byte(b: u8) -> Result<ValueType> {
        match b {
            0 => Ok(ValueType::Deletion),
            1 => Ok(ValueType::Value),
            other => Err(Error::corruption(format!("unknown value type {other}"))),
        }
    }
}

/// Packs a sequence number and value type into the eight-byte trailer.
///
/// The sequence is masked to 56 bits; callers must ensure it does not exceed
/// [`MAX_SEQUENCE`] (the engine guarantees this by construction).
pub fn pack_trailer(seq: Sequence, t: ValueType) -> u64 {
    debug_assert!(seq <= MAX_SEQUENCE, "sequence overflows 56 bits");
    (seq << 8) | (t.as_byte() as u64)
}

/// Extracts the sequence number from a trailer.
pub fn sequence_of(trailer: u64) -> Sequence {
    trailer >> 8
}

/// Extracts and validates the value type from a trailer.
pub fn value_type_of(trailer: u64) -> Result<ValueType> {
    ValueType::from_byte((trailer & 0xff) as u8)
}

/// An owned internal key: user key bytes followed by the packed trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalKey {
    rep: Vec<u8>,
}

impl InternalKey {
    /// Builds an internal key from its parts.
    pub fn new(user_key: &[u8], seq: Sequence, t: ValueType) -> InternalKey {
        let mut rep = Vec::with_capacity(user_key.len() + TRAILER_LEN);
        rep.extend_from_slice(user_key);
        put_fixed64(&mut rep, pack_trailer(seq, t));
        InternalKey { rep }
    }

    /// Adopts pre-encoded bytes as an internal key after validating length.
    pub fn decode(bytes: &[u8]) -> Result<InternalKey> {
        // Validate that a trailer is present and its type byte is known.
        ParsedInternalKey::parse(bytes)?;
        Ok(InternalKey {
            rep: bytes.to_vec(),
        })
    }

    /// The encoded internal key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.rep
    }

    /// Consumes the key, returning the owned bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.rep
    }

    /// The user key portion (everything but the trailer).
    pub fn user_key(&self) -> &[u8] {
        &self.rep[..self.rep.len() - TRAILER_LEN]
    }

    /// The sequence number encoded in the trailer.
    pub fn sequence(&self) -> Sequence {
        let trailer = get_fixed64(&self.rep[self.rep.len() - TRAILER_LEN..]).unwrap_or(0);
        sequence_of(trailer)
    }

    /// The value type encoded in the trailer.
    pub fn value_type(&self) -> ValueType {
        let trailer = get_fixed64(&self.rep[self.rep.len() - TRAILER_LEN..]).unwrap_or(0);
        value_type_of(trailer).unwrap_or(ValueType::Value)
    }
}

/// A borrowed, parsed view over an encoded internal key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedInternalKey<'a> {
    /// The user key (no trailer).
    pub user_key: &'a [u8],
    /// The decoded sequence number.
    pub sequence: Sequence,
    /// The decoded value type.
    pub value_type: ValueType,
}

impl<'a> ParsedInternalKey<'a> {
    /// Parses an encoded internal key, validating the trailer.
    ///
    /// Fails with [`Error::Corruption`] if the key is shorter than the trailer
    /// or the type byte is unknown — the two ways an internal key read from
    /// disk can be malformed.
    pub fn parse(internal_key: &'a [u8]) -> Result<ParsedInternalKey<'a>> {
        if internal_key.len() < TRAILER_LEN {
            return Err(Error::corruption("internal key shorter than trailer"));
        }
        let split = internal_key.len() - TRAILER_LEN;
        let user_key = &internal_key[..split];
        let trailer = get_fixed64(&internal_key[split..])?;
        let value_type = value_type_of(trailer)?;
        Ok(ParsedInternalKey {
            user_key,
            sequence: sequence_of(trailer),
            value_type,
        })
    }
}

/// A key used to probe the memtable and tables for a user key at a snapshot.
///
/// Its byte layout is a length-prefixed internal key, matching the encoding the
/// memtable stores, so a lookup can be performed with a single comparison
/// against stored entries:
///
/// ```text
///   memtable_key = varint(user_key.len() + 8) ++ user_key ++ trailer
///                  \_________ internal_key ranges over this ________/
/// ```
#[derive(Debug, Clone)]
pub struct LookupKey {
    // `rep` holds the full memtable key; `internal_offset` marks where the
    // length prefix ends and the internal key begins.
    rep: Vec<u8>,
    internal_offset: usize,
}

impl LookupKey {
    /// Builds a lookup key for `user_key` at snapshot `seq`.
    pub fn new(user_key: &[u8], seq: Sequence) -> LookupKey {
        let internal_len = user_key.len() + TRAILER_LEN;
        let mut rep = Vec::with_capacity(internal_len + 5);
        put_varint64(&mut rep, internal_len as u64);
        let internal_offset = rep.len();
        rep.extend_from_slice(user_key);
        put_fixed64(&mut rep, pack_trailer(seq, VALUE_TYPE_FOR_SEEK));
        LookupKey {
            rep,
            internal_offset,
        }
    }

    /// The full length-prefixed key, as stored in the memtable.
    pub fn memtable_key(&self) -> &[u8] {
        &self.rep
    }

    /// The internal key (user key + trailer), without the length prefix.
    pub fn internal_key(&self) -> &[u8] {
        &self.rep[self.internal_offset..]
    }

    /// The user key being looked up.
    pub fn user_key(&self) -> &[u8] {
        &self.rep[self.internal_offset..self.rep.len() - TRAILER_LEN]
    }
}

/// Extracts the user key from a length-prefixed memtable key.
///
/// Used by the memtable iterator, which stores entries in this layout. Returns
/// an error on a truncated prefix or a length that runs past the buffer.
pub fn extract_user_key_from_memtable(entry: &[u8]) -> Result<&[u8]> {
    let (internal_len, consumed) = decode_varint64(entry)?;
    let internal_len =
        usize::try_from(internal_len).map_err(|_| Error::corruption("memtable key too long"))?;
    if internal_len < TRAILER_LEN {
        return Err(Error::corruption("memtable internal key too short"));
    }
    let internal = entry
        .get(consumed..consumed + internal_len)
        .ok_or(Error::eof("memtable key"))?;
    Ok(&internal[..internal_len - TRAILER_LEN])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailer_pack_unpack() {
        let t = pack_trailer(0x00ab_cdef, ValueType::Deletion);
        assert_eq!(sequence_of(t), 0x00ab_cdef);
        assert_eq!(value_type_of(t).unwrap(), ValueType::Deletion);
    }

    #[test]
    fn value_type_byte_roundtrip() {
        assert_eq!(ValueType::from_byte(0).unwrap(), ValueType::Deletion);
        assert_eq!(ValueType::from_byte(1).unwrap(), ValueType::Value);
        assert!(ValueType::from_byte(2).is_err());
    }

    #[test]
    fn internal_key_accessors() {
        let ik = InternalKey::new(b"the-key", 1234, ValueType::Value);
        assert_eq!(ik.user_key(), b"the-key");
        assert_eq!(ik.sequence(), 1234);
        assert_eq!(ik.value_type(), ValueType::Value);

        let parsed = ParsedInternalKey::parse(ik.as_bytes()).unwrap();
        assert_eq!(parsed.user_key, b"the-key");
        assert_eq!(parsed.sequence, 1234);
    }

    #[test]
    fn parse_rejects_short_and_bad_type() {
        assert!(ParsedInternalKey::parse(b"short").is_err());
        let mut k = b"user".to_vec();
        put_fixed64(&mut k, pack_trailer(1, ValueType::Value));
        // Corrupt the type byte to an unknown value.
        let n = k.len();
        k[n - 8] = 9;
        assert!(ParsedInternalKey::parse(&k).is_err());
    }

    #[test]
    fn lookup_key_layout() {
        let lk = LookupKey::new(b"abc", 77);
        assert_eq!(lk.user_key(), b"abc");
        assert_eq!(lk.internal_key().len(), 3 + TRAILER_LEN);
        let recovered = extract_user_key_from_memtable(lk.memtable_key()).unwrap();
        assert_eq!(recovered, b"abc");
    }

    #[test]
    fn extract_rejects_garbage() {
        assert!(extract_user_key_from_memtable(&[0x80]).is_err()); // bad varint
        assert!(extract_user_key_from_memtable(&[0x02, 0x00]).is_err()); // len<trailer
    }
}
