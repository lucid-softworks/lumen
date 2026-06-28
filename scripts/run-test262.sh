#!/usr/bin/env bash
#
# Run the test262 conformance suite against the from-scratch `lumen` engine and print a score.
#
# Usage:
#   scripts/run-test262.sh                              # default slice (language/expressions+statements)
#   scripts/run-test262.sh language/expressions/addition
#   scripts/run-test262.sh built-ins/Array language/statements/for
#
# Ensures ./test262 exists (cloning it on first run), then builds + runs the runner. The runner
# links no V8 (lumen is std-only), so this loop is fast.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ ! -d "$ROOT/test262/test" ]; then
  "$ROOT/scripts/test262-clone.sh"
fi

cd "$ROOT"
cargo run --release -q -p test262-runner -- "$@"
