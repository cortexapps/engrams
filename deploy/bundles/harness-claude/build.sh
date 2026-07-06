#!/usr/bin/env bash
# ADR 0062: pack a STAGED harness-claude tree into the reproducible RO squashfs
# that rides the host `current_bundles` stamp under `harness-claude` (the built-in
# claude harness — mounted on dyn_0, exec'd as /opt/engram/dyn/0/harness).
#
# Unlike every other bundle's build.sh — which assembles its own tree (committed
# files for skills/sentinel; container-fetched binaries for browser/
# integrations-cli) — this one takes the tree as an ARGUMENT. The harness tree
# (the `harness` entry binary, the engram-harness-claude crate, + the bundled
# `claude` CLI) is produced by a separate build (the bake-harness-claude-artifact
# CI job), so the caller stages it and passes the dir in here. Both the e2e lane
# and node-assets pack through this one recipe so the bundle content-addresses to
# the same sha on every host.
#
# COUPLING (detect-rebake-lanes.py): this bundle's CONTENT is the
# engram-harness-claude source, NOT anything under deploy/bundles/. A change to
# that crate MUST re-run publish-bundles so `bundle-harness-claude` is rebuilt —
# else the fleet stages a stale harness against a freshly-rolled host-agent and
# in-guest attach fails ("attach ack read failed"). The detector wires this via
# `bundles |= harness_changed`; keep that term whenever the harness↔host wire
# evolves (the #542 wire-10 roll shipped without it and wedged new sessions).
#
# Usage:
#   build.sh <staged-tree-dir> <out.squashfs>
#
# Prints the sha256 of the squashfs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tree="${1:?usage: build.sh <staged-tree-dir> <out.squashfs>}"
out="${2:?usage: build.sh <staged-tree-dir> <out.squashfs>}"

command -v mksquashfs >/dev/null || {
    echo "mksquashfs not found (install squashfs-tools)" >&2
    exit 1
}
[[ -x "$tree/harness" ]] || {
    echo "staged tree $tree is missing an executable 'harness' entry binary" >&2
    exit 1
}

# Reproducible content-addressed pack (shared with skills/sentinel) — SOURCE_DATE_EPOCH=0.
# shellcheck source=../_pack.sh
. "$here/../_pack.sh"
pack_squashfs "$tree" "$out"

sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
