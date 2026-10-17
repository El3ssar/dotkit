#!/usr/bin/env bash
# Clone branches, tags or exact full commit IDs without exposing a partial checkout.
set -euo pipefail
url=$1 dest=$2 ref=${3:-}
[ ! -e "$dest" ] && [ ! -L "$dest" ] || { echo 'kit: external destination already exists' >&2; exit 1; }
mkdir -p "$(dirname "$dest")"
tmp=$(mktemp -d "${dest}.kit-clone.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
export GIT_TERMINAL_PROMPT=0
export GIT_SSH_COMMAND="${GIT_SSH_COMMAND:-ssh -o BatchMode=yes -o ConnectTimeout=15}"
if [[ "$ref" =~ ^[0-9a-fA-F]{40}$ ]]; then
  git init -q "$tmp"
  git -C "$tmp" remote add origin "$url"
  git -C "$tmp" fetch -q --depth 1 origin "$ref"
  actual=$(git -C "$tmp" rev-parse FETCH_HEAD)
  [ "$actual" = "$(printf '%s' "$ref" | tr 'A-F' 'a-f')" ]
  git -c advice.detachedHead=false -C "$tmp" checkout -q --detach "$actual"
else
  args=(git -c advice.detachedHead=false clone -q --depth 1)
  [ -z "$ref" ] || args+=(--branch "$ref")
  "${args[@]}" -- "$url" "$tmp"
fi
[ ! -e "$dest" ] && [ ! -L "$dest" ]
if [ "$(uname -s)" = Darwin ]; then mv -h "$tmp" "$dest"; else mv -T "$tmp" "$dest"; fi
