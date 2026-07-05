//! LEB128-style variable-length integer coding.
//!
//! Varints store small integers compactly: each byte carries 7 bits of payload
//! and a continuation bit. They are used pervasively in the record and block
//! formats for lengths and offsets, where most values are small.
//!
//! Decoding is defensive: a varint that does not terminate within the maximum
//! number of bytes, or that would overflow the target type, is rejected with a
//! structured [`Error`] rather than wrapping or reading past the buffer.

use crate::error::{Error, Result};

/// Maximum bytes a `u32` varint can occupy (`ceil(32 / 7)`).
pub const MAX_VARINT32_LEN: usize = 5;
/// Maximum bytes a `u64` varint can occupy (`ceil(64 / 7)`).
pub const MAX_VARINT64_LEN: usize = 10;

/// Appends `v` to `dst` as a `u32` varint.
pub fn put_varint32(dst: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        dst.push((v as u8) | 0x80);
        v >>= 7;
    }
    dst.push(v as u8);
}

/// Appends `v` to `dst` as a `u64` varint.
pub fn put_varint64(dst: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        dst.push((v as u8) | 0x80);
        v >>= 7;
    }
    dst.push(v as u8);
}

/// Appends a length-prefixed byte slice: `varint(len)` followed by the bytes.
pub fn put_length_prefixed(dst: &mut Vec<u8>, bytes: &[u8]) {
    put_varint64(dst, bytes.len() as u64);
    dst.extend_from_slice(bytes);
}

/// Number of bytes `v` occupies when encoded as a `u64` varint.
pub fn varint_len(v: u64) -> usize {
    let mut n = 1;
    let mut v = v;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// Decodes a `u32` varint from the front of `src`.
///
/// Returns the value and the number of bytes consumed. Fails with
/// [`Error::UnexpectedEof`] if the buffer ends mid-varint and
/// [`Error::Corruption`] if the encoding is longer than five bytes or overflows
/// a `u32`.
pub fn decode_varint32(src: &[u8]) -> Result<(u32, usize)> {
    let mut result: u32 = 0;
    let mut shift = 0;
    for (i, &byte) in src.iter().enumerate().take(MAX_VARINT32_LEN) {
        let payload = (byte & 0x7f) as u32;
        // Guard against a final byte whose high bits would overflow u32.
        if shift == 28 && payload > 0x0f {
            return Err(Error::corruption("varint32: overflow"));
        }
        result |= payload << shift;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    if src.len() < MAX_VARINT32_LEN {
        Err(Error::eof("varint32"))
    } else {
        Err(Error::corruption("varint32: unterminated"))
    }
}

/// Decodes a `u64` varint from the front of `src`.
///
/// Semantics mirror [`decode_varint32`] but for 64-bit values (max ten bytes).
pub fn decode_varint64(src: &[u8]) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in src.iter().enumerate().take(MAX_VARINT64_LEN) {
        let payload = (byte & 0x7f) as u64;
        if shift == 63 && payload > 0x01 {
            return Err(Error::corruption("varint64: overflow"));
        }
        result |= payload << shift;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    if src.len() < MAX_VARINT64_LEN {
        Err(Error::eof("varint64"))
    } else {
        Err(Error::corruption("varint64: unterminated"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip64(v: u64) {
        let mut buf = Vec::new();
        put_varint64(&mut buf, v);
        assert_eq!(buf.len(), varint_len(v));
        let (got, n) = decode_varint64(&buf).unwrap();
        assert_eq!(got, v);
        assert_eq!(n, buf.len());
    }

    #[test]
    fn varint64_roundtrips_boundaries() {
        for v in [0u64, 1, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX] {
            roundtrip64(v);
        }
        for shift in 0..64 {
            roundtrip64(1u64 << shift);
        }
    }

    #[test]
    fn varint32_roundtrips() {
        for v in [0u32, 1, 300, 70_000, u32::MAX] {
            let mut buf = Vec::new();
            put_varint32(&mut buf, v);
            let (got, n) = decode_varint32(&buf).unwrap();
            assert_eq!((got, n), (v, buf.len()));
        }
    }

    #[test]
    fn truncated_is_eof() {
        // 0x80 signals "continue", but there is no next byte.
        assert!(matches!(
            decode_varint64(&[0x80]),
            Err(Error::UnexpectedEof { .. })
        ));
        assert!(matches!(
            decode_varint32(&[0xff, 0xff]),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn overflow_is_rejected() {
        // Eleven continuation bytes cannot be a valid u64 varint.
        let bad = [0xff; 11];
        assert!(decode_varint64(&bad).is_err());
        let bad32 = [0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert!(decode_varint32(&bad32).is_err());
    }

    #[test]
    fn length_prefixed_layout() {
        let mut buf = Vec::new();
        put_length_prefixed(&mut buf, b"hello");
        let (len, n) = decode_varint64(&buf).unwrap();
        assert_eq!(len, 5);
        assert_eq!(&buf[n..], b"hello");
    }
}
