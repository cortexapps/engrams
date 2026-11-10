#!/bin/sh
# Run files/io-scheduler.sh against constructed sysfs trees: a partition on
# a disk, an md array over two disks, a disk without `none`, a disk with
# neither scheduler, a disk already set, and a directory on no block device.
# Needs no root and no real disk.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
script="$here/../files/io-scheduler.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# findmnt prints FAKE_MAJMIN, or fails when it is empty.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/findmnt" <<'EOF'
#!/bin/sh
[ -n "${FAKE_MAJMIN:-}" ] || exit 1
echo "$FAKE_MAJMIN"
EOF
chmod +x "$tmp/bin/findmnt"

fail() { echo "FAIL: $*" >&2; exit 1; }

# disk NAME SCHEDULERS: a whole disk under $tmp/sys/devices.
disk() {
    mkdir -p "$tmp/sys/devices/$1/queue"
    echo "$2" > "$tmp/sys/devices/$1/queue/scheduler"
}
# link MAJ:MIN PATH: the /sys/dev/block entry for a device.
link() {
    mkdir -p "$tmp/sys/dev/block"
    ln -sfn "$tmp/sys/devices/$2" "$tmp/sys/dev/block/$1"
}
run() {
    FAKE_MAJMIN=$1 IO_SCHEDULER=${2-none} IO_DIR=/var/lib/engram SYS="$tmp/sys" \
        PATH="$tmp/bin:$PATH" sh "$script" > "$tmp/out" 2>&1 \
        || fail "the script exited non-zero: $(cat "$tmp/out")"
}
expect() {
    got=$(cat "$tmp/sys/devices/$1/queue/scheduler")
    [ "$got" = "$2" ] || fail "$1 scheduler is '$got', want '$2'"
}

# A partition resolves to its disk.
disk nvme0n1 "mq-deadline kyber [bfq] none"
mkdir -p "$tmp/sys/devices/nvme0n1/nvme0n1p1"
touch "$tmp/sys/devices/nvme0n1/nvme0n1p1/partition"
link 259:1 nvme0n1/nvme0n1p1
run 259:1
expect nvme0n1 none
grep -q "nvme0n1: mq-deadline kyber \[bfq\] none -> none" "$tmp/out" \
    || fail "no before/after line: $(cat "$tmp/out")"

# An md array resolves to each member disk.
disk nvme1n1 "[mq-deadline] none"
disk nvme2n1 "[mq-deadline] none"
mkdir -p "$tmp/sys/devices/md127/slaves" "$tmp/sys/devices/md127/queue"
echo none > "$tmp/sys/devices/md127/queue/scheduler"
ln -s "$tmp/sys/devices/nvme1n1" "$tmp/sys/devices/md127/slaves/nvme1n1"
ln -s "$tmp/sys/devices/nvme2n1" "$tmp/sys/devices/md127/slaves/nvme2n1"
link 9:127 md127
run 9:127
expect nvme1n1 none
expect nvme2n1 none

# Without `none`, mq-deadline.
disk sda "[bfq] mq-deadline"
link 8:0 sda
run 8:0
expect sda mq-deadline

# With neither, a warning and no change.
disk sdb "[bfq] kyber"
link 8:16 sdb
run 8:16
expect sdb "[bfq] kyber"
grep -q "WARNING: sdb offers neither none nor mq-deadline" "$tmp/out" \
    || fail "no warning for sdb: $(cat "$tmp/out")"

# Already set: no write.
disk sdc "mq-deadline [none]"
link 8:32 sdc
run 8:32
expect sdc "mq-deadline [none]"
grep -q "already none" "$tmp/out" || fail "no 'already' line: $(cat "$tmp/out")"

# No block device: a warning, exit 0.
run ""
grep -q "WARNING: no block device" "$tmp/out" || fail "no warning: $(cat "$tmp/out")"

# An empty scheduler sets nothing.
disk sdd "[bfq] none"
link 8:48 sdd
run 8:48 ""
expect sdd "[bfq] none"

echo "io-scheduler: all cases pass"
