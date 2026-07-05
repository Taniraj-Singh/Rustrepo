//! CRC-32 checksums for on-disk integrity.
//!
//! Every block, WAL record, and table footer carries a CRC-32 (IEEE 802.3
//! polynomial, the same one used by zlib and gzip) so that silent corruption —
//! a flipped bit on disk, a truncated write — is caught on read rather than
//! surfaced as a wrong answer.
//!
//! The table is generated at compile time, so there is no start-up cost and no
//! `unsafe`/`static mut`. A streaming [`Crc32`] hasher is provided for data that
//! arrives in pieces, plus one-shot [`crc32`] for contiguous buffers.
//!
//! ## Masking
//!
//! When a CRC is stored immediately next to the very bytes it protects, a naive
//! layout can make certain corruptions cancel out. Following LevelDB, WAL
//! records store a *masked* CRC ([`mask`]) that rotates and offsets the value;
//! readers [`unmask`] before comparing.

/// The reflected IEEE polynomial used by CRC-32 (zlib/gzip compatible).
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// Precomputed lookup table, one entry per byte value, built at compile time.
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ POLYNOMIAL;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// A streaming CRC-32 hasher.
#[derive(Debug, Clone)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Crc32 {
        Crc32::new()
    }
}

impl Crc32 {
    /// Creates a fresh hasher.
    pub fn new() -> Crc32 {
        // Standard CRC-32 pre-conditioning: start at all-ones.
        Crc32 { state: 0xFFFF_FFFF }
    }

    /// Feeds more bytes into the running checksum.
    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.state;
        for &byte in data {
            let index = ((crc ^ byte as u32) & 0xff) as usize;
            crc = (crc >> 8) ^ TABLE[index];
        }
        self.state = crc;
    }

    /// Finalizes and returns the checksum. The hasher is left unchanged, so it
    /// may continue to be updated if desired.
    pub fn finalize(&self) -> u32 {
        // Standard CRC-32 post-conditioning: invert all bits.
        self.state ^ 0xFFFF_FFFF
    }
}

/// Computes the CRC-32 of a single contiguous buffer.
pub fn crc32(data: &[u8]) -> u32 {
    let mut h = Crc32::new();
    h.update(data);
    h.finalize()
}

// LevelDB-style masking constants.
const MASK_DELTA: u32 = 0xa282_ead8;

/// Masks a CRC so it can be safely stored next to the data it protects.
pub fn mask(crc: u32) -> u32 {
    // Rotate left by 17 bits, then add a constant.
    crc.rotate_left(17).wrapping_add(MASK_DELTA)
}

/// Reverses [`mask`].
pub fn unmask(masked: u32) -> u32 {
    masked.wrapping_sub(MASK_DELTA).rotate_right(17)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // CRC-32/ISO-HDLC reference values.
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn streaming_matches_one_shot() {
        let data = b"the quick brown fox";
        let mut h = Crc32::new();
        h.update(&data[..4]);
        h.update(&data[4..]);
        assert_eq!(h.finalize(), crc32(data));
    }

    #[test]
    fn mask_roundtrips() {
        for c in [0u32, 1, 0xdead_beef, 0xffff_ffff, crc32(b"hello")] {
            assert_eq!(unmask(mask(c)), c);
            // A masked CRC should differ from the raw CRC (avoids the
            // cancellation the mask exists to prevent).
            assert_ne!(mask(c), c);
        }
    }

    #[test]
    fn detects_single_bit_flips() {
        let mut data = b"integrity matters".to_vec();
        let good = crc32(&data);
        for i in 0..data.len() {
            for bit in 0..8 {
                data[i] ^= 1 << bit;
                assert_ne!(crc32(&data), good);
                data[i] ^= 1 << bit; // restore
            }
        }
    }
}
