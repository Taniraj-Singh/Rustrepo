//! Micro-benchmarks for the low-level codecs: compression, CRC-32, and the bloom
//! filter. Dependency-free harness.
//!
//! ```text
//! cargo bench --bench codec_bench
//! ```

use std::time::Instant;

use cairn::bloom;
use cairn::checksum::crc32;
use cairn::compression::{compress, decompress};
use cairn::CompressionKind;

fn main() {
    println!("cairn codec benchmark");
    println!("{:-<70}", "");

    // Build a few representative payloads.
    let compressible = b"the quick brown fox jumps over the lazy dog. ".repeat(4096);
    let incompressible = pseudo_random(compressible.len());
    let mib = 1 << 20;

    println!(
        "compression throughput (payload {} KiB):",
        compressible.len() / 1024
    );
    for kind in [
        CompressionKind::None,
        CompressionKind::Rle,
        CompressionKind::Lz,
    ] {
        bench_codec(kind, "text", &compressible);
        bench_codec(kind, "random", &incompressible);
    }

    println!("\nCRC-32 throughput:");
    let data = pseudo_random(mib);
    let iters = 200;
    let start = Instant::now();
    let mut acc = 0u32;
    for _ in 0..iters {
        acc ^= crc32(&data);
    }
    let mbps = (mib as f64 * iters as f64) / (1024.0 * 1024.0) / start.elapsed().as_secs_f64();
    println!("  {mbps:>8.0} MiB/s  (checksum {acc:#010x})");

    println!("\nbloom filter:");
    let keys: Vec<Vec<u8>> = (0..100_000u32).map(|i| i.to_le_bytes().to_vec()).collect();
    let start = Instant::now();
    let filter = bloom::build_from_keys(&keys, 10);
    println!(
        "  build   {:>8.0} keys/sec ({} bytes)",
        keys.len() as f64 / start.elapsed().as_secs_f64(),
        filter.len()
    );
    let start = Instant::now();
    let mut present = 0u64;
    for k in &keys {
        if bloom::may_contain(&filter, k) {
            present += 1;
        }
    }
    println!(
        "  query   {:>8.0} lookups/sec ({}/{} positive)",
        keys.len() as f64 / start.elapsed().as_secs_f64(),
        present,
        keys.len()
    );
}

fn bench_codec(kind: CompressionKind, label: &str, data: &[u8]) {
    let iters = 200;

    let start = Instant::now();
    let mut encoded = Vec::new();
    for _ in 0..iters {
        encoded = compress(kind, data);
    }
    let enc_mbps = throughput(data.len(), iters, start.elapsed());

    let start = Instant::now();
    for _ in 0..iters {
        let _ = decompress(kind, &encoded, data.len()).expect("decode");
    }
    let dec_mbps = throughput(data.len(), iters, start.elapsed());

    let ratio = data.len() as f64 / encoded.len().max(1) as f64;
    println!(
        "  {:<5} {:<7}  enc {enc_mbps:>7.0} MiB/s  dec {dec_mbps:>7.0} MiB/s  ratio {ratio:>4.1}x",
        format!("{kind:?}"),
        label
    );
}

fn throughput(bytes: usize, iters: usize, elapsed: std::time::Duration) -> f64 {
    (bytes as f64 * iters as f64) / (1024.0 * 1024.0) / elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
}

fn pseudo_random(len: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}
