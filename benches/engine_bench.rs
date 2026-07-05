//! A dependency-free throughput benchmark for the engine's write, read, and scan
//! paths.
//!
//! Uses a hand-rolled harness (no criterion) so it runs on stable Rust with no
//! extra crates:
//!
//! ```text
//! cargo bench --bench engine_bench
//! ```

use std::time::Instant;

use cairn::{Cairn, CompressionKind, Config};

fn main() {
    println!(
        "cairn engine benchmark (debug_assertions = {})",
        cfg!(debug_assertions)
    );
    println!("{:-<64}", "");

    let n: u64 = if cfg!(debug_assertions) {
        20_000
    } else {
        200_000
    };

    let dir = std::env::temp_dir().join(format!("cairn-bench-engine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = Config::default()
        .write_buffer_size(1 << 20)
        .compression(CompressionKind::Lz)
        .bloom_bits_per_key(10);
    let db = Cairn::open(&dir, config).expect("open");

    // --- sequential writes ---
    let start = Instant::now();
    for i in 0..n {
        db.put(key(i).as_bytes(), value(i).as_bytes()).expect("put");
    }
    report("sequential put", n, start.elapsed());

    // --- random reads (all present) ---
    let start = Instant::now();
    let mut idx = 12345u64;
    for _ in 0..n {
        idx = next_index(idx, n);
        let _ = db.get(key(idx).as_bytes()).expect("get");
    }
    report("random get (hit)", n, start.elapsed());

    // --- random reads (all absent) ---
    let start = Instant::now();
    for i in 0..n {
        let _ = db.get(format!("absent:{i}").as_bytes()).expect("get");
    }
    report("random get (miss)", n, start.elapsed());

    // --- full scan ---
    let start = Instant::now();
    let mut it = db.scan().expect("scan");
    it.seek_to_first();
    let mut scanned = 0u64;
    while it.valid() {
        scanned += 1;
        it.next();
    }
    report("full scan", scanned, start.elapsed());

    let m = db.metrics();
    println!("{:-<64}", "");
    println!(
        "flushes={} compactions={} blocks_read={} cache_hit={:.1}% bloom_reject={:.1}%",
        m.flushes,
        m.compactions,
        m.blocks_read,
        m.cache_hit_ratio() * 100.0,
        m.bloom_reject_ratio() * 100.0
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn report(label: &str, ops: u64, elapsed: std::time::Duration) {
    let secs = elapsed.as_secs_f64();
    let per_sec = ops as f64 / secs.max(f64::MIN_POSITIVE);
    println!("{label:<20} {ops:>9} ops  {secs:>7.3}s  {per_sec:>12.0} ops/sec");
}

fn next_index(prev: u64, n: u64) -> u64 {
    prev.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1) % n
}

fn key(i: u64) -> String {
    format!("key:{i:09}")
}

fn value(i: u64) -> String {
    format!("value-payload-{i}-abcdefghijklmnop")
}
