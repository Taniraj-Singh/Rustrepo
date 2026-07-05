# Contributing to Cairn

Thanks for your interest in Cairn! This document describes how to build, test,
and submit changes.

## Ground rules

- **No external dependencies in the library crate.** Cairn is deliberately
  dependency-free (`src/` uses only `std`). Please keep it that way; if you think
  a dependency is truly warranted, open an issue to discuss it first. (The `fuzz/`
  crate is exempt — it needs `libfuzzer-sys`.)
- **Safety on untrusted input is non-negotiable.** Any code that parses bytes read
  from disk must validate lengths, offsets, and counts before use, use checked
  arithmetic, and return a structured [`Error`] on bad input — never panic, read
  out of bounds, loop forever, or allocate without bound. New on-disk formats need
  a matching malformed-input test and, ideally, a fuzz target.
- **Every change compiles cleanly.** `cargo build`, `cargo test`,
  `cargo fmt --check`, and `cargo clippy --all-targets` must all pass.

## Development setup

```sh
git clone https://github.com/Taniraj-Singh/Rustrepo
cd Rustrepo
cargo build
cargo test
```

## Before you open a pull request

Run the full local check that CI runs:

```sh
cargo fmt --all -- --check       # formatting
cargo clippy --all-targets       # lints (treat warnings as things to fix)
cargo test                       # unit + integration + doc tests
```

If you touched a parser or on-disk format, also build the fuzz targets and run
the one closest to your change for a minute or two:

```sh
cargo +nightly fuzz build
cargo +nightly fuzz run <target> -- -max_total_time=60
```

## Coding style

- Follow `rustfmt` defaults (enforced by CI).
- Write doc comments (`///`) for all public items; the crate builds with
  `#![warn(missing_docs)]`.
- Prefer small, focused modules that mirror the existing layout.
- Add tests alongside the code (`#[cfg(test)] mod tests`) and, for user-visible
  behavior, an integration test under `tests/`.
- Keep commits focused and write clear messages explaining the *why*.

## Testing guidance

- **Unit tests** live next to the code they exercise.
- **Integration tests** in `tests/` drive the public `Cairn` API end to end. Use
  the `common::TempDb` helper for scratch directories that clean themselves up.
- **Regression tests** should capture the specific invariant a bug violated.
- **Malformed-input tests** feed random and adversarial bytes to decoders and
  assert no panics.

## Reporting bugs

Please include the smallest input or sequence of operations that reproduces the
problem, the expected vs. actual behavior, and your platform and Rust version.

## License

By contributing, you agree that your contributions are licensed under the
project's [MIT License](LICENSE).

[`Error`]: src/error.rs
