//! Integration tests for per-operation options, range scans, and introspection.

mod common;

use cairn::options::{ReadOptions, WriteOptions};
use cairn::{Cairn, Config};
use common::TempDb;

#[test]
fn write_options_sync_and_read_snapshot() {
    let dir = TempDb::new("opts");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();

    // A synced write is durable per call.
    db.put_with(b"k", b"v1", &WriteOptions::new().sync(true))
        .unwrap();

    let snap = db.snapshot();
    db.put(b"k", b"v2").unwrap();

    // Reading through a snapshot-pinned ReadOptions sees the old value.
    let ro = ReadOptions::new().snapshot(&snap);
    assert_eq!(db.get_with(b"k", &ro).unwrap().as_deref(), Some(&b"v1"[..]));
    // Default read options see the latest.
    assert_eq!(
        db.get_with(b"k", &ReadOptions::new()).unwrap().as_deref(),
        Some(&b"v2"[..])
    );

    // delete_with also honors options.
    db.delete_with(b"k", &WriteOptions::new().sync(true))
        .unwrap();
    assert_eq!(db.get(b"k").unwrap(), None);
}

#[test]
fn scan_with_options_at_snapshot() {
    let dir = TempDb::new("scan-opts");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    for i in 0..100u32 {
        db.put(format!("k{i:03}").as_bytes(), b"old").unwrap();
    }
    let snap = db.snapshot();
    for i in 0..100u32 {
        db.put(format!("k{i:03}").as_bytes(), b"new").unwrap();
    }

    let ro = ReadOptions::new().snapshot(&snap);
    let mut it = db.scan_with(&ro).unwrap();
    it.seek_to_first();
    let mut count = 0;
    while it.valid() {
        assert_eq!(it.value(), b"old");
        count += 1;
        it.next();
    }
    assert_eq!(count, 100);
}

#[test]
fn range_and_prefix_scans() {
    let dir = TempDb::new("range");
    let db = Cairn::open(&dir.path, Config::default().write_buffer_size(8192)).unwrap();
    for i in 0..500u32 {
        db.put(format!("item:{i:04}").as_bytes(), b"v").unwrap();
    }
    db.put(b"zzz", b"v").unwrap();

    // Half-open range.
    let pairs = db
        .range(b"item:0100", b"item:0150")
        .unwrap()
        .collect_pairs();
    assert_eq!(pairs.len(), 50);
    assert_eq!(pairs[0].0, b"item:0100");

    // Prefix scan.
    assert_eq!(db.prefix_scan(b"item:00").unwrap().count(), 100);
    assert_eq!(db.prefix_scan(b"nope").unwrap().count(), 0);

    // range_from reads to the end.
    assert!(db.range_from(b"zzz").unwrap().count() >= 1);
}

#[test]
fn introspection_properties() {
    let dir = TempDb::new("props");
    let db = Cairn::open(&dir.path, Config::default().write_buffer_size(4096)).unwrap();
    for i in 0..3000u32 {
        db.put(format!("k{i:06}").as_bytes(), b"payload-payload")
            .unwrap();
    }

    // Per-level summary covers every level.
    let summary = db.level_summary();
    assert_eq!(summary.len(), db.config().max_levels);
    assert!(summary.iter().map(|s| s.files).sum::<usize>() > 0);

    // Properties.
    assert!(db.property("cairn.stats").unwrap().contains("Total"));
    assert!(db.property("cairn.sstables").is_some());
    let total: u64 = db
        .property("cairn.total-file-size")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(total, db.total_file_size());
    assert!(db.property("cairn.bogus").is_none());
}
