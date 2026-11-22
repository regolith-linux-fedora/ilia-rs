#!/bin/sh
# Usage: meson-build.sh <cargo> <crate-dir> <cargo-target-dir> <output> \
#     <calculator-helper>
# Cargo discovers rust/.cargo/config.toml (for example a vendored source
# for offline builds) from the working directory, so build from inside the
# crate. <calculator-helper> is where meson installs the calculator page's
# helper; the launcher falls back to it when the helper is not beside it.
set -eu
cargo=$1
crate_dir=$2
target_dir=$3
output=$4
ILIA_CALCULATOR_HELPER=$5
export ILIA_CALCULATOR_HELPER
# Meson passes the output relative to the build directory; resolve it
# before leaving that directory.
case "$output" in
    /*) ;;
    *) output="$PWD/$output" ;;
esac
cd "$crate_dir"
CARGO_TARGET_DIR="$target_dir" "$cargo" build --release --locked
cp "$target_dir/release/ilia" "$output"
