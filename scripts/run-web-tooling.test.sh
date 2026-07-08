#!/usr/bin/env bash
set -euo pipefail

if [ -d /usr/bin ]; then
  PATH="/usr/bin:/bin:$PATH"
  export PATH
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
SCRIPT="$ROOT/scripts/run-web-tooling.sh"
TMP_ROOT="target/run-web-tooling-tests"
TMP="$TMP_ROOT/$$"
FAKE_WTB="$TMP/web-tooling-benchmark"
FAKE_BIN="$TMP/bin"
CARGO_LOG="$TMP/cargo.log"
LUMEN_LOG="$TMP/lumen.args"
STDERR_LOG="$TMP/stderr.log"
# The lumen stub lives in the test's own tmp dir and is passed via LUMEN_BIN, so
# the runner never builds or touches the real target/release/lumen artifact.
LUMEN_STUB="$FAKE_BIN/lumen"

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
    echo "did not expect to find: $needle" >&2
    echo "in: $file" >&2
    echo "--- file contents ---" >&2
    cat "$file" >&2
    fail "unexpected text present"
  fi
}

assert_file_equals() {
  local file="$1"
  local expected="$2"
  local actual
  actual="$(cat "$file")"
  if [ "$actual" != "$expected" ]; then
    echo "--- expected ---" >&2
    printf '%s\n' "$expected" >&2
    echo "--- actual ---" >&2
    printf '%s\n' "$actual" >&2
    fail "$file did not match expected contents"
  fi
}

assert_no_cargo_log() {
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo should not run for this test"
  fi
}

test_scripts_are_executable() {
  if [ ! -x "$SCRIPT" ]; then
    fail "$SCRIPT should be executable"
  fi

  if [ ! -x "$ROOT/scripts/run-web-tooling.test.sh" ]; then
    fail "$ROOT/scripts/run-web-tooling.test.sh should be executable"
  fi
}

cleanup() {
  rm -rf "$TMP"
}

reset_logs() {
  rm -f "$CARGO_LOG" "$LUMEN_LOG" "$TMP/stdout.log" "$STDERR_LOG"
}

setup_stubs() {
  mkdir -p "$TMP_ROOT" "$FAKE_BIN"

  cat > "$FAKE_BIN/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CARGO_LOG"
exit 0
SH
  chmod +x "$FAKE_BIN/cargo"

  cat > "$LUMEN_STUB" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$LUMEN_LOG"
if [ -n "${LUMEN_STUB_OUTPUT:-}" ]; then
  printf '%s\n' "$LUMEN_STUB_OUTPUT"
else
  printf '%s\n' 'babel: 1234.5 runs/s'
fi
exit "${LUMEN_STUB_EXIT:-0}"
SH
  chmod +x "$LUMEN_STUB"

  export CARGO_LOG LUMEN_LOG
  export PATH="$FAKE_BIN:$PATH"
}

write_fake_wtb() {
  rm -rf "$FAKE_WTB"
  mkdir -p "$FAKE_WTB/dist"
  printf '%s\n' 'fake web tooling cli bundle' > "$FAKE_WTB/dist/cli.js"
}

run_expect_failure() {
  local status=0
  reset_logs
  "$@" > "$TMP/stdout.log" 2> "$STDERR_LOG" || status=$?
  if [ "$status" -eq 0 ]; then
    fail "command unexpectedly succeeded: $*"
  fi
}

test_missing_checkout_fails_before_build() {
  run_expect_failure env WEB_TOOLING_BENCHMARK_DIR="$TMP/missing" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: Web Tooling Benchmark not found at $TMP/missing."
  assert_contains "$STDERR_LOG" "git clone https://github.com/v8/web-tooling-benchmark \"$TMP/missing\""
  assert_contains "$STDERR_LOG" "cd \"$TMP/missing\" && npm install"
  assert_no_cargo_log
}

test_missing_bundle_fails_before_build() {
  write_fake_wtb
  rm -f "$FAKE_WTB/dist/cli.js"

  run_expect_failure env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: Web Tooling Benchmark bundle not found at $FAKE_WTB/dist/cli.js."
  assert_contains "$STDERR_LOG" "cd \"$FAKE_WTB\" && npm install"
  assert_no_cargo_log
}

