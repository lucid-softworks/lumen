#!/usr/bin/env bash
#
# Run lumen's benchmarks. Two suites, both on the std-only harness (no third-party bench crate):
#
#   engine     — running representative JavaScript through Engine::eval, plus startup.
#   internals  — the compilation pipeline stage by stage (lex → parse → encode → decode),
#                via the `bench` feature's lumen::bench_api.
#
# Usage:
#   scripts/run-benches.sh            # both suites
#   scripts/run-benches.sh engine     # just the engine suite
#   scripts/run-benches.sh internals  # just the internals suite
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

suite="${1:-all}"

case "$suite" in
  engine)    cargo bench -p lumen --bench engine ;;
  internals) cargo bench -p lumen --features bench --bench internals ;;
  all)
    cargo bench -p lumen --bench engine
    cargo bench -p lumen --features bench --bench internals
    ;;
  *) echo "unknown suite '$suite' (want: engine | internals | all)" >&2; exit 1 ;;
esac
