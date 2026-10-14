#!/usr/bin/env bash
# Install the package manager inside kit, without editing the user's mise config.
set -euo pipefail
umask 077
KIT="${KIT:-$HOME/.local/kit}"
VERSION=2026.10.3
case "$(uname -s)" in Linux) os=linux suffix=-musl ;; Darwin) os=macos suffix= ;; *) echo 'kit: supported hosts are macOS and Linux' >&2; exit 1 ;; esac
case "$(uname -m)" in x86_64|amd64) arch=x64 ;; aarch64|arm64) arch=arm64 ;; *) echo 'kit: supported CPUs are x86_64 and arm64' >&2; exit 1 ;; esac
mkdir -p "$KIT/bin" "$KIT/state"
if [ -x "$KIT/bin/mise" ] && "$KIT/bin/mise" --version >/dev/null 2>&1; then exit 0; fi
command -v tar >/dev/null || { echo "kit: prerequisite missing: tar" >&2; exit 1; }
fetch() {
  if command -v curl >/dev/null; then curl -fsSL --retry 3 --connect-timeout 20 --max-time 300 -o "$2" "$1"
  elif command -v wget >/dev/null; then wget -q -T 300 -O "$2" "$1"
  else echo 'kit: install curl or wget and ca-certificates first' >&2; return 1; fi
}
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
asset="mise-v$VERSION-$os-$arch$suffix.tar.gz"
base="https://github.com/jdx/mise/releases/download/v$VERSION"
fetch "$base/$asset" "$tmp/$asset"
fetch "$base/SHASUMS256.txt" "$tmp/sums"
(cd "$tmp"; awk -v file="$asset" '{ name=$2; sub(/^\.\//,"",name); if (name==file) print $1 "  " file }' sums > checksum
  [ -s checksum ] || { echo 'kit: checksum missing from release' >&2; exit 1; }
  if command -v sha256sum >/dev/null; then sha256sum -c checksum
  else shasum -a 256 -c checksum; fi)
tar -xzf "$tmp/$asset" -C "$tmp"
"$tmp/mise/bin/mise" --version >/dev/null
install -m 755 "$tmp/mise/bin/mise" "$KIT/bin/mise"
