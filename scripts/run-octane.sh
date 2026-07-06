#!/usr/bin/env bash
#
# Run the Octane benchmark suite (chromium/octane) on the lumen engine.
# Expects Octane to already exist as a sibling ../octane checkout, or set OCTANE
# to the directory containing base.js and run.js. Builds the `lumen` CLI in
# release mode (unless LUMEN_BIN points at a prebuilt binary), then prints
# per-benchmark scores plus the composite score. Higher is better.
#
#   scripts/run-octane.sh                    # full suite
#   scripts/run-octane.sh richards crypto    # selected benchmarks
#   OCTANE=/path/to/octane scripts/run-octane.sh gbemu
#   LUMEN_BIN=/path/to/lumen scripts/run-octane.sh   # skip the build, use this binary
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OCTANE="${OCTANE:-$ROOT/../octane}"

if [ ! -d "$OCTANE" ] || [ ! -f "$OCTANE/base.js" ] || [ ! -f "$OCTANE/run.js" ]; then
  echo "error: Octane not found at $OCTANE (need base.js and run.js)." >&2
  echo "Set \$OCTANE, or clone it as a sibling checkout:" >&2
  echo "  git clone https://github.com/chromium/octane \"$OCTANE\"" >&2
  exit 1
fi

if [ $# -ge 1 ]; then
  SUITES=("$@")
else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes \
          pdfjs mandreel gbemu code-load box2d zlib typescript)
fi

# Expand and validate the requested suites into a file list *before* building, so
# a typo fails in milliseconds instead of after a multi-minute release build.
SUITE_FILES=()
for s in "${SUITES[@]}"; do
  case "${s%.js}" in   # tolerate a trailing `.js` (e.g. from tab-completion)
    gbemu) files=(gbemu-part1.js gbemu-part2.js) ;;
    zlib) files=(zlib.js zlib-data.js) ;;
    typescript) files=(typescript.js typescript-input.js typescript-compiler.js) ;;
    *) files=("${s%.js}.js") ;;
  esac
  for f in "${files[@]}"; do
    if [ ! -f "$OCTANE/$f" ]; then
      echo "error: unknown/missing Octane suite file: $OCTANE/$f (from suite '$s')" >&2
      exit 1
    fi
    SUITE_FILES+=("$OCTANE/$f")
  done
done

# Build the CLI in release mode unless the caller supplied a prebuilt binary.
if [ -z "${LUMEN_BIN:-}" ]; then
  cargo build --release -q -p lumen --bin lumen
  LUMEN_BIN="$ROOT/target/release/lumen"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
DRIVER="$WORK/octane-driver.js"
OUTPUT="$WORK/octane-output"

# The upstream driver uses the shell `load()`; the lumen CLI takes files in sequence instead.
sed '/^load(/d' "$OCTANE/run.js" > "$DRIVER"

ARGS=("$OCTANE/base.js" "${SUITE_FILES[@]}" "$DRIVER")

set +e
"$LUMEN_BIN" "${ARGS[@]}" 2>&1 | tee "$OUTPUT"
status=${PIPESTATUS[0]}
set -e

if grep -Eq '^[[:alnum:]_-]+: [[:alpha:]]*Error:' "$OUTPUT"; then
  echo "error: Octane reported benchmark failure." >&2
  status=1
fi

if [ "$status" -eq 0 ] && ! grep -Eq '^Score \(version [0-9]+\): ' "$OUTPUT"; then
  echo "error: Octane completed without reporting a score." >&2
  status=1
fi

exit "$status"
