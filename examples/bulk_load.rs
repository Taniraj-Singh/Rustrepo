//! Bulk-loads a million-ish keys and prints the engine's runtime metrics.
//!
//! Useful for eyeballing flush/compaction behavior and cache effectiveness.
//!
//! ```text
//! cargo run --release --example bulk_load
//! ```

use std::time::Instant;

use cairn::{Cairn, CompressionKind, Config};

fn main() -> cairn::Result<()> {
    let dir = std::env::temp_dir().join(format!("cairn-example-bulk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // A small write buffer forces many flushes and compactions even for a
    // modest dataset, so the metrics are interesting.
    let config = Config::default()
        .write_buffer_size(256 * 1024)
        .compression(CompressionKind::Lz)
        .bloom_bits_per_key(10);
    let db = Cairn::open(&dir, config)?;

    let n: u32 = if cfg!(debug_assertions) {
        50_000
    } else {
        500_000
    };

    let start = Instant::now();
    for i in 0..n {
        let key = format!("key:{i:09}");
        let value = format!("value-payload-for-record-{i}");
        db.put(key.as_bytes(), value.as_bytes())?;
    }
    let write_elapsed = start.elapsed();

    // Read a random-ish sample back.
    let start = Instant::now();
    let mut hits = 0u32;
    let mut i = 7u32;
    for _ in 0..n / 10 {
        i = i.wrapping_mul(2_654_435_761).wrapping_add(1) % n;
        let key = format!("key:{i:09}");
        if db.get(key.as_bytes())?.is_some() {
            hits += 1;
        }
    }
    let read_elapsed = start.elapsed();

    println!("wrote {n} keys in {write_elapsed:.2?}");
    println!("  {:.0} writes/sec", n as f64 / write_elapsed.as_secs_f64());
    println!("read {} keys in {read_elapsed:.2?} ({hits} hits)", n / 10);

    let m = db.metrics();
    println!("\nmetrics:");
    println!("  puts            {}", m.puts);
    println!("  gets            {}", m.gets);
    println!("  flushes         {}", m.flushes);
    println!("  compactions     {}", m.compactions);
    println!("  blocks read     {}", m.blocks_read);
    println!("  cache hit ratio {:.1}%", m.cache_hit_ratio() * 100.0);
    println!("  bloom rejects   {:.1}%", m.bloom_reject_ratio() * 100.0);
    println!("open tables       {}", db.open_table_count());

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
