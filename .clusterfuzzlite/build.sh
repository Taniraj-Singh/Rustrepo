#!/bin/bash -eu
# ClusterFuzzLite build script for Cairn.
#
# Builds every libFuzzer target in the `fuzz/` cargo-fuzz crate and copies the
# resulting binaries into $OUT, where the fuzzing infrastructure runs them.

# Fenrir/ClusterFuzzLite extracts the repository to $SRC and runs this script
# with $SRC as the repo root. The Cairn crate lives at that root (there is no
# nested subdirectory), so build from $SRC directly.
cd "$SRC"

# Build all fuzz targets with optimizations and sanitizer instrumentation
# (the base-builder-rust image sets the appropriate RUSTFLAGS).
cargo fuzz build -O

FUZZ_TARGET_OUTPUT_DIR="fuzz/target/x86_64-unknown-linux-gnu/release"
for target in $(cargo fuzz list); do
    cp "${FUZZ_TARGET_OUTPUT_DIR}/${target}" "${OUT}/"
done
