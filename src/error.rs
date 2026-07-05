//! Structured error and result types for the whole engine.
//!
//! Cairn never signals failure by panicking on data it read from disk or from a
//! caller. Every fallible operation returns [`Result`], and every distinct
//! failure mode is a variant of [`Error`]. The variants are grouped so callers
//! can react programmatically:
//!
//! * [`Error::is_corruption`] — the bytes on disk are malformed or hostile. The
//!   database should be treated as damaged.
//! * [`Error::is_not_found`] — a key or file that was expected is absent. Often
//!   an ordinary control-flow signal, not a real error.
//! * [`Error::is_io`] — the underlying filesystem failed.
//!
//! Corruption errors carry a human-readable *context* string describing where
//! the problem was detected (e.g. `"table footer: bad magic"`). Keep those
//! strings specific; they are the first thing a operator sees when a file goes
//! bad.

use std::fmt;
use std::io;

/// The result type used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Every way an operation in Cairn can fail.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error from the underlying filesystem.
    Io(io::Error),

    /// On-disk data is malformed, truncated, or otherwise inconsistent. The
    /// string names the structure and the specific check that failed.
    Corruption(String),

    /// A stored checksum did not match the recomputed one.
    ChecksumMismatch {
        /// Structure whose checksum failed (e.g. `"data block"`).
        context: &'static str,
        /// The checksum recorded on disk.
        expected: u32,
        /// The checksum computed from the bytes that were read.
        actual: u32,
    },

    /// The input ended before a structure that was being parsed was complete.
    UnexpectedEof {
        /// What was being parsed when the buffer ran out.
        context: &'static str,
    },

    /// A length, count, or offset read from untrusted input exceeded a
    /// configured or structural bound. Rejected instead of trusted.
    LimitExceeded {
        /// What limit was hit (e.g. `"value length"`).
        what: &'static str,
        /// The offending value.
        value: u64,
        /// The maximum that would have been accepted.
        limit: u64,
    },

    /// A caller passed an argument that cannot be honored (empty key, key too
    /// large, etc.). Distinct from corruption: the *caller* erred, not the disk.
    InvalidArgument(String),

    /// A requested key was not present in the database.
    NotFound,

    /// A required file (manifest, table, WAL) was missing from the database
    /// directory.
    MissingFile(String),

    /// The on-disk format version is newer than this build understands, or a
    /// feature byte is unrecognized.
    Unsupported(String),

    /// The database is already locked by another handle/process.
    AlreadyLocked(String),

    /// An operation was attempted on a database that has been closed or poisoned
    /// by an earlier fatal error.
    Closed,
}

impl Error {
    /// Builds a [`Error::Corruption`] from anything string-like.
    pub fn corruption(context: impl Into<String>) -> Error {
        Error::Corruption(context.into())
    }

    /// Builds an [`Error::InvalidArgument`].
    pub fn invalid(msg: impl Into<String>) -> Error {
        Error::InvalidArgument(msg.into())
    }

    /// Builds an [`Error::Unsupported`].
    pub fn unsupported(msg: impl Into<String>) -> Error {
        Error::Unsupported(msg.into())
    }

    /// Convenience constructor for [`Error::LimitExceeded`].
    pub fn limit(what: &'static str, value: u64, limit: u64) -> Error {
        Error::LimitExceeded { what, value, limit }
    }

    /// Convenience constructor for [`Error::UnexpectedEof`].
    pub fn eof(context: &'static str) -> Error {
        Error::UnexpectedEof { context }
    }

    /// True for any error indicating the on-disk data is damaged or hostile.
    ///
    /// Groups [`Error::Corruption`], [`Error::ChecksumMismatch`],
    /// [`Error::UnexpectedEof`], and [`Error::LimitExceeded`] — all of which
    /// mean "these bytes cannot be trusted".
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            Error::Corruption(_)
                | Error::ChecksumMismatch { .. }
                | Error::UnexpectedEof { .. }
                | Error::LimitExceeded { .. }
        )
    }

    /// True if this is a "not found" signal ([`Error::NotFound`] or a missing
    /// file). Callers often treat this as normal control flow.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound | Error::MissingFile(_))
    }

    /// True if this wraps an underlying [`io::Error`].
    pub fn is_io(&self) -> bool {
        matches!(self, Error::Io(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Corruption(ctx) => write!(f, "corruption: {ctx}"),
            Error::ChecksumMismatch {
                context,
                expected,
                actual,
            } => write!(
                f,
                "checksum mismatch in {context}: expected {expected:#010x}, computed {actual:#010x}"
            ),
            Error::UnexpectedEof { context } => {
                write!(f, "unexpected end of input while reading {context}")
            }
            Error::LimitExceeded { what, value, limit } => {
                write!(f, "{what} {value} exceeds limit {limit}")
            }
            Error::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            Error::NotFound => write!(f, "key not found"),
            Error::MissingFile(path) => write!(f, "missing file: {path}"),
            Error::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Error::AlreadyLocked(path) => write!(f, "database already locked: {path}"),
            Error::Closed => write!(f, "database is closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        // An `UnexpectedEof` from std I/O usually means a truncated file, which
        // is a corruption signal rather than a transient I/O fault. Preserve
        // that distinction where std hands it to us.
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::UnexpectedEof {
                context: "file (truncated)",
            }
        } else {
            Error::Io(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_consistent() {
        assert!(Error::corruption("x").is_corruption());
        assert!(Error::eof("hdr").is_corruption());
        assert!(Error::limit("len", 9, 4).is_corruption());
        assert!(Error::ChecksumMismatch {
            context: "blk",
            expected: 1,
            actual: 2
        }
        .is_corruption());

        assert!(Error::NotFound.is_not_found());
        assert!(Error::MissingFile("m".into()).is_not_found());
        assert!(!Error::NotFound.is_corruption());

        let io = Error::from(io::Error::other("boom"));
        assert!(io.is_io());
    }

    #[test]
    fn truncation_maps_to_corruption() {
        let e = Error::from(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        assert!(e.is_corruption());
    }

    #[test]
    fn display_is_nonempty() {
        for e in [
            Error::corruption("ctx"),
            Error::NotFound,
            Error::Closed,
            Error::invalid("bad"),
            Error::limit("n", 1, 0),
        ] {
            assert!(!e.to_string().is_empty());
        }
    }
}
