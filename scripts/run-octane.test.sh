#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
SCRIPT="$ROOT/scripts/run-octane.sh"
TMP_ROOT="target/run-octane-tests"
TMP="$TMP_ROOT/$$"
FAKE_OCTANE="$TMP/octane"
FAKE_BIN="$TMP/bin"
CARGO_LOG="$TMP/cargo.log"
LUMEN_LOG="$TMP/lumen.args"
DRIVER_CAPTURE="$TMP/driver.js"
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

# The driver is a mktemp path, so assert the base + suite args exactly and that
# the final argument is *some* octane-driver.js.
assert_lumen_suite_args() {
  local expected="$1"
  local total body last
  total="$(wc -l < "$LUMEN_LOG")"
  body="$(head -n "$((total - 1))" "$LUMEN_LOG")"
  last="$(tail -n 1 "$LUMEN_LOG")"
  if [ "$body" != "$expected" ]; then
    echo "--- expected suite args ---" >&2
    printf '%s\n' "$expected" >&2
    echo "--- actual ---" >&2
    printf '%s\n' "$body" >&2
    fail "lumen suite args did not match"
  fi
  case "$last" in
    */octane-driver.js) ;;
    *) fail "last lumen arg was not the driver: $last" ;;
  esac
}

cleanup() {
  rm -rf "$TMP"
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
# Capture the driver (last arg) so tests can assert the sed transform before the
# runner's trap removes its work dir.
driver="${@: -1}"
cp "$driver" "$DRIVER_CAPTURE" 2>/dev/null || true
if [ -n "${LUMEN_STUB_OUTPUT:-}" ]; then
  printf '%s\n' "$LUMEN_STUB_OUTPUT"
elif [ "${LUMEN_STUB_NO_SCORE:-0}" -eq 0 ]; then
  printf '%s\n' 'Score (version 9): 1'
fi
exit 0
SH
  chmod +x "$LUMEN_STUB"

  export CARGO_LOG LUMEN_LOG DRIVER_CAPTURE
  export PATH="$FAKE_BIN:$PATH"
}

write_fake_octane() {
  rm -rf "$FAKE_OCTANE"
  mkdir -p "$FAKE_OCTANE"
  printf 'base\n' > "$FAKE_OCTANE/base.js"
  cat > "$FAKE_OCTANE/run.js" <<'JS'
load("base.js");
load("richards.js");
print("driver");
JS

  local files=(
    richards.js deltablue.js crypto.js raytrace.js earley-boyer.js regexp.js
    splay.js navier-stokes.js pdfjs.js mandreel.js gbemu-part1.js
    gbemu-part2.js code-load.js box2d.js zlib.js zlib-data.js
    typescript.js typescript-input.js typescript-compiler.js
  )
  local f
  for f in "${files[@]}"; do
    printf '%s\n' "$f" > "$FAKE_OCTANE/$f"
  done
}

run_expect_failure() {
  local status=0
  "$@" > "$TMP/stdout.log" 2> "$STDERR_LOG" || status=$?
  if [ "$status" -eq 0 ]; then
    fail "command unexpectedly succeeded: $*"
  fi
}

test_missing_octane_error() {
  # No LUMEN_BIN: the build path is active, proving the Octane check fails before
  # any build is attempted.
  run_expect_failure env OCTANE="$TMP/missing-octane" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: Octane not found at $TMP/missing-octane"
  assert_contains "$STDERR_LOG" "need base.js and run.js"
  assert_contains "$STDERR_LOG" "git clone https://github.com/chromium/octane \"$TMP/missing-octane\""
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo should not run when the Octane checkout is missing"
  fi
}

