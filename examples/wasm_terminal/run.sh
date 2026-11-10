#!/bin/sh
# Opens the terminal-like grid guest in gpui-shell's wasm engine.
#
# `--no-default-features` leaves out the `webview` feature, which needs the
# system's WebKitGTK libraries and nothing here uses.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
"$here/build.sh"
exec cargo run --manifest-path "$root/Cargo.toml" -p gpui-shell --release \
    --no-default-features --features wasm-engine -- run "$here/app"
