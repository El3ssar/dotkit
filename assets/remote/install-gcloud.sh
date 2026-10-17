#!/usr/bin/env bash
# Kit-owned, pinned Google release archive. No upstream installer or shell edits.
set -euo pipefail
umask 077
KIT="${KIT:-$HOME/.local/kit}"
version=589.0.0
[ "${1:-$version}" = "$version" ] || { echo 'kit: unsupported gcloud bootstrap version' >&2; exit 1; }
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux-x86_64; checksum=f865da648f6fdae2015abb5b32eaa0eb10520e88f0ca735cfb33795868c8d26f ;;
  Linux-aarch64|Linux-arm64) platform=linux-arm; checksum=3febb61a57ea76612973e40a2ff7bd79939161451d11130659d5c981201dca97 ;;
  Darwin-x86_64) platform=darwin-x86_64; checksum=8159f5dc34e4229c2fb01567ba57f9b0ea1f68dd413884448124ccc85eb4a2fb ;;
  Darwin-arm64) platform=darwin-arm; checksum=3b317feba2f2a552e9aba529a2f8d22a3b1aa77ba28a47335e03a3d26200fe64 ;;
  *) echo 'kit: unsupported gcloud platform' >&2; exit 1 ;;
esac
url="https://dl.google.com/dl/cloudsdk/channels/rapid/downloads/google-cloud-cli-$version-$platform.tar.gz"
# Used by release checks without downloading/installing anything.
if [ "${2:-}" = --describe ]; then printf '%s\t%s\n' "$url" "$checksum"; exit 0; fi
mkdir -p "$KIT/opt" "$KIT/bin"
[ ! -L "$KIT/opt/gcloud" ] && [ ! -L "$KIT/bin/gcloud" ] || { echo 'kit: symlink blocks private gcloud installation' >&2; exit 1; }
tmp=$(mktemp -d "$KIT/opt/gcloud-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
if [ -f "$KIT/opt/gcloud/kit-version" ] && [ "$(cat "$KIT/opt/gcloud/kit-version")" = "$version" ]    && [ -x "$KIT/bin/gcloud" ] && [ -x "$KIT/mise/installs/python/3.14.8/bin/python3" ]    && CLOUDSDK_CONFIG="$tmp/probe-config" "$KIT/bin/gcloud" --version >/dev/null 2>&1; then
  exit 0
fi
if command -v curl >/dev/null; then curl -fsSL --retry 3 --connect-timeout 20 --max-time 300 "$url" -o "$tmp/sdk.tar.gz"
else wget -q -T 300 -O "$tmp/sdk.tar.gz" "$url"; fi
if command -v sha256sum >/dev/null; then actual=$(sha256sum "$tmp/sdk.tar.gz")
else actual=$(shasum -a 256 "$tmp/sdk.tar.gz"); fi
[ "${actual%% *}" = "$checksum" ] || { echo 'kit: Google Cloud SDK checksum mismatch' >&2; exit 1; }
tar -xzf "$tmp/sdk.tar.gz" -C "$tmp"
[ -x "$tmp/google-cloud-sdk/bin/gcloud" ]
# Always use a compatible private Python; never ask Google's installer for sudo.
export MISE_DATA_DIR="$KIT/mise" MISE_GLOBAL_CONFIG_FILE="$KIT/mise.toml" MISE_CONFIG_DIR="$KIT/config/mise" MISE_TRUSTED_CONFIG_PATHS="$KIT" MISE_YES=1
mise="$KIT/bin/mise"
[ -x "$mise" ] || mise=$(command -v mise)
cd "$KIT"
"$mise" install core:python@3.14.8
python=$("$mise" where core:python@3.14.8)/bin/python3
[ -x "$python" ]
CLOUDSDK_PYTHON="$python" CLOUDSDK_CONFIG="$tmp/probe-config" "$tmp/google-cloud-sdk/bin/gcloud" --version
printf '%s\n' "$version" > "$tmp/google-cloud-sdk/kit-version"
# Validate everything before replacing the old private SDK.
rm -rf "$KIT/opt/gcloud.previous"
[ ! -e "$KIT/opt/gcloud" ] || mv "$KIT/opt/gcloud" "$KIT/opt/gcloud.previous"
mv "$tmp/google-cloud-sdk" "$KIT/opt/gcloud"
cat > "$tmp/gcloud" <<'WRAPPER'
#!/usr/bin/env bash
set -e
KIT="${KIT:-$HOME/.local/kit}"
export CLOUDSDK_PYTHON="$KIT/mise/installs/python/3.14.8/bin/python3"
exec "$KIT/opt/gcloud/bin/gcloud" "$@"
WRAPPER
chmod 755 "$tmp/gcloud"
mv "$tmp/gcloud" "$KIT/bin/gcloud"
