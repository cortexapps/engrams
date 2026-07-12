#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: stage.sh <x86_64|aarch64> <wrapper-bin> <out-dir>}"
wrapper="${2:?usage: stage.sh <x86_64|aarch64> <wrapper-bin> <out-dir>}"
out="${3:?usage: stage.sh <x86_64|aarch64> <wrapper-bin> <out-dir>}"
version=0.144.1
release="rust-v$version"
asset="codex-package-${arch}-unknown-linux-musl.tar.gz"
base="https://github.com/openai/codex/releases/download/$release"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL --retry 3 "$base/$asset" -o "$tmp/$asset"
curl -fsSL --retry 3 "$base/codex-package_SHA256SUMS" -o "$tmp/SHA256SUMS"
expected="$(awk -v asset="$asset" '$2 == asset { print $1 }' "$tmp/SHA256SUMS")"
[[ -n "$expected" ]] || { echo "official checksum missing $asset" >&2; exit 1; }
if command -v sha256sum >/dev/null; then
    actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
else
    actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
fi
[[ "$actual" == "$expected" ]] || { echo "checksum mismatch for $asset" >&2; exit 1; }

mkdir "$tmp/package"
tar -xzf "$tmp/$asset" -C "$tmp/package"
rm -rf "$out"
mkdir -p "$out"
cp -a "$tmp/package/." "$out/"
cp -p "$wrapper" "$out/harness"
cp -p "$out/bin/codex" "$out/codex"
cp -p "$(dirname "$0")/harness.toml" "$out/harness.toml"
chmod +x "$out/harness" "$out/codex"
host_arch="$(uname -m)"
if [[ "$(uname -s)" == Linux ]] \
    && { [[ "$arch" == x86_64 && "$host_arch" == x86_64 ]] \
        || [[ "$arch" == aarch64 && "$host_arch" == aarch64 ]]; }; then
    "$(dirname "$0")/check-app-server-schema.sh" "$out/codex"
else
    echo "staged $asset; schema execution deferred to its native-architecture CI gate" >&2
fi
