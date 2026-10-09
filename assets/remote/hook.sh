#!/usr/bin/env bash
# Adds/removes the login hook that starts your kit shell.
#   hook.sh install | remove
# The hook only fires for interactive shells on a terminal, so scp, rsync,
# `ssh host command` and `ssh -t host command` behave exactly as before. Bail-outs: `touch ~/.kit-off`, or
# `ssh -t host bash --norc` for a plain shell.
set -euo pipefail

BEGIN='# >>> kit: interactive logins start ~/.local/kit (off: touch ~/.kit-off · remove: kit push <host> --remove) >>>'
END='# <<< kit <<<'
BLOCK="$BEGIN
if [ -z \"\${KIT_ACTIVE:-}\" ] && case \$- in *i*) true ;; *) false ;; esac \\
   && [ -t 0 ] && [ -t 1 ] && [ ! -e \"\$HOME/.kit-off\" ] \\
   && \"\$HOME/.local/kit/bin/kit-shell\" --ok; then
  exec \"\$HOME/.local/kit/bin/kit-shell\"
fi
$END"

HERE=$(cd "$(dirname "$0")" && pwd)
MANAGED="$HERE/../managed-rc.list"   # rc files kit tracks on this machine: never written to

login_shell=$(basename "$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7)" 2>/dev/null || echo bash)

# rc files the login shell reads; "top" = insert at the start (skip the old setup entirely),
# "end" = append (keep things like cluster module setup that bash rc files do first)
targets() {
  echo "end .bashrc"
  case "$login_shell" in
    zsh)   echo "top .zshrc" ;;
    fizsh) echo "top .fizsh/.zshrc" ;;
  esac
}

# delete the kit block; leaves the file alone if the end marker is missing (never truncates)
strip_block() {
  awk -v b='^# >>> kit: ' -v e='^# <<< kit <<<' '
    $0 ~ b && !inb { inb = 1; buf = $0 "\n"; next }
    inb { buf = buf $0 "\n"; if ($0 ~ e) { inb = 0; buf = "" }; next }
    { print }
    END { if (inb) printf "%s", buf }' "$1" > "$1.kit-tmp" && cat "$1.kit-tmp" > "$1" && rm -f "$1.kit-tmp"
}

install_hook() {
  local where rel file
  while read -r where rel; do
    file="$HOME/$rel"
    if [ -f "$MANAGED" ] && grep -qxF "$rel" "$MANAGED"; then
      echo "kit: not touching ~/$rel (kit tracks it); add the kit block yourself if you want it there" >&2
      continue
    fi
    if [ -L "$file" ] && [ ! -e "$file" ]; then echo "kit: skipping broken symlink $file" >&2; continue; fi
    [ -e "$file" ] || { [ "$rel" = .bashrc ] && continue; touch "$file"; }
    [ -e "$file.kit-backup" ] || cp -p "$file" "$file.kit-backup"
    strip_block "$file"
    if [ "$where" = top ]; then
      { printf '%s\n' "$BLOCK"; cat "$file"; } > "$file.kit-tmp" && cat "$file.kit-tmp" > "$file" && rm -f "$file.kit-tmp"
    else
      [ -s "$file" ] && [ "$(tail -c1 "$file")" != "" ] && echo >> "$file"   # file lacked a final newline
      printf '%s\n' "$BLOCK" >> "$file"
    fi
    echo "kit: hook in $file"
  done < <(targets)
}

remove_hook() {
  local f
  for f in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.fizsh/.zshrc"; do
    [ -f "$f" ] && grep -q '^# >>> kit: ' "$f" && strip_block "$f" && echo "kit: hook removed from $f"
    [ -f "$f.kit-backup" ] || continue
    # install added a final newline the file lacked: take it back out
    if ! cmp -s "$f" "$f.kit-backup" && cmp -s "$f" <(cat "$f.kit-backup"; echo); then
      cat "$f.kit-backup" > "$f"
    fi
    # the backup is only kept if the file changed in other ways since kit was installed
    cmp -s "$f" "$f.kit-backup" && rm -f "$f.kit-backup"
  done
  return 0
}

case "${1:-}" in
  install) install_hook ;;
  remove)  remove_hook ;;
  *) echo "usage: hook.sh install|remove" >&2; exit 2 ;;
esac
