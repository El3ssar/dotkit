#!/usr/bin/env bash
# Deploy all tracked dotfiles into the real home. Preserve originals for removal.
set -euo pipefail
umask 077
KIT="${KIT:-$HOME/.local/kit}"
P="$KIT/payload"
mkdir -p "$KIT/state" "$KIT/originals" "$KIT/backups"
originals="$KIT/state/originals.list"
touch "$originals"
backup="$KIT/backups/$(date +%Y%m%d-%H%M%S)-$$"
# Validate paths again on the target, even though the payload producer validates them.
valid() { case "$1" in ''|/*|..|../*|*/../*|*/..|*$'\t'*|*$'\n'*|*$'\r'*|.local/kit|.local/kit/*|.local/state/kit|.local/state/kit/*|.local/share/kit|.local/share/kit/*) return 1 ;; *) return 0 ;; esac; }
copy_entry() {
  local src=$1 dest=$2 parent
  parent=$(dirname "$dest")
  mkdir -p "$parent"
  if [ -d "$dest" ] && [ ! -L "$dest" ]; then echo "kit: directory blocks $dest; left untouched" >&2; return 1; fi
  # cp -a can follow the destination symlink: replace through an unused sibling instead.
  local tmp oldmode srcmode mode
  tmp=$(mktemp -d "$parent/.kit-copy.XXXXXX")
  if ! cp -a "$src" "$tmp/entry"; then rm -rf "$tmp"; return 1; fi
  if [ -f "$dest" ] && [ ! -L "$dest" ] && [ ! -L "$src" ]; then
    oldmode=$(stat -c %a "$dest" 2>/dev/null || stat -f %Lp "$dest")
    srcmode=$(stat -c %a "$src" 2>/dev/null || stat -f %Lp "$src")
    mode=$(( 8#$oldmode & 0666 ))
    if [ $(( 8#$srcmode & 0111 )) != 0 ]; then mode=$(( mode | ((8#$oldmode & 0444) >> 2) )); fi
    chmod "$(printf '%o' "$mode")" "$tmp/entry"
  fi
  if [ "$(uname -s)" = Darwin ]; then mv -fh "$tmp/entry" "$dest"
  else mv -fT "$tmp/entry" "$dest"; fi || { rm -rf "$tmp"; return 1; }
  rmdir "$tmp"
}
if [ "${1:-}" = remove ]; then
  failed=0
  while IFS=$'\t' read -r kind rel; do
    valid "$rel" || { failed=1; continue; }
    dest="$HOME/$rel"
    parent="$HOME"; rest="$rel"; safe=1
    while [[ "$rest" = */* ]]; do
      part=${rest%%/*}; rest=${rest#*/}; parent="$parent/$part"
      if [ -L "$parent" ] || { [ -e "$parent" ] && [ ! -d "$parent" ]; }; then safe=0; break; fi
    done
    if [ "$safe" = 0 ]; then echo "kit: parent blocks restoring ~/$rel; original kept" >&2; failed=1; continue; fi
    if [ "$kind" = existing ]; then
      mkdir -p "$backup/$(dirname "$rel")"
      if [ -e "$dest" ] || [ -L "$dest" ]; then cp -a "$dest" "$backup/$rel" || { failed=1; continue; }; fi
      copy_entry "$KIT/originals/$rel" "$dest" || failed=1
    else
      if [ -e "$dest" ] || [ -L "$dest" ]; then
        mkdir -p "$backup/$(dirname "$rel")"; cp -a "$dest" "$backup/$rel" || { failed=1; continue; }
        rm -f "$dest" || failed=1
      fi
    fi
  done < "$originals"
  exit "$failed"
fi
failed=0
while IFS= read -r -d '' rel; do
  valid "$rel" || { echo "kit: invalid path $rel" >&2; failed=1; continue; }
  dest="$HOME/$rel" src="$P/home/$rel"
  # Never traverse an existing directory symlink into some other location.
  parent="$HOME" rest="$rel"
  safe=1
  while [[ "$rest" = */* ]]; do
    part=${rest%%/*}; rest=${rest#*/}; parent="$parent/$part"
    if [ -L "$parent" ] || { [ -e "$parent" ] && [ ! -d "$parent" ]; }; then safe=0; break; fi
  done
  if [ "$safe" = 0 ]; then echo "kit: parent blocks ~/$rel; left untouched" >&2; failed=1; continue; fi
  if [ -d "$dest" ] && [ ! -L "$dest" ]; then echo "kit: folder blocks ~/$rel; left untouched" >&2; failed=1; continue; fi
  if [ -L "$src" ] && [ -L "$dest" ] && [ "$(readlink "$src")" = "$(readlink "$dest")" ]; then continue; fi
  if [ -f "$src" ] && [ ! -L "$src" ] && [ -f "$dest" ] && [ ! -L "$dest" ] && cmp -s "$src" "$dest"; then
    smode=$(stat -c %a "$src" 2>/dev/null || stat -f %Lp "$src")
    dmode=$(stat -c %a "$dest" 2>/dev/null || stat -f %Lp "$dest")
    if [ $(( (8#$smode & 0111) != 0 )) = $(( (8#$dmode & 0111) != 0 )) ]; then continue; fi
  fi
  if ! awk -F '\t' -v rel="$rel" '$2 == rel { found=1 } END { exit !found }' "$originals"; then
    if [ -e "$dest" ] || [ -L "$dest" ]; then
      mkdir -p "$KIT/originals/$(dirname "$rel")"
      cp -a "$dest" "$KIT/originals/$rel" || { failed=1; continue; }
      printf 'existing\t%s\n' "$rel" >> "$originals"
    else printf 'new\t%s\n' "$rel" >> "$originals"; fi
  fi
  if [ -e "$dest" ] || [ -L "$dest" ]; then
    mkdir -p "$backup/$(dirname "$rel")"
    cp -a "$dest" "$backup/$rel" || { failed=1; continue; }
  fi
  copy_entry "$src" "$dest" || failed=1
  echo "kit: wrote ~/$rel"
done < "$P/home.list"
exit "$failed"
