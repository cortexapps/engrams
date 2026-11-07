#!/bin/sh
# Set the block I/O scheduler of each whole disk under the filesystem that
# holds IO_DIR. A partition resolves to its disk; an md or device-mapper
# device resolves to its member disks. Idempotent: safe on every boot.
# Always exits 0: the scheduler is a performance setting, and a node must
# come up without it. Runs in the host mount namespace.
#
# Environment:
#   IO_SCHEDULER  the scheduler to set (none); mq-deadline when a disk
#                 does not offer it; empty sets nothing
#   IO_DIR        a directory on the filesystem (/var/lib/engram)
#   SYS           the sysfs root (/sys)
set -u

IO_SCHEDULER=${IO_SCHEDULER-none}
IO_DIR=${IO_DIR:-/var/lib/engram}
SYS=${SYS:-/sys}

log() { echo "[io-scheduler] $*"; }
warn() { echo "[io-scheduler] WARNING: $*" >&2; }

[ -n "$IO_SCHEDULER" ] || exit 0

majmin=$(findmnt -no MAJ:MIN -T "$IO_DIR" 2>/dev/null | head -n1 | tr -d ' ')
dev=
[ -n "$majmin" ] && dev=$(readlink -f "$SYS/dev/block/$majmin" 2>/dev/null)
if [ -z "$dev" ] || [ ! -d "$dev" ]; then
    warn "no block device under $IO_DIR (findmnt gave '$majmin'); scheduler not set"
    exit 0
fi

# Set the scheduler on one whole disk ($1, its sysfs directory).
set_disk() {
    disk=$(basename "$1")
    f="$1/queue/scheduler"
    if [ ! -r "$f" ]; then
        warn "$disk has no scheduler file; scheduler not set"
        return
    fi
    before=$(cat "$f")
    case "$before" in
        *"[$IO_SCHEDULER]"*)
            log "$disk: $before (already $IO_SCHEDULER)"
            return
            ;;
    esac
    offered=" $(echo "$before" | tr -d '[]') "
    pick=
    for s in "$IO_SCHEDULER" mq-deadline; do
        case "$offered" in
            *" $s "*) pick=$s; break ;;
        esac
    done
    if [ -z "$pick" ]; then
        warn "$disk offers neither $IO_SCHEDULER nor mq-deadline ($before); scheduler not set"
        return
    fi
    if ! (echo "$pick" > "$f") 2>/dev/null; then
        warn "$disk: cannot write $pick to $f ($before); scheduler not set"
        return
    fi
    log "$disk: $before -> $(cat "$f")"
}

# Walk from the filesystem's device down to its whole disks, one level of
# md or device-mapper members at a time. Sysfs paths contain no spaces.
level=$dev
while [ -n "$level" ]; do
    next=
    for d in $level; do
        [ -e "$d/partition" ] && d=$(dirname "$d")
        leaf=true
        for m in "$d"/slaves/*; do
            [ -e "$m" ] || continue
            leaf=false
            next="$next $(readlink -f "$m")"
        done
        if $leaf; then set_disk "$d"; fi
    done
    level=$next
done
exit 0
