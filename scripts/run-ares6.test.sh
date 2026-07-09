#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
SCRIPT="$ROOT/scripts/run-ares6.sh"
TMP_ROOT="target/run-ares6-tests"
TMP="$TMP_ROOT/$$"
FAKE_ARES="$TMP/ARES-6"
FAKE_BIN="$TMP/bin"
CARGO_LOG="$TMP/cargo.log"
LUMEN_LOG="$TMP/lumen.args"
ENTRY_CAPTURE="$TMP/ares6-entry.cjs"
STDOUT_LOG="$TMP/stdout.log"
STDERR_LOG="$TMP/stderr.log"
LUMEN_STUB="$FAKE_BIN/lumen-cli-stub"

fail() {
  echo "not ok - $*" >&2
  exit 1
}

assert_contains() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq "$needle" "$file"; then
    echo "expected to find: $needle" >&2
    echo "in: $file" >&2
    echo "--- file contents ---" >&2
    cat "$file" >&2
    fail "missing expected text"
  fi
}

assert_not_contains() {
  local file="$1"
  local needle="$2"
  if grep -Fq "$needle" "$file"; then
    echo "expected not to find: $needle" >&2
    echo "in: $file" >&2
    echo "--- file contents ---" >&2
    cat "$file" >&2
    fail "unexpected text was present"
  fi
}

assert_file_equals() {
  local file="$1"
  local expected="$2"
  local actual
  actual="$(cat "$file")"
  if [ "$actual" != "$expected" ]; then
    echo "expected exact file contents: $expected" >&2
    echo "actual file contents:" >&2
    cat "$file" >&2
    fail "file contents did not match exactly"
  fi
}

assert_before() {
  local file="$1"
  local first="$2"
  local second="$3"
  local first_line second_line
  first_line="$(grep -nF "$first" "$file" | head -n 1 | cut -d: -f1)"
  second_line="$(grep -nF "$second" "$file" | head -n 1 | cut -d: -f1)"
  if [ -z "$first_line" ] || [ -z "$second_line" ]; then
    echo "missing ordering marker(s): $first / $second" >&2
    cat "$file" >&2
    fail "missing ordering marker"
  fi
  if [ "$first_line" -ge "$second_line" ]; then
    echo "expected '$first' before '$second'" >&2
    cat "$file" >&2
    fail "markers were out of order"
  fi
}

assert_single_entry_arg() {
  if [ ! -f "$LUMEN_LOG" ]; then
    fail "lumen-cli stub was not invoked"
  fi
  local total entry
  total="$(wc -l < "$LUMEN_LOG" | tr -d ' ')"
  if [ "$total" != "1" ]; then
    cat "$LUMEN_LOG" >&2
    fail "expected lumen-cli to receive exactly one entry argument"
  fi
  entry="$(cat "$LUMEN_LOG")"
  case "$entry" in
    *.cjs) ;;
    *) fail "generated entry did not have .cjs suffix: $entry" ;;
  esac
  if [ ! -f "$ENTRY_CAPTURE" ]; then
    fail "lumen-cli stub did not capture the generated entry"
  fi
}

cleanup() {
  rm -rf "$TMP"
}

