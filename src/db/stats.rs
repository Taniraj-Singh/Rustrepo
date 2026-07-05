//! Introspection: level summaries and LevelDB-style property queries.
//!
//! These methods let an operator inspect the shape of the LSM tree at runtime —
//! how many tables sit at each level, how large they are, and how much is
//! buffered in memory — without reaching into internals. [`Cairn::property`]
//! mirrors LevelDB's string-property interface for familiarity.

use std::fmt::Write as _;

use crate::db::Cairn;
use crate::util::bytes::escape;

/// A per-level snapshot of table count and total size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelSummary {
    /// The level number (0 is the newest, flushed from memtables).
    pub level: usize,
    /// Number of tables at this level.
    pub files: usize,
    /// Total bytes of all tables at this level.
    pub bytes: u64,
}

impl Cairn {
    /// Returns a per-level summary of the current version.
    ///
    /// Levels with no files are still included, so the returned vector always
    /// has one entry per configured level.
    pub fn level_summary(&self) -> Vec<LevelSummary> {
        let version = self.lock().versions.current();
        (0..version.num_levels())
            .map(|level| LevelSummary {
                level,
                files: version.num_files(level),
                bytes: version.level_size(level),
            })
            .collect()
    }

    /// Approximate bytes buffered in the active memtable (not yet flushed).
    pub fn approximate_memory_usage(&self) -> usize {
        self.lock().mem.approximate_memory_usage()
    }

    /// Total bytes across every table on disk.
    pub fn total_file_size(&self) -> u64 {
        let version = self.lock().versions.current();
        (0..version.num_levels())
            .map(|level| version.level_size(level))
            .sum()
    }

    /// Answers a named property query, or `None` for an unknown name.
    ///
    /// Supported properties:
    ///
    /// * `cairn.num-files-at-level<N>` — table count at level `N`.
    /// * `cairn.total-file-size` — total on-disk bytes.
    /// * `cairn.approximate-memory-usage` — memtable bytes.
    /// * `cairn.stats` — a multi-line per-level table.
    /// * `cairn.sstables` — a listing of every table and its key range.
    pub fn property(&self, name: &str) -> Option<String> {
        if let Some(rest) = name.strip_prefix("cairn.num-files-at-level") {
            let level: usize = rest.parse().ok()?;
            let version = self.lock().versions.current();
            if level >= version.num_levels() {
                return None;
            }
            return Some(version.num_files(level).to_string());
        }

        match name {
            "cairn.total-file-size" => Some(self.total_file_size().to_string()),
            "cairn.approximate-memory-usage" => Some(self.approximate_memory_usage().to_string()),
            "cairn.stats" => Some(self.format_stats()),
            "cairn.sstables" => Some(self.format_sstables()),
            _ => None,
        }
    }

    /// Renders `cairn.stats`: a fixed-width per-level table.
    fn format_stats(&self) -> String {
        let summaries = self.level_summary();
        let mut out = String::new();
        let _ = writeln!(out, "{:>6} {:>8} {:>14}", "Level", "Files", "Size(bytes)");
        let _ = writeln!(out, "{:->30}", "");
        let mut total_files = 0usize;
        let mut total_bytes = 0u64;
        for s in &summaries {
            if s.files == 0 {
                continue;
            }
            let _ = writeln!(out, "{:>6} {:>8} {:>14}", s.level, s.files, s.bytes);
            total_files += s.files;
            total_bytes += s.bytes;
        }
        let _ = writeln!(out, "{:->30}", "");
        let _ = writeln!(out, "{:>6} {:>8} {:>14}", "Total", total_files, total_bytes);
        let _ = writeln!(out, "memtable: {} bytes", self.approximate_memory_usage());
        out
    }

    /// Renders `cairn.sstables`: every table with its key range.
    fn format_sstables(&self) -> String {
        let version = self.lock().versions.current();
        let mut out = String::new();
        for level in 0..version.num_levels() {
            let files = version.files_at(level);
            if files.is_empty() {
                continue;
            }
            let _ = writeln!(out, "--- level {level} ({} files) ---", files.len());
            for f in files {
                let _ = writeln!(
                    out,
                    "  #{:<6} {:>10} bytes  [{} .. {}]",
                    f.number,
                    f.file_size,
                    escape(user_key(&f.smallest)),
                    escape(user_key(&f.largest)),
                );
            }
        }
        out
    }
}

/// The user-key portion of an internal key (mirrors the private helper in the
/// parent module; kept local so this file has no cross-module reach-through).
fn user_key(internal: &[u8]) -> &[u8] {
    if internal.len() >= 8 {
        &internal[..internal.len() - 8]
    } else {
        internal
    }
}

#[cfg(test)]
mod tests {
    use crate::{Cairn, Config};
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-stats-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn properties_and_summaries() {
        let dir = temp_dir("props");
        let db = Cairn::open(&dir, Config::default().write_buffer_size(4096)).unwrap();
        for i in 0..2000u32 {
            db.put(format!("k{i:05}").as_bytes(), b"value-data")
                .unwrap();
        }

        // Level summary has one entry per level.
        let summary = db.level_summary();
        assert_eq!(summary.len(), db.config().max_levels);
        // Some flushing must have produced level-0 files.
        assert!(summary.iter().any(|s| s.files > 0));

        // Named properties.
        assert!(db.property("cairn.num-files-at-level0").is_some());
        assert!(db.property("cairn.total-file-size").is_some());
        assert!(db.property("cairn.stats").unwrap().contains("Level"));
        assert!(db.property("cairn.sstables").is_some());
        assert!(db.property("cairn.unknown-property").is_none());
        assert!(db.property("cairn.num-files-at-level999").is_none());

        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_usage_reported() {
        let dir = temp_dir("mem");
        let db = Cairn::open(&dir, Config::default()).unwrap();
        db.put(b"a", b"b").unwrap();
        assert!(db.approximate_memory_usage() > 0);
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }
}
