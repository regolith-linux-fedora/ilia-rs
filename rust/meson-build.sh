#!/bin/sh
# Usage: meson-build.sh <cargo> <crate-dir> <cargo-target-dir> <output>
# Cargo discovers rust/.cargo/config.toml (for example a vendored source
# for offline builds) from the working directory, so build from inside the
# crate.
set -eu
cargo=$1
crate_dir=$2
target_dir=$3
output=$4
# Meson passes the output relative to the build directory; resolve it
# before leaving that directory.
case "$output" in
    /*) ;;
    *) output="$PWD/$output" ;;
esac
cd "$crate_dir"
CARGO_TARGET_DIR="$target_dir" "$cargo" build --release --locked
cp "$target_dir/release/ilia" "$output"
