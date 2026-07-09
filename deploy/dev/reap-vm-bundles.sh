#!/usr/bin/env bash
# Prune stale RO skill bundles inside the fc-colima VM (ADR 0082), mirroring
# `just reap-sessions`'s var/shared prune. Runs IN the VM — piped to
# `sudo bash -s` over `colima ssh` (stdin, so colima ssh's non-shell arg
# handling can't mangle it). Removes any <sha>.{erofs,squashfs} in
# /opt/engram-dev/shared NOT referenced by the live current.json stamp; a
# VZ→FC switch strands the whole .erofs set, which alone was ~9 GiB on the
# small VM disk. No current.json -> nothing is "live", so skip (don't nuke).
set -euo pipefail
d=/opt/engram-dev/shared
[ -f "$d/current.json" ] || { echo "  (VM: no $d/current.json — skip)"; exit 0; }
keep="$(grep -oE '[0-9a-f]{64}' "$d/current.json" | sort -u || true)"
# Fail closed on an empty keep-set, like reap-vm-snapshots.sh does for its
# live set: current.json exists but parsed to zero shas (corrupt / torn
# write) — an empty `keep` would make the loop below rm EVERY bundle.
if [ -z "$keep" ]; then
    echo "  VM: refusing to prune — $d/current.json parsed to an EMPTY sha set" >&2
    echo "      (corrupt or torn stamp?). Pruning now would delete every bundle." >&2
    echo "      Aborting; re-run the bundles build and retry." >&2
    exit 1
fi
shopt -s nullglob
pruned=0
before=$(du -sk "$d" | awk '{print $1}')
for f in "$d"/*.erofs "$d"/*.squashfs; do
    sha="$(basename "$f")"; sha="${sha%.*}"
    printf '%s\n' "$keep" | grep -qx "$sha" && continue
    rm -f "$f"; pruned=$((pruned+1))
done
after=$(du -sk "$d" | awk '{print $1}')
echo "  VM: pruned $pruned stale bundle(s), freed $(( (before - after) / 1024 )) MiB from $d"
