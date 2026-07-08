#!/usr/bin/env bash
#
# Time the ejs benchmark (see README.md) on lumen and, when installed, node/bun/qjs.
#
#   ./bench.sh          # lumen jit tier + node/bun/qjs if present
#   ./bench.sh --all    # also the lumen default (interp) tier — slow
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

[ -f code.js ] || ./fetch.sh

ROOT="$(cd ../.. && pwd)"
cargo build --release -q -p lumen --bin lumen

run() {
  local label="$1"
  shift
  if command -v "$1" >/dev/null 2>&1 || [ -x "$1" ]; then
    echo "--- $label ---"
    local t0 t1
    t0=$(date +%s)
    "$@" code.js | tail -c 200
    t1=$(date +%s)
    echo "    ${label}: $((t1 - t0))s"
  else
    echo "--- $label: not installed, skipping ---"
  fi
}

run "node" node
run "bun" bun
run "qjs" qjs
run "lumen (jit)" "$ROOT/target/release/lumen" --tier=jit
if [ "${1:-}" = "--all" ]; then
  run "lumen (interp)" "$ROOT/target/release/lumen"
fi
