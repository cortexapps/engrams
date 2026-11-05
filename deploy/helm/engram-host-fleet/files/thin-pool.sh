#!/bin/sh
# Build or re-activate the node's dm-thin pool over two preallocated
# files attached as loop devices. Idempotent: safe on every boot.
# Exits non-zero when thin_check fails, so a node never comes up with a
# pool it cannot trust. Runs in the host mount namespace.
#
# Environment:
#   POOL_NAME          device-mapper name           (engram-pool)
#   POOL_DIR           directory for the two files  (/var/lib/engram/thin)
#   POOL_DATA_SIZE     data file size, e.g. 300G or 50% of the filesystem
#   POOL_META_SIZE     metadata file size           (8G)
#   POOL_BLOCK_SECTORS thin block size in 512-byte sectors (128 = 64 KiB)
#   POOL_LOW_WATER     low-water mark in blocks     (32768 = 2 GiB at 64 KiB)
set -eu

POOL_NAME=${POOL_NAME:-engram-pool}
POOL_DIR=${POOL_DIR:-/var/lib/engram/thin}
POOL_DATA_SIZE=${POOL_DATA_SIZE:-50%}
POOL_META_SIZE=${POOL_META_SIZE:-8G}
POOL_BLOCK_SECTORS=${POOL_BLOCK_SECTORS:-128}
POOL_LOW_WATER=${POOL_LOW_WATER:-32768}

META="$POOL_DIR/metadata.img"
DATA="$POOL_DIR/data.img"

log() { echo "[thin-pool] $*"; }

if dmsetup info "$POOL_NAME" >/dev/null 2>&1; then
    log "$POOL_NAME is active"
    dmsetup status "$POOL_NAME"
    exit 0
fi

modprobe dm_thin_pool 2>/dev/null || true
if ! dmsetup targets | grep -q '^thin-pool'; then
    log "FATAL: the kernel has no thin-pool target"
    exit 1
fi

mkdir -p "$POOL_DIR"

# A percentage is of the filesystem holding POOL_DIR.
case "$POOL_DATA_SIZE" in
    *%)
        pct=${POOL_DATA_SIZE%\%}
        total_kib=$(df -Pk "$POOL_DIR" | awk 'NR==2 {print $2}')
        POOL_DATA_SIZE="$(( total_kib * pct / 100 ))K"
        ;;
esac

fresh=0
if [ ! -e "$META" ]; then
    log "creating $META ($POOL_META_SIZE)"
    fallocate -l "$POOL_META_SIZE" "$META"
    fresh=1
fi
if [ ! -e "$DATA" ]; then
    log "creating $DATA ($POOL_DATA_SIZE)"
    fallocate -l "$POOL_DATA_SIZE" "$DATA"
fi

attach() {
    dev=$(losetup -j "$1" | cut -d: -f1 | head -n1)
    if [ -z "$dev" ]; then
        dev=$(losetup --direct-io=on --show -f "$1" 2>/dev/null || losetup --show -f "$1")
    fi
    echo "$dev"
}
META_DEV=$(attach "$META")
DATA_DEV=$(attach "$DATA")

if [ "$fresh" = 0 ]; then
    log "thin_check $META_DEV"
    thin_check "$META_DEV"
fi

data_sectors=$(( $(stat -c %s "$DATA") / 512 ))
log "dmsetup create $POOL_NAME ($DATA_DEV data, $META_DEV metadata, block $POOL_BLOCK_SECTORS sectors)"
# Node prep shares the host's mount namespace but not its IPC namespace,
# so udev can never release dmsetup's sync semaphore: create without the
# wait and make the device node here.
dmsetup create --noudevsync "$POOL_NAME" --table \
    "0 $data_sectors thin-pool $META_DEV $DATA_DEV $POOL_BLOCK_SECTORS $POOL_LOW_WATER 0"
dmsetup mknodes "$POOL_NAME"
dmsetup status "$POOL_NAME"
