# Cairn

**Cairn** is an embedded, **dependency-free** key-value storage engine built on a
log-structured merge-tree (LSM-tree), written in safe Rust. It is a from-scratch
storage engine in the spirit of LevelDB/RocksDB — an ordered, persistent
`key → value` map with crash recovery, atomic batches, MVCC snapshots, range
scans, block compression, bloom filters, and a block cache — implemented
entirely against the Rust standard library.

> A *cairn* is a stack of stones raised as a marker. The name fits an LSM engine:
> data is written as a stack of immutable, sorted tables that are periodically
> merged into deeper layers.

```rust
use cairn::{Cairn, Config};

fn main() -> cairn::Result<()> {
    let mut path = std::env::temp_dir();
    path.push("cairn-readme-demo");

    let db = Cairn::open(&path, Config::default())?;
    db.put(b"alpha", b"one")?;
    db.put(b"beta", b"two")?;
    assert_eq!(db.get(b"alpha")?.as_deref(), Some(&b"one"[..]));

    db.delete(b"alpha")?;
    assert_eq!(db.get(b"alpha")?, None);

    // Ordered range scan.
    let mut it = db.scan()?;
    it.seek_to_first();
    while it.valid() {
        println!("{:?} => {:?}", it.key(), it.value());
        it.next();
    }
    Ok(())
}
```

---

## Highlights

- **Ordered, persistent map** with `put` / `get` / `delete` and sorted range scans.
- **Crash recovery** via a write-ahead log (WAL) replayed on open.
- **Atomic writes** through [`WriteBatch`] / [`Transaction`] — all-or-nothing.
- **MVCC snapshots** for stable point-in-time reads.
- **On-disk tables (SSTables)** with per-block CRC-32 checksums, a sparse index,
  bloom filters, and prefix-compressed blocks.
- **Pluggable compression** — store / run-length / bounded-window LZSS.
- **Bounded block cache** (LRU) shared across all tables.
- **Leveled compaction** that keeps read amplification in check.
- **Zero external dependencies.** Varint coding, CRC-32, compression, bloom
  filters, the LRU cache, and the WAL are all implemented from scratch, keeping
  the trust surface small and the build reproducible.
- **Safe on untrusted input.** Every on-disk structure is parsed defensively; a
  corrupt or hostile file yields a structured error, never a panic, out-of-bounds
  read, infinite loop, or unbounded allocation. This is exercised by a
  malformed-input test suite and by `cargo fuzz`.

---

## Architecture

Cairn is a classic LSM tree. Writes are staged in memory and periodically frozen
into immutable on-disk tables; reads consult the in-memory buffer first, then the
tables newest-to-oldest; a background-free compaction merges tables to bound their
number.

### The write path

```text
  put(k, v) / write(batch)
       │
       ▼
  [ WAL append ]  ──►  durable on disk; recovery replays this after a crash
       │
       ▼
  [ MemTable ]    ──►  in-memory sorted map (ordered by internal key)
       │  (when it reaches write_buffer_size)
       ▼
  [ flush ]       ──►  immutable SSTable written at level 0
       │  (when level 0 has too many tables)
       ▼
  [ compaction ]  ──►  merge/rewrite tables into deeper levels, dropping
                       shadowed versions and obsolete tombstones
```

### The read path

A lookup consults, in order, stopping at the first match:

1. the active **memtable**,
2. **level-0** tables, newest first (they may overlap),
3. one table per **deeper level**, located by binary search (levels ≥ 1 are
   disjoint and sorted).

A **bloom filter** per table turns most "key is absent here" questions into a
definite *no* without a disk read, and decoded blocks are served from a shared
**LRU block cache**.

### Keys and versions (MVCC)

Every mutation is tagged with a monotonically increasing **sequence number** and
a **value type** (set or delete). A user key plus this 8-byte trailer forms an
*internal key*. Internal keys sort by user key ascending, then sequence
descending, so the newest version of any key is encountered first. A snapshot is
simply a sequence number; reads at a snapshot ignore any version newer than it.

### Durable metadata

The set of live tables is never mutated in place. Each change is a **version
edit** appended to the **manifest** log; replaying the manifest rebuilds the
current version. A `CURRENT` file names the live manifest and is swapped
atomically.

---

## Modules