setup_stubs() {
  mkdir -p "$TMP_ROOT" "$FAKE_BIN"

  cat > "$LUMEN_STUB" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$LUMEN_LOG"
entry="${1:-}"
if [ -n "${LUMEN_ENTRY_CAPTURE:-}" ] && [ -f "$entry" ]; then
  cp "$entry" "$LUMEN_ENTRY_CAPTURE"
fi

if [ -n "${LUMEN_STUB_OUTPUT:-}" ]; then
  printf '%s\n' "$LUMEN_STUB_OUTPUT"
else
  cat <<'OUT'
ARES-6 1.0.1
Running... Air ( 6  to go)
Running... Basic ( 6  to go)
Running... Babylon ( 6  to go)
Running... ML ( 6  to go)
summary:            12.34 +- 0.56 ms
Success! Benchmark is now finished.
OUT
fi

exit "${LUMEN_STUB_STATUS:-0}"
SH
  chmod +x "$LUMEN_STUB"

  cat > "$FAKE_BIN/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CARGO_LOG"
build_root="${CARGO_TARGET_DIR:?CARGO_TARGET_DIR must be set by tests}"
mkdir -p "$build_root/release"
cat > "$build_root/release/lumen-cli" <<'STUB'
#!/usr/bin/env bash
echo "stale extensionless lumen-cli stub should not run" >&2
exit 91
STUB
cp "$LUMEN_STUB_TEMPLATE" "$build_root/release/lumen-cli.exe"
chmod +x "$build_root/release/lumen-cli" "$build_root/release/lumen-cli.exe"
exit 0
SH
  chmod +x "$FAKE_BIN/cargo"

  export CARGO_LOG
  export LUMEN_LOG
  export LUMEN_ENTRY_CAPTURE="$ENTRY_CAPTURE"
  export LUMEN_STUB_TEMPLATE="$LUMEN_STUB"
  export PATH="$FAKE_BIN:$PATH"
}

# The full-suite source set: the 8 top-level files plus every nested payload each
# *_benchmark.js lists in its `!isInBrowser` sources array. Mirrors the array in
# scripts/run-ares6.sh; keep the two in sync.
REQUIRED_FILES=(
  driver.js results.js stats.js glue.js
  air_benchmark.js basic_benchmark.js babylon_benchmark.js ml_benchmark.js
  Air/symbols.js Air/tmp_base.js Air/arg.js Air/basic_block.js Air/code.js
  Air/frequented_block.js Air/inst.js Air/opcode.js Air/reg.js Air/stack_slot.js
  Air/tmp.js Air/util.js Air/custom.js Air/liveness.js Air/insertion_set.js
  Air/allocate_stack.js Air/payload-gbemu-executeIteration.js
  Air/payload-imaging-gaussian-blur-gaussianBlur.js Air/payload-airjs-ACLj8C.js
  Air/payload-typescript-scanIdentifier.js Air/benchmark.js
  Basic/ast.js Basic/basic.js Basic/caseless_map.js Basic/lexer.js Basic/number.js
  Basic/parser.js Basic/random.js Basic/state.js Basic/util.js Basic/benchmark.js
  Babylon/index.js Babylon/benchmark.js
  ml/index.js ml/benchmark.js
)

# Materialize a complete fake ARES-6 checkout. Empty files are enough because the
# fake lumen-cli stub captures the generated entry and emits canned benchmark
# output without executing the fake JavaScript.
write_fake_ares() {
  rm -rf "$FAKE_ARES"
  local rel
  for rel in "${REQUIRED_FILES[@]}"; do
    mkdir -p "$FAKE_ARES/$(dirname "$rel")"
    printf '// %s\n' "$rel" > "$FAKE_ARES/$rel"
  done
  cat > "$FAKE_ARES/glue.js" <<'GLUE'
// glue.js
driver.addBenchmark(AirBenchmarkRunner);
driver.addBenchmark(BasicBenchmarkRunner);
driver.addBenchmark(BabylonBenchmarkRunner);
driver.addBenchmark(MLBenchmarkRunner);
driver.readyTrigger();
GLUE
}

air_basic_output() {
  cat <<'OUT'
ARES-6 1.0.1
Running... Air ( 6  to go)
Running... Basic ( 6  to go)
summary:            12.34 +- 0.56 ms
Success! Benchmark is now finished.
OUT
}

air_basic_with_unselected_ml_output() {
  cat <<'OUT'
ARES-6 1.0.1
Running... Air ( 6  to go)
Running... Basic ( 6  to go)
Running... ML ( 6  to go)
summary:            12.34 +- 0.56 ms
Success! Benchmark is now finished.
OUT
}

run_expect_failure() {
  local status=0
  "$@" > "$STDOUT_LOG" 2> "$STDERR_LOG" || status=$?
  if [ "$status" -eq 0 ]; then
    fail "command unexpectedly succeeded: $*"
  fi
}

