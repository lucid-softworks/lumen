#!/usr/bin/env bash
#
# Run the classic V8 benchmark suite (v8-v7) on node, bun, and lumen and print a
# markdown comparison table. Higher is better; scores are normalized to a 2008
# reference machine at 100.
#
#   scripts/bench-compare.sh                 # node + bun + lumen (jit tier)
#   scripts/bench-compare.sh --tiers         # also include lumen bytecode + interp tiers
#
# Requires: node and bun on PATH (either is skipped with a warning if missing).
# Downloads the benchmark JS into ./v8-v7 (gitignored) on first run and builds
# the `lumen` CLI in release mode.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/v8-v7"
RAW="https://raw.githubusercontent.com/mozilla/arewefastyet/master/benchmarks/v8-v7"
FILES=(base.js richards.js deltablue.js crypto.js raytrace.js earley-boyer.js regexp.js splay.js navier-stokes.js run.js)
SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes)
BENCH_NAMES=(Richards DeltaBlue Crypto RayTrace EarleyBoyer RegExp Splay NavierStokes Score)

ALL_TIERS=0
if [ "${1:-}" = "--tiers" ]; then
  ALL_TIERS=1
fi

if [ ! -f "$DEST/base.js" ]; then
  echo "Downloading v8-v7 benchmark into $DEST ..." >&2
  mkdir -p "$DEST"
  for f in "${FILES[@]}"; do
    curl -fsSL "$RAW/$f" -o "$DEST/$f"
  done
fi

# The upstream driver uses the shell `load()`; the lumen CLI takes files in sequence instead.
sed '/^load(/d' "$DEST/run.js" > "$DEST/driver.js"

# node/bun need a single file plus a print() shim.
COMBINED="$DEST/combined.js"
{
  printf 'globalThis.print = (...a) => console.log(...a);\n'
  cat "$DEST/base.js"
  for s in "${SUITES[@]}"; do cat "$DEST/$s.js"; done
  cat "$DEST/driver.js"
} > "$COMBINED"

echo "Building lumen (release) ..." >&2
cargo build --release -q -p lumen --bin lumen

LUMEN_ARGS=("$DEST/base.js")
for s in "${SUITES[@]}"; do LUMEN_ARGS+=("$DEST/$s.js"); done
LUMEN_ARGS+=("$DEST/driver.js")

# run_suite <output-file> <cmd...>: run one engine, store "Name: score" lines.
run_suite() {
  local out="$1"; shift
  "$@" | sed 's/^Score (version 7)/Score/' | grep -E '^[A-Za-z]+: [0-9]+$' > "$out"
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

COLS=()   # column headers
OUTS=()   # per-column result files

if command -v node >/dev/null; then
  echo "Running node ..." >&2
  run_suite "$TMP/node.txt" node "$COMBINED"
  COLS+=("Node $(node --version)"); OUTS+=("$TMP/node.txt")
else
  echo "warning: node not found, skipping" >&2
fi

if command -v bun >/dev/null; then
  echo "Running bun ..." >&2
  run_suite "$TMP/bun.txt" bun "$COMBINED"
  COLS+=("Bun $(bun --version)"); OUTS+=("$TMP/bun.txt")
else
  echo "warning: bun not found, skipping" >&2
fi

echo "Running lumen (jit) ..." >&2
run_suite "$TMP/lumen-jit.txt" "$ROOT/target/release/lumen" --tier=jit "${LUMEN_ARGS[@]}"
COLS+=("Lumen (jit)"); OUTS+=("$TMP/lumen-jit.txt")

if [ "$ALL_TIERS" = 1 ]; then
  echo "Running lumen (bytecode) ..." >&2
  run_suite "$TMP/lumen-bc.txt" "$ROOT/target/release/lumen" --tier=bytecode "${LUMEN_ARGS[@]}"
  COLS+=("Lumen (bytecode)"); OUTS+=("$TMP/lumen-bc.txt")
  echo "Running lumen (interp) ..." >&2
  run_suite "$TMP/lumen-interp.txt" "$ROOT/target/release/lumen" --tier=interp "${LUMEN_ARGS[@]}"
  COLS+=("Lumen (interp)"); OUTS+=("$TMP/lumen-interp.txt")
fi

# score <file> <name>: extract one benchmark's score.
score() { grep "^$2: " "$1" | head -1 | cut -d' ' -f2; }

printf '\n| Benchmark |'
for c in "${COLS[@]}"; do printf ' %s |' "$c"; done
printf '\n|---|'
for _ in "${COLS[@]}"; do printf '%s' '---:|'; done
printf '\n'
for name in "${BENCH_NAMES[@]}"; do
  label="$name"
  [ "$name" = "Score" ] && label="**Composite**"
  printf '| %s |' "$label"
  for out in "${OUTS[@]}"; do
    v="$(score "$out" "$name")"
    if [ "$name" = "Score" ]; then printf ' **%s** |' "${v:-—}"; else printf ' %s |' "${v:-—}"; fi
  done
  printf '\n'
done
