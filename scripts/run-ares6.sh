#!/usr/bin/env bash
#
# Validate an ARES-6 checkout (BrowserBench) before building the lumen CLI.
#
# Resolution order (the local agent mirror is intentionally never used):
#   1. $ARES6            explicit override
#   2. $ROOT/../ARES-6   sibling checkout fallback
#
#   scripts/run-ares6.sh                                  # run all four workloads
#   scripts/run-ares6.sh air basic                        # run only Air and Basic
#   ARES6=/path/to/ARES-6 scripts/run-ares6.sh babylon ml # validate/run selected workloads
#   LUMEN_BIN=/path/to/lumen-cli scripts/run-ares6.sh     # skip the cargo build
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARES6_DIR="${ARES6:-$ROOT/../ARES-6}"

KNOWN_WORKLOAD_KEYS=(air basic babylon ml)
SELECTED_WORKLOAD_KEYS=()

workload_key_selected() {
  local candidate="$1"
  local key
  for key in "${SELECTED_WORKLOAD_KEYS[@]}"; do
    [ "$key" = "$candidate" ] && return 0
  done
  return 1
}

workload_display_name() {
  case "$1" in
    air) printf 'Air\n' ;;
    basic) printf 'Basic\n' ;;
    babylon) printf 'Babylon\n' ;;
    ml) printf 'ML\n' ;;
    *) return 1 ;;
  esac
}

workload_benchmark_file() {
  case "$1" in
    air) printf 'air_benchmark.js\n' ;;
    basic) printf 'basic_benchmark.js\n' ;;
    babylon) printf 'babylon_benchmark.js\n' ;;
    ml) printf 'ml_benchmark.js\n' ;;
    *) return 1 ;;
  esac
}

workload_add_benchmark_line() {
  case "$1" in
    air) printf 'driver.addBenchmark(AirBenchmarkRunner);\n' ;;
    basic) printf 'driver.addBenchmark(BasicBenchmarkRunner);\n' ;;
    babylon) printf 'driver.addBenchmark(BabylonBenchmarkRunner);\n' ;;
    ml) printf 'driver.addBenchmark(MLBenchmarkRunner);\n' ;;
    *) return 1 ;;
  esac
}

parse_selected_workloads() {
  local arg
  if [ "$#" -eq 0 ]; then
    SELECTED_WORKLOAD_KEYS=("${KNOWN_WORKLOAD_KEYS[@]}")
    return
  fi

  for arg in "$@"; do
    case "$arg" in
      air|basic|babylon|ml)
        if workload_key_selected "$arg"; then
          echo "error: duplicate ARES-6 workload: $arg" >&2
          echo "valid workloads: air basic babylon ml" >&2
          exit 1
        fi
        SELECTED_WORKLOAD_KEYS+=("$arg")
        ;;
      *)
        echo "error: unknown ARES-6 workload: $arg" >&2
        echo "valid workloads: air basic babylon ml" >&2
        exit 1
        ;;
    esac
  done
}

parse_selected_workloads "$@"

# Common harness files needed for every selected workload. Workload-specific
# files are appended from the selected friendly names below.
REQUIRED_FILES=(
  driver.js
  results.js
  stats.js
  glue.js
)

append_workload_required_files() {
  case "$1" in
    air)
      REQUIRED_FILES+=(
        air_benchmark.js
        Air/symbols.js
        Air/tmp_base.js
        Air/arg.js
        Air/basic_block.js
        Air/code.js
        Air/frequented_block.js
        Air/inst.js
        Air/opcode.js
        Air/reg.js
        Air/stack_slot.js
        Air/tmp.js
        Air/util.js
        Air/custom.js
        Air/liveness.js
        Air/insertion_set.js
        Air/allocate_stack.js
        Air/payload-gbemu-executeIteration.js
        Air/payload-imaging-gaussian-blur-gaussianBlur.js
        Air/payload-airjs-ACLj8C.js
        Air/payload-typescript-scanIdentifier.js
        Air/benchmark.js
      )
      ;;
    basic)
      REQUIRED_FILES+=(
        basic_benchmark.js
        Basic/ast.js
        Basic/basic.js
        Basic/caseless_map.js
        Basic/lexer.js
        Basic/number.js
        Basic/parser.js
        Basic/random.js
        Basic/state.js
        Basic/util.js
        Basic/benchmark.js
      )
      ;;
    babylon)
      REQUIRED_FILES+=(
        babylon_benchmark.js
        Babylon/index.js
        Babylon/benchmark.js
      )
      ;;
    ml)
      REQUIRED_FILES+=(
        ml_benchmark.js
        ml/index.js
        ml/benchmark.js
      )
      ;;
    *) return 1 ;;
  esac
}

for key in "${SELECTED_WORKLOAD_KEYS[@]}"; do
  append_workload_required_files "$key"
done

if [ ! -d "$ARES6_DIR" ]; then
  echo "error: ARES-6 checkout not found at: $ARES6_DIR" >&2
  echo "  - override the location with ARES6=/path/to/ARES-6" >&2
  echo "  - or place a checkout at the sibling path ../ARES-6 (i.e. $ROOT/../ARES-6)" >&2
  exit 1
fi

missing=()
for rel in "${REQUIRED_FILES[@]}"; do
  [ -f "$ARES6_DIR/$rel" ] || missing+=("$rel")
done
if [ "${#missing[@]}" -gt 0 ]; then
  echo "error: ARES-6 checkout at $ARES6_DIR is missing required source file(s):" >&2
  for rel in "${missing[@]}"; do
    echo "  missing: $rel" >&2
  done
  exit 1
