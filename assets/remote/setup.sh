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
  if [ -z "$owner" ]; then warn 'another setup is acquiring its lock; retry shortly (remove state/setup.lock only if a previous setup crashed)'; exit 1; fi
  if kill -0 "$owner" 2>/dev/null; then warn 'another setup is running'; exit 1; fi
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
# Source builds are private; shell activation deliberately preserves user Rust.
export RUSTUP_HOME="$KIT/rustup" CARGO_HOME="$KIT/cargo" GIT_TERMINAL_PROMPT=0
cd "$KIT"   # do not pick up a project's unrelated mise config
cp "$P/mise.toml" "$KIT/mise.toml"
MISE="$KIT/bin/mise"
"$MISE" trust "$KIT/mise.toml" >/dev/null 2>&1 || true
fetch() {
  if command -v curl >/dev/null; then curl -fsSL --retry 3 --connect-timeout 20 --max-time 300 -o "$2" "$1"
  elif command -v wget >/dev/null; then wget -q -T 300 -O "$2" "$1"
  else warn 'install curl or wget and CA certificates first'; return 1; fi
}
failed=0
report="$KIT/state/setup-results.tsv"
printf 'tool\tresult\trecipe\n' > "$report"
record() { printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$report"; }
# A failed install is an error even if an older command happens to be on PATH.
run_install() { local log=$1; shift; bash "$P/remote/run-install.sh" "$log" "${KIT_INSTALL_TIMEOUT:-1800}" "$@"; }
install_spec() { run_install "$2" "$MISE" install "$1" && run_install "$2" "$MISE" reshim; }
runs() {
  local out rc flag limit
  command -v "$1" >/dev/null 2>&1 || return 1
  limit=$(command -v timeout || command -v gtimeout || true)
  for flag in --version --help -h; do
    if [ -n "$limit" ]; then out=$("$limit" 15 "$1" "$flag" </dev/null 2>&1) && rc=0 || rc=$?
    else out=$(bash "$P/remote/run-install.sh" "$tmp/probe.log" 15 "$1" "$flag" 2>&1) && rc=0 || rc=$?; fi
    if printf '%s' "$out" | grep -qE 'GLIBC_[0-9]|GLIBCXX|Exec format|cannot execute|mise ERROR|No version is set for shim'; then return 1; fi
    [ "$rc" != 0 ] || return 0
    # Some small CLIs implement only -h. A version flag being unsupported is
    # different from a broken program, but a help probe must then actually pass.
    case "$rc" in 1|2) ;; *) return 1 ;; esac
    printf '%s' "$out" | grep -qiE 'unknown (option|flag)|unrecognized (option|argument)|invalid option|unexpected argument' || return 1
  done
  return 1
}
# `mise reshim` can create shims for every configured package before its turn
# in the install loop. Look through the original external directories instead
# of letting an uninstalled/broken Kit shim hide an existing Cargo/Brew tool.
external_command() {
  local rest="$PATH" dir candidate
  while :; do
    dir=${rest%%:*}; dir=${dir:-.}
    case "$dir" in "$KIT"|"$KIT"/*) ;; *)
      candidate="$dir/$1"
      if [ -f "$candidate" ] && [ -x "$candidate" ]; then printf '%s\n' "$candidate"; return 0; fi
      ;; esac
    case "$rest" in *:*) rest=${rest#*:} ;; *) break ;; esac
  done
  return 1
}
# Source/package backends need runtimes explicitly; mise does not add them automatically.
ensure_runtime() {
  local spec=$1 log=$2 key
  key=${spec%@*}
  if grep -qF "\"$key\" = " "$KIT/mise.toml"; then install_spec "$key" "$log"
  else run_install "$log" "$MISE" use --global "$spec" && run_install "$log" "$MISE" reshim; fi
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
while IFS=$'\t' read -r name bin primary fallback; do
  [ -n "$name" ] || continue
  log="$KIT/state/install-$name.log"; : > "$log"
  # Keep working tools installed outside kit. Updates use their existing provider.
  case "$primary" in builtin:*|blocked:*|system:*|cmd:*) ;; *)
    existing=$(external_command "$bin" || true)
    case "$existing" in ""|"$KIT"/*) ;; *)
      if runs "$existing"; then say "$name: using existing $existing"; record "$name" available "external:$existing"; continue; fi
      ;; esac
    ;; esac
  say "installing $name ($primary); log: $log"
  case "$primary" in
    missing:*) warn "$name has no recipe for this platform; kit add $name --linux <recipe>"; failed=1; record "$name" missing "$primary"; continue ;;
    builtin:*)
      if [ "$primary" = builtin:fizsh ]; then
        if ! run_install "$log" bash "$P/remote/install-fizsh.sh"; then
          warn "$name failed: $log"; failed=1; record "$name" failed "$primary"; continue
        fi
      fi
      if [ "$primary" = builtin:nvim ]; then install_spec aqua:neovim/neovim@0.12.6 "$log" || { failed=1; record "$name" failed "$primary"; continue; }; fi
      case "${primary#builtin:}" in fizsh) [ -x "$KIT/opt/fizsh/bin/fizsh" ] ;; zsh) runs zsh ;; nvim) runs nvim ;; mise) runs mise ;; *) false ;; esac && record "$name" installed "$primary" || { failed=1; record "$name" failed "$primary"; }
      continue ;;
    brew:*|cask:*)
      if [ "$OS" != Darwin ] && [[ "$primary" = cask:* ]]; then warn "$name: casks are macOS-only"; failed=1; record "$name" blocked "$primary"; continue; fi
      if ! command -v brew >/dev/null; then warn "$name: this recipe requires Homebrew; install Homebrew or choose a mise recipe"; failed=1; record "$name" failed "$primary"; continue; fi
      if [[ "$primary" = cask:* ]]; then
        # GUI applications may have no command on PATH. Use Homebrew's explicit
        # cask ownership record rather than treating a missing CLI as failure.
        if run_install "$log" brew list --cask "${primary#*:}"            || { run_install "$log" env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew install --cask "${primary#*:}"                 && run_install "$log" brew list --cask "${primary#*:}"; }; then record "$name" installed "$primary"
        else failed=1; record "$name" failed "$primary"; fi
      else
        if { command -v "$bin" >/dev/null || run_install "$log" env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew install --formula "${primary#*:}"; } && runs "$bin"; then record "$name" installed "$primary"
        else failed=1; record "$name" failed "$primary"; fi
      fi
      continue ;;
    blocked:*|system:*|cmd:*)
      warn "$name: unsupported source; use a supported mise, Cargo or Homebrew recipe. Inspect kit packages."
      failed=1; record "$name" blocked "$primary"; continue ;;
  esac
  effective=$(awk -F '\t' -v spec="$primary" '$1==spec { result=$2 } END {print result}' "$swaps")
  effective=${effective:-$primary}
  if [ "$effective" != "$primary" ]; then
    replace_tool "${primary%@*}" "# selected machine fallback"
    run_install "$log" "$MISE" use --global "$effective" || true
  fi
  ok=0
  if dependency "$effective" "$log" && install_spec "$effective" "$log" && runs "$bin"; then ok=1; fi
  if [ "$ok" = 0 ] && [ "$fallback" != - ]; then
    say "$name: trying source fallback $fallback"
    # Do not retain the primary version in the desired config: it may have a broken shim.
    replace_tool "${primary%@*}" "# selected source fallback"
    run_install "$log" "$MISE" use --global "$fallback" || true
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
  case "$url" in ''|-*) warn "invalid external URL"; failed=1; continue ;; esac
  case "$ref" in -*) warn "invalid external reference"; failed=1; continue ;; esac
  case "$rel" in ''|/*|..|../*|*/../*|*/..|*$'\n'*|*$'\r'*) warn "invalid external path"; failed=1; continue ;; esac
  parent="$HOME"; rest="$rel"; safe=1
  while [ -n "$rest" ]; do
    part=${rest%%/*}; parent="$parent/$part"
    if [ -L "$parent" ] || { [ -e "$parent" ] && [ ! -d "$parent" ]; }; then safe=0; break; fi
    case "$rest" in */*) rest=${rest#*/} ;; *) break ;; esac
  done
  if [ "$safe" = 0 ] || [ -L "$parent/.git" ]; then warn "external parent/path blocks ~/$rel; left untouched"; failed=1; continue; fi
  dest="$HOME/$rel"
  if [ ! -e "$dest" ]; then
    args=(git clone -q --depth 1); [ -z "$ref" ] || args+=(--branch "$ref")
    run_install "$KIT/state/external-install.log" env "GIT_SSH_COMMAND=${GIT_SSH_COMMAND:-ssh -o BatchMode=yes -o ConnectTimeout=15}" "${args[@]}" -- "$url" "$dest" || { warn "external clone failed: $rel"; failed=1; }
  elif [ -d "$dest/.git" ] && [ "$(git -C "$dest" config --get remote.origin.url)" != "$url" ]; then
    warn "external $rel has a different origin; left untouched"; failed=1
  elif [ -d "$dest/.git" ] && [ -n "$ref" ]; then
    if ! git -C "$dest" diff --quiet || ! git -C "$dest" diff --cached --quiet; then
      warn "external $rel has local changes; left untouched"; failed=1
    elif ! run_install "$KIT/state/external-install.log" env "GIT_SSH_COMMAND=${GIT_SSH_COMMAND:-ssh -o BatchMode=yes -o ConnectTimeout=15}" git -C "$dest" fetch -q origin "$ref"; then
      warn "external fetch failed: $rel"; failed=1
    elif ! git -C "$dest" merge-base --is-ancestor HEAD FETCH_HEAD; then
      warn "external $rel has local commits or a divergent ref; left untouched"; failed=1
    elif ! run_install "$KIT/state/external-install.log" git -C "$dest" merge --ff-only --no-edit FETCH_HEAD; then
      warn "external update failed: $rel"; failed=1
    fi
  elif [ ! -d "$dest/.git" ]; then warn "external path already exists: $rel"; failed=1; fi
done < "$P/externals.list"
cp -R "$P/share/." "$KIT/share/"
# Set up the shell even on a new account without ~/.bashrc; tracked rc files are safe to hook.
bash "$P/remote/hook.sh" install || failed=1
# Do not run the user's shell/editor configuration during provisioning. It can
# launch installers, prompt, or start unrelated services. Login remains explicit.
if ! "$KIT/bin/kit-shell" --ok; then warn 'kit shell cannot start'; failed=1; fi
if [ "$failed" = 0 ]; then cp "$P/.stamp" "$KIT/.stamp"; say 'environment ready'
else warn "setup incomplete; results: $report. Retry kit sync / kit push after correcting failures."; fi
exit "$failed"
