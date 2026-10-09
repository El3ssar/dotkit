#!/usr/bin/env bash
# Runs ON the remote (called by `kit push`). Installs or updates ~/.local/kit from the
# payload the Mac just synced to ~/.local/kit/payload. Safe to run again: tools whose
# version didn't change are skipped, shell history and other state are kept.
set -euo pipefail

KIT="$HOME/.local/kit"
P="$KIT/payload"
ZSH_BIN_VERSION="v6.1.1"   # romkatv/zsh-bin release (static, relocatable zsh 5.8)
NVIM_VERSION="0.12.5"      # neovim/neovim-releases: built against glibc 2.17, runs almost anywhere
NCURSES_VERSION="6.5"      # built only when the system has no ncurses headers (cbonsai dashboard)
MISE_VERSION="2026.10.3"   # installs every tool in packages.json (see mise.toml)
MODE=$(cat "$P/mode" 2>/dev/null || echo remote)   # remote: a server (configs live in the kit) · local: your own Linux machine

say()  { printf '\033[32m›\033[0m %s\n' "$*"; }
warn() { printf '\033[33m! %s\033[0m\n' "$*" >&2; }

case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ARCH_RE='x86_64|amd64|x64' ;;
  aarch64|arm64) ARCH=aarch64 ARCH_RE='aarch64|arm64' ;;
  *) echo "kit: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
[ "$(uname -s)" = Linux ] || { echo "kit: only Linux remotes are supported" >&2; exit 1; }

mkdir -p "$KIT"/{bin,opt/.versions,config,data,state,cache,fizsh,share}

# --- helpers ---------------------------------------------------------------
installed() { [ "$(cat "$KIT/opt/.versions/$1" 2>/dev/null)" = "$2" ]; }
mark()      { echo "$2" > "$KIT/opt/.versions/$1"; }

fetch() {  # url dest
  curl -fsSL --retry 3 --connect-timeout 20 -o "$2" "$1" || { warn "download failed: $1"; return 1; }
}

unpack() {  # archive dir
  case "$1" in
    *.tar.gz|*.tgz) tar -xzf "$1" -C "$2" ;;
    *.tar.xz)       tar -xJf "$1" -C "$2" ;;
    *.zip)          unzip -qo "$1" -d "$2" ;;
    *)              return 1 ;;
  esac
}