fi

validate_selected_glue_wiring() {
  local key line
  for key in "${SELECTED_WORKLOAD_KEYS[@]}"; do
    line="$(workload_add_benchmark_line "$key")"
    if ! grep -Fxq "$line" "$ARES6_DIR/glue.js"; then
      echo "error: ARES-6 glue.js is missing expected workload wiring: $line" >&2
      exit 1
    fi
  done
}

validate_selected_glue_wiring

echo "ares-6: validated checkout at $ARES6_DIR (${#REQUIRED_FILES[@]} source files present)"

if [ -z "${LUMEN_BIN:-}" ]; then
  cargo build --release -p lumen-cli
  target_dir="${CARGO_TARGET_DIR:-$ROOT/target}"
  LUMEN_BIN="$target_dir/release/lumen-cli"
  if [ -x "$LUMEN_BIN.exe" ]; then
    LUMEN_BIN="$LUMEN_BIN.exe"
  fi
fi

if [ ! -x "$LUMEN_BIN" ]; then
  echo "error: lumen-cli binary is not executable: $LUMEN_BIN" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ENTRY="$WORK/ares6-entry.cjs"
OUTPUT="$WORK/ares6-output.log"

to_js_path() {
  local path="$1"
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -m "$path"
  else
    printf '%s\n' "$path"
  fi
}

write_selected_glue() {
  local line
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      "driver.addBenchmark(AirBenchmarkRunner);")
        if workload_key_selected air; then
          printf '%s\n' "$line"
        fi
        ;;
      "driver.addBenchmark(BasicBenchmarkRunner);")
        if workload_key_selected basic; then
          printf '%s\n' "$line"
        fi
        ;;
      "driver.addBenchmark(BabylonBenchmarkRunner);")
        if workload_key_selected babylon; then
          printf '%s\n' "$line"
        fi
        ;;
      "driver.addBenchmark(MLBenchmarkRunner);")
        if workload_key_selected ml; then
          printf '%s\n' "$line"
        fi
        ;;
      *)
        printf '%s\n' "$line"
        ;;
    esac
  done < "$ARES6_DIR/glue.js"
}

ARES6_JS_DIR="$(to_js_path "$ARES6_DIR")"

{
  cat <<'JS'
var isInBrowser = false;

function print(...args) {
  console.log(...args);
}

const readFileSync = require('node:fs').readFileSync;
const ares6Root = process.env.ARES6_ROOT;

if (!ares6Root) {
  throw new Error("ARES6_ROOT environment variable is required");
}

const ares6Prefix = /[\\/]$/.test(ares6Root) ? ares6Root : `${ares6Root}/`;

function makeBenchmarkRunner(sources, name, count = 200) {
  return function runBenchmark() {
    let code = "";
    for (const source of sources) {
      code += readFileSync(ares6Prefix + source, "utf8");
      code += "\n";
    }
    code += `
var results = [];
var benchmark = new ${name}();
var numIterations = ${count};
for (var i = 0; i < numIterations; ++i) {
    var before = currentTime();
    benchmark.runIteration();
    var after = currentTime();
    results.push(after - before);
}
reportResult(results);
`;
    new Function(code).call(globalThis);
  };
}
JS

  for rel in stats.js results.js driver.js; do
    printf '\n// ---- %s ----\n' "$rel"
    cat "$ARES6_DIR/$rel"
  done

  for key in "${SELECTED_WORKLOAD_KEYS[@]}"; do
    rel="$(workload_benchmark_file "$key")"
    printf '\n// ---- %s ----\n' "$rel"
    cat "$ARES6_DIR/$rel"
  done

  printf '\n// ---- %s ----\n' "glue.js"
  write_selected_glue

  cat <<'JS'
globalThis.reportResult = reportResult;
driver.start(6);
JS
} > "$ENTRY"

set +e
ARES6_ROOT="$ARES6_JS_DIR" "$LUMEN_BIN" "$ENTRY" 2>&1 | tee "$OUTPUT"
status=${PIPESTATUS[0]}
set -e

sentinel_status=0

if ! grep -Fxq "ARES-6 1.0.1" "$OUTPUT"; then
  echo "error: ARES-6 did not report benchmark title." >&2
  sentinel_status=1
fi

for key in "${SELECTED_WORKLOAD_KEYS[@]}"; do
  name="$(workload_display_name "$key")"
  if ! grep -Eq "^Running[.][.][.] ${name}([[:space:]]|$)" "$OUTPUT"; then
    echo "error: ARES-6 did not run selected workload: $name" >&2
    sentinel_status=1
  fi
done

for key in "${KNOWN_WORKLOAD_KEYS[@]}"; do
  if ! workload_key_selected "$key"; then
    name="$(workload_display_name "$key")"
    if grep -Eq "^Running[.][.][.] ${name}([[:space:]]|$)" "$OUTPUT"; then
      echo "error: ARES-6 unexpectedly ran unselected workload: $name" >&2
      sentinel_status=1
    fi
  fi
done

if ! grep -Eq '^summary:[[:space:]]+[0-9]+([.][0-9]+)?([[:space:]]+\+-[[:space:]]+[0-9]+([.][0-9]+)?)?[[:space:]]+ms$' "$OUTPUT"; then
  echo "error: ARES-6 did not report a numeric summary geomean." >&2
  sentinel_status=1
fi

if ! grep -Fxq "Success! Benchmark is now finished." "$OUTPUT"; then
  echo "error: ARES-6 did not report completion." >&2
  sentinel_status=1
fi

if [ "$sentinel_status" -ne 0 ]; then
  status=1
fi

exit "$status"
