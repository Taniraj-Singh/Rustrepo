//! The set of live tables, organized into levels — and how it evolves.
//!
//! A [`Version`] is an immutable snapshot of which table files exist at which
//! LSM level. [`VersionSet`] owns the *current* version, the file-number and
//! sequence counters, and the manifest that makes all of it durable. Changes are
//! never mutations of a `Version`; instead a [`VersionEdit`] is logged to the
//! manifest and applied to produce a new `Version`
//! ([`VersionSet::log_and_apply`]).
//!
//! ## Level invariants
//!
//! * **Level 0** tables may have overlapping key ranges (they are flushed
//!   memtables). They are ordered newest-first so a read sees the most recent
//!   version of a key first.
//! * **Levels ≥ 1** hold tables with disjoint key ranges, sorted by key, so a
//!   read binary-searches to at most one table per level.

pub mod edit;

pub use edit::{FileMetaData, VersionEdit};

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::manifest::{read_current, set_current_file, ManifestWriter};
use crate::storage::{manifest_file_name, read_file, LogReader};

/// Returns the user-key portion of an internal key.
fn user_key(internal: &[u8]) -> &[u8] {
    if internal.len() >= 8 {
        &internal[..internal.len() - 8]
    } else {
        internal
    }
}

/// An immutable snapshot of the live tables at each level.
#[derive(Debug, Clone)]
pub struct Version {
    // files[level] holds the tables at that level.
    files: Vec<Vec<Arc<FileMetaData>>>,
}

impl Version {
    /// Creates an empty version with `num_levels` levels.
    pub fn new(num_levels: usize) -> Version {
        Version {
            files: vec![Vec::new(); num_levels],
        }
    }

    /// The number of levels.
    pub fn num_levels(&self) -> usize {
        self.files.len()
    }

    /// The tables at `level`, in level order (newest-first for level 0).
    pub fn files_at(&self, level: usize) -> &[Arc<FileMetaData>] {
        self.files.get(level).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// The number of tables at `level`.
    pub fn num_files(&self, level: usize) -> usize {
        self.files.get(level).map_or(0, |v| v.len())
    }

    /// Total bytes of all tables at `level`.
    pub fn level_size(&self, level: usize) -> u64 {
        self.files
            .get(level)
            .map_or(0, |v| v.iter().map(|f| f.file_size).sum())
    }

    /// Total tables across all levels.
    pub fn total_files(&self) -> usize {
        self.files.iter().map(|v| v.len()).sum()
    }

    /// Files at `level` whose key range overlaps the user-key range
    /// `[smallest, largest]` (inclusive).
    pub fn overlapping_files(
        &self,
        level: usize,
        smallest: &[u8],
        largest: &[u8],
    ) -> Vec<Arc<FileMetaData>> {
        let mut out = Vec::new();
        for f in self.files_at(level) {
            let fs = user_key(&f.smallest);
            let fl = user_key(&f.largest);
            // Disjoint iff the file ends before the range or starts after it.
            let disjoint = fl < smallest || fs > largest;
            if !disjoint {
                out.push(Arc::clone(f));
            }
        }
        out
    }

    /// Picks a level that has grown past its bound and should be compacted.
    ///
    /// Level 0 is chosen by table count; deeper levels by total size. Returns the
    /// shallowest level that qualifies, or `None` if the tree is in shape.
    pub fn pick_compaction_level(&self, config: &Config) -> Option<usize> {
        if self.num_files(0) >= config.level0_compaction_trigger {
            return Some(0);
        }
        (1..self.num_levels().saturating_sub(1))
            .find(|&level| self.level_size(level) > config.level_max_bytes(level))
    }
}

/// Owns the current [`Version`], the durable manifest, and the global counters.
#[derive(Debug)]
pub struct VersionSet {
    dbname: PathBuf,
    config: Config,
    num_levels: usize,
    next_file_number: u64,
    last_sequence: u64,
    log_number: u64,
    manifest_number: u64,
    current: Arc<Version>,
    manifest: Option<ManifestWriter>,
}

impl VersionSet {
    fn empty(dbname: &Path, config: Config) -> VersionSet {
        let num_levels = config.max_levels;
        VersionSet {
            dbname: dbname.to_path_buf(),
            config,
            num_levels,
            // File numbers start at 2; 1 is reserved for the first manifest.
            next_file_number: 2,
            last_sequence: 0,
            log_number: 0,
            manifest_number: 0,
            current: Arc::new(Version::new(num_levels)),
            manifest: None,
        }
    }

