//! The manifest: a durable, append-only log of [`VersionEdit`]s, plus the
//! `CURRENT` pointer that names the live manifest.
//!
//! Metadata durability works in two parts:
//!
//! * The **manifest** file (`MANIFEST-NNNNNNNNNN`) is a WAL-framed log of encoded
//!   version edits. Replaying it in order rebuilds the current version.
//! * The **`CURRENT`** file contains a single line naming the live manifest.
//!   Switching manifests is done by writing a new `CURRENT` atomically (write a
//!   temp file, then rename), so a crash leaves either the old or the new
//!   pointer intact — never a torn one.
//!
//! This module owns those two file formats; [`crate::version::VersionSet`] drives
//! them.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::{manifest_file_name, CURRENT_FILE};
use crate::version::VersionEdit;

/// Appends encoded version edits to a manifest log file.
#[derive(Debug)]
pub struct ManifestWriter {
    log: crate::storage::LogWriter,
}

impl ManifestWriter {
    /// Creates a new manifest log at `path`.
    pub fn create(path: impl AsRef<Path>) -> Result<ManifestWriter> {
        Ok(ManifestWriter {
            log: crate::storage::LogWriter::create(path)?,
        })
    }

    /// Appends one edit and fsyncs, making it durable.
    pub fn append(&mut self, edit: &VersionEdit) -> Result<()> {
        self.log.add_record(&edit.encode())?;
        self.log.sync()
    }
}

/// The path of the `CURRENT` file within a database directory.
pub fn current_file_path(dbname: impl AsRef<Path>) -> PathBuf {
    dbname.as_ref().join(CURRENT_FILE)
}

/// Atomically points `CURRENT` at the manifest with the given number.
///
/// Writes a temporary file and renames it over `CURRENT`, so a crash never
/// leaves a half-written pointer.
pub fn set_current_file(dbname: impl AsRef<Path>, manifest_number: u64) -> Result<()> {
    let dbname = dbname.as_ref();
    let contents = format!("{}\n", manifest_file_name(manifest_number));
    let tmp = dbname.join(format!("CURRENT.{manifest_number}.tmp"));
    std::fs::write(&tmp, contents.as_bytes())?;
    std::fs::rename(&tmp, current_file_path(dbname))?;
    Ok(())
}

/// Reads `CURRENT` and returns the number of the live manifest.
pub fn read_current(dbname: impl AsRef<Path>) -> Result<u64> {
    let path = current_file_path(&dbname);
    let bytes = crate::storage::read_file(&path)
        .map_err(|_| Error::MissingFile(path.display().to_string()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::corruption("CURRENT: not utf-8"))?
        .trim();
    let number = text
        .strip_prefix("MANIFEST-")
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| Error::corruption("CURRENT: malformed manifest name"))?;
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-manifest-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn current_file_roundtrips() {
        let dir = temp_dir("current");
        set_current_file(&dir, 42).unwrap();
        assert_eq!(read_current(&dir).unwrap(), 42);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_current_is_missing_file_error() {
        let dir = temp_dir("nocurrent");
        assert!(matches!(read_current(&dir), Err(Error::MissingFile(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manifest_appends_edits() {
        let dir = temp_dir("manifest");
        let path = dir.join(manifest_file_name(1));
        {
            let mut w = ManifestWriter::create(&path).unwrap();
            let mut e = VersionEdit::new();
            e.set_last_sequence(5);
            w.append(&e).unwrap();
        }
        let bytes = crate::storage::read_file(&path).unwrap();
        let recs = crate::storage::LogReader::new(&bytes).read_all().unwrap();
        assert_eq!(recs.len(), 1);
        let decoded = VersionEdit::decode(&recs[0]).unwrap();
        assert_eq!(decoded.last_sequence, Some(5));
        std::fs::remove_dir_all(&dir).ok();
    }
}
