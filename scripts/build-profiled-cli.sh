#!/usr/bin/env bash
# Build an opt-in Linux CLI using mimalloc and compiler PGO from generated workloads.
# Usage: bash scripts/build-profiled-cli.sh [new-output-directory]
# Requires Node and llvm-profdata from the compiler's llvm-tools-preview component.
set -euo pipefail
pgo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$pgo_root"
pgo_target="$(rustc -vV | sed -n 's/^host: //p')"
if [[ "$pgo_target" != x86_64-unknown-linux-gnu ]]; then
  echo 'This measured build recipe currently supports x86_64-unknown-linux-gnu.' >&2
  exit 1
fi
if [[ -n "${RUSTFLAGS:-}" || -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
  echo 'Run with RUSTFLAGS and CARGO_ENCODED_RUSTFLAGS unset.' >&2
  exit 1
fi
command -v node >/dev/null
pgo_profdata="${LLVM_PROFDATA:-$(rustc --print sysroot)/lib/rustlib/$pgo_target/bin/llvm-profdata}"
if [[ ! -x "$pgo_profdata" ]]; then
  echo 'Install llvm-tools-preview for this Rust toolchain, or set LLVM_PROFDATA to a matching llvm-profdata.' >&2
  exit 1
fi
pgo_directory="$(node -e 'console.log(require("node:path").resolve(process.argv[1]))' "${1:-target/profiled-cli}")"
if [[ -e "$pgo_directory" ]]; then
  echo "Choose a new output directory; refusing to mix profiles in $pgo_directory" >&2
  exit 1
fi
mkdir -p "$pgo_directory/raw"
export LUMEN_PGO_DIR="$pgo_directory"
export CARGO_TARGET_DIR="$pgo_directory/build"
export LUMEN_TIER=jit LUMEN_TIER_THRESHOLD=8
rustc -vV > "$pgo_directory/rustc.txt"
"$pgo_profdata" --version > "$pgo_directory/llvm-profdata.txt"
node scripts/pgo/prepare-training.mjs
printf 'Building unprofiled control in %s\n' "$pgo_directory"
cargo build --locked --release --target="$pgo_target" -p lumen-cli --features mimalloc > "$pgo_directory/control-build.log" 2>&1 || {
  cat "$pgo_directory/control-build.log" >&2
  exit 1
}
cp "$CARGO_TARGET_DIR/$pgo_target/release/lumen-cli" "$pgo_directory/lumen-baseline"
printf 'Building instrumented CLI\n'
CARGO_ENCODED_RUSTFLAGS="-Cprofile-generate=$pgo_directory/raw" \
  cargo build --locked --release --target="$pgo_target" -p lumen-cli --features mimalloc > "$pgo_directory/generate-build.log" 2>&1 || {
  cat "$pgo_directory/generate-build.log" >&2
  exit 1
}
cp "$CARGO_TARGET_DIR/$pgo_target/release/lumen-cli" "$pgo_directory/lumen-instrumented"
node scripts/pgo/train.mjs "$pgo_directory/lumen-instrumented" "$pgo_directory/lumen-baseline"
"$pgo_profdata" merge -o "$pgo_directory/merged.profdata" "$pgo_directory/raw"
"$pgo_profdata" show "$pgo_directory/merged.profdata" > "$pgo_directory/profile-summary.txt"
printf 'Building profile-optimized CLI\n'
CARGO_ENCODED_RUSTFLAGS="-Cprofile-use=$pgo_directory/merged.profdata"$'\x1f''-Cllvm-args=-pgo-warn-missing-function' \
  cargo build --locked --release --target="$pgo_target" -p lumen-cli --features mimalloc > "$pgo_directory/use-build.log" 2>&1 || {
  cat "$pgo_directory/use-build.log" >&2
  exit 1
}
node --input-type=module - "$pgo_directory/use-build.log" <<'JS'
import fs from 'node:fs';
import assert from 'node:assert/strict';
assert(!/profile data may be out of date|profile.*mismatch|no profile data available|function control flow change|LLVM Profile Error/i.test(fs.readFileSync(process.argv[2],'utf8')),'PGO profile mismatch; inspect use-build.log');
JS
cp "$CARGO_TARGET_DIR/$pgo_target/release/lumen-cli" "$pgo_directory/lumen-cli"
printf 'Profiled CLI: %s\nControl CLI: %s\n' "$pgo_directory/lumen-cli" "$pgo_directory/lumen-baseline"
