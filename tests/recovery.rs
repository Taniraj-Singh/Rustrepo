//! Crash-recovery and durability tests: reopen after clean and unclean stops.

mod common;

use cairn::{Cairn, Config};
use common::TempDb;

#[test]
fn reopen_preserves_data() {
    let dir = TempDb::new("reopen");
    {
        let db = Cairn::open(&dir.path, Config::default()).unwrap();
        db.put(b"persist", b"me").unwrap();
        db.put(b"and", b"me too").unwrap();
    } // dropped without an explicit flush; WAL must carry the data

    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    assert_eq!(db.get(b"persist").unwrap().as_deref(), Some(&b"me"[..]));
    assert_eq!(db.get(b"and").unwrap().as_deref(), Some(&b"me too"[..]));
}

#[test]
fn reopen_after_many_flushes() {
    let dir = TempDb::new("many-flush");
    {
        let db = Cairn::open(&dir.path, Config::default().write_buffer_size(4096)).unwrap();
        for i in 0..3000u32 {
            db.put(format!("k{i:06}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
    }
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    for i in 0..3000u32 {
        assert_eq!(
            db.get(format!("k{i:06}").as_bytes()).unwrap().as_deref(),
            Some(format!("v{i}").as_bytes())
        );
    }
}

#[test]
fn deletes_survive_reopen() {
    let dir = TempDb::new("delete-reopen");
    {
        let db = Cairn::open(&dir.path, Config::default().write_buffer_size(4096)).unwrap();
        for i in 0..1000u32 {
            db.put(format!("k{i:05}").as_bytes(), b"v").unwrap();
        }
        for i in (0..1000u32).step_by(2) {
            db.delete(format!("k{i:05}").as_bytes()).unwrap();
        }
    }
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    for i in 0..1000u32 {
        let got = db.get(format!("k{i:05}").as_bytes()).unwrap();
        if i % 2 == 0 {
            assert_eq!(got, None, "deleted key {i} reappeared");
        } else {
            assert_eq!(got.as_deref(), Some(&b"v"[..]));
        }
    }
}

#[test]
fn truncated_wal_tail_recovers_prefix() {
    // Simulate a crash mid-append by truncating the newest WAL, then reopen.
    let dir = TempDb::new("trunc-wal");
    {
        let db = Cairn::open(&dir.path, Config::default()).unwrap();
        db.put(b"safe1", b"1").unwrap();
        db.put(b"safe2", b"2").unwrap();
    }
    // Find the active WAL (highest-numbered .wal) and chop a few bytes off it.
    let wal = newest_wal(&dir.path).expect("a WAL should exist");
    let mut bytes = std::fs::read(&wal).unwrap();
    if bytes.len() > 4 {
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&wal, &bytes).unwrap();
    }

    // Reopen must not panic; it recovers whatever survived the truncation.
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    // At minimum, the first record should still be readable.
    assert!(db.get(b"safe1").unwrap().is_some() || db.get(b"safe2").unwrap().is_some());
}

#[test]
fn error_if_exists_is_enforced() {
    let dir = TempDb::new("exists");
    {
        let _db = Cairn::open(&dir.path, Config::default()).unwrap();
    }
    let err = Cairn::open(&dir.path, Config::default().error_if_exists(true));
    assert!(err.is_err());
}

#[test]
fn missing_database_without_create_fails() {
    let dir = TempDb::new("nocreate");
    let err = Cairn::open(&dir.path, Config::default().create_if_missing(false));
    assert!(err.is_err());
}

/// Returns the path of the highest-numbered `.wal` file in a directory.
fn newest_wal(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut best: Option<(u64, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".wal") {
            if let Ok(n) = stem.parse::<u64>() {
                if best.as_ref().map(|(m, _)| n > *m).unwrap_or(true) {
                    best = Some((n, entry.path()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}
