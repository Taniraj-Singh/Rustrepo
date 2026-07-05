//! Thin, safe wrappers over the two file access patterns the engine needs:
//! sequential append ([`WritableFile`]) and random reads ([`RandomAccessFile`]).
//!
//! [`RandomAccessFile`] validates every offset and length against the file's
//! known size before touching the disk, so a bad block handle read from a
//! corrupt table becomes an [`Error`], not an operating-system error or a hang.
//! Random reads take `&self` (guarded by an internal mutex), so a single opened
//! table can be read concurrently.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};

/// An append-only file with buffered writes and explicit durability control.
#[derive(Debug)]
pub struct WritableFile {
    inner: File,
    buffer: Vec<u8>,
    bytes_written: u64,
    path: PathBuf,
}

impl WritableFile {
    /// Creates (or truncates) `path` for writing.
    pub fn create(path: impl AsRef<Path>) -> Result<WritableFile> {
        let path = path.as_ref().to_path_buf();
        let inner = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(WritableFile {
            inner,
            buffer: Vec::with_capacity(64 * 1024),
            bytes_written: 0,
            path,
        })
    }

    /// Appends `data`, buffering it in memory until [`WritableFile::flush`].
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(data);
        self.bytes_written += data.len() as u64;
        // Bound the in-memory buffer so a long run of small appends does not
        // grow it without limit.
        if self.buffer.len() >= 64 * 1024 {
            self.flush()?;
        }
        Ok(())
    }

    /// Flushes buffered bytes to the operating system (not necessarily to disk).
    pub fn flush(&mut self) -> Result<()> {
        if !self.buffer.is_empty() {
            self.inner.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        Ok(())
    }

    /// Flushes and fsyncs, guaranteeing the bytes are durable on disk.
    pub fn sync(&mut self) -> Result<()> {
        self.flush()?;
        self.inner.sync_all()?;
        Ok(())
    }

    /// Total bytes appended over this file's lifetime.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WritableFile {
    fn drop(&mut self) {
        // Best-effort flush of buffered bytes on drop; errors are ignored
        // because a Drop impl cannot return them. Callers that need durability
        // must call `sync` explicitly.
        let _ = self.flush();
    }
}

/// A read-only file supporting bounds-checked positional reads.
#[derive(Debug)]
pub struct RandomAccessFile {
    inner: Mutex<File>,
    len: u64,
    path: PathBuf,
}

impl RandomAccessFile {
    /// Opens `path` for reading, recording its length up front.
    pub fn open(path: impl AsRef<Path>) -> Result<RandomAccessFile> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        Ok(RandomAccessFile {
            inner: Mutex::new(file),
            len,
            path,
        })
    }

    /// The file's length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads exactly `n` bytes starting at `offset`.
    ///
    /// Fails with [`Error::UnexpectedEof`] if the requested range extends past
    /// the end of the file — checked before any I/O, using checked arithmetic so
    /// a malicious `offset + n` cannot wrap.
    pub fn read_at(&self, offset: u64, n: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(n as u64)
            .ok_or_else(|| Error::corruption("read offset + length overflows"))?;
        if end > self.len {
            return Err(Error::eof("read past end of file"));
        }
        let mut buf = vec![0u8; n];
        let mut guard = self
            .inner
            .lock()
            .expect("random-access file mutex poisoned");
        guard.seek(SeekFrom::Start(offset))?;
        guard.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Reads an entire file into memory. Used for small control files (manifests,
/// WAL segments) whose size is bounded by configuration.
pub fn read_file(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let mut file = File::open(path.as_ref())?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-file-test-{}-{}", std::process::id(), name));
        p
    }

    #[test]
    fn write_then_random_read() {
        let path = temp_path("rw");
        {
            let mut w = WritableFile::create(&path).unwrap();
            w.append(b"hello ").unwrap();
            w.append(b"world").unwrap();
            w.sync().unwrap();
            assert_eq!(w.bytes_written(), 11);
        }
        let r = RandomAccessFile::open(&path).unwrap();
        assert_eq!(r.len(), 11);
        assert_eq!(r.read_at(0, 5).unwrap(), b"hello");
        assert_eq!(r.read_at(6, 5).unwrap(), b"world");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_past_end_is_rejected() {
        let path = temp_path("eof");
        {
            let mut w = WritableFile::create(&path).unwrap();
            w.append(&[0u8; 10]).unwrap();
            w.sync().unwrap();
        }
        let r = RandomAccessFile::open(&path).unwrap();
        assert!(r.read_at(5, 10).is_err());
        assert!(r.read_at(u64::MAX, 1).is_err());
        assert!(r.read_at(0, 10).is_ok());
        std::fs::remove_file(&path).ok();
    }
}
