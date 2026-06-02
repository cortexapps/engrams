#!/usr/bin/env bash
# ADR 0027: build the fleet-wide `skills` RO bundle.
#
# The skills bundle is just our own wrapper scripts + SKILL.md — the
# unpacked tree IS this directory (bin/ + skills/). This packs it into a
# squashfs for the FC host to mount, with executable bits on the wrappers.
#
# Usage:
#   build.sh <out.squashfs>     # pack to squashfs (CI / FC host bake)
#   build.sh --stage <dir>      # copy the unpacked tree to <dir> (dev)
#
# Prints the sha256 of the squashfs on the pack path.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

stage_tree() {
    local dest="$1"
    mkdir -p "$dest"
    cp -R "$here/bin" "$here/skills" "$dest/"
    chmod 0755 "$dest/bin/"*
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    rm -rf "$dest"
    stage_tree "$dest"
    echo "staged skills bundle tree -> $dest"
    exit 0
fi

out="${1:?usage: build.sh <out.squashfs> | --stage <dir>}"
command -v mksquashfs >/dev/null || {
    echo "mksquashfs not found (install squashfs-tools)" >&2
    exit 1
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
stage_tree "$tmp"

rm -f "$out"
# -all-root: every file owned by root in the image (the guest mounts RO as
# root). -no-xattrs / reproducible-ish for a stable digest.
mksquashfs "$tmp" "$out" -comp zstd -all-root -noappend -no-xattrs >/dev/null

sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
