//! Crash recovery: rebuilding a memtable by replaying a write-ahead log.
//!
//! On open, any WAL left behind by a previous run is replayed record by record —
//! each record is a serialized [`WriteBatch`] — into a fresh [`MemTable`]. The
//! result is the exact in-memory state that existed when the process stopped,
//! including writes that had not yet been flushed to a table.
//!
//! Because a crash usually leaves the final record half-written, the WAL reader
//! stops cleanly at a truncated tail (see [`crate::storage::wal`]). A checksum
//! failure in the *middle* of a log is genuine corruption; whether that aborts
//! recovery is governed by the `paranoid` flag, mirroring
//! [`Config::paranoid_checks`](crate::config::Config::paranoid_checks).

use crate::error::Result;
use crate::key::Sequence;
use crate::log_at;
use crate::memtable::MemTable;
use crate::storage::{read_file, LogReader};
use crate::transaction::WriteBatch;
use crate::util::logging::Level;

/// Replays WAL bytes into `memtable`.
///
/// Returns the highest sequence number seen, or `None` if the log contained no
/// operations. With `paranoid` set, a corrupt (non-tail) record aborts with an
/// error; otherwise recovery stops at the corruption and keeps what it read.
pub fn replay_wal(
    wal_bytes: &[u8],
    memtable: &mut MemTable,
    paranoid: bool,
) -> Result<Option<Sequence>> {
    let mut reader = LogReader::new(wal_bytes);
    let mut max_seq: Option<Sequence> = None;
    let mut records = 0u64;

    loop {
        match reader.read_record() {
            Ok(Some(record)) => {
                let batch = WriteBatch::from_bytes(&record)?;
                let count = batch.count();
                if count > 0 {
                    let last = batch.sequence() + count as u64 - 1;
                    max_seq = Some(max_seq.map_or(last, |m| m.max(last)));
                }
                batch.insert_into(memtable)?;
                records += 1;
            }
            Ok(None) => break,
            Err(e) => {
                if paranoid {
                    return Err(e);
                }
                log_at!(Level::Warn, "stopping WAL replay at corrupt record: {e}");
                break;
            }
        }
    }

    log_at!(Level::Info, "replayed {records} WAL record(s)");
    Ok(max_seq)
}

/// Replays a WAL file at `path` into `memtable`.
pub fn replay_wal_file(
    path: impl AsRef<std::path::Path>,
    memtable: &mut MemTable,
    paranoid: bool,
) -> Result<Option<Sequence>> {
    let bytes = read_file(path)?;
    replay_wal(&bytes, memtable, paranoid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::LookupResult;
    use crate::storage::LogWriter;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-recovery-{}-{}", std::process::id(), name));
        p
    }

    fn write_batch_record(w: &mut LogWriter, seq: u64, ops: &[(bool, &[u8], &[u8])]) {
        let mut batch = WriteBatch::new();
        batch.set_sequence(seq);
        for (is_put, k, v) in ops {
            if *is_put {
                batch.put(k, v);
            } else {
                batch.delete(k);
            }
        }
        w.add_record(batch.as_bytes()).unwrap();
    }

    #[test]
    fn replays_batches_in_order() {
        let path = temp_path("replay");
        {
            let mut w = LogWriter::create(&path).unwrap();
            write_batch_record(&mut w, 1, &[(true, b"a", b"1"), (true, b"b", b"2")]);
            write_batch_record(&mut w, 3, &[(true, b"a", b"1b"), (false, b"b", b"")]);
            w.sync().unwrap();
        }
        let mut mem = MemTable::new();
        let max = replay_wal_file(&path, &mut mem, true).unwrap();
        assert_eq!(max, Some(4)); // last op is seq 3 + (2-1) = 4
        assert_eq!(mem.get(b"a", 100), LookupResult::Found(b"1b".to_vec()));
        assert_eq!(mem.get(b"b", 100), LookupResult::Deleted);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_wal_yields_none() {
        let mut mem = MemTable::new();
        assert_eq!(replay_wal(&[], &mut mem, true).unwrap(), None);
    }

    #[test]
    fn tolerates_truncated_tail() {
        let path = temp_path("trunc");
        {
            let mut w = LogWriter::create(&path).unwrap();
            write_batch_record(&mut w, 1, &[(true, b"k", b"v")]);
            write_batch_record(&mut w, 2, &[(true, b"k2", b"v2")]);
            w.sync().unwrap();
        }
        let mut bytes = read_file(&path).unwrap();
        bytes.truncate(bytes.len() - 3); // cut the last record
        let mut mem = MemTable::new();
        // Non-paranoid: recover what we can.
        replay_wal(&bytes, &mut mem, false).unwrap();
        assert_eq!(mem.get(b"k", 100), LookupResult::Found(b"v".to_vec()));
        std::fs::remove_file(&path).ok();
    }
}
