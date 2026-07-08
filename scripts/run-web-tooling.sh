#!/usr/bin/env bash
#
# Run V8's Web Tooling Benchmark (babel, terser, acorn, etc. - real-world
# JS tooling workloads) on the lumen engine, via the benchmark's pre-built
# dist/cli.js bundle. Expects the benchmark to already exist as a sibling
# ../web-tooling-benchmark checkout with `npm install` run (which produces
# dist/cli.js), or set WEB_TOOLING_BENCHMARK_DIR to the checkout directory.
# Builds the `lumen` CLI in release mode (unless LUMEN_BIN points at a
# prebuilt binary), then runs the bundle. `--only <name>` is implemented by
# rebuilding the upstream bundle with webpack's build-time selector; it is not
# forwarded to lumen or to dist/cli.js.
#
#   scripts/run-web-tooling.sh                     # full suite
#   scripts/run-web-tooling.sh --only babel        # selected benchmark
#   WEB_TOOLING_BENCHMARK_DIR=/path/to/web-tooling-benchmark scripts/run-web-tooling.sh --only terser
#   LUMEN_BIN=/path/to/lumen scripts/run-web-tooling.sh --only acorn   # skip the build, use this binary
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEB_TOOLING_BENCHMARK_DIR="${WEB_TOOLING_BENCHMARK_DIR:-$ROOT/../web-tooling-benchmark}"
ONLY=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --only)
      if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        echo "error: --only requires a benchmark name." >&2
        exit 2
      fi
      ONLY="$2"
      shift 2
      ;;
    --only=*)
      ONLY="${1#--only=}"
      if [ -z "$ONLY" ]; then
        echo "error: --only requires a benchmark name." >&2
        exit 2
      fi
      shift
      ;;
    *)
      echo "error: unsupported argument: $1" >&2
      echo "Web Tooling Benchmark selection is baked into dist/cli.js; use --only <benchmark>." >&2
      exit 2
      ;;
  esac
done

if [ ! -d "$WEB_TOOLING_BENCHMARK_DIR" ]; then
  echo "error: Web Tooling Benchmark not found at $WEB_TOOLING_BENCHMARK_DIR." >&2
  echo "Clone it and build the CLI bundle:" >&2
  echo "  git clone https://github.com/v8/web-tooling-benchmark \"$WEB_TOOLING_BENCHMARK_DIR\"" >&2
  echo "  cd \"$WEB_TOOLING_BENCHMARK_DIR\" && npm install" >&2
  exit 1
fi

if [ -n "$ONLY" ]; then
  echo "Building Web Tooling Benchmark bundle for: $ONLY"
  (cd "$WEB_TOOLING_BENCHMARK_DIR" && npx webpack "--env.only=$ONLY")
fi

if [ ! -f "$WEB_TOOLING_BENCHMARK_DIR/dist/cli.js" ]; then
  echo "error: Web Tooling Benchmark bundle not found at $WEB_TOOLING_BENCHMARK_DIR/dist/cli.js." >&2
  echo "Build it:" >&2
  echo "  cd \"$WEB_TOOLING_BENCHMARK_DIR\" && npm install" >&2
  exit 1
fi

if [ -z "${LUMEN_BIN:-}" ]; then
  cargo build --release -q -p lumen --bin lumen
  LUMEN_BIN="$ROOT/target/release/lumen"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
OUTPUT="$WORK/web-tooling-output"

set +e
"$LUMEN_BIN" "$WEB_TOOLING_BENCHMARK_DIR/dist/cli.js" "$@" 2>&1 | tee "$OUTPUT"
status=${PIPESTATUS[0]}
set -e

if grep -Eq '^[[:space:]]*[[:alpha:].]*Error: ' "$OUTPUT" || grep -Eq '^[[:space:]]*at .+:[0-9]+:[0-9]+\)?[[:space:]]*$' "$OUTPUT"; then
  echo "error: Web Tooling Benchmark reported a failure." >&2
  status=1
fi

if ! grep -Fq 'Geometric mean:' "$OUTPUT"; then
  echo "error: Web Tooling Benchmark did not report a Geometric mean success marker." >&2
  status=1
fi

exit "$status"
