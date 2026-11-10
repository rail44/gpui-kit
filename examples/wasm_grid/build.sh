#!/bin/sh
# Builds the guest and places it where the manifest's `entry` points.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build --manifest-path "$here/Cargo.toml" --target wasm32-unknown-unknown --release
cp "$here/target/wasm32-unknown-unknown/release/wasm_grid.wasm" "$here/app/grid.wasm"
ls -l "$here/app/grid.wasm"
