//! Engine configuration.
//!
//! [`Config`] collects every knob that controls a database's behavior and
//! resource use. It is a plain value type with chainable setters:
//!
//! ```
//! use cairn::{Config, CompressionKind};
//!
//! let cfg = Config::default()
//!     .write_buffer_size(8 * 1024 * 1024)
//!     .compression(CompressionKind::Lz)
//!     .bloom_bits_per_key(12)
//!     .sync_writes(true);
//! assert!(cfg.validate().is_ok());
//! ```
//!
//! Several fields double as *safety limits* applied while reading untrusted
//! files (`max_key_size`, `max_value_size`, `max_block_size`). They bound how
//! much the reader will trust a length field before rejecting the file.

use crate::error::{Error, Result};

/// The block compression algorithm applied to on-disk data blocks.
///
/// Mirrors the codecs in [`crate::compression`]; the discriminant is written
/// into each block trailer so a reader knows how to decode it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CompressionKind {
    /// Store blocks uncompressed.
    None = 0,
    /// Run-length encoding — cheap, good for sparse or repetitive data.
    Rle = 1,
    /// A bounded-window LZ77-style compressor — general purpose.
    Lz = 2,
}

impl CompressionKind {
    /// The on-disk byte for this algorithm.
    pub fn as_byte(self) -> u8 {
        self as u8
    }

    /// Parses a compression byte read from a block trailer.
    pub fn from_byte(b: u8) -> Result<CompressionKind> {
        match b {
            0 => Ok(CompressionKind::None),
            1 => Ok(CompressionKind::Rle),
            2 => Ok(CompressionKind::Lz),
            other => Err(Error::unsupported(format!("compression id {other}"))),
        }
    }
}

/// All tunable parameters for a database instance.
#[derive(Debug, Clone)]
pub struct Config {
    /// Create the database directory if it does not exist. Default `true`.
    pub create_if_missing: bool,
    /// Fail to open if the database already exists. Default `false`.
    pub error_if_exists: bool,
    /// Verify block checksums on every read. Default `true`. Disabling trades
    /// safety for speed and is not recommended for untrusted data.
    pub paranoid_checks: bool,
    /// fsync the WAL after every write batch. Default `false` (durability on a
    /// per-batch basis is opt-in; the WAL is always flushed to the OS).
    pub sync_writes: bool,

    /// Bytes of mutations buffered in a memtable before it is flushed to a
    /// level-0 table. Larger buffers mean fewer, bigger tables. Default 4 MiB.
    pub write_buffer_size: usize,
    /// Target uncompressed size of a data block. Default 4 KiB.
    pub block_size: usize,
    /// Number of keys between restart points in a block (prefix-compression
    /// reset interval). Default 16.
    pub block_restart_interval: usize,
    /// Compression applied to data blocks. Default [`CompressionKind::Lz`].
    pub compression: CompressionKind,

    /// Bits of bloom filter per key. `0` disables bloom filters. Default 10
    /// (~1% false-positive rate).
    pub bloom_bits_per_key: usize,
    /// Capacity of the shared block cache, in bytes. `0` disables it.
    /// Default 8 MiB.
    pub block_cache_capacity: usize,

    /// Number of level-0 tables that triggers a compaction. Default 4.
    pub level0_compaction_trigger: usize,
    /// Maximum number of LSM levels. Default 7.
    pub max_levels: usize,
    /// Byte size of the first size-bounded level; each deeper level is this
    /// times [`Config::level_size_multiplier`] larger. Default 10 MiB.
    pub base_level_size: u64,
    /// Growth factor between adjacent levels. Default 10.
    pub level_size_multiplier: u64,

    /// Largest key accepted from a caller or trusted from a file. Default 64 KiB.
    pub max_key_size: usize,
    /// Largest value accepted from a caller or trusted from a file.
    /// Default 64 MiB.
    pub max_value_size: usize,
    /// Largest single block the reader will decode from a table. Guards against
    /// a hostile table claiming an enormous block. Default 64 MiB.
    pub max_block_size: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            create_if_missing: true,
            error_if_exists: false,
            paranoid_checks: true,
            sync_writes: false,

            write_buffer_size: 4 * 1024 * 1024,
            block_size: 4 * 1024,
            block_restart_interval: 16,
            compression: CompressionKind::Lz,

            bloom_bits_per_key: 10,
            block_cache_capacity: 8 * 1024 * 1024,

            level0_compaction_trigger: 4,
            max_levels: 7,
            base_level_size: 10 * 1024 * 1024,
            level_size_multiplier: 10,

            max_key_size: 64 * 1024,
            max_value_size: 64 * 1024 * 1024,
            max_block_size: 64 * 1024 * 1024,
        }
    }
}

impl Config {
    /// Returns the default configuration. Equivalent to [`Config::default`].
    pub fn new() -> Config {
        Config::default()
    }

