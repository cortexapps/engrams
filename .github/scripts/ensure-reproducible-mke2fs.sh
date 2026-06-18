#!/usr/bin/env bash
# Ensure a SOURCE_DATE_EPOCH-honoring mke2fs (e2fsprogs >= 1.47.1) is first on
# PATH, so engram's ext4 pack (engram-image-builder) is byte-DETERMINISTIC —
# the ADR 0036 keystone for cross-bake chunk dedup, and what the
# `ext4_pack_is_deterministic_across_rebuilds` test asserts.
#
# Why this exists: SOURCE_DATE_EPOCH (which `Mke2fsPacker` sets) was only added
# to e2fsprogs in 1.47.1. Ubuntu's apt ships <= 1.47.0, so its mke2fs silently
# stamps WALL-CLOCK times into the superblock + every inode (ctime/crtime),
# making two bakes of an identical tree differ on a second-straddle. We build
# 1.47.2 from source (idempotent, cache-friendly) and install it ahead of the
# system one in $PREFIX (default /usr/local, which precedes /usr/sbin on PATH).
#
# Idempotent: a no-op when the system (or a previously-built) mke2fs already
# honors SOURCE_DATE_EPOCH. Safe to run on every CI invocation.
set -euo pipefail

VER=1.47.2
PREFIX=${E2FSPROGS_PREFIX:-/usr/local}
# Does this mke2fs honor SOURCE_DATE_EPOCH (e2fsprogs >= 1.47.1)?
# NB: `grep -q` would exit on first match and SIGPIPE-kill `strings`, which
# under `set -o pipefail` fails the whole pipeline DESPITE the match — so let
# grep consume all of strings' output (no -q; stdout discarded).
have_epoch() { strings "$1" 2>/dev/null | grep -F SOURCE_DATE_EPOCH >/dev/null; }

existing=$(command -v mke2fs || true)
if [ -n "$existing" ] && have_epoch "$existing"; then
  echo "ensure-mke2fs: $("$existing" -V 2>&1 | head -1) already honors SOURCE_DATE_EPOCH — ok"
  exit 0
fi
if [ -x "$PREFIX/sbin/mke2fs" ] && have_epoch "$PREFIX/sbin/mke2fs"; then
  echo "ensure-mke2fs: cached $PREFIX/sbin/mke2fs honors SOURCE_DATE_EPOCH — ok"
  exit 0
fi

echo "ensure-mke2fs: system mke2fs lacks SOURCE_DATE_EPOCH; building e2fsprogs $VER -> $PREFIX"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 5 --retry-all-errors \
  "https://mirrors.edge.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v$VER/e2fsprogs-$VER.tar.gz" \
  | tar -xz -C "$tmp"
cd "$tmp/e2fsprogs-$VER"
./configure --prefix="$PREFIX" --disable-nls --disable-defrag >/dev/null
make -j"$(nproc)" >/dev/null
# mke2fs builds standalone (statically linked against the in-tree libext2fs et
# al.; only libc is dynamic — verified via ldd), and the packer invokes plain
# `mke2fs`, so install just that one binary ahead of the system one. Avoids
# `make install`, which writes udev rules + cron jobs to system paths
# (/lib/udev, /etc/cron.d) we neither need nor want to touch.
sbin="$PREFIX/sbin"
if mkdir -p "$sbin" 2>/dev/null && [ -w "$sbin" ]; then SUDO=""; else SUDO="${SUDO:-sudo}"; fi
$SUDO install -D -m 0755 misc/mke2fs "$sbin/mke2fs"
hash -r
built="$sbin/mke2fs"
have_epoch "$built" || { echo "ensure-mke2fs: FATAL — built mke2fs still lacks SOURCE_DATE_EPOCH" >&2; exit 1; }
echo "ensure-mke2fs: installed $("$built" -V 2>&1 | head -1) at $built"