| Module | Responsibility |
|---|---|
| [`error`](src/error.rs) | Structured `Error` / `Result` used throughout. |
| [`config`](src/config.rs) | Tunables and safety limits ([`Config`]). |
| [`key`](src/key.rs) | Internal keys, sequence numbers, value types, lookup keys. |
| [`util`](src/util/) | Varint & fixed-int coding, comparators, byte helpers, logging, metrics. |
| [`checksum`](src/checksum/) | CRC-32 (streaming + one-shot) and masking. |
| [`compression`](src/compression/) | `Codec` trait and the None / RLE / LZSS codecs. |
| [`bloom`](src/bloom/) | Bloom filters (build & query). |
| [`serializer`](src/serializer/) / [`parser`](src/parser/) | Symmetric binary encode / bounds-checked decode. |
| [`cache`](src/cache/) | Generic LRU cache and the shared block cache. |
| [`index`](src/index/) | `BlockHandle` and the in-memory sparse block index. |
| [`storage`](src/storage/) | File I/O, on-disk block framing, the WAL, and file naming. |
| [`table`](src/table/) | SSTable block format, builder, and reader. |
| [`iterator`](src/iterator/) | The `KvIterator` cursor trait and the merging iterator. |
| [`memtable`](src/memtable/) | The in-memory write buffer. |
| [`version`](src/version/) / [`manifest`](src/manifest/) | The live-table set, version edits, and the durable manifest. |
| [`transaction`](src/transaction/) | `WriteBatch` and the `Transaction` facade. |
| [`snapshot`](src/snapshot/) | MVCC snapshots and the snapshot list. |
| [`recovery`](src/recovery/) | WAL replay on open. |
| [`db`](src/db/) | The `Cairn` database facade, table cache, and scan iterator. |

---

## Building

Cairn builds on stable Rust (1.74+):

```sh
cargo build            # debug build
cargo build --release  # optimized build
cargo doc --open       # browse the API documentation
```

There are no external dependencies, so the first build needs no network access.

## Testing

```sh
cargo test                 # unit + integration + doc tests
cargo test --test integration
cargo test --test recovery
cargo test --test regression
cargo test --test malformed_input   # hostile-input safety tests
```

The suite includes unit tests in every module, end-to-end integration tests,
crash-recovery tests, regression tests for tricky LSM behaviors (tombstone
shadowing, version collapse during compaction), and a malformed-input suite that
feeds random and adversarial bytes to every decoder and asserts none of them
panic.

## Benchmarks

The benchmarks use a hand-written harness (no `criterion`) so they run on stable
Rust:

```sh
cargo bench --bench engine_bench   # put / get / scan throughput
cargo bench --bench codec_bench    # compression, CRC-32, bloom filter
```

## Fuzzing

Cairn ships [`cargo fuzz`](https://github.com/rust-fuzz/cargo-fuzz) targets that
drive every parser at the trust boundary. Each asserts that no input causes a
panic, out-of-bounds access, or unbounded allocation.

```sh
cargo install cargo-fuzz
cargo +nightly fuzz build            # build all targets
cargo +nightly fuzz run compression_fuzzer
cargo +nightly fuzz run block_fuzzer
cargo +nightly fuzz run wal_fuzzer
cargo +nightly fuzz run table_footer_fuzzer
cargo +nightly fuzz run version_edit_fuzzer
cargo +nightly fuzz run write_batch_fuzzer
```

A small, neutral seed corpus lives under [`fuzz/corpus/`](fuzz/corpus/). It
contains only benign starter inputs — no crash reproducers or exploit files.

## Examples

Runnable examples live under [`examples/`](examples/):

```sh
cargo run --example basic_usage
cargo run --example batch_and_snapshot
cargo run --example range_scan
cargo run --release --example bulk_load
```

---

## Project structure

```text
cairn/
├── Cargo.toml
├── LICENSE                 MIT
├── README.md
├── CHANGELOG.md
├── CONTRIBUTING.md
├── src/
│   ├── lib.rs              crate root + public prelude
│   ├── error.rs
│   ├── config.rs
│   ├── key.rs
│   ├── util/               coding, comparator, bytes, logging, metrics
│   ├── checksum/
│   ├── compression/        mod, rle, lz
│   ├── bloom/
│   ├── serializer/
│   ├── parser/
│   ├── cache/
│   ├── index/
│   ├── storage/            file, block, wal
│   ├── table/              block, builder, reader
│   ├── iterator/           mod, merging
│   ├── memtable/
│   ├── version/            mod, edit
│   ├── manifest/
│   ├── transaction/
│   ├── snapshot/
│   ├── recovery/
│   └── db/                 mod, table_cache, iter
├── tests/                  integration, recovery, regression, malformed_input
├── examples/
├── benches/                engine_bench, codec_bench
├── fuzz/                   cargo-fuzz crate + neutral corpus
└── .github/workflows/      CI
```

---

## Design notes & limitations

- **Concurrency.** A single mutex guards the mutable core; flushing and
  compaction run inline on the write path (no background thread). The block
  cache, table cache, metrics, and snapshot list are independently synchronized.
  `Cairn` is `Send + Sync` and can be shared behind an `Arc`.
- **Durability.** Every write is appended to the WAL and flushed to the OS before
  it is visible. Set [`Config::sync_writes`] to `true` to fsync on every batch for
  per-write durability.
- **The directory lock** is a best-effort `LOCK` file created exclusively on open
  and removed on drop; a crash can leave a stale lock that must be removed by
  hand — the honest cost of a portable, dependency-free lock.
- Cairn is a focused, readable implementation meant for learning and embedding,
  not a drop-in replacement for a production RocksDB deployment.

## License

Licensed under the [MIT License](LICENSE).

[`Cairn`]: https://docs.rs/cairn
[`Config`]: https://docs.rs/cairn
[`Config::sync_writes`]: https://docs.rs/cairn
[`WriteBatch`]: https://docs.rs/cairn
[`Transaction`]: https://docs.rs/cairn