install_tool() {  # name version url bins
  local name=$1 ver=$2 url=${3//\{v\}/$2} bins=$4 b
  local all_there=1
  for b in ${bins//,/ }; do [ -x "$KIT/bin/$b" ] || all_there=0; done
  if installed "$name" "$ver" && [ $all_there = 1 ]; then return 0; fi
  say "installing $name $ver"
  local tmp; tmp=$(mktemp -d)
  local file="$tmp/${url##*/}"
  fetch "$url" "$file" || { rm -rf "$tmp"; return 0; }
  mkdir -p "$tmp/x"
  if unpack "$file" "$tmp/x"; then
    for b in ${bins//,/ }; do
      local found; found=$(find "$tmp/x" -type f -name "$b" | head -1)
      if [ -n "$found" ]; then install -m 755 "$found" "$KIT/bin/$b"; else warn "$name: '$b' not found in archive"; fi
    done
  else
    install -m 755 "$file" "$KIT/bin/${bins%%,*}"
  fi
  rm -rf "$tmp"
  mark "$name" "$ver"
}

# --- mise: installs the tools from packages.json --------------------------
if ! installed mise "$MISE_VERSION" || [ ! -x "$KIT/bin/mise" ]; then
  say "installing mise $MISE_VERSION"
  marc=$([ "$ARCH" = x86_64 ] && echo x64 || echo arm64)
  tmp=$(mktemp -d)
  fetch "https://github.com/jdx/mise/releases/download/v$MISE_VERSION/mise-v$MISE_VERSION-linux-$marc-musl.tar.gz" "$tmp/mise.tar.gz"
  tar -xzf "$tmp/mise.tar.gz" -C "$tmp"
  install -m 755 "$tmp/mise/bin/mise" "$KIT/bin/mise"
  rm -rf "$tmp"
  mark mise "$MISE_VERSION"
fi
# old kit versions put tools straight into bin/; mise owns them now
for f in "$KIT"/bin/*; do
  case "${f##*/}" in mise|kit-shell|fizsh|kit-env) ;; *) rm -f "$f" ;; esac
done
rm -f "$KIT"/opt/.versions/{zellij,eza,bat,ripgrep,fd,skim,zoxide,starship,yazi,fzf,ov,mcat,delta,btop,lazygit,uv,diff-so-fancy} 2>/dev/null || true

# --- zsh (static) ----------------------------------------------------------
if ! installed zsh "$ZSH_BIN_VERSION" || [ ! -x "$KIT/opt/zsh/bin/zsh" ]; then
  say "installing zsh (zsh-bin $ZSH_BIN_VERSION)"
  tmp=$(mktemp -d)
  fetch "https://github.com/romkatv/zsh-bin/releases/download/$ZSH_BIN_VERSION/zsh-5.8-linux-$ARCH.tar.gz" "$tmp/zsh.tar.gz"
  fetch "https://raw.githubusercontent.com/romkatv/zsh-bin/$ZSH_BIN_VERSION/install" "$tmp/install"
  rm -rf "$KIT/opt/zsh"
  sh "$tmp/install" -f "$tmp/zsh.tar.gz" -d "$KIT/opt/zsh" -e no -q
  rm -rf "$tmp"
  mark zsh "$ZSH_BIN_VERSION"
fi

# --- neovim ----------------------------------------------------------------
if ! installed nvim "$NVIM_VERSION" || [ ! -x "$KIT/opt/nvim/bin/nvim" ]; then
  say "installing neovim $NVIM_VERSION"
  narch=$([ "$ARCH" = x86_64 ] && echo x86_64 || echo arm64)
  tmp=$(mktemp -d)
  fetch "https://github.com/neovim/neovim-releases/releases/download/v$NVIM_VERSION/nvim-linux-$narch.tar.gz" "$tmp/nvim.tar.gz"
  rm -rf "$KIT/opt/nvim"; mkdir -p "$KIT/opt/nvim"
  tar -xzf "$tmp/nvim.tar.gz" -C "$KIT/opt/nvim" --strip-components=1
  rm -rf "$tmp"
  mark nvim "$NVIM_VERSION"
fi

# --- configs, shell files, kitty bits --------------------------------------
echo "$MODE" > "$KIT/mode"
if [ "$MODE" = remote ]; then
  say "syncing configs"
  # remove only files an earlier push put there and that are gone now; anything else the server
  # keeps in ~/.local/kit/config (gh login, copilot, ...) stays
  (cd "$P/config" && find . \( -type f -o -type l \) | LC_ALL=C sort) > "$KIT/state/kit-config.new"
  if [ -f "$KIT/state/kit-config.list" ]; then
    LC_ALL=C comm -23 "$KIT/state/kit-config.list" "$KIT/state/kit-config.new" |
      while IFS= read -r f; do rm -f "$KIT/config/$f"; done
  fi
  rm -f "$KIT/config/mise/config.toml"     # generated by older kits; now $KIT/mise.toml
  rsync -a "$P/config/" "$KIT/config/"
  mv "$KIT/state/kit-config.new" "$KIT/state/kit-config.list"
  rsync -a "$P/fizsh/" "$KIT/fizsh/"          # no --delete: keeps history, zcompdump
fi
rsync -a --delete "$P/share/" "$KIT/share/"
install -m 755 "$P/remote/kit-shell" "$KIT/bin/kit-shell"
install -m 755 "$P/remote/kit-shell" "$KIT/bin/fizsh"
install -m 644 "$P/remote/kit-env"   "$KIT/bin/kit-env"

# shellcheck source=/dev/null
. "$KIT/bin/kit-env"

# tools (mise reads $KIT/mise.toml, generated from packages.json)
say "installing tools"
: > "$KIT/state/kit-mise.log"

# does a command really run on this machine? (catches "GLIBC_2.xx not found" and friends)
runs() {
  local out rc
  command -v "$1" >/dev/null 2>&1 || return 1
  out=$("$1" --version 2>&1 </dev/null); rc=$?
  [ $rc -ne 126 ] && [ $rc -ne 127 ] && ! printf '%s' "$out" | grep -qE 'GLIBC_[0-9]|GLIBCXX|not found \(required|cannot execute|Exec format'
}

# Fallbacks for tools whose download can't run on this system (usually: system too old):
#   1. the same release's musl build (runs on any Linux), found in the project's release list
#   2. the package's build-from-source recipe (linux_fallback in packages.json)
# Swaps are remembered per machine, so later pushes go straight to what works here.
MCONF="$KIT/mise.toml"
cp "$P/mise.toml" "$MCONF"
SWAPS="$KIT/state/kit-swaps"
touch "$SWAPS"

set_tool() {  # replace the config line for tool key $1 with line $2
  sed -i "s#^\"$1\" = .*#$2#" "$MCONF"
}
apply_swaps() {
  local key line
  while IFS=$'\t' read -r key line; do [ -n "$key" ] && set_tool "$key" "$line"; done < "$SWAPS"
}
remember_swap() {  # key line
  grep -v "^$1	" "$SWAPS" > "$SWAPS.tmp" || true
  printf '%s\t%s\n' "$1" "$2" >> "$SWAPS.tmp" && mv "$SWAPS.tmp" "$SWAPS"
}
mise_sync() {
  MISE_YES=1 "$KIT/bin/mise" install </dev/null >>"$KIT/state/kit-mise.log" 2>&1 || true
  "$KIT/bin/mise" reshim >/dev/null 2>&1 || true
}
musl_asset() {  # owner/repo version -> name of a musl release asset for this arch
  local json="" tag
  for tag in "v$2" "$2"; do
    json=$(curl -fsSL --connect-timeout 15 </dev/null "https://api.github.com/repos/$1/releases/tags/$tag" 2>/dev/null) && break
  done
  printf '%s' "$json" | grep -oE '"name": *"[^"]+"' | sed 's/.*"\([^"]*\)"$/\1/' \
    | grep -iE "musl" | grep -iE "$ARCH_RE" | grep -E "\.(tar\.gz|tar\.xz|tgz|zip)$" | head -1 || true
}

apply_swaps
mise_sync
while read -r name bin primary fallback <&3; do
  [ -n "$name" ] || continue
  runs "$bin" && continue
  key="${primary%@*}" ver="${primary##*@}"
  case "$primary" in
    aqua:*|github:*)
      repo=$(printf '%s' "${key#*:}" | cut -d/ -f1-2)
      asset=$(musl_asset "$repo" "$ver")
      if [ -n "$asset" ]; then
        say "$name: the download doesn't run on this system, using its musl build ($asset)"
        "$KIT/bin/mise" uninstall "$primary" "github:$repo@$ver" >/dev/null 2>&1 || true
        line="\"github:$repo\" = { version = \"$ver\", asset_pattern = \"$asset\" }"
        set_tool "$key" "$line"
        mise_sync
        if runs "$bin"; then remember_swap "$key" "$line"; continue; fi
      fi ;;
  esac
  if [ "$fallback" != - ]; then
    say "$name: building it from source ($fallback, one time, a few minutes)"
    case "$fallback" in
      cargo:*) grep -q '^"core:rust"' "$MCONF" || printf '"core:rust" = "stable"\n' >> "$MCONF" ;;
    esac
    line="\"${fallback%@*}\" = \"${fallback##*@}\""
    set_tool "$key" "$line"
    mise_sync
    if runs "$bin"; then remember_swap "$key" "$line"; continue; fi
  fi
  warn "$name: no build of it runs on this system (log: $KIT/state/kit-mise.log)"
