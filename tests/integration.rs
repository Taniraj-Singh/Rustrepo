//! End-to-end integration tests driving the public [`Cairn`] API.

mod common;

use cairn::{Cairn, CompressionKind, Config, Transaction, WriteBatch};
use common::TempDb;

#[test]
fn basic_crud() {
    let dir = TempDb::new("crud");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();

    assert_eq!(db.get(b"k").unwrap(), None);
    db.put(b"k", b"v").unwrap();
    assert_eq!(db.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
    db.put(b"k", b"v2").unwrap();
    assert_eq!(db.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));
    db.delete(b"k").unwrap();
    assert_eq!(db.get(b"k").unwrap(), None);
    // Deleting an absent key is a no-op.
    db.delete(b"never").unwrap();
}

#[test]
fn empty_key_and_value_are_allowed() {
    let dir = TempDb::new("empty");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    db.put(b"", b"empty-key").unwrap();
    db.put(b"empty-value", b"").unwrap();
    assert_eq!(db.get(b"").unwrap().as_deref(), Some(&b"empty-key"[..]));
    assert_eq!(db.get(b"empty-value").unwrap().as_deref(), Some(&b""[..]));
}

#[test]
fn write_batch_is_atomic_and_ordered() {
    let dir = TempDb::new("batch");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();

    let mut batch = WriteBatch::new();
    batch.put(b"a", b"1");
    batch.put(b"b", b"2");
    batch.delete(b"a");
    batch.put(b"c", b"3");
    db.write(batch).unwrap();

    // Within the batch, the later delete of "a" wins.
    assert_eq!(db.get(b"a").unwrap(), None);
    assert_eq!(db.get(b"b").unwrap().as_deref(), Some(&b"2"[..]));
    assert_eq!(db.get(b"c").unwrap().as_deref(), Some(&b"3"[..]));
}

#[test]
fn transaction_commit() {
    let dir = TempDb::new("txn");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    let mut txn = Transaction::new();
    txn.put(b"x", b"1").put(b"y", b"2").delete(b"x");
    db.commit(txn).unwrap();
    assert_eq!(db.get(b"x").unwrap(), None);
    assert_eq!(db.get(b"y").unwrap().as_deref(), Some(&b"2"[..]));
}

#[test]
fn large_dataset_roundtrips() {
    let dir = TempDb::new("large");
    let config = Config::default().write_buffer_size(64 * 1024);
    let db = Cairn::open(&dir.path, config).unwrap();

    let n = 5000u32;
    for i in 0..n {
        db.put(key(i).as_bytes(), value(i).as_bytes()).unwrap();
    }
    for i in 0..n {
        assert_eq!(
            db.get(key(i).as_bytes()).unwrap().as_deref(),
            Some(value(i).as_bytes())
        );
    }
    // Overwrite the first half.
    for i in 0..n / 2 {
        db.put(key(i).as_bytes(), b"updated").unwrap();
    }
    for i in 0..n / 2 {
        assert_eq!(
            db.get(key(i).as_bytes()).unwrap().as_deref(),
            Some(&b"updated"[..])
        );
    }
}

#[test]
fn scan_is_sorted_and_hides_deletes() {
    let dir = TempDb::new("scan");
    let db = Cairn::open(&dir.path, Config::default().write_buffer_size(8192)).unwrap();
    for i in 0..1000u32 {
        db.put(key(i).as_bytes(), value(i).as_bytes()).unwrap();
    }
    for i in (0..1000u32).step_by(3) {
        db.delete(key(i).as_bytes()).unwrap();
    }

    let mut it = db.scan().unwrap();
    it.seek_to_first();
    let mut prev: Option<Vec<u8>> = None;
    let mut count = 0;
    while it.valid() {
        if let Some(p) = &prev {
            assert!(it.key() > p.as_slice());
        }
        prev = Some(it.key().to_vec());
        count += 1;
        it.next();
    }
    // 1000 keys minus every third one deleted.
    let deleted = (0..1000u32).step_by(3).count();
    assert_eq!(count, 1000 - deleted);
}

#[test]
fn scan_seek_positions_correctly() {
    let dir = TempDb::new("seek");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    for i in 0..100u32 {
        db.put(key(i).as_bytes(), value(i).as_bytes()).unwrap();
    }
    let mut it = db.scan().unwrap();
    it.seek(key(50).as_bytes());
    assert!(it.valid());
    assert_eq!(it.key(), key(50).as_bytes());
}

#[test]
fn snapshots_are_isolated() {
    let dir = TempDb::new("snap");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    db.put(b"k", b"v1").unwrap();
    let snap1 = db.snapshot();
    db.put(b"k", b"v2").unwrap();
    let snap2 = db.snapshot();
    db.delete(b"k").unwrap();

    assert_eq!(
        db.get_at(b"k", &snap1).unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    assert_eq!(
        db.get_at(b"k", &snap2).unwrap().as_deref(),
        Some(&b"v2"[..])
    );
    assert_eq!(db.get(b"k").unwrap(), None);
}

#[test]
fn all_compression_kinds_work() {
    for kind in [
        CompressionKind::None,
        CompressionKind::Rle,
        CompressionKind::Lz,
    ] {
        let dir = TempDb::new(&format!("compress-{kind:?}"));
        let db = Cairn::open(
            &dir.path,
            Config::default().compression(kind).write_buffer_size(8192),
        )
        .unwrap();
        for i in 0..1000u32 {
            db.put(key(i).as_bytes(), value(i).as_bytes()).unwrap();
        }
        for i in 0..1000u32 {
            assert_eq!(
                db.get(key(i).as_bytes()).unwrap().as_deref(),
                Some(value(i).as_bytes())
            );
        }
    }
}

#[test]
fn metrics_reflect_activity() {
    let dir = TempDb::new("metrics");
    let db = Cairn::open(&dir.path, Config::default().write_buffer_size(8192)).unwrap();
    for i in 0..2000u32 {
        db.put(key(i).as_bytes(), value(i).as_bytes()).unwrap();
    }
    for i in 0..2000u32 {
        let _ = db.get(key(i).as_bytes()).unwrap();
    }
    let m = db.metrics();
    assert_eq!(m.puts, 2000);
    assert_eq!(m.gets, 2000);
    assert!(m.flushes > 0, "expected at least one flush");
}

fn key(i: u32) -> String {
    format!("key{i:07}")
}

fn value(i: u32) -> String {
    format!("value-number-{i}")
}
