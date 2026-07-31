#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: fetch-codex.sh <x86_64|aarch64> <out-dir>}"
out="${2:?usage: fetch-codex.sh <x86_64|aarch64> <out-dir>}"
version=0.146.0
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

rm -rf "$out"
mkdir -p "$out"
tar -xzf "$tmp/$asset" -C "$out"
