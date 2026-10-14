#!/usr/bin/env bash
# Reconcile the desired environment. Every tool gets its own result and log; retries are safe.
set -euo pipefail
umask 077
KIT="${KIT:-$HOME/.local/kit}"
P="${KIT_PAYLOAD:-$KIT/payload}"
MODE=$(cat "$P/mode")
OS=$(uname -s)
case "$(uname -m)" in x86_64|amd64) ARCH=x86_64 ;; aarch64|arm64) ARCH=aarch64 ;; *) echo 'kit: unsupported CPU' >&2; exit 1 ;; esac
say() { printf '› %s\n' "$*"; }
warn() { printf '! %s\n' "$*" >&2; }
mkdir -p "$KIT"/{bin,opt,state,cache,share,config/mise}
# Prevent concurrent pushes from mixing payload/config/package updates.
lock="$KIT/state/setup.lock"
if ! mkdir "$lock" 2>/dev/null; then
  owner=$(cat "$lock/pid" 2>/dev/null || true)
  if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then warn 'another setup is running'; exit 1; fi
  rm -rf "$lock"; mkdir "$lock"
fi
echo $$ > "$lock/pid"
if [ "$P" != "$KIT/payload" ]; then
  rm -rf "$KIT/payload.next"
  cp -R "$P" "$KIT/payload.next"
  rm -rf "$KIT/payload.old"
  [ ! -d "$KIT/payload" ] || mv "$KIT/payload" "$KIT/payload.old"
  mv "$KIT/payload.next" "$KIT/payload"
  P="$KIT/payload"
fi
tmp=$(mktemp -d); trap 'rm -rf "$tmp" "$lock"' EXIT
bash "$P/remote/bootstrap.sh"
for name in kit-env kit-shell; do install -m 755 "$P/remote/$name" "$KIT/bin/$name"; done
install -m 755 "$P/remote/kit-shell" "$KIT/bin/fizsh"
echo "$MODE" > "$KIT/mode"
. "$KIT/bin/kit-env"
cd "$KIT"   # do not pick up a project's unrelated mise config
cp "$P/mise.toml" "$KIT/mise.toml"
MISE="$KIT/bin/mise"
"$MISE" trust "$KIT/mise.toml" >/dev/null 2>&1 || true
fetch() { curl -fsSL --retry 3 --connect-timeout 20 --max-time 300 -o "$2" "$1"; }
failed=0
report="$KIT/state/setup-results.tsv"
printf 'tool\tresult\trecipe\n' > "$report"
record() { printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$report"; }
# A failed install is an error even if an older command happens to be on PATH.
install_spec() { "$MISE" install "$1" >> "$2" 2>&1 && "$MISE" reshim >> "$2" 2>&1; }
runs() {
  local out rc
  command -v "$1" >/dev/null 2>&1 || return 1
  local limit
  limit=$(command -v timeout || command -v gtimeout || true)
  if [ -n "$limit" ]; then out=$("$limit" 15 "$1" --version </dev/null 2>&1) && rc=0 || rc=$?
  else out=$("$1" --version </dev/null 2>&1) && rc=0 || rc=$?; fi
  [ "$rc" -ne 124 ] && [ "$rc" -ne 126 ] && [ "$rc" -ne 127 ] && ! printf '%s' "$out" | grep -qE 'GLIBC_[0-9]|GLIBCXX|Exec format|cannot execute|mise ERROR|No version is set for shim'
}
# Source/package backends need runtimes explicitly; mise does not add them automatically.
ensure_runtime() {
  local spec=$1 log=$2 key
  key=${spec%@*}
  if grep -qF "\"$key\" = " "$KIT/mise.toml"; then install_spec "$key" "$log"
  else "$MISE" use --global "$spec" >>"$log" 2>&1 && "$MISE" reshim >>"$log" 2>&1; fi
}
dependency() {
  local spec=$1 log=$2
  case "$spec" in
    cargo:*) ensure_runtime core:rust@stable "$log" ;;
    npm:*) ensure_runtime core:node@lts "$log" ;;
    go:*) ensure_runtime core:go@latest "$log" ;;
    pypi:*) ensure_runtime aqua:astral-sh/uv@latest "$log" && ensure_runtime core:python@3.12 "$log" ;;
    gem:*) ensure_runtime core:ruby@latest "$log" ;;
    *) return 0 ;;
  esac
}
system_install() {
  local pkg=$1
  local -a elevate=()
  if [ "$(id -u)" != 0 ]; then
    command -v sudo >/dev/null && sudo -n true || { warn "system:$pkg requires administrator access; install it with your distro's package manager, then retry"; return 1; }
    elevate=(sudo -n)
  fi
  if command -v apt-get >/dev/null; then ${elevate[@]+"${elevate[@]}"} apt-get update -qq && ${elevate[@]+"${elevate[@]}"} apt-get install -y "$pkg"
  elif command -v dnf >/dev/null; then ${elevate[@]+"${elevate[@]}"} dnf install -y "$pkg"
  elif command -v yum >/dev/null; then ${elevate[@]+"${elevate[@]}"} yum install -y "$pkg"
  elif command -v apk >/dev/null; then ${elevate[@]+"${elevate[@]}"} apk add "$pkg"
  elif command -v pacman >/dev/null; then ${elevate[@]+"${elevate[@]}"} pacman -S --needed --noconfirm "$pkg"
  elif command -v zypper >/dev/null; then ${elevate[@]+"${elevate[@]}"} zypper --non-interactive install "$pkg"
  else warn "no supported package manager for system:$pkg"; return 1; fi
}
# Install shell independently of the package list. Replace a working installation only after validation.
if [ "$OS" = Linux ]; then
  if ! "$KIT/opt/zsh/bin/zsh" -fc true >/dev/null 2>&1; then
    say 'installing portable zsh'
    if fetch "https://github.com/romkatv/zsh-bin/releases/download/v6.1.1/zsh-5.8-linux-$ARCH.tar.gz" "$tmp/zsh.tar.gz" \
       && fetch https://raw.githubusercontent.com/romkatv/zsh-bin/v6.1.1/install "$tmp/install" \
       && sh "$tmp/install" -f "$tmp/zsh.tar.gz" -d "$tmp/zsh" -e no -q \
       && "$tmp/zsh/bin/zsh" -fc true; then
      rm -rf "$KIT/opt/zsh.old"; [ ! -e "$KIT/opt/zsh" ] || mv "$KIT/opt/zsh" "$KIT/opt/zsh.old"
      mv "$tmp/zsh" "$KIT/opt/zsh"
    else warn 'portable zsh failed; existing shell preserved'; failed=1; fi
  fi
