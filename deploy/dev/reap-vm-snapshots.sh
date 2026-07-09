#!/usr/bin/env bash
# Sweep orphaned base-snapshot dirs in the fc-colima VM (ADR 0082). Dirs under
# /opt/engram-dev/var/sandboxes/snapshots/ whose id has no live coordinator DB
# row are dead weight: the coordinator's snapshot GC works off DB rows, so a
# snapshot dir left by a hard DB delete or a failed capture is never reclaimed,
# and its (~session-mem-budget-sized, i.e. GiB) memory dump fills the small VM
# disk. `just reap-sessions` gathers the live snapshot-id set from Postgres and
# passes it as $1 (SPACE-separated — colima ssh mangles multi-line args);
# anything on disk not in that set is swept. Piped to `sudo bash -s -- "<ids>"`.
set -euo pipefail
live="${1:-}"
# Fail closed on an empty live set. The caller gathers the live snapshot ids
# from Postgres; if that query fails (DB unreachable, wrong docker context, a
# schema change) it yields an empty string — and an empty `live` makes the loop
# below treat EVERY dir as orphaned and rm -rf the lot (each a GiB-sized memory
# dump). An empty set is never legitimate here (the enabled images alone pin
# their base snapshots), so refuse rather than nuke everything.
if [ -z "${live//[[:space:]]/}" ]; then
    echo "  VM: refusing to sweep — live snapshot set is EMPTY (the coordinator DB" >&2
    echo "      query returned nothing / failed). Sweeping now would delete every" >&2
    echo "      base snapshot. Aborting; fix the query and re-run." >&2
    exit 1
fi
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
