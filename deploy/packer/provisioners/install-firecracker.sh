#!/usr/bin/env bash
# Install Firecracker at /usr/local/bin/firecracker.
# Pins to the tag in $FC_VER (defaults to v1.10.1).

set -euo pipefail

FC_VER="${FC_VER:-v1.10.1}"
ARCH="$(uname -m)"

case "$ARCH" in
    x86_64)
        ;;
    aarch64)
        ;;
    *)
        echo "unsupported arch for Firecracker: $ARCH" >&2
        exit 1
        ;;
esac

URL="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VER}/firecracker-${FC_VER}-${ARCH}.tgz"

echo "==> downloading firecracker ${FC_VER} (${ARCH})"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fSL "$URL" -o "$tmp/fc.tgz"
tar -xzf "$tmp/fc.tgz" -C "$tmp"

# Release tarballs unpack to release-${FC_VER}-${ARCH}/ on x86_64;
# verify before installing.
bin_path="$(find "$tmp" -type f -name "firecracker-${FC_VER}-${ARCH}" | head -1)"
if [ -z "$bin_path" ]; then
    echo "could not locate firecracker binary in extracted tarball" >&2
    ls -la "$tmp" >&2
    exit 1
fi

sudo install -m 0755 "$bin_path" /usr/local/bin/firecracker
echo "==> firecracker installed: $(/usr/local/bin/firecracker --version 2>&1 | head -1)"
