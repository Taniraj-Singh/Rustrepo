//! The write-ahead log record format.
//!
//! Before a batch of mutations touches the in-memory memtable, it is appended to
//! a WAL so a crash cannot lose an acknowledged write. Each record is framed as:
//!
//! ```text
//!   [ masked CRC-32: 4 ][ length: 4 ][ type: 1 ][ payload: length bytes ]
//! ```
//!
//! The CRC covers the type byte and the payload. Records are self-delimiting, so
//! the reader can walk a log and stop cleanly at a **partial record** — the
//! normal state of the last record after a crash mid-append. That truncated tail
//! is reported as end-of-log (`Ok(None)`), while a record whose CRC fails is
//! reported as [`Error::ChecksumMismatch`] so genuine corruption is not silently
//! swallowed.

use crate::checksum::{mask, Crc32};
use crate::error::{Error, Result};
use crate::parser::ByteReader;
use crate::storage::file::WritableFile;
use crate::util::coding::put_fixed32;

/// Fixed record header size: CRC (4) + length (4) + type (1).
const HEADER_LEN: usize = 9;
/// The only record type currently emitted: a complete, unfragmented record.
const RECORD_FULL: u8 = 1;

/// Appends length-prefixed, checksummed records to a WAL file.
#[derive(Debug)]
pub struct LogWriter {
    file: WritableFile,
}

impl LogWriter {
    /// Creates a new log at `path`.
    pub fn create(path: impl AsRef<std::path::Path>) -> Result<LogWriter> {
        Ok(LogWriter {
            file: WritableFile::create(path)?,
        })
    }

    /// Wraps an already-open writable file as a log.
    pub fn from_file(file: WritableFile) -> LogWriter {
        LogWriter { file }
    }

    /// Appends one record.
    pub fn add_record(&mut self, data: &[u8]) -> Result<()> {
        let len =
            u32::try_from(data.len()).map_err(|_| Error::invalid("WAL record exceeds 4 GiB"))?;

        let mut hasher = Crc32::new();
        hasher.update(&[RECORD_FULL]);
        hasher.update(data);
        let crc = mask(hasher.finalize());

        let mut header = Vec::with_capacity(HEADER_LEN);
        put_fixed32(&mut header, crc);
        put_fixed32(&mut header, len);
        header.push(RECORD_FULL);

        self.file.append(&header)?;
        self.file.append(data)?;
        Ok(())
    }

    /// Flushes buffered records to the OS.
    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()
    }

    /// Flushes and fsyncs the log, making prior records durable.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync()
    }

    /// Total bytes appended.
    pub fn bytes_written(&self) -> u64 {
        self.file.bytes_written()
    }
}

/// Reads records back from an in-memory copy of a WAL file.
///
/// WAL segments are bounded by the write-buffer size, so reading one fully into
/// memory for recovery is cheap and keeps parsing entirely bounds-checked.
#[derive(Debug)]
pub struct LogReader<'a> {
    reader: ByteReader<'a>,
}

impl<'a> LogReader<'a> {
    /// Wraps the raw bytes of a log file.
    pub fn new(bytes: &'a [u8]) -> LogReader<'a> {
        LogReader {
            reader: ByteReader::new(bytes),
        }
    }

    /// Reads the next record.
    ///
    /// Returns `Ok(None)` at a clean end of log *or* at a truncated tail record
    /// (a crash artifact). Returns [`Error::ChecksumMismatch`] if a fully
    /// present record fails its checksum, and [`Error::Corruption`] on an
    /// unknown record type.
    pub fn read_record(&mut self) -> Result<Option<Vec<u8>>> {
        // A header that doesn't fit means we've reached a truncated tail.
        if self.reader.remaining_len() < HEADER_LEN {
            return Ok(None);
        }
        let crc = self.reader.read_u32()?;
        let len = self.reader.read_u32()? as usize;
        let record_type = self.reader.read_u8()?;

        if record_type != RECORD_FULL {
            return Err(Error::corruption("WAL: unknown record type"));
        }
        // Payload truncated by a crash: treat as end of log.
        if self.reader.remaining_len() < len {
            return Ok(None);
        }
        let data = self.reader.read_bytes(len)?;

        let mut hasher = Crc32::new();
        hasher.update(&[record_type]);
        hasher.update(data);
        let computed = mask(hasher.finalize());
        if computed != crc {
            return Err(Error::ChecksumMismatch {
                context: "WAL record",
                expected: crc,
                actual: computed,
            });
        }
        Ok(Some(data.to_vec()))
    }

    /// Reads every clean record, stopping at the first truncated tail.
    ///
    /// A checksum or format error is propagated; the caller (recovery) decides
    /// whether to treat it as fatal or as the end of usable data.
    pub fn read_all(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        while let Some(record) = self.read_record()? {
            out.push(record);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::file::read_file;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-wal-test-{}-{}", std::process::id(), name));
        p
    }

    #[test]
    fn records_roundtrip() {
        let path = temp_path("rt");
        {
            let mut w = LogWriter::create(&path).unwrap();
            w.add_record(b"first").unwrap();
            w.add_record(b"second record").unwrap();
            w.add_record(&[]).unwrap(); // empty record is valid
            w.sync().unwrap();
        }
        let bytes = read_file(&path).unwrap();
        let recs = LogReader::new(&bytes).read_all().unwrap();
        assert_eq!(
            recs,
            vec![b"first".to_vec(), b"second record".to_vec(), vec![]]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn truncated_tail_stops_cleanly() {
        let path = temp_path("trunc");
        {
            let mut w = LogWriter::create(&path).unwrap();
            w.add_record(b"complete").unwrap();
            w.add_record(b"this one gets cut off").unwrap();
            w.sync().unwrap();
        }
        let mut bytes = read_file(&path).unwrap();
        // Chop the final record mid-payload.
        bytes.truncate(bytes.len() - 5);
        let recs = LogReader::new(&bytes).read_all().unwrap();
        assert_eq!(recs, vec![b"complete".to_vec()]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn checksum_mismatch_is_reported() {
        let path = temp_path("crc");
        {
            let mut w = LogWriter::create(&path).unwrap();
            w.add_record(b"payload").unwrap();
            w.sync().unwrap();
        }
        let mut bytes = read_file(&path).unwrap();
        // Corrupt a payload byte (header is the first 9 bytes).
        let n = bytes.len();
        bytes[n - 1] ^= 0xff;
        let mut reader = LogReader::new(&bytes);
        assert!(matches!(
            reader.read_record(),
            Err(Error::ChecksumMismatch { .. })
        ));
        std::fs::remove_file(&path).ok();
    }
}
