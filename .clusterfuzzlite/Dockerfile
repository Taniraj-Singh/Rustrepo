# ClusterFuzzLite build image for the Cairn fuzz targets.
#
# Uses the OSS-Fuzz Rust base builder, which provides a nightly toolchain and
# cargo-fuzz. The whole repository is copied in; build.sh compiles the fuzz
# crate and stages each `*_fuzzer` binary into $OUT.
FROM gcr.io/oss-fuzz-base/base-builder-rust

COPY . $SRC/cairn
WORKDIR $SRC/cairn
COPY .clusterfuzzlite/build.sh $SRC/build.sh
