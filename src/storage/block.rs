//! On-disk block framing: compression tag plus checksum trailer.
//!
//! Every block Cairn writes — data blocks, index blocks, the filter block — is
//! laid out identically:
//!
//! ```text
//!   [ payload bytes ][ compression: 1 byte ][ masked CRC-32: 4 bytes ]
//!    \___ handle.size ___/\_________ BLOCK_TRAILER_SIZE = 5 _________/
//! ```
//!
//! The CRC covers the payload *and* the compression byte, so tampering with
//! either is caught. A [`BlockHandle`] addresses only the payload; readers add
//! the trailer length themselves. [`read_block`] is the single function that
//! turns a handle into decoded bytes, and it is the choke point where checksum
//! verification, compression-id validation, and the decompression size cap all
//! apply.

use crate::checksum::{crc32, mask, unmask, Crc32};
use crate::compression::decompress;
use crate::config::CompressionKind;
use crate::error::{Error, Result};
use crate::index::BlockHandle;
use crate::storage::file::RandomAccessFile;
use crate::util::coding::get_fixed32;

/// Size of the trailer appended after every block payload.
pub const BLOCK_TRAILER_SIZE: usize = 5;

/// Frames `payload` for storage: appends the compression byte and checksum.
///
/// Returns the full on-disk bytes (payload + trailer). The caller records a
/// [`BlockHandle`] whose `size` is `payload.len()` — the trailer is implied.
pub fn frame_block(payload: &[u8], kind: CompressionKind) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + BLOCK_TRAILER_SIZE);
    out.extend_from_slice(payload);
    out.push(kind.as_byte());

    // CRC over payload + compression byte, then masked for safe adjacency.
    let mut hasher = Crc32::new();
    hasher.update(&out);
    let crc = mask(hasher.finalize());
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Reads and decodes the block addressed by `handle` from `file`.
///
/// * `verify_checksum` — recompute and compare the CRC (recommended for
///   untrusted data).
/// * `max_output` — the largest decompressed size that will be produced;
///   protects against a hostile block claiming an enormous expansion.
///
/// Returns the decoded (decompressed) block bytes.
pub fn read_block(
    file: &RandomAccessFile,
    handle: BlockHandle,
    verify_checksum: bool,
    max_output: usize,
) -> Result<Vec<u8>> {
    let payload_len = usize::try_from(handle.size)
        .map_err(|_| Error::corruption("block size overflows usize"))?;
    let total = payload_len
        .checked_add(BLOCK_TRAILER_SIZE)
        .ok_or_else(|| Error::corruption("block length + trailer overflows"))?;

    let raw = file.read_at(handle.offset, total)?;
    // `raw` is exactly `total` bytes; split payload | type | crc.
    let payload = &raw[..payload_len];
    let type_byte = raw[payload_len];
    let stored_crc = unmask(get_fixed32(&raw[payload_len + 1..])?);

    if verify_checksum {
        // The stored CRC covers payload + type byte, i.e. raw[..payload_len + 1].
        let computed = crc32(&raw[..payload_len + 1]);
        if computed != stored_crc {
            return Err(Error::ChecksumMismatch {
                context: "data block",
                expected: stored_crc,
                actual: computed,
            });
        }
    }

    let kind = CompressionKind::from_byte(type_byte)?;
    decompress(kind, payload, max_output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::file::WritableFile;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-block-test-{}-{}", std::process::id(), name));
        p
    }

    fn write_and_read(payload: &[u8], kind: CompressionKind, name: &str) -> Vec<u8> {
        let path = temp_path(name);
        let framed = frame_block(&crate::compression::compress(kind, payload), kind);
        let handle = BlockHandle::new(0, (framed.len() - BLOCK_TRAILER_SIZE) as u64);
        {
            let mut w = WritableFile::create(&path).unwrap();
            w.append(&framed).unwrap();
            w.sync().unwrap();
        }
        let f = RandomAccessFile::open(&path).unwrap();
        let out = read_block(&f, handle, true, 1 << 20).unwrap();
        std::fs::remove_file(&path).ok();
        out
    }

    #[test]
    fn frames_and_reads_back() {
        let payload = b"the quick brown fox".repeat(10);
        for kind in [
            CompressionKind::None,
            CompressionKind::Rle,
            CompressionKind::Lz,
        ] {
            assert_eq!(write_and_read(&payload, kind, "rt"), payload);
        }
    }

    #[test]
    fn detects_corrupt_block() {
        let path = temp_path("corrupt");
        let payload = b"important data";
        let mut framed = frame_block(payload, CompressionKind::None);
        // Flip a payload byte after framing, leaving the stored CRC stale.
        framed[0] ^= 0xff;
        let handle = BlockHandle::new(0, payload.len() as u64);
        {
            let mut w = WritableFile::create(&path).unwrap();
            w.append(&framed).unwrap();
            w.sync().unwrap();
        }
        let f = RandomAccessFile::open(&path).unwrap();
        assert!(matches!(
            read_block(&f, handle, true, 1 << 20),
            Err(Error::ChecksumMismatch { .. })
        ));
        std::fs::remove_file(&path).ok();
    }
}
