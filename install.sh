#!/bin/sh
# Install kit (dotkit) and, optionally, set this machine up from your kit repo:
#   curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/El3ssar/dotkit/main/install.sh | sh -s -- <your-kit-repo-url>
# Puts a prebuilt `kit` in ~/.local/bin (no Rust needed). Set KIT_VERSION=v0.1.0 to pin a release.
set -eu
REPO="El3ssar/dotkit"
BIN_DIR="${KIT_BIN_DIR:-$HOME/.local/bin}"
say() { printf '\033[32m›\033[0m %s\n' "$*"; }
die() { printf '\033[31mkit install: %s\033[0m\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux)  os=unknown-linux-musl ;;   # static: runs on any Linux, old glibc or not
  *) die "unsupported system $(uname -s)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) die "unsupported CPU $(uname -m)" ;;
esac
target="$arch-$os"

if [ -n "${KIT_VERSION:-}" ]; then
  url="https://github.com/$REPO/releases/download/$KIT_VERSION/kit-$target.tar.gz"
else
  url="https://github.com/$REPO/releases/latest/download/kit-$target.tar.gz"
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
say "downloading kit for $target"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL --retry 3 -o "$tmp/kit.tar.gz" "$url" || die "download failed: $url"
else
  wget -qO "$tmp/kit.tar.gz" "$url" || die "download failed: $url"
fi
checksum_url="$url.sha256"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL --retry 3 -o "$tmp/checksum" "$checksum_url" || die "checksum download failed"
else
  wget -qO "$tmp/checksum" "$checksum_url" || die "checksum download failed"
fi
expected=$(awk 'NR == 1 {print $1}' "$tmp/checksum")
case "$expected" in *[!0-9a-fA-F]*|'') die "invalid release checksum" ;; esac
[ "${#expected}" = 64 ] || die "invalid release checksum"
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/kit.tar.gz")
else actual=$(shasum -a 256 "$tmp/kit.tar.gz"); fi
[ "$expected" = "${actual%% *}" ] || die "release checksum mismatch"
tar -xzf "$tmp/kit.tar.gz" -C "$tmp"
mkdir -p "$BIN_DIR"
install -m 755 "$tmp/kit" "$BIN_DIR/kit"
say "installed $("$BIN_DIR/kit" --version) to $BIN_DIR/kit"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) say "add $BIN_DIR to your PATH" ;; esac

# Package prerequisites are handled by kit sync according to the recorded recipes.
# Installing the kit binary alone must not install unrelated package managers.

if [ $# -gt 0 ]; then
  say "setting this machine up from $1"
  "$BIN_DIR/kit" init "$1"
  if [ -r /dev/tty ] && ( : </dev/tty ) 2>/dev/null; then
    "$BIN_DIR/kit" sync </dev/tty
  else
    "$BIN_DIR/kit" sync
  fi
  say "done — open a new terminal"
else
  say "next: kit init <your-kit-repo-url> && kit sync   (or kit init to start a new repo)"
fi
