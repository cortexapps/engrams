#!/bin/sh
# Every state node prep's thin-pool.sh can find on a node, constructed on a
# small loop-mounted filesystem so free space is under the test's control.
# Needs root, device-mapper, thin-provisioning-tools, and a running udev. Run from the repo
# root: sudo sh scripts/thin-pool-states-test.sh
set -eu

SCRIPT=deploy/helm/engram-host-fleet/files/thin-pool.sh
WORK=$(mktemp -d)
IMG="$WORK/fs.img"
MNT="$WORK/mnt"
POOL=engram-pool-states
FS_LOOP=""

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok - $*"; }

teardown_pool() {
    dmsetup remove --retry sentinel 2>/dev/null || true
    dmsetup remove --retry "$POOL" 2>/dev/null || true
    for f in "$MNT/thin/metadata.img" "$MNT/thin/data.img"; do
        for dev in $(losetup -j "$f" 2>/dev/null | cut -d: -f1); do
            losetup -d "$dev" || true
        done
    done
}

cleanup() {
    teardown_pool
    umount "$MNT" 2>/dev/null || true
    [ -n "$FS_LOOP" ] && losetup -d "$FS_LOOP" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

# Run the script as node prep does: the chart's environment, and an IPC
# namespace of its own (node prep enters only the host's mount namespace),
# so a step that waits on udev hangs here too. The timeout turns that hang
# into a failure. Output goes to $WORK/out.
prep() {
    timeout 120 unshare --ipc env POOL_NAME="$POOL" POOL_DIR="$MNT/thin" POOL_DATA_SIZE=50% \
        POOL_META_SIZE=32M POOL_CACHE_DIR="$MNT/cache" sh "$SCRIPT" > "$WORK/out" 2>&1
}

fresh_fs() {
    teardown_pool
    rm -rf "$MNT/thin" "$MNT/cache" "$MNT/filler"
    mkdir -p "$MNT/cache"
}

truncate -s 1G "$IMG"
mkfs.ext4 -q -F "$IMG"
FS_LOOP=$(losetup --show -f "$IMG")
mkdir -p "$MNT"
mount "$FS_LOOP" "$MNT"

# 1. A fresh disk builds the pool.
fresh_fs
prep || { cat "$WORK/out"; fail "a fresh disk did not build the pool"; }
dmsetup status "$POOL" | grep -q thin-pool || fail "no pool after a fresh build"
[ -b "/dev/mapper/$POOL" ] || fail "no device node for the pool"
pass "a fresh disk builds the pool"

# 2. A chunk cache that holds the space is cleared, and the pool builds.
fresh_fs
dd if=/dev/zero of="$MNT/cache/chunk" bs=1M count=700 status=none
prep || { cat "$WORK/out"; fail "a cache-filled disk did not build the pool"; }
grep -q "clearing the chunk cache" "$WORK/out" || fail "the cache was not cleared"
[ -d "$MNT/cache" ] && [ -z "$(ls -A "$MNT/cache")" ] || fail "the cache directory is not empty"
dmsetup status "$POOL" | grep -q thin-pool || fail "no pool after clearing the cache"
pass "a full chunk cache is cleared to make room"

# 3. A chunk cache that leaves room for the pool files but not for the
#    reserve is also cleared: a node left near full reports disk pressure
#    and admits no host-agent.
fresh_fs
total_k=$(df -Pk "$MNT" | awk 'NR==2 {print $2}')
avail_k=$(df -Pk "$MNT" | awk 'NR==2 {print $4}')
cache_m=$(( (avail_k - total_k / 2 - 32 * 1024 - total_k / 20) / 1024 ))
dd if=/dev/zero of="$MNT/cache/chunk" bs=1M count="$cache_m" status=none
prep || { cat "$WORK/out"; fail "a near-full disk did not build the pool"; }
grep -q "clearing the chunk cache" "$WORK/out" || fail "the cache was not cleared on a near-full disk"
left_k=$(df -Pk "$MNT" | awk 'NR==2 {print $4}')
[ "$left_k" -ge $(( total_k * 15 / 100 )) ] || fail "the pool left less than the reserve free"
pass "a cache that leaves no reserve is cleared"

# 4. Too little room even without the cache: a clear failure, nothing left
#    half-made, and a clean build once the space is free.
fresh_fs
dd if=/dev/zero of="$MNT/filler" bs=1M count=700 status=none
if prep; then fail "a disk without room built a pool"; fi
grep -q "FATAL: .* too little free space" "$WORK/out" || fail "no clear message"
[ ! -e "$MNT/thin/data.img" ] || fail "a partial data file is left"
[ ! -e "$MNT/thin/metadata.img" ] || fail "a metadata file is left for a pool that was never built"
rm -f "$MNT/filler"
prep || { cat "$WORK/out"; fail "the retry after freeing space failed"; }
pass "a disk without room fails clearly and retries cleanly"

# 5. A metadata file a crash left before the first activation (all zeros)
#    is fresh, not corrupt.
fresh_fs
mkdir -p "$MNT/thin"
dd if=/dev/zero of="$MNT/thin/metadata.img" bs=1M count=32 status=none
prep || { cat "$WORK/out"; fail "a zeroed metadata file stopped the build"; }
dmsetup status "$POOL" | grep -q thin-pool || fail "no pool over a zeroed metadata file"
pass "a zeroed metadata file is formatted, not rejected"

# 6. A restart re-activates the pool after thin_check, data intact.
fresh_fs
prep
dmsetup message "$POOL" 0 "create_thin 7"
dmsetup create sentinel --table "0 65536 thin /dev/mapper/$POOL 7"
dd if=/dev/urandom of=/dev/mapper/sentinel bs=1M count=16 oflag=direct status=none
before=$(sha256sum /dev/mapper/sentinel | cut -d' ' -f1)
teardown_pool
prep || { cat "$WORK/out"; fail "re-activation failed"; }
grep -q "thin_check" "$WORK/out" || fail "re-activation skipped thin_check"
dmsetup create sentinel --table "0 65536 thin /dev/mapper/$POOL 7"
after=$(sha256sum /dev/mapper/sentinel | cut -d' ' -f1)
[ "$before" = "$after" ] || fail "a volume's bytes changed across re-activation"
pass "a restart re-activates the pool with its data"

# 7. An active pool is left alone.
prep || fail "a run against an active pool failed"
grep -q "is active" "$WORK/out" || fail "an active pool was not recognised"
pass "an active pool is left alone"
