#!/bin/sh
# Builds the guest and places it where the manifest's `entry` points.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cargo build --manifest-path "$here/Cargo.toml" --target wasm32-unknown-unknown --release
cp "$here/target/wasm32-unknown-unknown/release/wasm_terminal.wasm" "$here/app/terminal.wasm"
ls -l "$here/app/terminal.wasm"