done 3< "$P/packages.list"

# git checkouts the shell needs (externals.json: antidote, ...); on your own machine kit sync does this
[ "$MODE" = remote ] && while read -r rel url ref; do
  [ -n "$rel" ] || continue
  dest="$KIT/fizsh/${rel#.fizsh/}"
  case "$rel" in .config/*) dest="$KIT/config/${rel#.config/}" ;; esac
  [ -e "$dest" ] && continue
  say "cloning $url"
  git -c advice.detachedHead=false clone -q --depth 1 ${ref:+--branch "$ref"} "$url" "$dest" || warn "clone failed: $url"
done < "$P/externals.list"

# zellij: pre-grant the stack-cycle plugin (zellij keys this by the plugin's absolute path)
mkdir -p "$XDG_CACHE_HOME/zellij"
wasm="$XDG_CONFIG_HOME/zellij/plugins/stack-cycle.wasm"
perms="$XDG_CACHE_HOME/zellij/permissions.kdl"
if [ -f "$wasm" ] && ! grep -qF "\"$wasm\"" "$perms" 2>/dev/null; then
  printf '"%s" {\n    ReadApplicationState\n    ChangeApplicationState\n    RunActionsAsUser\n}\n' "$wasm" >> "$perms"
fi

# bat: compile the GitHub Dark theme
bat cache --build >/dev/null 2>&1 || warn "bat cache --build failed"

# shell plugins: let antidote clone/update them now instead of on first login
say "fetching shell plugins"
"$KIT/bin/kit-shell" -i -c 'exit' </dev/null >/dev/null 2>&1 || warn "first shell start reported errors"

# ncurses (static, inside the kit) when the system lacks the headers: needed to build cbonsai
if ! echo '#include <curses.h>' | cc -E - >/dev/null 2>&1 && [ ! -f "$KIT/opt/ncurses/lib/libncursesw.a" ]; then
  say "building ncurses $NCURSES_VERSION (no system headers, about a minute)"
  tmp=$(mktemp -d)
  if fetch "https://ftp.gnu.org/gnu/ncurses/ncurses-$NCURSES_VERSION.tar.gz" "$tmp/nc.tar.gz" \
     && tar -xzf "$tmp/nc.tar.gz" -C "$tmp" \
     && (cd "$tmp/ncurses-$NCURSES_VERSION" \
         && ./configure --prefix="$KIT/opt/ncurses" --enable-widec --without-shared --with-normal \
              --without-debug --without-ada --without-cxx --without-cxx-binding --without-manpages \
              --without-progs --without-tests --with-default-terminfo-dir=/usr/share/terminfo \
              --with-terminfo-dirs="/etc/terminfo:/lib/terminfo:/usr/share/terminfo" \
         && make -j"$(nproc 2>/dev/null || echo 2)" && make install.libs install.includes) >"$tmp/build.log" 2>&1; then
    :
  else
    warn "ncurses build failed (log: $tmp/build.log); the nvim dashboard tree will be skipped"
  fi
fi

# nvim: plugins, language servers and syntax parsers now, so the first start is clean
nvim_step() {  # label seconds nvim-args...
  local label=$1 secs=$2; shift 2
  say "nvim: $label"
  timeout "$secs" nvim --headless "$@" +qa >"$KIT/state/kit-nvim-$label.log" 2>&1 \
    || warn "nvim: $label did not finish in ${secs}s (it continues on first open; log: $KIT/state/kit-nvim-$label.log)"
}
nvim_step plugins 600 "+Lazy! restore"
nvim_step language-servers 900 "+silent! MasonToolsInstallSync"
# mason ships prebuilt tools too; where one can't run here, point it at the kit's working copy
for tool in tree-sitter; do
  mbin="$XDG_DATA_HOME/nvim/mason/bin/$tool"
  if [ -e "$mbin" ] && ! runs "$mbin" && runs "$tool"; then
    ln -sfn "$(command -v "$tool")" "$mbin"
    say "nvim: using the kit's $tool (mason's build can't run on this system)"
  fi
done
nvim_step parsers 600 "+lua pcall(function() require('nvim-treesitter').install(require('astrocore').config.treesitter.ensure_installed):wait(540000) end)"
[ -x "$KIT/config/nvim/bin/cbonsai.sh" ] && bash "$KIT/config/nvim/bin/cbonsai.sh" --help >/dev/null 2>&1 || true

# --- login hook (servers only; KIT_HOOK=0 on your own Linux machine) ----
if [ "${KIT_HOOK:-1}" = 1 ]; then bash "$P/remote/hook.sh" install; fi

# --- report: everything that should be here, actually running? ---------------
failed=""
total=0
while read -r name bin primary fallback; do
  [ -n "$name" ] || continue
  total=$((total + 1))
  runs "$bin" || failed="$failed $name"
done < "$P/packages.list"
for core in zsh nvim; do total=$((total + 1)); runs "$core" || failed="$failed $core"; done

echo "$(cat "$P/.stamp")" > "$KIT/.stamp"
if [ -n "$failed" ]; then
  warn "$(( total - $(echo $failed | wc -w) ))/$total tools work; NOT working:$failed"
  warn "logs: $KIT/state/kit-*.log"
  exit 1
fi
if [ "$MODE" = remote ]; then say "all $total tools work — next ssh login drops you into your environment"
else say "all $total tools work"; fi