fi
# Per-package fallback state is keyed by the complete original recipe, including its version.
swaps="$KIT/state/package-swaps.tsv"; touch "$swaps"
replace_tool() {
  local key=$1 line=$2
  awk -v key="\"$key\" = " -v line="$line" 'index($0,key)==1 {$0=line} {print}' "$KIT/mise.toml" > "$tmp/config"
  mv "$tmp/config" "$KIT/mise.toml"
}
musl_asset() {
  local repo=$1 version=$2 names tag arch_re
  if [ "$ARCH" = x86_64 ]; then arch_re='x86_64|amd64|x64'; else arch_re='aarch64|arm64'; fi
  for tag in "v$version" "$version"; do
    names=$(curl -fsSL --connect-timeout 15 --max-time 30 "https://github.com/$repo/releases/expanded_assets/$tag" 2>/dev/null |
      grep -oE '/releases/download/[^" ]+' | sed 's#.*/##') || continue
    printf '%s\n' "$names" | grep -i musl | grep -iE "$arch_re" | grep -E '^[A-Za-z0-9_.+-]+\.(tar\.gz|tar\.xz|tgz|zip)$' | head -1
    return
  done
}
while IFS=$'\t' read -r name bin primary fallback; do
  [ -n "$name" ] || continue
  log="$KIT/state/install-$name.log"; : > "$log"
  # Keep working tools installed outside kit. Updates use their existing provider.
  case "$primary" in builtin:*|system:*|cmd:*|missing:*) ;; *)
    existing=$(command -v "$bin" || true)
    case "$existing" in ""|"$KIT"/*) ;; *)
      if runs "$bin"; then say "$name: using existing $existing"; record "$name" available "external:$existing"; continue; fi
      ;; esac
    ;; esac
  say "installing $name ($primary)"
  case "$primary" in
    missing:*) warn "$name has no recipe for this platform; kit add $name --linux <recipe>"; failed=1; record "$name" missing "$primary"; continue ;;
    builtin:*)
      if [ "$primary" = builtin:fizsh ]; then
        if ! bash "$P/remote/install-fizsh.sh" >>"$log" 2>&1; then
          warn "$name failed: $log"; failed=1; record "$name" failed "$primary"; continue
        fi
      fi
      if [ "$primary" = builtin:nvim ]; then install_spec aqua:neovim/neovim@0.12.6 "$log" || { failed=1; record "$name" failed "$primary"; continue; }; fi
      case "${primary#builtin:}" in fizsh) [ -x "$KIT/opt/fizsh/bin/fizsh" ] ;; zsh) runs zsh ;; nvim) runs nvim ;; mise) runs mise ;; *) false ;; esac && record "$name" installed "$primary" || { failed=1; record "$name" failed "$primary"; }
      continue ;;
    system:*)
      if [ "$primary" = system:coreutils ] && [ "$bin" = gtimeout ] && command -v timeout >/dev/null; then ln -sf "$(command -v timeout)" "$KIT/bin/gtimeout"; fi
      if command -v "$bin" >/dev/null || { system_install "${primary#system:}" >>"$log" 2>&1 && command -v "$bin" >/dev/null; }; then record "$name" installed "$primary"
      else warn "$name failed: $log"; failed=1; record "$name" failed "$primary"; fi
      continue ;;
    cmd:*)
      script="$P/commands/$name.sh"
      if grep -q 'uv tool' "$script"; then
        if ! ensure_runtime aqua:astral-sh/uv@latest "$log" || ! ensure_runtime core:python@3.12 "$log"; then failed=1; record "$name" failed "$primary"; continue; fi
      fi
      stamp="$KIT/state/command-$name.sha256"
      current=$(shasum -a 256 "$script" 2>/dev/null || sha256sum "$script")
      current=${current%% *}
      if [ "$(cat "$stamp" 2>/dev/null || true)" = "$current" ] && command -v "$bin" >/dev/null; then record "$name" installed "$primary"; continue; fi
      if bash "$script" >>"$log" 2>&1 && runs "$bin"; then printf '%s\n' "$current" > "$stamp"; record "$name" installed "$primary"
      else warn "$name failed: $log"; failed=1; record "$name" failed "$primary"; fi
      continue ;;
  esac
  effective=$(awk -F '\t' -v spec="$primary" '$1==spec { result=$2 } END {print result}' "$swaps")
  effective=${effective:-$primary}
  if [ "$effective" != "$primary" ]; then
    replace_tool "${primary%@*}" "# selected machine fallback"
    "$MISE" use --global "$effective" >>"$log" 2>&1 || true
  fi
  ok=0
  if dependency "$effective" "$log" && install_spec "$effective" "$log" && runs "$bin"; then ok=1; fi
  if [ "$ok" = 0 ] && [ "$OS" = Linux ]; then
    case "$primary" in
      aqua:*|github:*)
        repo=${primary#*:}; repo=${repo%@*}; ver=${primary##*@}
        asset=$(musl_asset "$repo" "$ver") || asset=""
        if [ -n "$asset" ]; then
          candidate="github:${repo}[asset_pattern=$asset]@$ver"
          say "$name: trying portable musl build"
          replace_tool "${primary%@*}" "\"github:$repo\" = { version = \"$ver\", asset_pattern = \"$asset\" }"
          if install_spec "$candidate" "$log" && runs "$bin"; then ok=1; effective=$candidate; fi
        fi ;;
    esac
  fi
  if [ "$ok" = 0 ] && [ "$fallback" != - ]; then
    say "$name: trying source fallback $fallback"
    # Do not retain the primary version in the desired config: it may have a broken shim.
    replace_tool "${primary%@*}" "# selected source fallback"
    case "$primary" in aqua:*|github:*) replace_tool "github:$repo" "# selected source fallback" ;; esac
    "$MISE" use --global "$fallback" >>"$log" 2>&1 || true
    if dependency "$fallback" "$log" && install_spec "$fallback" "$log" && runs "$bin"; then
      ok=1; effective=$fallback
      awk -F '\t' -v spec="$primary" '$1!=spec' "$swaps" > "$tmp/swaps"
      printf '%s\t%s\n' "$primary" "$fallback" >> "$tmp/swaps"; mv "$tmp/swaps" "$swaps"
    fi
  fi
  if [ "$ok" = 1 ]; then
    if [ "$effective" != "$primary" ]; then
      awk -F '\t' -v spec="$primary" '$1!=spec' "$swaps" > "$tmp/swaps"
      printf '%s\t%s\n' "$primary" "$effective" >> "$tmp/swaps"; mv "$tmp/swaps" "$swaps"
    fi
    record "$name" installed "$effective"
  else warn "$name failed: $log (compiler/library dependencies may be required for source builds)"; failed=1; record "$name" failed "$effective"; fi
done < "$P/packages.list"
# All home-relative dotfiles travel, including ~/.zshrc and ~/.gitconfig.
if [ "$MODE" = remote ]; then bash "$P/remote/deploy.sh" || failed=1; fi
# External repositories update refs on subsequent pushes as well as first installation.
while IFS=$'\t' read -r rel url ref; do
  [ -n "$rel" ] || continue
  dest="$HOME/$rel"
  if [ ! -e "$dest" ]; then
    args=(git clone -q --depth 1); [ -z "$ref" ] || args+=(--branch "$ref")
    "${args[@]}" -- "$url" "$dest" || { warn "external clone failed: $rel"; failed=1; }
  elif [ -d "$dest/.git" ] && [ "$(git -C "$dest" config --get remote.origin.url)" != "$url" ]; then
    warn "external $rel has a different origin; left untouched"; failed=1
  elif [ -d "$dest/.git" ] && [ -n "$ref" ]; then
    if ! git -C "$dest" fetch -q --depth 1 origin "$ref" || ! git -C "$dest" checkout -q FETCH_HEAD; then warn "external update failed: $rel"; failed=1; fi
  elif [ ! -d "$dest/.git" ]; then warn "external path already exists: $rel"; failed=1; fi
done < "$P/externals.list"
cp -R "$P/share/." "$KIT/share/"
# Set up the shell even on a new account without ~/.bashrc; tracked rc files are safe to hook.
bash "$P/remote/hook.sh" install || failed=1
# Warm owned shell/editor configuration before the first login. Optional failures have logs.
command -v bat >/dev/null && bat cache --build >/dev/null 2>&1 || true
LIMIT=$(command -v timeout || command -v gtimeout || true)
if [ -n "$LIMIT" ] && [ -f "$ZDOTDIR/.zshrc" ]; then
  say 'warming shell configuration'
  "$LIMIT" 180 "$KIT/bin/kit-shell" -i -c exit </dev/null > "$KIT/state/shell-init.log" 2>&1 \
    || warn 'shell configuration needs attention (state/shell-init.log)'
fi
if [ -f "$HOME/.config/nvim/lazy-lock.json" ] && [ -n "$LIMIT" ]; then
  say 'warming editor plugins and language tools'
  "$LIMIT" 300 nvim --headless '+Lazy! restore' +qa > "$KIT/state/nvim-plugins.log" 2>&1 || warn 'nvim plugins need attention (state/nvim-plugins.log)'
  "$LIMIT" 300 nvim --headless '+silent! MasonToolsInstallSync' +qa > "$KIT/state/nvim-language-tools.log" 2>&1 || warn 'nvim language tools need attention (state/nvim-language-tools.log)'
  "$LIMIT" 300 nvim --headless "+lua pcall(function() local c=require('astrocore').config.treesitter; require('nvim-treesitter').install(c.ensure_installed):wait(240000) end)" +qa > "$KIT/state/nvim-parsers.log" 2>&1 || warn 'nvim parsers need attention (state/nvim-parsers.log)'
fi
if ! "$KIT/bin/kit-shell" --ok; then warn 'kit shell cannot start'; failed=1; fi
if [ "$failed" = 0 ]; then cp "$P/.stamp" "$KIT/.stamp"; say 'environment ready'
else warn "setup incomplete; results: $report. Retry kit sync / kit push after correcting failures."; fi
exit "$failed"
