#!/usr/bin/env bash
# Sweep orphaned base-snapshot dirs in the fc-colima VM (ADR 0068). Dirs under
# /opt/engram-dev/var/sandboxes/snapshots/ whose id has no live coordinator DB
# row are dead weight: the coordinator's snapshot GC works off DB rows, so a
# snapshot dir left by a hard DB delete or a failed capture is never reclaimed,
# and its (~session-mem-budget-sized, i.e. GiB) memory dump fills the small VM
# disk. `just reap-sessions` gathers the live snapshot-id set from Postgres and
# passes it as $1 (SPACE-separated — colima ssh mangles multi-line args);
# anything on disk not in that set is swept. Piped to `sudo bash -s -- "<ids>"`.
set -euo pipefail
live="${1:-}"
d=/opt/engram-dev/var/sandboxes/snapshots
[ -d "$d" ] || { echo "  (VM: no $d — skip)"; exit 0; }
shopt -s nullglob
swept=0
before=$(du -sk "$d" 2>/dev/null | awk '{print $1}')
for dir in "$d"/*/; do
    id="$(basename "$dir")"
    # shellcheck disable=SC2086  # word-split the space-separated live set
    printf '%s\n' $live | grep -qx "$id" && continue
    rm -rf "$dir"
    swept=$((swept + 1))
done
after=$(du -sk "$d" 2>/dev/null | awk '{print $1}')
echo "  VM: swept $swept orphaned snapshot dir(s), freed $(( (before - after) / 1024 )) MiB from $d"