run_expect_success() {
  local status=0
  "$@" > "$STDOUT_LOG" 2> "$STDERR_LOG" || status=$?
  if [ "$status" -ne 0 ]; then
    echo "--- stdout ---" >&2
    cat "$STDOUT_LOG" >&2
    echo "--- stderr ---" >&2
    cat "$STDERR_LOG" >&2
    fail "command unexpectedly failed ($status): $*"
  fi
}

test_missing_checkout_fails_before_build() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  run_expect_failure env -u LUMEN_BIN ARES6="$TMP/missing-ares" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: ARES-6 checkout not found at: $TMP/missing-ares"
  assert_contains "$STDERR_LOG" "ARES6=/path/to/ARES-6"
  assert_contains "$STDERR_LOG" "../ARES-6"
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when the ARES-6 checkout is missing"
  fi
  if [ -f "$LUMEN_LOG" ]; then
    fail "lumen-cli must not run when the ARES-6 checkout is missing"
  fi
}

test_unknown_workload_fails_before_checkout_or_build() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"

  run_expect_failure env -u LUMEN_BIN ARES6="$TMP/missing-ares" "$SCRIPT" air wat

  assert_contains "$STDERR_LOG" "error: unknown ARES-6 workload: wat"
  assert_contains "$STDERR_LOG" "valid workloads: air basic babylon ml"
  assert_not_contains "$STDERR_LOG" "checkout not found"
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when workload selection is invalid"
  fi
  if [ -f "$LUMEN_LOG" ]; then
    fail "lumen-cli must not run when workload selection is invalid"
  fi
  if [ -f "$ENTRY_CAPTURE" ]; then
    fail "entry must not be generated when workload selection is invalid"
  fi
}

test_missing_required_file_fails_before_build() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  rm -f "$FAKE_ARES/Air/payload-airjs-ACLj8C.js"
  run_expect_failure env -u LUMEN_BIN ARES6="$FAKE_ARES" "$SCRIPT"

  assert_contains "$STDERR_LOG" "missing required source file"
  assert_contains "$STDERR_LOG" "missing: Air/payload-airjs-ACLj8C.js"
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when a required ARES-6 source file is missing"
  fi
  if [ -f "$LUMEN_LOG" ]; then
    fail "lumen-cli must not run when a required ARES-6 source file is missing"
  fi
}

test_selected_run_ignores_unselected_missing_payload_files() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  rm -f "$FAKE_ARES/Babylon/index.js"
  rm -f "$FAKE_ARES/ml/benchmark.js"
  local output
  output="$(air_basic_output)"

  run_expect_success env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT="$output" "$SCRIPT" air basic

  assert_contains "$STDOUT_LOG" "Running... Air"
  assert_contains "$STDOUT_LOG" "Running... Basic"
  assert_not_contains "$STDERR_LOG" "missing: Babylon/index.js"
  assert_not_contains "$STDERR_LOG" "missing: ml/benchmark.js"
  assert_single_entry_arg
  assert_not_contains "$ENTRY_CAPTURE" "driver.addBenchmark(BabylonBenchmarkRunner);"
  assert_not_contains "$ENTRY_CAPTURE" "driver.addBenchmark(MLBenchmarkRunner);"
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when LUMEN_BIN is provided"
  fi
}

test_selected_missing_required_file_fails_before_build() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  rm -f "$FAKE_ARES/Basic/parser.js"

  run_expect_failure env -u LUMEN_BIN ARES6="$FAKE_ARES" "$SCRIPT" basic

  assert_contains "$STDERR_LOG" "missing required source file"
  assert_contains "$STDERR_LOG" "missing: Basic/parser.js"
  assert_not_contains "$STDERR_LOG" "missing: Air/symbols.js"
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when a selected required ARES-6 source file is missing"
  fi
  if [ -f "$LUMEN_LOG" ]; then
    fail "lumen-cli must not run when a selected required ARES-6 source file is missing"
  fi
}

