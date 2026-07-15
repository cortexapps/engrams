#!/usr/bin/env bash
# ADR 0096: stage a CURRENT bootable test rootfs for the live VZ e2e —
# Docker-free (macOS dev machines and the Docker-less macOS CI runner).
#
#   1. Download + sha256-verify the pinned Alpine minirootfs (a complete
#      ~3 MB arm64 busybox userland; cached like pull-kernel.sh).
#   2. Extract it to a FRESH tree (rebuilt every run, so the rootfs can
#      never silently rot against HEAD the way the old `just bake-demo`
#      leftovers did).
#   3. Cross-build the `vz-e2e-echo` guest test helper (same
#      aarch64-unknown-linux-musl toolchain the agentd bundle uses).
#   4. `engram-mk-test-rootfs`: inject the real prod init shim
#      (ADR 0080 contract — agentd rides its bundle slot, NOT the
#      rootfs) and pack a deterministic ext4 via mkext4 (ADR 0093).
#
# Usage: make-test-rootfs.sh <out.ext4>
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

OUT="${1:?usage: make-test-rootfs.sh <out.ext4>}"

# Pinned Alpine minirootfs (arm64). Bump deliberately: update BOTH the
# version and the sha (from https://alpinelinux.org/downloads/ or the
# adjacent .sha256 file), and re-run `just vz-e2e`.
ALPINE_VERSION=3.22.2
ALPINE_SHA256=6bf491907b705caa5fc65773fbbc1d0954530b6c455d4446f670a4c1fdcea489
ALPINE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION%.*}/releases/aarch64/alpine-minirootfs-${ALPINE_VERSION}-aarch64.tar.gz"

CACHE_DIR="${HOME}/.cache/engram-vz-test"
TARBALL="${CACHE_DIR}/alpine-minirootfs-${ALPINE_VERSION}-aarch64.tar.gz"

sha256_of() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1;
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

mkdir -p "${CACHE_DIR}"
if [ ! -f "${TARBALL}" ] || [ "$(sha256_of "${TARBALL}")" != "${ALPINE_SHA256}" ]; then
    echo "downloading Alpine ${ALPINE_VERSION} arm64 minirootfs..."
    curl -fSL --retry 3 -o "${TARBALL}.tmp" "${ALPINE_URL}"
    got="$(sha256_of "${TARBALL}.tmp")"
    if [ "$got" != "${ALPINE_SHA256}" ]; then
        echo "make-test-rootfs: sha256 mismatch for ${ALPINE_URL}" >&2
        echo "  expected ${ALPINE_SHA256}" >&2
        echo "  got      ${got}" >&2
        rm -f "${TARBALL}.tmp"
        exit 1
    fi
    mv "${TARBALL}.tmp" "${TARBALL}"
fi

# Pinned Alpine packages layered onto the tree (an .apk is a tar.gz):
# iptables + its libs, so the live e2e can exercise the ADR 0096 D6
# in-guest egress redirect (the init shim degrades to a warning when
# an image carries no iptables). Musl-linked — fine here, the tree IS
# an Alpine userland.
APKS="iptables-1.8.11-r1 libxtables-1.8.11-r1 libip4tc-1.8.11-r1 libmnl-1.0.5-r2 libnftnl-1.2.9-r0"
apk_sha() {
    case "$1" in
        iptables-1.8.11-r1)  echo 0a10fe634e3525082a1219487cd044d987ac4a55ed5aa551bf758e81223e1cfb ;;
        libxtables-1.8.11-r1) echo e84f0d6b69d4318f297056d00ccbe433e6c7d163fe6767405e373248c42e3e88 ;;
        libip4tc-1.8.11-r1)  echo 6418c50ff5287f6aca02ba7d8baab7cfe729a898793c9a8856cf8975b3f759c2 ;;
        libmnl-1.0.5-r2)     echo 213a7e87553bed3d9159b2e74d2627885c259883e61714b357949ca806eb1f8d ;;
        libnftnl-1.2.9-r0)   echo 6912a5d56b31d3365b8dd0d6339bd70a2bfc9e25dab0c387f77174113c43e664 ;;
    esac
}
for pkg in ${APKS}; do
    f="${CACHE_DIR}/${pkg}.apk"
    if [ ! -f "$f" ] || [ "$(sha256_of "$f")" != "$(apk_sha "$pkg")" ]; then
        curl -fSL --retry 3 -o "$f.tmp" \
            "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/aarch64/${pkg}.apk"
        got="$(sha256_of "$f.tmp")"
        [ "$got" = "$(apk_sha "$pkg")" ] || {
            echo "make-test-rootfs: sha256 mismatch for ${pkg}.apk" >&2
            rm -f "$f.tmp"; exit 1
        }
        mv "$f.tmp" "$f"
    fi
done

# Fresh tree every run. Extract as the current user (no mknod entries in
# the minirootfs, so no root needed); ownership in the packed image is
# whatever the host files carry — the guest runs as root, which reads
# 0755 regardless, so uid fidelity is irrelevant for the e2e.
TREE="$(dirname "${OUT}")/.rootfs-tree"
rm -rf "${TREE}"
mkdir -p "${TREE}"
tar -xzf "${TARBALL}" -C "${TREE}"
for pkg in ${APKS}; do
    # .apk = tar.gz; strip the .PKGINFO/.SIGN control entries.
    tar -xzf "${CACHE_DIR}/${pkg}.apk" -C "${TREE}" --exclude '.*' 2>/dev/null || true
done

# Guest test helper for the relay/HOL e2e (see the example's header for
# why this replaces socat).
echo "cross-building vz-e2e-echo (aarch64-unknown-linux-musl)..."
cargo build --release --target aarch64-unknown-linux-musl -p engram-agentd --example vz-e2e-echo

mkdir -p "$(dirname "${OUT}")"
cargo run --release -p engram-rootfs-materializer --bin engram-mk-test-rootfs -- \
    --tree "${TREE}" \
    --out "${OUT}" \
    --install "vz-e2e-echo=target/aarch64-unknown-linux-musl/release/examples/vz-e2e-echo"

rm -rf "${TREE}"
ls -lh "${OUT}"
