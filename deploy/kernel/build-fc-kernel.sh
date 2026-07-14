#!/usr/bin/env bash
# Build the engram Firecracker guest kernel (ADR 0025; ARCH support ADR 0082).
#
# = Firecracker's stock microvm guest config (FC-bootable: virtio-mmio +
#   vsock + ip_pnp) + deploy/kernel/engram-docker.fragment (the netfilter
#   stack Docker needs in-guest). The stock FC-CI kernels — every published
#   5.10 and 6.1 binary — strip nf_tables and the `raw` table, which blocks
#   dockerd/compose inside a sandbox; this rebuild restores them.
#
# ARCH=x86_64 (default, unchanged): output $OUT/vmlinux-engram-${LINUX_VERSION}-${KERNEL_REV}
# ARCH=arm64 (ADR 0082, the Colima aarch64 dev rig): output
#   $OUT/Image-engram-${LINUX_VERSION}-${KERNEL_REV} — aarch64 Firecracker
#   boots a bare `Image`, not `vmlinux`.
# Published as a GitHub release asset by .github/workflows/build-fc-kernel.yml;
# consumed by deploy/packer/provisioners/install-fc-kernel.sh (prod host bake),
# crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh (dev),
# and deploy/dev/fc-colima-provision.sh (ARCH=arm64, built inside the VM).
#
# Reproducible: pinned linux source version + pinned Firecracker base-config
# ref + the in-repo fragment. Bump KERNEL_REV when the fragment changes.
#
# Build deps (Debian/Ubuntu): build-essential bc bison flex libelf-dev libssl-dev cpio
# arm64 build also needs: dwarves (pahole)
# Cross-compiling arm64 from an x86_64 host additionally needs a
# gcc-aarch64-linux-gnu toolchain (CROSS_COMPILE=aarch64-linux-gnu-).
set -euo pipefail

# --- pins (keep RELEASE_TAG / ASSET in sync with the consumers) ---
LINUX_VERSION="${LINUX_VERSION:-6.1.102}"
KERNEL_REV="${KERNEL_REV:-1}"                    # engram build revision; bump on fragment/base change

# ARCH selects the kernel build target: x86_64 (default, byte-for-byte
# unchanged behavior for existing callers/CI) or arm64 (ADR 0082).
ARCH="${ARCH:-x86_64}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FRAGMENT="${SCRIPT_DIR}/engram-docker.fragment"

case "$ARCH" in
  x86_64)
    # Vendored base config = Firecracker's microvm-kernel-ci-x86_64-6.1.config
    # at firecracker SHA 8a7e8a01 (their "full" microvm config, PCI/FUSE on).
    # We vendor it rather than download a release tag because the choice is
    # boot-critical: Firecracker's stripped *release* config (e.g. v1.10.1)
    # drops FUSE and other symbols and the guest boots but agentd never comes
    # up — dev-vm-verified. Re-sync from upstream deliberately (and bump
    # KERNEL_REV) when pulling driver/security updates.
    BASE_CONFIG="${SCRIPT_DIR}/microvm-kernel-ci-x86_64-6.1.config"
    ARTIFACT="vmlinux"
    ASSET="vmlinux-engram-${LINUX_VERSION}-${KERNEL_REV}"
    ;;
  arm64)
    # Same provenance as the x86_64 config above: Firecracker's
    # microvm-kernel-ci-aarch64-6.1.config at the same pinned SHA 8a7e8a01.
    BASE_CONFIG="${SCRIPT_DIR}/microvm-kernel-ci-aarch64-6.1.config"
    ARTIFACT="arch/arm64/boot/Image"
    ASSET="Image-engram-${LINUX_VERSION}-${KERNEL_REV}"
    ;;
  *)
    echo "build-fc-kernel: unsupported ARCH '$ARCH' (want x86_64 or arm64)" >&2
    exit 1
    ;;
esac

