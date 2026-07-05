//! The persistence layer: files, on-disk block framing, the WAL, and the naming
//! convention for the files that make up a database directory.
//!
//! Everything above this module deals in bytes and handles; everything here
//! deals in files and offsets. A database directory contains:
//!
//! ```text
//!   CURRENT              one line naming the live MANIFEST file
//!   MANIFEST-NNNNNNNNNN  the log of version edits (see crate::manifest)
//!   NNNNNNNNNN.wal       write-ahead logs (see crate::recovery)
//!   NNNNNNNNNN.sst       sorted string tables (see crate::table)
//!   LOCK                 an advisory lock guarding single-process access
//! ```
//!
//! File numbers are a single monotonically increasing counter shared across all
//! file kinds, so no two files ever collide.

pub mod block;
pub mod file;
pub mod wal;

pub use block::{frame_block, read_block, BLOCK_TRAILER_SIZE};
pub use file::{read_file, RandomAccessFile, WritableFile};
pub use wal::{LogReader, LogWriter};

/// Name of the file that records which manifest is current.
pub const CURRENT_FILE: &str = "CURRENT";
/// Name of the advisory lock file.
pub const LOCK_FILE: &str = "LOCK";

/// Filename for the SSTable with the given file number.
pub fn table_file_name(number: u64) -> String {
    format!("{number:010}.sst")
}

/// Filename for the write-ahead log with the given file number.
pub fn wal_file_name(number: u64) -> String {
    format!("{number:010}.wal")
}

/// Filename for the manifest with the given file number.
pub fn manifest_file_name(number: u64) -> String {
    format!("MANIFEST-{number:010}")
}

/// A classification of a file found in a database directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A sorted string table (`.sst`).
    Table,
    /// A write-ahead log (`.wal`).
    Wal,
    /// A manifest (`MANIFEST-*`).
    Manifest,
    /// The `CURRENT` pointer file.
    Current,
    /// The `LOCK` file.
    Lock,
}

/// Parses a directory entry name into its kind and file number.
///
/// Returns `None` for names that do not match any known pattern. `CURRENT` and
/// `LOCK` have no number and report `0`.
pub fn parse_file_name(name: &str) -> Option<(FileKind, u64)> {
    if name == CURRENT_FILE {
        return Some((FileKind::Current, 0));
    }
    if name == LOCK_FILE {
        return Some((FileKind::Lock, 0));
    }
    if let Some(rest) = name.strip_prefix("MANIFEST-") {
        return rest.parse::<u64>().ok().map(|n| (FileKind::Manifest, n));
    }
    if let Some(stem) = name.strip_suffix(".sst") {
        return stem.parse::<u64>().ok().map(|n| (FileKind::Table, n));
    }
    if let Some(stem) = name.strip_suffix(".wal") {
        return stem.parse::<u64>().ok().map(|n| (FileKind::Wal, n));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_zero_padded_and_parse_back() {
        assert_eq!(table_file_name(42), "0000000042.sst");
        assert_eq!(wal_file_name(7), "0000000007.wal");
        assert_eq!(manifest_file_name(3), "MANIFEST-0000000003");

        assert_eq!(
            parse_file_name("0000000042.sst"),
            Some((FileKind::Table, 42))
        );
        assert_eq!(parse_file_name("0000000007.wal"), Some((FileKind::Wal, 7)));
        assert_eq!(
            parse_file_name("MANIFEST-0000000003"),
            Some((FileKind::Manifest, 3))
        );
        assert_eq!(parse_file_name("CURRENT"), Some((FileKind::Current, 0)));
        assert_eq!(parse_file_name("LOCK"), Some((FileKind::Lock, 0)));
    }

    #[test]
    fn unknown_names_are_ignored() {
        assert_eq!(parse_file_name("README.md"), None);
        assert_eq!(parse_file_name("garbage.sst.tmp"), None);
        assert_eq!(parse_file_name("MANIFEST-notanumber"), None);
    }
}
