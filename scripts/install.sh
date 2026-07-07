#!/usr/bin/env bash
#
# lumen installer. Downloads the prebuilt `lumen` runtime CLI for this platform from the GitHub
# `nightly` release and installs it to ~/.lumen/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/lucid-softworks/lumen/main/scripts/install.sh | bash
#
# Environment overrides:
#   LUMEN_INSTALL   install prefix (default: $HOME/.lumen); the binary lands in $LUMEN_INSTALL/bin
#   LUMEN_RELEASE   release tag to pull from (default: nightly)
set -euo pipefail

REPO="lucid-softworks/lumen"
RELEASE="${LUMEN_RELEASE:-nightly}"
INSTALL_DIR="${LUMEN_INSTALL:-$HOME/.lumen}"
BIN_DIR="$INSTALL_DIR/bin"

red()   { printf '\033[31m%s\033[0m\n' "$1"; }
green() { printf '\033[32m%s\033[0m\n' "$1"; }
bold()  { printf '\033[1m%s\033[0m\n' "$1"; }

error() { red "error: $1" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || error "curl is required"

# Map this platform to a release asset target triple.
os="$(uname -s)"
arch="$(uname -m)"
case "$os-$arch" in
  Darwin-arm64)          target="aarch64-apple-darwin" ;;
  Linux-x86_64)          target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64|Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-x86_64)
    error "Intel macOS is not prebuilt. Build from source: cargo build --release -p lumen-cli" ;;
  *)
    error "unsupported platform '$os-$arch'. Build from source: cargo build --release -p lumen-cli" ;;
esac

asset="lumen-cli-$target"
url="https://github.com/$REPO/releases/download/$RELEASE/$asset"

bold "Installing lumen ($target, $RELEASE release)"
mkdir -p "$BIN_DIR"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

printf 'downloading %s ...\n' "$url"
if ! curl -fSL --progress-bar "$url" -o "$tmp"; then
  error "download failed. The '$RELEASE' release may not have an asset for $target yet."
fi

# Reject an HTML error page (a 404 returned with a 200 wrapper, etc.).
if head -c 4 "$tmp" | grep -q '<'; then
  error "downloaded file is not a binary (got an HTML page — the asset may be missing)"
fi

install -m 755 "$tmp" "$BIN_DIR/lumen"
green "installed lumen -> $BIN_DIR/lumen"
"$BIN_DIR/lumen" --version >/dev/null 2>&1 || true

# PATH guidance (offer to append to the detected shell profile).
case ":$PATH:" in
  *":$BIN_DIR:"*) exit 0 ;; # already on PATH
esac

shell_name="$(basename "${SHELL:-}")"
case "$shell_name" in
  zsh)  profile="$HOME/.zshrc" ;;
  bash) profile="$HOME/.bashrc" ;;
  fish) profile="$HOME/.config/fish/config.fish" ;;
  *)    profile="" ;;
esac

line="export PATH=\"$BIN_DIR:\$PATH\""
[ "$shell_name" = "fish" ] && line="fish_add_path $BIN_DIR"

echo
bold "Add lumen to your PATH:"
if [ -n "$profile" ] && [ -w "$(dirname "$profile")" ]; then
  if ! grep -qsF "$BIN_DIR" "$profile" 2>/dev/null; then
    printf '\n# lumen\n%s\n' "$line" >>"$profile"
    green "appended to $profile — restart your shell or: source $profile"
  else
    green "already configured in $profile"
  fi
else
  printf '  add this to your shell profile:\n\n    %s\n' "$line"
fi
