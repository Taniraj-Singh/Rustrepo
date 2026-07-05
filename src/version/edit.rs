//! Version edits: the atomic unit of change to the set of live tables.
//!
//! The database's durable state — which tables exist, at which levels, plus a
//! few counters — is not rewritten in place. Instead every change is expressed
//! as a [`VersionEdit`] and appended to the manifest log. Replaying the edits in
//! order reconstructs the current [`Version`](crate::version::Version). This is
//! the same design LevelDB uses: cheap, append-only, crash-consistent metadata.
//!
//! An edit is a tagged record; each optional field is present only if its tag
//! appears. Decoding is defensive — an unknown tag or a truncated field is a
//! structured [`Error`], never a panic.

use crate::error::{Error, Result};
use crate::parser::ByteReader;
use crate::serializer::Encoder;

// Field tags. Stable on disk — never renumber.
const TAG_COMPARATOR: u32 = 1;
const TAG_LOG_NUMBER: u32 = 2;
const TAG_NEXT_FILE_NUMBER: u32 = 3;
const TAG_LAST_SEQUENCE: u32 = 4;
const TAG_DELETED_FILE: u32 = 5;
const TAG_NEW_FILE: u32 = 6;

/// Metadata describing one on-disk table file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetaData {
    /// The file's number (its name is `{number}.sst`).
    pub number: u64,
    /// Size of the file in bytes.
    pub file_size: u64,
    /// Smallest internal key in the file.
    pub smallest: Vec<u8>,
    /// Largest internal key in the file.
    pub largest: Vec<u8>,
}

impl FileMetaData {
    /// Creates file metadata.
    pub fn new(number: u64, file_size: u64, smallest: Vec<u8>, largest: Vec<u8>) -> FileMetaData {
        FileMetaData {
            number,
            file_size,
            smallest,
            largest,
        }
    }
}

/// A batch of changes applied atomically to the current version.
#[derive(Debug, Default, Clone)]
pub struct VersionEdit {
    /// The key comparator name, recorded once when the database is created.
    pub comparator_name: Option<String>,
    /// The WAL file number in effect after this edit.
    pub log_number: Option<u64>,
    /// The next unused file number.
    pub next_file_number: Option<u64>,
    /// The highest sequence number assigned so far.
    pub last_sequence: Option<u64>,
    /// Files removed from a level: `(level, file number)`.
    pub deleted_files: Vec<(usize, u64)>,
    /// Files added to a level: `(level, metadata)`.
    pub new_files: Vec<(usize, FileMetaData)>,
}

impl VersionEdit {
    /// Creates an empty edit.
    pub fn new() -> VersionEdit {
        VersionEdit::default()
    }

    /// Records the comparator name.
    pub fn set_comparator_name(&mut self, name: impl Into<String>) {
        self.comparator_name = Some(name.into());
    }

    /// Records the current WAL number.
    pub fn set_log_number(&mut self, n: u64) {
        self.log_number = Some(n);
    }

    /// Records the next file number.
    pub fn set_next_file_number(&mut self, n: u64) {
        self.next_file_number = Some(n);
    }

    /// Records the last sequence number.
    pub fn set_last_sequence(&mut self, n: u64) {
        self.last_sequence = Some(n);
    }

    /// Marks a file for removal from `level`.
    pub fn delete_file(&mut self, level: usize, number: u64) {
        self.deleted_files.push((level, number));
    }

    /// Adds a file to `level`.
    pub fn add_file(&mut self, level: usize, file: FileMetaData) {
        self.new_files.push((level, file));
    }

