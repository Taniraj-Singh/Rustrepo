//! The encoding half of Cairn's binary formats.
//!
//! [`Encoder`] is a thin, ergonomic wrapper over a growable byte buffer. It
//! centralizes every "append an integer / length-prefixed blob" operation used
//! when writing WAL records, blocks, index entries, and manifest edits, so the
//! wire layout lives in one place and cannot drift between call sites.
//!
//! The matching decode side is [`crate::parser::ByteReader`]. Keeping the two
//! symmetric — every `put_*` here has a `read_*` there — makes the formats easy
//! to audit.

use crate::util::coding::{put_fixed32, put_fixed64};
use crate::util::varint::{put_length_prefixed, put_varint32, put_varint64};

/// A binary encoder accumulating bytes into an owned buffer.
#[derive(Debug, Clone, Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// Creates an empty encoder.
    pub fn new() -> Encoder {
        Encoder { buf: Vec::new() }
    }

    /// Creates an encoder with room for `cap` bytes reserved up front.
    pub fn with_capacity(cap: usize) -> Encoder {
        Encoder {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Wraps an existing buffer, appending to it.
    pub fn from_vec(buf: Vec<u8>) -> Encoder {
        Encoder { buf }
    }

    /// Appends a single byte.
    pub fn put_u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    /// Appends a little-endian `u32`.
    pub fn put_u32(&mut self, v: u32) -> &mut Self {
        put_fixed32(&mut self.buf, v);
        self
    }

    /// Appends a little-endian `u64`.
    pub fn put_u64(&mut self, v: u64) -> &mut Self {
        put_fixed64(&mut self.buf, v);
        self
    }

    /// Appends a `u32` varint.
    pub fn put_varint32(&mut self, v: u32) -> &mut Self {
        put_varint32(&mut self.buf, v);
        self
    }

    /// Appends a `u64` varint.
    pub fn put_varint64(&mut self, v: u64) -> &mut Self {
        put_varint64(&mut self.buf, v);
        self
    }

    /// Appends raw bytes with no length prefix.
    pub fn put_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Appends a varint length followed by the bytes.
    pub fn put_length_prefixed(&mut self, bytes: &[u8]) -> &mut Self {
        put_length_prefixed(&mut self.buf, bytes);
        self
    }

    /// Current encoded length in bytes.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been encoded yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Borrows the encoded bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Resets the encoder, retaining allocated capacity for reuse.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Consumes the encoder, returning the accumulated bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ByteReader;

    #[test]
    fn encoder_chains_and_roundtrips() {
        let bytes = {
            let mut e = Encoder::with_capacity(32);
            e.put_u8(0xAB)
                .put_u32(0xdead_beef)
                .put_u64(0x0102_0304_0506_0708)
                .put_varint32(300)
                .put_varint64(1_000_000)
                .put_length_prefixed(b"hello");
            e.into_bytes()
        };

        let mut r = ByteReader::new(&bytes);
        assert_eq!(r.read_u8().unwrap(), 0xAB);
        assert_eq!(r.read_u32().unwrap(), 0xdead_beef);
        assert_eq!(r.read_u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.read_varint32().unwrap(), 300);
        assert_eq!(r.read_varint64().unwrap(), 1_000_000);
        assert_eq!(r.read_length_prefixed().unwrap(), b"hello");
        assert!(r.is_empty());
    }

    #[test]
    fn from_vec_appends() {
        let mut e = Encoder::from_vec(vec![1, 2, 3]);
        e.put_u8(4);
        assert_eq!(e.into_bytes(), vec![1, 2, 3, 4]);
    }
}
