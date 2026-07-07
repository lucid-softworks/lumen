#!/usr/bin/env bash
# Compile addon.c into addon.node — a Node native addon. The flags mirror what node-gyp emits:
# a shared library whose undefined napi_* symbols are resolved at load time against whatever host
# dlopens it (lumen, or node itself).
set -euo pipefail
cd "$(dirname "$0")"

case "$(uname -s)" in
  Darwin) cc -dynamiclib -undefined dynamic_lookup -o addon.node addon.c ;;
  Linux)  cc -shared -fPIC -o addon.node addon.c ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

echo "built addon.node"