    /// Validates that the configuration is internally consistent.
    ///
    /// Called automatically by [`crate::Cairn::open`]; exposed so callers can
    /// check a configuration before use.
    pub fn validate(&self) -> Result<()> {
        if self.write_buffer_size < 4 * 1024 {
            return Err(Error::invalid("write_buffer_size must be at least 4 KiB"));
        }
        if self.block_size < 64 {
            return Err(Error::invalid("block_size must be at least 64 bytes"));
        }
        if self.block_restart_interval == 0 {
            return Err(Error::invalid("block_restart_interval must be non-zero"));
        }
        if !(1..=16).contains(&self.max_levels) {
            return Err(Error::invalid("max_levels must be in 1..=16"));
        }
        if self.level_size_multiplier < 2 {
            return Err(Error::invalid("level_size_multiplier must be >= 2"));
        }
        if self.level0_compaction_trigger == 0 {
            return Err(Error::invalid("level0_compaction_trigger must be non-zero"));
        }
        if self.max_key_size == 0 {
            return Err(Error::invalid("max_key_size must be non-zero"));
        }
        if self.max_block_size < self.block_size {
            return Err(Error::invalid("max_block_size must be >= block_size"));
        }
        Ok(())
    }

    /// The target byte size for level `level` (`level >= 1`).
    ///
    /// Level 0 is bounded by file *count*, not size, so this is only meaningful
    /// for deeper levels. Saturating arithmetic keeps a huge multiplier from
    /// overflowing.
    pub fn level_max_bytes(&self, level: usize) -> u64 {
        if level <= 1 {
            return self.base_level_size;
        }
        let mut bytes = self.base_level_size;
        for _ in 1..level {
            bytes = bytes.saturating_mul(self.level_size_multiplier);
        }
        bytes
    }

    // ---- chainable setters -------------------------------------------------

    /// Sets [`Config::create_if_missing`].
    pub fn create_if_missing(mut self, yes: bool) -> Config {
        self.create_if_missing = yes;
        self
    }

    /// Sets [`Config::error_if_exists`].
    pub fn error_if_exists(mut self, yes: bool) -> Config {
        self.error_if_exists = yes;
        self
    }

    /// Sets [`Config::paranoid_checks`].
    pub fn paranoid_checks(mut self, yes: bool) -> Config {
        self.paranoid_checks = yes;
        self
    }

    /// Sets [`Config::sync_writes`].
    pub fn sync_writes(mut self, yes: bool) -> Config {
        self.sync_writes = yes;
        self
    }

    /// Sets [`Config::write_buffer_size`].
    pub fn write_buffer_size(mut self, bytes: usize) -> Config {
        self.write_buffer_size = bytes;
        self
    }

    /// Sets [`Config::block_size`].
    pub fn block_size(mut self, bytes: usize) -> Config {
        self.block_size = bytes;
        self
    }

    /// Sets [`Config::compression`].
    pub fn compression(mut self, kind: CompressionKind) -> Config {
        self.compression = kind;
        self
    }

    /// Sets [`Config::bloom_bits_per_key`].
    pub fn bloom_bits_per_key(mut self, bits: usize) -> Config {
        self.bloom_bits_per_key = bits;
        self
    }

    /// Sets [`Config::block_cache_capacity`].
    pub fn block_cache_capacity(mut self, bytes: usize) -> Config {
        self.block_cache_capacity = bytes;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn builder_chains() {
        let c = Config::new()
            .write_buffer_size(1 << 20)
            .compression(CompressionKind::None)
            .bloom_bits_per_key(0)
            .sync_writes(true);
        assert_eq!(c.write_buffer_size, 1 << 20);
        assert_eq!(c.compression, CompressionKind::None);
        assert_eq!(c.bloom_bits_per_key, 0);
        assert!(c.sync_writes);
    }

    #[test]
    fn validation_catches_bad_values() {
        assert!(Config::default().block_size(0).validate().is_err());
        assert!(Config::default().write_buffer_size(1).validate().is_err());
        assert!(Config {
            max_levels: 0,
            ..Config::default()
        }
        .validate()
        .is_err());
        assert!(Config {
            max_levels: 99,
            ..Config::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn level_sizes_grow() {
        let c = Config::default();
        assert_eq!(c.level_max_bytes(1), c.base_level_size);
        assert!(c.level_max_bytes(3) > c.level_max_bytes(2));
    }

    #[test]
    fn compression_byte_roundtrip() {
        for k in [
            CompressionKind::None,
            CompressionKind::Rle,
            CompressionKind::Lz,
        ] {
            assert_eq!(CompressionKind::from_byte(k.as_byte()).unwrap(), k);
        }
        assert!(CompressionKind::from_byte(9).is_err());
    }
}