test_default_path_is_usable() {
  local default_repo default_wtb copied_script expected_root expected_wtb
  default_repo="$TMP/default-repo/lumen"
  default_wtb="$TMP/default-repo/web-tooling-benchmark"
  copied_script="$default_repo/scripts/run-web-tooling.sh"

  rm -rf "$TMP/default-repo"
  mkdir -p "$default_repo/scripts" "$default_wtb/dist"
  cp "$SCRIPT" "$copied_script"
  chmod +x "$copied_script"
  printf '%s\n' 'fake web tooling cli bundle' > "$default_wtb/dist/cli.js"
  expected_root="$(cd "$default_repo" && pwd)"
  expected_wtb="$expected_root/../web-tooling-benchmark"

  reset_logs
  env LUMEN_BIN="$LUMEN_STUB" "$copied_script" > "$TMP/stdout.log"

  assert_file_equals "$LUMEN_LOG" "$expected_wtb/dist/cli.js"
  assert_no_cargo_log
}

test_override_env_honored() {
  reset_logs
  write_fake_wtb

  env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" LUMEN_BIN="$LUMEN_STUB" "$SCRIPT" > "$TMP/stdout.log"

  assert_contains "$LUMEN_LOG" "$FAKE_WTB/dist/cli.js"
}

test_lumen_bin_skips_build() {
  reset_logs
  write_fake_wtb

  env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" LUMEN_BIN="$LUMEN_STUB" "$SCRIPT" > "$TMP/stdout.log"

  assert_no_cargo_log
}

test_args_forwarded_including_only_babel() {
  reset_logs
  write_fake_wtb

  env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" LUMEN_BIN="$LUMEN_STUB" \
    "$SCRIPT" --only babel --extra-flag value > "$TMP/stdout.log"

  assert_file_equals "$LUMEN_LOG" "$FAKE_WTB/dist/cli.js
--only
babel
--extra-flag
value"
}

test_exception_in_output_fails_even_on_zero_exit() {
  write_fake_wtb
  run_expect_failure env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT='TypeError: foo is not a function' "$SCRIPT"

  assert_contains "$TMP/stdout.log" "TypeError: foo is not a function"
  assert_contains "$STDERR_LOG" "error: Web Tooling Benchmark reported a failure."

  write_fake_wtb
  run_expect_failure env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT='    at Object.<anonymous> (/x/y.js:10:5)' "$SCRIPT"

  assert_contains "$TMP/stdout.log" "    at Object.<anonymous> (/x/y.js:10:5)"
  assert_contains "$STDERR_LOG" "error: Web Tooling Benchmark reported a failure."
}

test_benign_error_word_in_output_succeeds() {
  reset_logs
  write_fake_wtb

  env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT='reported 0 Error: warnings' "$SCRIPT" > "$TMP/stdout.log" 2> "$STDERR_LOG" \
    || fail "benign Error: output should not fail"

  assert_contains "$TMP/stdout.log" "reported 0 Error: warnings"
  assert_not_contains "$STDERR_LOG" "error: Web Tooling Benchmark reported a failure."
}

test_hints_reference_selected_path_not_default() {
  run_expect_failure env WEB_TOOLING_BENCHMARK_DIR="$TMP/custom-missing" "$SCRIPT"

  assert_contains "$STDERR_LOG" "$TMP/custom-missing"
  assert_not_contains "$STDERR_LOG" "../web-tooling-benchmark"

  write_fake_wtb
  rm -f "$FAKE_WTB/dist/cli.js"
  run_expect_failure env WEB_TOOLING_BENCHMARK_DIR="$FAKE_WTB" "$SCRIPT"

  assert_contains "$STDERR_LOG" "$FAKE_WTB/dist/cli.js"
  assert_not_contains "$STDERR_LOG" "../web-tooling-benchmark"
}

trap cleanup EXIT
setup_stubs

test_scripts_are_executable
test_missing_checkout_fails_before_build
test_missing_bundle_fails_before_build
test_default_path_is_usable
test_override_env_honored
test_lumen_bin_skips_build
test_args_forwarded_including_only_babel
test_exception_in_output_fails_even_on_zero_exit
test_benign_error_word_in_output_succeeds
test_hints_reference_selected_path_not_default

echo "ok - run-web-tooling"
