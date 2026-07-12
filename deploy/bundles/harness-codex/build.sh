#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tree="${1:?usage: build.sh <staged-tree-dir> <out.squashfs>}"
out="${2:?usage: build.sh <staged-tree-dir> <out.squashfs>}"

command -v mksquashfs >/dev/null || { echo "mksquashfs not found" >&2; exit 1; }
[[ -x "$tree/harness" ]] || { echo "missing executable harness" >&2; exit 1; }
[[ -x "$tree/codex" ]] || { echo "missing executable codex" >&2; exit 1; }

# shellcheck source=../_pack.sh
. "$here/../_pack.sh"
pack_squashfs "$tree" "$out"
sha256sum "$out" | cut -d' ' -f1
