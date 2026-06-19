#!/usr/bin/env bash
# ADR 0055: build the tiny `sentinel` RO bundle.
#
# Every reserved dynamic-mount slot (`dyn-0..dyn-{RESERVED_SLOTS-1}`) carries
# this sentinel squashfs at base-snapshot capture, because Firecracker needs
# every drive present at `load_snapshot`. A per-session create `patch_drive`s
# the selected skill over a slot in the paused restore window; unused slots
# keep the sentinel. The guest reads the bundle's `mount.json`
# (`{"kind":"sentinel"}`) and skips it — nothing to mount or wire.
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
    cp "$here/mount.json" "$dest/"
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    rm -rf "$dest"
    stage_tree "$dest"
    echo "staged sentinel bundle tree -> $dest"
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
# -all-root: root-owned in the image (the guest mounts RO as root).
# -no-xattrs for a stable digest.
mksquashfs "$tmp" "$out" -comp zstd -all-root -noappend -no-xattrs >/dev/null

sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
