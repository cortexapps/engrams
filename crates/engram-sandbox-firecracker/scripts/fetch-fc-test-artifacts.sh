#!/usr/bin/env bash
# Fetch Firecracker test artifacts (kernel + ext4 rootfs) into a local
# cache, then print `export FC_TEST_KERNEL=...` lines suitable for
# `eval $(...)`. Idempotent: subsequent runs reuse the cache.
#
# Used by the boot smoke test:
#   crates/engram-sandbox-firecracker/tests/boot.rs
#
# Run on the Linux dev VM:
#   eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
#   cargo test -p engram-sandbox-firecracker --test boot -- --ignored --nocapture
#
# Override the cache directory with $ENGRAM_FC_CACHE.
set -euo pipefail

cache="${ENGRAM_FC_CACHE:-$HOME/.cache/engram-fc-test}"
mkdir -p "$cache"

# Guest KERNEL: engram's own build (ADR 0025) — stock FC microvm config +
# deploy/kernel/engram-docker.fragment (nf_tables + raw table for in-guest
# Docker), published as a private GitHub release asset. Fetched with `gh`
# (the dev-vm is authed; CI's GITHUB_TOKEN can read its own releases). Keep
# the tag/asset in sync with deploy/kernel/build-fc-kernel.sh.
# ROOTFS: the standard public Ubuntu demo image from Firecracker's CI bucket
# (used only by the boot smoke test; nothing Docker-specific).
KERNEL_REPO="${ENGRAM_KERNEL_REPO:-cortexapps/engrams}"
KERNEL_TAG="${ENGRAM_KERNEL_TAG:-fc-kernel-6.1.102-1}"
KERNEL_ASSET="${ENGRAM_KERNEL_ASSET:-vmlinux-engram-6.1.102-1}"
ROOTFS_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/ubuntu-22.04.ext4"

KERNEL="$cache/$KERNEL_ASSET"
ROOTFS="$cache/$(basename "$ROOTFS_URL")"

if [ ! -s "$KERNEL" ]; then
  echo "fetching $KERNEL_ASSET via gh release ($KERNEL_TAG)..." >&2
  gh release download "$KERNEL_TAG" --repo "$KERNEL_REPO" --pattern "$KERNEL_ASSET" --output "$KERNEL" --clobber
fi
# The Tiltfile's FC default looks for an unversioned `vmlinux`; keep a symlink
# so `just dev` and the boot test agree on one cached kernel.
ln -sfn "$KERNEL" "$cache/vmlinux"

if [ ! -s "$ROOTFS" ]; then
  echo "fetching $(basename "$ROOTFS")..." >&2
  # --fail turns 4xx/5xx into a non-zero exit so we don't end up with an
  # HTML 404 page on disk masquerading as a rootfs.
  curl -L --fail --silent --show-error "$ROOTFS_URL" -o "$ROOTFS.tmp"
  mv "$ROOTFS.tmp" "$ROOTFS"
fi

# Punch holes where the image is all-zero (idempotent, ~1s). The ext4 image
# ships with substantial zeroed free space; a holey cache copy lets the
# per-test `cp --sparse=always` clones (tests/common `clone_rootfs`) skip the
# holes via SEEK_DATA/SEEK_HOLE on both the read and the write side — each of
# the ~20 FC tests that clones this image stops paying a full-size copy.
# Guest-visible content is unchanged (holes read back as the zeros they were).
# Best-effort: without fallocate the copies still work, just unsparsified.
if command -v fallocate >/dev/null 2>&1; then
  fallocate --dig-holes "$ROOTFS" 2>/dev/null \
    || echo "note: fallocate --dig-holes unsupported here; rootfs stays dense" >&2
fi

# Print export lines on stdout (anything diagnostic went to stderr above).
echo "export FC_TEST_KERNEL=$KERNEL"
echo "export FC_TEST_ROOTFS=$ROOTFS"
