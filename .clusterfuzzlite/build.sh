#!/bin/bash -eu
# ClusterFuzzLite build script for Cairn.
#
# Builds every libFuzzer target in the `fuzz/` cargo-fuzz crate and copies the
# resulting binaries into $OUT, where the fuzzing infrastructure runs them.

cd "$SRC/cairn"

# Build all fuzz targets with optimizations and sanitizer instrumentation
# (the base-builder-rust image sets the appropriate RUSTFLAGS).
cargo fuzz build -O

FUZZ_TARGET_OUTPUT_DIR="fuzz/target/x86_64-unknown-linux-gnu/release"
for target in $(cargo fuzz list); do
    cp "${FUZZ_TARGET_OUTPUT_DIR}/${target}" "${OUT}/"
done
