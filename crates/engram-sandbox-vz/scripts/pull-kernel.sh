#!/usr/bin/env bash
# Fetch the arm64 Linux kernel that VZ uses to boot Linux guests.
#
# Source (ADR 0096, superseding the Kata static kernel): engram's OWN
# kernel build (ADR 0025/0082) — the same vendored microvm config +
# engram-docker.fragment prod's FC guests boot, built for arm64
# (`deploy/kernel/build-fc-kernel.sh ARCH=arm64`) and published as a
# GitHub release asset by .github/workflows/build-fc-kernel.yml. One
# owned config for both backends: squashfs bundles (the erofs fork is
# retired), the in-guest netfilter stack, FUSE, and no more silent
# config drift between dev guests and prod guests.
#
# Fetched with `gh` (dev Macs are authed; CI's GITHUB_TOKEN can read
# its own releases) — the repo is private, so unlike the old Kata
# tarball this needs auth. Keep TAG/ASSET in sync with
# deploy/kernel/build-fc-kernel.sh.
#
# License: Linux is GPLv2.

set -euo pipefail

CACHE_DIR="${HOME}/.cache/engram-vz-test"
# Kept at the historical filename — VzConfig's default kernel path,
# the Tiltfile, and CI all point here. (It's a raw arm64 boot Image,
# which VZLinuxBootLoader accepts directly.)
DST="${CACHE_DIR}/vmlinux-arm64"

KERNEL_REPO="${ENGRAM_KERNEL_REPO:-cortexapps/engrams}"
KERNEL_TAG="${ENGRAM_KERNEL_TAG:-fc-kernel-6.1.102-1}"
KERNEL_ASSET="${ENGRAM_VZ_KERNEL_ASSET:-Image-engram-6.1.102-1}"

mkdir -p "${CACHE_DIR}"

if [ -s "${DST}" ]; then
    echo "kernel already cached at ${DST}"
    ls -lh "${DST}"
    exit 0
fi

echo "fetching ${KERNEL_ASSET} via gh release (${KERNEL_TAG})..."
gh release download "${KERNEL_TAG}" --repo "${KERNEL_REPO}" \
    --pattern "${KERNEL_ASSET}" --output "${DST}.tmp" --clobber
gh release download "${KERNEL_TAG}" --repo "${KERNEL_REPO}" \
    --pattern "${KERNEL_ASSET}.sha256" --output "${DST}.sha256.tmp" --clobber

want="$(cut -d' ' -f1 "${DST}.sha256.tmp")"
got="$(if command -v sha256sum >/dev/null; then sha256sum "${DST}.tmp"; else shasum -a 256 "${DST}.tmp"; fi | cut -d' ' -f1)"
if [ "$want" != "$got" ]; then
    echo "pull-kernel: sha256 mismatch for ${KERNEL_ASSET} (want ${want}, got ${got})" >&2
    rm -f "${DST}.tmp" "${DST}.sha256.tmp"
    exit 1
fi
rm -f "${DST}.sha256.tmp"
mv "${DST}.tmp" "${DST}"
echo "cached ${DST}"
ls -lh "${DST}"
