//! Fixed-width little-endian integer coding.
//!
//! Where varints are used for values that are usually small, fixed-width
//! encodings are used for structural fields whose position must be predictable:
//! block trailers, table footers, and WAL record headers. All multi-byte
//! integers in Cairn's on-disk format are little-endian.
//!
//! The `get_*` decoders validate length before reading, so they are safe to
//! call on untrusted slices.

use crate::error::{Error, Result};

/// Appends `v` to `dst` as four little-endian bytes.
pub fn put_fixed32(dst: &mut Vec<u8>, v: u32) {
    dst.extend_from_slice(&v.to_le_bytes());
}

/// Appends `v` to `dst` as eight little-endian bytes.
pub fn put_fixed64(dst: &mut Vec<u8>, v: u64) {
    dst.extend_from_slice(&v.to_le_bytes());
}

/// Reads a little-endian `u32` from the first four bytes of `src`.
pub fn get_fixed32(src: &[u8]) -> Result<u32> {
    let bytes: [u8; 4] = src
        .get(..4)
        .ok_or(Error::eof("fixed32"))?
        .try_into()
        .expect("slice of length 4 converts to [u8; 4]");
    Ok(u32::from_le_bytes(bytes))
}

/// Reads a little-endian `u64` from the first eight bytes of `src`.
pub fn get_fixed64(src: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = src
        .get(..8)
        .ok_or(Error::eof("fixed64"))?
        .try_into()
        .expect("slice of length 8 converts to [u8; 8]");
    Ok(u64::from_le_bytes(bytes))
}

/// Reads a little-endian `u32` at `offset` within `src`.
pub fn get_fixed32_at(src: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).ok_or(Error::eof("fixed32"))?;
    get_fixed32(src.get(offset..end).ok_or(Error::eof("fixed32"))?)
}

/// Reads a little-endian `u64` at `offset` within `src`.
pub fn get_fixed64_at(src: &[u8], offset: usize) -> Result<u64> {
    let end = offset.checked_add(8).ok_or(Error::eof("fixed64"))?;
    get_fixed64(src.get(offset..end).ok_or(Error::eof("fixed64"))?)
}

/// Casts a `u64` read from disk to `usize`, rejecting values that do not fit on
/// the current platform instead of silently truncating.
///
/// This is the single choke point for the 64-bit-length -> in-memory-index
/// conversion that untrusted files force on us. On a 64-bit host it never
/// fails; on a 32-bit host it guards against a file that claims a > 4 GiB
/// structure.
pub fn u64_to_usize(v: u64, what: &'static str) -> Result<usize> {
    usize::try_from(v).map_err(|_| Error::limit(what, v, usize::MAX as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_roundtrips() {
        let mut buf = Vec::new();
        put_fixed32(&mut buf, 0xdead_beef);
        put_fixed64(&mut buf, 0x0123_4567_89ab_cdef);
        assert_eq!(get_fixed32(&buf).unwrap(), 0xdead_beef);
        assert_eq!(get_fixed64_at(&buf, 4).unwrap(), 0x0123_4567_89ab_cdef);
    }

    #[test]
    fn short_input_is_eof() {
        assert!(get_fixed32(&[1, 2, 3]).is_err());
        assert!(get_fixed64(&[0; 7]).is_err());
        assert!(get_fixed32_at(&[0; 4], 2).is_err());
    }

    #[test]
    fn offset_overflow_is_rejected() {
        assert!(get_fixed64_at(&[0; 8], usize::MAX).is_err());
    }

    #[test]
    fn usize_conversion_guards() {
        assert_eq!(u64_to_usize(42, "x").unwrap(), 42);
        // On 64-bit this always succeeds; assert the happy path holds.
        assert!(u64_to_usize(u32::MAX as u64, "x").is_ok());
    }
}
