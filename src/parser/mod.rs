//! The decoding half of Cairn's binary formats — and the engine's primary
//! trust boundary.
//!
//! [`ByteReader`] is a cursor over a borrowed byte slice. Every read validates
//! that enough bytes remain *before* consuming them, so decoding hostile or
//! truncated input yields a structured [`Error`] instead of a panic or an
//! out-of-bounds access. Reads that return slices ([`ByteReader::read_bytes`],
//! [`ByteReader::read_length_prefixed`]) borrow from the underlying buffer,
//! enabling zero-copy parsing of table blocks.
//!
//! For length-prefixed data read from untrusted files, prefer
//! [`ByteReader::read_length_prefixed_capped`], which rejects an absurd length
//! field before it is used to slice — the difference between "reject this file"
//! and "trust a number an attacker chose".

use crate::error::{Error, Result};
use crate::util::varint::{decode_varint32, decode_varint64};

/// A bounds-checked forward cursor over a byte slice.
#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    /// Wraps `buf`, positioned at its start.
    pub fn new(buf: &'a [u8]) -> ByteReader<'a> {
        ByteReader { buf, pos: 0 }
    }

    /// The current read offset from the start of the buffer.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The total length of the underlying buffer.
    pub fn total_len(&self) -> usize {
        self.buf.len()
    }

    /// Number of unread bytes remaining.
    pub fn remaining_len(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the cursor has reached the end of the buffer.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Borrows the not-yet-consumed bytes without advancing.
    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// Reads one byte.
    pub fn read_u8(&mut self) -> Result<u8> {
        let &b = self.buf.get(self.pos).ok_or(Error::eof("u8"))?;
        self.pos += 1;
        Ok(b)
    }

    /// Reads a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_array::<4>("u32")?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Reads a little-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_array::<8>("u64")?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Reads exactly `N` bytes into a fixed-size array.
    fn read_array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N]> {
        let end = self.pos.checked_add(N).ok_or(Error::eof(what))?;
        let slice = self.buf.get(self.pos..end).ok_or(Error::eof(what))?;
        let arr: [u8; N] = slice.try_into().expect("slice length equals N");
        self.pos = end;
        Ok(arr)
    }

    /// Reads exactly `n` bytes, returning a borrowed slice.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::eof("bytes"))?;
        let slice = self.buf.get(self.pos..end).ok_or(Error::eof("bytes"))?;
        self.pos = end;
        Ok(slice)
    }

    /// Reads a `u32` varint.
    pub fn read_varint32(&mut self) -> Result<u32> {
        let (v, n) = decode_varint32(self.remaining())?;
        self.pos += n;
        Ok(v)
    }

    /// Reads a `u64` varint.
    pub fn read_varint64(&mut self) -> Result<u64> {
        let (v, n) = decode_varint64(self.remaining())?;
        self.pos += n;
        Ok(v)
    }

    /// Reads a varint-length-prefixed byte slice.
    pub fn read_length_prefixed(&mut self) -> Result<&'a [u8]> {
        let len = self.read_varint64()?;
        let len = usize::try_from(len).map_err(|_| Error::eof("length-prefixed slice"))?;
        self.read_bytes(len)
    }

    /// Reads a varint-length-prefixed slice, rejecting a length above `max`.
    ///
    /// Use this for any length read from an untrusted file: it fails with
    /// [`Error::LimitExceeded`] before the length is used to slice, so a
    /// corrupt file cannot even attempt an over-large read.
    pub fn read_length_prefixed_capped(&mut self, max: usize) -> Result<&'a [u8]> {
        let len = self.read_varint64()?;
        if len > max as u64 {
            return Err(Error::limit("length-prefixed slice", len, max as u64));
        }
        self.read_bytes(len as usize)
    }

    /// Advances the cursor by `n` bytes without returning them.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        let end = self.pos.checked_add(n).ok_or(Error::eof("skip"))?;
        if end > self.buf.len() {
            return Err(Error::eof("skip"));
        }
        self.pos = end;
        Ok(())
    }

    /// Consumes and returns all remaining bytes.
    pub fn take_rest(&mut self) -> &'a [u8] {
        let rest = &self.buf[self.pos..];
        self.pos = self.buf.len();
        rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_bounds_checked() {
        let mut r = ByteReader::new(&[1, 2, 3]);
        assert_eq!(r.read_u8().unwrap(), 1);
        assert!(r.read_u32().is_err()); // only two bytes left
        assert_eq!(r.remaining_len(), 2);
    }

    #[test]
    fn read_bytes_borrows_and_advances() {
        let data = [10, 20, 30, 40];
        let mut r = ByteReader::new(&data);
        assert_eq!(r.read_bytes(2).unwrap(), &[10, 20]);
        assert_eq!(r.position(), 2);
        assert_eq!(r.take_rest(), &[30, 40]);
        assert!(r.is_empty());
    }

    #[test]
    fn length_prefixed_cap_rejects_giant_length() {
        // Varint 100000 followed by no data.
        let mut buf = Vec::new();
        crate::util::varint::put_varint64(&mut buf, 100_000);
        let mut r = ByteReader::new(&buf);
        assert!(matches!(
            r.read_length_prefixed_capped(1024),
            Err(Error::LimitExceeded { .. })
        ));
    }

    #[test]
    fn skip_past_end_is_error() {
        let mut r = ByteReader::new(&[1, 2]);
        assert!(r.skip(5).is_err());
        assert!(r.skip(usize::MAX).is_err());
    }

    #[test]
    fn varints_advance_correctly() {
        let mut buf = Vec::new();
        crate::util::varint::put_varint32(&mut buf, 424242);
        buf.push(0xEE);
        let mut r = ByteReader::new(&buf);
        assert_eq!(r.read_varint32().unwrap(), 424242);
        assert_eq!(r.read_u8().unwrap(), 0xEE);
        assert!(r.is_empty());
    }
}