test_lumen_bin_skips_build_and_good_output_passes() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares

  run_expect_success env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" "$SCRIPT"

  assert_contains "$STDOUT_LOG" "ares-6: validated checkout at $FAKE_ARES"
  assert_contains "$STDOUT_LOG" "ARES-6 1.0.1"
  assert_contains "$STDOUT_LOG" "Running... Air"
  assert_contains "$STDOUT_LOG" "Running... Basic"
  assert_contains "$STDOUT_LOG" "Running... Babylon"
  assert_contains "$STDOUT_LOG" "Running... ML"
  assert_contains "$STDOUT_LOG" "summary:            12.34 +- 0.56 ms"
  assert_contains "$STDOUT_LOG" "Success! Benchmark is now finished."
  assert_single_entry_arg

  assert_contains "$ENTRY_CAPTURE" "var isInBrowser = false;"
  assert_contains "$ENTRY_CAPTURE" "function print(...args)"
  assert_contains "$ENTRY_CAPTURE" "function makeBenchmarkRunner(sources, name, count = 200)"
  assert_contains "$ENTRY_CAPTURE" "const readFileSync = require('node:fs').readFileSync;"
  assert_contains "$ENTRY_CAPTURE" "new Function(code).call(globalThis);"
  assert_contains "$ENTRY_CAPTURE" "globalThis.reportResult = reportResult;"
  assert_contains "$ENTRY_CAPTURE" "driver.start(6);"
  assert_before "$ENTRY_CAPTURE" "// ---- stats.js ----" "// ---- results.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- results.js ----" "// ---- driver.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- driver.js ----" "// ---- air_benchmark.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- air_benchmark.js ----" "// ---- basic_benchmark.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- basic_benchmark.js ----" "// ---- babylon_benchmark.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- babylon_benchmark.js ----" "// ---- ml_benchmark.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- ml_benchmark.js ----" "// ---- glue.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- glue.js ----" "driver.start(6);"

  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when LUMEN_BIN is provided"
  fi
}

test_selected_workloads_filter_generated_glue_wiring() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  local output
  output="$(air_basic_output)"

  run_expect_success env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT="$output" "$SCRIPT" air basic

  assert_contains "$STDOUT_LOG" "ares-6: validated checkout at $FAKE_ARES"
  assert_contains "$STDOUT_LOG" "ARES-6 1.0.1"
  assert_contains "$STDOUT_LOG" "Running... Air"
  assert_contains "$STDOUT_LOG" "Running... Basic"
  assert_not_contains "$STDOUT_LOG" "Running... Babylon"
  assert_not_contains "$STDOUT_LOG" "Running... ML"
  assert_contains "$STDOUT_LOG" "summary:            12.34 +- 0.56 ms"
  assert_contains "$STDOUT_LOG" "Success! Benchmark is now finished."
  assert_single_entry_arg

  assert_contains "$ENTRY_CAPTURE" "// ---- stats.js ----"
  assert_contains "$ENTRY_CAPTURE" "// ---- results.js ----"
  assert_contains "$ENTRY_CAPTURE" "// ---- driver.js ----"
  assert_contains "$ENTRY_CAPTURE" "// ---- air_benchmark.js ----"
  assert_contains "$ENTRY_CAPTURE" "// ---- basic_benchmark.js ----"
  assert_contains "$ENTRY_CAPTURE" "// ---- glue.js ----"
  assert_not_contains "$ENTRY_CAPTURE" "// ---- babylon_benchmark.js ----"
  assert_not_contains "$ENTRY_CAPTURE" "// ---- ml_benchmark.js ----"
  assert_contains "$ENTRY_CAPTURE" "driver.addBenchmark(AirBenchmarkRunner);"
  assert_contains "$ENTRY_CAPTURE" "driver.addBenchmark(BasicBenchmarkRunner);"
  assert_not_contains "$ENTRY_CAPTURE" "driver.addBenchmark(BabylonBenchmarkRunner);"
  assert_not_contains "$ENTRY_CAPTURE" "driver.addBenchmark(MLBenchmarkRunner);"
  assert_contains "$ENTRY_CAPTURE" "driver.readyTrigger();"
  assert_before "$ENTRY_CAPTURE" "// ---- driver.js ----" "// ---- air_benchmark.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- air_benchmark.js ----" "// ---- basic_benchmark.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- basic_benchmark.js ----" "// ---- glue.js ----"
  assert_before "$ENTRY_CAPTURE" "// ---- glue.js ----" "driver.start(6);"

  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when LUMEN_BIN is provided"
  fi
}

