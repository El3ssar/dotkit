#!/usr/bin/env bash
# Install upstream fizsh scripts privately, with no compiler or administrator access.
set -euo pipefail
umask 077
KIT="${KIT:-$HOME/.local/kit}"
revision=f047f8a68a6499500bc46d40c412d99f01d2378d
checksum=915991530c92eea60b8fd86cac0ee416afc9f211ebc48b44d088083a1389588b
mkdir -p "$KIT/opt"
tmp=$(mktemp -d "$KIT/opt/fizsh-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
if [ "$#" -gt 0 ]; then cp "$1" "$tmp/source.tar.gz"
else curl -fsSL --retry 3 --connect-timeout 20 --max-time 300 "https://codeload.github.com/zsh-users/fizsh/tar.gz/$revision" -o "$tmp/source.tar.gz"; fi
if command -v sha256sum >/dev/null; then actual=$(sha256sum "$tmp/source.tar.gz")
else actual=$(shasum -a 256 "$tmp/source.tar.gz"); fi
[ "${actual%% *}" = "$checksum" ] || { echo 'kit: fizsh source checksum mismatch' >&2; exit 1; }
tar -xzf "$tmp/source.tar.gz" -C "$tmp"
src="$tmp/fizsh-$revision"
mkdir -p "$tmp/installed/bin" "$tmp/installed/etc/fizsh"
cp "$src/COPYING" "$tmp/installed/COPYING"
cp "$src/scripts/"*.zsh "$tmp/installed/etc/fizsh/"
# Ignore insecure completion directories instead of accepting them (-u upstream).
sed 's/compinit -u/compinit -i/' "$src/scripts/fizshrc.zsh" > "$tmp/installed/etc/fizsh/fizshrc.zsh"
# Upstream resets ~/.fizsh/.zshrc on first launch/version change and refreshes user files.
# Kit owns reconciliation: initialize missing files, never delete or replace existing config.
awk '
  /^  rm -rf / {next}
  /^export _fizsh_F_BIN_DIR=/ {print "export _fizsh_F_BIN_DIR=$(dirname \"$_fizsh_F_FIZSH_FULL_PATH\")"; next}
  /^export _fizsh_F_ETC_DIR=/ {print "export _fizsh_F_ETC_DIR=\"${_fizsh_F_BIN_DIR%/bin}/etc/fizsh\""; next}
  {gsub(/! -f \$2 \|\| \$1 -nt \$2/, "! -e $2"); print}
' "$src/scripts/fizsh" > "$tmp/installed/bin/fizsh"
chmod 755 "$tmp/installed/bin/fizsh"
zsh_bin="$KIT/opt/zsh/bin/zsh"
[ -x "$zsh_bin" ] || zsh_bin=$(command -v zsh)
"$zsh_bin" -n "$tmp/installed/bin/fizsh"
printf '%s\n' "$revision" > "$tmp/installed/source-revision"
rm -rf "$KIT/opt/fizsh.previous"
[ ! -e "$KIT/opt/fizsh" ] || mv "$KIT/opt/fizsh" "$KIT/opt/fizsh.previous"
mv "$tmp/installed" "$KIT/opt/fizsh"
