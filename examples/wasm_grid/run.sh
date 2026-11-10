#!/bin/sh
# Opens the 40 x 5 grid in gpui-shell.
#
#   examples/wasm_grid/run.sh        the WebAssembly guest, on the wasm engine
#   examples/wasm_grid/run.sh js     the same grid as a script, on QuickJS
#
# `--no-default-features` leaves out the `webview` feature, which needs the
# system's WebKitGTK libraries and nothing here uses.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
case "${1:-wasm}" in
wasm)
    "$here/build.sh"
    exec cargo run --manifest-path "$root/Cargo.toml" -p gpui-shell --release \
        --no-default-features --features wasm-engine -- run "$here/app"
    ;;
js)
    exec cargo run --manifest-path "$root/Cargo.toml" -p gpui-shell --release \
        --no-default-features --features quickjs,components -- run "$here/app-js"
    ;;
*)
    echo "usage: $0 [wasm|js]" >&2
    exit 2
    ;;
esac
