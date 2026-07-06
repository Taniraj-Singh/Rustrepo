# ClusterFuzzLite build image for the Cairn fuzz targets.
#
# Uses the OSS-Fuzz Rust base builder, which provides a nightly toolchain and
# cargo-fuzz. The Cairn crate is at the repository root, so the whole repo is
# copied to $SRC and the build runs from there (matching how Fenrir extracts the
# repository to $SRC).
FROM gcr.io/oss-fuzz-base/base-builder-rust

COPY . $SRC/
WORKDIR $SRC
COPY .clusterfuzzlite/build.sh $SRC/build.sh
