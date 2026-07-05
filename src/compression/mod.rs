//! Block compression.
//!
//! Cairn compresses data blocks before writing them to disk. Three codecs are
//! provided, selected per database via [`crate::config::CompressionKind`]:
//!
//! * [`CompressionKind::None`] — store verbatim.
//! * [`CompressionKind::Rle`] — run-length encoding; cheap, ideal for sparse or
//!   repetitive data.
//! * [`CompressionKind::Lz`] — a bounded-window LZSS compressor; general
//!   purpose.
//!
//! ## Decoding is the trust boundary
//!
//! Compressed bytes come off disk, so every decoder is written to be safe on
//! arbitrary input. Each takes a `max_output` cap and refuses to produce more
//! than that many bytes, so a hostile block can never trigger an unbounded
//! allocation (a "decompression bomb"). Every offset, length, and buffer read
//! is bounds-checked; malformed input yields an [`Error`], never a panic.

pub mod lz;
pub mod rle;

use crate::config::CompressionKind;
use crate::error::{Error, Result};

/// A block compression algorithm.
///
/// Implemented by the three zero-sized codec markers ([`NoCodec`], [`RleCodec`],
/// [`LzCodec`]); most callers use the free [`compress`]/[`decompress`] functions
/// which dispatch on a [`CompressionKind`].
pub trait Codec {
    /// The kind byte recorded on disk for blocks this codec produces.
    fn kind(&self) -> CompressionKind;

    /// Compresses `input`. Never fails: a codec that cannot shrink its input
    /// may return a copy (callers compare sizes and choose).
    fn encode(&self, input: &[u8]) -> Vec<u8>;

    /// Decompresses `input`, producing at most `max_output` bytes.
    fn decode(&self, input: &[u8], max_output: usize) -> Result<Vec<u8>>;
}

/// The identity codec — stores bytes verbatim.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCodec;

impl Codec for NoCodec {
    fn kind(&self) -> CompressionKind {
        CompressionKind::None
    }

    fn encode(&self, input: &[u8]) -> Vec<u8> {
        input.to_vec()
    }

    fn decode(&self, input: &[u8], max_output: usize) -> Result<Vec<u8>> {
        if input.len() > max_output {
            return Err(Error::limit(
                "stored block length",
                input.len() as u64,
                max_output as u64,
            ));
        }
        Ok(input.to_vec())
    }
}

/// Run-length codec (see [`rle`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct RleCodec;

impl Codec for RleCodec {
    fn kind(&self) -> CompressionKind {
        CompressionKind::Rle
    }

    fn encode(&self, input: &[u8]) -> Vec<u8> {
        rle::encode(input)
    }

    fn decode(&self, input: &[u8], max_output: usize) -> Result<Vec<u8>> {
        rle::decode(input, max_output)
    }
}

/// LZSS codec (see [`lz`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct LzCodec;

impl Codec for LzCodec {
    fn kind(&self) -> CompressionKind {
        CompressionKind::Lz
    }

    fn encode(&self, input: &[u8]) -> Vec<u8> {
        lz::encode(input)
    }

    fn decode(&self, input: &[u8], max_output: usize) -> Result<Vec<u8>> {
        lz::decode(input, max_output)
    }
}

/// Compresses `input` with the given algorithm.
pub fn compress(kind: CompressionKind, input: &[u8]) -> Vec<u8> {
    match kind {
        CompressionKind::None => NoCodec.encode(input),
        CompressionKind::Rle => RleCodec.encode(input),
        CompressionKind::Lz => LzCodec.encode(input),
    }
}

/// Decompresses `input`, producing at most `max_output` bytes.
///
/// This is the function the table reader calls on every block. The `max_output`
/// cap must be the uncompressed length the block header claims *and* must itself
/// have been validated against a configured limit before this call.
pub fn decompress(kind: CompressionKind, input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    match kind {
        CompressionKind::None => NoCodec.decode(input, max_output),
        CompressionKind::Rle => RleCodec.decode(input, max_output),
        CompressionKind::Lz => LzCodec.decode(input, max_output),
    }
}

/// Compresses `input` with `preferred`, falling back to no compression if the
/// result is not actually smaller.
///
/// Returns the kind that was ultimately used and the bytes. Storing an
/// "compressed" block that is larger than the original wastes both space and
/// decode time, so the builder always takes the smaller of the two.
pub fn compress_or_store(preferred: CompressionKind, input: &[u8]) -> (CompressionKind, Vec<u8>) {
    if preferred == CompressionKind::None {
        return (CompressionKind::None, input.to_vec());
    }
    let compressed = compress(preferred, input);
    if compressed.len() < input.len() {
        (preferred, compressed)
    } else {
        (CompressionKind::None, input.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(kind: CompressionKind, data: &[u8]) {
        let enc = compress(kind, data);
        let dec = decompress(kind, &enc, data.len()).unwrap();
        assert_eq!(dec, data, "roundtrip failed for {kind:?}");
    }

    #[test]
    fn all_codecs_roundtrip_varied_data() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            b"a".to_vec(),
            vec![0u8; 1000],
            b"the quick brown fox the quick brown fox the quick brown fox".to_vec(),
            (0..=255u8).cycle().take(1000).collect(),
        ];
        for kind in [
            CompressionKind::None,
            CompressionKind::Rle,
            CompressionKind::Lz,
        ] {
            for data in &cases {
                roundtrip(kind, data);
            }
        }
    }

    #[test]
    fn decode_respects_output_cap() {
        let data = vec![7u8; 10_000];
        let enc = compress(CompressionKind::Rle, &data);
        // A 1 KiB cap must reject a block that expands to 10 KiB.
        assert!(matches!(
            decompress(CompressionKind::Rle, &enc, 1024),
            Err(Error::LimitExceeded { .. })
        ));
    }

    #[test]
    fn compress_or_store_never_expands() {
        let incompressible: Vec<u8> = (0..64u8).collect();
        let (kind, bytes) = compress_or_store(CompressionKind::Lz, &incompressible);
        assert!(bytes.len() <= incompressible.len() + 1);
        // Round-trips regardless of which kind was chosen.
        let dec = decompress(kind, &bytes, incompressible.len()).unwrap();
        assert_eq!(dec, incompressible);
    }
}
