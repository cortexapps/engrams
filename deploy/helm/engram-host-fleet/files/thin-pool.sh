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
#   POOL_RESERVE       free space left after the pool files are created
#                      (15%); below the kubelet's eviction threshold the
#                      node admits no pods, the host-agent included
#   POOL_CACHE_DIR     the host-agent's chunk cache; cleared to make room
#                      when the pool files are first created on a used disk
set -eu

POOL_NAME=${POOL_NAME:-engram-pool}
POOL_DIR=${POOL_DIR:-/var/lib/engram/thin}
POOL_DATA_SIZE=${POOL_DATA_SIZE:-50%}
POOL_META_SIZE=${POOL_META_SIZE:-8G}
POOL_BLOCK_SECTORS=${POOL_BLOCK_SECTORS:-128}
POOL_LOW_WATER=${POOL_LOW_WATER:-32768}
POOL_RESERVE=${POOL_RESERVE:-15%}

META="$POOL_DIR/metadata.img"
DATA="$POOL_DIR/data.img"

log() { echo "[thin-pool] $*"; }

# The host agent serves the source a resuming session's disk fills from
# as a ublk device; the module is loaded on every start, pool or not.
modprobe ublk_drv 2>/dev/null || true

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

# A size in KiB: a percentage of the filesystem holding POOL_DIR, or a
# number with a K, M, G, or T suffix.
total_kib=$(df -Pk "$POOL_DIR" | awk 'NR==2 {print $2}')
kib() {
    n=${1%?}
    case "$1" in
        *%) echo $(( total_kib * n / 100 )) ;;
        *K) echo "$n" ;;
        *M) echo $(( n * 1024 )) ;;
        *G) echo $(( n * 1024 * 1024 )) ;;
        *T) echo $(( n * 1024 * 1024 * 1024 )) ;;
        *) log "FATAL: size $1 has no unit" >&2; exit 1 ;;
    esac
}
avail_kib() { df -Pk "$POOL_DIR" | awk 'NR==2 {print $4}'; }
data_kib=$(kib "$POOL_DATA_SIZE")
meta_kib=$(kib "$POOL_META_SIZE")
reserve_kib=$(kib "$POOL_RESERVE")

# The pool files are created once. They must leave the reserve free, or
# the node reports disk pressure and admits no host-agent. On a used disk
# the chunk cache may hold that space; the cache refills from the chunk
# store, and no host-agent runs while node-prep does.
need_kib=$reserve_kib
[ -e "$DATA" ] || need_kib=$(( need_kib + data_kib ))
[ -e "$META" ] || need_kib=$(( need_kib + meta_kib ))
if [ "$need_kib" -gt "$reserve_kib" ] && [ "$(avail_kib)" -lt "$need_kib" ]; then
    if [ -n "${POOL_CACHE_DIR:-}" ] && [ -d "$POOL_CACHE_DIR" ]; then
        log "the pool files would leave less than $POOL_RESERVE free; clearing the chunk cache $POOL_CACHE_DIR"
        rm -rf -- "$POOL_CACHE_DIR"
        mkdir -p "$POOL_CACHE_DIR"
    fi
    if [ "$(avail_kib)" -lt "$need_kib" ]; then
        log "FATAL: $POOL_DIR has too little free space for the pool files and a $POOL_RESERVE reserve"
        df -Pk "$POOL_DIR" >&2
        exit 1
    fi
fi
if [ ! -e "$DATA" ]; then
    log "creating $DATA (${data_kib}K)"
    fallocate -l "${data_kib}K" "$DATA" || { rm -f "$DATA"; exit 1; }
fi
# A metadata file whose superblock is still zero was never activated:
# the kernel formats it on the first create, so it is fresh, not corrupt.
if [ ! -e "$META" ]; then
    log "creating $META (${meta_kib}K)"
    fallocate -l "${meta_kib}K" "$META" || { rm -f "$META"; exit 1; }
fi
fresh=0
if [ "$(head -c 4096 "$META" | tr -d '\000' | wc -c)" -eq 0 ]; then
    fresh=1
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
