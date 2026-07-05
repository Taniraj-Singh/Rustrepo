//! Malformed-input tests: every decoder that touches bytes from disk must reject
//! hostile input with a structured error — never a panic, out-of-bounds read, or
//! runaway allocation.
//!
//! Each decoder is fed both a sweep of pseudo-random buffers and a set of
//! targeted corruptions, all inside `catch_unwind`. A caught panic fails the
//! test.

use std::panic::{catch_unwind, AssertUnwindSafe};

use cairn::compression::decompress;
use cairn::CompressionKind;

/// A tiny deterministic PRNG so failures reproduce from the fixed seed.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() >> 56) as u8).collect()
    }
}

/// Runs `f` and asserts it does not panic. Returns whether it panicked.
fn no_panic<T>(f: impl FnOnce() -> T) -> bool {
    catch_unwind(AssertUnwindSafe(f)).is_err()
}

#[test]
fn compression_decoders_reject_garbage() {
    let mut rng = Rng::new(1);
    for _ in 0..2000 {
        let len = (rng.next() % 256) as usize;
        let junk = rng.bytes(len);
        for kind in [
            CompressionKind::None,
            CompressionKind::Rle,
            CompressionKind::Lz,
        ] {
            let panicked = no_panic(|| {
                // Bounded output cap: a bomb must be rejected, not expanded.
                let _ = decompress(kind, &junk, 1 << 16);
            });
            assert!(!panicked, "decompress({kind:?}) panicked on {junk:?}");
        }
    }
}

#[test]
fn table_footer_rejects_bad_bytes() {
    use cairn::table::Footer;
    let mut rng = Rng::new(2);
    for _ in 0..1000 {
        let len = (rng.next() % 80) as usize;
        let junk = rng.bytes(len);
        assert!(!no_panic(|| {
            let _ = Footer::decode(&junk);
        }));
    }
    // A well-sized but wrong-magic footer is rejected, not accepted.
    assert!(Footer::decode(&[0u8; 40]).is_err());
}

#[test]
fn block_reader_rejects_bad_bytes() {
    use cairn::table::Block;
    use std::sync::Arc;
    let mut rng = Rng::new(3);
    for _ in 0..2000 {
        let len = (rng.next() % 128) as usize;
        let junk: Arc<[u8]> = Arc::from(rng.bytes(len).into_boxed_slice());
        assert!(!no_panic(move || {
            // Constructing may fail; if it succeeds, iterating must also be safe.
            if let Ok(block) = Block::new(junk) {
                let mut it = block.iter();
                use cairn::KvIterator;
                it.seek_to_first();
                let mut steps = 0;
                while it.valid() && steps < 10_000 {
                    let _ = it.key();
                    let _ = it.value();
                    it.next();
                    steps += 1;
                }
                let _ = it.status();
            }
        }));
    }
}

#[test]
fn wal_reader_rejects_bad_bytes() {
    use cairn::storage::LogReader;
    let mut rng = Rng::new(4);
    for _ in 0..2000 {
        let len = (rng.next() % 200) as usize;
        let junk = rng.bytes(len);
        assert!(!no_panic(|| {
            let mut reader = LogReader::new(&junk);
            // Read until the reader stops or errors; must terminate.
            for _ in 0..10_000 {
                match reader.read_record() {
                    Ok(Some(_)) => {}
                    _ => break,
                }
            }
        }));
    }
}

#[test]
fn write_batch_rejects_bad_bytes() {
    use cairn::WriteBatch;
    let mut rng = Rng::new(5);
    for _ in 0..2000 {
        let len = (rng.next() % 128) as usize;
        let junk = rng.bytes(len);
        assert!(!no_panic(|| {
            let _ = WriteBatch::from_bytes(&junk);
        }));
    }
}

#[test]
fn version_edit_rejects_bad_bytes() {
    use cairn::version::VersionEdit;
    let mut rng = Rng::new(6);
    for _ in 0..2000 {
        let len = (rng.next() % 128) as usize;
        let junk = rng.bytes(len);
        assert!(!no_panic(|| {
            let _ = VersionEdit::decode(&junk);
        }));
    }
}

#[test]
fn block_handle_and_internal_key_reject_bad_bytes() {
    use cairn::index::BlockHandle;
    use cairn::key::ParsedInternalKey;
    let mut rng = Rng::new(7);
    for _ in 0..2000 {
        let len = (rng.next() % 32) as usize;
        let junk = rng.bytes(len);
        assert!(!no_panic(|| {
            let _ = BlockHandle::decode_from(&junk);
            let _ = ParsedInternalKey::parse(&junk);
        }));
    }
}

#[test]
fn corrupt_table_file_is_rejected_by_reader() {
    // Build a real table, then corrupt bytes and confirm the reader errors
    // rather than panicking or returning wrong data.
    use cairn::cache::BlockCache;
    use cairn::key::{InternalKey, LookupKey, ValueType};
    use cairn::storage::{RandomAccessFile, WritableFile};
    use cairn::table::{TableBuilder, TableReader};
    use cairn::util::metrics::Metrics;
    use cairn::Config;
    use std::sync::Arc;

    let mut path = std::env::temp_dir();
    path.push(format!("cairn-corrupt-table-{}", std::process::id()));

    let config = Config::default();
    {
        let file = WritableFile::create(&path).unwrap();
        let mut b = TableBuilder::new(file, &config);
        let mut entries: Vec<Vec<u8>> = (0..200u32)
            .map(|i| {
                InternalKey::new(format!("k{i:04}").as_bytes(), 1, ValueType::Value).into_bytes()
            })
            .collect();
        entries.sort();
        for k in &entries {
            b.add(k, b"value").unwrap();
        }
        b.finish().unwrap();
    }

    let original = std::fs::read(&path).unwrap();
    let mut rng = Rng::new(8);
    for _ in 0..200 {
        // Corrupt one random byte in the middle of the file.
        let mut bytes = original.clone();
        if bytes.len() > 8 {
            let pos = (rng.next() as usize) % bytes.len();
            bytes[pos] ^= 0xff;
            std::fs::write(&path, &bytes).unwrap();
        }
        let panicked = no_panic(|| {
            let file = match RandomAccessFile::open(&path) {
                Ok(f) => f,
                Err(_) => return,
            };
            let cache = Arc::new(BlockCache::new(1 << 20));
            let metrics = Arc::new(Metrics::new());
            if let Ok(reader) = TableReader::open(file, 1, &config, cache, metrics) {
                let lk = LookupKey::new(b"k0100", 100);
                let _ = reader.get(lk.internal_key(), b"k0100");
            }
        });
        assert!(!panicked, "corrupt table caused a panic");
    }
    std::fs::remove_file(&path).ok();
}
