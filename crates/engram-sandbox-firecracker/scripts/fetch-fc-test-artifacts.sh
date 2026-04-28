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

# Pinned to a known-good combination from Firecracker's CI bucket.
# Kernel is the one their own integration tests run against; the
# Ubuntu rootfs is the standard demo image. Both are public.
FC_CI_BASE="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64"
KERNEL_URL="$FC_CI_BASE/vmlinux-5.10.223"
ROOTFS_URL="$FC_CI_BASE/ubuntu-22.04.ext4"

KERNEL="$cache/$(basename "$KERNEL_URL")"
ROOTFS="$cache/$(basename "$ROOTFS_URL")"

fetch() {
  local url="$1" dest="$2"
  if [ -s "$dest" ]; then
    return 0
  fi
  echo "fetching $(basename "$dest")..." >&2
  # --fail turns 4xx/5xx into a non-zero exit so we don't end up with
  # an HTML 404 page on disk masquerading as a kernel.
  curl -L --fail --silent --show-error "$url" -o "$dest.tmp"
  mv "$dest.tmp" "$dest"
}

fetch "$KERNEL_URL" "$KERNEL"
fetch "$ROOTFS_URL" "$ROOTFS"

# Print export lines on stdout (anything diagnostic went to stderr above).
echo "export FC_TEST_KERNEL=$KERNEL"
echo "export FC_TEST_ROOTFS=$ROOTFS"