test_lumen_bin_skips_build_and_expands_suites() {
  write_fake_octane
  # A trailing `.js` (e.g. from tab-completion) is tolerated on every suite.
  env OCTANE="$FAKE_OCTANE" LUMEN_BIN="$LUMEN_STUB" "$SCRIPT" richards gbemu crypto.js \
    > "$TMP/stdout.log"

  assert_lumen_suite_args "$FAKE_OCTANE/base.js
$FAKE_OCTANE/richards.js
$FAKE_OCTANE/gbemu-part1.js
$FAKE_OCTANE/gbemu-part2.js
$FAKE_OCTANE/crypto.js"
  assert_file_equals "$DRIVER_CAPTURE" 'print("driver");'
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo must not run when LUMEN_BIN is provided"
  fi
}

test_js_suffix_expands_multifile_suites() {
  write_fake_octane
  # Regression: `zlib.js` / `typescript.js` must expand to the full multi-file
  # suite, not silently run only the single same-named file.
  env OCTANE="$FAKE_OCTANE" LUMEN_BIN="$LUMEN_STUB" "$SCRIPT" zlib.js typescript.js \
    > "$TMP/stdout.log"

  assert_lumen_suite_args "$FAKE_OCTANE/base.js
$FAKE_OCTANE/zlib.js
$FAKE_OCTANE/zlib-data.js
$FAKE_OCTANE/typescript.js
$FAKE_OCTANE/typescript-input.js
$FAKE_OCTANE/typescript-compiler.js"
}

test_full_suite_order() {
  write_fake_octane
  env OCTANE="$FAKE_OCTANE" LUMEN_BIN="$LUMEN_STUB" "$SCRIPT" > "$TMP/stdout.log"

  assert_lumen_suite_args "$FAKE_OCTANE/base.js
$FAKE_OCTANE/richards.js
$FAKE_OCTANE/deltablue.js
$FAKE_OCTANE/crypto.js
$FAKE_OCTANE/raytrace.js
$FAKE_OCTANE/earley-boyer.js
$FAKE_OCTANE/regexp.js
$FAKE_OCTANE/splay.js
$FAKE_OCTANE/navier-stokes.js
$FAKE_OCTANE/pdfjs.js
$FAKE_OCTANE/mandreel.js
$FAKE_OCTANE/gbemu-part1.js
$FAKE_OCTANE/gbemu-part2.js
$FAKE_OCTANE/code-load.js
$FAKE_OCTANE/box2d.js
$FAKE_OCTANE/zlib.js
$FAKE_OCTANE/zlib-data.js
$FAKE_OCTANE/typescript.js
$FAKE_OCTANE/typescript-input.js
$FAKE_OCTANE/typescript-compiler.js"
}

test_unknown_suite_fails_before_build() {
  write_fake_octane
  # No LUMEN_BIN: suite validation must reject the typo before the release build.
  run_expect_failure env OCTANE="$FAKE_OCTANE" "$SCRIPT" nope

  assert_contains "$STDERR_LOG" "error: unknown/missing Octane suite file: $FAKE_OCTANE/nope.js (from suite 'nope')"
  if [ -f "$CARGO_LOG" ]; then
    fail "suite validation must run before the release build"
  fi
}

test_octane_reported_error_fails() {
  write_fake_octane
  run_expect_failure env OCTANE="$FAKE_OCTANE" LUMEN_BIN="$LUMEN_STUB" \
    LUMEN_STUB_OUTPUT='zlib: ReferenceError: read is not defined' \
    "$SCRIPT" zlib

  assert_contains "$TMP/stdout.log" "zlib: ReferenceError: read is not defined"
  assert_contains "$STDERR_LOG" "error: Octane reported benchmark failure."
}

test_missing_score_fails() {
  write_fake_octane
  run_expect_failure env OCTANE="$FAKE_OCTANE" LUMEN_BIN="$LUMEN_STUB" LUMEN_STUB_NO_SCORE=1 \
    "$SCRIPT" mandreel

  assert_contains "$STDERR_LOG" "error: Octane completed without reporting a score."
}

trap cleanup EXIT
setup_stubs

test_missing_octane_error
test_lumen_bin_skips_build_and_expands_suites
test_js_suffix_expands_multifile_suites
test_full_suite_order
test_unknown_suite_fails_before_build
test_octane_reported_error_fails
test_missing_score_fails

echo "ok - run-octane"
