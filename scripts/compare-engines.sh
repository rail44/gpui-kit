#!/usr/bin/env bash
# Times the same applications on gpui-shell's two engines and puts the
# figures side by side: the grid and the todo list under examples/wasm_*,
# each as a script (QuickJS) and as a WebAssembly guest.
#
#   scripts/compare-engines.sh [runs]
#
# The engines are exclusive features, so each run is two release test
# builds (crates/shell/src/engine/comparison.rs), alternated `runs` times
# (default 3) so that machine load falls on both. Every figure is already
# the best of 7 batches of 50 calls; the table shows the best and the
# median of those across runs. Two runs whose descriptions differ did
# different work, so the script stops rather than compare them.
set -euo pipefail

runs=${1:-3}
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/target/engine-comparison
lines=$(mktemp)
trap 'rm -f "$lines"' EXIT

run() {
    cargo test --manifest-path "$root/Cargo.toml" -p gpui-shell --release --quiet \
        --no-default-features --features "$1" --lib engine::comparison -- --nocapture --test-threads 1 \
        2>/dev/null | grep -o '\[engine-comparison\].*' >>"$lines"
}

for ((i = 1; i <= runs; i++)); do
    echo "run $i/$runs: quickjs" >&2
    run quickjs,components
    echo "run $i/$runs: wasm" >&2
    run wasm-engine
done

for tree in "$out"/*.quickjs.tree; do
    case=$(basename "$tree" .quickjs.tree)
    if ! cmp -s "$tree" "$out/$case.wasm.tree"; then
        echo "the engines described different trees for $case:" >&2
        diff "$tree" "$out/$case.wasm.tree" | head -20 >&2
        exit 1
    fi
done

echo "load average while timing: $(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || uptime)"
awk '
{
    engine = ""; case_ = ""; nodes = ""
    for (i = 2; i <= NF; i++) {
        split($i, kv, "=")
        if (kv[1] == "engine") engine = kv[2]
        else if (kv[1] == "case") case_ = kv[2]
        else if (kv[1] == "nodes") nodes = kv[2]
        else if (kv[2] != "") {
            key = case_ SUBSEP kv[1]
            metrics[key] = 1
            n = ++count[key, engine]
            value[key, engine, n] = kv[2] + 0
        }
    }
    size[case_] = nodes
}
function stats(key, engine,    n, i, j, t, a) {
    n = count[key, engine]
    for (i = 1; i <= n; i++) a[i] = value[key, engine, i]
    for (i = 2; i <= n; i++) {
        t = a[i]
        for (j = i - 1; j >= 1 && a[j] > t; j--) a[j + 1] = a[j]
        a[j + 1] = t
    }
    return sprintf("%7.3f / %7.3f", a[1], a[int((n + 1) / 2)])
}
END {
    printf "%-12s %-12s %6s   %-19s   %-19s\n", "case", "metric", "nodes", "quickjs best/median", "wasm best/median"
    for (key in metrics) {
        split(key, parts, SUBSEP)
        printf "%-12s %-12s %6s   %-19s   %-19s  ms\n", parts[1], parts[2], size[parts[1]], stats(key, "quickjs"), stats(key, "wasm")
    }
}' "$lines" | sort
