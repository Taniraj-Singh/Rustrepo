//! Regression tests: specific behaviors that are easy to break and cheap to pin
//! down. Each documents an invariant worth guarding.

mod common;

use cairn::{Cairn, Config};
use common::TempDb;

/// Overwriting a key must not resurrect an older value after a flush boundary.
#[test]
fn overwrite_across_flush() {
    let dir = TempDb::new("overwrite-flush");
    let db = Cairn::open(&dir.path, Config::default().write_buffer_size(4096)).unwrap();
    // First value, forced to disk.
    db.put(b"key", b"first").unwrap();
    db.flush().unwrap();
    // Second value in a new memtable.
    db.put(b"key", b"second").unwrap();
    assert_eq!(db.get(b"key").unwrap().as_deref(), Some(&b"second"[..]));
    db.flush().unwrap();
    assert_eq!(db.get(b"key").unwrap().as_deref(), Some(&b"second"[..]));
}

/// A delete must shadow a value that lives in a deeper level.
#[test]
fn delete_shadows_disk_value() {
    let dir = TempDb::new("delete-shadow");
    let db = Cairn::open(&dir.path, Config::default().write_buffer_size(4096)).unwrap();
    db.put(b"k", b"v").unwrap();
    db.flush().unwrap(); // value now on disk
    db.delete(b"k").unwrap(); // tombstone in memtable
    assert_eq!(db.get(b"k").unwrap(), None);
    db.flush().unwrap(); // tombstone now on disk too
    assert_eq!(db.get(b"k").unwrap(), None);
}

/// Compaction must preserve the newest value and never expose a stale one.
#[test]
fn compaction_preserves_latest() {
    let dir = TempDb::new("compact-latest");
    let mut config = Config::default().write_buffer_size(4096);
    config.level0_compaction_trigger = 2;
    let db = Cairn::open(&dir.path, config).unwrap();

    for round in 0..30u32 {
        for i in 0..100u32 {
            db.put(
                format!("k{i:03}").as_bytes(),
                format!("r{round:02}").as_bytes(),
            )
            .unwrap();
        }
    }
    db.compact().unwrap();
    for i in 0..100u32 {
        assert_eq!(
            db.get(format!("k{i:03}").as_bytes()).unwrap().as_deref(),
            Some(&b"r29"[..])
        );
    }
}

/// A tombstone must not be dropped by compaction while it still shadows a value
/// in a level that has not yet been merged.
#[test]
fn tombstone_not_dropped_prematurely() {
    let dir = TempDb::new("tombstone-keep");
    let mut config = Config::default().write_buffer_size(4096);
    config.level0_compaction_trigger = 2;
    let db = Cairn::open(&dir.path, config).unwrap();

    // Insert many keys, flush, then delete half and keep writing to trigger
    // compactions while the deletes must remain effective.
    for i in 0..500u32 {
        db.put(format!("k{i:04}").as_bytes(), b"v").unwrap();
    }
    db.flush().unwrap();
    for i in (0..500u32).step_by(2) {
        db.delete(format!("k{i:04}").as_bytes()).unwrap();
    }
    for i in 500..1500u32 {
        db.put(format!("k{i:04}").as_bytes(), b"v").unwrap();
    }
    db.compact().unwrap();

    for i in 0..500u32 {
        let got = db.get(format!("k{i:04}").as_bytes()).unwrap();
        if i % 2 == 0 {
            assert_eq!(got, None, "deleted key {i} came back after compaction");
        } else {
            assert_eq!(got.as_deref(), Some(&b"v"[..]));
        }
    }
}

/// Values with binary (non-UTF-8) content survive a full round-trip.
#[test]
fn binary_keys_and_values() {
    let dir = TempDb::new("binary");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    let key = vec![0u8, 255, 1, 254, 2, 253];
    let value = vec![0xff, 0x00, 0xff, 0x00, 0x80];
    db.put(&key, &value).unwrap();
    assert_eq!(db.get(&key).unwrap().as_deref(), Some(value.as_slice()));
}

/// Oversized keys and values are rejected with an error, not silently truncated.
#[test]
fn oversized_inputs_are_rejected() {
    let dir = TempDb::new("oversize");
    let config = Config {
        max_key_size: 16,
        max_value_size: 16,
        ..Config::default()
    };
    let db = Cairn::open(&dir.path, config).unwrap();

    assert!(db.put(&[b'k'; 100], b"v").is_err());
    assert!(db.put(b"k", &[b'v'; 100]).is_err());
    // Within limits is fine.
    assert!(db.put(b"k", b"v").is_ok());
}

/// Reads at a snapshot taken before a key existed must not see it.
#[test]
fn snapshot_before_insert() {
    let dir = TempDb::new("snap-before");
    let db = Cairn::open(&dir.path, Config::default()).unwrap();
    let snap = db.snapshot();
    db.put(b"new", b"value").unwrap();
    assert_eq!(db.get_at(b"new", &snap).unwrap(), None);
    assert_eq!(db.get(b"new").unwrap().as_deref(), Some(&b"value"[..]));
}