# Cross-compile only when the host arch doesn't already match the target —
# the native path (Colima's aarch64 VM building ARCH=arm64) needs no
# toolchain prefix at all. `uname -m` on Linux/arm64 hosts reports "aarch64".
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH-$ARCH" in
  x86_64-x86_64 | aarch64-arm64 | arm64-arm64)
    CROSS_COMPILE="${CROSS_COMPILE:-}"
    ;;
  *-arm64)
    CROSS_COMPILE="${CROSS_COMPILE:-aarch64-linux-gnu-}"
    ;;
  *-x86_64)
    CROSS_COMPILE="${CROSS_COMPILE:-x86_64-linux-gnu-}"
    ;;
esac
MAKE_ARGS=(ARCH="$ARCH")
[ -n "${CROSS_COMPILE:-}" ] && MAKE_ARGS+=(CROSS_COMPILE="$CROSS_COMPILE")

WORK="${WORK:-$(pwd)/kbuild}"
OUT="${OUT:-$(pwd)}"
JOBS="${JOBS:-$(nproc)}"

mkdir -p "$WORK" && cd "$WORK"

src="linux-${LINUX_VERSION}"
if [ ! -d "$src" ]; then
  echo "==> downloading linux-${LINUX_VERSION} source"
  if ! curl -fSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${LINUX_VERSION}.tar.xz" -o linux.tar; then
    # kernel.org has 404'd transiently for this version; gregkh/linux is the
    # canonical linux-stable GitHub mirror and tags releases the same way
    # (tarball unpacks to the same linux-${LINUX_VERSION}/ dir name).
    echo "==> cdn.kernel.org fetch failed, falling back to the gregkh/linux GitHub mirror" >&2
    curl -fSL "https://github.com/gregkh/linux/archive/refs/tags/v${LINUX_VERSION}.tar.gz" -o linux.tar
  fi
  tar -xf linux.tar
fi
cd "$src"

echo "==> base config ($ARCH): vendored ${BASE_CONFIG}"
cp "$BASE_CONFIG" .config

echo "==> merging ${FRAGMENT}"
./scripts/kconfig/merge_config.sh -m .config "$FRAGMENT"
make "${MAKE_ARGS[@]}" olddefconfig

# Fail loudly if the merge didn't take (Docker needs) or an FC-boot option
# regressed — a silently-wrong kernel would brick every session.
echo "==> asserting required symbols"
require() {
  grep -q "^$1=y" .config || { echo "MISSING required config: $1" >&2; exit 1; }
}
# FUSE: stripped by FC's release config -> agentd never boots (dev-vm-verified).
# NAMESPACES/USER_NS/PID_NS/NET_NS/SECCOMP*: required for chromium's
# unprivileged namespace (zygote) sandbox (ADR 0065 §7, issue #569) — a future
# base-config re-sync must not silently drop them.
# VIRTIO_BALLOON: the ADR 0088 addendum's capture-time seed shrink inflates a
# balloon before the cold-base dump; a guest without the driver silently
# degrades every warm enable back to a dense multi-GiB seed upload.
for c in \
  CONFIG_NF_TABLES CONFIG_NFT_COMPAT CONFIG_NFT_NAT \
  CONFIG_IP_NF_RAW CONFIG_IP6_NF_NAT CONFIG_IP6_NF_RAW \
  CONFIG_BRIDGE_NETFILTER CONFIG_VXLAN CONFIG_OVERLAY_FS CONFIG_BRIDGE CONFIG_VETH \
  CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_BLK CONFIG_VIRTIO_NET CONFIG_VIRTIO_VSOCKETS \
  CONFIG_VIRTIO_BALLOON \
  CONFIG_IP_PNP CONFIG_EXT4_FS \
  CONFIG_FUSE_FS \
  CONFIG_NAMESPACES CONFIG_USER_NS CONFIG_PID_NS CONFIG_NET_NS \
  CONFIG_SECCOMP CONFIG_SECCOMP_FILTER ; do
  require "$c"
done
echo "    all required symbols present"

echo "==> building ${ARTIFACT} (-j${JOBS})"
make "${MAKE_ARGS[@]}" -j"${JOBS}" "$(basename "$ARTIFACT")"

cp "$ARTIFACT" "${OUT}/${ASSET}"
echo "==> built ${OUT}/${ASSET}"
ls -lh "${OUT}/${ASSET}"
( cd "$OUT" && sha256sum "${ASSET}" | tee "${ASSET}.sha256" )