test_build_happens_when_lumen_bin_absent() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares

  run_expect_success env -u LUMEN_BIN ARES6="$FAKE_ARES" \
    CARGO_TARGET_DIR="$TMP/cargo-target" "$SCRIPT"

  if [ ! -f "$CARGO_LOG" ]; then
    fail "a valid checkout should reach the cargo build when LUMEN_BIN is absent"
  fi
  assert_file_equals "$CARGO_LOG" "build --release -p lumen-cli"
  assert_single_entry_arg
}

test_thrown_error_output_fails() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  local output
  output="$(cat <<'OUT'
ARES-6 1.0.1
Running... Air ( 6  to go)
ReferenceError: currentTime is not defined
OUT
)"

  run_expect_failure env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT="$output" "$SCRIPT"

  assert_contains "$STDOUT_LOG" "ReferenceError: currentTime is not defined"
  assert_contains "$STDERR_LOG" "error: ARES-6 did not report completion."
}

test_missing_success_fails() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  local output
  output="$(cat <<'OUT'
ARES-6 1.0.1
Running... Air ( 6  to go)
Running... Basic ( 6  to go)
Running... Babylon ( 6  to go)
Running... ML ( 6  to go)
summary:            12.34 +- 0.56 ms
OUT
)"

  run_expect_failure env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT="$output" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: ARES-6 did not report completion."
}

test_missing_numeric_geomean_fails() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  local output
  output="$(cat <<'OUT'
ARES-6 1.0.1
Running... Air ( 6  to go)
Running... Basic ( 6  to go)
Running... Babylon ( 6  to go)
Running... ML ( 6  to go)
summary:            ERROR
Success! Benchmark is now finished.
OUT
)"

  run_expect_failure env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT="$output" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: ARES-6 did not report a numeric summary geomean."
}

test_missing_workload_fails() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  local output
  output="$(cat <<'OUT'
ARES-6 1.0.1
Running... Air ( 6  to go)
Running... Basic ( 6  to go)
Running... Babylon ( 6  to go)
summary:            12.34 +- 0.56 ms
Success! Benchmark is now finished.
OUT
)"

  run_expect_failure env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT="$output" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: ARES-6 did not run selected workload: ML"
}

test_selected_sentinel_rejects_unselected_workload_output() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares
  local output
  output="$(air_basic_with_unselected_ml_output)"

  run_expect_failure env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT="$output" "$SCRIPT" air basic

  assert_contains "$STDOUT_LOG" "Running... Air"
  assert_contains "$STDOUT_LOG" "Running... Basic"
  assert_contains "$STDOUT_LOG" "Running... ML"
  assert_contains "$STDERR_LOG" "error: ARES-6 unexpectedly ran unselected workload: ML"
}

test_nonzero_engine_status_fails_even_with_good_sentinel() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$ENTRY_CAPTURE"
  write_fake_ares

  run_expect_failure env ARES6="$FAKE_ARES" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_STATUS=7 "$SCRIPT"

  assert_contains "$STDOUT_LOG" "Success! Benchmark is now finished."
}

trap cleanup EXIT
setup_stubs

test_missing_checkout_fails_before_build
test_unknown_workload_fails_before_checkout_or_build
test_missing_required_file_fails_before_build
test_selected_run_ignores_unselected_missing_payload_files
test_selected_missing_required_file_fails_before_build
test_lumen_bin_skips_build_and_good_output_passes
test_selected_workloads_filter_generated_glue_wiring
test_build_happens_when_lumen_bin_absent
test_thrown_error_output_fails
test_missing_success_fails
test_missing_numeric_geomean_fails
test_missing_workload_fails
test_selected_sentinel_rejects_unselected_workload_output
test_nonzero_engine_status_fails_even_with_good_sentinel

echo "ok - run-ares6"
