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
STDERR_LOG="$TMP/stderr.log"
LUMEN="$ROOT/target/release/lumen"
BACKUP_DIR="$TMP/backup"
HAD_LUMEN=0

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

cleanup() {
  if [ "$HAD_LUMEN" -eq 1 ]; then
    rm -f "$LUMEN"
    mv "$BACKUP_DIR/lumen" "$LUMEN"
  else
    rm -f "$LUMEN"
  fi
  rm -rf "$TMP"
}

setup_stubs() {
  mkdir -p "$TMP_ROOT"
  mkdir -p "$FAKE_BIN" target/release "$BACKUP_DIR"

  if [ -e "$LUMEN" ]; then
    HAD_LUMEN=1
    mv "$LUMEN" "$BACKUP_DIR/lumen"
  fi

  cat > "$FAKE_BIN/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CARGO_LOG"
exit 0
SH
  chmod +x "$FAKE_BIN/cargo"

  cat > "$LUMEN" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$LUMEN_LOG"
if [ -n "${LUMEN_STUB_OUTPUT:-}" ]; then
  printf '%s\n' "$LUMEN_STUB_OUTPUT"
elif [ "${LUMEN_STUB_NO_SCORE:-0}" -eq 0 ]; then
  printf '%s\n' 'Score (version 9): 1'
fi
exit "${LUMEN_STUB_STATUS:-0}"
SH
  chmod +x "$LUMEN"

  export CARGO_LOG LUMEN_LOG
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
  run_expect_failure env OCTANE="$TMP/missing-octane" "$SCRIPT"

  assert_contains "$STDERR_LOG" "error: Octane not found at $TMP/missing-octane"
  assert_contains "$STDERR_LOG" "need base.js and run.js"
  assert_contains "$STDERR_LOG" "git clone https://github.com/chromium/octane \"$TMP/missing-octane\""
  if [ -f "$CARGO_LOG" ]; then
    fail "cargo should not run when the Octane checkout is missing"
  fi
}

test_selected_suite_expansion_and_driver() {
  write_fake_octane
  env OCTANE="$FAKE_OCTANE" "$SCRIPT" richards gbemu crypto.js > "$TMP/stdout.log"

  assert_file_equals "$CARGO_LOG" "build --release -q -p lumen --bin lumen"
  assert_file_equals "$LUMEN_LOG" "$FAKE_OCTANE/base.js
$FAKE_OCTANE/richards.js
$FAKE_OCTANE/gbemu-part1.js
$FAKE_OCTANE/gbemu-part2.js
$FAKE_OCTANE/crypto.js
$ROOT/target/octane-driver.js"
  assert_file_equals "$ROOT/target/octane-driver.js" 'print("driver");'
}

test_full_suite_order() {
  : > "$CARGO_LOG"
  write_fake_octane
  env OCTANE="$FAKE_OCTANE" "$SCRIPT" > "$TMP/stdout.log"

  assert_file_equals "$LUMEN_LOG" "$FAKE_OCTANE/base.js
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
$FAKE_OCTANE/typescript-compiler.js
$ROOT/target/octane-driver.js"
}

test_unknown_suite_error() {
  write_fake_octane
  run_expect_failure env OCTANE="$FAKE_OCTANE" "$SCRIPT" nope

  assert_contains "$STDERR_LOG" "error: unknown/missing Octane suite file: $FAKE_OCTANE/nope.js (from suite 'nope')"
}

test_octane_reported_error_fails() {
  write_fake_octane
  run_expect_failure env OCTANE="$FAKE_OCTANE" \
    LUMEN_STUB_OUTPUT='zlib: ReferenceError: read is not defined' \
    "$SCRIPT" zlib

  assert_contains "$TMP/stdout.log" "zlib: ReferenceError: read is not defined"
  assert_contains "$STDERR_LOG" "error: Octane reported benchmark failure."
}

test_missing_score_fails() {
  write_fake_octane
  run_expect_failure env OCTANE="$FAKE_OCTANE" LUMEN_STUB_NO_SCORE=1 "$SCRIPT" mandreel

  assert_contains "$STDERR_LOG" "error: Octane completed without reporting a score."
}

trap cleanup EXIT
setup_stubs

test_missing_octane_error
test_selected_suite_expansion_and_driver
test_full_suite_order
test_unknown_suite_error
test_octane_reported_error_fails
test_missing_score_fails

echo "ok - run-octane"
