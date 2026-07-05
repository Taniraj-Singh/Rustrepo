# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-07-05

Initial release of Cairn, an embedded, dependency-free LSM-tree key-value
storage engine.

### Added

- **Core engine** (`Cairn`): `open`, `put`, `get`, `delete`, atomic `write` of a
  `WriteBatch`, `commit` of a `Transaction`, ordered `scan`, `snapshot`,
  `get_at` / `scan_at` for snapshot reads, and explicit `flush` / `compact`.
- **Write-ahead log** with CRC-32-checksummed records and crash-safe recovery
  that stops cleanly at a truncated tail.
- **MemTable** in-memory write buffer ordered by internal key.
- **SSTable** on-disk format: prefix-compressed data blocks with restart points,
  a sparse index block, a per-table bloom filter, and a fixed footer, all with
  per-block CRC-32 + compression framing.
- **Leveled compaction** with MVCC-aware version collapse and tombstone removal
  at the bottom level.
- **Version set + manifest** for durable, append-only metadata, with atomic
  `CURRENT` pointer swaps.
- **MVCC snapshots** with a snapshot list that protects versions from premature
  compaction.
- **Compression** codecs: `None`, run-length encoding, and a bounded-window
  LZSS, behind a `Codec` trait, all with capped, bounds-checked decoders.
- **Bloom filters**, an **LRU block cache**, **CRC-32** checksums, and **varint /
  fixed-int** coding — all implemented from scratch with no external
  dependencies.
- **Safety on untrusted input**: defensive parsers for every on-disk structure;
  malformed input yields structured errors, never panics or unbounded work.
- **Runtime metrics**: puts/gets, flushes, compactions, block reads, cache hit
  ratio, and bloom rejection ratio.
- **Tests**: unit tests in every module plus integration, recovery, regression,
  and malformed-input suites.
- **Fuzzing**: six `cargo fuzz` targets covering the compression, block, WAL,
  table footer, version edit, and write batch decoders, with a neutral seed
  corpus.
- **Examples** and dependency-free **benchmarks**.
- **CI**: build, test, `rustfmt --check`, and Clippy.

[Unreleased]: https://github.com/Taniraj-Singh/Rustrepo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Taniraj-Singh/Rustrepo/releases/tag/v0.1.0
