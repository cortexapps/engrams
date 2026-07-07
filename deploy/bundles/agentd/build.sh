#!/usr/bin/env bash
# ADR 0080: pack `engram-agentd` into the reproducible RO squashfs that rides
# the host `current_bundles` stamp under `agentd` (reserved slot dyn_1). The
# guest's stage-1 init copies `engram-agentd` out of this mount to tmpfs and
# execs it at boot; on a fresh-create restore the captured agentd compares the
# mount's `agentd.sha256` against its own and re-execs when they differ —
# that's how an agentd change reaches new sessions with ZERO image re-bakes
# and ZERO base-snapshot recaptures.
#
# Like harness-claude's build.sh, the content is a COMPILED ARTIFACT, not
# committed files: the caller passes the built musl `engram-agentd` binary
# (CI: the cli-tools/musl build; dev: `cargo build -p engram-agentd --target
# <arch>-unknown-linux-musl --release`).
#
# COUPLING (detect-rebake-lanes.py): this bundle's CONTENT is the
# engram-agentd source. A change to that crate (or its dep closure) MUST
# re-run publish-bundles so `bundle-agentd` is rebuilt — the detector wires
# this via `bundles |= agentd_changed`, the exact same term (and failure
# mode) as the harness: ship a new host-agent against a stale agentd bundle
# and every fresh create runs yesterday's agentd.
#
# Usage:
#   build.sh <engram-agentd-binary> <out.squashfs>
#
# Prints the sha256 of the squashfs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
agentd="${1:?usage: build.sh <engram-agentd-binary> <out.squashfs>}"
out="${2:?usage: build.sh <engram-agentd-binary> <out.squashfs>}"

command -v mksquashfs >/dev/null || {
    echo "mksquashfs not found (install squashfs-tools)" >&2
    exit 1
}
[[ -f "$agentd" ]] || {
    echo "engram-agentd binary not found at $agentd" >&2
    exit 1
}

tree="$(mktemp -d)"
trap 'rm -rf "$tree"' EXIT

install -m 0755 "$agentd" "$tree/engram-agentd"
# The content stamp the stage-1 init copies to /run/engram/agentd.sha256 and
# RefreshAgent compares against the (possibly swapped) mount.
sha256sum "$tree/engram-agentd" | cut -d' ' -f1 > "$tree/agentd.sha256"

# Reproducible content-addressed pack (shared with every bundle) — SOURCE_DATE_EPOCH=0.
# shellcheck source=../_pack.sh
. "$here/../_pack.sh"
pack_squashfs "$tree" "$out"

sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
