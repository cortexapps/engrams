#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: stage.sh <x86_64|aarch64> <wrapper-bin> <out-dir>}"
wrapper="${2:?usage: stage.sh <x86_64|aarch64> <wrapper-bin> <out-dir>}"
out="${3:?usage: stage.sh <x86_64|aarch64> <wrapper-bin> <out-dir>}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

"$(dirname "$0")/fetch-codex.sh" "$arch" "$tmp/package"
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
    echo "staged Codex; schema execution deferred to its native-architecture CI gate" >&2
fi