    /// Initializes a brand-new database's version state and writes its first
    /// manifest.
    pub fn create(dbname: impl AsRef<Path>, config: Config) -> Result<VersionSet> {
        let mut vs = VersionSet::empty(dbname.as_ref(), config);
        vs.write_snapshot()?;
        Ok(vs)
    }

    /// Recovers version state from an existing database directory.
    pub fn recover(dbname: impl AsRef<Path>, config: Config) -> Result<VersionSet> {
        let dbname = dbname.as_ref();
        let manifest_number = read_current(dbname)?;
        let path = dbname.join(manifest_file_name(manifest_number));
        let bytes = read_file(&path).map_err(|_| Error::MissingFile(path.display().to_string()))?;
        let records = LogReader::new(&bytes).read_all()?;

        let mut vs = VersionSet::empty(dbname, config);
        let mut version = Version::new(vs.num_levels);
        for record in &records {
            let edit = VersionEdit::decode(record)?;
            if let Some(n) = edit.next_file_number {
                vs.next_file_number = vs.next_file_number.max(n);
            }
            if let Some(n) = edit.last_sequence {
                vs.last_sequence = vs.last_sequence.max(n);
            }
            if let Some(n) = edit.log_number {
                vs.log_number = n;
            }
            version = apply_edit(&version, &edit, vs.num_levels);
        }

        // Ensure the next file number is beyond everything already on disk.
        let mut max_file = manifest_number;
        for level in 0..vs.num_levels {
            for f in version.files_at(level) {
                max_file = max_file.max(f.number);
            }
        }
        vs.next_file_number = vs.next_file_number.max(max_file + 1);
        vs.current = Arc::new(version);

        // Start a fresh manifest capturing the recovered state.
        vs.write_snapshot()?;
        Ok(vs)
    }

    /// The current version snapshot.
    pub fn current(&self) -> Arc<Version> {
        Arc::clone(&self.current)
    }

    /// The database directory.
    pub fn dbname(&self) -> &Path {
        &self.dbname
    }

    /// The active configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Allocates the next unused file number.
    pub fn new_file_number(&mut self) -> u64 {
        let n = self.next_file_number;
        self.next_file_number += 1;
        n
    }

    /// The highest sequence assigned so far.
    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Advances the last-sequence counter.
    pub fn set_last_sequence(&mut self, seq: u64) {
        self.last_sequence = self.last_sequence.max(seq);
    }

    /// The current WAL file number.
    pub fn log_number(&self) -> u64 {
        self.log_number
    }

    /// Records the current WAL file number.
    pub fn set_log_number(&mut self, n: u64) {
        self.log_number = n;
    }

    /// Logs `edit` to the manifest (durably) and applies it to produce a new
    /// current version.
    pub fn log_and_apply(&mut self, mut edit: VersionEdit) -> Result<()> {
        // Persist the current counters alongside the edit so a replay recovers
        // them even without a fresh snapshot.
        edit.set_next_file_number(self.next_file_number);
        edit.set_last_sequence(self.last_sequence);
        edit.set_log_number(self.log_number);

        if let Some(w) = self.manifest.as_mut() {
            w.append(&edit)?;
        } else {
            return Err(Error::corruption("version set has no open manifest"));
        }
        let next = apply_edit(&self.current, &edit, self.num_levels);
        self.current = Arc::new(next);
        Ok(())
    }

    /// Writes a fresh manifest containing a full snapshot of the current state,
    /// and repoints `CURRENT` at it.
    fn write_snapshot(&mut self) -> Result<()> {
        let number = self.new_file_number();
        let path = self.dbname.join(manifest_file_name(number));
        let mut writer = ManifestWriter::create(&path)?;
        writer.append(&self.snapshot_edit())?;
        set_current_file(&self.dbname, number)?;
        self.manifest_number = number;
        self.manifest = Some(writer);
        Ok(())
    }