    /// Encodes the edit to bytes for the manifest log.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        if let Some(name) = &self.comparator_name {
            e.put_varint32(TAG_COMPARATOR)
                .put_length_prefixed(name.as_bytes());
        }
        if let Some(n) = self.log_number {
            e.put_varint32(TAG_LOG_NUMBER).put_varint64(n);
        }
        if let Some(n) = self.next_file_number {
            e.put_varint32(TAG_NEXT_FILE_NUMBER).put_varint64(n);
        }
        if let Some(n) = self.last_sequence {
            e.put_varint32(TAG_LAST_SEQUENCE).put_varint64(n);
        }
        for (level, number) in &self.deleted_files {
            e.put_varint32(TAG_DELETED_FILE)
                .put_varint32(*level as u32)
                .put_varint64(*number);
        }
        for (level, file) in &self.new_files {
            e.put_varint32(TAG_NEW_FILE)
                .put_varint32(*level as u32)
                .put_varint64(file.number)
                .put_varint64(file.file_size)
                .put_length_prefixed(&file.smallest)
                .put_length_prefixed(&file.largest);
        }
        e.into_bytes()
    }

    /// Decodes an edit from a manifest record.
    pub fn decode(bytes: &[u8]) -> Result<VersionEdit> {
        let mut edit = VersionEdit::new();
        let mut r = ByteReader::new(bytes);
        while !r.is_empty() {
            let tag = r.read_varint32()?;
            match tag {
                TAG_COMPARATOR => {
                    let name = r.read_length_prefixed_capped(1024)?;
                    edit.comparator_name = Some(
                        std::str::from_utf8(name)
                            .map_err(|_| Error::corruption("edit: comparator name not utf-8"))?
                            .to_string(),
                    );
                }
                TAG_LOG_NUMBER => edit.log_number = Some(r.read_varint64()?),
                TAG_NEXT_FILE_NUMBER => edit.next_file_number = Some(r.read_varint64()?),
                TAG_LAST_SEQUENCE => edit.last_sequence = Some(r.read_varint64()?),
                TAG_DELETED_FILE => {
                    let level = read_level(&mut r)?;
                    let number = r.read_varint64()?;
                    edit.deleted_files.push((level, number));
                }
                TAG_NEW_FILE => {
                    let level = read_level(&mut r)?;
                    let number = r.read_varint64()?;
                    let file_size = r.read_varint64()?;
                    let smallest = r.read_length_prefixed_capped(1 << 20)?.to_vec();
                    let largest = r.read_length_prefixed_capped(1 << 20)?.to_vec();
                    edit.new_files.push((
                        level,
                        FileMetaData::new(number, file_size, smallest, largest),
                    ));
                }
                other => {
                    return Err(Error::corruption(format!("edit: unknown tag {other}")));
                }
            }
        }
        Ok(edit)
    }
}

/// Reads a level field, rejecting an absurd level up front.
fn read_level(r: &mut ByteReader<'_>) -> Result<usize> {
    let level = r.read_varint32()?;
    if level as usize > 64 {
        return Err(Error::corruption("edit: level out of range"));
    }
    Ok(level as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_roundtrips() {
        let mut e = VersionEdit::new();
        e.set_comparator_name("cairn.InternalKeyComparator");
        e.set_log_number(7);
        e.set_next_file_number(42);
        e.set_last_sequence(1000);
        e.delete_file(0, 5);
        e.delete_file(1, 6);
        e.add_file(
            0,
            FileMetaData::new(10, 2048, b"aaa".to_vec(), b"zzz".to_vec()),
        );
        e.add_file(2, FileMetaData::new(11, 4096, b"m".to_vec(), b"q".to_vec()));

        let bytes = e.encode();
        let decoded = VersionEdit::decode(&bytes).unwrap();

        assert_eq!(
            decoded.comparator_name.as_deref(),
            Some("cairn.InternalKeyComparator")
        );
        assert_eq!(decoded.log_number, Some(7));
        assert_eq!(decoded.next_file_number, Some(42));
        assert_eq!(decoded.last_sequence, Some(1000));
        assert_eq!(decoded.deleted_files, vec![(0, 5), (1, 6)]);
        assert_eq!(decoded.new_files.len(), 2);
        assert_eq!(decoded.new_files[0].1.number, 10);
        assert_eq!(decoded.new_files[1].1.largest, b"q");
    }

    #[test]
    fn empty_edit_roundtrips() {
        let e = VersionEdit::new();
        assert!(VersionEdit::decode(&e.encode())
            .unwrap()
            .new_files
            .is_empty());
    }

    #[test]
    fn unknown_tag_is_rejected() {
        // Tag 99 is not defined.
        let mut e = Encoder::new();
        e.put_varint32(99).put_varint64(1);
        assert!(VersionEdit::decode(e.as_bytes()).is_err());
    }

    #[test]
    fn truncated_field_is_rejected() {
        let mut e = Encoder::new();
        e.put_varint32(TAG_LOG_NUMBER); // tag with no following value
        assert!(VersionEdit::decode(e.as_bytes()).is_err());
    }
}