    /// Builds an edit that fully describes the current state.
    fn snapshot_edit(&self) -> VersionEdit {
        let mut edit = VersionEdit::new();
        edit.set_comparator_name("cairn.InternalKeyComparator");
        edit.set_log_number(self.log_number);
        edit.set_next_file_number(self.next_file_number);
        edit.set_last_sequence(self.last_sequence);
        for level in 0..self.num_levels {
            for f in self.current.files_at(level) {
                edit.add_file(level, (**f).clone());
            }
        }
        edit
    }
}

/// Applies an edit to `base`, returning a new version with levels re-sorted.
fn apply_edit(base: &Version, edit: &VersionEdit, num_levels: usize) -> Version {
    let mut files = base.files.clone();
    files.resize(num_levels, Vec::new());

    if !edit.deleted_files.is_empty() {
        let deleted: HashSet<(usize, u64)> = edit.deleted_files.iter().copied().collect();
        for (level, level_files) in files.iter_mut().enumerate() {
            level_files.retain(|f| !deleted.contains(&(level, f.number)));
        }
    }

    for (level, meta) in &edit.new_files {
        if *level < num_levels {
            files[*level].push(Arc::new(meta.clone()));
        }
    }

    for (level, level_files) in files.iter_mut().enumerate() {
        if level == 0 {
            // Newest (highest number) first.
            level_files.sort_by_key(|f| std::cmp::Reverse(f.number));
        } else {
            // Disjoint ranges sorted by smallest user key.
            level_files.sort_by(|a, b| user_key(&a.smallest).cmp(user_key(&b.smallest)));
        }
    }

    Version { files }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cairn-versionset-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn meta(num: u64, small: &[u8], large: &[u8]) -> FileMetaData {
        // Append a trailer so these look like internal keys.
        let mut s = small.to_vec();
        s.extend_from_slice(&[0u8; 8]);
        let mut l = large.to_vec();
        l.extend_from_slice(&[0u8; 8]);
        FileMetaData::new(num, 1000, s, l)
    }

    #[test]
    fn apply_adds_and_deletes() {
        let base = Version::new(7);
        let mut e = VersionEdit::new();
        e.add_file(0, meta(1, b"a", b"m"));
        e.add_file(0, meta(2, b"n", b"z"));
        let v = apply_edit(&base, &e, 7);
        assert_eq!(v.num_files(0), 2);
        // Level 0 sorts newest (higher number) first.
        assert_eq!(v.files_at(0)[0].number, 2);

        let mut d = VersionEdit::new();
        d.delete_file(0, 1);
        let v2 = apply_edit(&v, &d, 7);
        assert_eq!(v2.num_files(0), 1);
        assert_eq!(v2.files_at(0)[0].number, 2);
    }

    #[test]
    fn overlap_detection() {
        let base = Version::new(7);
        let mut e = VersionEdit::new();
        e.add_file(1, meta(1, b"a", b"e"));
        e.add_file(1, meta(2, b"h", b"n"));
        let v = apply_edit(&base, &e, 7);
        assert_eq!(v.overlapping_files(1, b"b", b"c").len(), 1);
        assert_eq!(v.overlapping_files(1, b"d", b"i").len(), 2);
        assert_eq!(v.overlapping_files(1, b"x", b"z").len(), 0);
    }

    #[test]
    fn create_and_recover_roundtrip() {
        let dir = temp_dir("recover");
        {
            let mut vs = VersionSet::create(&dir, Config::default()).unwrap();
            vs.set_last_sequence(100);
            vs.set_log_number(3);
            let mut e = VersionEdit::new();
            e.add_file(0, meta(5, b"a", b"z"));
            vs.log_and_apply(e).unwrap();
            assert_eq!(vs.current().num_files(0), 1);
        }
        let mut vs = VersionSet::recover(&dir, Config::default()).unwrap();
        assert_eq!(vs.current().num_files(0), 1);
        assert_eq!(vs.last_sequence(), 100);
        assert!(vs.new_file_number() > 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compaction_level_selection() {
        let config = Config {
            level0_compaction_trigger: 2,
            ..Config::default()
        };
        let base = Version::new(7);
        let mut e = VersionEdit::new();
        e.add_file(0, meta(1, b"a", b"b"));
        e.add_file(0, meta(2, b"c", b"d"));
        let v = apply_edit(&base, &e, 7);
        assert_eq!(v.pick_compaction_level(&config), Some(0));
    }
}
